"""独立部署准备、短切换与受检恢复的离线生命周期，不连接外部服务"""
import base64
from contextlib import contextmanager
import json
from pathlib import Path
import shutil
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch, MagicMock

import schema53
from test_controller import MockSite, manifest, remote, controller, IMAGE, BASELINE_IMAGE

HERE = Path(__file__).resolve().parent


def accepted():
    value = manifest()
    example = json.loads((HERE / 'accepted.schema53.example.json').read_text())
    value.update(schema=53, baselineSchema=52, noMigrationChanges=False,
                 source=example['schema53Contract']['source'], schema53Contract=example['schema53Contract'])
    value['schema53Contract']['evidence'] = 'accepted exact product SQL and same-image migration/recovery evidence'
    return value


def observation(version=52, index='valid', active=None):
    return {'contract': 'cpr-successful-trace-index/2', 'schema_state': 'schema' + str(version),
            'schema_version': version, 'index_state': index, 'active_operations': active or []}


class SchemaSite(MockSite):
    def __init__(self, tmp, fail=None):
        super().__init__(tmp, fail)
        (self.run / 'accepted.json').write_text(json.dumps(accepted()))
        shutil.copytree(schema53.BUNDLE, self.run / 'schema53-artifacts')
        self.version, self.index = 52, 'missing'
        self.active = []
        self.schema_events = []
        self.rollback_mode = None
        self.candidate['revision'] = accepted()['source']

    def image(self, value, source):
        return {'id': self.candidate['imageId'] if source == self.candidate['revision'] else self.old['imageId']}

    def __enter__(self):
        super().__enter__()
        for p in (patch.object(schema53, 'inspect', side_effect=self.observe),
                  patch.object(schema53, 'execute', side_effect=self.action)):
            p.start()
            self.patches.append(p)
        return self

    def facts(self):
        value = super().facts()
        value.update(schemaMax=self.version, schemaCount=self.version)
        return value

    def observe(self, *args):
        result = observation(self.version, self.index, self.active)
        if self.fail == 'ledger_mismatch':
            result['schema_state'] = 'mismatch'
        return result

    def action(self, directory, value, role, pg, timeout):
        self.schema_events.append((role, self.current['running'], (self.gate / 'maintenance').exists()))
        if role == 'prepare':
            if self.fail in ('build_failed', 'build_unknown', 'build_unknown_complete'):
                self.index = 'valid' if self.fail == 'build_unknown_complete' else 'invalid'
                if self.fail != 'build_failed':
                    raise RuntimeError('transport_unknown')
                return {'outcome': 'failed', 'sqlstate': '57014', 'returnCode': 3}
            self.index = 'valid'
        elif role == 'cleanup':
            if self.index not in ('invalid', 'drop_incomplete', 'missing'):
                return {'outcome': 'failed', 'sqlstate': '55000', 'returnCode': 3}
            self.index = 'missing'
        else:
            assert not self.current['running']
            if self.rollback_mode == 'unknown_active':
                self.active = [{'pid': 900, 'application_name': 'cpr:schema53:revert'}]
                raise RuntimeError('transport_unknown')
            if self.rollback_mode in ('failed', 'unknown_uncommitted'):
                if self.rollback_mode == 'failed':
                    return {'outcome': 'failed', 'sqlstate': '55P03', 'returnCode': 3}
                raise RuntimeError('transport_unknown')
            self.version = 52
            if self.rollback_mode == 'unknown_committed':
                raise RuntimeError('transport_unknown')
        return {'outcome': 'completed', 'returnCode': 0, 'product': self.observe()}

    def command(self, args, **kwargs):
        result = super().command(args, **kwargs)
        if args[:2] == ['docker', 'compose'] and 'up' in args:
            if self.pin == IMAGE:
                self.version = 52 if self.fail == 'migration_failed' else 53
            elif self.version != 52:
                raise AssertionError('old schema52 image must never start with migration53 recorded')
        return result

    def prepare(self, cleanup=False):
        return remote.Deployment(self.run).prepare(cleanup)

    def resume(self):
        (self.run / 'attempt-claimed.json').touch(exist_ok=True)
        with patch.object(remote, 'set_pin', side_effect=self.set_pin):
            return remote.Deployment(self.run).resume()


