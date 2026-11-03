#!/usr/bin/env python3
"""Task40 单次部署：在线准备、52/53 切换及受检恢复，消费父签 manifest"""
import argparse
import datetime as dt
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time
import urllib.request

try:
    import fcntl
except ImportError:
    fcntl = None  # Windows 上仅运行纯函数与模拟测试；远端执行必需 Linux fcntl

BASE = Path('/opt/codex-proxy-rs')
SERVICE = 'codex-proxy-rs'
# 故障恢复例外只属于已核实的这一个退出代际；普通健康升级从 manifest 绑定基线
DISK_INCIDENT = {
    'source': '42794577b0205797b4a6d66803deb11f944f2599',
    'image': 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:35c02f6a36d5dd2b202186e6b1c424f7b6b0d31ff47a21fd2b5b26df575bf8cb',
    'containerId': '55c0a01038589e7612b0b63646eb18a13af1d5325591755480414fc15fbf7b58',
    'startedAt': '2026-10-27T14:38:45.88687926Z',
}
IMAGE_RE = r'ghcr\.io/x-t-e-r/codex-proxy-rs@sha256:[0-9a-f]{64}'
PG = '1Panel-postgresql-5AJX'
HELPER_SOURCE = '476d24c16764a0a535fabd975a3df18d2192df0a'
HELPER_IMAGE = 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:f8c705a4da6687ea39902bb09e020772a4083d2dae12c1d0cd507952bf88944e'
RECOVERY_TIMEOUT = 1800
INCIDENT_WORKERS = (
    'worker:stale_model_request_recovery:postgres', 'worker:request_analytics:postgres',
    'worker:retention:postgres', 'worker:account_freeze_recovery:account-freeze-recovery',
    'worker:reset_credit_automation:admin',
)


def require(ok, code):
    if not ok:
        raise RuntimeError(code)


def now():
    return dt.datetime.now(dt.timezone.utc).isoformat()


def atomic(path, value):
    temp = path.with_name(path.name + '.tmp')
    with temp.open('x') as file:
        json.dump(value, file, indent=2)
        file.flush()
        os.fsync(file.fileno())
    os.replace(temp, path)


def command(args, timeout=120, data=None):
    try:
        p = subprocess.run(args, input=data, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError('command_result_unknown') from None
    require(p.returncode == 0 and len(p.stdout) < 1048576, 'command_result_unknown')
    return p.stdout


def baseline_identity(value):
    require(type(value) is dict and
            re.fullmatch('[0-9a-f]{40}', value.get('source', '')) is not None and
            re.fullmatch(IMAGE_RE, value.get('image', '')) is not None and
            re.fullmatch('[0-9a-f]{64}', value.get('containerId', '')) is not None and
            isinstance(value.get('startedAt'), str), 'exact_baseline_identity_required')
    try:
        stamp = dt.datetime.fromisoformat(value['startedAt'].replace('Z', '+00:00'))
        require(stamp.tzinfo is not None, 'baseline_timestamp_required')
    except ValueError:
        raise RuntimeError('baseline_timestamp_required') from None
    return value


def manifest(m):
    require(type(m) is dict and m.get('accepted') is True and m.get('hardCutAuthorized') is True,
            'parent_accepted_hardcut_required')
    if m.get('schema') == 52:
        require(m.get('noMigrationChanges') is True, 'parent_accepted_same52_hardcut_required')
    else:
        require(m.get('schema') == 53 and m.get('noMigrationChanges') is False and
                m.get('baselineSchema') == 52 and m.get('admissionMode') == 'healthy',
                'parent_accepted_schema53_transition_required')
        import schema53
        schema53.contract(m.get('schema53Contract'))
        require(m.get('source') == m['schema53Contract']['source'], 'schema53_runtime_product_source_mismatch')
    b = baseline_identity(m.get('baseline'))
    require(re.fullmatch('[0-9a-f]{40}', m.get('source', '')) is not None and
            re.fullmatch(IMAGE_RE, m.get('image', '')) is not None and
            m['source'] != b['source'] and m['image'] != b['image'], 'new_exact_source_image_required')
    require(isinstance(m.get('migrationEvidence'), str) and m['migrationEvidence'].strip() and
            isinstance(m.get('acceptanceEvidence'), str) and m['acceptanceEvidence'].strip(),
            'parent_evidence_required')
    mode = m.get('admissionMode')
    require(mode in ('healthy', 'disk_logging_recovery'), 'explicit_admission_mode_required')
    if mode == 'disk_logging_recovery':
        evidence = m.get('degradedEvidence', {})
        require(b == DISK_INCIDENT and evidence.get('noCurrentComponentSnapshot') is True and
                evidence.get('boundedObservationRiskAccepted') is True and
                evidence.get('postgresSelect') is True and evidence.get('redisScan') is True and
                set(evidence.get('recoveredComponents', {})) == set(INCIDENT_WORKERS) | {'probe:postgres'} and
                isinstance(evidence.get('logWindow'), dict) and
                isinstance(evidence.get('evidenceRef'), str) and evidence['evidenceRef'].strip() and
                isinstance(evidence.get('healthFailures'), list) and evidence['healthFailures'],
                'exact_disk_incident_and_parent_evidence_required')
    return m


def pin(path):
    # 除唯一 CPR_IMAGE 行外，不解释配置，也不在收据中保存配置字节
    lines = path.read_bytes().splitlines(keepends=True)
    match = [i for i, line in enumerate(lines) if line.startswith(b'CPR_IMAGE=')]
    require(len(match) == 1, 'unique_CPR_IMAGE_required')
    index = match[0]
    value = lines[index].split(b'=', 1)[1].strip().strip(b'"\'').decode('ascii')
    require(re.fullmatch(IMAGE_RE, value), 'immutable_image_pin_required')
    return lines, index, value


def opaque_env_digest(path):
    # 仅把其余配置字节作为不可逆完整性输入；不存储、解码或输出秘密
    lines, index, _ = pin(path)
    return hashlib.sha256(b''.join(line for offset, line in enumerate(lines) if offset != index)).hexdigest()


def set_pin(path, old, new):
    lines, index, value = pin(path)
    require(value == old and re.fullmatch(IMAGE_RE, new), 'pin_drift')
    original = b''.join(lines)
    stat = path.stat()
    ending = b'\r\n' if lines[index].endswith(b'\r\n') else b'\n'
    lines[index] = b'CPR_IMAGE=' + new.encode() + ending
    tmp = path.with_name('.env.task40-pin.tmp')
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, stat.st_mode & 0o777)
    try:
        with os.fdopen(fd, 'wb') as f:
            f.write(b''.join(lines))
            f.flush()
            os.fsync(f.fileno())
        os.chown(tmp, stat.st_uid, stat.st_gid)
        require(path.read_bytes() == original, 'env_changed_during_pin')
        os.replace(tmp, path)
    finally:
        tmp.unlink(missing_ok=True)


