"""HI-01：原 watchdog → 已安装单次入口 → bac2 host；Linux 锁真实，数据面与容器为 stub"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import test_recovery_helper_install as fixtures

install = fixtures.install


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class LostHostProcess(BaseException):
    pass


@unittest.skipUnless(os.name == 'posix' and os.environ.get('CPR_TEST_WATCHDOG'), 'requires original Linux watchdog fixture')
class SingleAttemptEntryTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.InstallTest()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.fixture.operation.install()
        self.gate = self.fixture.gate
        self.host = self.fixture.operation.candidate()
        self.entry = load('installed_single_attempt_entry', self.gate / install.ENTRY)
        self.watchdog = load('original_single_attempt_watchdog', self.gate / 'watchdog.py')
        self.current = dict(self.fixture.current, running=False, restarting=False, restart='no',
                            status='exited', finishedAt='2026-11-03T12:00:00Z')
        self.identity = {'Id': self.current['id'], 'StartedAt': self.current['startedAt'],
                         'FinishedAt': self.current['finishedAt'], 'Project': self.current['project']}
        self.calls = {'entry': 0, 'host': 0, 'pg': 0, 'batch': 0, 'start': 0}
        self.row = {'id': 'req_fixture_unknown', 'client': 'key_fixture',
                    'started': '2026-11-03T08:00:00Z', 'expiry': None}

    def args(self, argv):
        values = {key: argv[argv.index('--' + key.replace('_', '-')) + 1] for key in (
            'recovery_host', 'old_container', 'receipt', 'pg_command_json',
            'redis_helper_command_json', 'helper_source', 'helper_image')}
        return SimpleNamespace(**values, apply=True)

    def pg(self, args, sql):
        if sql == self.host.SNAPSHOT:
            return json.dumps([self.row])
        self.calls['pg'] += 1
        return json.dumps([self.row['id']])

    def redis(self, args, request):
        if request['kind'] == 'authenticate':
            return self.host.CAPABILITY
        self.calls['batch'] += 1
        return {'contract': self.host.CONTRACT, 'released': {'accountSlots': 1, 'clientSlots': 1}}

    def command(self, fault):
        real_run = self.entry.run_host
        def call(argv, timeout=30):
            if argv[:2] == ['python3', str(self.gate / install.ENTRY)]:
                self.calls['entry'] += 1
                args = self.args(argv)
                def child(child_argv):
                    self.calls['host'] += 1
                    if fault == 'nonzero':
                        return real_run([sys.executable, '-c', 'import sys; sys.exit(7)'])
                    if fault == 'timeout':
                        with patch.object(self.entry, 'TIMEOUT', 0.03):
                            return real_run([sys.executable, '-c', 'import time; time.sleep(2)'])
                    def intent(path, receipt):
                        raise LostHostProcess('PG confirmed; host lost before durable batch intent')
                    with patch.object(self.host, 'guard', return_value=self.identity), \
                         patch.object(self.host, 'admit_helper'), patch.object(self.host, 'pg', side_effect=self.pg), \
                         patch.object(self.host, 'redis', side_effect=self.redis), \
                         patch.object(self.host, 'replace_receipt', side_effect=intent):
                        return json.dumps(self.host.execute(args))
                with patch.object(self.entry, 'load_host', return_value=self.host), \
                     patch.object(self.entry, 'owner', return_value=self.identity), \
                     patch.object(self.entry, 'run_host', side_effect=child):
                    try:
                        return json.dumps(self.entry.execute(args, self.gate))
                    except (Exception, LostHostProcess):
                        raise RuntimeError('controlled command failed; raw output suppressed') from None
            self.assertEqual(argv, ['docker', 'start', self.current['id']])
            self.calls['start'] += 1
            self.current.update(running=True, status='running', startedAt='2026-11-03T12:00:01Z')
            return self.current['id']
        return call

    def check_unknown(self, fault):
        with patch.object(self.watchdog, 'inspect', side_effect=lambda: copy.deepcopy(self.current)), \
             patch.object(self.watchdog, 'command', side_effect=self.command(fault)):
            with self.assertRaises(RuntimeError):
                self.watchdog.cycle(self.gate)
            self.assertTrue((self.gate / 'maintenance').exists())
            claim = next((self.gate / 'runs').glob('*/recovery.json.entry-v1.json'))
            self.assertEqual(json.loads(claim.read_text())['phase'], 'intent')
            before = dict(self.calls)
            self.assertEqual(self.watchdog.cycle(self.gate), {'phase': 'maintenance'})
            self.assertEqual(self.calls, before)
        self.assertEqual(self.calls['entry'], 1)
        self.assertEqual(self.calls['host'], 1)
        self.assertEqual(self.calls['start'], 0)
        if fault == 'pg_before_batch_intent':
            receipt = claim.with_name('recovery.json')
            self.assertEqual(json.loads(receipt.read_text())['phase'], 'prepared')
            self.assertNotIn('batch', json.loads(receipt.read_text()))
            self.assertEqual(self.calls['pg'], 1)
            self.assertEqual(self.calls['batch'], 0)
            observed = self.entry.observe(receipt)
            self.assertEqual(observed['receiptPhase'], 'prepared')
            self.assertEqual(observed['claimPhase'], 'intent')

    def test_pg_confirmation_before_batch_intent_loss_next_tick_is_held(self):
        self.check_unknown('pg_before_batch_intent')

    def test_nonzero_host_next_tick_is_held(self):
        self.check_unknown('nonzero')

    def test_real_child_transport_timeout_next_tick_is_held(self):
        self.check_unknown('timeout')

    def test_task40_existing_maintenance_and_complete_reentry_are_preserved(self):
        run = self.fixture.base / '.runtime/deployments' / ('task40-' + 'f' * 32)
        run.mkdir(mode=0o700)
        receipt = run / ('stopped-recovery-' + self.identity['Id'][:12] + '.json')
        (self.gate / 'maintenance').write_text(run.name)
        argv = json.loads((self.gate / 'binding.json').read_text())['recoveryCommand'] + [
            '--old-container', self.identity['Id'], '--receipt', str(receipt)]
        args = self.args(argv)
        def child(command):
            self.calls['host'] += 1
            self.host.save(receipt, {'owner': self.identity, 'phase': 'complete', 'requests': [self.row],
                'completed': [self.row['id']], 'batch': {'contract': self.host.CONTRACT, 'outcome': 'completed'},
                'released': {'accountSlots': 1, 'clientSlots': 1}}, exclusive=True)
            return {'phase': 'complete', 'requests': 1, 'accountSlots': 1, 'clientSlots': 1}
        with patch.object(self.entry, 'load_host', return_value=self.host), \
             patch.object(self.entry, 'owner', return_value=self.identity), \
             patch.object(self.entry, 'run_host', side_effect=child):
            first = self.entry.execute(args, self.gate)
            self.assertEqual(self.entry.execute(args, self.gate), first)
        self.assertEqual(self.calls['host'], 1)
        self.assertEqual((self.gate / 'maintenance').read_text(), run.name)

    def test_entry_integrity_is_required_by_install_observe_and_settle(self):
        (self.gate / install.ENTRY).write_text('# drift\n')
        self.assertFalse(self.fixture.operation.observe()['versionedEntryMatches'])
        with self.assertRaisesRegex(RuntimeError, 'entry_integrity'):
            self.fixture.operation.settle('confirmed')
        self.assertTrue((self.gate / 'maintenance').exists())
        self.assertEqual(self.fixture.operation.settle('previous')['phase'], 'settled')
        self.assertEqual((self.gate / 'binding.json').read_bytes(), self.fixture.original)


if __name__ == '__main__':
    unittest.main()
