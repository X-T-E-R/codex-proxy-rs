use bytes::Bytes;
use chrono::{TimeZone, Utc};
use gateway_core::provider_ports::OpenAiRequestBodyOverride;
use gateway_core::routing::ModelRequestPolicy;
use provider_openai::transport::request_override::{
    apply_model_request_policy, apply_responses_request_override, apply_standalone_search_override,
};

use super::*;

fn enabled_policy(timezone: &str, country: &str) -> OpenAiRequestBodyOverride {
    OpenAiRequestBodyOverride::try_new(true, timezone, country).expect("valid override")
}

fn environment_item(current_date: &str, timezone: &str, extra: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!(
                "<environment_context>\n  <cwd>C:/work&amp;space&lt;x&gt;</cwd>\n  <current_date>{current_date}</current_date>\n  <timezone>{timezone}</timezone>\n  <future>{extra}</future>\n</environment_context>"
            ),
        }],
        "future_item_field": {"preserved": true},
    })
}

fn environment_text(body: &Map<String, Value>, index: usize) -> &str {
    body["input"][index]["content"][0]["text"]
        .as_str()
        .expect("environment text")
}

#[test]
fn responses_override_rewrites_only_latest_dedicated_environment_and_search_tools() {
    let historical = environment_item("2026-01-01", "Asia/Shanghai", "old");
    let encrypted = json!({
        "type": "reasoning",
        "encrypted_content": "ciphertext",
        "future": {"opaque": [1, 2, 3]},
    });
    let latest = environment_item("2026-01-02", "Europe/Berlin", "latest");
    let quoted = json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "quoted: <environment_context><current_date>never</current_date><timezone>never</timezone></environment_context>"
        }]
    });
    let mut request = CodexResponsesRequest::from_body(Map::from_iter([
        ("model".to_owned(), json!("gpt-test")),
        (
            "input".to_owned(),
            Value::Array(vec![
                historical.clone(),
                encrypted.clone(),
                latest,
                quoted.clone(),
            ]),
        ),
        (
            "tools".to_owned(),
            json!([
                {"type": "function", "name": "web_search", "future": 1},
                {
                    "type": "web_search",
                    "user_location": {
                        "type": "approximate",
                        "unknown_before": 1,
                        "country": "DE",
                        "region": "BE",
                        "unknown_middle": 2,
                        "city": "Berlin",
                        "unknown_after": 3
                    },
                    "search_context_size": "high",
                    "future_tool": {"kept": true}
                },
                {"type": "web_search_preview_2025_03_11"}
            ]),
        ),
        ("future_top_level".to_owned(), json!({"kept": true})),
    ]));

    let changed = apply_responses_request_override(
        &mut request,
        &enabled_policy("America/Los_Angeles", "US"),
        Utc.with_ymd_and_hms(2026, 7, 1, 6, 30, 0)
            .single()
            .expect("timestamp"),
    );

    assert!(changed);
    assert_eq!(request.body()["input"][0], historical);
    assert_eq!(request.body()["input"][1], encrypted);
    assert_eq!(request.body()["input"][3], quoted);
    let latest = environment_text(request.body(), 2);
    assert!(latest.contains("<current_date>2026-06-30</current_date>"));
    assert!(latest.contains("<timezone>America/Los_Angeles</timezone>"));
    assert!(latest.contains("<cwd>C:/work&amp;space&lt;x&gt;</cwd>"));
    assert!(latest.contains("<future>latest</future>"));
    assert_eq!(request.body()["tools"][0]["future"], 1);
    assert!(request.body()["tools"][0].get("user_location").is_none());
    assert_eq!(
        request.body()["tools"][1]["user_location"],
        json!({
            "type": "approximate",
            "unknown_before": 1,
            "country": "US",
            "unknown_middle": 2,
            "unknown_after": 3,
            "timezone": "America/Los_Angeles",
        })
    );
    assert_eq!(
        request.body()["tools"][1]["user_location"]
            .as_object()
            .expect("Responses user_location")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "type",
            "unknown_before",
            "country",
            "unknown_middle",
            "unknown_after",
            "timezone"
        ]
    );
    assert_eq!(
        request.body()["tools"][1]["future_tool"],
        json!({"kept": true})
    );
    assert_eq!(
        request.body()["tools"][2]["user_location"],
        json!({
            "type": "approximate",
            "country": "US",
            "timezone": "America/Los_Angeles"
        })
    );
    assert_eq!(request.body()["future_top_level"], json!({"kept": true}));
}

