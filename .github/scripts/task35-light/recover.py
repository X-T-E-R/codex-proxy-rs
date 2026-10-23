"""Recover business state from the pinned 52 schema into a fresh old32 schema in the same PG database."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

HISTORY = {'model_requests', 'ops_events', 'request_turn_state_observations'}
RENAMES = {'client_key_budget_windows':'client_api_key_budget_windows',
           'client_key_charge_events':'client_api_key_charge_events'}


def identifier(value):
    if not re.fullmatch(r'[a-z][a-z0-9_]{0,62}', value):
        raise ValueError('invalid SQL identifier')
    return '"' + value + '"'


class PG:
    def __init__(self, container, database, user):
        self.command = ['docker','exec','-i','--env','PGPASSWORD',container,
            'psql','-X','-q','-A','-t','-U',user,'-d',database,'-v','ON_ERROR_STOP=1',
            '-v','VERBOSITY=sqlstate']

    def sql(self, text):
        result = subprocess.run(self.command, input=text, text=True, capture_output=True)
        if result.returncode:
            code = re.search(r'ERROR:\s+([0-9A-Z]{5})\b', result.stderr)
            raise RuntimeError('PostgreSQL operation failed; SQLSTATE=' + (code[1] if code else 'unknown'))
        return result.stdout.strip()

    def rows(self, text):
        return json.loads(self.sql("select coalesce(jsonb_agg(to_jsonb(q)),'[]'::jsonb) from ("+text+")q;").splitlines()[-1])


def recover(args):
    began = time.monotonic()
    pg = PG(args.container, args.database, args.user)
    source, target = identifier(args.source_schema), identifier(args.target_schema)
    if args.source_schema == args.target_schema or args.target_schema in {'public','pg_catalog','information_schema'}:
        raise ValueError('target must be a fresh isolated non-public schema')
    files = sorted(args.migrations.glob('*.sql'))
    if [int(p.name[:4]) for p in files] != list(range(1,53)):
        raise ValueError('requires the frozen 25fc migration set 1..52')
    actual = pg.rows(f'select version,success,encode(checksum,\'hex\') checksum from {source}._sqlx_migrations order by version')
    expected = [{'version':int(p.name[:4]),'success':True,'checksum':hashlib.sha384(p.read_bytes()).hexdigest()} for p in files]
    if actual != expected:
        raise ValueError('source migration identity differs from admitted 25fc 1..52')
    if pg.rows("select 1 from pg_namespace where nspname='"+args.target_schema+"'"):
        raise ValueError('target schema already exists; never reuse or overwrite it')
    source_tables = {r['tablename'] for r in pg.rows("select tablename from pg_tables where schemaname='"+args.source_schema+"'")}
    # 源只读，旧schema内的seed清理由完整表清单显式执行，不使用CASCADE
    pg.sql(f'create schema {target}; set search_path to {target},pg_catalog;\n'
        'create table _sqlx_migrations(version bigint primary key,description text not null,installed_on timestamptz not null default now(),success boolean not null default true,checksum bytea not null,execution_time bigint not null);')
    for path in files[:32]:
        checksum = hashlib.sha384(path.read_bytes()).hexdigest()
        pg.sql(f'begin; set local search_path to {target},pg_catalog; set local lock_timeout=\'1500ms\'; set local statement_timeout=\'180s\';\n'
            +path.read_text(encoding='utf-8')+f"\ninsert into _sqlx_migrations(version,description,checksum,execution_time) values({int(path.name[:4])},'{path.stem}',decode('{checksum}','hex'),0); commit;")
    old_tables = {r['tablename'] for r in pg.rows("select tablename from pg_tables where schemaname='"+args.target_schema+"' and tablename<>'_sqlx_migrations'")}
    business = old_tables - HISTORY
    missing_tables = {RENAMES.get(t,t) for t in business} - source_tables
    if missing_tables:
        raise ValueError('missing business source tables: '+repr(sorted(missing_tables)))
    fk = pg.rows(f"select child.relname child,parent.relname parent from pg_constraint c join pg_class child on child.oid=c.conrelid join pg_class parent on parent.oid=c.confrelid join pg_namespace n on n.oid=child.relnamespace where c.contype='f' and n.nspname='{args.target_schema}'")
    mappings = []
    remaining = set(business)
    consumed = {'_sqlx_migrations'} | HISTORY
    def columns(schema, table):
        return [r['column_name'] for r in pg.rows(f"select column_name from information_schema.columns where table_schema='{schema}' and table_name='{table}' order by ordinal_position")]
    while remaining:
        available = sorted(t for t in remaining if not any(f['child']==t and f['parent'] in remaining and f['parent']!=t for f in fk))
        if not available:
            raise ValueError('unresolved business foreign-key dependencies')
        for table in available:
            st = RENAMES.get(table,table)
            old_cols, new_cols = columns(args.target_schema,table), columns(args.source_schema,st)
            aliases = {}
            if table == 'account_groups':
                aliases['disable_fast'] = "(s.fast_mode='disabled')"
            if table == 'runtime_settings':
                aliases['request_interval_ms'] = str(args.legacy_request_interval_ms)
            absent = set(old_cols)-set(new_cols)-set(aliases)
            if absent:
                raise ValueError(f'unmapped old business columns {table}: {sorted(absent)}')
            expressions = [aliases.get(c,'s.'+identifier(c)) for c in old_cols]
            extras = sorted(set(new_cols)-set(old_cols)-({'fast_mode'} if table=='account_groups' else set()))
            mappings.append({'table':table,'source_table':st,'columns':old_cols,'expressions':expressions,'source_only_columns_retained_in_original':extras})
            consumed.add(st)
            remaining.remove(table)
    new_only = sorted(source_tables-consumed)
    omissions = [{'table':t,'rows':pg.rows(f'select count(*) n from {source}.{identifier(t)}')[0]['n']} for t in new_only if not t.startswith('request_analytics') and t not in {'model_request_leases','upgrade_retention_protected_requests'}]
    metadata = pg.rows(f"select c.relname table_name,pg_total_relation_size(c.oid) physical_bytes from pg_class c join pg_namespace n on n.oid=c.relnamespace where n.nspname='{args.source_schema}' and c.relkind='r'")
    copy_bytes = sum(r['physical_bytes'] for r in metadata if r['table_name'] in {m['source_table'] for m in mappings})
    plan = {'source_database':args.database,'source_schema':args.source_schema,'target_schema':args.target_schema,
        'old_readable_tables':[m['table'] for m in mappings], 'history_discarded':sorted(HISTORY),
        'new_only_business_tables_retained_in_original':omissions,
        'source_only_business_columns_retained_in_original':{m['table']:m['source_only_columns_retained_in_original'] for m in mappings if m['source_only_columns_retained_in_original']},
        'explicit_legacy_request_interval_ms':args.legacy_request_interval_ms,
        'source_business_physical_bytes':copy_bytes,
        'old_semantic_limits':['old group disable_fast cannot force enabled; disabled prohibition maps exactly',
            'old binary does not interpret new priority/plugin/reset-credit policy extensions; original new52 schema remains intact',
            '48 removed smart_scheduling_json.preferHigherWeight; current new52 JSON is retained as stored, not an invented previous value']}
    args.receipt.parent.mkdir(parents=True,exist_ok=True)
    args.receipt.write_text(json.dumps({'status':'prepared_not_ready','plan':plan},indent=2),encoding='utf-8')
    if copy_bytes > args.max_business_bytes:
        raise ValueError('source business bytes exceed explicit copy budget; prepared target remains empty')
    # 单事务持源业务SHARE锁，阻止偷跑writer；重复读保证全部复制和比较看到同一快照
    statements = [f'begin isolation level repeatable read; set local search_path to {target},pg_catalog;',
        "set local lock_timeout='1500ms'; set local statement_timeout='180s';",
        'lock table '+','.join(f'{source}.{identifier(m["source_table"])}' for m in mappings)+' in share mode;',
        'truncate '+','.join(f'{target}.{identifier(t)}' for t in sorted(old_tables))+';']
    for m in mappings:
        names = ','.join(identifier(c) for c in m['columns'])
        projection = 'select '+','.join(m['expressions'])+f' from {source}.{identifier(m["source_table"])} s'
        statements.append(f'insert into {target}.{identifier(m["table"])} ({names}) overriding system value {projection};')
        statements.append(f"do $$ begin if exists (({projection} except all select {names} from {target}.{identifier(m['table'])}) union all (select {names} from {target}.{identifier(m['table'])} except all {projection})) then raise exception 'business projection mismatch' using errcode='P0001'; end if; end $$;")
    statements += [f'''update {target}.runtime_settings set
 usage_retention_days=greatest(usage_retention_days,31),
 ops_event_retention_days=greatest(ops_event_retention_days,(select ceil(extract(epoch from (now()-min(created_at)))/86400)::bigint+7 from {target}.ops_events)),
 audit_retention_days=greatest(audit_retention_days,(select ceil(extract(epoch from (now()-min(created_at)))/86400)::bigint+7 from {target}.admin_audit_events));''', 'commit;']
    # 未复制history时usage/ops最老记录为空；保留旧writer prestart gate覆盖审计现存年龄
    pg.sql('\n'.join(statements))
    for r in pg.rows(f"select table_name,column_name from information_schema.columns where table_schema='{args.target_schema}' and (column_default like 'nextval(%' or is_identity='YES')"):
        t,c=identifier(r['table_name']),identifier(r['column_name'])
        pg.sql(f"select setval(pg_get_serial_sequence('{target}.{t}','{r['column_name']}'),coalesce(max({c}),1),count(*)>0) from {target}.{t};")
    counts = {t:pg.rows(f'select count(*) n from {target}.{identifier(t)}')[0]['n'] for t in sorted(old_tables)}
    assert all(counts[t]==0 for t in HISTORY)
    # receipt只保存计数/结构，不输出账号、凭据、Key或账本正文
    receipt = {'status':'ready_old32_business_only','plan':plan,'counts':counts,
        'target_physical_bytes':pg.rows(f"select pg_total_relation_size(c.oid) bytes from pg_class c join pg_namespace n on n.oid=c.relnamespace where n.nspname='{args.target_schema}' and c.relkind='r'"),
        'retention_prestart':pg.rows(f'select usage_retention_days,ops_event_retention_days,audit_retention_days from {target}.runtime_settings')[0],
        'seconds':round(time.monotonic()-began,3),'source_unchanged':True,
        'old_database_url_suffix':'?options=-csearch_path%3D'+args.target_schema,
        'checks':['all old business columns explicitly mapped','all compatible business values equal via bidirectional EXCEPT ALL',
            'old sqlx32 checksums populated','audit sequences advanced','history empty','retention prestart applied'],
        'activation':'keep new writer stopped; use old45 binary/image, original old45 config/runtime identity, change only database URL search_path to target; use a fresh isolated Redis service; do not restart new52 against target'}
    args.receipt.write_text(json.dumps(receipt,indent=2),encoding='utf-8')
    print(json.dumps(receipt,indent=2))
    return receipt


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--container',required=True)
    p.add_argument('--database',default='codex_proxy')
    p.add_argument('--user',default='codex_proxy')
    p.add_argument('--source-schema',default='public')
    p.add_argument('--target-schema',required=True)
    p.add_argument('--migrations',type=Path,required=True)
    p.add_argument('--receipt',type=Path,required=True)
    p.add_argument('--legacy-request-interval-ms',type=int,required=True)
    p.add_argument('--max-business-bytes',type=int,default=1073741824)
    p.add_argument('--source-writer-stopped',action='store_true',required=True)
    p.add_argument('--discard-request-history',action='store_true',required=True)
    p.add_argument('--acknowledge-new-only-state-stays-in-source',action='store_true',required=True)
    args=p.parse_args()
    if args.legacy_request_interval_ms < 0 or args.max_business_bytes<=0:
        p.error('interval must be nonnegative and copy budget positive')
    recover(args)


if __name__=='__main__':
    main()
