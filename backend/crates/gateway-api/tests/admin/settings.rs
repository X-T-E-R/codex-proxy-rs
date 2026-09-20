use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use gateway_api::admin::settings::{self, UpdateRuntimeSettingsRequest};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{AdminTestFixture, AdminTestState};

fn app(state: AdminTestState) -> Router {
    settings::router::<AdminTestState>().with_state(state)
}

fn request(method: Method, path: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, "cpr_admin_session=valid-session")
        .header("x-request-id", "req_admin_settings");
    let body = if let Some(value) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(value.to_string())
    } else {
        Body::empty()
    };
    builder.body(body).expect("build settings request")
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("parse response JSON")
}

fn update_body() -> Value {
    json!({
        "modelMappings": {
            "gpt-5.4": "gpt-5.5",
            "grok-latest": "grok-4.5"
        },
        "refreshMarginSeconds": 1800,
        "refreshConcurrency": 4,
        "maxConcurrentPerAccount": 5,
        "requestIntervalMs": 25,
        "capacityQueueRetrySeconds": 3,
        "capacityQueueTimeoutSeconds": 60,
        "rotationStrategy": "round_robin",
        "minCodexDesktopVersion": "26.825.6671",
        "minCodexCliVersion": "0.40.0",
        "usageRetentionDays": 32,
        "opsEventRetentionDays": 31,
        "auditRetentionDays": 91,
        "wsPoolEnabled": true,
        "wsPoolMaxAgeMs": 3_600_000,
        "wsPoolMaxConnecting": 6,
        "wsPoolStreamIdleTimeoutMs": 240_000,
        "wsPoolFastPathBudgetMs": 1_200,
        "overloadCooldownEnabled": true,
        "overloadCooldownThreshold": 2,
        "overloadCooldownSeconds": 120,
        "cyberSessionBlockEnabled": true,
        "cyberSessionBlockTtlSeconds": 600,
        "openaiUserAgent": "Codex Desktop/0.153.4 (Windows 10.0.26100; x86_64)",
        "openaiRequestBodyOverrideEnabled": true,
        "openaiRequestTimezone": "America/Los_Angeles",
        "openaiSearchCountry": "US"
    })
}

#[test]
fn settings_request_should_reject_unknown_rotation_strategy() {
    let mut body = update_body();
    body["rotationStrategy"] = json!("random");
    let request: UpdateRuntimeSettingsRequest =
        serde_json::from_value(body).expect("decode settings");

    assert_eq!(request.validate().unwrap_err().field(), "rotationStrategy");
}

#[test]
fn settings_request_should_reject_non_semver_client_min() {
    let mut body = update_body();
    body["minCodexCliVersion"] = json!("v0.40.0");
    let request: UpdateRuntimeSettingsRequest =
        serde_json::from_value(body).expect("decode settings");

    assert_eq!(
        request.validate().unwrap_err().field(),
        "minCodexCliVersion"
    );
}

#[test]
fn settings_request_should_reject_zero_ws_pool_numeric_fields() {
    // 四个 wsPool 数值字段任一为 0 都在 wire 层按字段名拒绝，不进入 store。
    for field in [
        "wsPoolMaxAgeMs",
        "wsPoolMaxConnecting",
        "wsPoolStreamIdleTimeoutMs",
        "wsPoolFastPathBudgetMs",
    ] {
        let mut body = update_body();
        body[field] = json!(0);
        let request: UpdateRuntimeSettingsRequest =
            serde_json::from_value(body).expect("decode settings");

        assert_eq!(
            request.validate().unwrap_err().field(),
            field,
            "field {field}"
        );
    }
}

#[tokio::test]
async fn settings_post_should_reject_ws_pool_max_connecting_overflow() {
    // maxConnecting 超过 u32 时命令转换失败，按 400 与固定中文消息返回。
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let mut body = update_body();
    body["wsPoolMaxConnecting"] = json!(u64::from(u32::MAX) + 1);
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(body),
        ))
        .await
        .expect("settings update response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = response_json(response).await;
    assert_eq!(payload["message"], json!("wsPoolMaxConnecting 不合法"));
}

