//! Usage 明细、诊断与过滤查询族。

use super::super::*;
use gateway_core::provider_ports::turn_state::model_turn_state_token_metadata;
use serde_json::Value;
use sha2::{Digest, Sha256};

const MODEL_DIAGNOSTIC_DIMENSION_SQL: &str =
    "coalesce(mr.upstream_model_id, mr.requested_model_id)";

// 只读取最终 attempt；历史行在迁移前没有采集位，不能把空值解释成未观测。
const TURN_STATE_CLASSIFICATION_SQL: &str =
    "case when mr.provider_kind is distinct from 'openai' or mr.operation <> 'generate'
            then 'notApplicable'
          when mr.outcome = 'running' then 'pending'
          when not mr.turn_state_collection_enabled then 'notCollected'
          when ts.value is null then 'unobserved'
          when octet_length(ts.value) = 292 then 'observed292'
          else 'observedOther' end";

const TURN_STATE_RELATION_SQL: &str =
    "case when not mr.turn_state_sent_collection_enabled then 'unknown'
          when ts.sent_value is not null and ts.value is not null
            then case when ts.sent_value = ts.value then 'same' else 'different' end
          when ts.sent_value is not null then 'only-sent'
          when ts.value is not null then 'only-returned'
          else 'neither' end";

pub(crate) fn push_usage_filter(
    query: &mut QueryBuilder<Postgres>,
    filter: &UsageRecordFilter,
    alias: &str,
) {
    if let Some(value) = &filter.client_api_key_ref {
        query.push(format!(" and {alias}.client_api_key_ref = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = &filter.request_id {
        query.push(format!(" and {alias}.id = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = &filter.provider_account_ref {
        query.push(format!(" and {alias}.provider_account_ref = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = &filter.operation {
        query.push(format!(" and {alias}.operation = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = &filter.provider_kind {
        query.push(format!(" and {alias}.provider_kind = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = &filter.model {
        query.push(format!(" and ({alias}.requested_model_id = "));
        query.push_bind(value.clone());
        query.push(format!(" or {alias}.upstream_model_id = "));
        query.push_bind(value.clone());
        query.push(")");
    }
    if let Some(value) = &filter.outcome {
        query.push(format!(" and {alias}.outcome = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = filter.status_code {
        query.push(format!(" and ({alias}.client_status_code = "));
        query.push_bind(i32::from(value));
        query.push(format!(" or {alias}.upstream_status_code = "));
        query.push_bind(i32::from(value));
        query.push(")");
    }
    if let Some(value) = &filter.transport {
        query.push(format!(" and ({alias}.client_transport = "));
        query.push_bind(value.clone());
        query.push(format!(" or {alias}.upstream_transport = "));
        query.push_bind(value.clone());
        query.push(")");
    }
    if let Some(value) = filter.attempt_index {
        query.push(format!(" and ({alias}.attempt_count = "));
        query.push_bind(i32::try_from(value).unwrap_or(i32::MAX));
        query.push(format!(
            " or exists (select 1 from ops_events attempt_event
                          where attempt_event.model_request_id = {alias}.id
                            and attempt_event.attempt_index = "
        ));
        query.push_bind(i32::try_from(value).unwrap_or(i32::MAX));
        query.push("))");
    }
    if let Some(value) = &filter.response_id {
        query.push(format!(" and ({alias}.client_response_id = "));
        query.push_bind(value.as_bytes().to_vec());
        query.push(format!(" or {alias}.upstream_response_id = "));
        query.push_bind(value.as_bytes().to_vec());
        query.push(")");
    }
    if let Some(value) = &filter.upstream_request_id {
        query.push(format!(" and {alias}.upstream_request_id = "));
        query.push_bind(value.clone());
    }
    if let Some(value) = &filter.search {
        let pattern = literal_prefix_pattern(value);
        query.push(format!(" and ({alias}.id like "));
        query.push_bind(pattern.clone());
        for column in [
            "client_api_key_ref",
            "provider_account_ref",
            "provider_account_email_snapshot",
            "provider_account_name_snapshot",
            "requested_model_id",
            "upstream_model_id",
            "upstream_request_id",
        ] {
            query.push(format!(" escape '\\' or {alias}.{column} like "));
            query.push_bind(pattern.clone());
        }
        query.push(" escape '\\'");
        push_client_key_name_search(query, &format!("{alias}.client_api_key_ref"), pattern);
        query.push(")");
    }
}

pub(crate) fn push_client_key_name_search(
    query: &mut QueryBuilder<Postgres>,
    key_ref: &str,
    pattern: String,
) {
    query.push(format!(
        " or exists (select 1 from client_api_keys searched_client_key
         where searched_client_key.id = {key_ref} and searched_client_key.name ilike "
    ));
    query.push_bind(pattern);
    query.push(" escape '\\')");
}

pub(crate) fn literal_prefix_pattern(value: &str) -> String {
    format!(
        "{}%",
        value
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

pub(crate) async fn turn_state_counts(
    pool: &PgPool,
    range: ObservabilityRange,
    filter: &UsageRecordFilter,
) -> StoreResult<TurnStateCounts> {
    filter.validate()?;
    let mut statement = QueryBuilder::<Postgres>::new(format!(
        "select count(*) filter (where classification = 'observed292')::bigint as observed_292,
                count(*) filter (where classification = 'observedOther')::bigint as observed_other,
                count(*) filter (where classification = 'unobserved')::bigint as unobserved,
                count(*) filter (where classification = 'notCollected')::bigint as not_collected,
                count(*) filter (where classification <> 'notApplicable' and relation = 'same')::bigint as relation_same,
                count(*) filter (where classification <> 'notApplicable' and relation = 'different')::bigint as relation_different,
                count(*) filter (where classification <> 'notApplicable' and relation = 'only-sent')::bigint as only_sent,
                count(*) filter (where classification <> 'notApplicable' and relation = 'only-returned')::bigint as only_returned,
                count(*) filter (where classification <> 'notApplicable' and relation = 'neither')::bigint as neither,
                count(*) filter (where classification <> 'notApplicable' and relation = 'unknown')::bigint as unknown,
                count(*) filter (where classification <> 'notApplicable' and sent_present)::bigint as sent_count,
                count(*) filter (where classification <> 'notApplicable' and returned_present)::bigint as returned_count
           from (select {TURN_STATE_CLASSIFICATION_SQL} as classification,
                        {TURN_STATE_RELATION_SQL} as relation,
                        ts.sent_value is not null as sent_present,
                        ts.value is not null as returned_present
                   from model_requests mr
                   left join request_turn_state_observations ts
                     on ts.request_id = mr.id and ts.attempt_index = mr.attempt_count
                  where mr.started_at >= "
    ));
    statement.push_bind(range.start);
    statement.push(" and mr.started_at < ");
    statement.push_bind(range.end);
    push_unrecovered_request_filter(&mut statement, "mr");
    push_usage_filter(&mut statement, filter, "mr");
    statement.push(") classified");
    let row = statement
        .build()
        .fetch_one(pool)
        .await
        .map_err(|_| postgres_unavailable("load request turn state summary"))?;
    Ok(TurnStateCounts {
        observed_292: unsigned(&row, "observed_292")?,
        observed_other: unsigned(&row, "observed_other")?,
        unobserved: unsigned(&row, "unobserved")?,
        not_collected: unsigned(&row, "not_collected")?,
        same: unsigned(&row, "relation_same")?,
        different: unsigned(&row, "relation_different")?,
        only_sent: unsigned(&row, "only_sent")?,
        only_returned: unsigned(&row, "only_returned")?,
        neither: unsigned(&row, "neither")?,
        unknown: unsigned(&row, "unknown")?,
        sent_count: unsigned(&row, "sent_count")?,
        returned_count: unsigned(&row, "returned_count")?,
    })
}

fn usage_list_record_select() -> String {
    format!(
        "select mr.id, mr.endpoint, mr.client_transport, mr.requested_model_id,
            mr.provider_kind, mr.provider_account_ref,
            mr.provider_account_name_snapshot as provider_account_name,
            mr.provider_account_email_snapshot as provider_account_email,
            account.notes as provider_account_notes,
            mr.provider_account_authentication_kind_snapshot
              as provider_account_authentication_kind,
            mr.upstream_model_id, mr.upstream_transport, mr.upstream_response_model, mr.service_tier,
            mr.input_tokens, mr.output_tokens, mr.cached_tokens, mr.cache_write_tokens,
            mr.reasoning_tokens, mr.image_input_tokens, mr.image_output_tokens,
            mr.total_tokens, mr.billing_snapshot_json, mr.cost_source, mr.cost_amount::text, mr.cost_currency,
            mr.transport_decision_wait_ms, mr.connect_ms, mr.headers_ms,
            mr.first_event_ms, mr.first_reasoning_ms, mr.first_text_ms, mr.first_token_ms,
            mr.provider_processing_ms, mr.latency_ms, mr.admission_decision_ms,
            mr.account_selection_wait_ms, mr.capacity_used_slots, mr.capacity_total_slots,
            host(mr.client_ip) as client_ip, mr.user_agent,
            mr.reasoning_effort, mr.reasoning_preset, mr.subagent_kind, mr.compact,
            mr.started_at,
            {TURN_STATE_CLASSIFICATION_SQL} as turn_state_classification,
            octet_length(ts.value)::bigint as turn_state_bytes,
            {TURN_STATE_RELATION_SQL} as turn_state_relation,
            octet_length(ts.sent_value)::bigint as turn_state_sent_bytes,
            octet_length(ts.value)::bigint as turn_state_returned_bytes
     from model_requests mr
     left join request_turn_state_observations ts
       on ts.request_id = mr.id and ts.attempt_index = mr.attempt_count"
    )
}

fn usage_record_detail_select() -> String {
    format!(
        "select mr.id, mr.client_api_key_ref, mr.config_revision,
            mr.routing_scope, mr.routing_group_refs, mr.routing_group_names_snapshot,
            mr.protocol, mr.operation,
            mr.endpoint, mr.client_transport, mr.requested_model_id,
            mr.provider_kind, mr.provider_account_ref,
            mr.provider_account_name_snapshot as provider_account_name,
            mr.provider_account_email_snapshot as provider_account_email,
            mr.provider_account_authentication_kind_snapshot
              as provider_account_authentication_kind,
            mr.upstream_model_id, mr.upstream_transport, mr.http_version, mr.websocket_pool,
            mr.service_tier, mr.upstream_response_model, mr.provider_observation_json, mr.diagnostic_trace_json,
            mr.attempt_count, mr.upstream_send_state, mr.downstream_committed_at,
            mr.turn_state_sent_collection_enabled,
            mr.outcome, mr.client_status_code, mr.upstream_status_code,
            mr.client_response_id, mr.upstream_request_id, mr.upstream_response_id,
            mr.error_kind, mr.provider_error_code, mr.error_message, mr.retry_after_ms,
            mr.input_tokens, mr.output_tokens, mr.cached_tokens, mr.cache_write_tokens,
            mr.reasoning_tokens, mr.image_input_tokens, mr.image_output_tokens,
            mr.total_tokens, mr.billing_snapshot_json, mr.cost_source, mr.cost_amount::text,
            mr.cost_currency, mr.transport_decision_wait_ms, mr.connect_ms, mr.headers_ms,
            mr.first_event_ms, mr.first_reasoning_ms, mr.first_text_ms, mr.first_token_ms,
            mr.provider_processing_ms, mr.latency_ms, mr.admission_decision_ms,
            mr.account_selection_wait_ms, mr.capacity_used_slots, mr.capacity_total_slots,
            host(mr.client_ip) as client_ip,
            mr.user_agent, mr.reasoning_effort, mr.reasoning_preset, mr.request_kind,
            mr.subagent_kind, mr.compact, mr.image_generation_requested,
            mr.image_generation_succeeded, mr.started_at, mr.deadline_at, mr.completed_at,
            {TURN_STATE_CLASSIFICATION_SQL} as turn_state_classification,
            octet_length(ts.value)::bigint as turn_state_bytes,
            {TURN_STATE_RELATION_SQL} as turn_state_relation,
            octet_length(ts.sent_value)::bigint as turn_state_sent_bytes,
            octet_length(ts.value)::bigint as turn_state_returned_bytes,
            ts.value as turn_state_value, ts.observed_at as turn_state_observed_at,
            ts.source as turn_state_source,
            ts.upstream_response_id as turn_state_upstream_response_id,
            ts.attempt_index as turn_state_attempt_index, ts.changed as turn_state_changed,
            ts.sent_value as turn_state_sent_value, ts.sent_at as turn_state_sent_at,
            ts.sent_source as turn_state_sent_source,
            ts.sent_transport as turn_state_sent_transport,
            ts.sent_account_id as turn_state_sent_account_id,
            ts.sent_identity_revision as turn_state_sent_identity_revision,
            ts.sent_effective_model as turn_state_sent_effective_model,
            ts.sent_generation as turn_state_sent_generation,
            ts.sent_candidate_id as turn_state_sent_candidate_id
     from model_requests mr
     left join request_turn_state_observations ts
       on ts.request_id = mr.id and ts.attempt_index = mr.attempt_count"
    )
}

pub(crate) async fn list_usage_records(
    pool: &PgPool,
    query: UsageRecordQuery,
) -> StoreResult<UsageRecordPage> {
    query.filter.validate()?;
    let total = count_usage_records(pool, query.range, &query.filter).await?;
    let items = list_usage_record_items(pool, &query).await?;
    Ok(UsageRecordPage {
        items,
        current_page: query.current_page,
        page_size: query.page_size.get(),
        total,
    })
}

pub(crate) async fn list_usage_record_items(
    pool: &PgPool,
    query: &UsageRecordQuery,
) -> StoreResult<Vec<UsageListRecord>> {
    query.filter.validate()?;
    let offset = observability_page_offset(query.current_page, query.page_size)?;
    let mut statement = QueryBuilder::<Postgres>::new(usage_list_record_select());
    statement.push(" where mr.started_at >= ");
    statement.push_bind(query.range.start);
    statement.push(" and mr.started_at < ");
    statement.push_bind(query.range.end);
    push_completed_usage_fact_filter(&mut statement, "mr");
    push_usage_filter(&mut statement, &query.filter, "mr");
    statement.push(" order by mr.started_at desc, mr.id desc limit ");
    statement.push_bind(i64::from(query.page_size.get()));
    statement.push(" offset ");
    statement.push_bind(offset);
    let rows = statement
        .build()
        .fetch_all(pool)
        .await
        .map_err(|_| postgres_unavailable("list usage records"))?;
    let items = rows
        .iter()
        .map(usage_list_record_from_row)
        .collect::<StoreResult<Vec<_>>>()?;
    Ok(items)
}

pub(crate) async fn count_usage_records(
    pool: &PgPool,
    range: ObservabilityRange,
    filter: &UsageRecordFilter,
) -> StoreResult<u64> {
    let mut statement = QueryBuilder::<Postgres>::new(
        "select count(*)::bigint from model_requests mr where mr.started_at >= ",
    );
    statement.push_bind(range.start);
    statement.push(" and mr.started_at < ");
    statement.push_bind(range.end);
    push_completed_usage_fact_filter(&mut statement, "mr");
    push_usage_filter(&mut statement, filter, "mr");
    let total = statement
        .build_query_scalar::<i64>()
        .fetch_one(pool)
        .await
        .map_err(|_| postgres_unavailable("count usage records"))?;
    to_u64(total)
}

pub(crate) async fn usage_record_detail(
    pool: &PgPool,
    request_id: &str,
) -> StoreResult<UsageRecordDetail> {
    require_nonempty("model request", "id", request_id)?;
    validate_text(request_id, MAX_FILTER_BYTES, "request ID")?;
    let mut statement = QueryBuilder::<Postgres>::new(usage_record_detail_select());
    statement.push(" where mr.id = ");
    statement.push_bind(request_id.to_owned());
    let row = statement
        .build()
        .fetch_optional(pool)
        .await
        .map_err(|_| postgres_unavailable("load usage record detail"))?
        .ok_or_else(|| StoreError::NotFound {
            entity: "model request",
            id: request_id.to_owned(),
        })?;
    let request = usage_record_from_row(&row)?;
    let value: Option<String> = get(&row, "turn_state_value")?;
    let returned = value
        .as_ref()
        .map(|value| {
            let metadata = model_turn_state_token_metadata(value);
            Ok::<_, StoreError>(TurnStateEvidence {
                value: value.clone(),
                bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
                sha256: hex::encode(Sha256::digest(value.as_bytes())),
                timestamp: get(&row, "turn_state_observed_at")?,
                source: "upstream".to_owned(),
                transport: get(&row, "turn_state_source")?,
                token_version: metadata.token_version,
                issued_at: metadata.issued_at,
                account_id: request.provider_account_ref.clone(),
                identity_revision: None,
                effective_model: request.upstream_model_id.clone(),
                generation: None,
                candidate_id: None,
                upstream_response_id: get(&row, "turn_state_upstream_response_id")?,
                changed: get(&row, "turn_state_changed")?,
            })
        })
        .transpose()?;
    let sent_value: Option<String> = get(&row, "turn_state_sent_value")?;
    let sent = sent_value
        .as_ref()
        .map(|value| {
            let metadata = model_turn_state_token_metadata(value);
            Ok::<_, StoreError>(TurnStateEvidence {
                value: value.clone(),
                bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
                sha256: hex::encode(Sha256::digest(value.as_bytes())),
                timestamp: get(&row, "turn_state_sent_at")?,
                source: get(&row, "turn_state_sent_source")?,
                transport: get(&row, "turn_state_sent_transport")?,
                token_version: metadata.token_version,
                issued_at: metadata.issued_at,
                account_id: get(&row, "turn_state_sent_account_id")?,
                identity_revision: get::<Option<i64>>(&row, "turn_state_sent_identity_revision")?
                    .and_then(|value| u64::try_from(value).ok()),
                effective_model: get(&row, "turn_state_sent_effective_model")?,
                generation: get::<Option<i64>>(&row, "turn_state_sent_generation")?
                    .and_then(|value| u64::try_from(value).ok()),
                candidate_id: get(&row, "turn_state_sent_candidate_id")?,
                upstream_response_id: None,
                changed: false,
            })
        })
        .transpose()?;
    let relation = match (&sent, &returned) {
        (Some(sent), Some(returned)) if sent.value == returned.value => "same",
        (Some(_), Some(_)) => "different",
        (Some(_), None) => "only-sent",
        (None, Some(_)) if !get::<bool>(&row, "turn_state_sent_collection_enabled")? => "unknown",
        (None, Some(_)) => "only-returned",
        (None, None) if !get::<bool>(&row, "turn_state_sent_collection_enabled")? => "unknown",
        (None, None) => "neither",
    }
    .to_owned();
    let turn_state = if let Some(value) = value {
        TurnStateDetail {
            summary: request.turn_state.clone(),
            relation,
            sent,
            returned,
            sha256: Some(hex::encode(Sha256::digest(value.as_bytes()))),
            value: Some(value),
            observed_at: Some(get(&row, "turn_state_observed_at")?),
            source: Some(get(&row, "turn_state_source")?),
            upstream_response_id: get(&row, "turn_state_upstream_response_id")?,
            attempt_index: Some(to_u32(get(&row, "turn_state_attempt_index")?)?),
            changed: get(&row, "turn_state_changed")?,
        }
    } else {
        TurnStateDetail {
            summary: request.turn_state.clone(),
            relation,
            sent,
            returned,
            value: None,
            sha256: None,
            observed_at: None,
            source: None,
            upstream_response_id: None,
            attempt_index: None,
            changed: false,
        }
    };
    let rows = sqlx::query(
        "select id, attempt_index, component, operation,
                provider_kind, provider_account_ref,
                provider_account_name_snapshot as provider_account_name,
                provider_account_email_snapshot as provider_account_email,
                provider_account_authentication_kind_snapshot
                  as provider_account_authentication_kind,
                upstream_model_id,
                failure_kind, status_code, provider_error_code, retry_after_ms,
                upstream_request_id, latency_ms, message, created_at
         from ops_events where model_request_id = $1
         order by attempt_index, created_at, id",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await
    .map_err(|_| postgres_unavailable("load usage attempt observations"))?;
    let mut attempts = rows
        .iter()
        .map(intermediate_attempt_from_row)
        .collect::<StoreResult<Vec<_>>>()?;
    if request.attempt_count > 0 {
        attempts.push(final_attempt_from_request(&request));
    }
    let trace: Option<Value> = row
        .try_get("diagnostic_trace_json")
        .map_err(|_| postgres_unavailable("decode request trace"))?;
    let related_requests = sqlx::query_scalar::<_, Value>(
        "select jsonb_build_object('requestId', related.id, 'outcome', related.outcome,
             'relation', case when related.id = current.recovery_request_id then 'recovered_by' else 'recovers' end,
             'completedAt', related.completed_at)
         from model_requests current join model_requests related
           on related.id = current.recovery_request_id or related.recovery_request_id = current.id
         where current.id = $1 order by related.started_at limit 20"
    ).bind(request_id).fetch_all(pool).await
        .map_err(|_| postgres_unavailable("load related recovery requests"))?;
    Ok(UsageRecordDetail {
        request,
        turn_state,
        attempts,
        trace,
        related_requests,
    })
}

pub(crate) fn intermediate_attempt_from_row(
    row: &sqlx::postgres::PgRow,
) -> StoreResult<UsageAttemptObservation> {
    Ok(UsageAttemptObservation {
        source: "ops_event".to_owned(),
        id: get(row, "id")?,
        attempt_index: to_u32(get(row, "attempt_index")?)?,
        component: get(row, "component")?,
        operation: get(row, "operation")?,
        provider_kind: get(row, "provider_kind")?,
        provider_account_ref: get(row, "provider_account_ref")?,
        provider_account_name: get(row, "provider_account_name")?,
        provider_account_email: get(row, "provider_account_email")?,
        provider_account_authentication_kind: get(row, "provider_account_authentication_kind")?,
        upstream_model_id: get(row, "upstream_model_id")?,
        upstream_transport: None,
        upstream_send_state: None,
        outcome: "failed".to_owned(),
        downstream_committed: false,
        status_code: optional_status(row, "status_code")?,
        provider_error_code: get(row, "provider_error_code")?,
        failure_kind: get(row, "failure_kind")?,
        retry_after_ms: optional_unsigned(row, "retry_after_ms")?,
        upstream_request_id: get(row, "upstream_request_id")?,
        latency_ms: optional_unsigned(row, "latency_ms")?,
        message: get(row, "message")?,
        input_tokens: None,
        output_tokens: None,
        cached_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        total_tokens: None,
        cost_source: None,
        cost_amount: None,
        cost_currency: None,
        occurred_at: get(row, "created_at")?,
    })
}

pub(crate) fn final_attempt_from_request(request: &UsageRecord) -> UsageAttemptObservation {
    UsageAttemptObservation {
        source: "model_request".to_owned(),
        id: format!("{}:final", request.id),
        attempt_index: request.attempt_count,
        component: "model_request".to_owned(),
        operation: request.operation.clone(),
        provider_kind: request.provider_kind.clone(),
        provider_account_ref: request.provider_account_ref.clone(),
        provider_account_name: request.provider_account_name.clone(),
        provider_account_email: request.provider_account_email.clone(),
        provider_account_authentication_kind: request.provider_account_authentication_kind.clone(),
        upstream_model_id: request.upstream_model_id.clone(),
        upstream_transport: request.upstream_transport.clone(),
        upstream_send_state: Some(request.upstream_send_state.clone()),
        outcome: request.outcome.clone(),
        downstream_committed: request.downstream_committed_at.is_some(),
        status_code: request.upstream_status_code.or(request.client_status_code),
        provider_error_code: request.provider_error_code.clone(),
        failure_kind: request.error_kind.clone(),
        retry_after_ms: request.retry_after_ms,
        upstream_request_id: request.upstream_request_id.clone(),
        latency_ms: request.latency_ms,
        message: request.error_message.clone(),
        input_tokens: request.input_tokens,
        output_tokens: request.output_tokens,
        cached_tokens: request.cached_tokens,
        cache_write_tokens: request.cache_write_tokens,
        reasoning_tokens: request.reasoning_tokens,
        total_tokens: request.total_tokens,
        cost_source: Some(request.cost_source.clone()),
        cost_amount: request.cost_amount.clone(),
        cost_currency: request.cost_currency.clone(),
        occurred_at: request.completed_at.unwrap_or(request.started_at),
    }
}

pub(crate) async fn usage_diagnostics(
    pool: &PgPool,
    range: ObservabilityRange,
    filter: &UsageRecordFilter,
    dimension: DiagnosticDimension,
) -> StoreResult<Vec<DiagnosticObservation>> {
    filter.validate()?;
    let dimension_sql = diagnostic_dimension_sql(dimension);
    let completed_usage = completed_usage_fact_predicate("mr");
    let mut statement = QueryBuilder::<Postgres>::new("with matched as (select ");
    statement.push(dimension_sql);
    statement.push(format!(
        " as dimension_name, mr.outcome, mr.attempt_count, mr.total_tokens,
                mr.latency_ms, mr.first_token_ms, mr.cost_source, mr.cost_amount,
                mr.cost_currency, mr.downstream_committed_at, mr.client_transport,
                mr.client_status_code, ({completed_usage}) as is_completed_usage
         from model_requests mr where mr.started_at >= ",
    ));
    statement.push_bind(range.start);
    statement.push(" and mr.started_at < ");
    statement.push_bind(range.end);
    push_unrecovered_request_filter(&mut statement, "mr");
    push_diagnostic_dimension_filter(&mut statement, dimension);
    push_usage_filter(&mut statement, filter, "mr");
    statement.push(
        "), aggregated as (
           select dimension_name, cost_currency,
                  grouping(cost_currency)::integer as currency_grouping,
                  count(*)::bigint as request_count,
                  count(*) filter (where outcome = 'succeeded')::bigint as success_count,
                  count(*) filter (where outcome = 'failed')::bigint as failure_count,
                  coalesce(sum(attempt_count), 0)::bigint as attempt_count,
                  coalesce(sum(total_tokens) filter (where is_completed_usage), 0)::bigint
                    as total_tokens,
                  round(avg(latency_ms) filter (where is_completed_usage))::bigint
                    as average_latency_ms,
                  round(percentile_cont(0.95) within group (order by latency_ms)
                    filter (where is_completed_usage))::bigint
                    as latency_p95_ms,
                  round(percentile_cont(0.95) within group (order by first_token_ms)
                    filter (where is_completed_usage))::bigint
                    as first_token_p95_ms,
                  count(*) filter (where outcome in ('cancelled', 'incomplete'))::bigint
                    as non_completion_count,
                  coalesce(sum(greatest(attempt_count - 1, 0)), 0)::bigint as retry_count,
                  count(*) filter (
                    where is_completed_usage and cost_source = 'provider_reported'
                  )::bigint
                    as provider_reported_count,
                  count(*) filter (
                    where is_completed_usage and cost_source = 'calculated'
                  )::bigint
                    as calculated_count,
                  count(*) filter (
                    where is_completed_usage and cost_source = 'unavailable'
                  )::bigint
                    as unavailable_count,
                  sum(cost_amount) filter (
                    where is_completed_usage
                  )::text as amount
             from matched
            group by dimension_name, grouping sets ((), (cost_currency))
         ), selected_dimensions as (
           select dimension_name,
                  row_number() over (order by request_count desc, dimension_name)
                    as sort_position
             from aggregated
            where currency_grouping = 1
            order by request_count desc, dimension_name limit ",
    );
    statement.push_bind(DIAGNOSTIC_LIMIT);
    statement.push(
        ")
         select aggregated.*
           from aggregated
           join selected_dimensions selected using (dimension_name)
          order by selected.sort_position, currency_grouping desc, cost_currency nulls last",
    );
    let rows = statement
        .build()
        .fetch_all(pool)
        .await
        .map_err(|_| postgres_unavailable("load usage diagnostics"))?;
    let mut observations = Vec::with_capacity(DIAGNOSTIC_LIMIT as usize);
    let mut costs = HashMap::<String, Vec<CurrencyCostTotal>>::new();
    for row in &rows {
        match get::<i32>(row, "currency_grouping")? {
            1 => observations.push(DiagnosticObservation {
                key: get(row, "dimension_name")?,
                name: get(row, "dimension_name")?,
                request_count: unsigned(row, "request_count")?,
                success_count: unsigned(row, "success_count")?,
                failure_count: unsigned(row, "failure_count")?,
                attempt_count: unsigned(row, "attempt_count")?,
                total_tokens: unsigned(row, "total_tokens")?,
                average_latency_ms: optional_unsigned(row, "average_latency_ms")?,
                latency_p95_ms: optional_unsigned(row, "latency_p95_ms")?,
                first_token_p95_ms: optional_unsigned(row, "first_token_p95_ms")?,
                non_completion_count: unsigned(row, "non_completion_count")?,
                retry_count: unsigned(row, "retry_count")?,
                cost_coverage: coverage_from_row(row)?,
                costs: Vec::new(),
            }),
            0 if get::<Option<String>>(row, "cost_currency")?.is_some()
                && get::<Option<String>>(row, "amount")?.is_some() =>
            {
                costs
                    .entry(get(row, "dimension_name")?)
                    .or_default()
                    .push(cost_from_row(row)?);
            }
            0 => {}
            _ => return Err(postgres_unavailable("decode usage diagnostic grouping")),
        }
    }
    let mut display_names = match dimension {
        DiagnosticDimension::Account => {
            diagnostic_account_display_names(
                pool,
                &observations
                    .iter()
                    .map(|item| item.key.clone())
                    .collect::<Vec<_>>(),
            )
            .await?
        }
        DiagnosticDimension::ApiKey => {
            diagnostic_api_key_display_names(
                pool,
                &observations
                    .iter()
                    .map(|item| item.key.clone())
                    .collect::<Vec<_>>(),
            )
            .await?
        }
        _ => HashMap::new(),
    };
    for observation in &mut observations {
        if let Some(name) = display_names.remove(&observation.key) {
            observation.name = name;
        }
        observation.costs = costs.remove(&observation.key).unwrap_or_default();
    }
    Ok(observations)
}

pub(crate) async fn diagnostic_account_display_names(
    pool: &PgPool,
    account_ids: &[String],
) -> StoreResult<HashMap<String, String>> {
    if account_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "select requested.account_id as provider_account_ref,
                coalesce(
                  (
                    select request.provider_account_email_snapshot
                    from model_requests request
                    where request.provider_account_ref = requested.account_id
                      and request.provider_account_email_snapshot is not null
                    order by request.started_at desc, request.id desc
                    limit 1
                  ),
                  (
                    select request.provider_account_name_snapshot
                    from model_requests request
                    where request.provider_account_ref = requested.account_id
                      and request.provider_account_name_snapshot is not null
                    order by request.started_at desc, request.id desc
                    limit 1
                  ),
                  requested.account_id
                ) as display_name
         from unnest($1::text[]) as requested(account_id)",
    )
    .bind(account_ids)
    .fetch_all(pool)
    .await
    .map_err(|_| postgres_unavailable("load diagnostic account display names"))?;
    let mut display_names = HashMap::with_capacity(rows.len());
    for row in rows {
        display_names.insert(
            get(&row, "provider_account_ref")?,
            get(&row, "display_name")?,
        );
    }
    Ok(display_names)
}

pub(crate) async fn diagnostic_api_key_display_names(
    pool: &PgPool,
    key_ids: &[String],
) -> StoreResult<HashMap<String, String>> {
    if key_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query("select id, name from client_api_keys where id = any($1::text[])")
        .bind(key_ids)
        .fetch_all(pool)
        .await
        .map_err(|_| postgres_unavailable("load diagnostic api key display names"))?;
    let mut display_names = HashMap::with_capacity(rows.len());
    for row in rows {
        display_names.insert(get(&row, "id")?, get(&row, "name")?);
    }
    Ok(display_names)
}

pub(crate) fn diagnostic_dimension_sql(dimension: DiagnosticDimension) -> &'static str {
    match dimension {
        DiagnosticDimension::Provider => "coalesce(mr.provider_kind, 'unrouted')",
        DiagnosticDimension::Model => MODEL_DIAGNOSTIC_DIMENSION_SQL,
        DiagnosticDimension::Account => "coalesce(mr.provider_account_ref, 'unrouted')",
        DiagnosticDimension::ApiKey => "mr.client_api_key_ref",
        DiagnosticDimension::Transport => {
            "coalesce(mr.upstream_transport, mr.client_transport, 'unknown')"
        }
        DiagnosticDimension::Failure => "coalesce(mr.error_kind, 'none')",
        DiagnosticDimension::Status => {
            "coalesce(mr.upstream_status_code, mr.client_status_code)::text"
        }
    }
}

pub(crate) fn push_diagnostic_dimension_filter(
    statement: &mut QueryBuilder<Postgres>,
    dimension: DiagnosticDimension,
) {
    match dimension {
        DiagnosticDimension::Failure => {
            statement.push(" and mr.error_kind is not null");
        }
        DiagnosticDimension::Model => {
            statement.push(" and ");
            statement.push(MODEL_DIAGNOSTIC_DIMENSION_SQL);
            statement.push(" is not null");
        }
        DiagnosticDimension::Status => {
            statement
                .push(" and coalesce(mr.upstream_status_code, mr.client_status_code) is not null");
        }
        DiagnosticDimension::Provider
        | DiagnosticDimension::Account
        | DiagnosticDimension::ApiKey
        | DiagnosticDimension::Transport => {}
    }
}
