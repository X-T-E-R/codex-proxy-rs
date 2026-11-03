"""新批次 helper 必须按父签源/镜像及 host 能力准入，旧载体合同不变"""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from test_controller import MockSite, manifest, remote


class BatchHelperAdmissionTests(unittest.TestCase):
    def test_batch_binding_is_explicit_and_drift_refuses_before_stop(self):
        helper = {'source': 'd' * 40, 'image': 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:' + 'e' * 64,
                  'contract': 'cpr-recovery-confirmed/1'}
        for fault in (None, 'missing_acceptance', 'source', 'image', 'contract', 'argv_image', 'misplaced_image', 'host', 'unknown'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as tmp, MockSite(tmp) as site:
                accepted = manifest()
                if fault != 'missing_acceptance':
                    accepted['recoveryHelper'] = dict(helper)
                (site.run / 'accepted.json').write_text(json.dumps(accepted))
                site.binding.update(helperSource=helper['source'], helperImage=helper['image'], helperContract=helper['contract'])
                site.binding['recoveryCommand'][1] = str(site.gate / 'site-recovery-confirmed-v1.py')
                site.binding['recoveryCommand'] += ['--helper-source', helper['source'], '--helper-image', helper['image'],
                    '--redis-helper-command-json', json.dumps(['docker', 'run', helper['image'], '/app/bin/codex-proxy-rs', 'site-recovery-redis'])]
                if fault in ('source', 'image', 'contract'):
                    site.binding[{'source': 'helperSource', 'image': 'helperImage', 'contract': 'helperContract'}[fault]] = 'drift'
                if fault == 'argv_image':
                    site.binding['recoveryCommand'][-1] = json.dumps(['docker', 'run', remote.HELPER_IMAGE, '/app/bin/codex-proxy-rs', 'site-recovery-redis'])
                elif fault == 'misplaced_image':
                    site.binding['recoveryCommand'][-1] = json.dumps(['docker', 'run', '--name', helper['image'], remote.HELPER_IMAGE, '/app/bin/codex-proxy-rs', 'site-recovery-redis'])
                (site.gate / 'binding.json').write_text(json.dumps(site.binding))
                def command(argv, **kwargs):
                    if argv[-1] == '--describe':
                        return json.dumps({'contract': 'old' if fault == 'host' else helper['contract']}).encode()
                    if fault == 'unknown' and '--apply' in argv:
                        raise RuntimeError('command_result_unknown')
                    if argv[:2] == ['python3', str(site.gate / 'site-recovery-confirmed-v1.py')]:
                        argv = [argv[0], str(site.gate / 'site-recovery.py'), *argv[2:]]
                    return site.command(argv, **kwargs)
                with patch.object(remote, 'command', side_effect=command):
                    result = site.deploy()
                if fault is None:
                    self.assertEqual(result['phase'], 'deployed', result)
                elif fault == 'unknown':
                    self.assertEqual(result['phase'], 'recovery_requires_operator')
                    self.assertTrue((site.gate / 'maintenance').exists())
                    self.assertFalse(any(argv[:2] == ['docker', 'compose'] and 'up' in argv for argv in site.events))
                    self.assertEqual(list(result['recoveryCommands'].values())[0]['outcome'], 'unknown')
                else:
                    self.assertEqual(result['phase'], 'preflight_blocked', result)
                    self.assertFalse(any(argv[:2] == ['docker', 'stop'] for argv in site.events))

    def test_frozen_helper_cannot_be_misdeclared_as_batch(self):
        for helper in ({'source': remote.HELPER_SOURCE, 'image': remote.HELPER_IMAGE, 'contract': 'cpr-recovery-confirmed/1'},
                       {'source': 'd' * 40, 'image': 'mutable:tag', 'contract': 'cpr-recovery-confirmed/1'}):
            with self.assertRaisesRegex(RuntimeError, 'exact_batch_helper'):
                remote.manifest(dict(manifest(), recoveryHelper=helper))