#[test]
fn responses_override_uses_pacific_dst_date_boundaries() {
    for (captured_at, expected_date) in [
        ((2026, 1, 1, 7, 30), "2025-12-31"),
        ((2026, 7, 1, 6, 30), "2026-06-30"),
    ] {
        let mut request = CodexResponsesRequest::from_body(Map::from_iter([(
            "input".to_owned(),
            Value::Array(vec![environment_item("2000-01-01", "Etc/UTC", "kept")]),
        )]));
        let (year, month, day, hour, minute) = captured_at;
        apply_responses_request_override(
            &mut request,
            &enabled_policy("America/Los_Angeles", "US"),
            Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                .single()
                .expect("timestamp"),
        );
        assert!(
            environment_text(request.body(), 0)
                .contains(&format!("<current_date>{expected_date}</current_date>"))
        );
    }
}

#[test]
fn responses_override_does_not_fabricate_environment_for_native_continuation() {
    let mut request = CodexResponsesRequest::from_body(Map::from_iter([
        ("previous_response_id".to_owned(), json!("resp_previous")),
        ("input".to_owned(), json!([])),
        ("future".to_owned(), json!({"opaque": true})),
    ]));
    let before = serde_json::to_vec(&request).expect("serialize before");

    let changed = apply_responses_request_override(
        &mut request,
        &enabled_policy("America/Los_Angeles", "US"),
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp"),
    );

    assert!(!changed);
    assert_eq!(
        serde_json::to_vec(&request).expect("serialize after"),
        before
    );
    assert_eq!(request.previous_response_id(), Some("resp_previous"));
}

#[test]
fn incomplete_newest_environment_does_not_fall_back_to_historical_block() {
    let historical = environment_item("2000-01-01", "Etc/UTC", "historical");
    let incomplete = json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "<environment_context>\n  <current_date>2026-01-01</current_date>\n</environment_context>"
        }]
    });
    let mut request = CodexResponsesRequest::from_body(Map::from_iter([(
        "input".to_owned(),
        Value::Array(vec![historical.clone(), incomplete.clone()]),
    )]));

    assert!(!apply_responses_request_override(
        &mut request,
        &enabled_policy("America/Los_Angeles", "US"),
        Utc.with_ymd_and_hms(2026, 1, 1, 7, 30, 0)
            .single()
            .expect("timestamp")
    ));
    assert_eq!(request.body()["input"][0], historical);
    assert_eq!(request.body()["input"][1], incomplete);
}

#[test]
fn disabled_responses_and_search_overrides_are_byte_identical() {
    let policy = OpenAiRequestBodyOverride::disabled();
    let mut request = CodexResponsesRequest::from_body(Map::from_iter([(
        "input".to_owned(),
        Value::Array(vec![environment_item("2000-01-01", "Etc/UTC", "kept")]),
    )]));
    let before = serde_json::to_vec(&request).expect("serialize before");
    assert!(!apply_responses_request_override(
        &mut request,
        &policy,
        Utc::now()
    ));
    assert_eq!(
        serde_json::to_vec(&request).expect("serialize after"),
        before
    );

    let search =
        Bytes::from_static(br#"{ "id": "search", "settings": null, "future": {"kept": true} }"#);
    assert_eq!(apply_standalone_search_override(&search, &policy), search);
}

#[test]
fn already_matching_targets_keep_original_serialization_and_search_bytes() {
    let policy = enabled_policy("America/Los_Angeles", "US");
    let mut request = CodexResponsesRequest::from_body(Map::from_iter([
        (
            "input".to_owned(),
            Value::Array(vec![environment_item(
                "2025-12-31",
                "America/Los_Angeles",
                "kept",
            )]),
        ),
        (
            "tools".to_owned(),
            json!([{
                "type": "web_search",
                "user_location": {
                    "type": "approximate",
                    "country": "US",
                    "timezone": "America/Los_Angeles",
                    "future": 1e3
                }
            }]),
        ),
    ]));
    let before = serde_json::to_vec(&request).expect("serialize before");
    assert!(!apply_responses_request_override(
        &mut request,
        &policy,
        Utc.with_ymd_and_hms(2026, 1, 1, 7, 30, 0)
            .single()
            .expect("timestamp")
    ));
    assert_eq!(
        serde_json::to_vec(&request).expect("serialize after"),
        before
    );

    let search = Bytes::from_static(
        br#"{
  "settings": {
    "user_location": {"type":"approximate","country":"US","timezone":"America/Los_Angeles","future":1e3},
    "future": true
  }
}"#,
    );
    assert_eq!(
        apply_standalone_search_override(&search, &policy),
        search,
        "already matching search must retain whitespace and numeric spelling"
    );
}