#[tokio::test]
async fn settings_post_should_reject_invalid_ws_pool_durations_without_mutation() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let router = app(fixture.state());
    let before = response_json(
        router
            .clone()
            .oneshot(request(Method::GET, "/api/admin/settings", None))
            .await
            .unwrap(),
    )
    .await;
    for field in [
        "wsPoolMaxAgeMs",
        "wsPoolStreamIdleTimeoutMs",
        "wsPoolFastPathBudgetMs",
    ] {
        for (value, status) in [
            (json!(0), StatusCode::BAD_REQUEST),
            (json!(-1), StatusCode::UNPROCESSABLE_ENTITY),
            (json!(1.5), StatusCode::UNPROCESSABLE_ENTITY),
            (json!(i64::MAX as u64 + 1), StatusCode::BAD_REQUEST),
        ] {
            let mut body = update_body();
            body[field] = value;
            let response = router
                .clone()
                .oneshot(request(
                    Method::POST,
                    "/api/admin/settings/update",
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{field}");
        }
    }
    let after = response_json(
        router
            .oneshot(request(Method::GET, "/api/admin/settings", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        before, after,
        "rejected settings must preserve the saved values"
    );
}

#[tokio::test]
async fn settings_post_should_require_all_ws_pool_fields_without_resetting_values() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let router = app(fixture.state());
    let before = response_json(
        router
            .clone()
            .oneshot(request(Method::GET, "/api/admin/settings", None))
            .await
            .unwrap(),
    )
    .await;
    for field in [
        "wsPoolEnabled",
        "wsPoolMaxAgeMs",
        "wsPoolMaxConnecting",
        "wsPoolStreamIdleTimeoutMs",
        "wsPoolFastPathBudgetMs",
    ] {
        let mut body = update_body();
        body.as_object_mut().unwrap().remove(field);
        let response = router
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/admin/settings/update",
                Some(body),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{field}"
        );
    }
    let after = response_json(
        router
            .oneshot(request(Method::GET, "/api/admin/settings", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(before, after);
}

#[test]
fn settings_response_should_cover_the_full_runtime_settings_contract() {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use gateway_admin::model::Revision;
    use gateway_admin::model::settings::RuntimeSettings;
    use gateway_api::admin::settings::RuntimeSettingsView;
    use gateway_core::account::RotationStrategy;
    use gateway_core::routing::{PublicModelId, UpstreamModelId};

    let settings = RuntimeSettings {
        config_revision: Revision::new(7).expect("revision"),
        model_mappings: BTreeMap::from_iter([
            (
                PublicModelId::new("gpt-5.4").expect("public model"),
                UpstreamModelId::new("gpt-5.5").expect("upstream model"),
            ),
            (
                PublicModelId::new("grok-latest").expect("public model"),
                UpstreamModelId::new("grok-4.5").expect("upstream model"),
            ),
        ]),
        refresh_margin_seconds: 1800,
        refresh_concurrency: 4,
        max_concurrent_per_account: 5,
        request_interval_ms: 25,
        capacity_queue_retry_seconds: 3,
        capacity_queue_timeout_seconds: 60,
        rotation_strategy: RotationStrategy::RoundRobin,
        min_codex_desktop_version: Some("26.825.6671".to_owned()),
        min_codex_cli_version: Some("0.40.0".to_owned()),
        usage_retention_days: 32,
        ops_event_retention_days: 31,
        audit_retention_days: 91,
        ws_pool_enabled: true,
        ws_pool_max_age_ms: 3_600_000,
        ws_pool_max_connecting: 6,
        ws_pool_stream_idle_timeout_ms: 240_000,
        ws_pool_fast_path_budget_ms: 1_200,
        overload_cooldown_enabled: true,
        overload_cooldown_threshold: 2,
        overload_cooldown_seconds: 120,
        cyber_session_block_enabled: true,
        cyber_session_block_ttl_seconds: 600,
        openai_user_agent: Some("Codex Desktop/0.153.4 (Windows 10.0.26100; x86_64)".to_owned()),
        openai_request_body_override_enabled: true,
        openai_request_timezone: "America/Los_Angeles".to_owned(),
        openai_search_country: "US".to_owned(),
        updated_at: Utc
            .with_ymd_and_hms(2026, 8, 2, 10, 30, 0)
            .single()
            .expect("timestamp"),
    };

    let value = serde_json::to_value(RuntimeSettingsView::from(settings)).expect("serialize view");
    assert_eq!(
        value,
        json!({
            "modelMappings": {
                "gpt-5.4": "gpt-5.5",
                "grok-latest": "grok-4.5"
            },
            "refreshMarginSeconds": 1800,
            "refreshConcurrency": 4,
            "maxConcurrentPerAccount": 5,
            "requestIntervalMs": 25,
            "capacityQueueRetrySeconds": 3,
            "capacityQueueTimeoutSeconds": 60,
            "rotationStrategy": "round_robin",
            "minCodexDesktopVersion": "26.825.6671",
            "minCodexCliVersion": "0.40.0",
            "usageRetentionDays": 32,
            "opsEventRetentionDays": 31,
            "auditRetentionDays": 91,
            "wsPoolEnabled": true,
            "wsPoolMaxAgeMs": 3_600_000,
            "wsPoolMaxConnecting": 6,
            "wsPoolStreamIdleTimeoutMs": 240_000,
            "wsPoolFastPathBudgetMs": 1_200,
            "overloadCooldownEnabled": true,
            "overloadCooldownThreshold": 2,
            "overloadCooldownSeconds": 120,
            "cyberSessionBlockEnabled": true,
            "cyberSessionBlockTtlSeconds": 600,
            "openaiUserAgent": "Codex Desktop/0.153.4 (Windows 10.0.26100; x86_64)",
            "openaiRequestBodyOverrideEnabled": true,
            "openaiRequestTimezone": "America/Los_Angeles",
            "openaiSearchCountry": "US",
            "updatedAt": "2026-08-02T10:30:00Z"
        })
    );
}

#[test]
fn settings_request_and_response_fields_should_stay_in_lockstep() {
    use std::collections::{BTreeMap, BTreeSet};

    use gateway_admin::model::Revision;
    use gateway_admin::model::settings::RuntimeSettings;
    use gateway_api::admin::settings::RuntimeSettingsView;
    use gateway_core::account::RotationStrategy;
    use gateway_core::routing::{PublicModelId, UpstreamModelId};

    let request: UpdateRuntimeSettingsRequest =
        serde_json::from_value(update_body()).expect("decode settings");
    request.validate().expect("fixture settings must validate");

    let request_fields: BTreeSet<String> = update_body()
        .as_object()
        .expect("request body object")
        .keys()
        .cloned()
        .collect();
    let settings = RuntimeSettings {
        config_revision: Revision::new(7).expect("revision"),
        model_mappings: request
            .model_mappings
            .iter()
            .map(|(public, upstream)| {
                Ok((
                    PublicModelId::new(public.clone())?,
                    UpstreamModelId::new(upstream.clone())?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, gateway_core::error::IdentifierError>>()
            .expect("valid model mappings"),
        refresh_margin_seconds: request.refresh_margin_seconds,
        refresh_concurrency: u32::try_from(request.refresh_concurrency).expect("u32"),
        max_concurrent_per_account: u32::try_from(request.max_concurrent_per_account).expect("u32"),
        request_interval_ms: request.request_interval_ms,
        capacity_queue_retry_seconds: u32::try_from(
            request.capacity_queue_retry_seconds.expect("queue retry"),
        )
        .expect("u32"),
        capacity_queue_timeout_seconds: u32::try_from(
            request
                .capacity_queue_timeout_seconds
                .expect("queue timeout"),
        )
        .expect("u32"),
        rotation_strategy: RotationStrategy::parse(&request.rotation_strategy)
            .expect("fixture rotation strategy"),
        min_codex_desktop_version: request.min_codex_desktop_version,
        min_codex_cli_version: request.min_codex_cli_version,
        usage_retention_days: u32::try_from(request.usage_retention_days).expect("u32"),
        ops_event_retention_days: u32::try_from(request.ops_event_retention_days).expect("u32"),
        audit_retention_days: u32::try_from(request.audit_retention_days).expect("u32"),
        ws_pool_enabled: request.ws_pool_enabled,
        ws_pool_max_age_ms: request.ws_pool_max_age_ms,
        ws_pool_max_connecting: u32::try_from(request.ws_pool_max_connecting).expect("u32"),
        ws_pool_stream_idle_timeout_ms: request.ws_pool_stream_idle_timeout_ms,
        ws_pool_fast_path_budget_ms: request.ws_pool_fast_path_budget_ms,
        overload_cooldown_enabled: request.overload_cooldown_enabled,
        overload_cooldown_threshold: u32::try_from(request.overload_cooldown_threshold)
            .expect("u32"),
        overload_cooldown_seconds: u32::try_from(request.overload_cooldown_seconds).expect("u32"),
        cyber_session_block_enabled: request.cyber_session_block_enabled.expect("cyber enabled"),
        cyber_session_block_ttl_seconds: u32::try_from(
            request.cyber_session_block_ttl_seconds.expect("cyber TTL"),
        )
        .expect("u32"),
        openai_user_agent: request.openai_user_agent,
        openai_request_body_override_enabled: request
            .openai_request_body_override_enabled
            .expect("request body override enabled"),
        openai_request_timezone: request.openai_request_timezone.expect("request timezone"),
        openai_search_country: request.openai_search_country.expect("search country"),
        updated_at: chrono::Utc::now(),
    };

    let response_fields: BTreeSet<String> =
        serde_json::to_value(RuntimeSettingsView::from(settings))
            .expect("serialize view")
            .as_object()
            .expect("view object")
            .keys()
            .cloned()
            .collect();
    let mut expected_fields = request_fields;
    expected_fields.insert("updatedAt".to_owned());

    assert_eq!(response_fields, expected_fields);
}

#[test]
fn settings_request_should_reject_unknown_revision_field() {
    let mut body = update_body();
    body["expectedConfigRevision"] = json!(7);

    assert!(serde_json::from_value::<UpdateRuntimeSettingsRequest>(body).is_err());
}

#[test]
fn settings_request_should_reject_removed_bucket_retention() {
    let mut body = update_body();
    body["bucketRetentionDays"] = json!(365);

    assert!(serde_json::from_value::<UpdateRuntimeSettingsRequest>(body).is_err());
}

#[tokio::test]
async fn settings_get_should_preserve_global_model_mappings() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let response = app(fixture.state())
        .oneshot(request(Method::GET, "/api/admin/settings", None))
        .await
        .expect("settings response");
    let data = response_json(response).await["data"].clone();

    assert_eq!(
        (
            data["modelMappings"]["coding-default"].as_str(),
            data["modelMappings"]["grok-latest"].as_str(),
            data["rotationStrategy"].as_str()
        ),
        (Some("gpt-5.4"), Some("grok-4.5"), Some("smart"))
    );
}

#[tokio::test]
async fn settings_post_should_replace_global_model_mappings() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(update_body()),
        ))
        .await
        .expect("settings update response");
    let data = response_json(response).await["data"].clone();

    assert!(data.get("configRevision").is_none());
    assert_eq!(data["modelMappings"]["gpt-5.4"], "gpt-5.5");
    assert_eq!(data["modelMappings"]["grok-latest"], "grok-4.5");
    // ws_pool 字段经过 wire 校验与设置用例后完整返回；这里的 store 是内存 fixture。
    assert_eq!(data["wsPoolMaxConnecting"], json!(6));
    assert_eq!(data["wsPoolFastPathBudgetMs"], json!(1_200));
    assert_eq!(data["overloadCooldownEnabled"], true);
    assert_eq!(data["overloadCooldownThreshold"], 2);
    assert_eq!(data["overloadCooldownSeconds"], 120);
    assert_eq!(data["cyberSessionBlockEnabled"], true);
    assert_eq!(data["cyberSessionBlockTtlSeconds"], 600);
    assert_eq!(data["openaiUserAgent"], update_body()["openaiUserAgent"]);
    assert_eq!(data["openaiRequestBodyOverrideEnabled"], true);
    assert_eq!(data["openaiRequestTimezone"], "America/Los_Angeles");
    assert_eq!(data["openaiSearchCountry"], "US");
}

#[tokio::test]
async fn request_locale_update_canonicalizes_country_and_preserves_omitted_or_null_fields() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let router = app(fixture.state());
    let mut body = update_body();
    body["openaiRequestBodyOverrideEnabled"] = json!(false);
    body["openaiRequestTimezone"] = json!(" Europe/Berlin ");
    body["openaiSearchCountry"] = json!(" de ");
    let first = router
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(body),
        ))
        .await
        .expect("first locale update");
    assert_eq!(first.status(), StatusCode::OK);
    let data = response_json(first).await["data"].clone();
    assert_eq!(data["openaiRequestBodyOverrideEnabled"], false);
    assert_eq!(data["openaiRequestTimezone"], "Europe/Berlin");
    assert_eq!(data["openaiSearchCountry"], "DE");

    let mut omitted = update_body();
    for field in [
        "openaiRequestBodyOverrideEnabled",
        "openaiRequestTimezone",
        "openaiSearchCountry",
    ] {
        omitted
            .as_object_mut()
            .expect("settings object")
            .remove(field);
    }
    let omitted = router
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(omitted),
        ))
        .await
        .expect("omitted locale update");
    let data = response_json(omitted).await["data"].clone();
    assert_eq!(data["openaiRequestBodyOverrideEnabled"], false);
    assert_eq!(data["openaiRequestTimezone"], "Europe/Berlin");
    assert_eq!(data["openaiSearchCountry"], "DE");

    let mut null = update_body();
    for field in [
        "openaiRequestBodyOverrideEnabled",
        "openaiRequestTimezone",
        "openaiSearchCountry",
    ] {
        null[field] = Value::Null;
    }
    let null = router
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(null),
        ))
        .await
        .expect("null locale update");
    let data = response_json(null).await["data"].clone();
    assert_eq!(data["openaiRequestBodyOverrideEnabled"], false);
    assert_eq!(data["openaiRequestTimezone"], "Europe/Berlin");
    assert_eq!(data["openaiSearchCountry"], "DE");
}

