#!/usr/bin/env python3
"""Task40 Windows 离线控制面：独立子进程持锁，SSH 不确定结果只观察。"""
import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid
import remote

HERE = Path(__file__).resolve().parent
SSH = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15', '-o', 'ServerAliveInterval=10',
       '-o', 'ServerAliveCountMax=3', 'lag', 'python3 -']


def read(path):
    return json.loads(Path(path).read_text(encoding='utf-8-sig'))


def save(path, value):
    remote.atomic(Path(path), value)


def ssh(code, payload=None, timeout=90):
    # SSH 仅发送固定脚本与非秘密 manifest，不打印错误原文
    body = ('payload=' + repr(payload) + '\n' if payload is not None else '') + code
    try:
        p = subprocess.run(SSH, input=body.encode(), capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError('ssh_result_unknown_observe_only') from None
    remote.require(p.returncode == 0 and len(p.stdout) < 1048576, 'ssh_result_unknown_observe_only')
    return json.loads(p.stdout)


def baseline(mode='healthy'):
    # 部署前独立只读观测；无需 manifest 或 GO，不创建远端目录
    code = """import json
namespace={'__name__':'task40_observer'}
exec(compile(payload['code'],'remote-observer.py','exec'),namespace)
print(json.dumps(namespace['baseline'](payload['mode'])))
"""
    return ssh(code, {'code': (HERE / 'remote.py').read_text(encoding='utf-8'), 'mode': mode}, timeout=90)


def provision(m, root, artifacts=None):
    import base64
    files = {}
    if m['schema'] == 53:
        import schema53
        remote.require(artifacts is not None, 'schema53_artifact_directory_required')
        files['schema53.py'] = base64.b64encode((HERE / 'schema53.py').read_bytes()).decode()
        for role in schema53.ROLES:
            item = m['schema53Contract']['artifacts'][role]
            files['schema53-artifacts/' + item['name']] = base64.b64encode(
                schema53.artifact_bytes(artifacts, m['schema53Contract'], role)).decode()
    code = """import ast,base64,json,os
from pathlib import Path
os.umask(0o077)
r=Path(payload['root'])
assert r.parent==Path('/opt/codex-proxy-rs/.runtime/deployments') and not r.exists()
ast.parse(payload['code'])
r.mkdir(mode=0o700)
(r/'remote.py').write_text(payload['code'])
(r/'accepted.json').write_text(json.dumps(payload['manifest']))
for name,content in payload['files'].items():
 p=r/name
 assert p.resolve().is_relative_to(r)
 p.parent.mkdir(exist_ok=True)
 p.write_bytes(base64.b64decode(content))
print(json.dumps({'phase':'provisioned'}))
"""
    return ssh(code, {'root': root, 'code': (HERE / 'remote.py').read_text(encoding='utf-8'),
                      'manifest': m, 'files': files})


def receipt(root):
    code = """import json
from pathlib import Path
r=Path(payload)
p=next((p for p in (r/'takeover-retry-result.json',r/'takeover-result.json',r/'result.json') if p.exists()),None)
if (r/'takeover-retry-launch-claimed.json').exists() and not (r/'takeover-retry-result.json').exists():
 print(json.dumps({'phase':'takeover_pending','attemptClaimed':(r/'takeover-retry-attempt-claimed.json').exists()}))
elif (r/'takeover-attempt-claimed.json').exists() and not (r/'takeover-result.json').exists():
 print(json.dumps({'phase':'takeover_pending','attemptClaimed':True}))
else:
 print(p.read_text() if p else json.dumps({'phase':'pending','attemptClaimed':(r/'attempt-claimed.json').exists(),'launchClaimed':(r/'launch-claimed.json').exists()}))
"""
    return ssh(code, root)


def abandon_unstarted(root):
    code = """import fcntl,json
from pathlib import Path
r=Path(payload)
with (r/'launch.guard').open('a') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 if (r/'attempt-claimed.json').exists() or (r/'result.json').exists():
  print(json.dumps({'phase':'pending','attemptClaimed':True}))
 else:
  (r/'abort-before-run').touch()
  result={'phase':'blocked_unchanged','success':False,'rollback':False,'errorCode':'unstarted_fenced'}
  (r/'result.json').write_text(json.dumps(result))
  print(json.dumps(result))
"""
    return ssh(code, root)


def preparation_receipt(root, cleanup=False):
    code = """import json
from pathlib import Path
r=Path(payload['root'])
role='cleanup' if payload['cleanup'] else 'prepare'
p=r/(role+'-result.json')
print(p.read_text() if p.exists() else json.dumps({'phase':'pending'}))
"""
    return ssh(code, {'root': root, 'cleanup': cleanup})


def observe(root, seconds=4500, clock=time.monotonic, sleep=time.sleep, preparation=False, cleanup=False):
    until = clock() + seconds
    unstarted = 0
    while clock() < until:
        try:
            value = preparation_receipt(root, cleanup) if preparation else receipt(root)
            if value['phase'] != 'pending':
                return value
            unstarted = unstarted + 1 if not value.get('attemptClaimed') else 0
            if not preparation and unstarted >= 3:
                value = abandon_unstarted(root)
                if value['phase'] != 'pending':
                    return value
        except RuntimeError:
            pass
        sleep(10)
    raise RuntimeError('remote_receipt_unknown_no_relaunch')


def pull_exact(root):
    code = """import json,subprocess,sys
from pathlib import Path
r=Path(payload)
sys.path.insert(0,str(r))
namespace={'__name__':'task40_image_admission'}
exec(compile((r/'remote.py').read_text(),'remote.py','exec'),namespace)
m=namespace['manifest'](json.loads((r/'accepted.json').read_text()))
local=subprocess.run(['docker','image','inspect','--format','{{json .Id}}',m['image']],capture_output=True,timeout=20)
if local.returncode==0:
 namespace['image'](m['image'],m['source'])
 value={'phase':'pulled','origin':'verified_local_digest'}
elif local.returncode==1 and (b'No such image:' in local.stderr or b'No such object:' in local.stderr):
 p=subprocess.run(['docker','pull',m['image']],capture_output=True,timeout=420)
 value={'phase':'pulled' if p.returncode==0 else 'pull_blocked','origin':'registry'}
else:
 value={'phase':'pull_blocked','origin':'inspect_unknown'}
print(json.dumps(value))
"""
    value = ssh(code, root, timeout=450)
    remote.require(value['phase'] == 'pulled', 'exact_pull_blocked')


def verify_remote(root, sleep=time.sleep):
    code = """import json,subprocess
r=payload
p=subprocess.run(['python3',r+'/remote.py','verify','--root',r],capture_output=True,timeout=35)
print(p.stdout.decode() if p.returncode==0 else json.dumps({'phase':'verification_blocked'}))
"""
    for attempt in range(6):
        try:
            value = ssh(code, root, timeout=50)
            if value['phase'] == 'verified':
                return value
        except RuntimeError:
            pass
        if attempt < 5:
            sleep(3)
    raise RuntimeError('fresh_receipt_unverified_inspect_run')


def retry_admission(root):
    code = """import json,subprocess
r=payload
p=subprocess.run(['python3',r+'/remote.py','retry-admission','--root',r],capture_output=True,timeout=20)
print(p.stdout.decode() if p.returncode==0 else json.dumps({'phase':'retry_blocked'}))
"""
    return ssh(code, root, timeout=35)


def start_remote_takeover(root, retry=False):
    code = """import fcntl,json,os,subprocess
from pathlib import Path
r=Path(payload['root'])
retry=payload['retry']
active=Path('/opt/codex-proxy-rs/.runtime/deployments/task40-active.lock')
with active.open('r') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
 fcntl.flock(lock,fcntl.LOCK_UN)
assert (r/'attempt-claimed.json').exists()
if retry:
 check=subprocess.run(['python3',str(r/'remote.py'),'retry-admission','--root',str(r)],capture_output=True,timeout=15)
 assert check.returncode==0 and json.loads(check.stdout).get('phase')=='retry_admissible'
 assert not (r/'takeover-retry-result.json').exists()
else:
 assert not (r/'takeover-result.json').exists() and not (r/'takeover-launch-claimed.json').exists()
claim=r/('takeover-retry-launch-claimed.json' if retry else 'takeover-launch-claimed.json')
with claim.open('x') as file:
 json.dump({'at':'operator_GO','singleAttempt':True},file)
logname='takeover-retry-console.private' if retry else 'takeover-console.private'
with (r/logname).open('xb') as log:
 argv=['python3',str(r/'remote.py'),'resume','--root',str(r),'--GO']
 if retry:argv+=['--retry-readonly']
 p=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True,close_fds=True)
print(json.dumps({'phase':'detached_takeover_started','pid':p.pid,'retryReadOnly':retry}))
"""
    return ssh(code, {'root': root, 'retry': retry})


def start_remote(root):
    code = """import json,os,subprocess
from pathlib import Path
r=Path(payload)
with (r/'launch-claimed.json').open('x') as file:json.dump({'singleAttempt':True},file)
with (r/'console.private').open('xb') as log:
 p=subprocess.Popen(['python3',str(r/'remote.py'),'run','--root',str(r)],stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True,close_fds=True)
print(json.dumps({'phase':'detached_started','pid':p.pid}))
"""
    return ssh(code, root)


def start_remote_preparation(root, cleanup=False):
    code = """import json,subprocess
from pathlib import Path
r=Path(payload['root'])
role='cleanup' if payload['cleanup'] else 'prepare'
assert not (r/'attempt-claimed.json').exists()
with (r/('schema53-'+role+'-launch-claimed.json')).open('x') as file:
 json.dump({'singleAttempt':True},file)
with (r/(role+'-console.private')).open('xb') as log:
 p=subprocess.Popen(['python3',str(r/'remote.py'),role,'--root',str(r),'--GO'],stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True,close_fds=True)
print(json.dumps({'phase':'detached_preparation_started','pid':p.pid}))
"""
    return ssh(code, {'root': root, 'cleanup': cleanup})


def reconcile_preparation(root):
    code = """import json,subprocess
r=payload
p=subprocess.run(['python3',r+'/remote.py','reconcile-preparation','--root',r],capture_output=True,timeout=40)
print(p.stdout.decode() if p.returncode==0 else json.dumps({'phase':'schema53_preparation_unverified'}))
"""
    return ssh(code, root, timeout=55)


def preparation_recovery_admission(root, role, target):
    # 用当前消费者只读取原 run 的有界结构收据，不替换其脚本或 claim
    code = """import json
namespace={'__name__':'task40_preparation_recovery_admission'}
exec(compile(payload['code'],'remote-observer.py','exec'),namespace)
print(json.dumps(namespace['preparation_recovery_evidence'](payload['root'],payload['role'],payload['target'])))
"""
    return ssh(code, {'code': (HERE / 'remote.py').read_text(encoding='utf-8'),
                      'root': root, 'role': role, 'target': target})


@contextmanager
def action_lock(path, seconds=420):
    import msvcrt
    with Path(path).open('a+b') as lock:
        until = time.monotonic() + seconds
        while True:
            try:
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                break
            except OSError:
                if time.monotonic() >= until:
                    raise RuntimeError('local_worker_unsettled') from None
                time.sleep(.05)
        try:
            yield
        finally:
            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)


