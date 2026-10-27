#!/usr/bin/env python3
"""Task40 一次性同 schema52 切换；只由 controller 传入父签 manifest 后运行。"""
import argparse
import datetime as dt
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
OLD_SOURCE = 'a6461769337f83a2a7e41f69be232897c91b9041'
OLD_IMAGE = 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:96c2584487318f721d3ca2ba6b67299c9a17747071578cd3d768b8d1e1a9b5b4'
IMAGE_RE = r'ghcr\.io/x-t-e-r/codex-proxy-rs@sha256:[0-9a-f]{64}'
PG = '1Panel-postgresql-5AJX'
HELPER_SOURCE = '476d24c16764a0a535fabd975a3df18d2192df0a'
HELPER_IMAGE = 'ghcr.io/x-t-e-r/codex-proxy-rs@sha256:f8c705a4da6687ea39902bb09e020772a4083d2dae12c1d0cd507952bf88944e'
RECOVERY_TIMEOUT = 1800


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


def manifest(m):
    require(type(m) is dict and m.get('accepted') is True and m.get('schema') == 52 and
            m.get('noMigrationChanges') is True and m.get('hardCutAuthorized') is True,
            'parent_accepted_same52_hardcut_required')
    require(re.fullmatch('[0-9a-f]{40}', m.get('source', '')) is not None and
            re.fullmatch(IMAGE_RE, m.get('image', '')) is not None and
            m['source'] != OLD_SOURCE and m['image'] != OLD_IMAGE, 'new_exact_source_image_required')
    require(isinstance(m.get('migrationEvidence'), str) and m['migrationEvidence'].strip() and
            isinstance(m.get('acceptanceEvidence'), str) and m['acceptanceEvidence'].strip(),
            'parent_evidence_required')
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
    value = json.loads(sql(FACTS_SQL))
    require((value['schemaMax'], value['schemaCount'], value['schemaSuccess']) == (52, 52, True), 'schema52_required')
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


def health():
    try:
        with urllib.request.urlopen('http://127.0.0.1:8080/healthz', timeout=3) as response:
            return response.status == 204
    except OSError:
        return False


def configured_service_image(raw):
    # 只返回目标服务已解析的镜像，不把含其它配置与凭据的 Compose JSON 写入收据
    config = json.loads(raw)
    services = config.get('services')
    require(isinstance(services, dict) and isinstance(services.get(SERVICE), dict) and
            isinstance(services[SERVICE].get('image'), str), 'compose_target_service_image_missing')
    return services[SERVICE]['image']


