"""Exercise authenticated policy and observability APIs in the isolated Compose runtime."""
import http.cookiejar
import json
import os
from pathlib import Path
import subprocess
import urllib.error
import urllib.parse
import urllib.request

origin = 'http://127.0.0.1:8080'
client = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
checks = []


def api(path, body=None, status=200):
    request = urllib.request.Request(origin + path,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Origin': origin, 'Content-Type': 'application/json'})
    try:
        response = client.open(request, timeout=15)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        value = json.load(response)
        assert response.status == status, (path, response.status, value)
        if status == 200:
            assert value['code'] == 200, (path, value)
            return value['data']


def sql(statement):
    result = subprocess.run(['docker', 'compose', '--project-name', os.environ['COMPOSE_PROJECT_NAME'],
        '-f', 'deploy/compose.yaml', 'exec', '-T', '-e', 'PGPASSWORD=' + os.environ['CI_POSTGRES_PASSWORD'],
        'postgres', 'psql', '-X', '-U', 'codex_proxy', '-d', 'codex_proxy', '-v', 'ON_ERROR_STOP=1'],
        input=statement, text=True, capture_output=True)
    assert result.returncode == 0, result.stderr


assert api('/api/auth/login', {'mode':'admin', 'username':'admin@cpr.local',
    'password':os.environ['CI_ADMIN_PASSWORD']})['role'] == 'admin'
checks.append('real admin login and cookie authentication')


def settings_command(view):
    read_only = {'smartSchedulingDefaults', 'updatedAt', 'updatedAtDisplay',
                 'providerRequestProfiles', 'openaiClientProfile', 'xaiClientProfile'}
    return {key:value for key,value in view.items() if key not in read_only}


original_settings = api('/api/admin/settings')
protection = {'cyberSessionBlockEnabled':True, 'cyberSessionBlockTtlSeconds':3600,
    'wsPoolEnabled':True, 'wsPoolMaxAgeMs':10000000, 'wsPoolMaxConnecting':16,
    'wsPoolStreamIdleTimeoutMs':3000000, 'wsPoolFastPathBudgetMs':30000}
command = settings_command(original_settings)
command.update(protection)
settings = api('/api/admin/settings/update', command)
assert {key:settings[key] for key in protection} == protection
api('/api/admin/settings/update', command, status=409)
partial = settings_command(settings)
for key in protection:
    del partial[key]
partial['cyberSessionBlockEnabled'] = False
settings = api('/api/admin/settings/update', partial)
expected = dict(protection, cyberSessionBlockEnabled=False)
assert {key:settings[key] for key in protection} == expected
assert {key:value for key,value in api('/api/admin/settings').items() if key in protection} == expected
restore = settings_command(original_settings)
restore['configRevision'] = settings['configRevision']
api('/api/admin/settings/update', restore)
checks.append('real Settings/PG save, seven retained Cyber/pool values, omitted fields, explicit off, and stale-revision 409')
initial = api('/api/admin/priority-policy')
assert initial['mode'] == 'default' and initial['autoOffOnQuotaReset'] is False
command = {'expectedRevision':initial['revision'], 'mode':'enabled', 'autoOffOnQuotaReset':True}
saved = api('/api/admin/priority-policy/update', command)
api('/api/admin/priority-policy/update', command, status=409)
checks.append('global auto-off default saved with stale-revision 409')
try:
    # 仅在临时 Compose 库内写合成停用账号，不调用 Provider 或携带真实凭据
    sql("""
    insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at)
    values('acct_ci_policy','openai','CI disabled synthetic','oauth','{}',1,false,false,'ready',now(),now(),now());
    insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,provider_account_ref,upstream_transport,outcome,client_status_code,upstream_status_code,error_kind,attempt_count,started_at,recovery_lease_expires_at,completed_at,request_observation_json)
    select id,'key_ci','responses','http_sse','openai','acct_ci_policy',transport,'failed',client_status,upstream_status,kind,attempts,now()-interval '1 minute',now()+interval '1 hour',now()-interval '30 seconds',
    jsonb_build_object('request',jsonb_build_object('configRevision',1,'protocol','openai','endpoint','/v1/responses','compact',false),'account',jsonb_build_object('name','CI disabled synthetic','authenticationKind','oauth'),'routing',jsonb_build_object('scope','all','groupRefs','[]'::jsonb,'groupNamesSnapshot','[]'::jsonb),'error',jsonb_build_object('providerErrorCode',code,'message','CI synthetic error'))
    from (values
      ('req_ci_stream','http_sse',200,503,'upstream_error',1,'server_overloaded'),
      ('req_ci_infra',null,503,null,'provider_infrastructure_unavailable',0,'infrastructure_unavailable'),
      ('req_ci_server','http_sse',500,500,'upstream_error',1,'server_error')
    ) rows(id,transport,client_status,upstream_status,kind,attempts,code);
    """)
    account = api('/api/admin/accounts/priority-policy?accountId=acct_ci_policy')
    assert account['autoOffOnQuotaReset'] is None and account['effectiveAutoOffOnQuotaReset'] is True
    assert account['effectiveMode'] == 'enabled'
    account = api('/api/admin/accounts/priority-policy/update', {'accountId':'acct_ci_policy',
        'expectedRevision':account['revision'], 'mode':'disabled', 'autoOffOnQuotaReset':False})
    assert account['effectiveMode'] == 'disabled' and account['effectiveAutoOffOnQuotaReset'] is False
    account = api('/api/admin/accounts/priority-policy/update', {'accountId':'acct_ci_policy',
        'expectedRevision':account['revision'], 'mode':'inherit', 'autoOffOnQuotaReset':None})
    assert account['effectiveMode'] == 'enabled' and account['effectiveAutoOffOnQuotaReset'] is True
    checks.append('account nullable inheritance, explicit off, and persisted re-inheritance')
    summary = api('/api/admin/errors/summary?period=7d')
    expected = {'retainedRequests':3, 'serverRelatedRequests':3, 'terminalServerRelatedRequests':3,
        'overloaded':1, 'status503':2, 'upstream503':1, 'infrastructure503':1,
        'noProvider503':0, 'client200Errors':1}
    assert {key:summary['totals'][key] for key in expected} == expected, summary
    assert summary['attempts']['coverage'] == 'observed_only'
    records = api('/api/admin/usage/records?requestId=req_ci_stream')
    assert records['total'] == 1 and records['items'][0]['id'] == 'req_ci_stream', records
    assert records['items'][0]['errorCode'] == 'server_overloaded', records
    detail = api('/api/admin/usage/records/detail?id=req_ci_stream')
    assert detail['id'] == 'req_ci_stream', detail
    checks.append('actual PostgreSQL-backed overload/503 sources, stream-200 error, exact lookup and detail')
finally:
    sql("delete from model_requests where id in ('req_ci_stream','req_ci_infra','req_ci_server'); delete from provider_accounts where id='acct_ci_policy';")
    api('/api/admin/priority-policy/update', {'expectedRevision':saved['revision'],
        'mode':initial['mode'], 'autoOffOnQuotaReset':initial['autoOffOnQuotaReset']})
    api('/api/auth/logout', {})
receipt = {'source_commit':os.environ.get('GITHUB_SHA'), 'checks':checks,
           'data':'synthetic isolated Compose PostgreSQL; no provider request'}
Path(os.environ['DIAGNOSTICS_DIR'], 'admin-api-smoke.json').write_text(json.dumps(receipt, indent=2))
print(json.dumps(receipt, indent=2))
