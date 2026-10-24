"""停止闸的进程、归属、认证参数和失败续作边界"""
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('site_recovery', Path(__file__).parents[1] / 'site-recovery.py')
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.old = {'Id': 'a' * 64, 'Running': False, 'Restarting': False, 'Pid': 0,
            'Status': 'exited', 'StartedAt': '2026-10-24T00:00:00Z',
            'FinishedAt': '2026-10-24T01:00:00Z', 'RestartPolicy': 'no',
            'Service': 'codex-proxy-rs', 'Project': 'isolated'}
        self.row = {'id': 'req_old', 'client': 'key_a', 'started': '2026-10-24T00:30:00Z',
            'expiry': '2026-10-24T02:00:00Z'}
        self.args = SimpleNamespace(old_container='a' * 64, apply=True)

    def test_guard_projects_no_env_and_refuses_live_process_restart_or_other_app(self):
        def inspect(argv, data=None):
            self.assertEqual(argv[:3], ['docker', 'inspect', '--format'])
            self.assertNotIn('Env', argv[3])
            return json.dumps(self.old)
        with patch.object(recovery, 'run', side_effect=[json.dumps(self.old), '']), patch.object(recovery, 'pg', return_value='0'):
            self.assertEqual(recovery.guard(self.args)['Id'], self.old['Id'])
        for change in [{'Running': True}, {'Pid': 42}, {'Restarting': True},
                       {'RestartPolicy': 'unless-stopped'}, {'Status': 'created'}]:
            with patch.object(recovery, 'run', return_value=json.dumps(dict(self.old, **change))):
                with self.assertRaises(RuntimeError):
                    recovery.guard(self.args)
        with patch.object(recovery, 'run', side_effect=[json.dumps(self.old), 'another']), patch.object(recovery, 'pg', return_value='0'):
            with self.assertRaises(RuntimeError):
                recovery.guard(self.args)
        with patch.object(recovery, 'run', side_effect=inspect):
            self.old['Running'] = True
            with self.assertRaises(RuntimeError):
                recovery.guard(self.args)

    def test_other_database_client_refuses(self):
        with patch.object(recovery, 'run', side_effect=[json.dumps(self.old), '']), patch.object(recovery, 'pg', return_value='1'):
            with self.assertRaises(RuntimeError):
                recovery.guard(self.args)

    def test_command_uses_inherited_environment_names_not_password_values_or_shell(self):
        safe = ['docker', 'exec', '-i', '-e', 'PGPASSWORD', 'postgres', 'psql']
        self.assertEqual(recovery.command(json.dumps(safe)), safe)
        for unsafe in [['redis-cli', '-a', 'secret'], ['docker', '-e', 'CPR_REDIS_PASSWORD=secret'],
                       ['sh', '-c', 'echo secret'], ['psql', 'postgres://user:secret@db/name']]:
            with self.assertRaises(ValueError):
                recovery.command(json.dumps(unsafe))
        with self.assertRaises(ValueError):
            recovery.command('["redis-cli"]', helper=True)

    def test_manifest_rejects_duplicate_or_unstructured_identity(self):
        self.assertEqual(recovery.validate_rows([self.row]), [self.row])
        for rows in [[self.row, self.row], [dict(self.row, id="req_';drop")], [dict(self.row, client='')]]:
            with self.assertRaises(ValueError):
                recovery.validate_rows(rows)

    def test_auth_failure_happens_before_pg_snapshot_or_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            self.args.receipt = str(Path(directory) / 'receipt.json')
            with patch.object(recovery, 'guard', return_value=self.old), patch.object(recovery, 'redis', side_effect=RuntimeError('auth')), patch.object(recovery, 'pg') as pg:
                with self.assertRaises(RuntimeError):
                    recovery.execute(self.args)
                pg.assert_not_called()
                self.assertFalse(Path(self.args.receipt).exists())

    def test_changed_manifest_never_releases_and_receipt_stays_prepared(self):
        with tempfile.TemporaryDirectory() as directory:
            self.args.receipt = str(Path(directory) / 'receipt.json')
            with patch.object(recovery, 'guard', return_value=self.old), patch.object(recovery, 'redis', return_value=['0', []]), patch.object(recovery, 'pg', side_effect=[json.dumps([self.row]), '[]']), patch.object(recovery, 'release') as release:
                with self.assertRaises(RuntimeError):
                    recovery.execute(self.args)
                release.assert_not_called()
            self.assertEqual(json.loads(Path(self.args.receipt).read_text())['phase'], 'prepared')

    def test_request_after_process_exit_refuses_before_write(self):
        row = dict(self.row, started='2026-10-24T01:00:01Z')
        with tempfile.TemporaryDirectory() as directory:
            self.args.receipt = str(Path(directory) / 'receipt.json')
            with patch.object(recovery, 'guard', return_value=self.old), patch.object(recovery, 'redis', return_value=['0', []]), patch.object(recovery, 'pg', return_value=json.dumps([row])) as pg:
                with self.assertRaises(RuntimeError):
                    recovery.execute(self.args)
                self.assertEqual(pg.call_count, 1)


if __name__ == '__main__':
    unittest.main()
