//! 服务端错误族真实 SQL 的请求去重、来源隔离和尝试观测合同

use super::*;
use gateway_core::time::DeploymentTimeZone;

#[tokio::test]
async fn summary_counts_stream_errors_legacy_codes_and_intermediate_attempts_once() {
    let Some(db) = TestDatabase::create("server_error_summary").await else {
        return;
    };
    let now = Utc::now();
    let rows = [
        (
            "stream",
            200,
            Some(503),
            "upstream_error",
            Some("server_overloaded"),
            None,
            None,
            1,
        ),
        (
            "infra",
            503,
            None,
            "provider_infrastructure_unavailable",
            None,
            None,
            None,
            0,
        ),
        (
            "none",
            503,
            None,
            "no_available_provider",
            None,
            None,
            None,
            0,
        ),
        (
            "raw",
            200,
            None,
            "upstream_error",
            None,
            None,
            Some(
                r#"{"upstream":"{\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_overloaded\",\"status\":503}}}"}"#,
            ),
            1,
        ),
        (
            "message",
            200,
            None,
            "upstream_error",
            None,
            Some("The server is overloaded. Try again later."),
            None,
            1,
        ),
        (
            "other",
            500,
            Some(500),
            "upstream_error",
            Some("server_error"),
            None,
            None,
            1,
        ),
        ("recovered", 200, Some(200), "", None, None, None, 2),
        ("success", 200, Some(200), "", None, None, None, 1),
    ];
    for (id, client, upstream, kind, code, message, details, attempts) in rows {
        sqlx::query("insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,provider_account_ref,upstream_transport,outcome,client_status_code,upstream_status_code,error_kind,attempt_count,started_at,recovery_lease_expires_at,completed_at,error_details,request_observation_json) values($1,'key','responses','http_sse','openai','acct_synthetic','http_sse',$2,$3,$4,nullif($5,''),$6,$7,$7+interval '1 hour',$7,$8,$9)")
            .bind(id).bind(if kind.is_empty() { "succeeded" } else { "failed" }).bind(client).bind(upstream)
            .bind(kind).bind(attempts).bind(now).bind(details)
            .bind(serde_json::json!({"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"account":{"name":"Synthetic","authenticationKind":"oauth"},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]},"error":{"providerErrorCode":code,"message":message}}))
            .execute(&db.pool).await.unwrap();
    }
    for (id, request, attempt) in [
        ("event1", "recovered", 1),
        ("event2", "recovered", 1),
        ("event3", "stream", 1),
    ] {
        sqlx::query("insert into ops_events(id,model_request_id,attempt_index,level,component,operation,failure_kind,status_code,provider_error_code,message,created_at) values($1,$2,$3,'error','openai','responses','upstream_error',503,'server_overloaded','overloaded',$4)")
            .bind(id).bind(request).bind(attempt).bind(now).execute(&db.pool).await.unwrap();
    }
    let range = admin_observability::TimeRange::new(
        now - TimeDelta::seconds(1),
        now + TimeDelta::seconds(1),
    )
    .unwrap();
    let summary = admin_observability_store(&db.pool)
        .server_error_summary(range, DeploymentTimeZone::default(), None)
        .await
        .unwrap();
    assert_eq!(summary.totals.retained_requests, 8);
    assert_eq!(summary.totals.server_related_requests, 7);
    assert_eq!(summary.totals.terminal_server_related_requests, 6);
    assert_eq!(summary.totals.overloaded, 4);
    assert_eq!(summary.totals.status503, 5);
    assert_eq!(summary.totals.infrastructure503, 1);
    assert_eq!(summary.totals.no_provider503, 1);
    assert_eq!(summary.totals.upstream503, 3);
    assert_eq!(summary.totals.client200_errors, 4);
    assert_eq!(summary.totals.legacy_message_matches, 1);
    assert_eq!(summary.attempts.observed, 7);
    assert_eq!(summary.attempts.overloaded, 4);
    assert_eq!(summary.attempts.coverage, "observed_only");
    assert_eq!(summary.daily.len(), 1);
    let empty =
        admin_observability::TimeRange::new(now + TimeDelta::days(1), now + TimeDelta::days(2))
            .unwrap();
    let summary = admin_observability_store(&db.pool)
        .server_error_summary(empty, DeploymentTimeZone::default(), None)
        .await
        .unwrap();
    assert_eq!(summary.totals.retained_requests, 0);
    assert!(summary.daily.is_empty());
    assert!(summary.earliest_retained_at.is_none());
    db.close().await;
}

#[tokio::test]
async fn summary_and_drilldown_share_requests_across_retries_accounts_and_date_boundaries() {
    use gateway_admin::model::observability::{UsageErrorType, UsageRecordStatus};

    let Some(db) = TestDatabase::create("server_error_drilldown").await else {
        return;
    };
    let start = "2026-10-23T00:00:00Z"
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    let end = start + TimeDelta::days(2);
    for (id, provider, account, outcome, attempts, offset, kind, status, code) in [
        ("retry", "openai", "B", "succeeded", 3, 15, None, 200, None),
        (
            "failed",
            "openai",
            "A",
            "failed",
            2,
            17,
            Some("upstream_error"),
            503,
            Some("server_is_overloaded"),
        ),
        (
            "cancelled",
            "openai",
            "B",
            "cancelled",
            2,
            18,
            None,
            200,
            None,
        ),
        (
            "incomplete",
            "openai",
            "C",
            "incomplete",
            2,
            19,
            Some("process_terminated"),
            200,
            None,
        ),
        (
            "infra",
            "openai",
            "D",
            "failed",
            0,
            20,
            Some("provider_infrastructure_unavailable"),
            503,
            None,
        ),
        ("clean", "openai", "B", "succeeded", 1, 21, None, 200, None),
        (
            "other_provider",
            "xai",
            "B",
            "succeeded",
            2,
            22,
            None,
            200,
            None,
        ),
        (
            "at_end",
            "openai",
            "A",
            "failed",
            1,
            48,
            Some("upstream_error"),
            503,
            Some("server_is_overloaded"),
        ),
    ] {
        sqlx::query("insert into model_requests(id,client_api_key_ref,operation,client_transport,provider_kind,provider_account_ref,upstream_transport,outcome,client_status_code,upstream_status_code,error_kind,attempt_count,started_at,recovery_lease_expires_at,completed_at,downstream_committed_at,request_observation_json) values($1,'key','responses','http_sse',$2,$3,'http_sse',$4,case when $6='provider_infrastructure_unavailable' then 503 else 200 end,case when $6='provider_infrastructure_unavailable' then null else $5 end,$6,$7,$8,$8+interval '1 hour',$8,case when $4='succeeded' then $8 end,$9)")
            .bind(id).bind(provider).bind(account).bind(outcome).bind(status).bind(kind).bind(attempts)
            .bind(start + TimeDelta::hours(offset))
            .bind(serde_json::json!({"request":{"configRevision":1,"protocol":"openai","endpoint":"/v1/responses","compact":false},"account":{"name":account,"authenticationKind":"oauth"},"routing":{"scope":"all","groupRefs":[],"groupNamesSnapshot":[]},"error":{"providerErrorCode":code}}))
            .execute(&db.pool).await.unwrap();
    }
    for (id, request, attempt, provider, account) in [
        ("retry_1", Some("retry"), 1, "openai", "A"),
        ("retry_1_duplicate", Some("retry"), 1, "openai", "A"),
        ("retry_2", Some("retry"), 2, "openai", "B"),
        ("failed_1", Some("failed"), 1, "openai", "A"),
        ("failed_final_duplicate", Some("failed"), 2, "openai", "A"),
        ("cancelled_1", Some("cancelled"), 1, "openai", "A"),
        ("incomplete_1", Some("incomplete"), 1, "openai", "C"),
        ("other_provider_1", Some("other_provider"), 1, "xai", "A"),
        ("diagnostic", None, 1, "openai", "A"),
    ] {
        // 事件时间与请求日桶刻意不同，独立诊断只进入运维事件列表
        sqlx::query("insert into ops_events(id,model_request_id,attempt_index,level,component,operation,provider_kind,provider_account_ref,failure_kind,status_code,provider_error_code,message,created_at) values($1,$2,$3,'error','openai','responses',$4,$5,'upstream_error',503,'server_is_overloaded','Server is overloaded',$6)")
            .bind(id).bind(request).bind(attempt).bind(provider).bind(account).bind(start + TimeDelta::hours(23))
            .execute(&db.pool).await.unwrap();
    }
    let range = admin_observability::TimeRange::new(start, end).unwrap();
    let summary = admin_observability_store(&db.pool)
        .server_error_summary(
            range,
            DeploymentTimeZone::default(),
            Some("openai".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(summary.totals.retained_requests, 6);
    assert_eq!(summary.totals.overloaded, 4);
    assert_eq!(summary.totals.terminal_server_related_requests, 2);
    assert_eq!(summary.attempts.observed, 10);
    assert_eq!(summary.attempts.overloaded, 6);
    assert_eq!(
        summary
            .daily
            .iter()
            .map(|day| day.counts.overloaded)
            .sum::<u64>(),
        4
    );
    assert_eq!(summary.daily.len(), 2);
    assert_eq!(summary.daily[0].counts.overloaded, 1);
    assert_eq!(summary.daily[1].counts.overloaded, 3);

    let repo = observability_repository(&db.pool);
    let store_range = ObservabilityRange::new(start, end).unwrap();
    for keyword in [
        None,
        Some("server is overloaded"),
        Some("server_is_overloaded"),
    ] {
        let mut ids = Vec::new();
        for page in 1..=3 {
            let result = repo
                .list_usage_records(UsageRecordQuery {
                    range: store_range,
                    status: UsageRecordStatus::All,
                    filter: UsageRecordFilter {
                        provider_kind: Some("openai".to_owned()),
                        error: keyword.map(str::to_owned),
                        error_types: if keyword.is_none() {
                            vec![UsageErrorType::Overloaded]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    },
                    current_page: page,
                    page_size: ObservabilityPageSize::new(2).unwrap(),
                })
                .await
                .unwrap();
            assert_eq!(result.total, summary.totals.overloaded);
            for row in &result.items {
                let matched = row.matched_error.as_ref().unwrap();
                if row.id == "failed" {
                    assert_eq!(row.outcome.as_str(), "failed");
                    assert_eq!(matched.source, "terminal");
                } else {
                    assert_eq!(matched.source, "attempt");
                    assert!(row.error_code.is_none());
                    if row.id == "retry" {
                        assert_eq!(row.outcome.as_str(), "succeeded");
                        assert_eq!(matched.attempt_index, Some(2));
                    }
                }
            }
            ids.extend(result.items.into_iter().map(|row| row.id));
        }
        assert_eq!(ids, ["incomplete", "cancelled", "failed", "retry"]);
    }
    // 请求账号沿用终态快照，不能把其他账号的中间失败误算为该账号最终失败
    for (account, expected) in [
        ("A", vec!["failed"]),
        ("B", vec!["cancelled", "retry"]),
        ("C", vec!["incomplete"]),
    ] {
        let result = repo
            .list_usage_records(UsageRecordQuery {
                range: store_range,
                status: UsageRecordStatus::All,
                filter: UsageRecordFilter {
                    provider_kind: Some("openai".to_owned()),
                    provider_account_name: Some(account.to_owned()),
                    error_types: vec![UsageErrorType::Overloaded],
                    ..Default::default()
                },
                current_page: 1,
                page_size: ObservabilityPageSize::new(10).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(result.total, expected.len() as u64);
        assert_eq!(
            result
                .items
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    let events = repo
        .list_ops_errors(OpsErrorQuery {
            range: store_range,
            filter: OpsErrorFilter {
                provider_kind: Some("openai".to_owned()),
                search: Some("server is overloaded".to_owned()),
                ..Default::default()
            },
            current_page: 1,
            page_size: ObservabilityPageSize::new(100).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(events.total, 9);
    let events_a = repo
        .list_ops_errors(OpsErrorQuery {
            range: store_range,
            filter: OpsErrorFilter {
                provider_kind: Some("openai".to_owned()),
                provider_account_ref: Some("A".to_owned()),
                search: Some("server_is_overloaded".to_owned()),
                ..Default::default()
            },
            current_page: 1,
            page_size: ObservabilityPageSize::new(100).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(events_a.total, 7);
    assert!(
        events_a
            .items
            .iter()
            .all(|event| event.provider_account_ref.as_deref() == Some("A"))
    );
    db.close().await;
}
