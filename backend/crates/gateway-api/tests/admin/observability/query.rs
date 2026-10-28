//! 查询 wire 校验的固定合同测试

use gateway_api::admin::observability::{
    DashboardQuery, DiagnosticDimension, DiagnosticsQuery, OpsQuery, TrendKind,
    UsageAnalyticsQuery, UsageQuery, parse_attempt_index, parse_datetime, parse_status,
};
use serde_json::json;

#[test]
fn dashboard_query_should_parse_error_trends() {
    let query: DashboardQuery = serde_json::from_value(json!({"kind": "errors"})).unwrap();
    assert_eq!(query.trend_kind().unwrap(), TrendKind::Errors);
}

#[test]
fn dashboard_query_should_reject_unknown_trend_kind() {
    let query: DashboardQuery = serde_json::from_value(json!({"kind": "secret"})).unwrap();
    assert_eq!(query.trend_kind().unwrap_err().field(), "kind");
}

#[test]
fn usage_query_should_accept_page_number_and_size() {
    let query: UsageQuery = serde_json::from_value(json!({
        "currentPage": 129,
        "pageSize": 10
    }))
    .unwrap();
    assert_eq!(query.validate_pagination().unwrap(), (129, 10));
}

#[test]
fn observability_queries_should_reject_unknown_fields() {
    let query = json!({"unknownFilter": true});
    assert!(serde_json::from_value::<DashboardQuery>(query.clone()).is_err());
    assert!(serde_json::from_value::<UsageQuery>(query.clone()).is_err());
    assert!(serde_json::from_value::<UsageAnalyticsQuery>(query.clone()).is_err());
    assert!(serde_json::from_value::<OpsQuery>(query.clone()).is_err());
    assert!(serde_json::from_value::<DiagnosticsQuery>(query).is_err());
}

#[test]
fn ops_query_should_reject_page_size_above_maximum() {
    let query: OpsQuery = serde_json::from_value(json!({"pageSize": 101})).unwrap();
    assert_eq!(query.validate_pagination().unwrap_err().field(), "pageSize");
}

#[test]
fn diagnostics_query_should_keep_wire_dimension_name() {
    let query: DiagnosticsQuery =
        serde_json::from_value(json!({"dimension": "failure_class"})).unwrap();
    assert_eq!(query.dimension().unwrap(), DiagnosticDimension::Failure);
    assert_eq!(DiagnosticDimension::Failure.display_name(), "failureClass");
}

#[test]
fn scalar_query_parsers_should_reject_out_of_range_values_without_echoing_input() {
    assert_eq!(parse_status(Some(99)).unwrap_err().field(), "statusCode");
    assert_eq!(
        parse_attempt_index(Some(0)).unwrap_err().field(),
        "attemptIndex"
    );
    assert_eq!(
        parse_datetime(Some("not-a-time")).unwrap_err().field(),
        "timeRange"
    );
}

#[test]
fn usage_error_types_should_round_trip_canonical_values() {
    use gateway_admin::model::observability::UsageErrorType;

    for (value, error_type) in [
        ("overloaded", UsageErrorType::Overloaded),
        ("status_503", UsageErrorType::Status503),
        ("rate_limited", UsageErrorType::RateLimited),
        ("upstream_unavailable", UsageErrorType::UpstreamUnavailable),
        ("timeout", UsageErrorType::Timeout),
        ("no_available_provider", UsageErrorType::NoAvailableProvider),
        ("capacity_unavailable", UsageErrorType::CapacityUnavailable),
        (
            "provider_infrastructure_unavailable",
            UsageErrorType::ProviderInfrastructureUnavailable,
        ),
        ("unauthorized", UsageErrorType::Unauthorized),
        (
            "continuation_recovery_required",
            UsageErrorType::ContinuationRecoveryRequired,
        ),
        ("invalid_request", UsageErrorType::InvalidRequest),
        ("process_interrupted", UsageErrorType::ProcessInterrupted),
    ] {
        assert_eq!(UsageErrorType::parse(value), Some(error_type));
        assert_eq!(error_type.as_str(), value);
    }
    assert_eq!(UsageErrorType::parse("overloaded_alias"), None);
}

