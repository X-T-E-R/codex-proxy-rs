#!/usr/bin/env python3
"""单部署 pre-start 停止闸：准备/应用精确旧请求恢复，不启停服务，不读取或传递凭据。

调用方必须全程持有站点生命周期锁、维护闸和单部署写入授权。
PG 使用现有 peer/服务认证的 psql argv；Redis 使用同镜像 site-recovery-redis 子命令的正常配置认证。
默认 prepare；只有 --apply 写入。每个旧容器 run 使用独立持久 receipt，失败后用原 receipt 重入。
"""
import argparse
import csv
from datetime import datetime
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys

NAMESPACE = 'codex-proxy-rs'
SNAPSHOT = """select coalesce(jsonb_agg(jsonb_build_object(
'id',r.id,'client',r.client_api_key_ref,'started',r.started_at::text,
'expiry',l.lease_expires_at::text)),'[]'::jsonb)::text
from model_requests r left join model_request_leases l on l.request_id=r.id
where r.outcome='running';"""
CHECK_CLIENTS = """select count(*) from pg_stat_activity where datname=current_database()
and backend_type='client backend' and pid<>pg_backend_pid();"""
APPLY_PREFIX = """begin;
set local statement_timeout='15s'; set local lock_timeout='3s';
select pg_advisory_xact_lock(3649,20261024);
lock table model_requests in share row exclusive mode;
lock table model_request_leases in share row exclusive mode;
do $$ begin
if exists(select 1 from pg_stat_activity where datname=current_database()
 and backend_type='client backend' and pid<>pg_backend_pid()) then
 raise exception 'another database client exists'; end if;
end $$;
create temp table recovery_input(id text primary key, client text, started timestamptz, expiry timestamptz) on commit drop;
copy recovery_input from stdin with (format csv, null '');
"""
APPLY_SUFFIX = """do $$ begin
if exists(select 1 from model_requests r where r.outcome='running'
 and not exists(select 1 from recovery_input i where i.id=r.id)) then
 raise exception 'running request outside prepared manifest'; end if;
end $$;
with changed as (
 update model_requests r set outcome='incomplete',error_kind='process_interrupted',
 request_observation_json=r.request_observation_json || jsonb_build_object('error',
 jsonb_build_object('message','request owner exited; single-deployment pre-start recovery')),
 image_generation_succeeded=case when r.image_generation_requested then false else null end,
 completed_at=greatest(now(),r.started_at)
 from recovery_input i where r.id=i.id and r.client_api_key_ref=i.client
 and r.started_at=i.started and r.outcome='running'
 and ((i.expiry is null and not exists(select 1 from model_request_leases l where l.request_id=r.id))
 or exists(select 1 from model_request_leases l where l.request_id=r.id and l.lease_expires_at=i.expiry))
 returning r.id
) select count(*) from changed;
select coalesce(jsonb_agg(i.id),'[]'::jsonb)::text from recovery_input i
join model_requests r on r.id=i.id and r.client_api_key_ref=i.client and r.started_at=i.started
where r.outcome='incomplete' and r.error_kind='process_interrupted'
 and r.request_observation_json #>> '{error,message}' = 'request owner exited; single-deployment pre-start recovery'
 and r.recovery_lease_expires_at=coalesce(i.expiry,i.started);
commit;
"""
PROJECTION = ('{"Id":{{json .Id}},"Running":{{json .State.Running}},'
    '"Restarting":{{json .State.Restarting}},"Pid":{{json .State.Pid}},'
    '"Status":{{json .State.Status}},"StartedAt":{{json .State.StartedAt}},'
    '"FinishedAt":{{json .State.FinishedAt}},"RestartPolicy":{{json .HostConfig.RestartPolicy.Name}},'
    '"Service":{{json (index .Config.Labels "com.docker.compose.service")}},'
    '"Project":{{json (index .Config.Labels "com.docker.compose.project")}}}')