#[test]
fn standalone_search_override_creates_location_and_preserves_unknown_fields() {
    let body = Bytes::from_static(
        br#"{"id":"search","settings":{"search_context_size":"high","future":{"kept":true}},"future_top":7}"#,
    );
    let rewritten =
        apply_standalone_search_override(&body, &enabled_policy("America/Los_Angeles", "US"));
    let value: Value = serde_json::from_slice(&rewritten).expect("rewritten search JSON");

    assert_eq!(
        value["settings"]["user_location"],
        json!({
            "type": "approximate",
            "country": "US",
            "timezone": "America/Los_Angeles"
        })
    );
    assert_eq!(value["settings"]["search_context_size"], "high");
    assert_eq!(value["settings"]["future"], json!({"kept": true}));
    assert_eq!(value["future_top"], 7);

    let body = Bytes::from_static(
        br#"{"settings":{"user_location":{"type":"approximate","unknown_before":1,"country":"DE","region":"BE","unknown_middle":2,"city":"Berlin","unknown_after":3},"future":true}}"#,
    );
    let rewritten =
        apply_standalone_search_override(&body, &enabled_policy("America/Los_Angeles", "US"));
    let value: Value = serde_json::from_slice(&rewritten).expect("rewritten Search location");
    assert_eq!(
        value["settings"]["user_location"]
            .as_object()
            .expect("Standalone user_location")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "type",
            "unknown_before",
            "country",
            "unknown_middle",
            "unknown_after",
            "timezone"
        ]
    );
    assert_eq!(
        value["settings"]["user_location"],
        json!({
            "type": "approximate",
            "unknown_before": 1,
            "country": "US",
            "unknown_middle": 2,
            "unknown_after": 3,
            "timezone": "America/Los_Angeles"
        })
    );

    let malformed = Bytes::from_static(br#"{"settings":"opaque","future":true}"#);
    assert_eq!(
        apply_standalone_search_override(&malformed, &enabled_policy("America/Los_Angeles", "US")),
        malformed
    );
}

fn wire_request() -> CodexResponsesRequest {
    CodexResponsesRequest::from_body(Map::from_iter([
        ("model".to_owned(), json!("gpt-test")),
        ("instructions".to_owned(), json!("be brief")),
        (
            "input".to_owned(),
            Value::Array(vec![environment_item("2000-01-01", "Etc/UTC", "wire")]),
        ),
        ("tools".to_owned(), json!([{"type": "web_search"}])),
        ("future_top_level".to_owned(), json!({"wire": true})),
    ]))
}

fn assert_wire_override(value: &Value) {
    let text = value["input"][0]["content"][0]["text"]
        .as_str()
        .expect("wire environment text");
    assert!(text.contains("<current_date>2025-12-31</current_date>"));
    assert!(text.contains("<timezone>America/Los_Angeles</timezone>"));
    assert_eq!(value["tools"][0]["user_location"]["country"], "US");
    assert_eq!(
        value["tools"][0]["user_location"]["timezone"],
        "America/Los_Angeles"
    );
    assert_eq!(value["future_top_level"], json!({"wire": true}));
}

#[tokio::test]
async fn http_transport_sends_overridden_body_on_the_actual_wire() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP server");
    let address = listener.local_addr().expect("HTTP server address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept HTTP client");
        let request = read_http_request_with_body(&mut stream).await;
        write_completed_sse_response(&mut stream).await;
        request
    });
    let mut request = wire_request();
    request.force_http_sse = true;
    apply_responses_request_override(
        &mut request,
        &enabled_policy("America/Los_Angeles", "US"),
        Utc.with_ymd_and_hms(2026, 1, 1, 7, 30, 0)
            .single()
            .expect("timestamp"),
    );
    let client = CodexBackendClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client"),
        format!("http://{address}"),
        test_wire_profile(),
    );

    client
        .create_response(
            &request,
            request_context("req_locale_http", Some("acct-locale")),
        )
        .await
        .expect("HTTP response");
    let raw = server.await.expect("HTTP server task");
    let separator = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP separator");
    let decompressed = zstd::stream::decode_all(std::io::Cursor::new(&raw[separator + 4..]))
        .expect("decode zstd body");
    let value: Value = serde_json::from_slice(&decompressed).expect("HTTP wire JSON");
    assert_wire_override(&value);
}

#[tokio::test]
async fn websocket_transport_sends_overridden_body_on_the_actual_wire() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind WebSocket server");
    let address = listener.local_addr().expect("WebSocket server address");
    let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept WebSocket client");
        let mut websocket = accept_codex_test_websocket(stream).await;
        let Some(Ok(Message::Text(payload))) = websocket.next().await else {
            panic!("client should send response.create");
        };
        payload_tx
            .send(payload.to_string())
            .expect("capture response.create");
        websocket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_locale_ws",
                        "status": "completed",
                        "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send completed response");
    });
    let mut request = wire_request();
    request.use_websocket = true;
    apply_responses_request_override(
        &mut request,
        &enabled_policy("America/Los_Angeles", "US"),
        Utc.with_ymd_and_hms(2026, 1, 1, 7, 30, 0)
            .single()
            .expect("timestamp"),
    );
    let client = CodexBackendClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("WebSocket client"),
        format!("http://{address}"),
        test_wire_profile(),
    )
    .with_websocket_pool(Arc::new(CodexWebSocketPool::new(Duration::from_mins(1))));

    client
        .create_response(
            &request,
            request_context("req_locale_ws", Some("acct-locale")),
        )
        .await
        .expect("WebSocket response");
    let payload = payload_rx.await.expect("captured response.create");
    server.await.expect("WebSocket server task");
    let value: Value = serde_json::from_str(&payload).expect("WebSocket wire JSON");
    assert_eq!(value["type"], "response.create");
    assert_wire_override(&value);
}

