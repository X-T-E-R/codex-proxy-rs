//! 错误搜索、终态与尝试分类，以及明细计数和时间分页的一致性

use super::*;
use gateway_admin::model::observability::{UsageErrorType, UsageRecordStatus};

#[tokio::test]
async fn error_filters_match_terminal_and_attempt_sources_with_exact_pages() {
    let Some(db) = TestDatabase::create("usage_error_filters").await else {
        return;
    };
    let start = Utc::now() - TimeDelta::hours(2);
    let cases = [
        (
            UsageErrorType::Overloaded,
            "upstream_error",
            Some("server_overloaded"),
            None,
        ),
        (UsageErrorType::Status503, "upstream_error", None, Some(503)),
        (
            UsageErrorType::RateLimited,
            "quota_exhausted",
            None,
            Some(429),
        ),
        (UsageErrorType::UpstreamUnavailable, "protocol", None, None),
        (UsageErrorType::Timeout, "timeout", None, None),
        (
            UsageErrorType::NoAvailableProvider,
            "no_eligible_account",
            None,
            None,
        ),
        (
            UsageErrorType::CapacityUnavailable,
            "account_capacity_unavailable",
            None,
            None,
        ),
        (
            UsageErrorType::ProviderInfrastructureUnavailable,
            "provider_infrastructure_unavailable",
            None,
            None,
        ),
        (
            UsageErrorType::Unauthorized,
            "permission_denied",
            None,
            Some(403),
        ),
        (
            UsageErrorType::ContinuationRecoveryRequired,
            "continuation_recovery_required",
            None,
            None,
        ),
        (
            UsageErrorType::InvalidRequest,
            "invalid_request",
            None,
            Some(400),
        ),
        (
            UsageErrorType::ProcessInterrupted,
            "process_terminated",
            None,
            None,
        ),
    ];
    for (index, (_, kind, code, status)) in cases.iter().enumerate() {
        for event in [false, true] {
            let id = format!("{}_{index:02}", if event { "event" } else { "terminal" });
            seed_request(
                &db.pool,
                &id,
                start + TimeDelta::minutes(index as i64),
                false,
            )
            .await;
            set_error(&db.pool, &id, event, kind, *code, *status, None, None).await;
        }
    }
    for (id, event, message, details) in [
        (
            "raw_terminal",
            false,
            None,
            Some(r#"{"upstream":"{\"response\":{\"error\":{\"type\":\"server_overloaded\"}}}"}"#),
        ),
        (
            "raw_event",
            true,
            None,
            Some(r#"{"error":{"code":"server_overloaded"}}"#),
        ),
        (
            "message_terminal",
            false,
            Some("The server is overloaded. Try again later."),
            None,
        ),
        ("message_event", true, Some("server_is_overloaded"), None),
        (
            "malformed",
            false,
            Some("unrelated failure"),
            Some("{not json"),
        ),
    ] {
        seed_request(&db.pool, id, start, false).await;
        set_error(
            &db.pool,
            id,
            event,
            "upstream_error",
            None,
            None,
            message,
            details,
        )
        .await;
    }
    // 成功重试仍可用显式 success/all 找到，但不应混入最终失败视图
    seed_request(&db.pool, "recovered", start, true).await;
    set_error(
        &db.pool,
        "recovered",
        true,
        "upstream_error",
        Some("server_overloaded"),
        Some(503),
        None,
        None,
    )
    .await;
    seed_request(&db.pool, "clean_success", start, true).await;
    // 续接原因来自请求观测 JSON，不是 ops_events 的列
    seed_request(&db.pool, "continuation_reason", start, false).await;
    sqlx::query("update model_requests set request_observation_json=jsonb_set(request_observation_json,'{continuation}','{\"unavailableReason\":\"state_expired\"}'::jsonb) where id='continuation_reason'")
        .execute(&db.pool).await.unwrap();
    seed_request(&db.pool, "incomplete", start, false).await;
    sqlx::query("update model_requests set outcome='incomplete' where id='incomplete'")
        .execute(&db.pool)
        .await
        .unwrap();
    for (id, event) in [("raw503_terminal", false), ("raw503_event", true)] {
        seed_request(&db.pool, id, start, false).await;
        set_error(
            &db.pool,
            id,
            event,
            "unclassified",
            None,
            None,
            None,
            Some(r#"{"response":{"error":{"status":503}}}"#),
        )
        .await;
    }
    super::super::drain_analytics(&db.pool).await;

    let range = ObservabilityRange::new(start, start + TimeDelta::hours(1)).unwrap();
    for (index, (error_type, ..)) in cases.iter().enumerate() {
        let mut expected = vec![format!("terminal_{index:02}"), format!("event_{index:02}")];
        if *error_type == UsageErrorType::Overloaded {
            expected.extend(
                [
                    "raw_terminal",
                    "raw_event",
                    "message_terminal",
                    "message_event",
                ]
                .map(str::to_owned),
            );
        } else if *error_type == UsageErrorType::UpstreamUnavailable {
            expected.extend(
                [
                    "terminal_00",
                    "event_00",
                    "terminal_01",
                    "event_01",
                    "raw_terminal",
                    "raw_event",
                    "message_terminal",
                    "message_event",
                    "malformed",
                ]
                .map(str::to_owned),
            );
        } else if *error_type == UsageErrorType::ContinuationRecoveryRequired {
            expected.push("continuation_reason".to_owned());
        } else if *error_type == UsageErrorType::Status503 {
            expected.extend(["raw503_terminal", "raw503_event"].map(str::to_owned));
        } else if *error_type == UsageErrorType::ProcessInterrupted {
            expected.push("incomplete".to_owned());
        }
        assert_pages(
            &db.pool,
            range,
            UsageRecordStatus::Error,
            UsageRecordFilter {
                error_types: vec![*error_type],
                ..Default::default()
            },
            &expected,
        )
        .await;
    }

    let overloaded = [
        "terminal_00",
        "raw_terminal",
        "raw_event",
        "message_terminal",
        "message_event",
        "event_00",
    ]
    .map(str::to_owned);
    for keyword in ["过载", "過載", "overloaded", "server_overloaded"] {
        for general_search in [false, true] {
            let filter = if general_search {
                UsageRecordFilter {
                    search: Some(keyword.to_owned()),
                    ..Default::default()
                }
            } else {
                UsageRecordFilter {
                    error: Some(keyword.to_owned()),
                    ..Default::default()
                }
            };
            assert_pages(
                &db.pool,
                range,
                UsageRecordStatus::Error,
                filter,
                &overloaded,
            )
            .await;
        }
    }
    for (keyword, expected) in [
        ("server_is_overloaded", overloaded.to_vec()),
        ("Try again later", vec!["message_terminal".to_owned()]),
        (
            "503",
            ["terminal_01", "event_01", "raw503_terminal", "raw503_event"]
                .map(str::to_owned)
                .to_vec(),
        ),
        (
            "限流",
            vec!["terminal_02".to_owned(), "event_02".to_owned()],
        ),
        (
            "quota_exhausted",
            vec!["terminal_02".to_owned(), "event_02".to_owned()],
        ),
        ("no_match_%", vec![]),
    ] {
        assert_pages(
            &db.pool,
            range,
            UsageRecordStatus::Error,
            UsageRecordFilter {
                error: Some(keyword.to_owned()),
                ..Default::default()
            },
            &expected,
        )
        .await;
    }
    let mut multi = overloaded.to_vec();
    multi.extend(["terminal_04", "event_04"].map(str::to_owned));
    let selected = UsageRecordFilter {
        error_types: vec![UsageErrorType::Overloaded, UsageErrorType::Timeout],
        provider_kind: Some("openai".to_owned()),
        model: Some("model_errors".to_owned()),
        ..Default::default()
    };
    assert_pages(
        &db.pool,
        range,
        UsageRecordStatus::Error,
        selected.clone(),
        &multi,
    )
    .await;
    assert_pages(
        &db.pool,
        range,
        UsageRecordStatus::Error,
        UsageRecordFilter {
            error: Some("超时".to_owned()),
            ..selected.clone()
        },
        &["terminal_04".to_owned(), "event_04".to_owned()],
    )
    .await;
    assert_pages(
        &db.pool,
        range,
        UsageRecordStatus::Error,
        UsageRecordFilter {
            provider_kind: Some("xai".to_owned()),
            ..selected
        },
        &[],
    )
    .await;
    assert_pages(
        &db.pool,
        range,
        UsageRecordStatus::Success,
        UsageRecordFilter {
            error_types: vec![UsageErrorType::Overloaded],
            ..Default::default()
        },
        &["recovered".to_owned()],
    )
    .await;
    let mut with_recovered = overloaded.to_vec();
    with_recovered.push("recovered".to_owned());
    assert_pages(
        &db.pool,
        range,
        UsageRecordStatus::All,
        UsageRecordFilter {
            error: Some("过载".to_owned()),
            ..Default::default()
        },
        &with_recovered,
    )
    .await;
    // 清空筛选后恢复成功入口，而不是残留上一轮错误谓词
    assert_pages(
        &db.pool,
        range,
        UsageRecordStatus::Success,
        UsageRecordFilter::default(),
        &["recovered".to_owned(), "clean_success".to_owned()],
    )
    .await;
    let empty_range =
        ObservabilityRange::new(start + TimeDelta::hours(1), start + TimeDelta::hours(2)).unwrap();
    assert_pages(
        &db.pool,
        empty_range,
        UsageRecordStatus::Error,
        UsageRecordFilter {
            error_types: vec![UsageErrorType::Overloaded],
            ..Default::default()
        },
        &[],
    )
    .await;
    db.close().await;
}

async fn seed_request(pool: &PgPool, id: &str, at: chrono::DateTime<Utc>, success: bool) {
    sqlx::query("insert into model_requests(id,client_api_key_ref,operation,client_transport,requested_model_id,provider_kind,provider_account_ref,upstream_transport,outcome,client_status_code,attempt_count,started_at,recovery_lease_expires_at,completed_at,downstream_committed_at,request_observation_json) values($1,'key_errors','responses','http_sse','model_errors','openai','account_errors','http_sse',$2,200,1,$3,$3+interval '1 hour',$3,case when $4 then $3 end,$5)")
        .bind(id).bind(if success { "succeeded" } else { "failed" }).bind(at).bind(success)
        .bind(serde_json::json!({"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]}}))
        .execute(pool).await.unwrap();
}

#[expect(
    clippy::too_many_arguments,
    reason = "夹具分别提供终态和尝试事件的错误事实"
)]
async fn set_error(
    pool: &PgPool,
    id: &str,
    event: bool,
    kind: &str,
    code: Option<&str>,
    status: Option<i32>,
    message: Option<&str>,
    details: Option<&str>,
) {
    if event {
        sqlx::query("insert into ops_events(id,model_request_id,attempt_index,level,component,operation,failure_kind,status_code,provider_error_code,message,error_details,created_at) select $1||'_attempt',id,1,'error','openai','responses',$2,$3,$4,coalesce($5,'attempt failed'),$6,started_at from model_requests where id=$1")
            .bind(id).bind(kind).bind(status).bind(code).bind(message).bind(details)
            .execute(pool).await.unwrap();
    } else {
        sqlx::query("update model_requests set error_kind=$2,upstream_status_code=$3,error_details=$4,request_observation_json=jsonb_set(request_observation_json,'{error}',jsonb_strip_nulls(jsonb_build_object('providerErrorCode',$5::text,'message',$6::text))) where id=$1")
            .bind(id).bind(kind).bind(status).bind(details).bind(code).bind(message)
            .execute(pool).await.unwrap();
    }
}

async fn assert_pages(
    pool: &PgPool,
    range: ObservabilityRange,
    status: UsageRecordStatus,
    filter: UsageRecordFilter,
    expected: &[String],
) {
    // 独立给定匹配集合，只借源记录的排序键计算预期页，不复用被测筛选谓词
    let sorted: Vec<String> = sqlx::query_scalar(
        "select id from model_requests where id=any($1) order by started_at desc,id desc",
    )
    .bind(expected)
    .fetch_all(pool)
    .await
    .unwrap();
    let repo = observability_repository(pool);
    for current_page in 1..=(expected.len() as u32 / 2 + 2) {
        let page = repo
            .list_usage_records(UsageRecordQuery {
                range,
                status,
                filter: filter.clone(),
                current_page,
                page_size: ObservabilityPageSize::new(2).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(
            page.total,
            expected.len() as u64,
            "{filter:?}, page {current_page}"
        );
        assert_eq!(
            page.items.iter().map(|r| &r.id).collect::<Vec<_>>(),
            sorted
                .iter()
                .skip((current_page as usize - 1) * 2)
                .take(2)
                .collect::<Vec<_>>(),
            "{filter:?}, page {current_page}"
        );
    }
}