def run(argv, data=None):
    result = subprocess.run(argv, input=data, text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError('command failed; raw stderr intentionally not exported')
    return result.stdout.strip()


def command(value, helper=False):
    argv = json.loads(value)
    if not isinstance(argv, list) or not argv or not all(isinstance(x, str) and x for x in argv):
        raise ValueError('command must be a nonempty JSON argv array')
    for index, value in enumerate(argv):
        # Docker 允许只传环境变量名；值由现有认证环境继承，不进入 ps 或 receipt
        if index and argv[index - 1] in ('-e', '--env') and value in (
                'PGPASSWORD', 'PGPASSFILE', 'CPR_REDIS_PASSWORD'):
            continue
        if (re.search(r'password|rediscli_auth|pgpassword|://[^/]*:[^/@]*@', value, re.I)
                or value in ('-a', '--pass', '--password', '-c', 'sh', 'bash')):
            raise ValueError('credentials and shell command strings are forbidden in argv')
    if helper and argv[-1] != 'site-recovery-redis':
        raise ValueError('Redis must use the normal-config site-recovery-redis helper')
    return argv


def pg(args, sql):
    return run(args.pg + ['-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=1'], sql)


def redis(args, request):
    return json.loads(run(args.redis, json.dumps(request)))


def guard(args):
    old = json.loads(run(['docker', 'inspect', '--format', PROJECTION, args.old_container]))
    if old['Id'] != args.old_container or not re.fullmatch('[0-9a-f]{64}', old['Id']):
        raise RuntimeError('old container must be the full immutable ID')
    if old['Running'] or old['Restarting'] or old['Pid'] != 0 or old['Status'] not in ('exited', 'dead'):
        raise RuntimeError('old app process is not confirmed stopped')
    if old['RestartPolicy'] != 'no' or old['FinishedAt'].startswith('0001-'):
        raise RuntimeError('old app needs restart=no and a completed exit')
    if old['Service'] != 'codex-proxy-rs' or not old['Project']:
        raise RuntimeError('old app compose identity is missing')
    active = run(['docker', 'ps', '-q', '--filter', 'label=com.docker.compose.service=codex-proxy-rs'])
    if active:
        raise RuntimeError('another compose app is running')
    if int(pg(args, CHECK_CLIENTS)) != 0:
        raise RuntimeError('another database client exists')
    return {key: old[key] for key in ('Id', 'StartedAt', 'FinishedAt', 'Project')}


def validate_rows(rows):
    if not isinstance(rows, list) or len(rows) > 5000:
        raise ValueError('request manifest exceeds bound')
    ids = set()
    timestamp = r'[0-9T :.+-]+(?:Z)?'
    for row in rows:
        if not isinstance(row, dict) or set(row) != {'id', 'client', 'started', 'expiry'}:
            raise ValueError('invalid request manifest shape')
        if not isinstance(row['id'], str) or not re.fullmatch(r'req_[A-Za-z0-9_-]{1,124}', row['id']) or row['id'] in ids:
            raise ValueError('invalid or duplicate request ID')
        if not isinstance(row['client'], str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,256}', row['client']):
            raise ValueError('invalid client reference')
        for field in ('started', 'expiry'):
            if field == 'expiry' and row[field] is None:
                continue
            if not isinstance(row[field], str) or not re.fullmatch(timestamp, row[field]):
                raise ValueError('invalid timestamp identity')
        ids.add(row['id'])
    return rows


def apply_sql(rows):
    # COPY 数据流与 SQL 分离；manifest 不放 argv，也不插入 SQL 字符串字面量
    values = io.StringIO(newline='')
    writer = csv.writer(values, lineterminator='\n')
    for row in rows:
        writer.writerow([row['id'], row['client'], row['started'], row['expiry']])
    return APPLY_PREFIX + values.getvalue() + '\\.\n' + APPLY_SUFFIX


def save(path, value, exclusive=False):
    flags = os.O_CREAT | os.O_WRONLY | (os.O_EXCL if exclusive else os.O_TRUNC)
    flags |= getattr(os, 'O_NOFOLLOW', 0)
    fd = os.open(path, flags, 0o600)
    with os.fdopen(fd, 'w') as file:
        json.dump(value, file)
        file.flush()
        os.fsync(file.fileno())
    sync_directory(Path(path).parent)


def sync_directory(path):
    # 站点入口运行于 Linux；目录持久化确保 PG 提交后仍可找到原 manifest 续作
    if os.name == 'posix':
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


def release(args, rows, completed):
    approved = {row['id']: row['client'] for row in rows if row['id'] in completed}
    counts = {'accountSlots': 0, 'clientSlots': 0}
    if not approved:
        return counts
    for reserved in (False, True):
        cursor, scanned = '0', 0
        while True:
            cursor, keys = redis(args, {'kind': 'scan', 'cursor': int(cursor), 'reserved': reserved})
            for key in keys:
                counts['accountSlots'] += int(redis(args, {'kind': 'releaseAccount', 'key': key, 'requests': approved}))
            scanned += len(keys)
            if scanned > 10000:
                raise RuntimeError('recovery key scan exceeds bound')
            if str(cursor) == '0':
                break
    grouped = {}
    for request, client in approved.items():
        grouped.setdefault(client, []).append(request)
    for client, requests in grouped.items():
        counts['clientSlots'] += int(redis(args, {'kind': 'releaseClient', 'client': client, 'requests': requests}))
    return counts


def execute(args):
    path = Path(args.receipt)
    identity = guard(args)
    # 在 PG 写入前先验证正常 Redis 认证路径；错误认证必须不触发终结
    redis(args, {'kind': 'scan', 'cursor': 0, 'reserved': False})
    if path.exists():
        if path.is_symlink():
            raise RuntimeError('receipt must not be a symlink')
        receipt = json.loads(path.read_text())
        if receipt['owner'] != identity:
            raise RuntimeError('receipt belongs to another container run')
        rows = validate_rows(receipt['requests'])
    else:
        rows = validate_rows(json.loads(pg(args, SNAPSHOT)))
        receipt = {'owner': identity, 'requests': rows, 'phase': 'prepared'}
        path.parent.mkdir(parents=True, exist_ok=True)
        save(path, receipt, exclusive=True)
    finished = datetime.fromisoformat(identity['FinishedAt'].replace('Z', '+00:00'))
    if any(datetime.fromisoformat(row['started'].replace('Z', '+00:00')) > finished for row in rows):
        raise RuntimeError('request started after the confirmed process exit')
    if not args.apply:
        return {'phase': 'prepared', 'requests': len(rows)}
    guard(args)
    result = pg(args, apply_sql(rows))
    arrays = [line for line in result.splitlines() if line.strip().startswith('[')]
    if len(arrays) != 1:
        raise RuntimeError('unexpected recovery transaction result')
    completed = set(json.loads(arrays[0]))
    if completed != {row['id'] for row in rows}:
        raise RuntimeError('request identity or lease changed; manual reconciliation required')
    guard(args)
    counts = release(args, rows, completed)
    receipt.update(phase='complete', completed=sorted(completed), released=counts)
    temporary = path.with_suffix(path.suffix+'.tmp')
    save(temporary, receipt)
    os.replace(temporary, path)
    sync_directory(path.parent)
    # 此处 complete 仅指 manifest 内的可证明请求；无 detail/关联不明的槽仍保 TTL
    return {'phase': 'complete', 'requests': len(completed), **counts}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--old-container', required=True)
    parser.add_argument('--pg-command-json', required=True)
    parser.add_argument('--redis-helper-command-json', required=True)
    parser.add_argument('--receipt', required=True)
    parser.add_argument('--ack-single-deployment', action='store_true', required=True)
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    args.pg = command(args.pg_command_json)
    args.redis = command(args.redis_helper_command_json, helper=True)
    print(json.dumps(execute(args)))


if __name__ == '__main__':
    try:
        main()
    except Exception:
        print('recovery refused; no automatic start permitted, inspect the prepared receipt and service status', file=sys.stderr)
        sys.exit(1)