def bound_operation(j, operation):
    if j['accepted']['schema'] == 53:
        remote.require(operation in ('prepare', 'activate', 'cleanup') and
                       j.get('operation') == operation and j.get('pendingOperation') == operation,
                       'schema53_authorized_operation_binding_required')
    else:
        remote.require(operation in (None, 'launch'), 'schema52_launch_operation_required')
    return operation or j.get('operation', 'launch')


def work(run_dir, operation=None):
    with action_lock(run_dir / 'worker.lock'):
        jpath = run_dir / 'controller.json'
        j = read(jpath)
        # 旧/重复进程不准改写另一授权阶段的 journal，更不准重新下发
        operation = bound_operation(j, operation)
        try:
            remote.require(not (run_dir / 'recovery-requested').exists(), 'locally_fenced')
            if operation in ('launch', 'prepare') or (
                    operation == 'cleanup' and j['accepted'].get('preparationRecovery', {}).get('role') == 'cleanup'):
                remote.require(not j.get('provisionIntent'), 'provision_already_claimed_no_reissue')
                j['phase'] = 'provision_intent'; j['provisionIntent'] = True; save(jpath, j)
                # provision ack 失联可能已建远端目录；不猜测是否可重发
                if j['accepted']['schema'] == 53:
                    provision(j['accepted'], j['remoteRoot'], j['schema53Artifacts'])
                else:
                    provision(j['accepted'], j['remoteRoot'])
                remote.require(not (run_dir / 'recovery-requested').exists(), 'locally_fenced')
                j['phase'] = 'pulling_exact_image'; save(jpath, j)
                pull_exact(j['remoteRoot'])
            remote.require(not (run_dir / 'recovery-requested').exists(), 'locally_fenced')
            preparing = operation in ('prepare', 'cleanup')
            claim = 'schema53' + operation.title() + 'Intent' if preparing else 'launchIntent'
            remote.require(not j.get(claim), 'local_action_already_claimed_no_reissue')
            j[claim] = True
            j['phase'] = operation + '_intent'
            j['schemaMayHaveStarted' if preparing else 'remoteMayHaveStarted'] = True
            save(jpath, j)
            try:
                j['launch'] = (start_remote_preparation(j['remoteRoot'], cleanup=operation == 'cleanup')
                               if preparing else start_remote(j['remoteRoot']))
            except RuntimeError:
                j['launchAckUnknown'] = True
            save(jpath, j)
            j['remote'] = (observe(j['remoteRoot'], preparation=True, cleanup=operation == 'cleanup')
                           if preparing else observe(j['remoteRoot']))
            if preparing:
                j[operation + 'Receipt'] = j['remote']
            save(jpath, j)
            if j['remote']['phase'] in ('deployed', 'rolled_back_same52'):
                j['fresh'] = verify_remote(j['remoteRoot'])
            j['phase'] = 'finished'; save(jpath, j)
        except Exception as error:
            j['phase'] = 'worker_exit'
            j['errorCode'] = str(error) if isinstance(error, RuntimeError) else 'worker_error_suppressed'
            save(jpath, j)