fn policy_facts(mode: Option<&str>, value: Option<&str>, tier: Option<&str>) -> ModelRequestPolicy {
    ModelRequestPolicy::from_facts(mode, value, tier).expect("valid policy")
}

fn request_with_entries(entries: Vec<(&str, Value)>) -> CodexResponsesRequest {
    CodexResponsesRequest::from_body(Map::from_iter(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    ))
}

#[test]
fn model_policy_lock_fast_writes_service_tier() {
    let policy = policy_facts(None, None, Some("lock_fast"));
    let mut request = request_with_entries(vec![("model", json!("gpt-test"))]);
    assert!(apply_model_request_policy(&mut request, &policy));
    assert_eq!(request.body()["service_tier"], json!("fast"));
    // 已是 fast 时不重复改写。
    assert!(!apply_model_request_policy(&mut request, &policy));
}

#[test]
fn model_policy_lock_never_fast_strips_fast_and_priority_only() {
    let policy = policy_facts(None, None, Some("lock_never_fast"));
    for fast in ["fast", " priority ", "FAST"] {
        let mut request = request_with_entries(vec![("service_tier", json!(fast))]);
        assert!(apply_model_request_policy(&mut request, &policy));
        assert!(request.body().get("service_tier").is_none());
    }
    let mut request = request_with_entries(vec![("service_tier", json!("flex"))]);
    assert!(!apply_model_request_policy(&mut request, &policy));
    assert_eq!(request.body()["service_tier"], json!("flex"));
    let mut request = request_with_entries(vec![("model", json!("gpt-test"))]);
    assert!(!apply_model_request_policy(&mut request, &policy));
}

#[test]
fn model_policy_locked_effort_overwrites_and_creates_reasoning() {
    let policy = policy_facts(Some("locked"), Some("xhigh"), None);
    let mut request = request_with_entries(vec![("model", json!("gpt-test"))]);
    assert!(apply_model_request_policy(&mut request, &policy));
    assert_eq!(request.body()["reasoning"]["effort"], json!("xhigh"));

    let mut request = request_with_entries(vec![(
        "reasoning",
        json!({"effort": "low", "summary": "auto"}),
    )]);
    assert!(apply_model_request_policy(&mut request, &policy));
    assert_eq!(request.body()["reasoning"]["effort"], json!("xhigh"));
    // 其他 reasoning 字段保持不变。
    assert_eq!(request.body()["reasoning"]["summary"], json!("auto"));

    // reasoning 为 null 时按缺失对象处理并写入。
    let mut request = request_with_entries(vec![("reasoning", Value::Null)]);
    assert!(apply_model_request_policy(&mut request, &policy));
    assert_eq!(request.body()["reasoning"]["effort"], json!("xhigh"));
}

#[test]
fn model_policy_min_max_clamp_respects_known_effort_and_skips_unknown() {
    let min = policy_facts(Some("min"), Some("high"), None);
    let mut absent = request_with_entries(vec![("model", json!("gpt-test"))]);
    assert!(apply_model_request_policy(&mut absent, &min));
    assert_eq!(absent.body()["reasoning"]["effort"], json!("high"));
    let mut lower = request_with_entries(vec![("reasoning", json!({"effort": "minimal"}))]);
    assert!(apply_model_request_policy(&mut lower, &min));
    assert_eq!(lower.body()["reasoning"]["effort"], json!("high"));
    let mut higher = request_with_entries(vec![("reasoning", json!({"effort": "max"}))]);
    assert!(!apply_model_request_policy(&mut higher, &min));
    assert_eq!(higher.body()["reasoning"]["effort"], json!("max"));
    let mut unknown = request_with_entries(vec![("reasoning", json!({"effort": "ultra"}))]);
    assert!(!apply_model_request_policy(&mut unknown, &min));

    let max = policy_facts(Some("max"), Some("medium"), None);
    let mut above = request_with_entries(vec![("reasoning", json!({"effort": "xhigh"}))]);
    assert!(apply_model_request_policy(&mut above, &max));
    assert_eq!(above.body()["reasoning"]["effort"], json!("medium"));
    let mut below = request_with_entries(vec![("reasoning", json!({"effort": "none"}))]);
    assert!(!apply_model_request_policy(&mut below, &max));
}
