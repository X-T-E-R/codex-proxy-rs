"""准备前置阻断的有界诊断、新 GO 独立后继与未知动作不可重放，不连接外部服务"""
import errno
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

from test_schema53 import SchemaSite, accepted, schema53, remote, controller

try:
    import fcntl as real_fcntl
except ImportError:
    real_fcntl = None


def snapshot(root):
    return {str(path.relative_to(root)): path.read_bytes() for path in root.rglob('*') if path.is_file()}


def blocked_source(site, role='cleanup'):
    # 与历史 suppressed 收据兼容：不要求后来才有的诊断字段
    value = dict(runId=site.run.name, phase='schema53_preparation_requires_operator', success=False,
                 rollback=False, schemaWrites=0, startedAt=remote.now(), finishedAt=remote.now(),
                 errorCode='schema53_prepare_error_suppressed')
    remote.atomic(site.run / (role + '-result.json'), value)
    remote.atomic(site.run / ('schema53-' + role + '-claimed.json'),
                  {'at': remote.now(), 'pid': 99999999, 'singleAttempt': True})
    (site.run.parent / 'task40-active.lock').touch(exist_ok=True)
    return value


def child_run(site, role='cleanup', digit='2', evidence=None):
    evidence = evidence or remote.preparation_recovery_evidence(site.run, role, accepted())
    path = site.run.parent / ('task40-' + digit * 32)
    path.mkdir()
    target = dict(accepted(), preparationRecovery={key: evidence[key] for key in ('runId', 'role', 'receiptSha256')})
    (path / 'accepted.json').write_text(json.dumps(target))
    shutil.copytree(schema53.BUNDLE, path / 'schema53-artifacts')
    return path