def supervise(run_dir, operation=None):
    with action_lock(HERE / 'controller-active.lock'):
        operation = bound_operation(read(run_dir / 'controller.json'), operation)
        child = subprocess.Popen([sys.executable, '-B', str(HERE / 'controller.py'), 'worker', '--run-dir', str(run_dir),
                                  '--operation', operation],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        child.wait()
        j = read(run_dir / 'controller.json')
        bound_operation(j, operation)
        value = {'phase': j['phase'], 'operation': operation, 'workerExit': child.returncode,
                 'localLocks': {'controller': 'held_during_publication', 'worker': 'process_exited'},
                 'capturedAt': remote.now()}
        # 发布也属于同一阶段：先保独立收据，再更新摘要，最后开放下一授权
        if j['accepted']['schema'] == 53:
            save(run_dir / ('supervisor-' + operation + '.json'), value)
        save(run_dir / 'supervisor.json', value)
        if j['accepted']['schema'] == 53 and j['phase'] in ('finished', 'worker_exit'):
            j.pop('pendingOperation')
            save(run_dir / 'controller.json', j)


def recover_local(run_dir):
    # 只封本地未开始的后续动作、等持锁 worker；远端已启动时只读 receipt
    (run_dir / 'recovery-requested').touch(exist_ok=True)
    with action_lock(HERE / 'controller-active.lock'):
        with action_lock(run_dir / 'worker.lock'):
            j = read(run_dir / 'controller.json')
            if j.get('remoteMayHaveStarted'):
                j['remote'] = receipt(j['remoteRoot'])
                save(run_dir / 'controller.json', j)
            elif j.get('schemaMayHaveStarted'):
                j['remote'] = preparation_receipt(j['remoteRoot'], cleanup=j['operation'] == 'cleanup')
                save(run_dir / 'controller.json', j)
            return j


def takeover(run_dir):
    (run_dir / 'recovery-requested').touch(exist_ok=True)
    with action_lock(HERE / 'controller-active.lock'):
        with action_lock(run_dir / 'worker.lock'):
            j = read(run_dir / 'controller.json')
            remote.require(j.get('remoteMayHaveStarted') is True and
                           j['remoteRoot'].endswith('/' + run_dir.name) and
                           not j.get('takeoverRetryIntent'), 'takeover_run_owner_or_claim_drift')
            observed = receipt(j['remoteRoot'])
            retry = bool(j.get('takeoverIntent'))
            if retry:
                pending = observed['phase'] == 'takeover_pending' and observed.get('attemptClaimed') is True
                completed = (observed['phase'] == 'recovery_requires_operator' and
                             observed.get('retryableReadOnly') is True and
                             observed.get('takeoverCommandsIssued') is False)
                remote.require(pending or completed, 'takeover_retry_requires_confirmed_no_new_actions')
                evidence = retry_admission(j['remoteRoot'])
                remote.require(evidence.get('phase') == 'retry_admissible' and
                               evidence.get('mode') == ('orphaned_read_only' if pending else 'read_only_result'),
                               'takeover_retry_requires_confirmed_no_new_actions')
                j['takeoverRetryIntent'] = True
            else:
                remote.require(observed['phase'] in ('pending', 'recovery_requires_operator') and
                               observed.get('attemptClaimed', True), 'takeover_remote_not_recoverable')
                j['takeoverIntent'] = True
            j['takeoverAdmission'] = {'at': remote.now(), 'remotePhase': observed['phase'],
                                      'retryReadOnly': retry}
            j.pop('takeoverAckUnknown', None)
            save(run_dir / 'controller.json', j)
            try:
                j['takeoverLaunch'] = start_remote_takeover(j['remoteRoot'], retry=retry)
            except RuntimeError:
                j['takeoverAckUnknown'] = True
            save(run_dir / 'controller.json', j)
            return {'phase': 'takeover_single_attempt', 'runDirectory': str(run_dir),
                    'remoteRoot': j['remoteRoot'], 'retryReadOnly': retry,
                    'launchAckUnknown': j.get('takeoverAckUnknown', False)}


def spawn_supervisor(run_dir, operation):
    child = subprocess.Popen([sys.executable, '-B', str(HERE / 'controller.py'), 'supervise', '--run-dir', str(run_dir),
                              '--operation', operation],
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             creationflags=subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP)
    return {'phase': 'detached_supervisor_started', 'pid': child.pid, 'runDirectory': str(run_dir),
            'operation': operation}


def preparation_successor_path(run_dir):
    return HERE / ('schema53-recovery-' + run_dir.name + '-claimed.json')


def continue_schema53(run_dir, operation):
    remote.require(operation in ('activate', 'cleanup'), 'schema53_operation_invalid')
    with action_lock(HERE / 'controller-active.lock'):
        with action_lock(run_dir / 'worker.lock'):
            j = read(run_dir / 'controller.json')
            remote.require(not preparation_successor_path(run_dir).exists(),
                           'schema53_recovery_successor_already_claimed_observe_child')
            remote.require(j['accepted']['schema'] == 53 and j.get('schemaMayHaveStarted') and
                           not j.get('remoteMayHaveStarted') and not j.get('launchIntent') and
                           not (run_dir / 'recovery-requested').exists(), 'schema53_run_already_activated_or_fenced')
            remote.require(not j.get('pendingOperation') and j.get('phase') in ('finished', 'worker_exit'),
                           'schema53_previous_operation_unsettled')
            authorization = operation + 'Authorized'
            remote.require(not j.get(authorization), 'schema53_operation_already_authorized_no_reissue')
            if operation == 'activate':
                j['preparationAdmission'] = reconcile_preparation(j['remoteRoot'])
                remote.require(j['preparationAdmission'].get('phase') == 'schema53_prepared',
                               'schema53_preparation_unverified_no_activation')
            else:
                remote.require(not j.get('schema53CleanupIntent'), 'schema53_cleanup_already_claimed_no_reissue')
            j['operation'] = j['pendingOperation'] = operation
            j[authorization] = True
            j['phase'] = operation + '_authorized'
            save(run_dir / 'controller.json', j)
            return spawn_supervisor(run_dir, operation)


def new_run(m, operation, artifacts, recovery_source=None):
    if m['schema'] == 53:
        import schema53
        schema53.product_contract(artifacts, m['schema53Contract'])
        for role in schema53.ROLES:
            schema53.artifact_bytes(artifacts, m['schema53Contract'], role)
    run_dir = HERE / ('task40-' + uuid.uuid4().hex)
    run_dir.mkdir(mode=0o700)
    j = {'runId': run_dir.name, 'phase': 'prepared', 'accepted': m, 'operation': operation,
         'remoteRoot': '/opt/codex-proxy-rs/.runtime/deployments/' + run_dir.name,
         'startedAt': remote.now(), 'providerProbes': 0, 'channelMutations': 0}
    if m['schema'] == 53:
        j['pendingOperation'] = operation
    if artifacts:
        j['schema53Artifacts'] = str(artifacts.resolve())
    if recovery_source:
        j['recoverySourceDirectory'] = str(recovery_source.resolve())
    save(run_dir / 'controller.json', j)
    subprocess.run(['icacls', str(run_dir), '/inheritance:r', '/grant:r', os.getlogin() + ':(OI)(CI)F'],
                   capture_output=True, check=True)
    if recovery_source:
        with preparation_successor_path(recovery_source).open('x') as claim:
            json.dump({'sourceRunId': recovery_source.name, 'successorRunId': run_dir.name,
                       'role': operation, 'at': remote.now()}, claim)
            claim.flush()
            os.fsync(claim.fileno())
    return spawn_supervisor(run_dir, operation)


def recover_preparation(run_dir, target, artifacts):
    with action_lock(HERE / 'controller-active.lock'):
        with action_lock(run_dir / 'worker.lock'):
            j = read(run_dir / 'controller.json')
            remote.require(not preparation_successor_path(run_dir).exists(),
                           'schema53_recovery_successor_already_claimed_observe_child')
            role = j.get('operation')
            remote.require(target['schema'] == j['accepted']['schema'] == 53 and
                           role in ('prepare', 'cleanup') and j.get('schemaMayHaveStarted') and
                           not j.get('remoteMayHaveStarted') and not j.get('launchIntent') and
                           j.get('phase') in ('finished', 'worker_exit') and not j.get('pendingOperation') and
                           j['remoteRoot'].endswith('/' + run_dir.name), 'schema53_recovery_source_not_settled')
            original = j.get(role + 'Receipt', {})
            remote.zero_sql_preparation(original)
            remote.require(original.get('phase') == 'schema53_preparation_requires_operator',
                           'schema53_recovery_source_terminal_required')
            evidence = preparation_recovery_admission(j['remoteRoot'], role, target)
            remote.require(evidence.get('runId') == run_dir.name and evidence.get('role') == role and
                           evidence.get('sourceProcess') == 'exited' and evidence.get('schemaWrites') == 0,
                           'schema53_recovery_remote_evidence_invalid')
            m = dict(target, preparationRecovery={key: evidence[key] for key in ('runId', 'role', 'receiptSha256')})
            remote.manifest(m)
            return new_run(m, role, artifacts, recovery_source=run_dir)


def main():
    p = argparse.ArgumentParser()
    p.add_argument('action', choices=['baseline', 'launch', 'prepare', 'activate', 'cleanup', 'preparation-status',
                                     'recover-preparation', 'supervise', 'worker', 'status', 'recover-local', 'takeover'])
    p.add_argument('--accepted', type=Path)
    p.add_argument('--run-dir', type=Path)
    p.add_argument('--operation', choices=['launch', 'prepare', 'activate', 'cleanup'])
    p.add_argument('--GO', action='store_true')
    p.add_argument('--mode', choices=['healthy', 'disk_logging_recovery'], default='healthy')
    p.add_argument('--artifacts', type=Path, default=HERE / 'schema53-contract')
    a = p.parse_args()
    if a.action == 'baseline':
        print(json.dumps(baseline(a.mode), indent=2))
        return
    if a.action in ('launch', 'prepare'):
        remote.require(os.name == 'nt' and a.GO and a.accepted, 'Windows_new_GO_and_manifest_required')
        m = remote.manifest(read(a.accepted))
        remote.require((a.action == 'launch' and m['schema'] == 52) or
                       (a.action == 'prepare' and m['schema'] == 53 and a.artifacts),
                       'schema53_requires_online_prepare_then_new_activation_GO')
        remote.require('preparationRecovery' not in m, 'schema53_recovery_requires_linked_GO_entry')
        print(json.dumps(new_run(m, a.action, a.artifacts)))
        return
    remote.require(a.run_dir and a.run_dir.resolve().parent == HERE and
                   a.run_dir.name.startswith('task40-') and (a.run_dir / 'controller.json').exists(), 'owned_run_required')
    if a.action == 'supervise': supervise(a.run_dir, a.operation)
    elif a.action == 'worker': work(a.run_dir, a.operation)
    elif a.action == 'recover-local':
        remote.require(a.GO, 'explicit_GO_for_local_fence_required')
        print(json.dumps(recover_local(a.run_dir)))
    elif a.action == 'takeover':
        remote.require(os.name == 'nt' and a.GO, 'Windows_new_operator_recovery_GO_required')
        print(json.dumps(takeover(a.run_dir)))
    elif a.action == 'recover-preparation':
        remote.require(os.name == 'nt' and a.GO and a.accepted, 'Windows_new_preparation_recovery_GO_required')
        target = remote.manifest(read(a.accepted))
        remote.require('preparationRecovery' not in target, 'schema53_recovery_link_is_derived_from_source')
        print(json.dumps(recover_preparation(a.run_dir, target, a.artifacts)))
    elif a.action in ('activate', 'cleanup'):
        remote.require(os.name == 'nt' and a.GO, 'Windows_new_schema53_operation_GO_required')
        print(json.dumps(continue_schema53(a.run_dir, a.action)))
    elif a.action == 'preparation-status':
        j = read(a.run_dir / 'controller.json')
        print(json.dumps(reconcile_preparation(j['remoteRoot'])))
    else:
        j = read(a.run_dir / 'controller.json')
        if (a.run_dir / 'supervisor.json').exists():
            j['supervisor'] = read(a.run_dir / 'supervisor.json')
        print(json.dumps(j, indent=2))


if __name__ == '__main__':
    main()
