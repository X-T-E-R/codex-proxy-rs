//! 有界保留记录上的服务端错误族计数，不读取请求正文或完整 trace

use super::super::*;
use gateway_admin::model::server_errors::ServerErrorSummary;

const SUMMARY: &str = r#"
with base as materialized (
  select id,started_at,attempt_count,error_kind,client_status_code,upstream_status_code,
    provider_error_code,error_message,error_details
  from model_request_observations where started_at >= $1 and started_at < $2
), sources as materialized (
  select id request_id, 'terminal' source, attempt_count attempt_index, error_kind,
    client_status_code,upstream_status_code,provider_error_code,error_message,error_details
  from base
  union all
  select b.id,'event',e.attempt_index,e.failure_kind,null,e.status_code,e.provider_error_code,e.message,e.error_details
  from base b join ops_events e on e.model_request_id=b.id where e.attempt_index > 0
), documents as (
  select *,case when pg_input_is_valid(error_details,'jsonb') then error_details::jsonb end document
  from sources
), upstream as (
  select *,case
    when document ? 'upstream' and pg_input_is_valid(document->>'upstream','jsonb') then (document->>'upstream')::jsonb
    else document end raw_error
  from documents
), codes as (
  select *,lower(coalesce(provider_error_code,'')) stored_code,
    lower(coalesce(raw_error#>>'{error,code}',raw_error->>'code','')) raw_code,
    lower(coalesce(raw_error#>>'{error,type}',raw_error->>'type','')) raw_type,
    coalesce(error_message,'') ~* 'server[ _]+(is[ _]+)?overloaded|server_overloaded' message_overload,
    coalesce(error_message,'') ~* '\mserver_[a-z0-9_]+' message_server
  from upstream
), flags as materialized (
  select *,
    (stored_code like '%overload%' or raw_code like '%overload%' or raw_type like '%overload%' or message_overload) overloaded,
    (left(stored_code,7)='server_' or left(raw_code,7)='server_' or left(raw_type,7)='server_' or message_server) server_prefix,
    (coalesce(client_status_code=503,false) or coalesce(upstream_status_code=503,false) or stored_code='503' or raw_code='503') status503,
    (coalesce(client_status_code between 500 and 599,false) or coalesce(upstream_status_code between 500 and 599,false)) status5xx,
    (message_overload or message_server) legacy_message
  from codes
), attempts as (
  select request_id,attempt_index,bool_or(overloaded) overloaded,bool_or(status503) status503,
    bool_or(overloaded or server_prefix or status503 or status5xx) server_related
  from flags where attempt_index>0 group by request_id,attempt_index
), requests as (
  select b.id,b.started_at,b.client_status_code,
    bool_or(f.overloaded or f.server_prefix or f.status503 or f.status5xx) server_related,
    bool_or(f.source='terminal' and (f.overloaded or f.server_prefix or f.status503 or f.status5xx)) terminal_related,
    bool_or(f.overloaded) overloaded,bool_or(f.status503) status503,
    bool_or(f.server_prefix and not f.overloaded) other_prefix,
    bool_or(f.status5xx and not f.status503) other5xx,
    bool_or(f.upstream_status_code=503 or (f.status503 and f.error_kind not in ('provider_infrastructure_unavailable','no_available_provider') and (f.raw_code='503' or f.stored_code='503'))) upstream503,
    bool_or(f.status503 and f.error_kind='provider_infrastructure_unavailable') infra503,
    bool_or(f.status503 and f.error_kind='no_available_provider') no_provider503,
    bool_or(f.legacy_message) legacy_message
  from base b join flags f on f.request_id=b.id group by b.id,b.started_at,b.client_status_code
), counts as (
  select (started_at at time zone $3)::date::text as "day",
    count(*) "retainedRequests",count(*) filter(where server_related) "serverRelatedRequests",
    count(*) filter(where terminal_related) "terminalServerRelatedRequests",
    count(*) filter(where overloaded) overloaded,count(*) filter(where status503) "status503",
    count(*) filter(where other_prefix) "otherServerPrefix",count(*) filter(where other5xx) "other5xx",
    count(*) filter(where upstream503) "upstream503",count(*) filter(where infra503) "infrastructure503",
    count(*) filter(where no_provider503) "noProvider503",
    count(*) filter(where status503 and not coalesce(upstream503,false) and not coalesce(infra503,false) and not coalesce(no_provider503,false)) "unclassified503",
    count(*) filter(where server_related and client_status_code=200) "client200Errors",
    count(*) filter(where server_related and legacy_message) "legacyMessageMatches"
  from requests group by grouping sets((),((started_at at time zone $3)::date::text))
)
select jsonb_build_object(
  'fromInclusive',$1::timestamptz,'toExclusive',$2::timestamptz,'timezone',$3::text,
  'timeBasis','started_at','source','retained_records',
  'earliestRetainedAt',(select min(started_at) from base),'latestRetainedAt',(select max(started_at) from base),
  'totals',(select to_jsonb(counts)-'day' from counts where day is null),
  'daily',coalesce((select jsonb_agg(to_jsonb(counts) order by day) from counts where day is not null),'[]'::jsonb),
  'attempts',(select jsonb_build_object('observed',count(*),'serverRelated',count(*) filter(where server_related),'overloaded',count(*) filter(where overloaded),'status503',count(*) filter(where status503),'coverage','observed_only') from attempts)
)
"#;

pub(crate) async fn server_error_summary(
    connection: &mut ObservabilityConnection,
    range: admin_observability::TimeRange,
    timezone: gateway_core::time::DeploymentTimeZone,
) -> StoreResult<ServerErrorSummary> {
    let value: serde_json::Value = sqlx::query_scalar(SUMMARY)
        .bind(range.start)
        .bind(range.end)
        .bind(timezone.name())
        .fetch_one(&mut **connection)
        .await
        .map_err(|source| postgres_unavailable("summarize server error family", source))?;
    serde_json::from_value(value).map_err(|_| invalid("invalid server error summary projection"))
}