class Schema53LifecycleTest(unittest.TestCase):
    def test_online_preparation_keeps_generation_and_no_maintenance_then_short_cut_verifies53(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            old_id = site.current['id']
            prepared = site.prepare()
            self.assertEqual(prepared['phase'], 'schema53_prepared', prepared.get('errorCode'))
            self.assertEqual(site.schema_events, [('prepare', True, False)])
            self.assertEqual(site.current['id'], old_id)
            self.assertEqual(site.version, 52)
            self.assertFalse(any(args[:2] == ['docker', 'stop'] for args in site.events))
            result = site.deploy()
            self.assertEqual(result['phase'], 'deployed', result.get('errorCode'))
            self.assertEqual(result['afterFacts']['schemaMax'], 53)
            self.assertEqual(result['targetSchema'], 53)
            self.assertEqual(site.index, 'valid')
            self.assertEqual(len(result['stoppedRecovery']), 1)
            for p in (site.run / 'launch.guard', site.base / '.runtime/deployments/task40-active.lock',
                      site.gate / 'lifecycle.lock'):
                p.touch()
            self.assertEqual(remote.verify(site.run)['facts']['schemaMax'], 53)

    def test_missing_invalid_drifted_or_busy_preparation_never_stops(self):
        for state in ('missing_receipt', 'invalid', 'drop_incomplete', 'unexpected', 'ledger_mismatch', 'busy'):
            with self.subTest(state=state), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                if state != 'missing_receipt':
                    self.assertEqual(site.prepare()['phase'], 'schema53_prepared')
                if state in ('invalid', 'drop_incomplete', 'unexpected'):
                    site.index = state
                elif state == 'ledger_mismatch':
                    site.fail = state
                elif state == 'busy':
                    site.active = [{'pid': 99, 'application_name': 'codex-proxy-rs:migration'}]
                result = site.deploy()
                self.assertFalse(result['success'])
                self.assertFalse(any(args[:2] == ['docker', 'stop'] for args in site.events))
                self.assertFalse((site.gate / 'maintenance').exists())

    def test_prepare_health_observation_does_not_weaken_fresh_cut_admission(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            site.old['health'] = 'unhealthy'
            self.assertEqual(site.prepare()['phase'], 'schema53_prepared')
            result = site.deploy()
            self.assertEqual(result['phase'], 'blocked_unchanged')
            self.assertEqual(result['errorCode'], 'baseline_runtime_drift')
            self.assertFalse(any(args[:2] == ['docker', 'stop'] for args in site.events))

    def test_failed_build_or_unknown_stays_online_and_cannot_replay_same_action(self):
        for fail in ('build_failed', 'build_unknown'):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp, fail) as site:
                result = site.prepare()
                self.assertEqual(result['phase'], 'schema53_preparation_requires_operator')
                self.assertEqual(result['schemaCommands']['prepare']['outcome'],
                                 'failed' if fail == 'build_failed' else 'unknown')
                original = (site.run / 'prepare-result.json').read_bytes()
                with self.assertRaises(FileExistsError):
                    site.prepare()
                self.assertEqual((site.run / 'prepare-result.json').read_bytes(), original)
                self.assertEqual(site.schema_events, [('prepare', True, False)])
                self.assertTrue(site.current['running'])
                self.assertFalse((site.gate / 'maintenance').exists())

    def test_unknown_success_is_reconciled_readonly_only_after_original_action_has_finished(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp, 'build_unknown_complete') as site:
            site.prepare()
            original = (site.run / 'prepare-result.json').read_bytes()
            site.active = [{'pid': 99, 'application_name': 'cpr:schema53:prebuild'}]
            with self.assertRaisesRegex(RuntimeError, 'still_active'):
                remote.Deployment(site.run).reconcile_preparation()
            site.active = []
            observed = remote.Deployment(site.run).reconcile_preparation()
            self.assertEqual(observed['phase'], 'schema53_prepared')
            self.assertEqual((site.run / 'prepare-result.json').read_bytes(), original)
            self.assertEqual(len(site.schema_events), 1)
            self.assertEqual(site.deploy()['phase'], 'deployed')

    def test_recoverable_cleanup_is_separate_once_and_protects_valid_or_unexpected_index(self):
        for initial in ('missing', 'invalid', 'drop_incomplete', 'valid', 'unexpected'):
            with self.subTest(initial=initial), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                site.index = initial
                recoverable = initial in ('missing', 'invalid', 'drop_incomplete')
                result = site.prepare(cleanup=True)
                self.assertEqual(result['phase'], 'schema53_cleaned' if recoverable else
                                 'schema53_preparation_requires_operator')
                self.assertEqual(site.index, 'missing' if recoverable else initial)
                self.assertTrue(site.current['running'])
                with self.assertRaises(FileExistsError):
                    site.prepare(cleanup=True)

    def test_candidate_or_migration_failure_reverts_checked_ledger_before_old_start(self):
        for fail in ('candidate_unhealthy', 'migration_failed', 'candidate_drift', 'pin_failure'):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                self.assertEqual(site.prepare()['phase'], 'schema53_prepared')
                site.fail = fail
                result = site.deploy()
                self.assertEqual(result['phase'], 'rolled_back_same52', result.get('recoveryErrorCode'))
                self.assertEqual(result['schemaRollback']['phase'], 'verified52')
                self.assertEqual(result['afterFacts']['schemaMax'], 52)
                self.assertEqual(site.index, 'valid')
                self.assertEqual(site.pin, BASELINE_IMAGE)
                self.assertIn(('rollback', False, True), site.schema_events)
                self.assertFalse((site.gate / 'maintenance').exists())
                expected = 1 if fail == 'pin_failure' else 2
                self.assertEqual(len(result['stoppedRecovery']), expected)

    def test_revert_unknown_only_observes_after_commit_and_never_reissues(self):
        for mode in ('unknown_committed', 'unknown_uncommitted', 'unknown_active'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
                site.prepare()
                site.fail, site.rollback_mode = 'candidate_unhealthy', mode
                result = site.deploy()
                self.assertEqual(result['phase'], 'recovery_requires_operator')
                self.assertTrue((site.gate / 'maintenance').exists())
                site.rollback_mode = None
                result = site.resume()
                self.assertEqual(result['phase'], 'rolled_back_same52' if mode == 'unknown_committed' else
                                 'recovery_requires_operator', result.get('takeoverErrorCode'))
                self.assertEqual(sum(event[0] == 'rollback' for event in site.schema_events), 1)
                self.assertEqual(sum(args[:2] == ['docker', 'compose'] and 'up' in args for args in site.events),
                                 2 if mode == 'unknown_committed' else 1)

    def test_rollback_sql_failure_keeps_stopped_candidate_and_maintenance(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            site.prepare()
            site.fail, site.rollback_mode = 'candidate_unhealthy', 'failed'
            result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertEqual(result['schemaCommands']['rollback']['sqlstate'], '55P03')
            self.assertEqual(site.version, 53)
            self.assertFalse(site.current['running'])
            self.assertTrue((site.gate / 'maintenance').exists())
            self.assertEqual(result['schemaCommands']['rollback']['observedAfter']['schema_state'], 'schema53')
            site.rollback_mode = None
            recovered = site.resume()
            self.assertEqual(recovered['phase'], 'rolled_back_same52', recovered.get('takeoverErrorCode'))
            self.assertEqual(recovered['schemaCommands']['rollback']['outcome'], 'failed')
            self.assertEqual(recovered['schemaCommands']['rollback_takeover']['outcome'], 'completed')
            self.assertEqual(sum(event[0] == 'rollback' for event in site.schema_events), 2)

    def test_unknown_compose_takeover_recovers_candidate_generation_then_schema52(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            site.prepare()
            original = site.command
            def lost_ack(args, **kwargs):
                result = original(args, **kwargs)
                if args[:2] == ['docker', 'compose'] and 'up' in args and site.pin == IMAGE:
                    raise RuntimeError('command_result_unknown')
                return result
            with patch.object(remote, 'command', side_effect=lost_ack):
                result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            result = site.resume()
            self.assertEqual(result['phase'], 'rolled_back_same52', result.get('takeoverErrorCode'))
            self.assertEqual(len(result['stoppedRecovery']), 2)
            self.assertEqual(sum(event[0] == 'rollback' for event in site.schema_events), 1)


class ProductCarrierTest(unittest.TestCase):
    def test_exact_bundle_is_provisioned_with_consumer_module(self):
        seen = []
        def ssh(code, payload, **kwargs):
            seen.append(payload)
            return {'phase': 'provisioned'}
        with patch.object(controller, 'ssh', side_effect=ssh):
            controller.provision(accepted(), '/unused', schema53.BUNDLE)
        payload = seen[0]
        self.assertIn('schema53.py', payload['files'])
        for role in schema53.ROLES:
            name = accepted()['schema53Contract']['artifacts'][role]['name']
            self.assertEqual(base64.b64decode(payload['files']['schema53-artifacts/' + name]),
                             (schema53.BUNDLE / name).read_bytes())

    def test_product_action_classifies_sqlstate_unknown_and_single_final_json(self):
        value = accepted()['schema53Contract']
        outcomes = [SimpleNamespace(returncode=0, stdout=json.dumps(observation()).encode(), stderr=b''),
                    SimpleNamespace(returncode=3, stdout=b'', stderr=b'ERROR:  57014\n'),
                    SimpleNamespace(returncode=1, stdout=b'', stderr=b'connection lost'),
                    SimpleNamespace(returncode=0, stdout=b'{}\n{}', stderr=b'')]
        for index, outcome in enumerate(outcomes):
            with self.subTest(index=index), patch.object(schema53.subprocess, 'run', return_value=outcome) as run:
                if index < 2:
                    result = schema53.execute(schema53.BUNDLE, value, 'prepare', remote.PG, 30)
                    self.assertEqual(result['outcome'], 'completed' if index == 0 else 'failed')
                else:
                    with self.assertRaisesRegex(RuntimeError, 'unknown_no_reissue'):
                        schema53.execute(schema53.BUNDLE, value, 'prepare', remote.PG, 30)
                self.assertEqual(run.call_args.kwargs['input'], (schema53.BUNDLE / 'prebuild.sql').read_bytes())
                self.assertNotIn('--single-transaction', run.call_args.args[0])

    def test_local_prepare_unknown_ack_observes_without_relaunch(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            remote.atomic(run / 'controller.json', {'accepted': accepted(), 'remoteRoot': '/unused',
                                                   'operation': 'prepare', 'pendingOperation': 'prepare',
                                                   'schema53Artifacts': str(schema53.BUNDLE)})
            ctx = MagicMock()
            with patch.object(controller, 'action_lock', return_value=ctx), \
                 patch.object(controller, 'provision'), patch.object(controller, 'pull_exact'), \
                 patch.object(controller, 'start_remote_preparation', side_effect=RuntimeError('unknown')) as start, \
                 patch.object(controller, 'observe', return_value={'phase': 'schema53_prepared'}):
                controller.work(run, 'prepare')
                controller.work(run, 'prepare')
            self.assertEqual(start.call_count, 1)
            self.assertTrue(controller.read(run / 'controller.json')['schemaMayHaveStarted'])

    def test_manifest_source_and_transferred_artifact_identity_are_bound(self):
        value = accepted()
        value['source'] = 'f' * 40
        with self.assertRaisesRegex(RuntimeError, 'runtime_product_source_mismatch'):
            remote.manifest(value)
        with tempfile.TemporaryDirectory() as tmp:
            bundle = Path(tmp) / 'bundle'
            shutil.copytree(schema53.BUNDLE, bundle)
            path = bundle / 'inspect.sql'
            path.write_bytes(path.read_bytes() + b'\n')
            with self.assertRaisesRegex(RuntimeError, 'artifact_drift_inspect'):
                schema53.inspect(bundle, accepted()['schema53Contract'], lambda _: '{}')

    def test_active_database_action_blocks_cleanup_without_issuing_sql(self):
        with tempfile.TemporaryDirectory() as tmp, SchemaSite(tmp) as site:
            site.index = 'drop_incomplete'
            site.active = [{'pid': 90, 'application_name': 'cpr:schema53:cleanup-invalid'}]
            result = site.prepare(cleanup=True)
            self.assertEqual(result['phase'], 'schema53_preparation_requires_operator')
            self.assertEqual(result['errorCode'], 'schema53_original_database_action_still_active')
            self.assertEqual(site.schema_events, [])
            self.assertTrue(site.current['running'])
            self.assertFalse((site.gate / 'maintenance').exists())

    def test_local_prepare_activate_retains_phase_receipts_and_only_launches_each_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            prepared = {'phase': 'schema53_prepared'}
            deployed = {'phase': 'deployed'}
            remote.atomic(run / 'controller.json', {'accepted': accepted(), 'remoteRoot': '/unused',
                                                   'operation': 'prepare', 'pendingOperation': 'prepare',
                                                   'schema53Artifacts': str(schema53.BUNDLE)})
            with patch.object(controller, 'action_lock', return_value=MagicMock()), \
                 patch.object(controller, 'provision') as provision, patch.object(controller, 'pull_exact') as pull, \
                 patch.object(controller, 'start_remote_preparation', return_value={}) as prepare, \
                 patch.object(controller, 'start_remote', return_value={}) as activate, \
                 patch.object(controller, 'observe', side_effect=[prepared, deployed]), \
                 patch.object(controller, 'verify_remote', return_value={'phase': 'verified'}), \
                 patch.object(controller, 'reconcile_preparation', return_value=prepared), \
                 patch.object(controller, 'spawn_supervisor', return_value={}), \
                 patch.object(controller.subprocess, 'Popen') as child:
                child.return_value.returncode = 0
                controller.work(run, 'prepare')
                controller.supervise(run, 'prepare')
                original = (run / 'supervisor-prepare.json').read_bytes()
                controller.continue_schema53(run, 'activate')
                controller.work(run, 'activate')
                controller.supervise(run, 'activate')
                with self.assertRaisesRegex(RuntimeError, 'already_activated_or_fenced'):
                    controller.continue_schema53(run, 'activate')
            value = controller.read(run / 'controller.json')
            self.assertEqual(value['prepareReceipt'], prepared)
            self.assertEqual(value['remote'], deployed)
            self.assertEqual((run / 'supervisor-prepare.json').read_bytes(), original)
            self.assertTrue((run / 'supervisor-activate.json').exists())
            self.assertEqual((provision.call_count, pull.call_count, prepare.call_count, activate.call_count), (1, 1, 1, 1))

    def test_supervisor_phase_is_captured_before_next_authorization_takes_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            remote.atomic(run / 'controller.json', {'accepted': accepted(), 'operation': 'prepare',
                                                   'pendingOperation': 'prepare', 'phase': 'finished'})
            @contextmanager
            def lock(*args):
                yield
                value = controller.read(run / 'controller.json')
                value.update(operation='activate', phase='activate_authorized')
                remote.atomic(run / 'controller.json', value)
            with patch.object(controller, 'action_lock', side_effect=lock), \
                 patch.object(controller.subprocess, 'Popen') as child:
                child.return_value.returncode = 0
                controller.supervise(run, 'prepare')
            self.assertEqual(controller.read(run / 'supervisor-prepare.json')['phase'], 'finished')
            self.assertFalse((run / 'supervisor-activate.json').exists())
            self.assertEqual(controller.read(run / 'controller.json')['phase'], 'activate_authorized')


if __name__ == '__main__':
    unittest.main()