FACTS_SQL = """select jsonb_build_object(
 'schemaMax',(select max(version) from public._sqlx_migrations),
 'schemaCount',(select count(*) from public._sqlx_migrations),
 'schemaSuccess',(select bool_and(success) from public._sqlx_migrations),
 'accounts',(select count(*) from public.provider_accounts),
 'keys',(select count(*) from public.client_api_keys),
 'groups',(select count(*) from public.account_groups),
 -- 只投影稳定身份和策略；ID、代理 URL 与凭据值不离开 PostgreSQL
 'accountIdentity',(select md5(coalesce(string_agg(
   jsonb_build_array(id,name,provider_kind,authentication_kind,upstream_user_id,upstream_account_id,
                     enabled,weight,concurrency_limit,model_access_json,outbound_proxy_id)::text,
                     ',' order by id),'')) from public.provider_accounts),
 'keyIdentity',(select md5(coalesce(string_agg(
   jsonb_build_array(id,name,label,encode(client_api_key_sha256(key),'hex'),enabled,max_concurrency,
                     requests_per_minute,daily_limit_usd,weekly_limit_usd,provider_request_profiles_json)::text,
                     ',' order by id),'')) from public.client_api_keys),
 'groupIdentity',(select md5(coalesce(string_agg(
   jsonb_build_array(id,name,enabled,color,fast_mode,allow_image_generation)::text,',' order by id),'')) from public.account_groups),
 'accountMembership',(select md5(coalesce(string_agg(
   jsonb_build_array(account_group_id,provider_account_id)::text,',' order by account_group_id,provider_account_id),''))
   from public.account_group_accounts),
 'keyMembership',(select md5(coalesce(string_agg(
   jsonb_build_array(client_api_key_id,account_group_id)::text,',' order by client_api_key_id,account_group_id),''))
   from public.client_api_key_groups),
 'pluginIdentity',(select md5(coalesce(string_agg(
   jsonb_build_array(id,artifact_sha256,name,enabled,revision,bindings_json,configuration_json)::text,',' order by id),''))
   from public.plugin_instances),
 'pluginGroups',(select md5(coalesce(string_agg(
   jsonb_build_array(instance_id,resource_key,group_id)::text,',' order by instance_id,resource_key),''))
   from public.plugin_account_group_resources),
 'pluginKeys',(select md5(coalesce(string_agg(
   jsonb_build_array(instance_id,resource_key,key_id)::text,',' order by instance_id,resource_key),''))
   from public.plugin_client_api_key_resources),
 'ledgerRows',(select count(*) from public.client_api_key_charge_events),
 'budgetRows',(select count(*) from public.client_api_key_budget_windows),
 'resetRows',(select count(*) from public.codex_reset_operations),
 'running',(select count(*) from public.model_requests where outcome='running'),
 'settings',(select jsonb_build_object('revision',config_revision,
  'cyberSessionBlockEnabled',cyber_session_block_enabled,'cyberSessionBlockTtlSeconds',cyber_session_block_ttl_seconds,
  'wsPoolEnabled',ws_pool_enabled,'wsPoolMaxAgeMs',ws_pool_max_age_ms,'wsPoolMaxConnecting',ws_pool_max_connecting,
  'wsPoolStreamIdleTimeoutMs',ws_pool_stream_idle_timeout_ms,'wsPoolFastPathBudgetMs',ws_pool_fast_path_budget_ms) from public.runtime_settings),
 'settingsPolicy',(select md5(jsonb_build_array(config_revision,rotation_strategy,max_concurrent_per_account,
   model_mappings_json,provider_request_profiles_json,pricing_overrides_json,smart_scheduling_json)::text)
   from public.runtime_settings),
 'quotaGlobal',(select jsonb_build_object('revision',revision,'policy',policy_json) from public.codex_quota_policy_global where id=1),
 'quotaModes',(select jsonb_object_agg(mode,n) from (select mode,count(*) n from public.codex_quota_policy_accounts group by mode) x),
 'quotaAccountPolicy',(select md5(coalesce(string_agg(
   jsonb_build_array(account_id,revision,mode,policy_json)::text,',' order by account_id),''))
   from public.codex_quota_policy_accounts),
 'proxyPolicy',(select md5(coalesce(string_agg(
   jsonb_build_array(id,revision,proxy_url)::text,',' order by id),'')) from public.outbound_proxies),
 'priority',(select jsonb_build_object('mode',mode,'autoOffOnQuotaReset',auto_off_on_quota_reset) from public.codex_priority_global where id=1));"""


def sql(query):
    return command(['docker', 'exec', '-i', '-u', 'postgres', '-e',
                    'PGOPTIONS=-c statement_timeout=5000 -c lock_timeout=1000 -c default_transaction_read_only=on',
                    PG, 'sh', '-c', 'exec psql -X -w -qAt -U "$POSTGRES_USER" -d codex_proxy -v ON_ERROR_STOP=1 -v VERBOSITY=sqlstate'],
                   timeout=15, data=query.encode()).decode().strip()


def facts():
    return json.loads(sql(FACTS_SQL))


def require_schema(value, version):
    require((value['schemaMax'], value['schemaCount'], value['schemaSuccess']) ==
            (version, version, True), 'schema' + str(version) + '_required')
    return value


def preserve(before, after):
    for key in ('settings', 'settingsPolicy', 'quotaGlobal', 'quotaModes', 'quotaAccountPolicy', 'proxyPolicy',
                'priority', 'accounts', 'keys', 'groups', 'budgetRows', 'accountIdentity', 'keyIdentity',
                'groupIdentity', 'accountMembership', 'keyMembership', 'pluginIdentity', 'pluginGroups', 'pluginKeys'):
        require(key in before and key in after and before[key] == after[key], 'preservation_drift_' + key)
    for key in ('ledgerRows', 'resetRows'):
        require(after[key] >= before[key], 'preservation_decreased_' + key)


def container():
    fmt = '{"id":{{json .Id}},"imageId":{{json .Image}},"pid":{{.State.Pid}},"running":{{.State.Running}},"startedAt":{{json .State.StartedAt}},"health":{{json .State.Health.Status}},"revision":{{json (index .Config.Labels "org.opencontainers.image.revision")}},"composeFiles":{{json (index .Config.Labels "com.docker.compose.project.config_files")}},"workingDir":{{json (index .Config.Labels "com.docker.compose.project.working_dir")}},"project":{{json (index .Config.Labels "com.docker.compose.project")}},"service":{{json (index .Config.Labels "com.docker.compose.service")}}}'
    return json.loads(command(['docker', 'inspect', '--format', fmt, SERVICE]))


def image(value, source):
    fmt = '{"id":{{json .Id}},"os":{{json .Os}},"arch":{{json .Architecture}},"revision":{{with .Config}}{{with .Labels}}{{json (index . "org.opencontainers.image.revision")}}{{else}}null{{end}}{{else}}null{{end}}}'
    result = json.loads(command(['docker', 'image', 'inspect', '--format', fmt, value]))
    require(result.get('os') == 'linux' and result.get('arch') == 'amd64' and result.get('revision') == source and result.get('id'), 'image_identity_mismatch')
    digests = json.loads(command(['docker', 'image', 'inspect', '--format', '{{json .RepoDigests}}', value]))
    require(isinstance(digests, list) and value in digests, 'registry_digest_not_bound')
    return result


def health_status():
    try:
        with urllib.request.urlopen('http://127.0.0.1:8080/healthz', timeout=3) as response:
            return response.status
    except urllib.error.HTTPError as error:
        error.close()
        return error.code
    except OSError:
        return None


def health():
    return health_status() == 204


def timestamp(value):
    return dt.datetime.fromisoformat(value.replace('Z', '+00:00'))


def bound_runtime(old, b):
    require(old['project'] == SERVICE and old['service'] == SERVICE and old['running'] and
            old['id'] == b['containerId'] and old['startedAt'] == b['startedAt'] and
            old['revision'] == b['source'], 'baseline_runtime_drift')


HEALTH_FAILURES_SQL = """select coalesce(jsonb_agg(x order by at,component),'[]') from (
 select created_at as at,message::jsonb->>'correlationId' as component,
        error_details::jsonb#>>'{causes,messages,0}' as reason
 from public.ops_events where model_request_id is null and component='health'
 and created_at>='2026-10-27T14:38:45.88687926Z' limit 65) x;"""


def incident_failures():
    rows = json.loads(sql(HEALTH_FAILURES_SQL))
    require(isinstance(rows, list) and len(rows) == 10 and
            max(timestamp(row['at']) for row in rows) == timestamp('2026-10-31T19:34:31.095157+00:00'),
            'incident_health_history_new_or_missing')
    for row in rows:
        component, reason = row.get('component', ''), row.get('reason', '')
        allowed = ((component == 'probe:file_logging' and re.fullmatch(
            r'probe:file_logging is unhealthy: log completeness compromised: [1-9][0-9]* write/flush failures, 0 rejected records \(0 warnings/errors\) since startup', reason)) or
            (component == 'probe:postgres' and reason == 'probe:postgres is unhealthy: PostgreSQL connection acquisition timed out') or
            (component == 'probe:redis' and reason == 'probe:redis is unhealthy: Redis is unavailable') or
            (component in INCIDENT_WORKERS and re.fullmatch(
                re.escape(component) + r' is unhealthy: state=(BackingOff|Running); last_error=worker lease acquisition failed', reason)))
        require(allowed, 'unexplained_health_failure')
    require({row['component'] for row in rows} == set(INCIDENT_WORKERS) |
            {'probe:file_logging', 'probe:postgres', 'probe:redis'}, 'incident_component_history_incomplete')
    return rows


def incident_log_window(since, sampled):
    # 只投影健康转移，正文与其它日志字段不进入收据；缺文件、坏行、超预算均停止
    root = BASE / '.runtime/logs'
    files = []
    for path in root.glob('codex-proxy-rs-application.*.log*'):
        match = re.fullmatch(r'codex-proxy-rs-application\.(\d{4}-\d{2}-\d{2})(?:\.(\d+))?\.log(?:\.gz)?', path.name)
        if match:
            files.append(((match[1], int(match[2] or 0)), path))
    events, spans, consumed = [], [], 0
    for _, path in sorted(files, reverse=True):
        first = last = None
        opener = gzip.open if path.suffix == '.gz' else open
        with opener(path, 'rb') as stream:
            for line in iter(lambda: stream.readline(2 * 1024**2 + 1), b''):
                consumed += len(line)
                require(consumed <= 128 * 1024**2 and len(line) <= 2 * 1024**2, 'incident_logs_over_bound')
                try:
                    row = json.loads(line)
                    at = timestamp(row['timestamp'])
                except (ValueError, KeyError):
                    raise RuntimeError('incident_log_gap_or_invalid') from None
                first = at if first is None else min(first, at)
                last = at if last is None else max(last, at)
                if row.get('target') != 'gateway_api::health':
                    continue
                fields = row.get('fields', {})
                event = fields.get('message')
                require(at < since or event != 'health diagnostic could not be recorded',
                        'incident_diagnostic_queue_gap')
                if at >= since and event in ('health check failed', 'health component recovered'):
                    require(isinstance(fields.get('component'), str), 'incident_log_component_missing')
                    events.append({'at': row['timestamp'], 'component': fields['component'], 'event': event})
        require(first is not None, 'incident_log_empty')
        spans.append({'file': path.name, 'first': first.isoformat(), 'last': last.isoformat()})
        if first <= since:
            break
    require(spans and timestamp(spans[-1]['first']) <= since and
            timestamp(spans[0]['last']) >= sampled - dt.timedelta(seconds=15), 'incident_log_window_incomplete')
    for newer, older in zip(spans, spans[1:]):
        names = [re.fullmatch(r'codex-proxy-rs-application\.(\d{4}-\d{2}-\d{2})(?:\.(\d+))?\.log(?:\.gz)?',
                              row['file']) for row in (newer, older)]
        require(names[0][1] == names[1][1] and int(names[0][2] or 0) == int(names[1][2] or 0) + 1 and
                timestamp(newer['first']) - timestamp(older['last']) <= dt.timedelta(seconds=60),
                'incident_log_rotation_gap')
    return {'files': list(reversed(spans)), 'events': sorted(events, key=lambda row: timestamp(row['at'])),
            'readBytes': consumed, 'sampledAt': sampled.isoformat()}


