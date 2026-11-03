#!/usr/bin/env python3
"""在既有 watchdog 闸内版本化安装恢复器；只换 host/binding，不执行恢复或启停应用"""
import argparse
from contextlib import contextmanager, ExitStack
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import sys

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('recovery_install_remote', HERE / 'task40-prod-upgrade/remote.py')
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)
require = remote.require
BASE = remote.BASE
CONTRACT = 'cpr-recovery-helper-install/1'
HELPER = {'source': 'bac2c8e0f4d7eae551d8eb194f79d285eede140b',
          'image': 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:6809441bf0cef500485abd2767f63ddab388ee2f65cae5dd721e3883422742d7',
          'contract': 'cpr-recovery-confirmed/1'}
HOST = 'site-recovery-confirmed-v1.py'
OWNER_FIELDS = ('id', 'project', 'service', 'revision')
GENERATION_FIELDS = ('id', 'imageId', 'pid', 'revision', 'startedAt', 'project', 'service',
                     'composeFiles', 'workingDir', 'configuration')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return (json.dumps(value, indent=2) + '\n').encode()


def secure(path, directory=False):
    info = path.lstat()
    require((stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)) and
            info.st_uid == os.geteuid() and not info.st_mode & 0o022,
            'unsafe_local_owner_or_permissions')
    return info


def read(path):
    secure(path)
    return path.read_bytes()


def sync(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def create(path, data):
    # 独占文件和同目录 fsync 是单次意图与原始备份的持久边界
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'wb') as file:
        file.write(data)
        file.flush()
        os.fsync(file.fileno())
    sync(path.parent)


def replace(path, data):
    temporary = path.with_name(path.name + '.helper-install.tmp')
    create(temporary, data)
    os.replace(temporary, path)
    sync(path.parent)


def manifest(value, accepted=False):
    require(isinstance(value, dict) and value.get('contract') == CONTRACT and
            re.fullmatch(r'helper-install-[0-9a-f]{32}', value.get('operationId', '')) and
            value.get('helper') == HELPER and
            re.fullmatch('[0-9a-f]{64}', value.get('hostSha256', '')), 'exact_install_input_required')
    remote.baseline_identity(value.get('baseline'))
    if accepted:
        require(value.get('accepted') is True and
                set(value.get('previous', {})) == {'bindingSha256', 'hostSha256', 'watchdogSha256'} and
                all(re.fullmatch('[0-9a-f]{64}', item) for item in value['previous'].values()),
                'parent_accepted_fresh_proposal_required')
    return value


