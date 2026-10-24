//! 请求终态与保留尝试事件共享的错误文本和分类筛选谓词

use super::super::*;
use super::literal_contains_pattern;

pub(super) fn push_error_keyword(query: &mut QueryBuilder<Postgres>, value: &str, alias: &str) {
    let pattern = literal_contains_pattern(value);
    let status = value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|code| (100..=599).contains(code));
    query.push("(");
    push_error_text_source(query, &pattern, status, alias, false);
    query.push(format!(
        " or exists(select 1 from ops_events error_event where error_event.model_request_id = {alias}.id and error_event.attempt_index > 0 and ("
    ));
    push_error_text_source(query, &pattern, status, "error_event", true);
    query.push("))");
    for error_type in matched_error_types_for_keyword(value) {
        query.push(" or ");
        push_error_type(query, error_type, alias);
    }
    query.push(")");
}

fn push_error_text_source(
    query: &mut QueryBuilder<Postgres>,
    pattern: &str,
    status: Option<u16>,
    alias: &str,
    event: bool,
) {
    let columns = if event {
        [
            "provider_error_code",
            "failure_kind",
            "message",
            "error_details",
        ]
    } else {
        [
            "provider_error_code",
            "error_kind",
            "error_message",
            "error_details",
        ]
    };
    for (index, column) in columns.into_iter().enumerate() {
        if index > 0 {
            query.push(" or ");
        }
        query
            .push(format!("{alias}.{column} ilike "))
            .push_bind(pattern.to_owned())
            .push(" escape '\\'");
    }
    if let Some(status) = status {
        for column in if event {
            &["status_code"][..]
        } else {
            &["client_status_code", "upstream_status_code"][..]
        } {
            query
                .push(format!(" or {alias}.{column} = "))
                .push_bind(i32::from(status));
        }
    }
}

pub(super) fn push_error_type(
    query: &mut QueryBuilder<Postgres>,
    error_type: UsageErrorType,
    alias: &str,
) {
    query
        .push("(")
        .push(error_type_source_predicate(error_type, alias, false))
        .push(format!(
            " or exists(select 1 from ops_events error_event where error_event.model_request_id = {alias}.id and error_event.attempt_index > 0 and ("
        ))
        .push(error_type_source_predicate(error_type, "error_event", true))
        .push(")))");
}

fn error_type_source_predicate(error_type: UsageErrorType, alias: &str, event: bool) -> String {
    let kind = format!(
        "{alias}.{}",
        if event { "failure_kind" } else { "error_kind" }
    );
    let code = format!("lower(coalesce({alias}.provider_error_code,''))");
    let status = |condition: &str| {
        if event {
            format!("coalesce({alias}.status_code {condition},false)")
        } else {
            format!(
                "(coalesce({alias}.client_status_code {condition},false) or coalesce({alias}.upstream_status_code {condition},false))"
            )
        }
    };
    match error_type {
        UsageErrorType::Overloaded => {
            let message = if event { "message" } else { "error_message" };
            let raw = structured_error_predicate(
                alias,
                "raw_code like '%overload%' or raw_type like '%overload%'",
            );
            format!(
                "{code} like '%overload%' or coalesce({alias}.{message},'') ~* 'server[ _]+(is[ _]+)?overloaded|server_overloaded' or {raw}"
            )
        }
        UsageErrorType::Status503 => {
            let raw = structured_error_predicate(alias, "raw_code = '503' or raw_status = '503'");
            format!("{} or {code} = '503' or {raw}", status("= 503"))
        }
        UsageErrorType::RateLimited => format!(
            "{kind} in ('rate_limited','quota_exhausted') or {} or {code} like '%rate_limit%' or {code} like '%quota%' or {code} like '%usage_limit%'",
            status("= 429")
        ),
        UsageErrorType::UpstreamUnavailable => format!(
            "{kind} in ('upstream_unavailable','unavailable','upstream_capacity_unavailable','upstream_error','transport','protocol') or {}",
            status("between 500 and 599")
        ),
        UsageErrorType::Timeout => format!("{kind} = 'timeout' or {code} like '%timeout%'"),
        UsageErrorType::NoAvailableProvider => format!(
            "{kind} in ('no_available_provider','no_eligible_account','session_policy_restricted')"
        ),
        UsageErrorType::CapacityUnavailable => format!(
            "{kind} in ('account_capacity_unavailable','middleware_capacity_unavailable','concurrency_queue_full','concurrency_queue_timeout')"
        ),
        UsageErrorType::ProviderInfrastructureUnavailable => {
            format!("{kind} = 'provider_infrastructure_unavailable'")
        }
        UsageErrorType::Unauthorized => format!(
            "{kind} in ('unauthorized','permission_denied','policy_denied','request_policy_denied') or {}",
            status("in (401,403)")
        ),
        UsageErrorType::ContinuationRecoveryRequired => {
            let reason = if event {
                String::new()
            } else {
                format!(" or {alias}.continuation_unavailable_reason is not null")
            };
            format!("{kind} = 'continuation_recovery_required'{reason}")
        }
        UsageErrorType::InvalidRequest => format!(
            "{kind} in ('invalid_request','unsupported','model_not_found','message_too_big') or {code} = 'message_too_big' or {}",
            status("in (400,404,413,422)")
        ),
        UsageErrorType::ProcessInterrupted => {
            let incomplete = if event {
                String::new()
            } else {
                format!(" or {alias}.outcome = 'incomplete'")
            };
            format!(
                "{kind} in ('process_interrupted','process_terminated','internal_error'){incomplete}"
            )
        }
    }
}