def disk_recovery_evidence(old, expected=None):
    bound_runtime(old, DISK_INCIDENT)
    require(old['health'] == 'unhealthy' and health_status() == 503, 'incident_current_503_required')
    require(shutil.disk_usage(BASE).free >= 2 * 1024**3, 'disk_space_low')
    sampled = dt.datetime.now(dt.timezone.utc)
    rows = incident_failures()
    if expected is not None:
        require(rows == expected['healthFailures'], 'incident_health_history_changed')
    # 从最后一批故障前一分钟开始，必须完整覆盖该批故障和之后的全部健康转移
    since = max(timestamp(row['at']) for row in rows) - dt.timedelta(minutes=1)
    window = incident_log_window(since, sampled)
    for row in rows:
        if timestamp(row['at']) >= since:
            require(any(event['event'] == 'health check failed' and event['component'] == row['component'] and
                        abs((timestamp(event['at']) - timestamp(row['at'])).total_seconds()) < .01
                        for event in window['events']), 'incident_failure_log_missing')
    last = {row['component']: row for row in rows}
    for event in window['events']:
        if event['event'] == 'health check failed':
            require(any(event['component'] == row['component'] and
                        abs((timestamp(event['at']) - timestamp(row['at'])).total_seconds()) < .01
                        for row in rows), 'incident_new_or_unrecorded_failure')
        last[event['component']] = event
    for component, event in last.items():
        if component in ('probe:file_logging', 'probe:redis'):
            # Redis 由下方同配置的即时只读操作验证；其旧失败早于保留窗口
            continue
        require(event.get('event') == 'health component recovered' and
                timestamp(event['at']) > max(timestamp(row['at']) for row in rows if row['component'] == component),
                'incident_component_not_recovered_' + component)
    if expected is not None:
        require({component: last[component]['at'] for component in (*INCIDENT_WORKERS, 'probe:postgres')} ==
                expected['recoveredComponents'], 'incident_recovery_evidence_changed')
    require(sql('select 1;') == '1', 'incident_postgres_unavailable')
    # 现役二进制正常认证配置的 typed SCAN，无服务启动、PG初始化或 Redis 写入
    scan = json.loads(command(['docker', 'exec', '-i', old['id'], '/app/bin/codex-proxy-rs',
                              'site-recovery-redis'], timeout=15,
                             data=b'{"kind":"scan","cursor":0,"reserved":false}'))
    require(isinstance(scan, list) and len(scan) == 2 and isinstance(scan[1], list), 'incident_redis_unavailable')
    current = container()
    bound_runtime(current, DISK_INCIDENT)
    require(current['imageId'] == old['imageId'], 'incident_runtime_changed')
    return {'healthFailures': rows, 'logWindow': window, 'postgresSelect': True, 'redisScan': True,
            'recoveredComponents': {component: last[component]['at'] for component in
                                    (*INCIDENT_WORKERS, 'probe:postgres')},
            'noCurrentComponentSnapshot': True, 'capturedAt': now(), 'healthz': 503}


def configured_service_image(raw):
    # 只返回目标服务已解析的镜像，不把含其它配置与凭据的 Compose JSON 写入收据
    config = json.loads(raw)
    services = config.get('services')
    require(isinstance(services, dict) and isinstance(services.get(SERVICE), dict) and
            isinstance(services[SERVICE].get('image'), str), 'compose_target_service_image_missing')
    return services[SERVICE]['image']


def gate_enter(root, baseline):
    gate = BASE / '.runtime/restart-recovery'
    require((gate / 'binding.json').is_file(), 'restart_gate_required')
    lock = (gate / 'lifecycle.lock').open('a')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(not (gate / 'maintenance').exists(), 'restart_gate_maintenance_occupied')
        binding = json.loads((gate / 'binding.json').read_text())
        require(binding.get('runtime', {}).get('revision') == baseline['source'] and
                binding['runtime'].get('id') == baseline['containerId'], 'restart_gate_source_drift')
        return gate, lock, binding
    except BaseException:
        lock.close()
        raise


