"""隔离 Compose 中验证精确镜像的在线准备、schema53、固定 helper 与回52恢复"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from unittest.mock import patch
import uuid

ROOT = Path(__file__).resolve().parents[2]
CARRIER = ROOT / 'deploy/task40-prod-upgrade'
spec = importlib.util.spec_from_file_location('task40_schema53', CARRIER / 'schema53.py')
schema53 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(schema53)
spec = importlib.util.spec_from_file_location('task40_preservation', CARRIER / 'remote.py')
preservation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preservation)
OLD_SOURCE = '90497b7106bb228c21858293bfca0327a9f10072'
OLD_IMAGE = 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:a4db0c28cddd35691a1e87d13bfdeed8f5f2d4e48d82a1b0b4df7e569e181b03'
HELPER_SOURCE = '476d24c16764a0a535fabd975a3df18d2192df0a'
HELPER_IMAGE = 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:f8c705a4da6687ea39902bb09e020772a4083d2dae12c1d0cd507952bf88944e'


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def run(argv, data=None, timeout=120, env=None):
    process = subprocess.run(argv, input=data, capture_output=True, text=True, timeout=timeout, env=env)
    require(process.returncode == 0, 'isolated_command_failed_' + str(process.returncode))
    return process.stdout.strip()


def image_identity(image, source):
    fmt = '{"source":{{json (index .Config.Labels "org.opencontainers.image.revision")}},"os":{{json .Os}},"arch":{{json .Architecture}},"digests":{{json .RepoDigests}}}'
    value = json.loads(run(['docker', 'image', 'inspect', '--format', fmt, image]))
    require(value['source'] == source and value['os'] == 'linux' and value['arch'] == 'amd64' and
            image in value['digests'], 'exact_runtime_image_identity_mismatch')
    return value


def verify(candidate, source, output):
    require(re.fullmatch(r'ghcr\.io/x-t-e-r/codex-proxy-rs@sha256:[0-9a-f]{64}', candidate) and
            re.fullmatch('[0-9a-f]{40}', source) and candidate != OLD_IMAGE and source != OLD_SOURCE,
            'new_exact_runtime_image_and_source_required')
    binding = json.loads((CARRIER / 'accepted.schema53.example.json').read_text(encoding='utf-8'))['schema53Contract']
    binding['evidence'] = 'same-image deployment verification'
    product = schema53.product_contract(schema53.BUNDLE, binding)
    require(source == product['product_candidate'], 'runtime_source_must_match_product_contract')
    scripts = {role: schema53.artifact_bytes(schema53.BUNDLE, binding, role).decode('utf-8')
               for role in schema53.ROLES}
    identities = {}
    for image, revision in ((candidate, source), (OLD_IMAGE, OLD_SOURCE), (HELPER_IMAGE, HELPER_SOURCE)):
        run(['docker', 'pull', image], timeout=420)
        identities[image] = image_identity(image, revision)
    checks = []
    project = 'task40-schema53-' + uuid.uuid4().hex[:12]
    env = dict(os.environ, MSYS_NO_PATHCONV='1', CPR_IMAGE=OLD_IMAGE,
               PGPASSWORD=uuid.uuid4().hex + uuid.uuid4().hex[:16],
               REDISCLI_AUTH=uuid.uuid4().hex + uuid.uuid4().hex[:16])
    with tempfile.TemporaryDirectory(prefix=project) as directory:
        directory = Path(directory)
        deploy = directory / 'deploy'
        deploy.mkdir()
        config = (ROOT / 'deploy/config.example.yaml').read_text(encoding='utf-8')
        for old, new in (("password: &postgres_password ''", "password: &postgres_password '" + env['PGPASSWORD'] + "'"),
                         ("password: &redis_password ''", "password: &redis_password '" + env['REDISCLI_AUTH'] + "'"),
                         ("default_password: ''", "default_password: '" + uuid.uuid4().hex + "'")):
            require(config.count(old) == 1, 'config_template_changed')
            config = config.replace(old, new)
        (deploy / 'config.yaml').write_text(config, encoding='utf-8')
        compose = (ROOT / 'deploy/compose.yaml').read_text(encoding='utf-8')
        for port in (5432, 6379, 8080):
            original = '127.0.0.1:' + str(port) + ':' + str(port)
            require(compose.count(original) == 1, 'compose_port_contract_changed')
            compose = compose.replace(original, '127.0.0.1::' + str(port))
        compose = compose.replace('restart: unless-stopped', 'restart: "no"')
        # 为消费者原样 docker exec psql 提供本夹具现有认证，仍使用 SCRAM
        compose = compose.replace('POSTGRES_USER: codex_proxy', 'POSTGRES_USER: codex_proxy\n      PGPASSWORD: ${PGPASSWORD}')
        compose = compose.replace('type: bind', 'type: volume').replace('        bind:\n          create_host_path: false\n', '')
        for name in ('postgres', 'redis', 'data', 'logs'):
            compose = compose.replace('source: ../.runtime/' + name, 'source: ' + name)
        compose += '\nnetworks:\n  default:\n    internal: true\nvolumes:\n  postgres:\n  redis:\n  data:\n  logs:\n'
        (deploy / 'compose.yaml').write_text(compose, encoding='utf-8')
        cmd = ['docker', 'compose', '--project-directory', str(deploy), '-p', project,
               '-f', str(deploy / 'compose.yaml')]

        def call(args, data=None, timeout=120):
            return run(cmd + args, data, timeout, env)

        def sql(text):
            return call(['exec', '-T', '-e', 'PGPASSWORD', 'postgres', 'psql', '-X', '-qAt',
                         '-U', 'codex_proxy', '-d', 'codex_proxy', '-v', 'ON_ERROR_STOP=1'], text)

        def inspect():
            return json.loads(sql(scripts['inspect']))

        def unknown_after_commit(role):
            original = subprocess.run
            calls = []
            def lost_ack(*args, **kwargs):
                process = original(*args, **kwargs)
                require(process.returncode == 0, 'fault_injection_requires_successful_real_sql')
                calls.append(process)
                # 真实服务器动作完成后丢弃回执；消费者须记 unknown，再仅用 inspect 收敛
                raise subprocess.TimeoutExpired('isolated_acknowledgement_loss', 30)
            pg = call(['ps', '-q', 'postgres'])
            with patch.object(schema53.subprocess, 'run', side_effect=lost_ack):
                try:
                    schema53.execute(schema53.BUNDLE, binding, role, pg, 30)
                except RuntimeError as error:
                    require(str(error) == 'schema53_' + role + '_result_unknown_no_reissue',
                            'lost_ack_must_be_unknown')
                else:
                    raise RuntimeError('lost_ack_was_not_classified_unknown')
            require(len(calls) == 1, 'unknown_action_reissued')
            observed = inspect()
            schema53.validate_observation(observed, 52, True)
            return observed

        def snapshot():
            return json.loads(sql(preservation.FACTS_SQL))

        def preserve_before():
            preservation.preserve(before, snapshot())
            require(sql("select jsonb_build_array(id,client_api_key_ref,attempt_count,upstream_send_state,input_tokens,output_tokens,diagnostic_trace_json) from model_requests where id='req_task40_preserved';") == request_before,
                    'request_identity_usage_trace_drift')

        def app_id():
            return call(['ps', '-a', '-q', 'codex-proxy-rs'])

        def healthy(current):
            until = time.monotonic() + 180
            while time.monotonic() < until:
                try:
                    result = run(['docker', 'exec', current, 'curl', '-fsS', '-o', '/dev/null', '-w', '%{http_code}',
                                  'http://127.0.0.1:8080/healthz'], env=env)
                    status = run(['docker', 'inspect', '--format', '{{.State.Health.Status}}', current], env=env)
                    if result == '204' and status == 'healthy':
                        return current
                except RuntimeError:
                    pass
                time.sleep(2)
            raise RuntimeError('isolated_runtime_unhealthy')

        def start(image):
            env['CPR_IMAGE'] = image
            call(['up', '-d', '--no-deps', '--no-build', '--pull', 'never', '--force-recreate', 'codex-proxy-rs'])
            return healthy(app_id())

        def stopped_recovery(current, label):
            call(['stop', '--timeout', '75', 'codex-proxy-rs'])
            full = run(['docker', 'inspect', '--format', '{{.Id}}', current], env=env)
            pg = cmd + ['exec', '-T', '-e', 'PGPASSWORD', 'postgres', 'psql', '-U', 'codex_proxy', '-d', 'codex_proxy']
            helper = ['docker', 'run', '--rm', '-i', '--network', project + '_default', '--mount',
                      'type=bind,src=' + str(deploy / 'config.yaml') + ',dst=/app/deploy/config.yaml,readonly',
                      '-e', 'CPR_REDIS_URL=redis://redis:6379/',
                      '-e', 'CPR_DATABASE_URL=postgres://codex_proxy@must-not-connect.invalid:5432/codex_proxy',
                      HELPER_IMAGE, '/app/bin/codex-proxy-rs', 'site-recovery-redis']
            value = json.loads(run([os.sys.executable, str(ROOT / 'deploy/site-recovery.py'), '--old-container', full,
                                   '--pg-command-json', json.dumps(pg), '--redis-helper-command-json', json.dumps(helper),
                                   '--receipt', str(directory / (label + '-recovery.json')), '--ack-single-deployment', '--apply'],
                                  timeout=1800, env=env))
            require(value['phase'] == 'complete', 'same_generation_recovery_incomplete')
            return value

        try:
            call(['up', '-d', '--no-build', '--wait', 'postgres', 'redis'], timeout=180)
            old = start(OLD_IMAGE)
            initial = inspect()
            schema53.validate_observation(initial, 52)
            sql("""insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,
              credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at)
              values('acct_task40','openai','isolated disabled account','oauth','{}'::jsonb,1,false,false,'ready',now(),now(),now());
              insert into client_api_keys(id,name,key,enabled,created_at,updated_at)
              values('key_task40','isolated disabled key','synthetic-task40-key',false,now(),now());
              insert into account_groups(id,name,color,enabled,created_at,updated_at)
              values('grp_00000000000000000000000000000040','isolated group','#2563EBFF',false,now(),now());
              insert into account_group_accounts(account_group_id,provider_account_id,created_at) values('grp_00000000000000000000000000000040','acct_task40',now());
              insert into client_api_key_groups(client_api_key_id,account_group_id,created_at) values('key_task40','grp_00000000000000000000000000000040',now());
              insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,
              provider_account_ref,upstream_transport,upstream_send_state,outcome,attempt_count,started_at,
              completed_at,input_tokens,output_tokens,diagnostic_trace_json,request_observation_json)
              select 'req_task40_preserved','key_task40','responses','http_sse','openai','acct_task40',
              'websocket','sent','succeeded',1,now()-interval '1 minute',now(),123,456,'{}'::jsonb,
              '{"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]}}'::jsonb;""")
            before = snapshot()
            request_before = sql("select jsonb_build_array(id,client_api_key_ref,attempt_count,upstream_send_state,input_tokens,output_tokens,diagnostic_trace_json) from model_requests where id='req_task40_preserved';")
            prepared = unknown_after_commit('prepare')
            schema53.validate_observation(prepared, 52, True)
            require(app_id() == old, 'online_prepare_recreated_old_runtime')
            healthy(old)
            preserve_before()
            checks.append({'phase': 'online_prepared52', 'schema': prepared, 'preservation': snapshot()})
            old_recovery = stopped_recovery(old, 'old')
            candidate_id = start(candidate)
            upgraded = inspect()
            schema53.validate_observation(upgraded, 53, True)
            preserve_before()
            checks.append({'phase': 'candidate_healthy53', 'schema': upgraded, 'oldRecovery': old_recovery})
            # 留下一条未过期在飞请求，固定旧 helper 须在撤53之前完成真实恢复
            sql("""insert into model_requests(id,client_api_key_ref,operation,client_transport,outcome,started_at,request_observation_json)
              values('req_task40_running','key_task40','responses','http_sse','running',now(),
              '{"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]}}'::jsonb);
              insert into model_request_leases values('req_task40_running',now()+interval '1 hour');""")
            def redis(*args):
                return call(['exec', '-T', '-e', 'REDISCLI_AUTH', 'redis', 'redis-cli', '--no-auth-warning', '--raw', *args])
            tag = hashlib.sha256(b'key_task40').hexdigest()
            client_active = 'codex-proxy-rs:client:{' + tag + '}:active'
            account_tag = hashlib.sha256(b'account_task40').hexdigest()
            for scope in ('account', 'account-reserved'):
                key = 'codex-proxy-rs:lease:' + scope + ':{' + account_tag + '}:active'
                redis('ZADD', key, '9000000000000', 'owner|task40|1')
                redis('HSET', key + ':detail:task40', 'facts', json.dumps({
                    'request_id': 'req_task40_running', 'client_key_id': 'key_task40'}))
            redis('ZADD', client_active, '9000000000000', 'req_task40_running', '9000000000000', 'req_foreign')
            candidate_recovery = stopped_recovery(candidate_id, 'candidate')
            require(candidate_recovery == {'phase': 'complete', 'requests': 1, 'accountSlots': 2, 'clientSlots': 1},
                    'schema53_candidate_request_or_slots_not_recovered')
            require(redis('ZRANGE', client_active, '0', '-1') == 'req_foreign', 'schema53_foreign_slot_drift')
            schema53.validate_observation(inspect(), 53, True)
            require(sql("select outcome from model_requests where id='req_task40_running';") == 'incomplete',
                    'schema53_request_recovery_missing')
            ledger53 = sql("select jsonb_agg(jsonb_build_array(version,success,encode(checksum,'hex')) order by version) from _sqlx_migrations;")
            exited_at = run(['docker', 'inspect', '--format', '{{.State.StartedAt}}', candidate_id], env=env)
            # 与已验证 watchdog 相同的长期路径：固定 helper 恢复后仅 start 原容器，53 登记不撤销
            run(['docker', 'start', candidate_id], env=env)
            healthy(candidate_id)
            require(app_id() == candidate_id and
                    run(['docker', 'inspect', '--format', '{{.State.StartedAt}}', candidate_id], env=env) != exited_at,
                    'schema53_bound_container_restart_unproven')
            schema53.validate_observation(inspect(), 53, True)
            require(sql("select jsonb_agg(jsonb_build_array(version,success,encode(checksum,'hex')) order by version) from _sqlx_migrations;") == ledger53,
                    'schema53_restart_ledger_drift')
            preserve_before()
            restarted_recovery = stopped_recovery(candidate_id, 'candidate-restarted')
            checks.append({'phase': 'schema53_same_container_recovered_and_restarted',
                           'container': candidate_id, 'recovery': candidate_recovery,
                           'nextGenerationRecovery': restarted_recovery, 'ledgerUnchanged': True})
            reverted = unknown_after_commit('rollback')
            schema53.validate_observation(reverted, 52, True)
            restored = start(OLD_IMAGE)
            schema53.validate_observation(inspect(), 52, True)
            preserve_before()
            checks.append({'phase': 'old_healthy52', 'schema': reverted, 'candidateRecovery': candidate_recovery,
                           'same53Helper': HELPER_IMAGE, 'restoredContainer': restored})
            result = {'phase': 'passed', 'candidateImage': candidate, 'candidateSource': source,
                      'oldImage': OLD_IMAGE, 'oldSource': OLD_SOURCE, 'helperSource': HELPER_SOURCE,
                      'imageIdentities': identities, 'checks': checks, 'providerProbes': 0,
                      'unknownResultRecovery': {'roles': ['prepare', 'rollback'], 'executionsPerRole': 1,
                          'fault': 'hide acknowledgement after successful real SQL; inspect only, no reissue'}}
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(result, indent=2), encoding='utf-8')
            print(json.dumps(result, indent=2))
        finally:
            call(['down', '--volumes', '--remove-orphans'], timeout=180)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--image', required=True)
    parser.add_argument('--source', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        verify(args.image, args.source, args.output)
    except Exception as error:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps({'phase': 'failed', 'candidateImage': args.image,
                                         'candidateSource': args.source, 'errorType': type(error).__name__,
                                         'errorCode': str(error) if isinstance(error, RuntimeError) else 'isolated_check_error'},
                                        indent=2), encoding='utf-8')
        raise


if __name__ == '__main__':
    main()
