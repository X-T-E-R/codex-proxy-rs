-- 大表可先在线并发预建同名索引，启动事务仅校验并登记
-- 未预建时仍保留单次语句和锁等待预算，失败不登记迁移
set local statement_timeout = '5s';
set local lock_timeout = '500ms';

do $$
begin
    if to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema())) is null then
        create index model_requests_successful_trace_pending_idx
            on model_requests using btree (completed_at)
            where outcome = 'succeeded' and diagnostic_trace_json is not null;
    end if;

    -- IF NOT EXISTS 无法辨别并发建索引失败残留或同名异义对象，必须拒绝这些状态
    if not exists (
        select 1
        from pg_index i
        join pg_class c on c.oid = i.indexrelid
        join pg_am am on am.oid = c.relam
        join pg_attribute a on a.attrelid = i.indrelid and a.attname = 'completed_at'
        join pg_opclass op on op.oid = i.indclass[0]
        join pg_namespace op_namespace on op_namespace.oid = op.opcnamespace
        where i.indexrelid = to_regclass(format('%I.model_requests_successful_trace_pending_idx', current_schema()))
          and i.indrelid = 'model_requests'::regclass
          and i.indisvalid and i.indisready and i.indislive
          and not i.indisunique and not i.indisprimary and not i.indisexclusion
          and am.amname = 'btree'
          and i.indnkeyatts = 1 and i.indnatts = 1
          and i.indkey[0] = a.attnum and i.indoption[0] = 0
          and op_namespace.nspname = 'pg_catalog' and op.opcname = 'timestamptz_ops'
          and i.indexprs is null
          and pg_get_expr(i.indpred, i.indrelid) = '((outcome = ''succeeded''::text) AND (diagnostic_trace_json IS NOT NULL))'
    ) then
        raise exception 'successful trace pending index is invalid or has an unexpected definition'
            using hint = 'Inspect the named index; remove only a confirmed failed or incorrect build and prebuild it concurrently before retrying startup';
    end if;
end
$$;