class Deployment:
    def __init__(self, root):
        self.root = Path(root)
        require(re.fullmatch(r'task40-[0-9a-f]{32}', self.root.name) and
                self.root.parent == BASE / '.runtime/deployments', 'run_path_invalid')
        self.m = manifest(json.loads((self.root / 'accepted.json').read_text()))
        self.b = self.m['baseline']
        self.result = dict(runId=self.root.name, phase='admitting', success=False, rollback=False,
                           startedAt=now(), providerProbes=0, channelMutations=0, schemaWrites=0)
        self.stopped = False
        self.pin_changed = False
        self.old_recovered = False
        self.recovery_pending = False

    def save(self, phase):
        self.result['phase'] = phase
        atomic(self.root / getattr(self, 'progress_name', 'progress.json'), self.result)

    def schema_observation(self, version, pending_required=False):
        value = require_schema(facts(), version)
        if self.m['schema'] == 53:
            import schema53
            observed = schema53.inspect(self.root / 'schema53-artifacts', self.m['schema53Contract'], sql)
            schema53.validate_observation(observed, version, pending_required)
            self.result.setdefault('schemaObservations', []).append({'at': now(), 'facts': {
                key: value[key] for key in ('schemaMax', 'schemaCount', 'schemaSuccess')}, 'product': observed})
        return value

    def schema_command(self, role, attempt=None):
        import schema53
        key = attempt or role
        commands = self.result.setdefault('schemaCommands', {})
        require(key not in commands, 'schema53_' + key + '_already_claimed_no_reissue')
        commands[key] = {'role': role, 'outcome': 'intent', 'issuedAt': now()}
        self.result['schemaWrites'] += 1
        self.save('schema53_' + key + '_intent')
        try:
            outcome = schema53.execute(self.root / 'schema53-artifacts', self.m['schema53Contract'],
                                       role, PG, 2100 if role == 'prepare' else 30)
        except Exception:
            outcome = {'outcome': 'unknown'}
        commands[key].update(outcome)
        if outcome['outcome'] != 'completed':
            try:
                commands[key]['observedAfter'] = schema53.inspect(
                    self.root / 'schema53-artifacts', self.m['schema53Contract'], sql)
            except Exception:
                commands[key]['observationError'] = 'schema53_read_only_inspection_unavailable'
        self.save('schema53_' + key + '_' + outcome['outcome'])
        require(outcome['outcome'] != 'unknown', 'schema53_' + role + '_result_unknown_no_reissue')
        require(outcome['outcome'] == 'completed', 'schema53_' + role + '_failed_inspect_before_new_GO')

    def prepare(self, cleanup=False):
        require(self.m['schema'] == 53, 'schema53_prepare_only')
        role = 'cleanup' if cleanup else 'prepare'
        with (self.root / ('schema53-' + role + '-claimed.json')).open('x') as claim:
            json.dump({'at': now(), 'pid': os.getpid(), 'singleAttempt': True}, claim)
            claim.flush()
            os.fsync(claim.fileno())
        self.progress_name = role + '-progress.json'
        lock = None
        try:
            # 慢准备不持生命周期锁、不写维护闸，旧进程继续提供业务
            self.gate, lock, self.binding = gate_enter(self.root, self.b)
            self.preflight(self.binding, preparation=True)
            lock.close()
            lock = None
            self.schema_observation(52)
            self.owned(self.old, self.b['image'], True)
            self.schema_command(role)
            self.schema_observation(52, pending_required=not cleanup)
            self.owned(self.old, self.b['image'], True)
            self.gate, lock, binding = gate_enter(self.root, self.b)
            require(binding == self.binding, 'schema53_prepare_binding_drift')
            preserve(self.before, self.schema_observation(52, pending_required=not cleanup))
            self.result.update(phase='schema53_cleaned' if cleanup else 'schema53_prepared',
                               healthzObserved=health_status())
        except Exception as error:
            self.result['phase'] = 'schema53_preparation_requires_operator'
            self.result['errorCode'] = str(error) if isinstance(error, RuntimeError) else 'schema53_prepare_error_suppressed'
        finally:
            if lock:
                lock.close()
            self.result['finishedAt'] = now()
            atomic(self.root / (role + '-result.json'), self.result)
        return self.result

    def reconcile_preparation(self):
        import schema53
        require(self.m['schema'] == 53 and not (self.root / 'attempt-claimed.json').exists(),
                'schema53_preparation_only_before_activation')
        # 持全局执行锁时调用；数据库侧原 psql 也必须确定退出
        schema53.require_quiescent(self.root, self.m['schema53Contract'], sql)
        gate, lock, binding = gate_enter(self.root, self.b)
        try:
            self.gate, self.binding = gate, binding
            self.progress_name = 'prepare-observed-progress.json'
            self.preflight(binding, preparation=True)
            value = self.schema_observation(52, pending_required=True)
            self.owned(self.old, self.b['image'], True)
            result = {'phase': 'schema53_prepared', 'at': now(), 'runId': self.root.name,
                      'runtime': self.old, 'facts': value, 'healthzObserved': health_status(),
                      'origin': 'read_only_reconciliation'}
            atomic(self.root / 'prepare-observed.json', result)
            return result
        finally:
            lock.close()

    def preparation_admission(self):
        if self.m['schema'] == 52:
            return
        import schema53
        schema53.require_quiescent(self.root, self.m['schema53Contract'], sql)
        path = self.root / 'prepare-observed.json'
        if not path.exists():
            path = self.root / 'prepare-result.json'
        prepared = json.loads(path.read_text())
        require(prepared.get('phase') == 'schema53_prepared' and prepared.get('runId') == self.root.name,
                'schema53_online_preparation_required')
        self.result['preparationReceipt'] = path.name
        self.result['schema53Contract'] = self.m['schema53Contract']
        self.result['targetSchema'] = 53

    def rollback_schema(self):
        if self.m['schema'] == 52:
            return
        current = container()
        require(not current['running'] and
                (self.gate / 'maintenance').read_text() == self.root.name,
                'schema53_rollback_requires_stopped_owned_generation')
        import schema53
        observed = schema53.require_quiescent(self.root, self.m['schema53Contract'], sql)
        # 产品脚本负责逐条校验迁移身份；消费者不猜可删除的记录
        prior = self.result.get('schemaCommands', {}).get('rollback')
        if prior is None:
            self.schema_command('rollback')
        elif (prior.get('outcome') == 'failed' and getattr(self, 'operator_takeover', False) and
              observed.get('schema_state') == 'schema53'):
            # 新恢复 GO 只给已知失败一条独立接管声明；未知动作仅能观察其结果
            self.schema_observation(53, pending_required=True)
            self.schema_command('rollback', attempt='rollback_takeover')
        self.schema_observation(52, pending_required=True)
        self.result['schemaRollback'] = {'phase': 'verified52', 'at': now()}
        self.save('schema53_rollback_verified52')

    def compose(self, args):
        cmd = ['docker', 'compose', '--project-directory', self.workdir, '--env-file', str(BASE / '.env'), '-p', SERVICE]
        for path in self.files:
            cmd += ['-f', path]
        return command(cmd + args, timeout=180)

    def effective_image(self):
        return configured_service_image(self.compose(['config', '--format', 'json']))

    def stamps(self):
        return {path: [Path(path).stat().st_size, Path(path).stat().st_mtime_ns] for path in
                self.files + [str(BASE / 'deploy/config.yaml'), str(BASE / '.env')]}

    def refresh_env_stamp(self):
        value = (BASE / '.env').stat()
        self.file_stamps[str(BASE / '.env')] = [value.st_size, value.st_mtime_ns]
        self.result['admittedFileStamps'] = self.file_stamps

    def owned(self, expected, expected_pin, running=None):
        current = container()
        require(all(current[k] == expected[k] for k in ('id', 'imageId', 'revision', 'startedAt')), 'container_owner_drift')
        if running is not None:
            require(current['running'] is running, 'container_run_state_drift')
        require(pin(BASE / '.env')[2] == expected_pin and self.stamps() == self.file_stamps, 'configuration_drift')
        return current

    def admit_baseline_health(self, old):
        if self.m['admissionMode'] == 'healthy':
            require(old['health'] == 'healthy' and health(), 'baseline_runtime_drift')
            return {'healthz': 204, 'mode': 'healthy'}
        evidence = disk_recovery_evidence(old, self.m['degradedEvidence'])
        return dict(evidence, mode='disk_logging_recovery')

    def preflight(self, binding, preparation=False):
        old = container()
        bound_runtime(old, self.b)
        require(binding['runtime'] == {k: old[k] for k in ('id', 'project', 'service', 'revision')}, 'restart_binding_runtime_drift')
        require(binding.get('singleDeployment') is True and binding.get('helperSource') == HELPER_SOURCE and
                binding.get('helperImage') == HELPER_IMAGE and
                isinstance(binding.get('recoveryCommand'), list) and
                binding['recoveryCommand'][:2] == ['python3', str(BASE / '.runtime/restart-recovery/site-recovery.py')],
                'recovery_helper_binding_drift')
        require(command(['systemctl', 'is-enabled', 'cpr-restart-recovery.timer']).decode().strip() == 'enabled' and
                command(['systemctl', 'is-active', 'cpr-restart-recovery.timer']).decode().strip() == 'active',
                'recovery_watchdog_not_active')
        require(command(['docker', 'inspect', '--format', '{{json .HostConfig.RestartPolicy.Name}}', SERVICE]).decode().strip().strip('"') == 'no', 'restart_policy_not_no')
        require(old['imageId'] == image(self.b['image'], self.b['source'])['id'], 'baseline_image_drift')
        target = image(self.m['image'], self.m['source'])
        self.old, self.target = old, target
        self.files = old['composeFiles'].split(',')
        self.workdir = old['workingDir']
        require(self.workdir and Path(self.workdir).resolve().is_relative_to(BASE) and self.files and
                all(Path(path).resolve().is_relative_to(BASE) for path in self.files) and
                not any('override' in Path(path).name for path in self.files), 'compose_scope_drift')
        require(pin(BASE / '.env')[2] == self.b['image'] and self.effective_image() == self.b['image'],
                'target_service_image_or_pin_drift')
        require(shutil.disk_usage(BASE).free >= 2 * 1024**3, 'disk_space_low')
        self.result['baselineHealthAdmission'] = ({'mode': 'online_preparation', 'healthz': health_status()}
                                                if preparation else self.admit_baseline_health(old))
        self.before = self.schema_observation(52, pending_required=not preparation)
        self.file_stamps = self.stamps()
        self.result.update(beforeRuntime=old, beforeFacts=self.before, targetImage=target,
                           admittedFileStamps=self.file_stamps, envOpaqueDigest=opaque_env_digest(BASE / '.env'),
                           envOriginalDigest=hashlib.sha256((BASE / '.env').read_bytes()).hexdigest())
        self.save('ready')

    def activity(self):
        app = container()
        require(app['running'] and app['pid'] > 0, 'old_not_running')
        tcp = command(['nsenter', '-t', str(app['pid']), '-n', 'ss', '-Htn', 'state', 'established', '(', 'sport', '=', ':8080', ')'])
        return {'running': int(sql("select count(*) from public.model_requests where outcome='running';")),
                'tcpEstablished8080': len([line for line in tcp.splitlines() if line.strip()])}

    def stop(self, expected, expected_pin):
        self.owned(expected, expected_pin, True)
        self.stopped = True
        require(expected['id'] not in self.result.setdefault('stopCommands', {}), 'stop_already_claimed_no_reissue')
        self.result['stopCommand'] = {'containerId': expected['id'], 'outcome': 'intent'}
        self.result['stopCommands'][expected['id']] = self.result['stopCommand']
        self.save('stopping')
        try:
            command(['docker', 'stop', '--timeout', '75', expected['id']], timeout=120)
        except Exception:
            self.result['stopCommand']['outcome'] = 'unknown'
            self.save('stop_result_unknown')
            raise RuntimeError('stop_result_unknown_no_retry') from None
        self.result['stopCommand']['outcome'] = 'completed'
        self.save('stopped')
        self.owned(expected, expected_pin, False)

    def recovery_receipt(self, expected):
        path = self.root / ('stopped-recovery-' + expected['id'][:12] + '.json')
        if not path.exists():
            return path, None
        receipt = json.loads(path.read_text())
        require(receipt.get('owner', {}).get('Id') == expected['id'] and
                receipt['owner'].get('StartedAt') == expected['startedAt'] and
                receipt.get('phase') in ('prepared', 'complete'), 'recovery_receipt_owner_drift')
        if receipt['phase'] == 'complete':
            require(isinstance(receipt.get('completed'), list) and isinstance(receipt.get('released'), dict),
                    'recovery_receipt_invalid')
        return path, receipt

    def recover(self, expected, expected_pin, binding):
        self.owned(expected, expected_pin, False)
        require(self.gate is not None, 'recovery_gate_required')
        path, receipt = self.recovery_receipt(expected)
        commands = self.result.setdefault('recoveryCommands', {})
        if receipt is None or receipt['phase'] != 'complete':
            if expected['id'] in commands:
                # 原命令结果不明：只观察同代 receipt，绝不重发
                until = time.monotonic() + RECOVERY_TIMEOUT + 300
                while time.monotonic() < until:
                    path, receipt = self.recovery_receipt(expected)
                    if receipt is not None and receipt['phase'] == 'complete':
                        break
                    time.sleep(5)
                require(receipt is not None and receipt['phase'] == 'complete',
                        'recovery_claimed_wait_exact_receipt_no_reissue')
            else:
                require(receipt is None, 'prepared_receipt_no_reissue')
                commands[expected['id']] = {'outcome': 'intent', 'startedAt': expected['startedAt']}
                self.recovery_pending = True
                self.save('recovering_stopped')
                try:
                    command(binding['recoveryCommand'] + ['--old-container', expected['id'], '--receipt', str(path),
                                                         '--ack-single-deployment', '--apply'], timeout=RECOVERY_TIMEOUT)
                except Exception:
                    commands[expected['id']]['outcome'] = 'unknown'
                    self.save('recovery_result_unknown')
                    raise RuntimeError('recovery_result_unknown_no_retry') from None
                commands[expected['id']]['outcome'] = 'completed'
                path, receipt = self.recovery_receipt(expected)
        require(receipt is not None and receipt['phase'] == 'complete', 'recovery_receipt_incomplete')
        self.owned(expected, expected_pin, False)
        require(int(sql("select count(*) from public.model_requests where outcome='running';")) == 0, 'running_after_recovery')
        self.recovery_pending = False
        seen = self.result.setdefault('stoppedRecovery', [])
        if not any(row['containerId'] == expected['id'] for row in seen):
            seen.append(dict(receipt['released'], containerId=expected['id'],
                             requests=len(receipt['completed']), phase='complete'))
        self.save('recovered_stopped')

    def observed_replacement(self, expected, expected_image, expected_source, target_pin, phase, limit=30):
        until = time.monotonic() + limit
        while time.monotonic() < until:
            try:
                current = container()
                if current['id'] == expected['id'] and current['startedAt'] == expected['startedAt'] and not current['running']:
                    time.sleep(2)
                    continue
                issued = self.result['composeCommands'][phase]['issuedAt']
                require(current['id'] != expected['id'] and current['startedAt'] != expected['startedAt'] and
                        dt.datetime.fromisoformat(current['startedAt'].replace('Z', '+00:00')) >=
                        dt.datetime.fromisoformat(issued) and
                        current['imageId'] == expected_image and current['revision'] == expected_source and
                        current['project'] == SERVICE and current['service'] == SERVICE, 'replacement_identity_unproven')
                require(pin(BASE / '.env')[2] == target_pin and self.stamps() == self.file_stamps,
                        'replacement_config_drift')
                if phase == 'candidate':
                    self.result['candidateRuntime'] = current
                else:
                    self.result['rollbackRuntime'] = current
                self.save(phase + '_created')
                return current
            except RuntimeError as error:
                if str(error) != 'command_result_unknown':
                    raise
            time.sleep(2)
        raise RuntimeError('replacement_observation_unknown_no_reissue')

    def start(self, expected, previous_pin, target_pin, expected_image, expected_source, phase):
        self.owned(expected, previous_pin, False)
        require(phase not in self.result.get('pinCommands', {}) and
                phase not in self.result.get('composeCommands', {}), 'start_already_claimed_no_reissue')
        self.result.setdefault('pinCommands', {})[phase] = {'from': previous_pin, 'to': target_pin, 'outcome': 'intent'}
        self.save('pin_intent_' + phase)
        set_pin(BASE / '.env', previous_pin, target_pin)
        self.result['pinCommands'][phase]['outcome'] = 'completed'
        self.pin_changed = target_pin == self.m['image']
        self.refresh_env_stamp()
        self.save('pin_changed_' + phase)
        require(self.effective_image() == target_pin, 'compose_pin_drift')
        self.owned(expected, target_pin, False)
        self.result.setdefault('composeCommands', {})[phase] = {'issuedAt': now(), 'outcome': 'intent',
            'oldContainerId': expected['id'], 'targetImage': target_pin}
        self.save('starting_' + phase)
        try:
            self.compose(['up', '-d', '--no-deps', '--no-build', '--pull', 'never', '--force-recreate', SERVICE])
        except Exception:
            self.result['composeCommands'][phase]['outcome'] = 'unknown'
            self.save('compose_result_unknown')
            raise RuntimeError('compose_result_unknown_no_retry') from None
        self.result['composeCommands'][phase]['outcome'] = 'completed'
        self.save('compose_acknowledged_' + phase)
        current = self.observed_replacement(expected, expected_image, expected_source, target_pin, phase)
        until = time.monotonic() + 180
        while time.monotonic() < until:
            current = self.owned(current, target_pin, True)
            if current['health'] == 'healthy' and health():
                return current
            time.sleep(2)
        raise RuntimeError('started_container_unhealthy')

    def rollback(self, current):
        self.save('rolling_back_same52')
        require(not self.recovery_pending and self.old_recovered and
                self.result.get('stopCommand', {}).get('outcome') in ('completed', 'observed_stopped'),
                'unsettled_stop_or_recovery_no_reissue')
        previous_pin = self.m['image'] if self.pin_changed else self.b['image']
        if current is not None:
            current = self.owned(current, self.m['image'])
            if current['running']:
                self.stop(current, self.m['image'])
            self.recover(current, self.m['image'], self.binding)
        else:
            self.owned(self.old, previous_pin, False)
        previous = current or self.old
        self.rollback_schema()
        old = self.start(previous, previous_pin, self.b['image'], self.old['imageId'], self.b['source'], 'rollback')
        after = self.schema_observation(52, pending_required=self.m['schema'] == 53)
        preserve(self.before, after)
        self.result.update(rollbackRuntime=old, rollbackHealthz=204, afterFacts=after)
        self.save('rollback_verified_pending_gate')

    def settle_gate(self):
        phase = self.result['phase']
        require(phase in ('candidate_verified_pending_gate', 'rollback_verified_pending_gate',
                          'blocked_unchanged'), 'gate_held_for_operator')
        current = container()
        expected = (self.result.get('afterRuntime') if phase == 'candidate_verified_pending_gate' else
                    self.result.get('rollbackRuntime') if phase == 'rollback_verified_pending_gate' else self.old)
        require(expected is not None, 'gate_expected_runtime_missing')
        require(all(current[k] == expected[k] for k in ('id', 'imageId', 'startedAt')) and
                current['running'], 'gate_runtime_drift')
        if phase == 'blocked_unchanged' and not self.stopped:
            # 未停止的旧代际按原准入复核后释放本 run 闸，不发布部署或回滚成功
            self.admit_baseline_health(current)
        else:
            require(current['health'] == 'healthy' and health(), 'gate_runtime_drift')
        target_pin = self.m['image'] if phase == 'candidate_verified_pending_gate' else self.b['image']
        require(pin(BASE / '.env')[2] == target_pin, 'gate_pin_drift')
        require(self.stamps() == self.file_stamps and
                opaque_env_digest(BASE / '.env') == self.result['envOpaqueDigest'] and
                self.effective_image() == target_pin, 'gate_configuration_or_service_image_drift')
        require((self.gate / 'maintenance').read_text() == self.root.name and
                json.loads((self.gate / 'binding.json').read_text()) == self.binding, 'gate_owner_drift')
        updated = dict(self.binding)
        updated['runtime'] = {k: current[k] for k in ('id', 'project', 'service', 'revision')}
        # 先核健康和完整保全再放行 watchdog
        preserve(self.result['beforeFacts'], self.schema_observation(
            self.m['schema'] if phase == 'candidate_verified_pending_gate' else 52,
            pending_required=self.m['schema'] == 53))
        atomic(self.gate / 'binding.json', updated)
        require(json.loads((self.gate / 'binding.json').read_text()) == updated, 'gate_rebind_not_persisted')
        require(self.stamps() == self.file_stamps and
                opaque_env_digest(BASE / '.env') == self.result['envOpaqueDigest'] and
                self.effective_image() == target_pin and pin(BASE / '.env')[2] == target_pin,
                'gate_configuration_or_service_image_drift')
        (self.gate / 'maintenance').unlink()
        require(not (self.gate / 'maintenance').exists(), 'gate_maintenance_not_released')
        self.result['restartRecoveryGate'] = {'phase': 'rebound', 'runtime': updated['runtime']}
        if phase == 'candidate_verified_pending_gate':
            self.result['success'] = True
            self.result['phase'] = 'deployed'
        elif phase == 'rollback_verified_pending_gate':
            self.result['rollback'] = True
            self.result['phase'] = 'rolled_back_same52'

    def execute(self):
        gate_lock = None
        try:
            self.preparation_admission()
            self.gate, gate_lock, self.binding = gate_enter(self.root, self.b)
            self.preflight(self.binding)
            self.owned(self.old, self.b['image'], True)
            # 硬切有在飞请求时，退出代际恢复负责 PG 终结和 Redis 槽位 CAS；不探针、不重放
            self.result['beforeCutActivity'] = self.activity()
            self.before = self.schema_observation(52, pending_required=self.m['schema'] == 53)
            preserve(self.result['beforeFacts'], self.before)
            self.result['beforeCutHealthAdmission'] = self.admit_baseline_health(
                self.owned(self.old, self.b['image'], True))
            with (self.gate / 'maintenance').open('x') as file:
                file.write(self.root.name)
            self.save('cut_admitted')
            self.stop(self.old, self.b['image'])
            self.recover(self.old, self.b['image'], self.binding)
            self.old_recovered = True
            candidate = self.start(self.old, self.b['image'], self.m['image'], self.target['id'], self.m['source'], 'candidate')
            self.result['candidateRuntime'] = candidate
            after = self.schema_observation(self.m['schema'], pending_required=self.m['schema'] == 53)
            preserve(self.before, after)
            self.owned(candidate, self.m['image'], True)
            self.result.update(afterRuntime=candidate, afterFacts=after, healthz=204)
            self.save('candidate_verified_pending_gate')
        except Exception as error:
            self.result['errorCode'] = str(error) if isinstance(error, RuntimeError) else 'remote_error_suppressed'
            if not self.stopped:
                self.save('blocked_unchanged' if hasattr(self, 'old') else 'preflight_blocked')
            elif self.result.get('stopCommand', {}).get('outcome') != 'completed' or self.recovery_pending:
                self.save('recovery_requires_operator')
            elif self.pin_changed:
                # Docker up 结果未知时只能观察现场，不能发第二次 stop / up。
                if self.result['errorCode'] in ('compose_result_unknown_no_retry',
                                                'replacement_observation_unknown_no_reissue',
                                                'replacement_identity_unproven'):
                    self.save('recovery_requires_operator')
                else:
                    try:
                        self.rollback(self.result.get('candidateRuntime'))
                    except Exception as recovery:
                        self.result['recoveryErrorCode'] = str(recovery) if isinstance(recovery, RuntimeError) else 'recovery_error_suppressed'
                        self.save('recovery_requires_operator')
            elif self.old_recovered:
                try:
                    self.rollback(None)
                except Exception as recovery:
                    self.result['recoveryErrorCode'] = str(recovery) if isinstance(recovery, RuntimeError) else 'recovery_error_suppressed'
                    self.save('recovery_requires_operator')
            else:
                self.save('recovery_requires_operator')
        finally:
            if gate_lock:
                try:
                    owner = self.gate / 'maintenance'
                    if owner.exists() and owner.read_text() == self.root.name:
                        try:
                            self.settle_gate()
                        except Exception as gate_error:
                            self.result['restartRecoveryGate'] = {'phase': 'maintenance_requires_operator',
                                'errorCode': str(gate_error) if isinstance(gate_error, RuntimeError) else 'gate_error_suppressed'}
                            self.result['success'] = False
                            self.result['rollback'] = False
                            self.result['phase'] = 'recovery_requires_operator'
                    else:
                        if self.stopped:
                            self.result['restartRecoveryGate'] = {'phase': 'gate_owner_requires_operator',
                                'errorCode': 'maintenance_owner_lost_after_stop'}
                            self.result['success'] = self.result['rollback'] = False
                            self.result['phase'] = 'recovery_requires_operator'
                        else:
                            self.result['restartRecoveryGate'] = {'phase': 'not_entered'}
                finally:
                    gate_lock.close()
            self.result['finishedAt'] = now()
            atomic(self.root / 'result.json', self.result)
        return self.result

    def confirm_settled_before_receipt(self, current, actual_pin):
        # 原 worker 在解闸后、终态收据落盘前退出：只核事实，不再改闸或重发命令
        phase = self.result['phase']
        require(phase in ('candidate_verified_pending_gate', 'rollback_verified_pending_gate'),
                'takeover_unowned_released_gate')
        expected = self.result.get('afterRuntime' if phase == 'candidate_verified_pending_gate' else 'rollbackRuntime')
        target_pin = self.m['image'] if phase == 'candidate_verified_pending_gate' else self.b['image']
        require(expected and actual_pin == target_pin and
                all(current[k] == expected[k] for k in ('id', 'imageId', 'revision', 'startedAt', 'project', 'service')) and
                current['running'] and current['health'] == 'healthy' and health(), 'takeover_settled_runtime_drift')
        require(self.binding['runtime'] == {k: current[k] for k in ('id', 'project', 'service', 'revision')} and
                not (self.gate / 'maintenance').exists() and self.effective_image() == target_pin,
                'takeover_settled_gate_or_compose_drift')
        _, old_receipt = self.recovery_receipt(self.old)
        require(old_receipt and old_receipt['phase'] == 'complete', 'takeover_old_receipt_unproven')
        candidate = self.result.get('candidateRuntime')
        if phase == 'rollback_verified_pending_gate' and candidate:
            _, candidate_receipt = self.recovery_receipt(candidate)
            require(candidate_receipt and candidate_receipt['phase'] == 'complete',
                    'takeover_candidate_receipt_unproven')
        preserve(self.before, self.schema_observation(
            self.m['schema'] if phase == 'candidate_verified_pending_gate' else 52,
            pending_required=self.m['schema'] == 53))
        self.result['restartRecoveryGate'] = {'phase': 'rebound', 'runtime': self.binding['runtime']}
        self.result['phase'] = 'deployed' if phase == 'candidate_verified_pending_gate' else 'rolled_back_same52'
        self.result['success'] = phase == 'candidate_verified_pending_gate'
        self.result['rollback'] = phase == 'rollback_verified_pending_gate'

    def resume(self, retry_readonly=False, active_held=False):
        self.operator_takeover = True
        # 每次接管保留独立终态；无结果接管以进程退出和未改进度证据重获一次新 GO
        origin = self.root / 'result.json'
        prior = self.root / 'takeover-result.json'
        if retry_readonly and not prior.exists():
            retry_evidence(self.root, active_held=active_held)
        source = prior if retry_readonly and prior.exists() else (
                 origin if origin.exists() else self.root / 'progress.json')
        initial = json.loads(source.read_text())
        if retry_readonly and prior.exists():
            require(initial.get('retryableReadOnly') is True and
                    initial.get('takeoverCommandsIssued') is False, 'takeover_retry_not_readonly')
        require(initial['phase'] not in ('deployed', 'rolled_back_same52', 'blocked_unchanged',
                                          'preflight_blocked') and
                (self.root / 'attempt-claimed.json').exists(), 'takeover_source_not_recoverable')
        self.result = initial
        self.result['resumedFrom'] = {key: initial.get(key) for key in
            ('phase', 'errorCode', 'recoveryErrorCode', 'takeoverErrorCode', 'restartRecoveryGate')}
        for key in ('errorCode', 'recoveryErrorCode', 'takeoverErrorCode', 'takeoverFinishedAt'):
            self.result.pop(key, None)
        self.result['success'] = self.result['rollback'] = False
        self.result['retryableReadOnly'] = False
        self.result['takeoverCommandsIssued'] = False
        action_started = False
        maintenance_owner = None
        lock = None
        try:
            self.old = self.result['beforeRuntime']
            self.before = self.result['beforeFacts']
            self.target = self.result['targetImage']
            require(self.old['revision'] == self.b['source'] and self.old['id'] == self.b['containerId'] and
                    self.old['startedAt'] == self.b['startedAt'] and
                    self.old['imageId'] == image(self.b['image'], self.b['source'])['id'] and
                    self.target['id'] == image(self.m['image'], self.m['source'])['id'], 'takeover_image_drift')
            self.files = self.old['composeFiles'].split(',')
            self.workdir = self.old['workingDir']
            require(self.workdir and Path(self.workdir).resolve().is_relative_to(BASE) and self.files and
                    all(Path(path).resolve().is_relative_to(BASE) for path in self.files), 'takeover_compose_scope_drift')
            self.file_stamps = self.result['admittedFileStamps']
            self.gate = BASE / '.runtime/restart-recovery'
            lock = (self.gate / 'lifecycle.lock').open('a')
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            maintenance = self.gate / 'maintenance'
            maintenance_owner = maintenance.read_text() if maintenance.exists() else None
            require(maintenance_owner in (None, self.root.name), 'takeover_maintenance_owner_drift')
            self.binding = json.loads((self.gate / 'binding.json').read_text())
            require(self.binding.get('helperSource') == HELPER_SOURCE and
                    self.binding.get('helperImage') == HELPER_IMAGE and self.binding.get('singleDeployment') is True and
                    self.binding.get('recoveryCommand', [])[:2] ==
                    ['python3', str(self.gate / 'site-recovery.py')], 'takeover_helper_drift')
            require(command(['docker', 'inspect', '--format', '{{json .HostConfig.RestartPolicy.Name}}', SERVICE]).decode().strip().strip('"') == 'no',
                    'takeover_restart_policy_drift')
            require(opaque_env_digest(BASE / '.env') == self.result['envOpaqueDigest'], 'takeover_env_opaque_drift')
            actual_pin = pin(BASE / '.env')[2]
            require(actual_pin in (self.b['image'], self.m['image']), 'takeover_pin_unknown')
            if actual_pin != self.b['image']:
                require(self.result.get('pinCommands', {}).get('candidate', {}).get('to') == actual_pin,
                        'takeover_pin_not_claimed')
            current_stamps = self.stamps()
            require(all(current_stamps[path] == stamp for path, stamp in self.file_stamps.items()
                        if path != str(BASE / '.env')), 'takeover_config_drift')
            require(self.effective_image() == actual_pin, 'takeover_service_image_drift')
            # .env 可为本 run 已记录 pin，或原始字节完整恢复后仅 mtime 变化
            if current_stamps[str(BASE / '.env')] != self.file_stamps[str(BASE / '.env')]:
                restored = (actual_pin == self.b['image'] and self.result.get('envOriginalDigest') ==
                            hashlib.sha256((BASE / '.env').read_bytes()).hexdigest())
                require(self.result.get('pinCommands') or restored, 'takeover_env_change_not_claimed')
                self.file_stamps[str(BASE / '.env')] = current_stamps[str(BASE / '.env')]
                self.result['admittedFileStamps'] = self.file_stamps
            self.pin_changed = actual_pin == self.m['image']
            current = container()
            allowed_binding = [{k: self.old[k] for k in ('id', 'project', 'service', 'revision')}]
            for name in ('candidateRuntime', 'rollbackRuntime'):
                known = self.result.get(name)
                if known:
                    allowed_binding.append({k: known[k] for k in ('id', 'project', 'service', 'revision')})
            require(self.binding.get('runtime') in allowed_binding, 'takeover_binding_runtime_drift')
            if maintenance_owner is None:
                self.confirm_settled_before_receipt(current, actual_pin)
                return self.result
            old_identity = all(current[k] == self.old[k] for k in ('id', 'imageId', 'revision', 'startedAt'))
            old_claim = self.result.get('stopCommands', {}).get(self.old['id'])
            # 持久化即将执行的意图；此后即使进程消失也不能因只读失败而重获执行授权
            action_started = True
            self.result['takeoverCommandsIssued'] = True
            self.save('takeover_action_intent')
            if old_identity and current['running']:
                rollback_cmd = self.result.get('composeCommands', {}).get('rollback')
                require(not old_claim and not rollback_cmd and actual_pin == self.b['image'],
                        'old_running_after_stop_unknown')
                self.admit_baseline_health(current)
                self.save('blocked_unchanged')
            else:
                _, old_receipt = self.recovery_receipt(self.old)
                if old_identity:
                    require(not current['running'] and old_claim is not None,
                            'takeover_old_stop_unproven')
                    require(not self.result.get('composeCommands', {}).get('candidate'),
                            'candidate_compose_outcome_unobserved_no_reissue')
                    if self.result.get('stopCommand', {}).get('containerId') == self.old['id']:
                        self.result['stopCommand']['outcome'] = 'observed_stopped'
                    self.recover(self.old, actual_pin, self.binding)
                    self.old_recovered = True
                    self.rollback(None)
                else:
                    # rollback up 曾发送时，仅观察其确切运行容器，不再执行第二次 up
                    rollback_cmd = self.result.get('composeCommands', {}).get('rollback')
                    candidate_cmd = self.result.get('composeCommands', {}).get('candidate')
                    if rollback_cmd and actual_pin == self.b['image'] and current['imageId'] == self.old['imageId']:
                        require(old_receipt is not None and old_receipt['phase'] == 'complete',
                                'takeover_old_receipt_unproven')
                        candidate_owner = self.result.get('candidateRuntime')
                        if candidate_owner:
                            _, candidate_receipt = self.recovery_receipt(candidate_owner)
                            require(candidate_receipt is not None and candidate_receipt['phase'] == 'complete',
                                    'takeover_candidate_receipt_unproven')
                        current = self.observed_replacement(self.old, self.old['imageId'], self.b['source'],
                                                            self.b['image'], 'rollback', limit=3)
                        require(current['running'] and current['health'] == 'healthy' and health(),
                                'takeover_rollback_health_drift')
                        preserve(self.before, self.schema_observation(52, pending_required=self.m['schema'] == 53))
                        self.result['rollbackRuntime'] = current
                        self.save('rollback_verified_pending_gate')
                    else:
                        require(candidate_cmd is not None and actual_pin == self.m['image'] and
                                old_receipt is not None and old_receipt['phase'] == 'complete',
                                'takeover_candidate_or_old_receipt_unproven')
                        current = self.observed_replacement(self.old, self.target['id'], self.m['source'],
                                                            self.m['image'], 'candidate', limit=3)
                        require(self.binding['runtime'] in
                                ({k: self.old[k] for k in ('id', 'project', 'service', 'revision')},
                                 {k: current[k] for k in ('id', 'project', 'service', 'revision')}),
                                'takeover_binding_runtime_drift')
                        self.old_recovered = True
                        if not any(row['containerId'] == self.old['id'] for row in self.result.setdefault('stoppedRecovery', [])):
                            self.result['stoppedRecovery'].append(dict(old_receipt['released'],
                                containerId=self.old['id'], requests=len(old_receipt['completed']), phase='complete'))
                        if not current['running'] and self.result.get('stopCommand', {}).get('containerId') == current['id']:
                            self.result['stopCommand']['outcome'] = 'observed_stopped'
                        self.rollback(current)
            self.settle_gate()
        except Exception as error:
            self.result['success'] = self.result['rollback'] = False
            self.result['phase'] = 'recovery_requires_operator'
            self.result['takeoverErrorCode'] = str(error) if isinstance(error, RuntimeError) else 'takeover_error_suppressed'
            if lock is not None:
                self.result['restartRecoveryGate'] = {'phase': 'maintenance_requires_operator' if
                    (self.gate / 'maintenance').exists() else 'released_requires_operator'}
            # 首个只读探测可在拿闸锁前失败；先留零动作证据，下次重新取双锁核现场
            maintenance = BASE / '.runtime/restart-recovery/maintenance'
            try:
                owned_maintenance = maintenance.read_text() == self.root.name
            except OSError:
                owned_maintenance = False
            self.result['retryableReadOnly'] = not action_started and owned_maintenance
        finally:
            if lock is not None:
                lock.close()
            self.result['takeoverFinishedAt'] = now()
            atomic(self.root / ('takeover-retry-result.json' if retry_readonly else 'takeover-result.json'), self.result)
        return self.result


