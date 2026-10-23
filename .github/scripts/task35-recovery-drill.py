"""One-time Task35 synthetic PG rehearsal; writes only newly created isolated schemas."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import subprocess
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--repo", type=Path, required=True)
parser.add_argument("--container", required=True)
parser.add_argument("--artifacts", type=Path, required=True)
args = parser.parse_args()
args.artifacts.mkdir(parents=True, exist_ok=True)
migrations = sorted((args.repo / "backend/migrations").glob("*.sql"))
prefix = "task35_" + uuid.uuid4().hex[:10]
source, target, archive = prefix + "_source", prefix + "_recovery", prefix + "_archive"
command = ["docker", "exec", "-i", "--env", "PGPASSWORD=codex_proxy", args.container,
           "psql", "-X", "-U", "codex_proxy", "-d", "codex_proxy", "-A", "-t", "-v", "ON_ERROR_STOP=1"]


def sql(text, schema=None, database="codex_proxy"):
    selected = command.copy()
    selected[selected.index("-d") + 1] = database
    result = subprocess.run(selected, input=(f'SET search_path TO "{schema}",public;\n' if schema else "") + text,
                            text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return "\n".join(line for line in result.stdout.splitlines() if line not in {"SET", "BEGIN", "COMMIT"})


def query(text, schema=source, database="codex_proxy"):
    return json.loads(sql("select coalesce(jsonb_agg(to_jsonb(q)),'[]'::jsonb) from (" + text + ") q;", schema, database).splitlines()[-1])


migration_receipts = []
phases = {}


def apply(schema, files):
    for path in files:
        version = int(path.name[:4])
        checksum = hashlib.sha384(path.read_bytes()).hexdigest()  # sqlx migration identity is SHA-384
        began = time.monotonic()
        output = sql("begin; set local lock_timeout='1500ms'; set local statement_timeout='180s';\n" + path.read_text(encoding="utf-8") +
            f"\ninsert into _sqlx_migrations(version,description,checksum,execution_time) values({version},'{path.stem}',decode('{checksum}','hex'),0);\n" +
            "select 'TASK35_LOCKS '||coalesce(jsonb_agg(to_jsonb(q)),'[]'::jsonb)::text from (select c.relname relation,l.mode from pg_locks l join pg_class c on c.oid=l.relation join pg_namespace n on n.oid=c.relnamespace where l.pid=pg_backend_pid() and n.nspname=current_schema() and l.granted group by c.relname,l.mode)q;\ncommit;", schema)
        locks = json.loads(next(line.removeprefix('TASK35_LOCKS ') for line in output.splitlines() if line.startswith('TASK35_LOCKS ')))
        migration_receipts.append({"schema":schema,"version":version,"file":path.name,"transaction_seconds":round(time.monotonic()-began,3),"lock_wait_limit_ms":1500,"locks_held_before_commit":locks})


for schema in [source, target, archive]:
    sql(f'create schema "{schema}";')
for schema in [source, target]:
    sql("create table _sqlx_migrations(version bigint primary key,description text not null,installed_on timestamptz not null default now(),success boolean not null default true,checksum bytea not null,execution_time bigint not null);", schema)
started = time.monotonic()
apply(source, [p for p in migrations if int(p.name[:4]) <= 32])
sql("""
insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at)
values('acct_bridge','openai','Synthetic bridge','oauth','{}',1,false,true,'ready',now(),now(),now());
insert into client_api_keys(id,name,key,created_at,updated_at) values('key_bridge','Synthetic key','sk_' || repeat('a',43),now(),now());
insert into account_groups(id,name,color,disable_fast,created_at,updated_at) values('grp_11111111111111111111111111111111','Synthetic group','#112233FF',true,now(),now());
insert into account_group_accounts values('grp_11111111111111111111111111111111','acct_bridge',now());
insert into client_key_budget_windows values('key_bridge',now()-interval '1 hour',now()+interval '23 hours',now()-interval '1 day',now()+interval '6 days',0.1,0.1);
insert into client_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd) values('req_before','key_bridge',now()-interval '40 days',0.1);
insert into codex_quota_policy_accounts(account_id,revision,mode) values('acct_bridge',3,'inherit');
insert into codex_reset_operations(id,account_id,credential_revision,automatic,episode,trigger_percent,state,result_code)
values('11111111-1111-4111-8111-111111111111','acct_bridge',1,true,'threshold_secondary:123',99,'failed','synthetic');
insert into codex_quota_policy_audit(account_id,revision,actor_ref) values('acct_bridge',3,'synthetic');
insert into model_requests(id,client_api_key_ref,config_revision,protocol,operation,endpoint,client_transport,provider_kind,provider_account_ref,provider_account_name_snapshot,provider_account_authentication_kind_snapshot,attempt_count,upstream_send_state,outcome,client_status_code,error_kind,provider_error_code,error_message,started_at,deadline_at,completed_at,routing_scope)
values('req_before','key_bridge',1,'openai','responses','/v1/responses','http_sse','openai','acct_bridge','Synthetic bridge','oauth',0,'not_sent','failed',503,'provider_infrastructure_unavailable','infrastructure_unavailable','synthetic old failure',now()-interval '40 days',now()-interval '40 days'+interval '1 hour',now()-interval '40 days'+interval '1 minute','all');
insert into ops_events(id,model_request_id,attempt_index,level,component,operation,failure_kind,message,created_at)
values('event_before','req_before',1,'error','synthetic','responses','provider_infrastructure_unavailable','synthetic',now()-interval '40 days');
""", source)
# 只使用合成宽值模拟父提供的行数与物理量级，不携带真实请求或凭据
phases["old_schema_and_seed_seconds"] = round(time.monotonic()-started,3)
began = time.monotonic()
sql("""
alter table model_requests alter column provider_observation_json set storage external;
with padding as materialized (select jsonb_build_object('syntheticPadding',string_agg(md5(n::text),'')) payload from generate_series(1,576)n)
insert into model_requests select cloned.* from model_requests original cross join generate_series(1,57744)n cross join padding
cross join lateral jsonb_populate_record(null::model_requests,to_jsonb(original)||jsonb_build_object(
'id','req_scale_'||lpad(n::text,6,'0'),'provider_observation_json',payload,
'started_at',now()-interval '1 day'+n*interval '1 second','deadline_at',now()+interval '1 hour',
'completed_at',now()-interval '1 day'+n*interval '1 second'+interval '1 minute',
'outcome',case when n%100=0 then 'failed' else 'succeeded' end,
'client_status_code',case when n%100=0 then 503 else 200 end,
'error_kind',case when n%100=0 then 'provider_infrastructure_unavailable' else null end,
'provider_error_code',case when n%100=0 then 'infrastructure_unavailable' else null end,
'error_message',case when n%100=0 then 'synthetic old failure' else null end)) cloned where original.id='req_before';
with padding as materialized (select string_agg(md5(n::text),'') value from generate_series(1,40)n)
insert into request_turn_state_observations(request_id,attempt_index,observation_id,value,observed_at,source)
select 'req_scale_'||lpad(((n-1)%57744+1)::text,6,'0'),((n-1)/57744+1)::integer,'obs_'||n,p.value,now(),'http'
from generate_series(1,605149)n cross join padding p;
with padding as materialized (select string_agg(md5(n::text),'') value from generate_series(1,37)n)
insert into client_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd)
select 'ledger_'||n||'_'||p.value,'key_bridge',now(),0 from generate_series(1,54894)n cross join padding p;
analyze model_requests; analyze request_turn_state_observations; analyze client_key_charge_events;
""", source)
phases["scale_seed_seconds"] = round(time.monotonic()-began,3)
shape = query("select c.relname table_name,c.reltuples::bigint estimated_rows,pg_total_relation_size(c.oid) physical_bytes from pg_class c join pg_namespace n on n.oid=c.relnamespace where n.nspname=current_schema() and c.relname in ('model_requests','request_turn_state_observations','client_key_charge_events')")
old_tables = query("select tablename from pg_tables where schemaname=current_schema() and tablename<>'_sqlx_migrations'")
sql(f'create table "{archive}".before_runtime_settings as table "{source}".runtime_settings;')
began = time.monotonic()
apply(source, [p for p in migrations if int(p.name[:4]) > 32])
upgrade_seconds = time.monotonic()-began
phases["upgrade_32_to_52_seconds"] = round(upgrade_seconds,3)
assert query("select credential_revision from provider_accounts where id='acct_bridge'")[0]["credential_revision"] == 1
assert query("select provider_error_code from model_request_observations where id='req_before'")[0]["provider_error_code"] == "infrastructure_unavailable"
assert query("select request_id from upgrade_retention_protected_requests where request_id='req_before'")[0]["request_id"] == "req_before"
sql("""
update provider_accounts set credential_revision=2,updated_at=now() where id='acct_bridge';
update codex_quota_policy_accounts set revision=4 where account_id='acct_bridge';
update client_api_keys set name='Updated after upgrade',enabled=false,updated_at=now() where id='key_bridge';
update account_groups set name='Updated after upgrade',updated_at=now();
update client_api_key_budget_windows set daily_used_usd=0.3,weekly_used_usd=0.3;
insert into client_api_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd) values('req_after','key_bridge',now(),0.2);
insert into codex_priority_accounts(account_id,revision,mode,auto_off_on_quota_reset,reason) values('acct_bridge',1,'disabled',null,'quota_reset');
insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,provider_account_ref,upstream_transport,attempt_count,upstream_send_state,outcome,client_status_code,upstream_status_code,input_tokens,output_tokens,total_tokens,cost_source,cost_amount,cost_currency,started_at,recovery_lease_expires_at,completed_at,request_observation_json)
values('req_after','key_bridge','responses','http_sse','openai','acct_bridge','http_sse',1,'sent','succeeded',200,200,100,20,120,'calculated',0.2,'USD',now(),now()+interval '1 hour',now(),'{"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"account":{"name":"Synthetic bridge","authenticationKind":"oauth"},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]},"timings":{"local":{"latencyMs":100}}}');
""", source)
# The same predicate as the real retention adapter protects the old failed request and its event
sql("delete from model_requests where outcome<>'running' and not exists(select 1 from upgrade_retention_protected_requests p where p.request_id=model_requests.id) and completed_at<now()-interval '31 days';", source)
assert query("select count(*) n from model_requests")[0]["n"] == 57746
assert query("select id from ops_events")[0]["id"] == "event_before"
# 源库无其他 writer；正式恢复必须在停写边界执行，不能逐表复制并发变化的账本
began = time.monotonic()
for row in query("select tablename from pg_tables where schemaname=current_schema()"):
    table = row["tablename"]
    sql(f'create table "{archive}"."{table}" as table "{source}"."{table}";')
phases["archive_seconds"] = round(time.monotonic()-began,3)
began = time.monotonic()
apply(target, [p for p in migrations if int(p.name[:4]) <= 32])
tables = [row["tablename"] for row in old_tables]
sql("truncate " + ",".join('"'+t+'"' for t in tables) + " cascade;", target)
renames = {"client_key_budget_windows":"client_api_key_budget_windows", "client_key_charge_events":"client_api_key_charge_events"}
model_aliases = {"raw_upstream_error":"s.error_details", "deadline_at":"coalesce(s.effective_recovery_lease_expires_at,s.started_at)", "cached_tokens":"s.cached_input_tokens", "cache_write_tokens":"s.cache_write_input_tokens", "reasoning_tokens":"s.reasoning_output_tokens", "client_api_key_id":f'(select id from "{source}".client_api_keys where id=s.client_api_key_ref)', "provider_account_id":f'(select id from "{source}".provider_accounts where id=s.provider_account_ref)'}
fks = query("select child.relname child,parent.relname parent from pg_constraint c join pg_class child on child.oid=c.conrelid join pg_class parent on parent.oid=c.confrelid join pg_namespace n on n.oid=child.relnamespace where c.contype='f' and n.nspname=current_schema()", target)
remaining, counts = set(tables), {}
while remaining:
    available = sorted(t for t in remaining if not any(f["child"]==t and f["parent"] in remaining and f["parent"]!=t for f in fks))
    if not available:
        raise RuntimeError("cyclic target dependencies: " + repr(remaining))
    for table in available:
        source_table = renames.get(table, table)
        if table == "model_requests":
            source_table = "model_request_observations"
        columns = query("select column_name,column_default,is_nullable from information_schema.columns where table_schema=current_schema() and table_name='"+table+"' order by ordinal_position", target)
        source_columns = {row["column_name"] for row in query("select column_name from information_schema.columns where table_schema=current_schema() and table_name='"+source_table+"'")}
        aliases = dict(model_aliases) if table == "model_requests" else {}
        if table == "ops_events":
            aliases = {"raw_upstream_error":"s.error_details", "provider_account_id":f'(select id from "{source}".provider_accounts where id=s.provider_account_ref)'}
        if table == "account_groups":
            aliases = {"disable_fast":"(s.fast_mode='disabled')"}
        if table == "runtime_settings":
            aliases = {"request_interval_ms":f'(select request_interval_ms from "{archive}".before_runtime_settings b where b.id=s.id)'}
        selected = [(c["column_name"], aliases.get(c["column_name"], 's."'+c["column_name"]+'"')) for c in columns if c["column_name"] in aliases or c["column_name"] in source_columns]
        missing = [c["column_name"] for c in columns if c["column_name"] not in {v[0] for v in selected} and c["column_default"] is None and c["is_nullable"] == "NO"]
        if missing:
            raise RuntimeError(f"unmapped required target fields {table}: {missing}")
        names = ','.join('"'+c+'"' for c,_ in selected)
        projection = 'select ' + ','.join(v for _,v in selected) + f' from "{source}"."{source_table}" s'
        sql(f'insert into "{table}" ({names}) overriding system value ' + projection + ';', target)
        difference = query(f'select count(*) n from (({projection} except all select {names} from "{table}") union all (select {names} from "{table}" except all {projection})) differences', target)[0]["n"]
        assert difference == 0, (table, difference)
        counts[table] = query(f'select count(*) n from "{table}"', target)[0]["n"]
        remaining.remove(table)
# Audit IDs were copied explicitly; advance sequences before the old writer resumes
for row in query("select table_name,column_name from information_schema.columns where table_schema=current_schema() and (column_default like 'nextval(%' or is_identity='YES')", target):
    table, column = row["table_name"], row["column_name"]
    sql(f"select setval(pg_get_serial_sequence('{target}.{table}','{column}'),coalesce(max(\"{column}\"),1),count(*)>0) from \"{table}\";", target)
assert query("select count(*) n from _sqlx_migrations", target)[0]["n"] == 32
assert query("select credential_revision from provider_accounts where id='acct_bridge'", target)[0]["credential_revision"] == 2
assert query("select revision from codex_quota_policy_accounts where account_id='acct_bridge'", target)[0]["revision"] == 4
assert query("select sum(amount_usd)::text amount from client_key_charge_events", target)[0]["amount"] == "0.3000000000"
phases["reverse_bridge_and_comparison_seconds"] = round(time.monotonic()-began,3)
assert query("select id,protocol,endpoint from model_requests where id in ('req_after','req_before') order by id", target) == [{"id":"req_after","protocol":"openai","endpoint":"/v1/responses"},{"id":"req_before","protocol":"openai","endpoint":"/v1/responses"}]
assert counts["model_requests"] == 57746
assert counts["request_turn_state_observations"] == 605149
assert counts["client_key_charge_events"] == 54896
assert query("select count(*) n from codex_reset_operations", target)[0]["n"] == 1
assert query("select mode from codex_priority_accounts", archive)[0]["mode"] == "disabled"
# 保存实际可独立还原的归档，再向另一数据库还原，不能把同库投影当作备份恢复
recovery_database = prefix + "_restored"
docker = ["docker", "exec", "-i", "--env", "PGPASSWORD=codex_proxy", args.container]
began = time.monotonic()
dump = subprocess.run(docker + ["pg_dump", "-U", "codex_proxy", "-d", "codex_proxy", "-Fc", "--no-owner", "--schema", target, "--schema", archive], capture_output=True, check=True).stdout
dump_path = args.artifacts / "synthetic-recovery.dump"
dump_path.write_bytes(dump)
phases["dump_seconds"] = round(time.monotonic()-began,3)
sql(f'create database "{recovery_database}";')
began = time.monotonic()
subprocess.run(docker + ["pg_restore", "-U", "codex_proxy", "-d", recovery_database, "--exit-on-error", "--no-owner"], input=dump, capture_output=True, check=True)
phases["independent_restore_seconds"] = round(time.monotonic()-began,3)
# 按原主键排序流式比较全部值，不把 GiB 级表聚合进单个 JSON 或重复计算摘要
began = time.monotonic()
restored_tables = {}
for schema in [target, archive]:
    rows = query("select tablename from pg_tables where schemaname=current_schema()", schema)
    for row in rows:
        table = row["tablename"]
        key_schema = source if schema == archive else target
        keys = [] if table == 'before_runtime_settings' else query(f"select a.attname from pg_index i cross join lateral unnest(i.indkey) with ordinality k(num,position) join pg_attribute a on a.attrelid=i.indrelid and a.attnum=k.num where i.indrelid='\"{key_schema}\".\"{table}\"'::regclass and i.indisprimary order by k.position", key_schema)
        order = ','.join('t.\"'+key['attname']+'\"' for key in keys) if keys else 'to_jsonb(t)::text'
        copy = f'copy (select row_to_json(t)::text from "{schema}"."{table}" t order by {order}) to stdout'
        streams = []
        for database in ['codex_proxy', recovery_database]:
            selected = command.copy()
            selected[selected.index('-d')+1] = database
            streams.append(subprocess.Popen(selected+['-c',copy],stdout=subprocess.PIPE,stderr=subprocess.PIPE))
        while True:
            left, right = (stream.stdout.read(262144) for stream in streams)
            assert left == right, (schema,table,'restored values differ')
            if not left:
                break
        for stream in streams:
            error = stream.stderr.read()
            assert stream.wait() == 0, (schema,table,error.decode())
        restored_tables[schema+'.'+table] = query(f'select count(*) n from "{table}"',schema)[0]['n']
phases['restored_value_comparison_seconds'] = round(time.monotonic()-began,3)
receipt = {"source_commit":os.environ.get("GITHUB_SHA"),"source_schema":source,"recovery_schema":target,"archive_schema":archive,"restored_database":recovery_database,"dump_file":dump_path.name,"dump_bytes":len(dump),"restored_tables":restored_tables,"migration_versions":[1,len(migrations)],"old_migration_versions":[1,32],"upgrade_seconds":round(upgrade_seconds,3),"recovery_table_counts":counts,"checks":["old failures protected","new request retained","credential revision 2 retained","quota policy revision 4 retained","ledger 0.30 retained","reset operation retained","old migration identity retained","all compatible fields equal via bidirectional EXCEPT ALL","new-only facts archived","pg_dump custom archive restored into independent database","all restored old-compatible and new-only table values equal"]}
receipt.update({"synthetic_old_shape":shape,"phase_seconds":phases,"migration_transactions":migration_receipts,"lock_contract":"isolated no-writer rehearsal; locks held before each commit recorded; transaction duration is an upper bound on lock hold, not production wait; lock_timeout=1500ms, statement_timeout=180s"})
(args.artifacts / "task35-recovery.json").write_text(json.dumps(receipt,indent=2),encoding="utf-8")
print(json.dumps(receipt,indent=2))