fn structured_error_predicate(alias: &str, predicate: &str) -> String {
    // 与服务端错误摘要使用相同的受控详情解码路径，非法 JSON 不参与分类
    format!(
        "exists(select 1 from (
           select lower(coalesce(raw_error#>>'{{error,code}}',raw_error#>>'{{response,error,code}}',raw_error->>'code','')) raw_code,
                  lower(coalesce(raw_error#>>'{{error,type}}',raw_error#>>'{{response,error,type}}',raw_error->>'type','')) raw_type,
                  coalesce(raw_error#>>'{{error,status}}',raw_error#>>'{{response,error,status}}',raw_error->>'status','') raw_status
           from (select case
             when document ? 'upstream' and pg_input_is_valid(document->>'upstream','jsonb') then (document->>'upstream')::jsonb
             else document end raw_error
             from (select case when pg_input_is_valid({alias}.error_details,'jsonb') then {alias}.error_details::jsonb end document) documents
           ) upstream
         ) codes where {predicate})"
    )
}

fn matched_error_types_for_keyword(value: &str) -> Vec<UsageErrorType> {
    let value = value.to_lowercase();
    let aliases: &[(UsageErrorType, &[&str])] = &[
        (UsageErrorType::Overloaded, &["过载", "過載", "overload"]),
        (
            UsageErrorType::Status503,
            &["503", "服务不可用", "服務不可用"],
        ),
        (
            UsageErrorType::RateLimited,
            &[
                "限流",
                "配额",
                "配額",
                "額度",
                "额度",
                "rate_limit",
                "rate limit",
                "quota",
                "429",
            ],
        ),
        (UsageErrorType::UpstreamUnavailable, &["上游", "upstream"]),
        (UsageErrorType::Timeout, &["超时", "超時", "timeout"]),
        (
            UsageErrorType::NoAvailableProvider,
            &["无可用", "無可用", "no_available", "no_eligible"],
        ),
        (
            UsageErrorType::CapacityUnavailable,
            &[
                "容量",
                "并发",
                "併發",
                "队列",
                "佇列",
                "capacity",
                "concurrency",
            ],
        ),
        (
            UsageErrorType::ProviderInfrastructureUnavailable,
            &["基础设施", "基礎設施", "infrastructure"],
        ),
        (
            UsageErrorType::Unauthorized,
            &[
                "认证",
                "認證",
                "权限",
                "權限",
                "未授权",
                "未授權",
                "unauthorized",
                "permission",
                "policy_denied",
            ],
        ),
        (
            UsageErrorType::ContinuationRecoveryRequired,
            &["续接", "續接", "恢复", "恢復", "continuation"],
        ),
        (
            UsageErrorType::InvalidRequest,
            &[
                "无效",
                "無效",
                "超限",
                "过长",
                "過長",
                "invalid",
                "unsupported",
                "message_too_big",
            ],
        ),
        (
            UsageErrorType::ProcessInterrupted,
            &[
                "中断",
                "中斷",
                "未完成",
                "内部错误",
                "內部錯誤",
                "interrupted",
                "terminated",
                "incomplete",
                "internal_error",
            ],
        ),
    ];
    aliases
        .iter()
        .filter_map(|(error_type, keywords)| {
            keywords
                .iter()
                .any(|keyword| value.contains(keyword))
                .then_some(*error_type)
        })
        .collect()
}