#[tokio::test]
async fn runtime_settings_reject_invalid_values() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (field, value) in [
        ("overloadCooldownThreshold", json!(0)),
        ("overloadCooldownSeconds", json!(0)),
        ("overloadCooldownThreshold", json!(u64::from(u32::MAX) + 1)),
        ("overloadCooldownSeconds", json!(u64::from(u32::MAX) + 1)),
        ("cyberSessionBlockTtlSeconds", json!(0)),
        (
            "cyberSessionBlockTtlSeconds",
            json!(u64::from(u32::MAX) + 1),
        ),
        ("openaiUserAgent", json!("agent\r\nAuthorization: injected")),
        ("openaiUserAgent", json!(" ")),
        ("openaiUserAgent", json!("a".repeat(513))),
        ("openaiRequestTimezone", json!("Pacific")),
        ("openaiRequestTimezone", json!("Not/A_Real_Zone")),
        ("openaiSearchCountry", json!("USA")),
        ("openaiSearchCountry", json!("中")),
    ] {
        let mut body = update_body();
        body[field] = value;
        let response = app(fixture.state())
            .oneshot(request(
                Method::POST,
                "/api/admin/settings/update",
                Some(body),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{field}");
    }

    let mut body = update_body();
    body["cyberSessionBlockTtlSeconds"] = json!(1.5);
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(body),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn old_settings_client_omissions_preserve_each_cyber_field() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let mut body = update_body();
    body.as_object_mut()
        .expect("object")
        .remove("cyberSessionBlockEnabled");
    body.as_object_mut()
        .expect("object")
        .remove("cyberSessionBlockTtlSeconds");
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(body),
        ))
        .await
        .expect("response");
    let data = response_json(response).await["data"].clone();
    assert_eq!(data["cyberSessionBlockEnabled"], false);
    assert_eq!(data["cyberSessionBlockTtlSeconds"], 3600);

    let mut body = update_body();
    body.as_object_mut()
        .expect("object")
        .remove("cyberSessionBlockTtlSeconds");
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/update",
            Some(body),
        ))
        .await
        .expect("response");
    let data = response_json(response).await["data"].clone();
    assert_eq!(data["cyberSessionBlockEnabled"], true);
    assert_eq!(data["cyberSessionBlockTtlSeconds"], 3600);
}

