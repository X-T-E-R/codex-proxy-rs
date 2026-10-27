"""隔离模拟 Docker/PG/恢复闸的单次切换与失败收据；绝不连接 lag。"""
import datetime as dt
import importlib.util
import itertools
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, MagicMock

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('task40_remote', HERE / 'remote.py')
remote = importlib.util.module_from_spec(spec)
with patch.dict('sys.modules', {'fcntl': MagicMock()}):
    spec.loader.exec_module(remote)
controller_spec = importlib.util.spec_from_file_location('task40_controller', HERE / 'controller.py')
controller = importlib.util.module_from_spec(controller_spec)
with patch.dict('sys.modules', {'remote': remote}):
    controller_spec.loader.exec_module(controller)

SOURCE = 'b' * 40
IMAGE = 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:' + 'c' * 64


def manifest():
    return {'accepted': True, 'schema': 52, 'noMigrationChanges': True,
            'hardCutAuthorized': True, 'source': SOURCE, 'image': IMAGE,
            'migrationEvidence': '52 frozen migrations from exact source',
            'acceptanceEvidence': 'same-image isolated API and recovery acceptance'}


def runtime(id, revision, image_id, running=True, health='healthy'):
    return {'id': id, 'revision': revision, 'imageId': image_id, 'running': running,
            'health': health, 'pid': 2 if running else 0,
            'startedAt': '2026-10-25T12:00:00Z' if id == 'old' else remote.now(),
            'project': remote.SERVICE, 'service': remote.SERVICE,
            'workingDir': '/opt/codex-proxy-rs', 'composeFiles': '/opt/codex-proxy-rs/docker-compose.yml'}


def facts(running=0):
    return dict(schemaMax=52, schemaCount=52, schemaSuccess=True, accounts=6, keys=5,
                groups=5, ledgerRows=50, budgetRows=1, resetRows=0, settings={'revision': 400},
                quotaGlobal={'revision': 1}, quotaModes={'inherit': 1}, priority={'mode': 'enabled'}, running=running,
                **{name: 'a' * 32 for name in ('settingsPolicy', 'quotaAccountPolicy', 'proxyPolicy',
                   'accountIdentity', 'keyIdentity', 'groupIdentity', 'accountMembership', 'keyMembership',
                   'pluginIdentity', 'pluginGroups', 'pluginKeys')})


