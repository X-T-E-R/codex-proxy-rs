"""版本化 helper 安装的隔离检查；真实 Linux flock/原 watchdog，Docker 与配置为显式 stub"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

DEPLOY = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('helper_install', DEPLOY / 'recovery-helper-install.py')
install = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install)


@unittest.skipUnless(os.name == 'posix', 'site installer requires Linux flock and file ownership')
class InstallTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name) / 'app'
        self.gate = self.base / '.runtime/restart-recovery'
        self.gate.mkdir(parents=True, mode=0o700)
        (self.base / '.runtime/deployments').mkdir(mode=0o700)
        for path in (self.base / '.runtime/deployments/task36-active.lock',
                     self.base / '.runtime/deployments/task40-active.lock', self.gate / 'lifecycle.lock'):
            path.touch(mode=0o600)
        self.old_host = b'#!/usr/bin/env python3\nprint("previous host untouched")\n'
        (self.gate / 'site-recovery.py').write_bytes(self.old_host)
        watchdog = os.environ.get('CPR_TEST_WATCHDOG')
        self.watchdog_source = Path(watchdog) if watchdog else None
        (self.gate / 'watchdog.py').write_bytes(self.watchdog_source.read_bytes() if watchdog else b'# watchdog stub\n')
        self.b = {'source': '9888052ce26e106bb3cd2dc86c5d3050a6eccc9c',
                  'image': 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:' + '2' * 64,
                  'containerId': '3' * 64, 'startedAt': '2026-11-03T07:17:41.105110141Z'}
        self.current = {'id': self.b['containerId'], 'imageId': 'sha256:' + '4' * 64, 'pid': 123,
                        'running': True, 'health': 'healthy', 'revision': self.b['source'],
                        'startedAt': self.b['startedAt'], 'workingDir': str(self.base),
                        'composeFiles': str(self.base / 'docker-compose.yml'),
                        'project': 'codex-proxy-rs', 'service': 'codex-proxy-rs'}
        (self.base / '.env').write_text('CPR_IMAGE=' + self.b['image'] + '\n')
        (self.base / 'docker-compose.yml').write_text('services: {}\n')
        (self.base / 'deploy').mkdir()
        (self.base / 'deploy/config.yaml').write_text('# fixture has no credentials\n')
        self.before = {'runtime': {key: self.current[key] for key in install.OWNER_FIELDS},
                       'helperSource': install.remote.HELPER_SOURCE, 'helperImage': install.remote.HELPER_IMAGE,
                       'singleDeployment': True, 'recoveryCommand': [
                           'python3', str(self.gate / 'site-recovery.py'), '--pg-command-json',
                           json.dumps(['docker', 'exec', '-i', '--user', 'postgres', 'fixture-pg', 'psql', '-d', 'fixture']),
                           '--redis-helper-command-json', json.dumps(['docker', 'run', '--rm', '-i', '--network',
                           'fixture', '--read-only', '--mount', 'type=bind,src=/fixture,dst=/app/deploy/config.yaml,readonly',
                           install.remote.HELPER_IMAGE, '/app/bin/codex-proxy-rs', 'site-recovery-redis'])]}
        self.original = (json.dumps(self.before, separators=(',', ':')) + '\n').encode()
        (self.gate / 'binding.json').write_bytes(self.original)
        (self.gate / 'runs').mkdir()
        (self.gate / 'runs/completed.json').write_bytes(b'{"phase":"complete","keep":"exact bytes"}\n')
        self.m = {'contract': install.CONTRACT, 'operationId': 'helper-install-' + 'a' * 32,
                  'helper': install.HELPER, 'hostSha256': install.digest((DEPLOY / 'site-recovery.py').read_bytes()),
                  'entrySha256': install.digest((DEPLOY / install.ENTRY).read_bytes()),
                  'accepted': False, 'baseline': self.b}
        self.calls = []
        self.auth_calls = 0
        self.auth_error = None
        self.auth_reply = None
        self.running_helpers = []
        self.patches = [patch.object(install, 'BASE', self.base), patch.object(install.remote, 'BASE', self.base),
                        patch.object(install.remote, 'container', side_effect=lambda: copy.deepcopy(self.current)),
                        patch.object(install.remote, 'health', return_value=True),
                        patch.object(install.remote, 'image', side_effect=self.image),
                        patch.object(install.remote, 'command', side_effect=self.command)]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)
        self.host = self.base / 'candidate-host.py'
        self.host.write_bytes((DEPLOY / 'site-recovery.py').read_bytes())
        self.host.chmod(0o600)
        self.entry = self.base / 'candidate-entry.py'
        self.entry.write_bytes((DEPLOY / install.ENTRY).read_bytes())
        self.entry.chmod(0o600)
        self.operation = install.Installation(self.m, self.host, self.entry)
        self.m = self.operation.inspect()['proposal']
        self.m['accepted'] = True
        self.operation = install.Installation(self.m, self.host, self.entry)

    def image(self, image, source):
        self.calls.append(('image', image, source))
        return {'id': 'sha256:' + '4' * 64, 'os': 'linux', 'arch': 'amd64', 'revision': source}

    def command(self, argv, timeout=120, data=None):
        self.calls.append((argv, data))
        if argv[:2] == ['python3', str(self.gate / 'watchdog.py')]:
            if self.watchdog_source:
                return subprocess.check_output(argv, timeout=timeout)
            return b'{"phase":"lifecycle_busy"}'
        if argv[0] == 'python3' and argv[-1] == '--describe':
            return subprocess.check_output(argv, timeout=timeout)
        if argv[:2] == ['docker', 'compose']:
            return json.dumps({'services': {'codex-proxy-rs': {'image': self.b['image'], 'restart': 'no'}}}).encode()
        if argv[0] == 'systemctl':
            self.assertIn(argv[1], ('is-enabled', 'is-active'))
            return b'enabled' if argv[1] == 'is-enabled' else b'active'
        if argv[:2] == ['docker', 'inspect']:
            self.assertEqual(argv[-1], 'codex-proxy-rs')
            return b'"no"'
        if argv[:2] == ['docker', 'ps']:
            return '\n'.join(self.running_helpers).encode()
        if argv[:2] == ['docker', 'run']:
            self.auth_calls += 1
            self.assertEqual(json.loads(data), {'kind': 'authenticate'})
            self.assertTrue(install.remote.helper_argv(argv, install.HELPER['image']))
            self.assertEqual(argv[argv.index('--name') + 1], self.m['operationId'] + '-auth')
            Path(argv[argv.index('--cidfile') + 1]).write_text('5' * 64)
            self.assertEqual((self.gate / 'maintenance').read_bytes(), self.operation.marker)
            if self.auth_error:
                raise self.auth_error
            module = self.operation.candidate()
            return json.dumps(module.CAPABILITY if self.auth_reply is None else self.auth_reply).encode()
        self.fail('unexpected side effect command: ' + repr(argv))

    def assert_preserved(self):
        self.assertEqual((self.gate / 'site-recovery.py').read_bytes(), self.old_host)
        self.assertEqual((self.gate / 'runs/completed.json').read_bytes(), b'{"phase":"complete","keep":"exact bytes"}\n')
        self.assertEqual((self.operation.root / 'binding.before.json').read_bytes(), self.original)

    def test_cli_requires_go_before_reading_site_files(self):
        result = subprocess.run(['python3', '-B', str(DEPLOY / 'recovery-helper-install.py'),
                                 'install', '--manifest', '/fixture/missing-input.json'],
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 1)
        expected = 'explicit_site_GO_required' if os.geteuid() == 0 else 'linux_root_required'
        self.assertEqual(json.loads(result.stdout)['error'], expected)
        self.assertEqual(self.auth_calls, 0)

    def test_install_and_checked_rollback_preserve_generation_and_bytes(self):
        self.assertEqual(self.operation.install()['phase'], 'installed')
        self.assertEqual(self.auth_calls, 1)
        self.assertFalse((self.gate / 'maintenance').exists())
        self.assertEqual(stat.S_IMODE((self.gate / install.HOST).stat().st_mode), 0o600)
        binding = json.loads((self.gate / 'binding.json').read_bytes())
        install.remote.recovery_binding(binding, install.HELPER)
        self.assertEqual(binding['runtime'], self.before['runtime'])
        argv = binding['recoveryCommand']
        self.assertEqual(argv[argv.index('--pg-command-json') + 1], self.before['recoveryCommand'][3])
        self.assertEqual(argv[:4], ['python3', str(self.gate / install.ENTRY), '--recovery-host', str(self.gate / install.HOST)])
        self.host.unlink()
        self.entry.unlink()
        self.assertEqual(self.operation.observe()['bindingState'], 'confirmed')
        self.assertTrue(self.operation.observe()['versionedEntryMatches'])
        with self.assertRaisesRegex(RuntimeError, 'already_claimed'):
            self.operation.install()
        self.assertEqual(self.operation.settle('previous')['phase'], 'settled')
        self.assertEqual((self.gate / 'binding.json').read_bytes(), self.original)
        self.assertEqual(self.auth_calls, 1)
        self.assert_preserved()

    def test_foreign_maintenance_and_stale_proposal_refuse_before_auth(self):
        (self.gate / 'maintenance').write_text('foreign')
        with self.assertRaisesRegex(RuntimeError, 'maintenance'):
            self.operation.install()
        (self.gate / 'maintenance').unlink()
        (self.gate / 'binding.json').write_bytes(self.original + b' ')
        with self.assertRaisesRegex(RuntimeError, 'integrity'):
            self.operation.install()
        self.assertEqual(self.auth_calls, 0)
        self.assertFalse(self.operation.root.exists())

    def test_each_existing_lock_excludes_install_and_watchdog(self):
        for path in (self.base / '.runtime/deployments/task36-active.lock',
                     self.base / '.runtime/deployments/task40-active.lock', self.gate / 'lifecycle.lock'):
            with self.subTest(lock=path.name), path.open('r') as lock:
                install.remote.fcntl.flock(lock, install.remote.fcntl.LOCK_EX | install.remote.fcntl.LOCK_NB)
                with self.assertRaises(BlockingIOError):
                    self.operation.install()
        self.assertEqual(self.auth_calls, 0)
        if self.watchdog_source:
            with self.operation.locked():
                self.assertEqual(json.loads(subprocess.check_output(['python3', str(self.gate / 'watchdog.py'),
                                  '--base', str(self.gate)])), {'phase': 'lifecycle_busy'})

    def test_auth_unknown_keeps_gate_observe_and_no_reissue(self):
        self.auth_error = RuntimeError('command_result_unknown')
        self.running_helpers = ['5' * 64]
        with self.assertRaisesRegex(RuntimeError, 'unknown'):
            self.operation.install()
        observed = self.operation.observe()
        self.assertEqual(observed['bindingState'], 'previous')
        self.assertEqual(observed['maintenance'], 'owned')
        self.assertEqual(observed['helperContainerId'], '5' * 64)
        with self.assertRaises(RuntimeError):
            self.operation.install()
        with self.assertRaisesRegex(RuntimeError, 'helper_still_present'):
            self.operation.settle('previous')
        selectors = [call[0][-1] for call in self.calls if isinstance(call[0], list) and call[0][:2] == ['docker', 'ps']]
        self.assertIn('id=' + '5' * 64, selectors)
        self.running_helpers = []
        (self.operation.root / 'helper.cid').write_text('')
        with self.assertRaisesRegex(RuntimeError, 'helper_identity_unknown'):
            self.operation.settle('previous')
        (self.operation.root / 'helper.cid').write_text('5' * 64)
        self.assertEqual(self.operation.settle('previous')['phase'], 'settled')
        self.assertEqual(self.auth_calls, 1)
        self.assert_preserved()

    def test_capability_mismatch_holds_before_host_or_binding(self):
        self.auth_reply = {'contract': 'wrong'}
        with self.assertRaisesRegex(RuntimeError, 'helper_capability'):
            self.operation.install()
        self.assertFalse((self.gate / install.HOST).exists())
        self.assertEqual((self.gate / 'binding.json').read_bytes(), self.original)
        self.assertEqual(self.operation.observe()['maintenance'], 'owned')

    def test_live_pin_image_and_option_operand_rejected_before_writes(self):
        (self.base / '.env').write_text('CPR_IMAGE=' + install.HELPER['image'] + '\n')
        with self.assertRaisesRegex(RuntimeError, 'pin_drift'):
            self.operation.install()
        (self.base / '.env').write_text('CPR_IMAGE=' + self.b['image'] + '\n')
        self.current['imageId'] = 'sha256:' + '9' * 64
        with self.assertRaisesRegex(RuntimeError, 'image_drift'):
            self.operation.install()
        self.current['imageId'] = 'sha256:' + '4' * 64
        bad = copy.deepcopy(self.before)
        argv = json.loads(bad['recoveryCommand'][5])
        argv.insert(2, 'another-image:tag')
        bad['recoveryCommand'][5] = json.dumps(argv)
        (self.gate / 'binding.json').write_bytes(install.encoded(bad))
        fresh = dict(self.m, previous=self.operation.integrity())
        with self.assertRaisesRegex(RuntimeError, 'helper_argv_drift'):
            install.Installation(fresh, self.host).install()
        self.assertEqual(self.auth_calls, 0)
        self.assertFalse(self.operation.root.exists())

    def test_registry_digest_failure_blocks_before_auth(self):
        # 使用真实 image 合同，daemon 响应由明确错误 RepoDigests stub 提供
        spec = importlib.util.spec_from_file_location('image_contract', DEPLOY / 'task40-prod-upgrade/remote.py')
        contract = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(contract)
        def inspect(argv, timeout=120, data=None):
            return b'[]' if argv[4] == '{{json .RepoDigests}}' else json.dumps(
                {'id': 'sha256:' + '4' * 64, 'os': 'linux', 'arch': 'amd64', 'revision': self.b['source']}).encode()
        with patch.object(contract, 'command', side_effect=inspect), patch.object(install.remote, 'image', side_effect=contract.image):
            with self.assertRaisesRegex(RuntimeError, 'digest_not_bound'):
                self.operation.install()
        self.assertEqual(self.auth_calls, 0)
        self.assertFalse(self.operation.root.exists())

    def test_incomplete_install_evidence_remains_readonly_observable(self):
        self.operation.root.parent.mkdir(mode=0o700)
        self.operation.root.mkdir(mode=0o700)
        observed = self.operation.observe()
        self.assertFalse(observed['evidenceMatches'])
        self.assertEqual(observed['bindingState'], 'unproven')
        self.assertFalse((self.gate / 'maintenance').exists())
        self.assertEqual(self.auth_calls, 0)

    def test_runtime_drift_after_auth_keeps_gate(self):
        original = self.command
        def drift(argv, timeout=120, data=None):
            reply = original(argv, timeout, data)
            if argv[:2] == ['docker', 'run']:
                self.current['startedAt'] = '2026-11-03T09:00:00Z'
            return reply
        with patch.object(install.remote, 'command', side_effect=drift):
            with self.assertRaisesRegex(RuntimeError, 'runtime_drift'):
                self.operation.install()
        self.assertEqual(self.operation.observe()['maintenance'], 'owned')
        with self.assertRaisesRegex(RuntimeError, 'runtime_drift'):
            self.operation.settle('previous')
        self.assertEqual((self.gate / 'binding.json').read_bytes(), self.original)

    def test_unknown_after_atomic_binding_is_observed_then_settled_without_rewrite(self):
        real_replace = install.replace
        def lose_ack(path, data):
            real_replace(path, data)
            if path == self.gate / 'binding.json':
                raise OSError('injected after os.replace')
        with patch.object(install, 'replace', side_effect=lose_ack):
            with self.assertRaises(OSError):
                self.operation.install()
        self.assertEqual(self.operation.observe()['bindingState'], 'confirmed')
        with patch.object(install, 'replace', wraps=real_replace) as writes:
            self.assertEqual(self.operation.settle('confirmed')['phase'], 'settled')
            self.assertTrue(all(call.args[0] != self.gate / 'binding.json' for call in writes.call_args_list))
        self.assertEqual(self.auth_calls, 1)
        self.assert_preserved()

    def test_permissions_and_foreign_binding_block_checked_rollback(self):
        self.operation.install()
        (self.gate / install.HOST).chmod(0o666)
        with self.assertRaisesRegex(RuntimeError, 'permissions'):
            self.operation.settle('confirmed')
        (self.gate / install.HOST).chmod(0o600)
        (self.gate / 'binding.json').write_text('{}')
        with self.assertRaisesRegex(RuntimeError, 'foreign_binding'):
            self.operation.settle('previous')
        self.assertEqual((self.gate / 'binding.json').read_text(), '{}')

    def test_actual_watchdog_new_binding_zero_request_exit_generation(self):
        if not self.watchdog_source:
            self.skipTest('set CPR_TEST_WATCHDOG to the installed Task36 watchdog source')
        self.operation.install()
        bin_dir = self.base / 'bin'
        bin_dir.mkdir()
        state = self.base / 'docker-state.json'
        docker = bin_dir / 'docker'
        docker.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
p=pathlib.Path(os.environ['CPR_TEST_DOCKER_STATE'])
s=json.loads(p.read_text()); a=sys.argv[1:]
with p.with_suffix('.calls').open('a') as f: f.write(json.dumps(a)+'\\n')
if a[0]=='inspect':
 if 'RestartPolicy' in a[2] and 'FinishedAt' not in a[2]: print('"no"')
 elif '"Id"' in a[2]:
  print(json.dumps({'Id':s['id'],'Running':False,'Restarting':False,'Pid':0,'Status':'exited',
    'StartedAt':s['startedAt'],'FinishedAt':s['finishedAt'],'RestartPolicy':'no','Service':s['service'],'Project':s['project']}))
 else: print(json.dumps(s))
elif a[:2]==['image','inspect']: print(os.environ['CPR_TEST_HELPER_SOURCE'])
elif a[0]=='ps': pass
elif a[0]=='exec':
 q=sys.stdin.read()
 print('[]' if 'jsonb_agg' in q and 'begin;' not in q else '0\\n[]' if 'begin;' in q else '0')
elif a[0]=='run':
 assert json.loads(sys.stdin.read())=={'kind':'authenticate'}
 print(os.environ['CPR_TEST_CAPABILITY'])
elif a[0]=='start':
 assert a[1]==s['id'];s.update(running=True,status='running',startedAt=s['finishedAt']);p.write_text(json.dumps(s));print(s['id'])
else: raise RuntimeError(a)
''')
        docker.chmod(0o700)
        stopped = dict(self.current, running=False, restarting=False, status='exited', restart='no',
                       finishedAt='2026-11-03T10:00:00Z')
        state.write_text(json.dumps(stopped))
        env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ['PATH'],
                   CPR_TEST_DOCKER_STATE=str(state), CPR_TEST_HELPER_SOURCE=install.HELPER['source'],
                   CPR_TEST_CAPABILITY=json.dumps(self.operation.candidate().CAPABILITY))
        process = subprocess.run(['python3', str(self.gate / 'watchdog.py'), '--base', str(self.gate)],
                                 env=env, timeout=30, capture_output=True, text=True)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr +
                         (state.with_suffix('.calls').read_text() if state.with_suffix('.calls').exists() else 'no Docker calls'))
        result = json.loads(process.stdout)
        self.assertEqual(result['phase'], 'recovered_and_started')
        self.assertEqual(result['requests'], 0)
        calls = [json.loads(line) for line in state.with_suffix('.calls').read_text().splitlines()]
        self.assertEqual(sum(call[0] == 'start' for call in calls), 1)
        self.assertEqual(sum(call[0] == 'run' for call in calls), 1)
        # 旧 binding 同样由原 watchdog 解释；旧 host 用显式完整响应 stub，不连接数据面
        legacy = b'#!/usr/bin/env python3\nimport json\nprint(json.dumps({"phase":"complete","requests":0,"accountSlots":0,"clientSlots":0}))\n'
        (self.gate / 'site-recovery.py').write_bytes(legacy)
        (self.gate / 'binding.json').write_bytes(self.original)
        stopped.update(startedAt='2026-11-03T10:01:00Z', finishedAt='2026-11-03T10:02:00Z')
        state.write_text(json.dumps(stopped))
        process = subprocess.run(['python3', str(self.gate / 'watchdog.py'), '--base', str(self.gate)],
                                 env=env, timeout=30, capture_output=True, text=True)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr +
                         (state.with_suffix('.calls').read_text() if state.with_suffix('.calls').exists() else 'no Docker calls'))
        result = json.loads(process.stdout)
        self.assertEqual(result['phase'], 'recovered_and_started')


if __name__ == '__main__':
    unittest.main()