#[tokio::test]
async fn client_downloads_should_return_validated_direct_links() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let response = app(fixture.state())
        .oneshot(request(
            Method::GET,
            "/api/admin/settings/client-downloads/codex-desktop/windows?refresh=true",
            None,
        ))
        .await
        .expect("client downloads response");

    assert_eq!(response.status(), StatusCode::OK);
    let data = response_json(response).await["data"].clone();
    assert_eq!(data["packages"][0]["architecture"], "x64");
    assert_eq!(data["packages"][0]["source"], "microsoft_store");
    assert_eq!(data["packages"][0]["version"], "26.825.6671.0");
    assert!(
        data["packages"][0]["downloadUrl"]
            .as_str()
            .is_some_and(|url| url.starts_with("https://dl.delivery.mp.microsoft.com/"))
    );
}

#[test]
fn settings_request_should_reject_invalid_model_mapping_name() {
    let mut body = update_body();
    body["modelMappings"] = json!({ "\0": "gpt-5.5" });
    let request: UpdateRuntimeSettingsRequest =
        serde_json::from_value(body).expect("decode settings");

    assert_eq!(request.validate().unwrap_err().field(), "modelMappings");
}

#[tokio::test]
async fn settings_should_require_admin_auth() {
    let fixture = AdminTestFixture::new().await;
    let response = app(fixture.state())
        .oneshot(
            Request::builder()
                .uri("/api/admin/settings")
                .header("x-request-id", "req_unauthorized")
                .body(Body::empty())
                .expect("unauthorized request"),
        )
        .await
        .expect("unauthorized response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_key_should_return_secret_only_on_regenerate() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/admin-api-key/regenerate",
            None,
        ))
        .await
        .expect("regenerate response");
    let data = response_json(response).await["data"].clone();

    assert!(
        data["key"]
            .as_str()
            .is_some_and(|key| key.starts_with("admin-") && key.len() == 70)
    );
}