class MockSite:
    def __init__(self, tmp, fail=None):
        self.tmp, self.fail = Path(tmp), fail
        self.base = self.tmp / 'base'
        self.base.mkdir()
        self.run = self.base / '.runtime/deployments' / ('task40-' + '1' * 32)
        self.run.mkdir(parents=True)
        (self.run / 'accepted.json').write_text(json.dumps(manifest()))
        (self.base / '.env').write_bytes(b'A=secret_not_read\nCPR_IMAGE=' + remote.OLD_IMAGE.encode() + b'\nZ=opaque\r\n')
        (self.base / 'deploy').mkdir()
        (self.base / 'deploy/config.yaml').write_text('opaque')
        (self.base / 'docker-compose.yml').write_text('opaque')
        self.gate = self.base / '.runtime/restart-recovery'
        self.gate.mkdir()
        self.old = runtime('old', remote.OLD_SOURCE, 'sha256:' + 'a' * 64)
        self.candidate = runtime('new', SOURCE, 'sha256:' + 'c' * 64)
        for app in (self.old, self.candidate):
            app['workingDir'] = str(self.base)
            app['composeFiles'] = str(self.base / 'docker-compose.yml')
        self.current = self.old
        self.pin = remote.OLD_IMAGE
        self.compose_target_override = None
        self.events = []
        self.inspect_failed = False
        self.unknown_once = False
        self.binding = {'runtime': {k: self.old[k] for k in ('id', 'project', 'service', 'revision')},
                        'helperSource': remote.HELPER_SOURCE, 'helperImage': remote.HELPER_IMAGE,
                        'singleDeployment': True,
                        'recoveryCommand': ['python3', str(self.gate / 'site-recovery.py')]}
        (self.gate / 'binding.json').write_text(json.dumps(self.binding))
        self.patches = []

    def __enter__(self):
        self.patches = [patch.object(remote, 'BASE', self.base),
                        patch.object(remote, 'container', side_effect=self.container),
                        patch.object(remote, 'image', side_effect=self.image),
                        patch.object(remote, 'facts', side_effect=self.facts),
                        patch.object(remote, 'sql', return_value='0'),
                        patch.object(remote, 'health', side_effect=self.health),
                        patch.object(remote, 'command', side_effect=self.command),
                        patch.object(remote, 'shutil'),
                        patch.object(remote, 'time')]
        for p in self.patches:
            obj = p.start()
            if p.attribute == 'shutil':
                obj.disk_usage.return_value.free = 8 * 1024**3
            if p.attribute == 'time':
                obj.monotonic.side_effect = itertools.count(1)
                obj.sleep.return_value = None
        return self

    def __exit__(self, *args):
        for p in reversed(self.patches):
            p.stop()

    def container(self):
        if self.fail == 'inspect_then_unhealthy' and self.pin == IMAGE and self.current['id'] != 'old' and not self.inspect_failed:
            self.inspect_failed = True
            raise RuntimeError('command_result_unknown')
        return self.current

    def image(self, value, source):
        return {'id': self.candidate['imageId'] if source == SOURCE else self.old['imageId']}

    def facts(self):
        value = facts()
        if self.fail == 'candidate_drift' and self.pin == IMAGE and self.current['running']:
            value['settings']['revision'] = 2
        return value

    def health(self):
        if self.fail == 'candidate_exit' and self.pin == IMAGE:
            self.current = dict(self.current, running=False, health='unhealthy')
            return False
        return self.fail not in ('candidate_unhealthy', 'inspect_then_unhealthy', 'rollback_up_unknown') or self.pin == remote.OLD_IMAGE

    def command(self, args, **kwargs):
        self.events.append(args)
        if args[:2] == ['docker', 'inspect']:
            return b'"no"'
        if args[:2] == ['systemctl', 'is-enabled']:
            return b'enabled'
        if args[:2] == ['systemctl', 'is-active']:
            return b'active'
        if args[0] == 'nsenter':
            return b'connected\n'
        if args[:2] == ['docker', 'stop']:
            if self.fail in ('stop_unknown', 'stop_unknown_stopped'):
                if self.fail == 'stop_unknown_stopped':
                    self.current = dict(self.current, running=False, health='unhealthy')
                raise RuntimeError('command_result_unknown')
            self.current = dict(self.current, running=False, health='unhealthy')
            return b''
        if args[0] == 'python3' and args[1] == str(self.gate / 'site-recovery.py'):
            if self.fail == 'recovery_unknown':
                raise RuntimeError('command_result_unknown')
            target = self.current
            receipt = Path(args[args.index('--receipt') + 1])
            receipt.write_text(json.dumps({'phase': 'complete', 'owner': {'Id': target['id'], 'StartedAt': target['startedAt']},
                                           'completed': ['req1', 'req2'], 'released': {'accounts': 2, 'clients': 2}}))
            return b'{}'
        if args[:2] == ['docker', 'compose']:
            if 'config' in args:
                assert args[-3:] == ['config', '--format', 'json']
                wrong_after_pin = self.fail == 'compose_service_wrong_after_pin' and self.pin == IMAGE
                return json.dumps({'services': {remote.SERVICE: {
                    'image': self.compose_target_override or (remote.OLD_IMAGE if wrong_after_pin else self.pin)},
                    'postgres': {'image': self.pin if wrong_after_pin or self.compose_target_override else
                                 remote.OLD_IMAGE}}}).encode()
            if self.fail == 'up_unknown' or (self.fail == 'rollback_up_unknown' and self.pin == remote.OLD_IMAGE):
                if self.fail == 'rollback_up_unknown':
                    self.current = dict(self.old, id='old-recreated', startedAt=remote.now())
                raise RuntimeError('command_result_unknown')
            self.current = self.candidate if self.pin == IMAGE else self.old
            self.current = dict(self.current, id=self.current['id'] + '-recreated', startedAt=remote.now())
            return b''
        raise AssertionError(args)

    def deploy(self):
        with patch.object(remote, 'set_pin', side_effect=self.set_pin):
            return remote.Deployment(self.run).execute()

    def set_pin(self, path, old, new):
        assert self.pin == old
        if self.fail == 'pin_failure' and new == IMAGE:
            raise RuntimeError('pin_write_failed_before_replace')
        self.pin = new
        lines = path.read_bytes().splitlines(keepends=True)
        lines[1] = b'CPR_IMAGE=' + new.encode() + b'\n'
        path.write_bytes(b''.join(lines))


