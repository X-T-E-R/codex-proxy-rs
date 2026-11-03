#!/usr/bin/env python3
"""新恢复 binding 的单次入口；调用前持久 claim/维护闸，未知结果只能观察原 receipt"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys

CONTRACT = 'cpr-recovery-entry/1'
CAPABILITY = {'contract': CONTRACT, 'hostContract': 'cpr-recovery-confirmed/1', 'singleAttempt': True}
HOST = 'site-recovery-confirmed-v1.py'
TIMEOUT = 1800


def require(value, message):
    if not value:
        raise RuntimeError(message)


def secure(path):
    info = path.lstat()
    require(not path.is_symlink() and info.st_uid == os.geteuid() and not info.st_mode & 0o022,
            'entry_path_owner_or_permissions_drift')
    return info


def load_host(path):
    require(stat.S_ISREG(secure(path).st_mode), 'entry_host_not_regular')
    spec = importlib.util.spec_from_file_location('single_attempt_recovery_host', path)
    module = importlib.util.module_from_spec(spec)
    exec(compile(path.read_bytes(), str(path), 'exec'), module.__dict__)
    return module


def owner(host, container):
    value = json.loads(host.run(['docker', 'inspect', '--format', host.PROJECTION, container]))
    require(re.fullmatch('[0-9a-f]{64}', container) and value['Id'] == container and
            not value['Running'] and not value['Restarting'] and value['Pid'] == 0 and
            value['Status'] in ('exited', 'dead') and value['RestartPolicy'] == 'no' and
            not value['FinishedAt'].startswith('0001-') and value['Project'] and
            value['Service'] == 'codex-proxy-rs', 'entry_exact_exited_generation_required')
    return {key: value[key] for key in ('Id', 'StartedAt', 'FinishedAt', 'Project')}


def claim_path(receipt):
    return receipt.with_name(receipt.name + '.entry-v1.json')


def maintenance(gate):
    path = gate / 'maintenance'
    if not path.exists() and not path.is_symlink():
        return None
    secure(path)
    return path.read_text()


def hold(gate, marker, host):
    if maintenance(gate) is None:
        descriptor = os.open(gate / 'maintenance', os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, 'w') as file:
            file.write(marker)
            file.flush()
            os.fsync(file.fileno())
        host.sync_directory(gate)
    require(maintenance(gate) == marker, 'entry_maintenance_owner_drift')


def completed(receipt, identity, host):
    secure(receipt)
    value = json.loads(receipt.read_text())
    rows = host.validate_rows(value.get('requests'))
    counts = value.get('released', {})
    require(value.get('owner') == identity and value.get('phase') == 'complete' and
            value.get('batch') == {'contract': host.CONTRACT, 'outcome': 'completed'} and
            isinstance(value.get('completed'), list) and
            len(value['completed']) == len(rows) and set(value['completed']) == {row['id'] for row in rows} and
            set(counts) == {'accountSlots', 'clientSlots'} and
            all(type(item) is int and item >= 0 for item in counts.values()), 'entry_complete_receipt_unproven')
    return {'phase': 'complete', 'requests': len(rows), **counts}


def run_host(argv):
    process = subprocess.run(argv, capture_output=True, text=True, timeout=TIMEOUT)
    require(process.returncode == 0 and len(process.stdout) < 1048576, 'entry_host_failed_or_unknown')
    return json.loads(process.stdout)


def execute(args, gate=None):
    gate = gate or Path(__file__).resolve().parent
    receipt = Path(args.receipt)
    require(Path(args.recovery_host) == gate / HOST and receipt.is_absolute() and
            receipt.resolve() == receipt, 'entry_exact_paths_required')
    secure(gate)
    secure(receipt.parent)
    watchdog_run = receipt.parent.parent == gate / 'runs' and receipt.name == 'recovery.json'
    deployment_run = (receipt.parent.parent == gate.parent / 'deployments' and
                      re.fullmatch('task40-[0-9a-f]{32}', receipt.parent.name) and
                      receipt.name == 'stopped-recovery-' + args.old_container[:12] + '.json')
    require(watchdog_run or deployment_run, 'entry_receipt_scope_required')
    host = load_host(Path(args.recovery_host))
    identity = owner(host, args.old_container)
    marker = 'entry-v1:' + str(receipt.relative_to(gate.parent))
    prior = maintenance(gate)
    # Task40 已持 run 专属闸；成功时不夺走或解除调用者的维护 owner
    external = receipt.parent.name if deployment_run else None
    require(prior in (None, marker, external) and (prior is not None or not deployment_run),
            'entry_foreign_maintenance')
    path = claim_path(receipt)
    held = external if prior == external and external is not None else marker
    if path.exists() or path.is_symlink():
        try:
            secure(path)
            claim = json.loads(path.read_text())
            require(claim.get('contract') == CONTRACT and claim.get('owner') == identity and
                    claim.get('receipt') == str(receipt), 'entry_claim_owner_drift')
            if claim.get('phase') == 'complete' and prior in (None, external):
                # caller 可能在 complete 后、start 前退出；只读原结果，不重发恢复 host
                return completed(receipt, identity, host)
        except Exception:
            hold(gate, held, host)
            raise
        hold(gate, held, host)
        raise RuntimeError('entry_already_claimed_observe_only')
    if receipt.exists() or receipt.is_symlink() or receipt.with_suffix(receipt.suffix + '.tmp').exists():
        hold(gate, held, host)
        raise RuntimeError('entry_existing_receipt_observe_only')
    claim = {'contract': CONTRACT, 'phase': 'intent', 'owner': identity, 'receipt': str(receipt), 'pid': os.getpid()}
    hold(gate, held, host)
    host.save(path, claim, exclusive=True)
    argv = ['python3', args.recovery_host, '--old-container', args.old_container,
            '--receipt', str(receipt), '--pg-command-json', args.pg_command_json,
            '--redis-helper-command-json', args.redis_helper_command_json,
            '--helper-source', args.helper_source, '--helper-image', args.helper_image,
            '--ack-single-deployment', '--apply']
    try:
        result = run_host(argv)
        require(owner(host, args.old_container) == identity, 'entry_generation_drift')
        require(result == completed(receipt, identity, host), 'entry_host_receipt_result_drift')
        require(maintenance(gate) == held, 'entry_maintenance_owner_drift')
        claim['phase'] = 'complete'
        host.replace_receipt(path, claim)
        if held == marker:
            (gate / 'maintenance').unlink()
            host.sync_directory(gate)
        return result
    except BaseException:
        # 预先持闸覆盖 SIGKILL/父传输超时；即使写 unknown 失败也留 durable intent
        hold(gate, held, host)
        raise


def observe(receipt):
    path = claim_path(receipt)
    secure(path)
    claim = json.loads(path.read_text())
    value = None
    if receipt.exists():
        secure(receipt)
        value = json.loads(receipt.read_text())
    return {'contract': CONTRACT, 'phase': 'observed', 'claimPhase': claim.get('phase'),
            'owner': claim.get('owner'), 'receiptPhase': value.get('phase') if value else None,
            'batchOutcome': value.get('batch', {}).get('outcome') if value else None}


def main():
    os.umask(0o077)
    if sys.argv[1:] == ['--describe']:
        print(json.dumps(CAPABILITY))
        return
    if sys.argv[1:2] == ['observe']:
        parser = argparse.ArgumentParser()
        parser.add_argument('action')
        parser.add_argument('--receipt', type=Path, required=True)
        args = parser.parse_args()
        print(json.dumps(observe(args.receipt)))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('recovery-host', 'old-container', 'receipt', 'pg-command-json',
                 'redis-helper-command-json', 'helper-source', 'helper-image'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--ack-single-deployment', action='store_true', required=True)
    parser.add_argument('--apply', action='store_true', required=True)
    print(json.dumps(execute(parser.parse_args())))


if __name__ == '__main__':
    try:
        main()
    except Exception:
        print('single-attempt recovery held; observe the original generation and receipt', file=sys.stderr)
        sys.exit(1)
