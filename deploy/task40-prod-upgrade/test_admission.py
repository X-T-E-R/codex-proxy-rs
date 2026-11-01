"""精确满盘事件的只读证据准入、普通新基线与同代际回退模拟，不连接外部服务"""
from contextlib import contextmanager, redirect_stdout
import datetime as dt
import io
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

from test_controller import MockSite, manifest, remote, controller, BASELINE_IMAGE


class Incident:
    def __init__(self, site):
        self.site = site
        self.at = dt.datetime.now(dt.timezone.utc)
        self.failure_at = '2026-10-31T19:34:31.095157+00:00'
        failed = remote.timestamp(self.failure_at)
        recovered = failed + dt.timedelta(seconds=30)
        self.rows = []
        for component in ('probe:redis', *remote.INCIDENT_WORKERS, 'probe:postgres', 'probe:file_logging'):
            reason = (component + ' is unhealthy: state=BackingOff; last_error=worker lease acquisition failed'
                      if component.startswith('worker:') else
                      'probe:postgres is unhealthy: PostgreSQL connection acquisition timed out' if component == 'probe:postgres' else
                      'probe:redis is unhealthy: Redis is unavailable' if component == 'probe:redis' else
                      'probe:file_logging is unhealthy: log completeness compromised: 889 write/flush failures, 0 rejected records (0 warnings/errors) since startup')
            self.rows.append({'at': self.failure_at if component != 'probe:redis' else
                              (failed - dt.timedelta(hours=1)).isoformat(),
                              'component': component, 'reason': reason})
        for component in ('worker:retention:postgres', 'probe:file_logging'):
            prior = next(row for row in self.rows if row['component'] == component)
            self.rows.append(dict(prior, at=(failed - dt.timedelta(hours=2)).isoformat()))
        self.rows.sort(key=lambda row: (row['at'], row['component']))
        self.log = site.base / '.runtime/logs/codex-proxy-rs-application.2026-11-01.23.log'
        self.log.parent.mkdir()
        self.lines = [self.line(failed - dt.timedelta(seconds=120), 'request complete')]
        for row in self.rows:
            if row['at'] == self.failure_at:
                self.lines.append(self.line(remote.timestamp(row['at']), 'health check failed', row['component']))
        for component in (*remote.INCIDENT_WORKERS, 'probe:postgres'):
            self.lines.append(self.line(recovered, 'health component recovered', component))
        self.lines.append(self.line(self.at, 'request complete'))
        self.write_log()
        self.accepted = manifest()
        self.accepted.update(admissionMode='disk_logging_recovery', degradedEvidence={
            'healthFailures': json.loads(json.dumps(self.rows)), 'noCurrentComponentSnapshot': True,
            'boundedObservationRiskAccepted': True, 'postgresSelect': True, 'redisScan': True,
            'recoveredComponents': {component: recovered.isoformat()
                                    for component in (*remote.INCIDENT_WORKERS, 'probe:postgres')},
            'logWindow': {'files': [self.log.name]}, 'evidenceRef': 'parent-reviewed incident evidence'})
        (site.run / 'accepted.json').write_text(json.dumps(self.accepted))
        site.old['health'] = 'unhealthy'
        self.pg = '1'
        self.redis = True

    @staticmethod
    def line(at, message, component=None):
        fields = {'message': message}
        if component:
            fields['component'] = component
        return {'timestamp': at.isoformat(), 'fields': fields, 'target': 'gateway_api::health'}

    def write_log(self):
        self.log.write_text(''.join(json.dumps(row) + '\n' for row in self.lines))

    def sql(self, query):
        if query == remote.HEALTH_FAILURES_SQL:
            return json.dumps(self.rows)
        if query == 'select 1;':
            return self.pg
        return '0'

    def command(self, args, **kwargs):
        if args[-1] == 'site-recovery-redis':
            self.site.events.append(args)
            if not self.redis:
                raise RuntimeError('command_result_unknown')
            self.assert_scan = kwargs['data'] == b'{"kind":"scan","cursor":0,"reserved":false}'
            return b'["0",[]]'
        return self.site.command(args, **kwargs)

    @contextmanager
    def observe(self):
        with patch.object(remote, 'sql', side_effect=self.sql), \
             patch.object(remote, 'command', side_effect=self.command), \
             patch.object(remote, 'health_status', return_value=503):
            yield self