class Installation:
    def __init__(self, value, host=HERE / 'site-recovery.py'):
        self.m = manifest(value)
        self.gate = BASE / '.runtime/restart-recovery'
        self.root = self.gate / 'helper-installs' / value['operationId']
        self.host = host
        self.marker = value['operationId'].encode()

    @contextmanager
    def locked(self):
        require(remote.fcntl is not None and os.name == 'posix', 'linux_flock_required')
        for path in (BASE, BASE / '.runtime', BASE / '.runtime/deployments', self.gate):
            secure(path, directory=True)
        with ExitStack() as stack:
            # 同时排斥旧 Task36 和现 Task40；非阻塞取锁不会与既有部署反向等待
            for path in (BASE / '.runtime/deployments/task36-active.lock',
                         BASE / '.runtime/deployments/task40-active.lock', self.gate / 'lifecycle.lock'):
                secure(path)
                lock = stack.enter_context(path.open('r'))
                remote.fcntl.flock(lock, remote.fcntl.LOCK_EX | remote.fcntl.LOCK_NB)
            yield

    def maintenance(self):
        path = self.gate / 'maintenance'
        return read(path) if path.exists() or path.is_symlink() else None

    def runtime(self):
        current = remote.container()
        remote.bound_runtime(current, self.m['baseline'])
        require(current['health'] == 'healthy' and remote.health(), 'fresh_health_required')
        require(current['workingDir'] == str(BASE) and current['project'] == current['service'] == remote.SERVICE,
                'compose_owner_drift')
        files = current['composeFiles'].split(',')
        require(files and all(Path(item).is_absolute() and Path(item).parent == BASE for item in files),
                'compose_file_owner_drift')
        current['configuration'] = {}
        for path in [Path(item) for item in files] + [BASE / '.env', BASE / 'deploy/config.yaml']:
            info = secure(path)
            current['configuration'][str(path)] = [info.st_ino, info.st_size, info.st_mtime_ns, info.st_mode]
        require(remote.pin(BASE / '.env')[2] == self.m['baseline']['image'], 'live_pin_drift')
        argv = ['docker', 'compose', '--project-directory', str(BASE), '--env-file', str(BASE / '.env')]
        for item in files:
            argv += ['-f', item]
        effective = json.loads(remote.command(argv + ['config', '--format', 'json']))
        service = effective['services'][remote.SERVICE]
        require(service['image'] == self.m['baseline']['image'] and service['restart'] == 'no',
                'effective_compose_drift')
        policy = json.loads(remote.command(['docker', 'inspect', '--format',
                                          '{{json .HostConfig.RestartPolicy.Name}}', remote.SERVICE]))
        require(policy == 'no', 'restart_policy_not_no')
        image = remote.image(self.m['baseline']['image'], self.m['baseline']['source'])
        require(current['imageId'] == image['id'], 'live_image_drift')
        for query, expected in (('is-enabled', 'enabled'), ('is-active', 'active')):
            require(remote.command(['systemctl', query, 'cpr-restart-recovery.timer']).decode().strip() == expected,
                    'watchdog_timer_not_ready')
        return current

    def previous(self, binding, current):
        remote.recovery_binding(binding)
        require(binding['runtime'] == {key: current[key] for key in OWNER_FIELDS}, 'binding_runtime_drift')
        argv = binding['recoveryCommand']
        require(len(argv) == 6 and argv[2] == '--pg-command-json' and
                argv[4] == '--redis-helper-command-json', 'previous_command_shape_drift')
        redis = json.loads(argv[5])
        require(remote.helper_argv(redis, remote.HELPER_IMAGE) and '--rm' in redis and '-i' in redis and
                '--name' not in redis and '--cidfile' not in redis, 'previous_helper_argv_drift')
        return redis

    def integrity(self):
        return {'bindingSha256': digest(read(self.gate / 'binding.json')),
                'hostSha256': digest(read(self.gate / 'site-recovery.py')),
                'watchdogSha256': digest(read(self.gate / 'watchdog.py'))}

    def candidate(self):
        payload = self.root / 'host.confirmed.py'
        if not payload.exists():
            payload = self.host
        source = read(payload)
        require(digest(source) == self.m['hostSha256'], 'candidate_host_integrity_drift')
        spec = importlib.util.spec_from_file_location('confirmed_install_host', payload)
        module = importlib.util.module_from_spec(spec)
        # 执行已核字节而非可能遗留的同 mtime/size .pyc
        exec(compile(source, str(payload), 'exec'), module.__dict__)
        capability = json.loads(remote.command(['python3', str(payload), '--describe'], timeout=15))
        require(capability == module.CAPABILITY and capability['contract'] == HELPER['contract'],
                'host_capability_drift')
        return module

    def build_binding(self, before, module):
        argv = before['recoveryCommand']
        module.command(argv[3])
        redis = module.command(argv[5], helper=True)
        require(remote.helper_argv(redis, remote.HELPER_IMAGE), 'previous_helper_argv_drift')
        redis = [HELPER['image'] if item == remote.HELPER_IMAGE else item for item in redis]
        if '--pull=never' not in redis:
            redis.insert(2, '--pull=never')
        after = dict(before, helperSource=HELPER['source'], helperImage=HELPER['image'],
                     helperContract=HELPER['contract'])
        after['recoveryCommand'] = ['python3', str(self.gate / HOST), '--pg-command-json', argv[3],
                                    '--redis-helper-command-json', json.dumps(redis),
                                    '--helper-source', HELPER['source'], '--helper-image', HELPER['image']]
        return after, redis

    def probe_watchdog(self):
        result = json.loads(remote.command(['python3', str(self.gate / 'watchdog.py'), '--base', str(self.gate)], timeout=15))
        require(result == {'phase': 'lifecycle_busy'}, 'watchdog_lock_contract_drift')

    def unchanged(self, expected, previous_hashes):
        current = self.runtime()
        require(all(current[key] == expected[key] for key in GENERATION_FIELDS), 'container_generation_drift')
        require(digest(read(self.gate / 'site-recovery.py')) == previous_hashes['hostSha256'] and
                digest(read(self.gate / 'watchdog.py')) == previous_hashes['watchdogSha256'],
                'previous_host_or_watchdog_drift')
        return current

    def inspect(self):
        with self.locked():
            require(self.maintenance() is None, 'foreign_or_unknown_maintenance')
            require(not self.root.exists(), 'operation_already_claimed_observe_only')
            current = self.runtime()
            before = json.loads(read(self.gate / 'binding.json'))
            self.previous(before, current)
            self.candidate()
            previous = self.integrity()
            proposal = dict(self.m, accepted=False, previous=previous)
            return {'phase': 'proposal', 'capturedAt': remote.now(), 'runtime': current, 'proposal': proposal}

    def result(self, phase, **extra):
        value = {'phase': phase, 'operationId': self.m['operationId'], 'at': remote.now(), **extra}
        replace(self.root / 'result.json', encoded(value))
        return value

    def install(self):
        manifest(self.m, accepted=True)
        with self.locked():
            require(self.maintenance() is None, 'foreign_or_unknown_maintenance')
            require(not self.root.exists(), 'operation_already_claimed_observe_only')
            require(not (self.gate / HOST).exists() and not (self.gate / HOST).is_symlink(),
                    'versioned_host_exists_observe_only')
            current = self.runtime()
            hashes = self.integrity()
            require(hashes == self.m['previous'], 'fresh_previous_integrity_drift')
            original = read(self.gate / 'binding.json')
            before = json.loads(original)
            self.previous(before, current)
            module = self.candidate()
            after, redis = self.build_binding(before, module)
            remote.image(HELPER['image'], HELPER['source'])
            remote.image(remote.HELPER_IMAGE, remote.HELPER_SOURCE)
            parent = self.root.parent
            if not parent.exists():
                parent.mkdir(mode=0o700)
                sync(self.gate)
            secure(parent, directory=True)
            self.root.mkdir(mode=0o700)
            sync(parent)
            create(self.root / 'accepted.json', encoded(self.m))
            create(self.root / 'binding.before.json', original)
            create(self.root / 'binding.confirmed.json', encoded(after))
            create(self.root / 'host.confirmed.py', read(self.host))
            create(self.root / 'intent.json', encoded({'at': remote.now(), 'singleAttempt': True,
                                                      'runtime': current, 'pid': os.getpid()}))
            try:
                create(self.gate / 'maintenance', self.marker)
                self.probe_watchdog()
                # 此次只认证/PING；受跟踪身份约束的自有 helper 即使 CLI 超时也只能观察，不再发一次
                probe = redis[:2] + ['--name', self.m['operationId'] + '-auth', '--cidfile', str(self.root / 'helper.cid')] + redis[2:]
                create(self.root / 'authenticate-intent.json', encoded({'at': remote.now(), 'name': self.m['operationId'] + '-auth'}))
                capability = json.loads(remote.command(probe, timeout=15, data=b'{"kind":"authenticate"}'))
                require(capability == module.CAPABILITY, 'helper_capability_drift')
                create(self.root / 'authenticated.json', encoded(capability))
                self.unchanged(current, hashes)
                require(self.integrity() == hashes and self.maintenance() == self.marker,
                        'gate_or_binding_drift')
                payload = read(self.root / 'host.confirmed.py')
                require(digest(payload) == self.m['hostSha256'], 'candidate_host_integrity_drift')
                replace(self.gate / HOST, payload)
                require(stat.S_IMODE(secure(self.gate / HOST).st_mode) == 0o600 and
                        digest(read(self.gate / HOST)) == self.m['hostSha256'], 'installed_host_integrity_drift')
                self.unchanged(current, hashes)
                require(read(self.gate / 'binding.json') == original and self.maintenance() == self.marker,
                        'gate_or_binding_drift')
                create(self.root / 'binding-intent.json', encoded({'at': remote.now()}))
                replace(self.gate / 'binding.json', encoded(after))
                self.verify_target('confirmed', current)
                self.result('binding_verified_pending_release')
                self.release()
                return self.result('installed', runtime=current, helper=HELPER,
                                   businessRestarted=False, recoveryExecuted=False)
            except Exception:
                # 不自动回退；写结果或传输未知时保原意图，下一次只读观察或显式受检 settle
                self.hold_if_absent()
                raise

    def evidence(self):
        secure(self.root.parent, directory=True)
        secure(self.root, directory=True)
        stored = json.loads(read(self.root / 'accepted.json'))
        require(stored == self.m, 'accepted_input_drift')
        before = read(self.root / 'binding.before.json')
        require(digest(before) == self.m['previous']['bindingSha256'], 'previous_backup_integrity_drift')
        module = self.candidate()
        after, _ = self.build_binding(json.loads(before), module)
        require(read(self.root / 'binding.confirmed.json') == encoded(after), 'confirmed_backup_integrity_drift')
        return before, encoded(after), module

    def state(self, before, after):
        current = read(self.gate / 'binding.json')
        return 'previous' if current == before else 'confirmed' if current == after else 'foreign'

    def verify_target(self, target, expected):
        before, after, _ = self.evidence()
        self.unchanged(expected, self.m['previous'])
        require(self.state(before, after) == target and self.maintenance() == self.marker,
                'target_binding_or_maintenance_drift')
        self.admit_target(target, expected)
        self.probe_watchdog()
        self.unchanged(expected, self.m['previous'])
        require(self.state(before, after) == target and self.maintenance() == self.marker,
                'target_binding_or_maintenance_drift')

    def admit_target(self, target, expected):
        require(target in ('confirmed', 'previous'), 'explicit_binding_choice_required')
        before, after, module = self.evidence()
        binding = json.loads(after if target == 'confirmed' else before)
        if target == 'confirmed':
            require(digest(read(self.gate / HOST)) == self.m['hostSha256'] and
                    stat.S_IMODE(secure(self.gate / HOST).st_mode) == 0o600,
                    'installed_host_integrity_drift')
            require(json.loads(read(self.root / 'authenticated.json')) == module.CAPABILITY,
                    'authenticated_capability_unproven')
            remote.recovery_binding(binding, HELPER)
        else:
            self.previous(binding, expected)
            remote.image(remote.HELPER_IMAGE, remote.HELPER_SOURCE)
        if (self.root / 'authenticate-intent.json').exists():
            require(self.helper_id() is not None, 'authentication_helper_identity_unknown_observe_only')
        require(not self.helper_containers(), 'authentication_helper_still_present_observe_only')

    def helper_id(self):
        path = self.root / 'helper.cid'
        value = read(path).decode().strip() if path.exists() else ''
        return value if re.fullmatch('[0-9a-f]{64}', value) else None

    def helper_containers(self):
        filters = ['name=^/' + self.m['operationId'] + '-auth$']
        cid = self.helper_id()
        if cid is not None:
            filters.append('id=' + cid)
        ids = set()
        for selector in filters:
            value = remote.command(['docker', 'ps', '-aq', '--no-trunc', '--filter', selector]).decode().strip()
            ids.update(value.splitlines() if value else [])
        require(all(re.fullmatch('[0-9a-f]{64}', item) for item in ids), 'helper_identity_unproven')
        return sorted(ids)

    def release(self):
        require(self.maintenance() == self.marker, 'maintenance_owner_drift')
        (self.gate / 'maintenance').unlink()
        sync(self.gate)

    def hold_if_absent(self):
        if self.maintenance() is None:
            create(self.gate / 'maintenance', self.marker)

    def temporary_files(self):
        return [str(path.relative_to(self.gate)) for directory in (self.gate, self.root)
                for path in directory.glob('*.helper-install.tmp')]

    def observe(self):
        manifest(self.m, accepted=True)
        with self.locked():
            secure(self.root, directory=True)
            evidence_ok, binding_state = False, 'unproven'
            try:
                before, after, _ = self.evidence()
                binding_state = self.state(before, after)
                evidence_ok = True
            except Exception:
                pass
            try:
                expected = json.loads(read(self.root / 'intent.json'))['runtime']
                baseline = self.unchanged(expected, self.m['previous'])
                baseline_ok = True
            except Exception:
                baseline, baseline_ok = None, False
            maintenance = self.maintenance()
            result = json.loads(read(self.root / 'result.json')) if (self.root / 'result.json').exists() else None
            host_matches = False
            try:
                host_matches = digest(read(self.gate / HOST)) == self.m['hostSha256'] and stat.S_IMODE(
                    secure(self.gate / HOST).st_mode) == 0o600
            except (OSError, RuntimeError):
                pass
            # 只输出标识与状态，不导出恢复 argv、配置或业务 receipt
            return {'phase': 'observed', 'operationId': self.m['operationId'],
                    'bindingState': binding_state, 'evidenceMatches': evidence_ok, 'baselineMatches': baseline_ok,
                    'runtime': baseline, 'maintenance': 'absent' if maintenance is None else
                    'owned' if maintenance == self.marker else 'foreign',
                    'versionedHostMatches': host_matches,
                    'temporaryFiles': self.temporary_files(),
                    'helperContainerId': self.helper_id(), 'helperContainersPresent': self.helper_containers(),
                    'lastResult': result}

    def settle(self, target):
        manifest(self.m, accepted=True)
        with self.locked():
            before, after, _ = self.evidence()
            expected = json.loads(read(self.root / 'intent.json'))['runtime']
            current = self.unchanged(expected, self.m['previous'])
            state = self.state(before, after)
            require(state in ('previous', 'confirmed'), 'foreign_binding_observe_only')
            require(self.maintenance() in (None, self.marker), 'foreign_or_unknown_maintenance')
            require(not self.temporary_files(), 'unsettled_temporary_observe_only')
            # 先证明目标可用，再声明写入；已是目标时只复核放闸，不重发原子替换
            self.hold_if_absent()
            try:
                self.admit_target(target, current)
                if state != target:
                    claim = self.root / ('settle-' + target + '-intent.json')
                    require(not claim.exists(), 'settle_already_claimed_observe_only')
                    create(claim, encoded({'at': remote.now()}))
                    require(self.state(before, after) == state and self.maintenance() == self.marker,
                            'gate_or_binding_drift')
                    self.unchanged(current, self.m['previous'])
                    replace(self.gate / 'binding.json', after if target == 'confirmed' else before)
                self.verify_target(target, current)
                self.result('settled_pending_release', binding=target)
                self.release()
                return self.result('settled', binding=target, businessRestarted=False, recoveryExecuted=False)
            except Exception:
                self.hold_if_absent()
                raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('inspect', 'install', 'observe', 'settle'))
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--host', type=Path, default=HERE / 'site-recovery.py')
    parser.add_argument('--binding', choices=('previous', 'confirmed'))
    parser.add_argument('--GO', action='store_true')
    args = parser.parse_args()
    os.umask(0o077)
    require(os.name == 'posix' and os.geteuid() == 0, 'linux_root_required')
    require(args.action not in ('install', 'settle') or args.GO, 'explicit_site_GO_required')
    require(args.action != 'settle' or args.binding is not None, 'explicit_binding_choice_required')
    operation = Installation(json.loads(read(args.manifest)), args.host)
    if args.action == 'settle':
        result = operation.settle(args.binding)
    else:
        result = getattr(operation, args.action)()
    print(json.dumps(result))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        code = str(error) if type(error) is RuntimeError else type(error).__name__
        print(json.dumps({'phase': 'refused_observe_before_next_action', 'error': code}))
        sys.exit(1)
