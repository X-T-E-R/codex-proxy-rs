"""在已停的隔离 Compose 实例验证真实 PG、Redis、配置认证与跨阶段续作"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('site_recovery', ROOT / 'deploy/site-recovery.py')
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)
COMPOSE = ['docker', 'compose', '--project-name', os.environ['COMPOSE_PROJECT_NAME'], '-f', 'deploy/compose.yaml']
os.environ['PGPASSWORD'] = os.environ['CI_POSTGRES_PASSWORD']
os.environ['REDISCLI_AUTH'] = os.environ['CI_REDIS_PASSWORD']
PG = COMPOSE + ['exec', '-T', '-e', 'PGPASSWORD', 'postgres', 'psql', '-U', 'codex_proxy', '-d', 'codex_proxy']
REDIS = COMPOSE + ['exec', '-T', '-e', 'REDISCLI_AUTH', 'redis', 'redis-cli', '--raw']
HELPER_IMAGE = recovery.run(['docker', 'image', 'inspect', '--format', '{{.Id}}', os.environ['CPR_IMAGE']])
HELPER_SOURCE = recovery.run(['docker', 'image', 'inspect', '--format',
    '{{index .Config.Labels "org.opencontainers.image.revision"}}', HELPER_IMAGE])


def helper(*environment):
    return ['docker', 'run', '--rm', '-i', '--network', os.environ['COMPOSE_PROJECT_NAME'] + '_default',
        '--mount', 'type=bind,src=' + str(ROOT / 'deploy/config.yaml') + ',dst=/app/deploy/config.yaml,readonly',
        '-e', 'CPR_REDIS_URL=redis://redis:6379/', *environment,
        HELPER_IMAGE, '/app/bin/codex-proxy-rs', 'site-recovery-redis']


HELPER = helper()
checks = []


def sql(text):
    return recovery.run(PG + ['-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=1'], text)


def redis(*args):
    return recovery.run(REDIS + list(args))


def refused(action):
    try:
        action()
    except RuntimeError:
        return
    raise AssertionError('unsafe recovery unexpectedly accepted')


old = recovery.run(COMPOSE + ['ps', '-a', '-q', 'codex-proxy-rs'])
# inspect 只投影 ID，不加载 Env 或完整对象
old = recovery.run(['docker', 'inspect', '--format', '{{.Id}}', old])
recovery.run(['docker', 'update', '--restart=no', old])
with tempfile.TemporaryDirectory() as directory:
    args = SimpleNamespace(old_container=old, pg=PG, redis=HELPER,
        helper_image=HELPER_IMAGE, helper_source=HELPER_SOURCE,
        receipt=str(Path(directory) / 'receipt.json'), apply=True)
    recovery.guard(args)
    recovery.run(['docker', 'start', old])
    refused(lambda: recovery.execute(args))
    recovery.run(COMPOSE + ['stop', '--timeout', '30', 'codex-proxy-rs'])
    checks.append('actual live app refused without recovery writes')

    # 额外同 service 进程，即使不连 PG 也必须拒绝
    extra = recovery.run(['docker', 'run', '-d', '--label',
        'com.docker.compose.service=codex-proxy-rs', os.environ['CPR_IMAGE'], 'sleep', '30'])
    try:
        refused(lambda: recovery.execute(args))
    finally:
        recovery.run(['docker', 'rm', '-f', extra])
    checks.append('another live compose service refused')

    writer = subprocess.Popen(PG + ['-X', '-q', '-A', '-t'], stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        # 保持已确认的数据库会话直到检查结束，不让容器启动耗时穿过固定 sleep
        writer.stdin.write("select 'writer-ready';\n")
        writer.stdin.flush()
        assert writer.stdout.readline().strip() == 'writer-ready'
        refused(lambda: recovery.execute(args))
    finally:
        writer.stdin.close()
        writer.wait(timeout=10)
    assert writer.returncode == 0
    checks.append('actual other PG client refused')

    # 完整当前 schema，合成未发送/已发送请求和未过期长租约；不调用上游
    sql("""
    insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,
      provider_account_ref,upstream_transport,upstream_send_state,outcome,attempt_count,started_at,request_observation_json)
    select id,'key_ci_recovery','responses','http_sse','openai',
      case when sent then 'acct_ci_recovery' else null end,
      case when sent then 'websocket' else null end,
      case when sent then 'sent' else 'not_sent' end,'running',case when sent then 1 else 0 end,
      now()-interval '1 minute',
      '{"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]}}'::jsonb
    from (values ('req_ci_recovery_unsent',false),('req_ci_recovery_sent',true)) v(id,sent);
    insert into model_request_leases select id,now()+interval '1 hour' from model_requests where id like 'req_ci_recovery_%';
    """)
    before_ledger = sql('select count(*) from client_api_key_charge_events;')
    before_migrations = sql('select jsonb_agg(jsonb_build_array(version,success) order by version) from _sqlx_migrations;')
    before_send = sql("select jsonb_agg(jsonb_build_array(id,attempt_count,upstream_send_state) order by id) from model_requests where id like 'req_ci_recovery_%';")
    client = 'key_ci_recovery'
    request = 'req_ci_recovery_unsent'
    other = 'req_ci_unattributed'
    tag = hashlib.sha256(client.encode()).hexdigest()
    active = f'codex-proxy-rs:client:{{{tag}}}:active'
    rpm = f'codex-proxy-rs:client:{{{tag}}}:requests'
    account_tag = hashlib.sha256(b'ci-recovery-account').hexdigest()
    accounts = [f'codex-proxy-rs:lease:{scope}:{{{account_tag}}}:active'
        for scope in ['account', 'account-reserved']]
    for key in accounts:
        for lease, rid, key_id in [('old', request, client), ('alive', other, client),
                ('wrongkey', request, 'key_another')]:
            redis('ZADD', key, '9000000000000', f'owner|{lease}|1')
            redis('HSET', f'{key}:detail:{lease}', 'facts',
                json.dumps({'request_id': rid, 'client_key_id': key_id}))
        redis('ZADD', key, '9000000000000', 'owner|missing|1')
    for key in [active, rpm]:
        redis('ZADD', key, '9000000000000', request, '9000000000000', other)

    # 只继承认证变量名，值不在 argv 或 receipt 中；错误认证先于任何 PG 终结
    os.environ['CPR_REDIS_PASSWORD'] = 'synthetic-intentionally-wrong'
    args.redis = helper('-e', 'CPR_REDIS_PASSWORD')
    recovery.command(json.dumps(args.redis), helper=True)
    refused(lambda: recovery.execute(args))
    assert sql("select count(*) from model_requests where id like 'req_ci_recovery_%' and outcome='running';") == '2'
    assert not Path(args.receipt).exists()
    del os.environ['CPR_REDIS_PASSWORD']
    args.redis = HELPER
    checks.append('normal config Redis authentication; wrong credentials refuse before PG mutation')

    # 先准备并改变租约，证明精确 CAS 不可越过；原 receipt 必须保留
    args.apply = False
    assert recovery.execute(args) == {'phase': 'prepared', 'requests': 2}
    manifest = json.loads(Path(args.receipt).read_text())
    assert Path(args.receipt).stat().st_mode & 0o777 == 0o600
    args.apply = True
    sql("update model_request_leases set lease_expires_at=lease_expires_at+interval '1 second' where request_id='req_ci_recovery_unsent';")
    refused(lambda: recovery.execute(args))
    assert redis('ZCARD', active) == '2'
    assert json.loads(Path(args.receipt).read_text())['phase'] == 'prepared'
    # 另一条已 CAS 成功而本条失配时拒绝 complete，不释放任何 Redis 槽；恢复其精确原 expiry 后续作
    sql("update model_request_leases set lease_expires_at=lease_expires_at-interval '1 second' where request_id='req_ci_recovery_unsent';")
    checks.append('changed unexpired lease refuses completion and Redis release')

    # PG 已提交但 batch 意图尚未声明时中断，原 receipt 仍可精确续作
    with patch.object(recovery, 'replace_receipt', side_effect=RuntimeError('synthetic interruption before batch intent')):
        refused(lambda: recovery.execute(args))
    assert sql("select count(*) from model_requests where id like 'req_ci_recovery_%' and outcome='incomplete' and error_kind='process_interrupted';") == '2'
    assert sql("select count(*) from model_request_leases where request_id like 'req_ci_recovery_%';") == '0'
    first_completion = sql("select jsonb_agg(jsonb_build_array(id,completed_at) order by id) from model_requests where id like 'req_ci_recovery_%';")
    assert json.loads(Path(args.receipt).read_text())['phase'] == 'prepared'
    value = recovery.execute(args)
    assert value == {'phase': 'complete', 'requests': 2, 'accountSlots': 2, 'clientSlots': 1}, value
    assert redis('ZRANGE', active, '0', '-1') == other
    assert redis('ZCARD', rpm) == '2'
    for key in accounts:
        assert redis('ZCARD', key) == '3'
        assert redis('EXISTS', f'{key}:detail:old') == '0'
        assert redis('EXISTS', f'{key}:detail:wrongkey') == '1'
    assert sql('select count(*) from client_api_key_charge_events;') == before_ledger
    assert recovery.execute(args)['phase'] == 'complete'
    assert first_completion == sql("select jsonb_agg(jsonb_build_array(id,completed_at) order by id) from model_requests where id like 'req_ci_recovery_%';")
    checks.append('PG trigger removes exact request leases; normal/reserved account and client slots released after PG commit')
    checks.append('same-run resume and replay preserve terminal time, foreign key/request, missing metadata, RPM and ledger')

    # stdout 是 watchdog 消费的单 JSON；CLI argv 不包含认证值
    cli = subprocess.run(['python', 'deploy/site-recovery.py', '--old-container', old,
        '--pg-command-json', json.dumps(PG), '--redis-helper-command-json', json.dumps(HELPER),
        '--helper-image', HELPER_IMAGE, '--helper-source', HELPER_SOURCE, '--receipt', args.receipt, '--ack-single-deployment', '--apply'],
        text=True, capture_output=True, timeout=120)
    assert cli.returncode == 0, cli.stderr
    assert json.loads(cli.stdout)['phase'] == 'complete'
    assert len(cli.stdout.splitlines()) == 1
    assert all(os.environ[name] not in Path(args.receipt).read_text()
        for name in ['CI_POSTGRES_PASSWORD', 'CI_REDIS_PASSWORD'])
    checks.append('host CLI single complete JSON and credential-free receipt/argv')
    assert before_migrations == sql('select jsonb_agg(jsonb_build_array(version,success) order by version) from _sqlx_migrations;')
    assert before_send == sql("select jsonb_agg(jsonb_build_array(id,attempt_count,upstream_send_state) order by id) from model_requests where id like 'req_ci_recovery_%';")
    checks.append('migration ledger and real not-sent/sent attempt facts unchanged')

    # 使用独立请求证明真实 batch 已成功但 ACK 丢失时不能重新释放或发布 complete
    unknown_request = 'req_ci_recovery_ack'
    sql("""insert into model_requests(id,client_api_key_ref,operation,client_transport,outcome,started_at,request_observation_json)
      select 'req_ci_recovery_ack',client_api_key_ref,operation,client_transport,'running',started_at,request_observation_json
      from model_requests where id='req_ci_recovery_unsent';
      insert into model_request_leases values('req_ci_recovery_ack',now()+interval '1 hour');""")
    redis('ZADD', active, '9000000000000', unknown_request)
    for key in accounts:
        redis('ZADD', key, '9000000000000', 'owner|ack|1')
        redis('HSET', key + ':detail:ack', 'facts', json.dumps({'request_id': unknown_request, 'client_key_id': client}))
    args.receipt = str(Path(directory) / 'lost-ack.json')
    actual_release = recovery.release
    batches = []
    def lost_ack(*params):
        batches.append(actual_release(*params))
        raise RuntimeError('synthetic acknowledgement loss after actual batch success')
    with patch.object(recovery, 'release', side_effect=lost_ack):
        refused(lambda: recovery.execute(args))
    assert batches == [{'accountSlots': 2, 'clientSlots': 1}]
    assert json.loads(Path(args.receipt).read_text())['batch']['outcome'] == 'unknown'
    with patch.object(recovery, 'release') as retry:
        refused(lambda: recovery.execute(args))
        retry.assert_not_called()
    assert sql("select outcome from model_requests where id='req_ci_recovery_ack';") == 'incomplete'
    assert redis('ZRANGE', active, '0', '-1') == other
    assert sql('select count(*) from client_api_key_charge_events;') == before_ledger
    checks.append('actual batch succeeded then ACK lost: durable unknown, no same-receipt batch reissue')

print(json.dumps({'checks': checks, 'count': len(checks)}, indent=2))