class AdmissionTest(unittest.TestCase):
    def test_exact_local_image_avoids_pull_and_unknown_inspect_never_starts_network(self):
        for mode in ('cached', 'missing', 'unknown', 'wrong_revision'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                m = manifest()
                (root / 'accepted.json').write_text(json.dumps(m))
                (root / 'remote.py').write_text((Path(__file__).parent / 'remote.py').read_text(encoding='utf-8'), encoding='utf-8')
                calls = []
                def run(args, **kwargs):
                    calls.append(args)
                    code, stderr, output = 0, b'', b'{}'
                    if args[:2] == ['docker', 'pull']:
                        pass
                    elif args[-2] == '{{json .Id}}':
                        if mode == 'missing':
                            code, stderr = 1, b'Error: No such image: exact'
                        elif mode == 'unknown':
                            code, stderr = 125, b'daemon unavailable'
                    elif args[-2] == '{{json .RepoDigests}}':
                        output = json.dumps([m['image']]).encode()
                    else:
                        output = json.dumps({'id': 'sha256:' + 'c' * 64, 'os': 'linux', 'arch': 'amd64',
                                             'revision': 'd' * 40 if mode == 'wrong_revision' else m['source']}).encode()
                    return SimpleNamespace(returncode=code, stderr=stderr, stdout=output)
                def ssh(code, payload, **kwargs):
                    output = io.StringIO()
                    with redirect_stdout(output):
                        exec(code, {'payload': payload})
                    return json.loads(output.getvalue())
                with patch.object(controller, 'ssh', side_effect=ssh), patch('subprocess.run', side_effect=run):
                    if mode in ('unknown', 'wrong_revision'):
                        with self.assertRaises(RuntimeError):
                            controller.pull_exact(str(root))
                    else:
                        controller.pull_exact(str(root))
                self.assertEqual(sum(args[:2] == ['docker', 'pull'] for args in calls), int(mode == 'missing'))

    def assert_no_stop(self, site, result):
        self.assertFalse(result['success'])
        self.assertFalse(any(args[:2] == ['docker', 'stop'] for args in site.events))
        self.assertFalse((site.gate / 'maintenance').exists())

    def test_degraded_evidence_permits_exact_old_only_and_target_remains_healthy(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            incident = Incident(site)
            with incident.observe():
                (site.gate / 'lifecycle.lock').touch()
                observed = remote.baseline('disk_logging_recovery')
                self.assertEqual(observed['healthz'], 503)
                self.assertTrue(observed['degradedEvidence']['postgresSelect'])
                result = site.deploy()
            self.assertEqual(result['phase'], 'deployed', result.get('errorCode'))
            self.assertEqual(result['healthz'], 204)
            self.assertTrue(incident.assert_scan)
            self.assertEqual(result['beforeCutHealthAdmission']['healthz'], 503)
            self.assertEqual([row['containerId'] for row in result['stoppedRecovery']], [site.old['id']])
            self.assertFalse((site.gate / 'maintenance').exists())

    def test_degraded_candidate_failure_recovers_its_generation_then_rolls_back_35c(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'candidate_unhealthy') as site:
            incident = Incident(site)
            with incident.observe():
                result = site.deploy()
            self.assertEqual(result['phase'], 'rolled_back_same52', result.get('recoveryErrorCode'))
            self.assertEqual(site.pin, BASELINE_IMAGE)
            self.assertEqual(result['rollbackHealthz'], 204)
            self.assertEqual([row['containerId'] for row in result['stoppedRecovery']],
                             [site.old['id'], 'new-recreated'])
            self.assertEqual(result['rollbackRuntime']['health'], 'healthy')

    def test_degraded_rollback_unhealthy_holds_gate(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'candidate_unhealthy') as site:
            incident = Incident(site)
            with incident.observe(), patch.object(remote, 'health', return_value=False):
                result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertFalse(result['rollback'])
            self.assertFalse(result['success'])
            self.assertTrue((site.gate / 'maintenance').exists())

    def test_new_fault_after_preflight_is_rechecked_before_maintenance(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            incident = Incident(site)
            def activity(app):
                incident.lines.append(incident.line(incident.at, 'health check failed', 'probe:new_unknown'))
                incident.write_log()
                return {'running': 1}
            with incident.observe(), patch.object(remote.Deployment, 'activity', new=activity):
                result = site.deploy()
            self.assert_no_stop(site, result)
            self.assertEqual(result['errorCode'], 'incident_new_or_unrecorded_failure')

    def test_unknown_stop_result_is_not_replayed_from_degraded_mode(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp, 'stop_unknown') as site:
            incident = Incident(site)
            with incident.observe():
                result = site.deploy()
            self.assertEqual(result['phase'], 'recovery_requires_operator')
            self.assertEqual(sum(args[:2] == ['docker', 'stop'] for args in site.events), 1)
            self.assertFalse(any(args[-1] == '--apply' for args in site.events))

    def test_manifest_binds_incident_identity_and_explicit_observation_risk(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            incident = Incident(site)
            for changed in ('containerId', 'startedAt', 'source', 'image', 'noCurrentComponentSnapshot',
                            'boundedObservationRiskAccepted'):
                bad = json.loads(json.dumps(incident.accepted))
                if changed in bad['baseline']:
                    bad['baseline'][changed] = {'containerId': 'a' * 64, 'startedAt': '2026-10-28T00:00:00Z',
                                                'source': 'a' * 40,
                                                'image': 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:' + 'd' * 64}[changed]
                else:
                    bad['degradedEvidence'][changed] = False
                with self.subTest(changed=changed), self.assertRaises(RuntimeError):
                    remote.manifest(bad)

    def test_missing_recovery_unknown_fault_and_changed_count_stop_before_cut(self):
        for fault in ('missing_recovery', 'new_failure', 'new_known_record', 'logging_count', 'queue_gap', 'missing_file'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
                incident = Incident(site)
                if fault == 'missing_recovery':
                    incident.lines = [row for row in incident.lines if row['fields'].get('component') !=
                                      remote.INCIDENT_WORKERS[0] or row['fields']['message'] != 'health component recovered']
                elif fault == 'new_failure':
                    incident.lines.append(incident.line(incident.at, 'health check failed', 'worker:ops_flush:postgres'))
                elif fault == 'new_known_record':
                    incident.rows.append(dict(incident.rows[-1], at=incident.at.isoformat()))
                    incident.accepted['degradedEvidence']['healthFailures'] = incident.rows
                    (site.run / 'accepted.json').write_text(json.dumps(incident.accepted))
                elif fault == 'logging_count':
                    incident.rows[-1]['reason'] += ' changed'
                elif fault == 'queue_gap':
                    incident.lines.append(incident.line(incident.at, 'health diagnostic could not be recorded'))
                incident.write_log()
                if fault == 'missing_file':
                    incident.log.unlink()
                with incident.observe():
                    result = site.deploy()
                self.assert_no_stop(site, result)

    def test_store_unavailable_and_low_space_stop_before_cut(self):
        for fault in ('postgres', 'redis', 'disk'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
                incident = Incident(site)
                if fault == 'postgres':
                    incident.pg = '0'
                elif fault == 'redis':
                    incident.redis = False
                else:
                    remote.shutil.disk_usage.return_value.free = 1024
                with incident.observe():
                    result = site.deploy()
                self.assert_no_stop(site, result)

    def test_identity_change_same_container_new_generation_blocks(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            incident = Incident(site)
            site.old['startedAt'] = '2026-11-01T00:00:00Z'
            with incident.observe():
                result = site.deploy()
            self.assert_no_stop(site, result)
            self.assertEqual(result['errorCode'], 'baseline_runtime_drift')

    def test_ordinary_healthy_mode_does_not_admit_503(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            site.old['health'] = 'unhealthy'
            result = site.deploy()
            self.assert_no_stop(site, result)
            self.assertEqual(result['errorCode'], 'baseline_runtime_drift')

    def test_healthy_manifest_supports_a_new_exact_baseline_without_global_old_pin(self):
        with tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
            b = {'source': 'e' * 40, 'image': 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:' + 'd' * 64,
                 'containerId': 'f' * 64, 'startedAt': '2026-10-30T00:00:00Z'}
            m = manifest()
            m['baseline'] = b
            (site.run / 'accepted.json').write_text(json.dumps(m))
            site.old.update(id=b['containerId'], revision=b['source'], startedAt=b['startedAt'])
            site.binding['runtime'] = {key: site.old[key] for key in ('id', 'project', 'service', 'revision')}
            (site.gate / 'binding.json').write_text(json.dumps(site.binding))
            site.set_pin(site.base / '.env', site.pin, b['image'])
            result = site.deploy()
            self.assertEqual(result['phase'], 'deployed', result.get('errorCode'))
            self.assertEqual(result['beforeRuntime']['revision'], b['source'])

    def test_log_rotation_gap_and_stale_tail_fail_closed(self):
        for fault in ('rotation', 'stale', 'bad_line'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
                incident = Incident(site)
                if fault == 'rotation':
                    older = incident.log.with_name(incident.log.name.replace('.23.', '.21.'))
                    older.write_text(json.dumps(incident.lines[0]) + '\n')
                    incident.lines = incident.lines[1:]
                elif fault == 'stale':
                    incident.lines = incident.lines[:-1]
                    incident.at += dt.timedelta(minutes=2)
                incident.write_log()
                if fault == 'bad_line':
                    with incident.log.open('a') as stream:
                        stream.write('invalid\n')
                with self.assertRaises(RuntimeError):
                    remote.incident_log_window(remote.timestamp(incident.failure_at) - dt.timedelta(seconds=60), incident.at)


if __name__ == '__main__':
    unittest.main()
