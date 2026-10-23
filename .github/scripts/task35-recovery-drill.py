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
args = parser.parse_args()
migrations = sorted((args.repo / "backend/migrations").glob("*.sql"))
prefix = "task35_" + uuid.uuid4().hex[:10]
source, target, archive = prefix + "_source", prefix + "_recovery", prefix + "_archive"
command = ["docker", "exec", "-i", "--env", "PGPASSWORD=codex_proxy", args.container,
           "psql", "-X", "-U", "codex_proxy", "-d", "codex_proxy", "-A", "-t", "-v", "ON_ERROR_STOP=1"]


def sql(text, schema=None):
    result = subprocess.run(command, input=(f'SET search_path TO "{schema}",public;\n' if schema else "") + text,
                            text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return "\n".join(line for line in result.stdout.splitlines() if line not in {"SET", "BEGIN", "COMMIT"})


def query(text, schema=source):
    return json.loads(sql("select coalesce(jsonb_agg(to_jsonb(q)),'[]'::jsonb) from (" + text + ") q;", schema).splitlines()[-1])


def apply(schema, files):
    for path in files:
        version = int(path.name[:4])
        checksum = hashlib.sha384(path.read_bytes()).hexdigest()  # sqlx migration identity is SHA-384
        sql("begin;\n" + path.read_text(encoding="utf-8") +
            f"\ninsert into _sqlx_migrations(version,description,checksum,execution_time) values({version},'{path.stem}',decode('{checksum}','hex'),0);\ncommit;", schema)


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
old_tables = query("select tablename from pg_tables where schemaname=current_schema() and tablename<>'_sqlx_migrations'")
sql(f'create table "{archive}".before_runtime_settings as table "{source}".runtime_settings;')
apply(source, [p for p in migrations if int(p.name[:4]) > 32])
upgrade_seconds = time.monotonic() - started
assert query("select credential_revision from provider_accounts where id='acct_bridge'")[0]["credential_revision"] == 1
assert query("select provider_error_code from model_request_observations where id='req_before'")[0]["provider_error_code"] == "infrastructure_unavailable"
assert query("select request_id from upgrade_retention_protected_requests")[0]["request_id"] == "req_before"
sql("""
update provider_accounts set credential_revision=2,updated_at=now() where id='acct_bridge';
update codex_quota_policy_accounts set revision=4 where account_id='acct_bridge';
update client_api_keys set name='Updated after upgrade',enabled=false,updated_at=now() where id='key_bridge';
update account_groups set name='Updated after upgrade',updated_at=now();
update client_api_key_budget_windows set daily_used_usd=0.3,weekly_used_usd=0.3;
insert into client_api_key_charge_events(request_id,client_api_key_id,completed_at,amount_usd) values('req_after','key_bridge',now(),0.2);
insert into codex_priority_accounts(account_id,revision,mode,auto_off_on_quota_reset,reason) values('acct_bridge',1,'disabled',null,'quota_reset');
insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,provider_account_ref,attempt_count,upstream_send_state,outcome,client_status_code,upstream_status_code,input_tokens,output_tokens,total_tokens,cost_source,cost_amount,cost_currency,started_at,recovery_lease_expires_at,completed_at,request_observation_json)
values('req_after','key_bridge','responses','http_sse','openai','acct_bridge',1,'sent','succeeded',200,200,100,20,120,'calculated',0.2,'USD',now(),now()+interval '1 hour',now(),'{"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"account":{"name":"Synthetic bridge","authenticationKind":"oauth"},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]},"timings":{"local":{"latencyMs":100}}}');
""", source)
# The same predicate as the real retention adapter protects the old failed request and its event
sql("delete from model_requests where outcome<>'running' and not exists(select 1 from upgrade_retention_protected_requests p where p.request_id=model_requests.id) and completed_at<now()-interval '31 days';", source)
assert query("select id from model_requests order by id") == [{"id":"req_after"},{"id":"req_before"}]
assert query("select id from ops_events")[0]["id"] == "event_before"
# Archive every new-version table before projecting the compatible fields into a fresh old schema
for row in query("select tablename from pg_tables where schemaname=current_schema()"):
    table = row["tablename"]
    sql(f'create table "{archive}"."{table}" as table "{source}"."{table}";')
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
assert query("select id,protocol,endpoint from model_requests order by id", target) == [{"id":"req_after","protocol":"openai","endpoint":"/v1/responses"},{"id":"req_before","protocol":"openai","endpoint":"/v1/responses"}]
assert query("select count(*) n from codex_reset_operations", target)[0]["n"] == 1
assert query("select mode from codex_priority_accounts", archive)[0]["mode"] == "disabled"
print(json.dumps({"source_commit":os.environ.get("GITHUB_SHA"),"source_schema":source,"recovery_schema":target,"archive_schema":archive,"migration_versions":[1,len(migrations)],"old_migration_versions":[1,32],"upgrade_seconds":round(upgrade_seconds,3),"recovery_table_counts":counts,"checks":["old failures protected","new request retained","credential revision 2 retained","quota policy revision 4 retained","ledger 0.30 retained","reset operation retained","old migration identity retained","all compatible fields equal via bidirectional EXCEPT ALL","new-only facts archived"]},indent=2))