def retry_evidence(root, active_held=False):
    # 控制器和执行器分别取得锁检查：无结果的首次接管必须证实进程退出且进度原封不动
    active = None
    try:
        if not active_held:
            active = (BASE / '.runtime/deployments/task40-active.lock').open('r')
            fcntl.flock(active, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with (BASE / '.runtime/restart-recovery/lifecycle.lock').open('r') as gate_lock:
            fcntl.flock(gate_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            require((BASE / '.runtime/restart-recovery/maintenance').read_text() == root.name,
                    'takeover_retry_gate_owner_drift')
            claim = json.loads((root / 'takeover-attempt-claimed.json').read_text())
            require(type(claim.get('pid')) is int and claim['pid'] > 1 and
                    claim.get('protocol') == 'readonly_progress_barrier_v1',
                    'takeover_original_process_identity_unproven')
            try:
                os.kill(claim['pid'], 0)
            except ProcessLookupError:
                pass
            else:
                raise RuntimeError('takeover_original_process_not_confirmed_exited')
            prior = root / 'takeover-result.json'
            if prior.exists():
                value = json.loads(prior.read_text())
                require(value.get('retryableReadOnly') is True and
                        value.get('takeoverCommandsIssued') is False,
                        'takeover_retry_not_readonly')
                mode = 'read_only_result'
            else:
                source = root / claim.get('sourceFile', '')
                progress = root / 'progress.json'
                require(claim.get('sourceFile') in ('progress.json', 'result.json') and
                        source.name == ('result.json' if (root / 'result.json').exists() else 'progress.json') and
                        source.exists() and progress.exists() and
                        not (root / 'progress.json.tmp').exists() and
                        not (root / 'takeover-result.json.tmp').exists() and
                        not (root / (source.name + '.tmp')).exists() and
                        hashlib.sha256(source.read_bytes()).hexdigest() == claim.get('sourceDigest') and
                        hashlib.sha256(progress.read_bytes()).hexdigest() == claim.get('progressDigest'),
                        'takeover_progress_changed_or_unknown_no_reissue')
                mode = 'orphaned_read_only'
            fcntl.flock(gate_lock, fcntl.LOCK_UN)
            return {'phase': 'retry_admissible', 'mode': mode}
    finally:
        if active is not None:
            active.close()


def baseline(mode='healthy'):
    require(mode in ('healthy', 'disk_logging_recovery'), 'explicit_admission_mode_required')
    old = container()
    b = baseline_identity({'source': old['revision'], 'image': pin(BASE / '.env')[2],
                           'containerId': old['id'], 'startedAt': old['startedAt']})
    bound_runtime(old, b)
    require(old['imageId'] == image(b['image'], b['source'])['id'], 'baseline_image_drift')
    gate = BASE / '.runtime/restart-recovery'
    require(not (gate / 'maintenance').exists(), 'restart_maintenance_occupied')
    binding = json.loads((gate / 'binding.json').read_text())
    require(binding['runtime'] == {k: old[k] for k in ('id', 'project', 'service', 'revision')} and
            binding.get('helperSource') == HELPER_SOURCE and binding.get('helperImage') == HELPER_IMAGE and
            binding.get('singleDeployment') is True and binding.get('recoveryCommand', [])[:2] ==
            ['python3', str(gate / 'site-recovery.py')], 'restart_binding_drift')
    require(command(['systemctl', 'is-active', 'cpr-restart-recovery.timer']).decode().strip() == 'active' and
            command(['systemctl', 'is-enabled', 'cpr-restart-recovery.timer']).decode().strip() == 'enabled',
            'watchdog_inactive')
    require(command(['docker', 'inspect', '--format', '{{json .HostConfig.RestartPolicy.Name}}', SERVICE]).decode().strip().strip('"') == 'no', 'restart_policy_not_no')
    app = Deployment.__new__(Deployment)
    app.root = BASE / '.runtime/deployments'
    app.workdir, app.files = old['workingDir'], old['composeFiles'].split(',')
    require(app.workdir and Path(app.workdir).resolve().is_relative_to(BASE) and app.files and
            all(Path(path).resolve().is_relative_to(BASE) for path in app.files) and
            not any('override' in Path(path).name for path in app.files), 'compose_scope_drift')
    require(app.effective_image() == b['image'], 'baseline_compose_pin_drift')
    with (gate / 'lifecycle.lock').open('r') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if mode == 'healthy':
            require(old['health'] == 'healthy' and health(), 'baseline_runtime_drift')
            evidence = {'healthz': 204}
        else:
            evidence = disk_recovery_evidence(old)
        bound_runtime(container(), b)
    return {'capturedAt': now(), 'baseline': b, 'admissionMode': mode, 'runtime': old,
            'envPin': b['image'], 'healthz': evidence['healthz'], 'degradedEvidence': evidence,
            'schemaAndPreservationFacts': require_schema(facts(), 52), 'activity': app.activity(),
            'freeBytes': shutil.disk_usage(BASE).free,
            'recoveryBindingRuntime': binding['runtime'], 'helperSource': binding['helperSource'],
            'helperImage': binding['helperImage'], 'maintenanceExists': False,
            'serviceImage': b['image'], 'recoveryLock': 'unheld'}


def verify(root):
    result_path = next((path for path in (root / 'takeover-retry-result.json', root / 'takeover-result.json',
                        root / 'result.json') if path.exists()), root / 'result.json')
    result = json.loads(result_path.read_text())
    require(result['phase'] in ('deployed', 'rolled_back_same52'), 'terminal_health_not_admitted')
    for path in (BASE / '.runtime/deployments/task40-active.lock',
                 BASE / '.runtime/restart-recovery/lifecycle.lock', root / 'launch.guard'):
        with path.open('r') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(lock, fcntl.LOCK_UN)
    current = container()
    expected = result['afterRuntime'] if result['phase'] == 'deployed' else result['rollbackRuntime']
    target = manifest(json.loads((root / 'accepted.json').read_text()))
    expected_image = target['image'] if result['phase'] == 'deployed' else target['baseline']['image']
    require(all(current[k] == expected[k] for k in ('id', 'imageId', 'startedAt', 'revision')) and
            current['running'] and current['health'] == 'healthy' and health(), 'fresh_health_drift')
    require(pin(BASE / '.env')[2] == expected_image, 'fresh_pin_drift')
    gate = BASE / '.runtime/restart-recovery'
    require(not (gate / 'maintenance').exists() and
            json.loads((gate / 'binding.json').read_text())['runtime'] ==
            {key: current[key] for key in ('id', 'project', 'service', 'revision')}, 'fresh_gate_drift')
    app = Deployment(root)
    app.workdir, app.files = current['workingDir'], current['composeFiles'].split(',')
    require(app.effective_image() == expected_image, 'fresh_compose_drift')
    fresh = app.schema_observation(target['schema'] if result['phase'] == 'deployed' else 52,
                                   pending_required=target['schema'] == 53)
    preserve(result['beforeFacts'], fresh)
    return {'phase': 'verified', 'capturedAt': now(), 'runId': root.name,
            'runtime': current, 'envPin': expected_image, 'serviceImage': expected_image,
            'facts': fresh, 'healthz': 204, 'recoveryGate': 'rebound', 'locks': 'unheld'}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['run', 'prepare', 'cleanup', 'reconcile-preparation',
                                          'resume', 'retry-admission', 'receipt', 'verify'])
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--GO', action='store_true')
    parser.add_argument('--retry-readonly', action='store_true')
    args = parser.parse_args()
    os.umask(0o077)
    app = Deployment(args.root)
    if args.action == 'retry-admission':
        print(json.dumps(retry_evidence(args.root)))
        return
    if args.action == 'receipt':
        retry = args.root / 'takeover-retry-result.json'
        first = args.root / 'takeover-result.json'
        if (args.root / 'takeover-retry-launch-claimed.json').exists() and not retry.exists():
            print(json.dumps({'phase': 'takeover_pending', 'attemptClaimed':
                              (args.root / 'takeover-retry-attempt-claimed.json').exists()}))
        elif retry.exists():
            print(retry.read_text())
        elif (args.root / 'takeover-attempt-claimed.json').exists() and not first.exists():
            print(json.dumps({'phase': 'takeover_pending', 'attemptClaimed': True}))
        elif first.exists():
            print(first.read_text())
        else:
            result = args.root / 'result.json'
            print(result.read_text() if result.exists() else json.dumps({'phase': 'pending', 'attemptClaimed': (args.root / 'attempt-claimed.json').exists()}))
        return
    if args.action == 'verify':
        print(json.dumps(verify(args.root)))
        return
    lock_path = BASE / '.runtime/deployments/task40-active.lock'
    with lock_path.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if args.action == 'reconcile-preparation':
            print(json.dumps(app.reconcile_preparation()))
            return
        if args.action in ('prepare', 'cleanup'):
            require(args.GO and (args.root / ('schema53-' + args.action + '-launch-claimed.json')).exists() and
                    not (args.root / 'attempt-claimed.json').exists(), 'new_schema53_preparation_GO_required')
            print(json.dumps(app.prepare(cleanup=args.action == 'cleanup')))
            return
        if args.action == 'resume':
            retry = args.retry_readonly
            launch = args.root / ('takeover-retry-launch-claimed.json' if retry else 'takeover-launch-claimed.json')
            prior = args.root / 'takeover-result.json'
            result_path = args.root / ('takeover-retry-result.json' if retry else 'takeover-result.json')
            require(args.GO and (args.root / 'attempt-claimed.json').exists() and launch.exists() and
                    not result_path.exists(), 'new_takeover_GO_and_claim_required')
            if retry:
                retry_evidence(args.root, active_held=True)
            else:
                require(not prior.exists(), 'takeover_already_finished')
            source = prior if retry and prior.exists() else (
                args.root / 'result.json' if (args.root / 'result.json').exists() else args.root / 'progress.json')
            progress = args.root / 'progress.json'
            claim = args.root / ('takeover-retry-attempt-claimed.json' if retry else 'takeover-attempt-claimed.json')
            with claim.open('x') as file:
                json.dump({'at': now(), 'pid': os.getpid(), 'protocol': 'readonly_progress_barrier_v1',
                           'sourceFile': source.name,
                           'sourceDigest': hashlib.sha256(source.read_bytes()).hexdigest(),
                           'progressDigest': hashlib.sha256(progress.read_bytes()).hexdigest()}, file)
                file.flush()
                os.fsync(file.fileno())
            result = app.resume(retry_readonly=retry, active_held=True)
            print(json.dumps({'phase': result['phase'], 'success': result['success'], 'rollback': result['rollback']}))
            return
        with (args.root / 'launch.guard').open('a') as guard:
            fcntl.flock(guard, fcntl.LOCK_EX)
            require(not (args.root / 'abort-before-run').exists() and not (args.root / 'result.json').exists(), 'run_fenced_or_finished')
            with (args.root / 'attempt-claimed.json').open('x') as claim:
                json.dump({'at': now()}, claim)
        result = app.execute()
        print(json.dumps({'phase': result['phase'], 'success': result['success'], 'rollback': result['rollback']}))


if __name__ == '__main__':
    main()