class ContractTest(unittest.TestCase):
    def test_manifest_rejects_prior_and_unaccepted(self):
        m = manifest()
        for field, value in [('source', remote.OLD_SOURCE), ('image', remote.OLD_IMAGE),
                             ('schema', 53), ('accepted', False), ('hardCutAuthorized', False),
                             ('migrationEvidence', '')]:
            bad = dict(m, **{field: value})
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                remote.manifest(bad)

    def test_read_only_baseline_composes_old_and_facts(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            with patch.object(remote, 'set_pin', side_effect=AssertionError('write forbidden')):
                (site.gate / 'lifecycle.lock').touch()
                observed = remote.baseline()
            self.assertEqual(observed['runtime']['id'], 'old')
            self.assertEqual(observed['schemaAndPreservationFacts']['schemaMax'], 52)
            self.assertEqual(observed['serviceImage'], remote.OLD_IMAGE)
            self.assertFalse((site.gate / 'maintenance').exists())
            self.assertFalse(any(e[:2] == ['docker', 'stop'] for e in site.events))

    def test_target_service_image_cannot_be_substituted_by_another_service(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            # postgres 仍声明旧镜像；目标服务换成候选，不能凭全项目镜像列表蒙混
            site.compose_target_override = IMAGE
            result = site.deploy()
            self.assertEqual(result['phase'], 'blocked_unchanged')
            self.assertEqual(result['errorCode'], 'target_service_image_or_pin_drift')
            self.assertFalse(any(e[:2] == ['docker', 'stop'] for e in site.events))
            with self.assertRaisesRegex(RuntimeError, 'baseline_compose_pin_drift'):
                remote.baseline()

    def test_candidate_pin_checks_its_service_not_other_service(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'compose_service_wrong_after_pin') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'rolled_back_same52')
            self.assertEqual(result['errorCode'], 'compose_pin_drift')
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'compose'] and 'up' in e]), 1)
            self.assertEqual(result['rollbackRuntime']['revision'], remote.OLD_SOURCE)

    def test_image_requires_digest_in_registry_binding(self):
        metadata = json.dumps({'id': 'sha256:' + 'c' * 64, 'os': 'linux', 'arch': 'amd64', 'revision': SOURCE}).encode()
        with patch.object(remote, 'command', side_effect=[metadata, b'[]']):
            with self.assertRaisesRegex(RuntimeError, 'registry_digest_not_bound'):
                remote.image(IMAGE, SOURCE)
        with patch.object(remote, 'command', side_effect=[metadata, json.dumps([IMAGE]).encode()]):
            self.assertEqual(remote.image(IMAGE, SOURCE)['revision'], SOURCE)

    def test_pin_byte_preservation_and_unique_authority(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / '.env'
            path.write_bytes(b'A=secret\r\nCPR_IMAGE=' + remote.OLD_IMAGE.encode() + b'\r\nB=opaque')
            with patch.object(remote.os, 'chown', create=True):
                remote.set_pin(path, remote.OLD_IMAGE, IMAGE)
            self.assertEqual(path.read_bytes(), b'A=secret\r\nCPR_IMAGE=' + IMAGE.encode() + b'\r\nB=opaque')
            path.write_bytes(path.read_bytes() + b'\nCPR_IMAGE=' + IMAGE.encode())
            with self.assertRaisesRegex(RuntimeError, 'unique_CPR_IMAGE'):
                remote.pin(path)

    def test_success_exact_recovery_and_gate_rebind(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'deployed')
            self.assertEqual(result['stoppedRecovery'][0]['containerId'], 'old')
            self.assertEqual(result['stoppedRecovery'][0]['requests'], 2)
            self.assertEqual(result['restartRecoveryGate']['phase'], 'rebound')
            self.assertFalse((site.gate / 'maintenance').exists())
            self.assertEqual(site.pin, IMAGE)
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'stop']]), 1)
            (site.run / 'launch.guard').touch()
            (site.base / '.runtime/deployments/task40-active.lock').touch()
            (site.gate / 'lifecycle.lock').touch()
            receipt = remote.verify(site.run)
            self.assertEqual(receipt['runtime']['id'], 'new-recreated')
            self.assertEqual(receipt['locks'], 'unheld')

    def test_fresh_verify_checks_target_service_image_when_other_service_has_candidate(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            self.assertEqual(site.deploy()['phase'], 'deployed')
            for path in (site.run / 'launch.guard', site.base / '.runtime/deployments/task40-active.lock',
                         site.gate / 'lifecycle.lock'):
                path.touch(exist_ok=True)
            site.compose_target_override = remote.OLD_IMAGE
            with self.assertRaisesRegex(RuntimeError, 'fresh_compose_drift'):
                remote.verify(site.run)

    def test_binding_write_failure_never_publishes_success(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            original = remote.atomic
            def fail_binding(path, value):
                if path == site.gate / 'binding.json':
                    raise OSError('binding write fault')
                return original(path, value)
            with patch.object(remote, 'atomic', side_effect=fail_binding):
                result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertFalse(result['success'])
            self.assertEqual(result['restartRecoveryGate']['phase'], 'maintenance_requires_operator')
            self.assertTrue((site.gate / 'maintenance').exists())
            self.assertEqual(json.loads((site.run / 'result.json').read_text())['success'], False)

    def test_maintenance_owner_lost_before_terminal_is_operator_not_success(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            original = remote.Deployment.save
            def remove_owner_at_candidate_verified(app, phase):
                original(app, phase)
                if phase == 'candidate_verified_pending_gate':
                    (site.gate / 'maintenance').unlink()
            with patch.object(remote.Deployment, 'save', new=remove_owner_at_candidate_verified):
                result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertFalse(result['success'])
            self.assertEqual(result['restartRecoveryGate']['phase'], 'gate_owner_requires_operator')

    def test_takeover_after_gate_write_failure_rolls_back_and_rebinds(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            original = remote.atomic
            def fail_binding(path, value):
                if path == site.gate / 'binding.json':
                    raise OSError('injected binding write fault')
                return original(path, value)
            with patch.object(remote, 'atomic', side_effect=fail_binding):
                initial = site.deploy()
            self.assertEqual(initial['phase'], 'recovery_requires_operator')
            (site.run / 'attempt-claimed.json').touch()
            with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                resumed = remote.Deployment(site.run).resume()
            self.assertEqual(resumed['phase'], 'rolled_back_same52', resumed.get('takeoverErrorCode'))
            self.assertTrue(resumed['rollback'])
            self.assertEqual(resumed['restartRecoveryGate']['phase'], 'rebound')
            self.assertFalse((site.gate / 'maintenance').exists())
            self.assertEqual(json.loads((site.run / 'result.json').read_text())['success'], False)
            (site.run / 'launch.guard').touch()
            (site.base / '.runtime/deployments/task40-active.lock').touch()
            (site.gate / 'lifecycle.lock').touch(exist_ok=True)
            self.assertEqual(remote.verify(site.run)['runtime']['revision'], remote.OLD_SOURCE)

    def test_takeover_after_runner_exits_without_result_uses_progress(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            with patch.object(remote.Deployment, 'settle_gate', side_effect=SystemExit('runner_exit')):
                with self.assertRaises(SystemExit):
                    site.deploy()
            self.assertFalse((site.run / 'result.json').exists())
            self.assertEqual(json.loads((site.run / 'progress.json').read_text())['phase'], 'candidate_verified_pending_gate')
            (site.run / 'attempt-claimed.json').touch()
            with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                result = remote.Deployment(site.run).resume()
            self.assertEqual(result['phase'], 'rolled_back_same52', result.get('takeoverErrorCode'))
            self.assertEqual(result['restartRecoveryGate']['phase'], 'rebound')

    def test_takeover_after_gate_released_but_before_result_uses_fresh_facts(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            original = remote.Deployment.settle_gate
            def exit_after_release(app):
                original(app)
                raise SystemExit('runner_exit_after_release')
            with patch.object(remote.Deployment, 'settle_gate', new=exit_after_release):
                with self.assertRaises(SystemExit):
                    site.deploy()
            self.assertFalse((site.run / 'result.json').exists())
            self.assertFalse((site.gate / 'maintenance').exists())
            (site.run / 'attempt-claimed.json').touch()
            actions_before = len(site.events)
            result = remote.Deployment(site.run).resume()
            self.assertEqual(result['phase'], 'deployed', result.get('takeoverErrorCode'))
            self.assertTrue(result['success'])
            self.assertEqual(result['restartRecoveryGate']['phase'], 'rebound')
            self.assertFalse(any(e[:2] == ['docker', 'compose'] and 'up' in e
                                 for e in site.events[actions_before:]))

    def test_takeover_missing_maintenance_without_matching_binding_stays_operator(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            with patch.object(remote.Deployment, 'settle_gate', side_effect=SystemExit('runner_exit')):
                with self.assertRaises(SystemExit):
                    site.deploy()
            (site.gate / 'maintenance').unlink()
            (site.run / 'attempt-claimed.json').touch()
            result = remote.Deployment(site.run).resume()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertFalse(result['success'])
            self.assertEqual(result['restartRecoveryGate']['phase'], 'released_requires_operator')
            self.assertEqual(result['takeoverErrorCode'], 'takeover_settled_gate_or_compose_drift')

    def test_unknown_stop_holds_gate_and_never_reissues(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'stop_unknown') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertEqual(result['stopCommand']['outcome'], 'unknown')
            self.assertTrue((site.gate / 'maintenance').exists())
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'stop']]), 1)

    def test_takeover_unknown_stop_observed_stopped_recovers_once(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'stop_unknown_stopped') as site:
            first = site.deploy()
            self.assertEqual(first['stopCommand']['outcome'], 'unknown')
            (site.run / 'attempt-claimed.json').touch()
            with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                resumed = remote.Deployment(site.run).resume()
            self.assertEqual(resumed['phase'], 'rolled_back_same52', resumed.get('takeoverErrorCode'))
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'stop']]), 1)
            self.assertEqual(len([e for e in site.events if e[:2] == ['python3', str(site.gate / 'site-recovery.py')]]), 1)

    def test_takeover_rejects_drifted_binding_before_mutation(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'stop_unknown_stopped') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            drifted = json.loads((site.gate / 'binding.json').read_text())
            drifted['runtime']['id'] = 'alien'
            (site.gate / 'binding.json').write_text(json.dumps(drifted))
            resumed = remote.Deployment(site.run).resume()
            self.assertEqual(resumed['takeoverErrorCode'], 'takeover_binding_runtime_drift')
            self.assertFalse(any(e[0] == 'python3' for e in site.events))
            self.assertTrue((site.gate / 'maintenance').exists())

    def test_takeover_rejects_opaque_env_change_before_recovery(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'stop_unknown_stopped') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            env = site.base / '.env'
            env.write_bytes(env.read_bytes().replace(b'Z=opaque', b'Z=change'))
            result = remote.Deployment(site.run).resume()
            self.assertEqual(result['takeoverErrorCode'], 'takeover_env_opaque_drift')
            self.assertFalse(any(e[0] == 'python3' for e in site.events))
            self.assertTrue((site.gate / 'maintenance').exists())

    def test_new_go_retries_readonly_failure_on_same_run_after_exact_env_restoration(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'stop_unknown_stopped') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            env = site.base / '.env'
            original = env.read_bytes()
            env.write_bytes(original.replace(b'Z=opaque', b'Z=change'))
            remote.atomic(site.run / 'controller.json', {'remoteMayHaveStarted': True,
                          'remoteRoot': '/opt/codex-proxy-rs/.runtime/deployments/' + site.run.name,
                          'phase': 'finished'})
            ctx = MagicMock()
            ctx.__enter__.return_value = None
            ctx.__exit__.return_value = False
            def observed(_):
                result = site.run / 'takeover-retry-result.json'
                if not result.exists():
                    result = site.run / 'takeover-result.json'
                return json.loads(result.read_text()) if result.exists() else {'phase': 'recovery_requires_operator'}
            def launch(_, retry=False):
                with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                    return remote.Deployment(site.run).resume(retry_readonly=retry)
            with patch.object(controller, 'action_lock', return_value=ctx), \
                 patch.object(controller, 'receipt', side_effect=observed), \
                 patch.object(controller, 'start_remote_takeover', side_effect=launch):
                first = controller.takeover(site.run)
                self.assertFalse(first['retryReadOnly'])
                failed = json.loads((site.run / 'takeover-result.json').read_text())
                self.assertTrue(failed['retryableReadOnly'])
                self.assertFalse(failed['takeoverCommandsIssued'])
                self.assertEqual(failed['takeoverErrorCode'], 'takeover_env_opaque_drift')
                self.assertFalse(any(e[0] == 'python3' for e in site.events))
                env.write_bytes(original)
                second = controller.takeover(site.run)
                self.assertTrue(second['retryReadOnly'])
                result = json.loads((site.run / 'takeover-retry-result.json').read_text())
                self.assertEqual(result['phase'], 'rolled_back_same52', result.get('takeoverErrorCode'))
                self.assertNotIn('takeoverErrorCode', result)
                self.assertFalse((site.gate / 'maintenance').exists())
                self.assertEqual(len([e for e in site.events if e[0] == 'python3']), 1)
                with self.assertRaisesRegex(RuntimeError, 'takeover_run_owner_or_claim_drift'):
                    controller.takeover(site.run)

    def test_readonly_retry_still_never_replays_original_unknown_helper(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'recovery_unknown') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            env = site.base / '.env'
            original = env.read_bytes()
            env.write_bytes(original.replace(b'Z=opaque', b'Z=change'))
            first = remote.Deployment(site.run).resume()
            self.assertTrue(first['retryableReadOnly'])
            env.write_bytes(original)
            second = remote.Deployment(site.run).resume(retry_readonly=True)
            self.assertEqual(second['phase'], 'recovery_requires_operator')
            self.assertEqual(second['takeoverErrorCode'], 'recovery_claimed_wait_exact_receipt_no_reissue')
            self.assertFalse(second['retryableReadOnly'])
            self.assertEqual(len([e for e in site.events if e[0] == 'python3']), 1)

    def test_unknown_recovery_does_not_start_or_recover_twice(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'recovery_unknown') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertEqual(len([e for e in site.events if e[:2] == ['python3', str(site.gate / 'site-recovery.py')]]), 1)
            self.assertFalse(any(e[:2] == ['docker', 'compose'] and 'up' in e for e in site.events))
            self.assertTrue((site.gate / 'maintenance').exists())

    def test_takeover_unknown_helper_waits_for_original_complete_receipt(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'recovery_unknown') as site:
            first = site.deploy()
            self.assertEqual(first['phase'], 'recovery_requires_operator')
            (site.run / 'attempt-claimed.json').touch()
            second = remote.Deployment(site.run).resume()
            self.assertEqual(second['phase'], 'recovery_requires_operator')
            self.assertEqual(second['takeoverErrorCode'], 'recovery_claimed_wait_exact_receipt_no_reissue')
            self.assertTrue(second['takeoverCommandsIssued'])
            self.assertFalse(second['retryableReadOnly'])
            with self.assertRaisesRegex(RuntimeError, 'takeover_retry_not_readonly'):
                remote.Deployment(site.run).resume(retry_readonly=True)
            self.assertEqual(len([e for e in site.events if e[:2] == ['python3', str(site.gate / 'site-recovery.py')]]), 1)
            self.assertTrue((site.gate / 'maintenance').exists())

    def test_takeover_prepared_receipt_never_reissues_helper(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'recovery_unknown') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            (site.run / 'stopped-recovery-old.json').write_text(json.dumps({'phase': 'prepared',
                'owner': {'Id': site.old['id'], 'StartedAt': site.old['startedAt']}}))
            result = remote.Deployment(site.run).resume()
            self.assertEqual(result['takeoverErrorCode'], 'recovery_claimed_wait_exact_receipt_no_reissue')
            self.assertEqual(len([e for e in site.events if e[:2] == ['python3', str(site.gate / 'site-recovery.py')]]), 1)

    def test_takeover_consumes_original_complete_receipt_without_helper_replay(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'recovery_unknown') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            path = site.run / 'stopped-recovery-old.json'
            path.write_text(json.dumps({'phase': 'complete', 'owner': {'Id': site.old['id'],
                'StartedAt': site.old['startedAt']}, 'completed': ['req1'], 'released': {'accounts': 1}}))
            with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                resumed = remote.Deployment(site.run).resume()
            self.assertEqual(resumed['phase'], 'rolled_back_same52', resumed.get('takeoverErrorCode'))
            self.assertEqual(len([e for e in site.events if e[:2] == ['python3', str(site.gate / 'site-recovery.py')]]), 1)

    def test_unknown_compose_holds_gate_without_retry(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'up_unknown') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'compose'] and 'up' in e]), 1)
            self.assertTrue((site.gate / 'maintenance').exists())

    def test_takeover_unknown_compose_without_candidate_never_reissues(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'up_unknown') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            result = remote.Deployment(site.run).resume()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertEqual(result['takeoverErrorCode'], 'candidate_compose_outcome_unobserved_no_reissue')
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'compose'] and 'up' in e]), 1)

    def test_takeover_unknown_compose_observes_candidate_then_rolls_back(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'up_unknown') as site:
            site.deploy()
            (site.run / 'attempt-claimed.json').touch()
            site.current = dict(site.candidate, id='new-recreated', startedAt=remote.now())
            site.fail = None
            with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                result = remote.Deployment(site.run).resume()
            self.assertEqual(result['phase'], 'rolled_back_same52', result.get('takeoverErrorCode'))
            self.assertEqual(result['stoppedRecovery'][-1]['containerId'], 'new-recreated')
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'compose'] and 'up' in e]), 2)

    def test_takeover_unknown_rollback_up_observes_exact_old_image_no_reissue(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'rollback_up_unknown') as site:
            first = site.deploy()
            self.assertEqual(first['phase'], 'recovery_requires_operator')
            (site.run / 'attempt-claimed.json').touch()
            with patch.object(remote, 'set_pin', side_effect=site.set_pin):
                result = remote.Deployment(site.run).resume()
            self.assertEqual(result['phase'], 'rolled_back_same52', result.get('takeoverErrorCode'))
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'compose'] and 'up' in e]), 2)
            self.assertEqual(result['restartRecoveryGate']['phase'], 'rebound')

    def test_failed_candidate_health_rolls_back_after_same_epoch_recovery(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'candidate_unhealthy') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'rolled_back_same52')
            self.assertEqual([x['containerId'] for x in result['stoppedRecovery']], ['old', 'new-recreated'])
            self.assertEqual(result['rollbackRuntime']['revision'], remote.OLD_SOURCE)
            self.assertEqual(site.pin, remote.OLD_IMAGE)
            self.assertEqual(result['restartRecoveryGate']['phase'], 'rebound')

    def test_compose_ack_first_inspect_unknown_then_owned_candidate_rolls_back(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'inspect_then_unhealthy') as site:
            result = site.deploy()
            self.assertTrue(site.inspect_failed)
            self.assertEqual(result['phase'], 'rolled_back_same52')
            self.assertEqual(result['stoppedRecovery'][-1]['containerId'], 'new-recreated')
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'compose'] and 'up' in e]), 2)

    def test_candidate_exits_before_health_recovers_own_epoch(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'candidate_exit') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'rolled_back_same52')
            self.assertEqual([x['containerId'] for x in result['stoppedRecovery']], ['old', 'new-recreated'])
            self.assertEqual(len([e for e in site.events if e[:2] == ['docker', 'stop']]), 1)

    def test_pin_failure_after_old_recovery_restarts_same52_old(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'pin_failure') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'rolled_back_same52')
            self.assertEqual([x['containerId'] for x in result['stoppedRecovery']], ['old'])
            self.assertEqual(result['rollbackRuntime']['revision'], remote.OLD_SOURCE)
            self.assertEqual(result['restartRecoveryGate']['phase'], 'rebound')

    def test_preservation_drift_rolls_back_to_old_pin(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'candidate_drift') as site:
            result = site.deploy()
            self.assertEqual(result['phase'], 'rolled_back_same52')
            self.assertIn('preservation_drift_settings', result['errorCode'])
            self.assertEqual(site.pin, remote.OLD_IMAGE)

    def test_recovery_helper_drift_blocks_before_stop(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            site.binding['helperSource'] = 'd' * 40
            (site.gate / 'binding.json').write_text(json.dumps(site.binding))
            result = site.deploy()
            self.assertEqual(result['phase'], 'preflight_blocked')
            self.assertEqual(result['errorCode'], 'recovery_helper_binding_drift')
            self.assertFalse(any(e[:2] == ['docker', 'stop'] for e in site.events))
            self.assertEqual(result['restartRecoveryGate']['phase'], 'not_entered')
            self.assertFalse((site.gate / 'maintenance').exists())

    def test_gate_occupied_refuses_any_stop(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            (site.gate / 'maintenance').write_text('other')
            result = site.deploy()
            self.assertEqual(result['phase'], 'preflight_blocked')
            self.assertFalse(any(e[:2] == ['docker', 'stop'] for e in site.events))
            self.assertEqual((site.gate / 'maintenance').read_text(), 'other')

    def test_activity_checks_before_maintenance_holds_no_fence_on_failure(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site, \
             patch.object(remote.Deployment, 'activity', side_effect=RuntimeError('activity_unavailable')):
            result = site.deploy()
            self.assertEqual(result['phase'], 'blocked_unchanged')
            self.assertEqual(result['restartRecoveryGate']['phase'], 'not_entered')
            self.assertFalse((site.gate / 'maintenance').exists())

    def test_preservation_detects_settings_drift(self):
        before = facts()
        after = facts()
        after['settings']['revision'] = 2
        with self.assertRaisesRegex(RuntimeError, 'preservation_drift_settings'):
            remote.preserve(before, after)

    def test_equal_counts_identity_disable_and_relationship_drift_are_blocked(self):
        before = facts()
        for key in ('settingsPolicy', 'quotaAccountPolicy', 'proxyPolicy', 'keyIdentity', 'accountIdentity',
                    'groupIdentity', 'accountMembership', 'keyMembership', 'pluginIdentity', 'pluginGroups', 'pluginKeys'):
            after = facts()
            after[key] = 'b' * 32
            with self.subTest(key=key), self.assertRaisesRegex(RuntimeError, 'preservation_drift_' + key):
                remote.preserve(before, after)

    def test_short_lived_request_and_account_observations_are_not_identity(self):
        before = facts(running=3)
        after = facts(running=4)
        after['ledgerRows'] = before['ledgerRows'] + 2
        remote.preserve(before, after)


class SupervisorTest(unittest.TestCase):
    def test_lost_launch_ack_observes_terminal_without_relaunch(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            remote.atomic(run / 'controller.json', {'accepted': manifest(), 'remoteRoot': '/fake/task40-1', 'phase': 'prepared'})
            ctx = MagicMock()
            ctx.__enter__.return_value = None
            ctx.__exit__.return_value = False
            with patch.object(controller, 'action_lock', return_value=ctx), \
                 patch.object(controller, 'provision'), \
                 patch.object(controller, 'pull_exact'), \
                 patch.object(controller, 'start_remote', side_effect=RuntimeError('ssh_result_unknown')) as launch, \
                 patch.object(controller, 'observe', return_value={'phase': 'deployed', 'success': True}) as observe, \
                 patch.object(controller, 'verify_remote', return_value={'phase': 'verified'}):
                controller.work(run)
            journal = controller.read(run / 'controller.json')
            self.assertEqual(journal['phase'], 'finished')
            self.assertTrue(journal['launchAckUnknown'])
            launch.assert_called_once()
            observe.assert_called_once()

    def test_unknown_remote_receipt_never_relaunches(self):
        with patch.object(controller, 'receipt', side_effect=RuntimeError('ssh_unknown')), \
             patch.object(controller, 'abandon_unstarted') as abandon:
            clock = iter([0, 10, 20, 30, 40])
            with self.assertRaisesRegex(RuntimeError, 'remote_receipt_unknown'):
                controller.observe('/fake', seconds=25, clock=lambda: next(clock), sleep=lambda _: None)
            abandon.assert_not_called()

    def test_takeover_launch_ack_unknown_is_single_claim_and_only_observed(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / ('task40-' + '2' * 32)
            run.mkdir()
            root = '/opt/codex-proxy-rs/.runtime/deployments/' + run.name
            remote.atomic(run / 'controller.json', {'remoteMayHaveStarted': True, 'remoteRoot': root})
            ctx = MagicMock()
            ctx.__enter__.return_value = None
            ctx.__exit__.return_value = False
            with patch.object(controller, 'action_lock', return_value=ctx), \
                 patch.object(controller, 'receipt', return_value={'phase': 'recovery_requires_operator'}) as observe, \
                 patch.object(controller, 'start_remote_takeover', side_effect=RuntimeError('ssh_unknown')) as launch:
                first = controller.takeover(run)
                self.assertTrue(first['launchAckUnknown'])
                with self.assertRaisesRegex(RuntimeError, 'takeover_retry_requires_confirmed_no_new_actions'):
                    controller.takeover(run)
                launch.assert_called_once_with(root, retry=False)
                self.assertEqual(observe.call_count, 2)


if __name__ == '__main__':
    unittest.main()
