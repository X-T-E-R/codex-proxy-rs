//! 有界保留记录上的服务端错误族计数，不读取请求正文或完整 trace

use super::super::*;
use gateway_admin::model::server_errors::ServerErrorSummary;

// 窄物化分别固定观测子对象和受控详情解析结果，避免内联后按分类重复解压与解码
// 来源携带请求日桶和最终客户端状态，按请求聚合时无需再次关联全量请求
const SUMMARY: &str = r#"
with base as materialized (
  select id,started_at,attempt_count,error_kind,client_status_code,upstream_status_code,
    request_observation_json->'error' error_observation,error_details
  from model_requests where started_at >= $1 and started_at < $2 and ($4::text is null or provider_kind=$4)
), sources as (
  select id request_id,started_at,client_status_code request_client_status_code,
    'terminal' source,attempt_count attempt_index,error_kind,
    client_status_code,upstream_status_code,error_observation->>'providerErrorCode' provider_error_code,
    error_observation->>'message' error_message,error_details
  from base
  union all
  select b.id,b.started_at,b.client_status_code,'event',e.attempt_index,e.failure_kind,
    null,e.status_code,e.provider_error_code,e.message,e.error_details
  from base b join ops_events e on e.model_request_id=b.id where e.attempt_index > 0
), documents as (
  select *,case when pg_input_is_valid(error_details,'jsonb') then error_details::jsonb end document
  from sources
), upstream as materialized (
  select request_id,started_at,request_client_status_code,source,attempt_index,
    error_kind,client_status_code,upstream_status_code,
    lower(coalesce(provider_error_code,'')) stored_code,
    coalesce(error_message,'') ~* 'server[ _]+(is[ _]+)?overloaded|server_overloaded' message_overload,
    coalesce(error_message,'') ~* '\mserver_[a-z0-9_]+' message_server,
    case when document ? 'upstream' and pg_input_is_valid(document->>'upstream','jsonb')
      then (document->>'upstream')::jsonb else document end raw_error
  from documents
), codes as materialized (
  select request_id,started_at,request_client_status_code,source,attempt_index,
    error_kind,client_status_code,upstream_status_code,stored_code,message_overload,message_server,
    lower(coalesce(raw_error#>>'{error,code}',raw_error#>>'{response,error,code}',raw_error->>'code','')) raw_code,
    lower(coalesce(raw_error#>>'{error,type}',raw_error#>>'{response,error,type}',raw_error->>'type','')) raw_type,
    coalesce(raw_error#>>'{error,status}',raw_error#>>'{response,error,status}',raw_error->>'status','') raw_status
  from upstream
), classified as (
  select request_id,started_at,request_client_status_code,source,attempt_index,
    error_kind,upstream_status_code,stored_code,raw_code,raw_status,
    (__OVERLOADED__) overloaded,
    (left(stored_code,7)='server_' or left(raw_code,7)='server_' or left(raw_type,7)='server_' or message_server) server_prefix,
    (coalesce(client_status_code=503,false) or coalesce(upstream_status_code=503,false) or stored_code='503' or raw_code='503' or raw_status='503') status503,
    (coalesce(client_status_code between 500 and 599,false) or coalesce(upstream_status_code between 500 and 599,false) or raw_status ~ '^5[0-9]{2}$') status5xx,
    (message_overload or message_server) legacy_message
  from codes
), flags as materialized (
  select request_id,started_at,request_client_status_code,source,attempt_index,
    overloaded,server_prefix,status503,status5xx,legacy_message,
    (upstream_status_code=503 or (status503 and error_kind not in ('provider_infrastructure_unavailable','no_available_provider') and (raw_code='503' or stored_code='503' or raw_status='503'))) upstream503,
    (status503 and error_kind='provider_infrastructure_unavailable') infra503,
    (status503 and error_kind='no_available_provider') no_provider503
  from classified
), attempts as (
  select request_id,attempt_index,bool_or(overloaded) overloaded,bool_or(status503) status503,
    bool_or(overloaded or server_prefix or status503 or status5xx) server_related
  from flags where attempt_index>0 group by request_id,attempt_index
), requests as (
  select request_id,started_at,request_client_status_code client_status_code,
    bool_or(overloaded or server_prefix or status503 or status5xx) server_related,
    bool_or(source='terminal' and (overloaded or server_prefix or status503 or status5xx)) terminal_related,
    bool_or(overloaded) overloaded,bool_or(status503) status503,
    bool_or(server_prefix and not overloaded) other_prefix,
    bool_or(status5xx and not status503) other5xx,
    bool_or(upstream503) upstream503,bool_or(infra503) infra503,bool_or(no_provider503) no_provider503,
    bool_or(legacy_message) legacy_message
  from flags group by request_id,started_at,request_client_status_code
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
  'provider',$4::text,'timeBasis','started_at','source','retained_records',
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
    provider: Option<String>,
) -> StoreResult<ServerErrorSummary> {
    validate_optional_text(provider.as_deref(), MAX_FILTER_BYTES, "provider filter")?;
    let sql = SUMMARY.replace(
        "__OVERLOADED__",
        &super::usage_errors::overloaded_predicate(
            &["stored_code", "raw_code", "raw_type"],
            "message_overload",
        ),
    );
    // SQL 片段仅由固定列名与共享分类规则组成，所有请求参数仍通过 bind 传入
    let value: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(range.start)
        .bind(range.end)
        .bind(timezone.name())
        .bind(provider)
        .fetch_one(&mut **connection)
        .await
        .map_err(|source| postgres_unavailable("summarize server error family", source))?;
    serde_json::from_value(value).map_err(|_| invalid("invalid server error summary projection"))
}
