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


def baseline():
    # 部署前独立只读观测；无需 manifest 或 GO，不创建远端目录
    code = """import json
namespace={'__name__':'task40_observer'}
exec(compile(payload,'remote-observer.py','exec'),namespace)
print(json.dumps(namespace['baseline']()))
"""
    return ssh(code, (HERE / 'remote.py').read_text(encoding='utf-8'), timeout=90)


def provision(m, root):
    code = """import ast,json,os
from pathlib import Path
os.umask(0o077)
r=Path(payload['root'])
assert r.parent==Path('/opt/codex-proxy-rs/.runtime/deployments') and not r.exists()
ast.parse(payload['code'])
r.mkdir(mode=0o700)
(r/'remote.py').write_text(payload['code'])
(r/'accepted.json').write_text(json.dumps(payload['manifest']))
print(json.dumps({'phase':'provisioned'}))
"""
    return ssh(code, {'root': root, 'code': (HERE / 'remote.py').read_text(encoding='utf-8'), 'manifest': m})


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


def observe(root, seconds=4500, clock=time.monotonic, sleep=time.sleep):
    until = clock() + seconds
    unstarted = 0
    while clock() < until:
        try:
            value = receipt(root)
            if value['phase'] != 'pending':
                return value
            unstarted = unstarted + 1 if not value.get('attemptClaimed') else 0
            if unstarted >= 3:
                value = abandon_unstarted(root)
                if value['phase'] != 'pending':
                    return value
        except RuntimeError:
            pass
        sleep(10)
    raise RuntimeError('remote_receipt_unknown_no_relaunch')


def pull_exact(root):
    code = """import json,subprocess
from pathlib import Path
m=json.loads((Path(payload)/'accepted.json').read_text())
p=subprocess.run(['docker','pull',m['image']],capture_output=True,timeout=420)
print(json.dumps({'phase':'pulled' if p.returncode==0 else 'pull_blocked'}))
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


def work(run_dir):
    with action_lock(run_dir / 'worker.lock'):
        jpath = run_dir / 'controller.json'
        j = read(jpath)
        try:
            remote.require(not (run_dir / 'recovery-requested').exists(), 'locally_fenced')
            j['phase'] = 'provision_intent'; save(jpath, j)
            # provision ack 失联可能已建远端目录；不猜测是否可重发
            provision(j['accepted'], j['remoteRoot'])
            remote.require(not (run_dir / 'recovery-requested').exists(), 'locally_fenced')
            j['phase'] = 'pulling_exact_image'; save(jpath, j)
            pull_exact(j['remoteRoot'])
            remote.require(not (run_dir / 'recovery-requested').exists(), 'locally_fenced')
            j['phase'] = 'launch_intent'; j['remoteMayHaveStarted'] = True; save(jpath, j)
            try:
                j['launch'] = start_remote(j['remoteRoot']); save(jpath, j)
            except RuntimeError:
                j['launchAckUnknown'] = True; save(jpath, j)
            j['remote'] = observe(j['remoteRoot'])
            save(jpath, j)
            if j['remote']['phase'] in ('deployed', 'rolled_back_same52'):
                j['fresh'] = verify_remote(j['remoteRoot'])
            j['phase'] = 'finished'; save(jpath, j)
        except Exception as error:
            j['phase'] = 'worker_exit'
            j['errorCode'] = str(error) if isinstance(error, RuntimeError) else 'worker_error_suppressed'
            save(jpath, j)


def supervise(run_dir):
    with action_lock(HERE / 'controller-active.lock'):
        child = subprocess.Popen([sys.executable, '-B', str(HERE / 'controller.py'), 'worker', '--run-dir', str(run_dir)],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        child.wait()
    # supervisor 与 worker 均已退出持锁范围；不自动重发远端动作
    j = read(run_dir / 'controller.json')
    save(run_dir / 'supervisor.json', {'phase': j['phase'], 'workerExit': child.returncode,
                                      'localLocks': 'unheld', 'capturedAt': remote.now()})


def recover_local(run_dir):
    # 只封本地未开始的后续动作、等持锁 worker；远端已启动时只读 receipt
    (run_dir / 'recovery-requested').touch(exist_ok=True)
    with action_lock(HERE / 'controller-active.lock'):
        with action_lock(run_dir / 'worker.lock'):
            j = read(run_dir / 'controller.json')
            if j.get('remoteMayHaveStarted'):
                j['remote'] = receipt(j['remoteRoot'])
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


def main():
    p = argparse.ArgumentParser()
    p.add_argument('action', choices=['baseline', 'launch', 'supervise', 'worker', 'status', 'recover-local', 'takeover'])
    p.add_argument('--accepted', type=Path)
    p.add_argument('--run-dir', type=Path)
    p.add_argument('--GO', action='store_true')
    a = p.parse_args()
    if a.action == 'baseline':
        print(json.dumps(baseline(), indent=2))
        return
    if a.action == 'launch':
        remote.require(os.name == 'nt' and a.GO and a.accepted, 'Windows_new_GO_and_manifest_required')
        m = remote.manifest(read(a.accepted))
        run_dir = HERE / ('task40-' + uuid.uuid4().hex)
        run_dir.mkdir(mode=0o700)
        j = {'runId': run_dir.name, 'phase': 'prepared', 'accepted': m,
             'remoteRoot': '/opt/codex-proxy-rs/.runtime/deployments/' + run_dir.name,
             'startedAt': remote.now(), 'providerProbes': 0, 'channelMutations': 0}
        save(run_dir / 'controller.json', j)
        subprocess.run(['icacls', str(run_dir), '/inheritance:r', '/grant:r', os.getlogin() + ':(OI)(CI)F'],
                       capture_output=True, check=True)
        child = subprocess.Popen([sys.executable, '-B', str(HERE / 'controller.py'), 'supervise', '--run-dir', str(run_dir)],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 creationflags=subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP)
        print(json.dumps({'phase': 'detached_supervisor_started', 'pid': child.pid, 'runDirectory': str(run_dir)}))
        return
    remote.require(a.run_dir and a.run_dir.resolve().parent == HERE and
                   a.run_dir.name.startswith('task40-') and (a.run_dir / 'controller.json').exists(), 'owned_run_required')
    if a.action == 'supervise': supervise(a.run_dir)
    elif a.action == 'worker': work(a.run_dir)
    elif a.action == 'recover-local':
        remote.require(a.GO, 'explicit_GO_for_local_fence_required')
        print(json.dumps(recover_local(a.run_dir)))
    elif a.action == 'takeover':
        remote.require(os.name == 'nt' and a.GO, 'Windows_new_operator_recovery_GO_required')
        print(json.dumps(takeover(a.run_dir)))
    else:
        j = read(a.run_dir / 'controller.json')
        if (a.run_dir / 'supervisor.json').exists():
            j['supervisor'] = read(a.run_dir / 'supervisor.json')
        print(json.dumps(j, indent=2))


if __name__ == '__main__':
    main()