def gate_enter(root):
    gate = BASE / '.runtime/restart-recovery'
    require((gate / 'binding.json').is_file(), 'restart_gate_required')
    lock = (gate / 'lifecycle.lock').open('a')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(not (gate / 'maintenance').exists(), 'restart_gate_maintenance_occupied')
        binding = json.loads((gate / 'binding.json').read_text())
        require(binding.get('runtime', {}).get('revision') == OLD_SOURCE, 'restart_gate_source_drift')
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
        self.result = dict(runId=self.root.name, phase='admitting', success=False, rollback=False,
                           startedAt=now(), providerProbes=0, channelMutations=0, schemaWrites=0)
        self.stopped = False
        self.pin_changed = False
        self.old_recovered = False
        self.recovery_pending = False

    def save(self, phase):
        self.result['phase'] = phase
        atomic(self.root / 'progress.json', self.result)

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

    def preflight(self, binding):
        old = container()
        require(old['project'] == SERVICE and old['service'] == SERVICE and old['revision'] == OLD_SOURCE and
                old['running'] and old['health'] == 'healthy' and health(), 'baseline_runtime_drift')
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
        require(old['imageId'] == image(OLD_IMAGE, OLD_SOURCE)['id'], 'baseline_image_drift')
        target = image(self.m['image'], self.m['source'])
        self.old, self.target = old, target
        self.files = old['composeFiles'].split(',')
        self.workdir = old['workingDir']
        require(self.workdir and Path(self.workdir).resolve().is_relative_to(BASE) and self.files and
                all(Path(path).resolve().is_relative_to(BASE) for path in self.files) and
                not any('override' in Path(path).name for path in self.files), 'compose_scope_drift')
        require(pin(BASE / '.env')[2] == OLD_IMAGE and self.effective_image() == OLD_IMAGE,
                'target_service_image_or_pin_drift')
        require(shutil.disk_usage(BASE).free >= 2 * 1024**3, 'disk_space_low')
        self.before = facts()
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
        previous_pin = self.m['image'] if self.pin_changed else OLD_IMAGE
        if current is not None:
            current = self.owned(current, self.m['image'])
            if current['running']:
                self.stop(current, self.m['image'])
            self.recover(current, self.m['image'], self.binding)
        else:
            self.owned(self.old, previous_pin, False)
        previous = current or self.old
        old = self.start(previous, previous_pin, OLD_IMAGE, self.old['imageId'], OLD_SOURCE, 'rollback')
        preserve(self.before, facts())
        self.result.update(rollbackRuntime=old, rollbackHealthz=204, afterFacts=facts())
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
                current['running'] and current['health'] == 'healthy' and health(), 'gate_runtime_drift')
        target_pin = self.m['image'] if phase == 'candidate_verified_pending_gate' else OLD_IMAGE
        require(pin(BASE / '.env')[2] == target_pin, 'gate_pin_drift')
        require(self.stamps() == self.file_stamps and
                opaque_env_digest(BASE / '.env') == self.result['envOpaqueDigest'] and
                self.effective_image() == target_pin, 'gate_configuration_or_service_image_drift')
        require((self.gate / 'maintenance').read_text() == self.root.name and
                json.loads((self.gate / 'binding.json').read_text()) == self.binding, 'gate_owner_drift')
        updated = dict(self.binding)
        updated['runtime'] = {k: current[k] for k in ('id', 'project', 'service', 'revision')}
        # 先核健康和完整保全再放行 watchdog
        preserve(self.result['beforeFacts'], facts())
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
            self.gate, gate_lock, self.binding = gate_enter(self.root)
            self.preflight(self.binding)
            self.owned(self.old, OLD_IMAGE, True)
            # 硬切有在飞请求时，退出代际恢复负责 PG 终结和 Redis 槽位 CAS；不探针、不重放
            self.result['beforeCutActivity'] = self.activity()
            self.before = facts()
            preserve(self.result['beforeFacts'], self.before)
            self.owned(self.old, OLD_IMAGE, True)
            with (self.gate / 'maintenance').open('x') as file:
                file.write(self.root.name)
            self.save('cut_admitted')
            self.stop(self.old, OLD_IMAGE)
            self.recover(self.old, OLD_IMAGE, self.binding)
            self.old_recovered = True
            candidate = self.start(self.old, OLD_IMAGE, self.m['image'], self.target['id'], self.m['source'], 'candidate')
            self.result['candidateRuntime'] = candidate
            after = facts()
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
        target_pin = self.m['image'] if phase == 'candidate_verified_pending_gate' else OLD_IMAGE
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
        preserve(self.before, facts())
        self.result['restartRecoveryGate'] = {'phase': 'rebound', 'runtime': self.binding['runtime']}
        self.result['phase'] = 'deployed' if phase == 'candidate_verified_pending_gate' else 'rolled_back_same52'
        self.result['success'] = phase == 'candidate_verified_pending_gate'
        self.result['rollback'] = phase == 'rollback_verified_pending_gate'

    def resume(self, retry_readonly=False, active_held=False):
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
            require(self.old['revision'] == OLD_SOURCE and
                    self.old['imageId'] == image(OLD_IMAGE, OLD_SOURCE)['id'] and
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
            require(actual_pin in (OLD_IMAGE, self.m['image']), 'takeover_pin_unknown')
            if actual_pin != OLD_IMAGE:
                require(self.result.get('pinCommands', {}).get('candidate', {}).get('to') == actual_pin,
                        'takeover_pin_not_claimed')
            current_stamps = self.stamps()
            require(all(current_stamps[path] == stamp for path, stamp in self.file_stamps.items()
                        if path != str(BASE / '.env')), 'takeover_config_drift')
            require(self.effective_image() == actual_pin, 'takeover_service_image_drift')
            # .env 可为本 run 已记录 pin，或原始字节完整恢复后仅 mtime 变化
            if current_stamps[str(BASE / '.env')] != self.file_stamps[str(BASE / '.env')]:
                restored = (actual_pin == OLD_IMAGE and self.result.get('envOriginalDigest') ==
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
                require(not old_claim and not rollback_cmd and actual_pin == OLD_IMAGE and
                        current['health'] == 'healthy' and health(), 'old_running_after_stop_unknown')
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
                    if rollback_cmd and actual_pin == OLD_IMAGE and current['imageId'] == self.old['imageId']:
                        require(old_receipt is not None and old_receipt['phase'] == 'complete',
                                'takeover_old_receipt_unproven')
                        candidate_owner = self.result.get('candidateRuntime')
                        if candidate_owner:
                            _, candidate_receipt = self.recovery_receipt(candidate_owner)
                            require(candidate_receipt is not None and candidate_receipt['phase'] == 'complete',
                                    'takeover_candidate_receipt_unproven')
                        current = self.observed_replacement(self.old, self.old['imageId'], OLD_SOURCE,
                                                            OLD_IMAGE, 'rollback', limit=3)
                        require(current['running'] and current['health'] == 'healthy' and health(),
                                'takeover_rollback_health_drift')
                        preserve(self.before, facts())
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


def baseline():
    old = container()
    require(old['project'] == SERVICE and old['service'] == SERVICE and old['revision'] == OLD_SOURCE and
            old['running'] and old['health'] == 'healthy' and health(), 'baseline_runtime_drift')
    require(old['imageId'] == image(OLD_IMAGE, OLD_SOURCE)['id'], 'baseline_image_drift')
    require(pin(BASE / '.env')[2] == OLD_IMAGE, 'baseline_pin_drift')
    gate = BASE / '.runtime/restart-recovery'
    require(not (gate / 'maintenance').exists(), 'restart_maintenance_occupied')
    binding = json.loads((gate / 'binding.json').read_text())
    require(binding['runtime'] == {k: old[k] for k in ('id', 'project', 'service', 'revision')} and
            binding.get('helperSource') == HELPER_SOURCE and binding.get('helperImage') == HELPER_IMAGE,
            'restart_binding_drift')
    require(command(['systemctl', 'is-active', 'cpr-restart-recovery.timer']).decode().strip() == 'active',
            'watchdog_inactive')
    require(command(['docker', 'inspect', '--format', '{{json .HostConfig.RestartPolicy.Name}}', SERVICE]).decode().strip().strip('"') == 'no', 'restart_policy_not_no')
    cmd = ['docker', 'compose', '--project-directory', old['workingDir'], '--env-file', str(BASE / '.env'),
           '-p', SERVICE]
    for file in old['composeFiles'].split(','):
        cmd += ['-f', file]
    require(configured_service_image(command(cmd + ['config', '--format', 'json'])) == OLD_IMAGE,
            'baseline_compose_pin_drift')
    with (gate / 'lifecycle.lock').open('r') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(lock, fcntl.LOCK_UN)
    app = Deployment.__new__(Deployment)
    app.root = BASE / '.runtime/deployments'
    return {'capturedAt': now(), 'runtime': old, 'envPin': OLD_IMAGE, 'healthz': 204,
            'schemaAndPreservationFacts': facts(), 'activity': app.activity(),
            'freeBytes': shutil.disk_usage(BASE).free,
            'recoveryBindingRuntime': binding['runtime'], 'helperSource': binding['helperSource'],
            'helperImage': binding['helperImage'], 'maintenanceExists': False,
            'serviceImage': OLD_IMAGE, 'recoveryLock': 'unheld'}


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
    expected_image = target['image'] if result['phase'] == 'deployed' else OLD_IMAGE
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
    fresh = facts()
    preserve(result['beforeFacts'], fresh)
    return {'phase': 'verified', 'capturedAt': now(), 'runId': root.name,
            'runtime': current, 'envPin': expected_image, 'serviceImage': expected_image,
            'facts': fresh, 'healthz': 204, 'recoveryGate': 'rebound', 'locks': 'unheld'}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['run', 'resume', 'retry-admission', 'receipt', 'verify'])
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
