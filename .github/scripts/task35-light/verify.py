"""Isolated synthetic business recovery and old45 startup; never uses production data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from urllib.parse import quote
from recover import PG

p=argparse.ArgumentParser()
p.add_argument('--repo',type=Path,required=True)
p.add_argument('--container',required=True)
p.add_argument('--redis',required=True)
p.add_argument('--binary-directory',type=Path,required=True)
p.add_argument('--artifacts',type=Path,required=True)
a=p.parse_args()
a.artifacts.mkdir(parents=True,exist_ok=True)
pg=PG(a.container,'codex_proxy','codex_proxy')
files=sorted((a.repo/'backend/migrations').glob('*.sql'))
pg.sql('create table public._sqlx_migrations(version bigint primary key,description text not null,installed_on timestamptz not null default now(),success boolean not null default true,checksum bytea not null,execution_time bigint not null);')
def apply(files):
    for f in files:
        digest=hashlib.sha384(f.read_bytes()).hexdigest()
        pg.sql('begin;set local search_path to public,pg_catalog;\n'+f.read_text(encoding='utf8')+f"\ninsert into _sqlx_migrations(version,description,checksum,execution_time)values({int(f.name[:4])},'{f.stem}',decode('{digest}','hex'),0);commit;")
apply(files[:32])
pg.sql("""
insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at)
values('acct_light','openai','Synthetic retained','oauth','{}',1,false,false,'ready',now(),now(),now());
insert into client_api_keys(id,name,key,created_at,updated_at)values('key_light','Before','sk_'||repeat('a',43),now(),now());
insert into account_groups(id,name,color,disable_fast,created_at,updated_at)values('grp_11111111111111111111111111111111','Group before','#112233FF',true,now(),now());
insert into account_group_accounts values('grp_11111111111111111111111111111111','acct_light',now());
insert into client_api_key_groups values('key_light','grp_11111111111111111111111111111111',now());
insert into client_key_budget_windows values('key_light',now()-interval '1 hour',now()+interval '23 hours',now()-interval '1 day',now()+interval '6 days',0.1,0.1);
insert into client_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd)values('charged_before','key_light',now(),0.1);
insert into client_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd)
select 'ledger_'||lpad(n::text,8,'0'),'key_light',now(),0 from generate_series(1,735655)n;
insert into codex_quota_policy_accounts(account_id,revision,mode)values('acct_light',3,'inherit');
insert into codex_reset_operations(id,account_id,credential_revision,automatic,episode,trigger_percent,state,result_code)
values('11111111-1111-4111-8111-111111111111','acct_light',1,true,'threshold_secondary:123',99,'failed','synthetic');
insert into codex_quota_policy_audit(account_id,revision,actor_ref)values('acct_light',3,'synthetic');
insert into admin_audit_events(id,actor_kind,actor_ref,action,entity_kind,entity_ref,created_at)
values('audit_old','system','synthetic','synthetic','account','acct_light',now()-interval '100 days');
""")
apply(files[32:])
pg.sql("""
update provider_accounts set credential_revision=2,name='Updated after52',updated_at=now() where id='acct_light';
update client_api_keys set name='Updated after52',enabled=false,updated_at=now() where id='key_light';
update account_groups set name='Updated after52',updated_at=now();
update runtime_settings set config_revision=7,refresh_concurrency=3;
update codex_quota_policy_accounts set revision=4,paused=true,reason='synthetic';
update client_api_key_budget_windows set daily_used_usd=0.3,weekly_used_usd=0.3;
insert into client_api_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd)values('charged_after','key_light',now(),0.2);
insert into codex_priority_accounts(account_id,revision,mode,auto_off_on_quota_reset,reason)values('acct_light',1,'disabled',null,'quota_reset');
insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,provider_account_ref,attempt_count,upstream_send_state,outcome,client_status_code,error_kind,started_at,recovery_lease_expires_at,completed_at,request_observation_json)
values('discarded_history','key_light','responses','http_sse','openai','acct_light',0,'not_sent','failed',503,'provider_infrastructure_unavailable',now(),now()+interval '1 hour',now(),'{"request":{"configRevision":7,"protocol":"openai","endpoint":"/v1/responses","compact":false},"account":{"name":"Synthetic retained","authenticationKind":"oauth"},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]}}');
""")
receipt=a.artifacts/'light-recovery.json'
subprocess.run(['python',str(Path(__file__).with_name('recover.py')),'--container',a.container,
    '--target-schema','cpr_light_rollback','--migrations',str(a.repo/'backend/migrations'),'--receipt',str(receipt),
    '--legacy-request-interval-ms','50','--source-writer-stopped','--discard-request-history',
    '--acknowledge-new-only-state-stays-in-source'],check=True)
r=json.loads(receipt.read_text())
def read(text):
    return pg.sql('set search_path to cpr_light_rollback,pg_catalog;'+text).splitlines()[-1]
assert r['counts']['client_key_charge_events']==735657
assert pg.rows("select count(*) n from public.model_requests")[0]['n']==1
assert read("select credential_revision from provider_accounts where id='acct_light'")=='2'
assert read('select config_revision from runtime_settings')=='7'
assert read('select request_interval_ms from runtime_settings')=='50'
assert read('select revision from codex_quota_policy_accounts')=='4'
assert read('select sum(amount_usd)::text from client_key_charge_events')=='0.3000000000'
assert r['retention_prestart']['audit_retention_days']>=107
# 已扣费request ID再次出现不能新增账本项
pg.sql("set search_path to cpr_light_rollback,pg_catalog;insert into client_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd)values('charged_after','key_light',now(),99)on conflict do nothing;")
assert read('select count(*) from client_key_charge_events')=='735657'
assert read('select sum(amount_usd)::text from client_key_charge_events')=='0.3000000000'

def run(cmd,**kw):
    return subprocess.run(cmd,check=True,capture_output=True,**kw)
network='task35_light_internal'
container='task35_light_old45'
workspace=(a.artifacts/'old45').resolve()
(workspace/'deploy').mkdir(parents=True)
(workspace/'runtime').mkdir()
(workspace/'web').mkdir()
config={'schema_version':1,'host':{'listen':{'host':'0.0.0.0','port':8080},'runtime_data_dir':'/workspace/runtime',
 'logging':{'level':'info','stdout':True,'oauth_recovery':False,'request_dump':False,'request_dump_retention_days':1,
 'file':{'enabled':False,'directory':'/workspace/runtime/logs','retention_days':7,'max_file_size_mb':20}},
 'drain_timeout_seconds':2,'worker_shutdown_timeout_seconds':2},
 'store':{'database':{'url':'postgres://codex_proxy@postgres:5432/codex_proxy?options='+quote('-csearch_path=cpr_light_rollback',safe=''),'password':'a'*48},
 'redis':{'url':'redis://redis:6379/','password':'b'*48}},
 'admin':{'session_ttl_minutes':1440,'default_username':'admin@task35.synthetic','default_password':'task35-synthetic-only'},
 'client':{'session_ttl_minutes':1440},'api':{'asset_directory':'/workspace/web','cors_allowed_origins':[],
 'request_timeout_seconds':None,'request_id_header':'x-request-id'},'openai':{'residency':'us'}}
(workspace/'deploy/config.yaml').write_text(json.dumps(config))
binaries=list(a.binary_directory.rglob('codex-proxy-rs'))
assert len(binaries)==1
binary=binaries[0].resolve()
binary.chmod(0o755)
status=None
began=time.monotonic()
try:
    run(['docker','exec',a.redis,'redis-cli','CONFIG','SET','requirepass','b'*48])
    run(['docker','network','create','--internal',network])
    run(['docker','network','connect','--alias','postgres',network,a.container])
    run(['docker','network','connect','--alias','redis',network,a.redis])
    run(['docker','run','--detach','--name',container,'--network',network,
        '--mount',f'type=bind,source={binary},target=/app/codex-proxy-rs,readonly',
        '--mount',f'type=bind,source={workspace},target=/workspace','--workdir','/workspace',
        '--entrypoint','/app/codex-proxy-rs',
        'postgres:18-bookworm@sha256:d9c83446333daec3f0588cc709adb80c26090b7f9f0f7ec8d43c243385d79818','serve'])
    for _ in range(60):
        result=subprocess.run(['docker','exec',container,'bash','-c',"exec 3<>/dev/tcp/127.0.0.1/8080; printf 'GET /healthz HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n' >&3; head -n 1 <&3"],capture_output=True,text=True)
        if result.returncode==0 and result.stdout.startswith('HTTP/'):
            status=int(result.stdout.split()[1])
            if status==204:
                break
        if run(['docker','inspect','--format','{{.State.Running}}',container],text=True).stdout.strip()!='true':
            break
        time.sleep(1)
    log=run(['docker','logs',container])
    (a.artifacts/'old45-startup.log').write_bytes(log.stdout+log.stderr)
    assert status==204
    assert read('select count(*) from admin_users')=='1'
    assert read('select count(*) from admin_audit_events')=='1'
    assert read('select count(*) from client_key_charge_events')=='735657'
    assert read('select sum(amount_usd)::text from client_key_charge_events')=='0.3000000000'
    assert read('select count(*) from _sqlx_migrations')=='32'
    assert pg.rows('select max(version) n from public._sqlx_migrations')[0]['n']==52
    r['verification']={'old_source':'45f72b3a0e366411cd137b6a8def48df7f88439f','artifact_run':37242026140,
        'healthz':status,'startup_seconds':round(time.monotonic()-began,3),'network':'internal, external egress prohibited',
        'checks':['credential/config/key/group/budget/quota/reset/audit new writes preserved','735657 dedup ledger rows preserved; duplicate insert does not recharge',
            'old32 migrator accepted and old admin bootstrap writer committed','audit prestart retention preserves 100-day event','new52 public remains unchanged; request history deliberately absent only in target']}
    receipt.write_text(json.dumps(r,indent=2))
    print(json.dumps(r['verification'],indent=2))
finally:
    subprocess.run(['docker','rm','--force',container],capture_output=True)
    subprocess.run(['docker','network','disconnect',network,a.container],capture_output=True)
    subprocess.run(['docker','network','disconnect',network,a.redis],capture_output=True)
    subprocess.run(['docker','network','rm',network],capture_output=True)