#[tokio::test]
async fn usage_query_should_parse_error_filters_and_status_precedence() {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use gateway_admin::model::observability::{UsageErrorType, UsageRecordStatus};
    use tower::ServiceExt as _;

    let fixture = crate::admin::AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (suffix, status, error, error_types) in [
        (
            "?error=%20overloaded%20",
            UsageRecordStatus::All,
            Some("overloaded"),
            vec![],
        ),
        (
            "?error=server%20is%20overloaded",
            UsageRecordStatus::All,
            Some("server is overloaded"),
            vec![],
        ),
        (
            "?errorTypes=timeout",
            UsageRecordStatus::All,
            None,
            vec![UsageErrorType::Timeout],
        ),
        (
            "?errorTypes=rate_limited,%20overloaded,rate_limited,%20,%20timeout",
            UsageRecordStatus::All,
            None,
            vec![
                UsageErrorType::Overloaded,
                UsageErrorType::RateLimited,
                UsageErrorType::Timeout,
            ],
        ),
        (
            "?errorTypes=,%20,%20",
            UsageRecordStatus::Success,
            None,
            vec![],
        ),
        (
            "?error=overloaded&status=all",
            UsageRecordStatus::All,
            Some("overloaded"),
            vec![],
        ),
        (
            "?error=overloaded&status=error",
            UsageRecordStatus::Error,
            Some("overloaded"),
            vec![],
        ),
        (
            "?error=overloaded&status=success",
            UsageRecordStatus::Success,
            Some("overloaded"),
            vec![],
        ),
        (
            "?requestId=req_locator&errorTypes=timeout",
            UsageRecordStatus::All,
            None,
            vec![UsageErrorType::Timeout],
        ),
    ] {
        let response = gateway_api::admin::observability::router::<crate::admin::AdminTestState>()
            .with_state(fixture.state())
            .oneshot(
                Request::builder()
                    .uri(format!("/api/admin/usage/records{suffix}"))
                    .header(header::COOKIE, "cpr_session=valid-session")
                    .header("x-request-id", "req_usage_error_query")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{suffix}");
        let observations = fixture.observations.lock().unwrap();
        let query = observations.records.last().unwrap();
        assert_eq!(query.status, status, "{suffix}");
        assert_eq!(query.filter.error.as_deref(), error, "{suffix}");
        assert_eq!(query.filter.error_types, error_types, "{suffix}");
    }
}

#[tokio::test]
async fn usage_query_should_reject_invalid_error_filters_and_enforce_error_limit() {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt as _;

    let fixture = crate::admin::AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for suffix in ["?errorTypes=unknown", "?error=%00"] {
        let response = gateway_api::admin::observability::router::<crate::admin::AdminTestState>()
            .with_state(fixture.state())
            .oneshot(
                Request::builder()
                    .uri(format!("/api/admin/usage/records{suffix}"))
                    .header(header::COOKIE, "cpr_session=valid-session")
                    .header("x-request-id", "req_usage_error_query_invalid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{suffix}");
    }

    let exact = format!("/api/admin/usage/records?error={}", "x".repeat(512));
    let (status, _) = crate::admin::quota_policy::request(&fixture, &exact, None, true).await;
    assert_eq!(status, StatusCode::OK);
    {
        let observations = fixture.observations.lock().unwrap();
        let query = observations.records.last().unwrap();
        assert_eq!(
            query.status,
            gateway_admin::model::observability::UsageRecordStatus::All
        );
        assert_eq!(query.filter.error.as_ref().map(String::len), Some(512));
    }

    let oversized = format!("?error={}", "x".repeat(513));
    let response = gateway_api::admin::observability::router::<crate::admin::AdminTestState>()
        .with_state(fixture.state())
        .oneshot(
            Request::builder()
                .uri(format!("/api/admin/usage/records{oversized}"))
                .header(header::COOKIE, "cpr_session=valid-session")
                .header("x-request-id", "req_usage_error_query_oversized")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