class PreparationRecoveryTest(unittest.TestCase):
    @unittest.skipUnless(real_fcntl, '真实 flock 调度在 Linux 运行')
    def test_lifecycle_and_execution_lock_busy_persist_zero_sql_receipt(self):
        for role, lock_name, code in (
                ('prepare', 'lifecycle.lock', 'restart_gate_lifecycle_busy'),
                ('cleanup', 'lifecycle.lock', 'restart_gate_lifecycle_busy'),
                ('cleanup', 'task40-active.lock', 'schema53_deployment_busy')):
            with self.subTest(role=role, lock=lock_name), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                path = (site.gate if lock_name == 'lifecycle.lock' else site.run.parent) / lock_name
                with path.open('a') as owner, patch.object(remote, 'fcntl', real_fcntl):
                    real_fcntl.flock(owner, real_fcntl.LOCK_EX | real_fcntl.LOCK_NB)
                    result = site.prepare(cleanup=role == 'cleanup')
                self.assertEqual(result['errorCode'], code)
                self.assertEqual(result['preparationFailure']['causeType'], 'BlockingIOError')
                self.assertIn(result['preparationFailure']['errno'], (errno.EAGAIN, errno.EACCES))
                self.assertEqual(result['preparationFailure']['stage'],
                                 'lifecycle_gate' if lock_name == 'lifecycle.lock' else 'execution_lock')
                self.assertTrue(result['zeroSqlConfirmed'])
                self.assertEqual(result['schemaWrites'], 0)
                self.assertEqual(site.schema_events, [])
                self.assertTrue(site.current['running'])
                self.assertFalse((site.gate / 'maintenance').exists())
                self.assertEqual(json.loads((site.run / (role + '-result.json')).read_text()), result)

    def test_preflight_nonruntime_failure_records_only_bounded_metadata(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            with patch.object(remote.Deployment, 'preflight', side_effect=PermissionError(errno.EACCES, 'PRIVATE_BODY')):
                result = site.prepare(cleanup=True)
            self.assertEqual(result['preparationFailure'],
                             {'stage': 'preflight', 'errorType': 'PermissionError',
                              'causeType': 'PermissionError', 'errno': errno.EACCES})
            self.assertNotIn('PRIVATE_BODY', json.dumps(result))
            self.assertTrue(result['zeroSqlConfirmed'])

    def test_legacy_zero_sql_cleanup_has_fresh_child_and_preserves_all_source_bytes(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp, 'build_failed') as site:
            self.assertEqual(site.prepare()['schemaCommands']['prepare']['sqlstate'], '57014')
            blocked_source(site)
            original = snapshot(site.run)
            with patch.object(remote.os, 'kill', side_effect=ProcessLookupError):
                child = child_run(site)
                result = remote.Deployment(child).prepare(cleanup=True)
            self.assertEqual(result['phase'], 'schema53_cleaned', result.get('errorCode'))
            self.assertEqual(result['schemaWrites'], 1)
            self.assertEqual(result['preparationRecovery']['runId'], site.run.name)
            self.assertEqual(result['preparationRecovery']['successorRunId'], child.name)
            self.assertEqual(site.schema_events, [('prepare', True, False), ('cleanup', True, False)])
            self.assertEqual(site.index, 'missing')
            self.assertEqual(snapshot(site.run), original)
            self.assertTrue(site.current['running'])
            self.assertFalse((site.gate / 'maintenance').exists())
            with self.assertRaises(FileExistsError):
                remote.Deployment(child).prepare(cleanup=True)

    def test_source_admission_rejects_issued_unknown_live_unsettled_or_drifted_source(self):
        for case in ('issued', 'intent_without_count', 'alive', 'temporary', 'activated', 'baseline', 'successor'):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                value = blocked_source(site)
                target = accepted()
                expected = 'zero_sql'
                if case == 'issued':
                    value.update(schemaWrites=1, schemaCommands={'cleanup': {'outcome': 'unknown'}})
                elif case == 'intent_without_count':
                    value['schemaCommands'] = {'cleanup': {'outcome': 'intent'}}
                elif case == 'temporary':
                    (site.run / 'cleanup-progress.json.tmp').write_text('{}')
                    expected = 'unsettled'
                elif case == 'activated':
                    (site.run / 'launch-claimed.json').touch()
                    expected = 'activation_claimed'
                elif case == 'baseline':
                    target['baseline']['containerId'] = 'f' * 64
                    expected = 'baseline_drift'
                elif case == 'successor':
                    (site.run.parent / ('schema53-recovery-' + site.run.name + '-cleanup-claimed.json')).write_text('{}')
                    expected = 'successor_already_claimed'
                elif case == 'alive':
                    expected = 'process_not_exited'
                remote.atomic(site.run / 'cleanup-result.json', value)
                with patch.object(remote.os, 'kill', side_effect=None if case == 'alive' else ProcessLookupError):
                    with self.assertRaisesRegex(RuntimeError, expected):
                        remote.preparation_recovery_evidence(site.run, 'cleanup', target)
                self.assertEqual(site.schema_events, [])

    def test_rechecks_receipt_identity_and_active_sql_before_any_sql(self):
        for case in ('receipt_changed', 'active_sql'):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                blocked_source(site)
                with patch.object(remote.os, 'kill', side_effect=ProcessLookupError):
                    child = child_run(site)
                    if case == 'receipt_changed':
                        value = json.loads((site.run / 'cleanup-result.json').read_text())
                        value['finishedAt'] = remote.now()
                        remote.atomic(site.run / 'cleanup-result.json', value)
                    else:
                        site.active = [{'pid': 90, 'application_name': 'cpr:schema53:cleanup-invalid'}]
                    result = remote.Deployment(child).prepare(cleanup=True)
                self.assertEqual(result['schemaWrites'], 0)
                self.assertEqual(result['errorCode'], 'schema53_recovery_source_receipt_drift' if case == 'receipt_changed'
                                 else 'schema53_original_database_action_still_active')
                self.assertEqual(site.schema_events, [])

    def test_unknown_successor_cannot_be_replayed_via_ancestor_or_unadmitted_sibling(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            blocked_source(site)
            with patch.object(remote.os, 'kill', side_effect=ProcessLookupError):
                evidence = remote.preparation_recovery_evidence(site.run, 'cleanup', accepted())
                child = child_run(site, evidence=evidence)
                with patch.object(schema53, 'execute', side_effect=RuntimeError('transport_unknown')):
                    result = remote.Deployment(child).prepare(cleanup=True)
                self.assertEqual(result['schemaCommands']['cleanup']['outcome'], 'unknown')
                with self.assertRaisesRegex(RuntimeError, 'successor_already_claimed'):
                    remote.preparation_recovery_evidence(site.run, 'cleanup', accepted())
                with self.assertRaisesRegex(RuntimeError, 'zero_sql'):
                    remote.preparation_recovery_evidence(child, 'cleanup', accepted())
                sibling = child_run(site, digit='3', evidence=evidence)
                result = remote.Deployment(sibling).prepare(cleanup=True)
                self.assertTrue(result['zeroSqlConfirmed'])
                with self.assertRaisesRegex(RuntimeError, 'not_admitted_successor'):
                    remote.preparation_recovery_evidence(sibling, 'cleanup', accepted())

    def test_admitted_zero_sql_child_can_have_one_new_child(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            blocked_source(site)
            with patch.object(remote.os, 'kill', side_effect=ProcessLookupError):
                child = child_run(site)
                with patch.object(remote, 'gate_enter', side_effect=RuntimeError('restart_gate_lifecycle_busy')):
                    result = remote.Deployment(child).prepare(cleanup=True)
                self.assertTrue(result['zeroSqlConfirmed'])
                evidence = remote.preparation_recovery_evidence(child, 'cleanup', accepted())
                next_child = child_run(site, digit='3', evidence=evidence)
                result = remote.Deployment(next_child).prepare(cleanup=True)
            self.assertEqual(result['phase'], 'schema53_cleaned', result.get('errorCode'))
            self.assertEqual(len(site.schema_events), 1)

    @unittest.skipUnless(real_fcntl, '真实 flock 调度在 Linux 运行')
    def test_execution_busy_linked_child_keeps_recovery_route_and_checks_full_lineage(self):
        for drift in (False, True):
            with self.subTest(drift=drift), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                blocked_source(site)
                with patch.object(remote.os, 'kill', side_effect=ProcessLookupError), patch.object(remote, 'fcntl', real_fcntl):
                    child = child_run(site)
                    with (site.run.parent / 'task40-active.lock').open('r') as owner:
                        real_fcntl.flock(owner, real_fcntl.LOCK_EX | real_fcntl.LOCK_NB)
                        result = remote.Deployment(child).prepare(cleanup=True)
                    self.assertEqual(result['errorCode'], 'schema53_deployment_busy')
                    self.assertTrue(result['zeroSqlConfirmed'])
                    if drift:
                        value = json.loads((site.run / 'cleanup-result.json').read_text())
                        value.update(schemaWrites=1, schemaCommands={'cleanup': {'outcome': 'unknown'}})
                        remote.atomic(site.run / 'cleanup-result.json', value)
                        with self.assertRaisesRegex(RuntimeError, 'zero_sql'):
                            remote.preparation_recovery_evidence(child, 'cleanup', accepted())
                    else:
                        evidence = remote.preparation_recovery_evidence(child, 'cleanup', accepted())
                        next_child = child_run(site, digit='3', evidence=evidence)
                        self.assertEqual(remote.Deployment(next_child).prepare(cleanup=True)['phase'], 'schema53_cleaned')

    def test_online_transport_covers_cic_and_short_actions_keep_their_budget(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp, 'build_failed') as site:
            with patch.object(schema53, 'execute', side_effect=site.action) as execute:
                prepared = site.prepare()
                cleaned = site.prepare(cleanup=True)
            self.assertEqual([call.args[-1] for call in execute.call_args_list], [180, 30])
            self.assertEqual(prepared['schemaCommands']['prepare']['transportTimeoutSeconds'], 180)
            self.assertEqual(cleaned['schemaCommands']['cleanup']['transportTimeoutSeconds'], 30)
            self.assertEqual(schema53.product_contract(schema53.BUNDLE, accepted()['schema53Contract'])['actions'][
                'prebuild']['online_index_statement_timeout_ms'], 120000)

    def test_cleanup_after_linked_prepare_rechecks_source_and_full_lineage(self):
        for depth in (1, 2):
            for drift in ('activation', 'issued_unknown', 'receipt', 'pid_alive', 'reservation'):
                with self.subTest(depth=depth, drift=drift), tempfile.TemporaryDirectory() as tmp, \
                     SchemaSite(tmp, 'build_failed') as site, patch.object(remote, 'fcntl', real_fcntl or remote.fcntl):
                    blocked_source(site, 'prepare')
                    with patch.object(remote.os, 'kill', side_effect=ProcessLookupError):
                        child = child_run(site, 'prepare')
                        if depth == 2:
                            with patch.object(remote, 'gate_enter', side_effect=RuntimeError('restart_gate_lifecycle_busy')):
                                self.assertTrue(remote.Deployment(child).prepare()['zeroSqlConfirmed'])
                            evidence = remote.preparation_recovery_evidence(child, 'prepare', accepted())
                            child = child_run(site, 'prepare', digit='3', evidence=evidence)
                        prepared = remote.Deployment(child).prepare()
                    self.assertEqual(prepared['schemaCommands']['prepare']['outcome'], 'failed')
                    self.assertEqual(prepared['schemaWrites'], 1)
                    original = (child / 'prepare-result.json').read_bytes()
                    if drift == 'activation':
                        (site.run / 'attempt-claimed.json').touch()
                        expected = 'source_activation_claimed'
                    elif drift == 'issued_unknown':
                        value = json.loads((site.run / 'prepare-result.json').read_text())
                        value.update(schemaWrites=1, schemaCommands={'prepare': {'outcome': 'unknown'}})
                        remote.atomic(site.run / 'prepare-result.json', value)
                        expected = 'confirmed_zero_sql'
                    elif drift == 'receipt':
                        value = json.loads((site.run / 'prepare-result.json').read_text())
                        value['finishedAt'] = '2099-01-01T00:00:00+00:00'
                        remote.atomic(site.run / 'prepare-result.json', value)
                        expected = 'source_receipt_drift'
                    elif drift == 'reservation':
                        slot = site.run.parent / ('schema53-recovery-' + site.run.name + '-prepare-claimed.json')
                        value = json.loads(slot.read_text())
                        value['successorRunId'] = 'task40-' + 'f' * 32
                        remote.atomic(slot, value)
                        expected = 'reservation_owner_drift' if depth == 1 else 'source_not_admitted_successor'
                    else:
                        expected = 'source_process_not_exited'
                    events = list(site.schema_events)
                    with patch.object(remote.os, 'kill', side_effect=None if drift == 'pid_alive' else ProcessLookupError):
                        result = remote.Deployment(child).prepare(cleanup=True)
                    self.assertEqual(result['phase'], 'schema53_preparation_requires_operator')
                    self.assertIn(expected, result['errorCode'])
                    self.assertEqual(result['schemaWrites'], 0)
                    self.assertNotIn('schemaCommands', result)
                    self.assertEqual(result['preparationFailure']['stage'], 'recovery_admission')
                    self.assertEqual(site.schema_events, events)
                    self.assertEqual(site.index, 'invalid')
                    self.assertEqual((child / 'prepare-result.json').read_bytes(), original)
                    self.assertTrue(site.current['running'])
                    self.assertFalse((site.gate / 'maintenance').exists())

    def test_prepare_recovery_still_supports_activate_or_separate_cleanup_after_sql_failure(self):
        for fail in (None, 'build_failed'):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp, fail) as site:
                blocked_source(site, 'prepare')
                with patch.object(remote.os, 'kill', side_effect=ProcessLookupError):
                    child = child_run(site, 'prepare')
                    result = remote.Deployment(child).prepare()
                    if fail:
                        result = remote.Deployment(child).prepare(cleanup=True)
                        self.assertEqual(result['phase'], 'schema53_cleaned', result.get('errorCode'))
                    else:
                        site.run = child
                        self.assertEqual(site.deploy()['phase'], 'deployed')

    def test_local_recovery_creates_new_journal_and_provisions_cleanup_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            here = Path(tmp)
            source = here / ('task40-' + '1' * 32)
            source.mkdir()
            result = dict(runId=source.name, phase='schema53_preparation_requires_operator',
                          success=False, rollback=False, schemaWrites=0, finishedAt=remote.now())
            j = {'runId': source.name, 'phase': 'finished', 'accepted': accepted(), 'operation': 'cleanup',
                 'schemaMayHaveStarted': True, 'cleanupAuthorized': True, 'schema53CleanupIntent': True,
                 'remoteRoot': '/opt/codex-proxy-rs/.runtime/deployments/' + source.name, 'cleanupReceipt': result}
            remote.atomic(source / 'controller.json', j)
            original = snapshot(source)
            evidence = {'runId': source.name, 'role': 'cleanup', 'receiptSha256': 'a' * 64,
                        'sourceProcess': 'exited', 'schemaWrites': 0}
            with patch.object(controller, 'HERE', here), \
                 patch.object(controller, 'action_lock', return_value=MagicMock()), \
                 patch.object(controller, 'preparation_recovery_admission', return_value=evidence), \
                 patch.object(controller.os, 'getlogin', return_value='operator'), \
                 patch.object(controller.subprocess, 'run'), \
                 patch.object(controller, 'spawn_supervisor', side_effect=lambda path, role: {'runDirectory': str(path)}):
                launched = controller.recover_preparation(source, accepted(), schema53.BUNDLE)
                child = Path(launched['runDirectory'])
                with self.assertRaisesRegex(RuntimeError, 'successor_already_claimed'):
                    controller.recover_preparation(source, accepted(), schema53.BUNDLE)
                with self.assertRaisesRegex(RuntimeError, 'successor_already_claimed'):
                    controller.continue_schema53(source, 'activate')
                with patch.object(controller, 'provision') as provision, patch.object(controller, 'pull_exact'), \
                     patch.object(controller, 'start_remote_preparation', side_effect=RuntimeError('lost_ack')) as start, \
                     patch.object(controller, 'observe', return_value={'phase': 'schema53_cleaned'}):
                    controller.work(child, 'cleanup')
                    controller.work(child, 'cleanup')
            self.assertNotEqual(child, source)
            self.assertEqual(snapshot(source), original)
            self.assertEqual((provision.call_count, start.call_count), (1, 1))
            self.assertEqual(controller.read(child / 'controller.json')['accepted']['preparationRecovery']['runId'], source.name)

    def test_recovery_cli_requires_new_go(self):
        with tempfile.TemporaryDirectory() as tmp:
            here = Path(tmp)
            run = here / ('task40-' + '1' * 32)
            run.mkdir()
            (run / 'controller.json').write_text('{}')
            with patch.object(controller, 'HERE', here), patch.object(sys, 'argv',
                    ['controller.py', 'recover-preparation', '--run-dir', str(run)]), \
                 patch.object(controller, 'recover_preparation') as recover:
                with self.assertRaisesRegex(RuntimeError, 'new_preparation_recovery_GO_required'):
                    controller.main()
            recover.assert_not_called()


if __name__ == '__main__':
    unittest.main()
