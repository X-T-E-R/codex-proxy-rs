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
            Some(r#"{"upstream":"{\"error\":{\"type\":\"server_overloaded\"}}"}"#),
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
        sqlx::query("insert into model_requests(id,client_api_key_ref,operation,client_transport,outcome,client_status_code,upstream_status_code,error_kind,attempt_count,started_at,recovery_lease_expires_at,completed_at,error_details,request_observation_json) values($1,'key','responses','http_sse',$2,$3,$4,nullif($5,''),$6,$7,$7+interval '1 hour',$7,$8,$9)")
            .bind(id).bind(if kind.is_empty() { "succeeded" } else { "failed" }).bind(client).bind(upstream)
            .bind(kind).bind(attempts).bind(now).bind(details)
            .bind(serde_json::json!({"error":{"providerErrorCode":code,"message":message}}))
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
        .server_error_summary(range, DeploymentTimeZone::default())
        .await
        .unwrap();
    assert_eq!(summary.totals.retained_requests, 8);
    assert_eq!(summary.totals.server_related_requests, 7);
    assert_eq!(summary.totals.terminal_server_related_requests, 6);
    assert_eq!(summary.totals.overloaded, 4);
    assert_eq!(summary.totals.status503, 4);
    assert_eq!(summary.totals.infrastructure503, 1);
    assert_eq!(summary.totals.no_provider503, 1);
    assert_eq!(summary.totals.upstream503, 2);
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
        .server_error_summary(empty, DeploymentTimeZone::default())
        .await
        .unwrap();
    assert_eq!(summary.totals.retained_requests, 0);
    assert!(summary.daily.is_empty());
    assert!(summary.earliest_retained_at.is_none());
    db.close().await;
}