#[tokio::test]
async fn admin_key_delete_should_use_fixed_post_path() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    fixture.settings.set_api_key("admin-valid-test-key");
    let response = app(fixture.state())
        .oneshot(request(
            Method::POST,
            "/api/admin/settings/admin-api-key/delete",
            None,
        ))
        .await
        .expect("delete response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn settings_should_accept_admin_api_key_header() {
    let fixture = AdminTestFixture::new().await;
    let key = format!("admin-{}", "a".repeat(64));
    fixture.auth.set_api_key(&key);
    let response = app(fixture.state())
        .oneshot(
            Request::builder()
                .uri("/api/admin/settings")
                .header("x-api-key", key)
                .header("x-request-id", "req_api_key")
                .body(Body::empty())
                .expect("api key request"),
        )
        .await
        .expect("api key response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_auth_should_accept_a_configured_request_id_header_name() {
    use axum::http::HeaderName;
    use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};

    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    // 部署把 api.request_id_header 改名后，注入的 header 不再叫 x-request-id；
    // 管理请求仍须拿到请求上下文，而不是退化为 500。
    let custom = HeaderName::from_static("x-trace-id");
    let app = app(fixture.state()).layer(SetRequestIdLayer::new(custom, MakeRequestUuid));
    let unlabelled = Request::builder()
        .method(Method::GET)
        .uri("/api/admin/settings")
        .header(header::COOKIE, "cpr_admin_session=valid-session")
        .body(Body::empty())
        .expect("build settings request");

    let response = app.oneshot(unlabelled).await.expect("settings response");

    assert_eq!(response.status(), StatusCode::OK);
}
