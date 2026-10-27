//! 新 OAuth 账号经过真实 Provider 选号后不可退回旧 Codex 出站路径

use super::*;
use futures::FutureExt as _;
use gateway_core::account::{CredentialRevision, NewProviderAccount, ProviderAccount};
use provider_openai::credential::{
    CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING, CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    CodexCredentialCodec, CodexCredentialData, CodexOAuthCredentialData, OpenAiSelectionDomain,
    ResponsesTransport, open_ai_client_session_key,
};

pub(super) async fn seed_harness(store: &MemoryAccountStore, kind: &str) {
    seed_harness_as(store, "acct_provider_contract", kind, "test-harness-bearer").await;
}

async fn seed_harness_as(store: &MemoryAccountStore, id: &str, kind: &str, token: &str) {
    let credential = CodexCredentialCodec::encode_complete(CodexCredentialData::OAuth(
        CodexOAuthCredentialData {
            transport: ResponsesTransport::Http,
            schema_version: 1,
            principal: None,
            installation_id: uuid::Uuid::new_v4().to_string(),
            access_token: token.to_owned(),
            refresh_token: Some("test-refresh".to_owned()),
            id_token: None,
            oauth_client_id: Some("test-client".to_owned()),
            oauth_scope: Some(
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
                    .to_owned(),
            ),
            authentication_kind: kind.to_owned(),
            cookies: Vec::new(),
        },
    ))
    .expect("credential");
    let account = ProviderAccount::new(
        ProviderAccountId::new(id).expect("id"),
        ProviderKind::new("openai").expect("provider"),
        "test harness".to_owned(),
        None,
        kind.to_owned(),
        CredentialRevision::new(1).expect("revision"),
        None,
    )
    .with_account_facts(
        true,
        CredentialState::Ready,
        QuotaState::unknown(),
        None,
        None,
    )
    .with_refresh_schedule(true, None);
    store
        .create_account(NewProviderAccount {
            account,
            credential,
            model_access: None,
        })
        .await
        .expect("account");
}

#[tokio::test]
async fn missing_harness_runtime_fails_closed_after_selecting_either_oauth_account() {
    for kind in [
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    ] {
        for websocket in [false, true] {
            let store = Arc::new(MemoryAccountStore::default());
            seed_harness(&store, kind).await;
            let payload = ProtocolPayload::json_object(
                "openai",
                Map::from_iter([
                    ("model".to_owned(), json!("gpt-5.4")),
                    ("input".to_owned(), json!("hello")),
                    ("session_id".to_owned(), json!("session_runtime_missing")),
                ]),
            )
            .expect("request")
            .with_context(Map::from_iter([(
                "use_websocket".to_owned(),
                json!(websocket),
            )]));
            let operation = Operation::Generate(GenerateRequest::from_protocol_payload(payload));
            let affinity = Arc::new(MemorySessionAffinity::default());
            let error = provider_with_affinity(&store, Arc::clone(&affinity))
                .execute(
                    planned_request("openai", operation),
                    fallback_transport_context(&format!("req_harness_{kind}_{websocket}")),
                )
                .await
                .err()
                .expect("missing runtime must not silently call legacy Codex");
            assert_eq!(
                error.kind(),
                ProviderErrorKind::ProviderInfrastructureUnavailable
            );
            assert_eq!(error.send_state(), UpstreamSendState::NotSent);
            let client = ClientApiKeyId::new("key_openai_contract").expect("client");
            let provider_kind = ProviderKind::new("openai").expect("provider");
            let legacy = open_ai_client_session_key(
                &client,
                "session_runtime_missing",
                OpenAiSelectionDomain::ProviderDefault,
            )
            .expect("legacy key");
            let domain = if kind == CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING {
                OpenAiSelectionDomain::OpenCode
            } else {
                OpenAiSelectionDomain::Pi
            };
            let harness = open_ai_client_session_key(&client, "session_runtime_missing", domain)
                .expect("harness key");
            assert!(
                affinity
                    .load(&provider_kind, &legacy)
                    .await
                    .expect("legacy load")
                    .is_none()
            );
            assert_eq!(
                affinity
                    .load(&provider_kind, &harness)
                    .await
                    .expect("harness load")
                    .expect("selected account bound")
                    .as_str(),
                "acct_provider_contract"
            );
        }
    }
}

#[tokio::test]
async fn codex_turn_state_and_websocket_lite_select_legacy_in_a_mixed_pool() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("authorization", "Bearer at-acct_affinity_switch_a"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(CAPTURE_COMPLETED_SSE),
        )
        .expect(4)
        .mount(&origin)
        .await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_affinity_switch_a").await;
    seed_harness_as(
        &store,
        "acct_affinity_switch_b",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "test-open-code",
    )
    .await;
    seed_harness_as(
        &store,
        "acct_scope_new",
        CODEX_AUTHENTICATION_KIND_PI_OPENAI,
        "test-pi",
    )
    .await;
    let provider = provider_with_affinity_and_base_url(
        &store,
        Arc::new(MemorySessionAffinity::default()),
        origin.uri(),
    );
    for (mode, metadata) in [
        (
            "http-turn-state",
            json!({"x-codex-turn-state": "state-from-codex"}),
        ),
        (
            "websocket-lite",
            json!({"ws_request_header_x_openai_internal_codex_responses_lite": "true"}),
        ),
    ] {
        for iteration in 0..2 {
            let payload = ProtocolPayload::json_object(
                "openai",
                Map::from_iter([
                    ("model".to_owned(), json!("gpt-5.4")),
                    ("input".to_owned(), json!("hello")),
                    (
                        "session_id".to_owned(),
                        json!(format!("session_{mode}_{iteration}")),
                    ),
                    ("client_metadata".to_owned(), metadata.clone()),
                ]),
            )
            .expect("legacy Codex request")
            .with_context(Map::from_iter([(
                "use_websocket".to_owned(),
                json!(mode == "websocket-lite"),
            )]));
            let mut stream = Arc::clone(&provider)
                .execute(
                    planned_request(
                        "openai",
                        Operation::Generate(GenerateRequest::from_protocol_payload(payload)),
                    ),
                    fallback_transport_context(&format!("req_{mode}_{iteration}")),
                )
                .await
                .expect("Codex-only state must never route to a harness account");
            assert_eq!(
                stream.metadata().provider_account_id().as_str(),
                "acct_affinity_switch_a"
            );
            while let Some(event) = stream.next().await {
                event.expect("legacy Codex upstream response");
            }
        }
    }
    let requests = origin
        .received_requests()
        .await
        .expect("Codex mock requests");
    assert_eq!(requests.len(), 4);
    for request in &requests[..2] {
        // 首轮尚未证明原 Codex 状态属于所选账号；沿用旧路径的账号边界清理
        assert!(request.headers.get("x-codex-turn-state").is_none());
    }
    for request in &requests[2..] {
        assert_eq!(
            request
                .headers
                .get("x-openai-internal-codex-responses-lite")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
    }
}

#[tokio::test]
async fn mixed_pool_rejects_credential_kind_switch_before_harness_send() {
    let store = Arc::new(MemoryAccountStore::default());
    seed_harness(&store, CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING).await;
    store.on_credential_load(Arc::new(|store, account, _| {
        if account.as_str() == "acct_provider_contract" {
            store.set_oauth_authentication_kind(
                account.as_str(),
                CODEX_AUTHENTICATION_KIND_PI_OPENAI,
            );
        }
        async { Ok(()) }.boxed()
    }));
    let error = provider(&store)
        .execute(
            planned_request("openai", generate_operation()),
            fallback_transport_context("req_harness_kind_switch"),
        )
        .await
        .err()
        .expect("credential cannot switch to the other harness after selection");
    assert_eq!(error.kind(), ProviderErrorKind::NoEligibleAccount);
    assert_eq!(error.send_state(), UpstreamSendState::NotSent);
}

#[cfg(debug_assertions)]
fn harness_attempt(request_id: &str, excluded: BTreeSet<ProviderAccountId>) -> AttemptContext {
    harness_attempt_mode(request_id, excluded, FastMode::Enabled)
}

#[cfg(debug_assertions)]
fn harness_attempt_mode(
    request_id: &str,
    excluded: BTreeSet<ProviderAccountId>,
    mode: FastMode,
) -> AttemptContext {
    AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new(request_id).expect("request id"),
            ClientApiKeyId::new("key_openai_contract").expect("key"),
        )
        .with_fast_mode(mode),
        NonZeroU32::MIN,
        SystemTime::now() + Duration::from_secs(30),
        account_policy()
            .with_openai_account_affinity(gateway_core::account::AccountAffinity::Preferred),
        AccountAttemptContext::new(excluded, None, None)
            .with_account_scope(contract_account_scope()),
        None,
        CancellationToken::new(),
    )
    .with_transport(AttemptTransport::Fallback)
}

#[cfg(debug_assertions)]
fn harness_request(thread: &str, history: bool) -> ProviderRequest {
    harness_request_with_model(thread, history, "gpt-5.6-luna", None)
}

#[cfg(debug_assertions)]
fn harness_request_with_model(
    thread: &str,
    history: bool,
    model: &str,
    tier: Option<&str>,
) -> ProviderRequest {
    harness_request_with_fields(thread, history, model, tier, &[])
}

#[cfg(debug_assertions)]
fn harness_request_with_fields(
    thread: &str,
    history: bool,
    model: &str,
    tier: Option<&str>,
    extra: &[(&str, Value)],
) -> ProviderRequest {
    let input = if history {
        json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Earlier user"}]},
            {"type":"reasoning","id":"rs_account_a","encrypted_content":"encrypted-account-a","summary":[]},
            {"type":"function_call","id":"fc_account_a","call_id":"call_account_a","name":"lookup","arguments":"{\"q\":\"x\"}"},
            {"type":"function_call_output","call_id":"call_account_a","output":"result"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Continue"}]}
        ])
    } else {
        json!("Start")
    };
    let mut fields = Map::from_iter([
        ("model".to_owned(), json!(model)),
        ("input".to_owned(), input),
        ("session_id".to_owned(), json!("root_provider_harness")),
        ("thread_id".to_owned(), json!(thread)),
        ("top_p".to_owned(), json!(0.7)),
        ("parallel_tool_calls".to_owned(), json!(false)),
        (
            "tools".to_owned(),
            json!([{"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}]),
        ),
    ]);
    if let Some(tier) = tier {
        fields.insert("service_tier".to_owned(), json!(tier));
    }
    for (key, value) in extra {
        fields.insert((*key).to_owned(), value.clone());
    }
    let payload = ProtocolPayload::json_object("openai", fields).expect("request");
    planned_request_with_presentation(
        "openai",
        Operation::Generate(GenerateRequest::from_protocol_payload(payload)),
        model,
        Some(
            gateway_core::routing::ModelPresentation::new(None, None)
                .with_service_tiers(vec![ModelServiceTier::new("priority", "fast", "Fast")]),
        ),
    )
}

#[cfg(debug_assertions)]
fn harness_sse() -> String {
    let item = json!({"id":"fc_response","type":"function_call","call_id":"call_response","name":"lookup","arguments":"{\"q\":\"x\"}"});
    [
        json!({"type":"response.created","response":{"id":"resp_harness","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":{"id":"resp_harness","status":"completed","output":[item],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}),
    ].into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn selected_harness_completes_fixed_source_mock_and_protects_account_history() {
    const CHILD: &str = "CPR_TEST_SELECTED_HARNESS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../");
        let bundle = tempfile::tempdir().expect("bundle temp");
        let output_dir = bundle.path().join("openai-harness");
        let assembled = std::process::Command::new("python")
            .arg(root.join("deploy/harness-sources/assemble.py"))
            .arg("assemble")
            .arg("--output")
            .arg(&output_dir)
            .output()
            .expect("assemble pinned sources");
        assert!(
            assembled.status.success(),
            "{}",
            String::from_utf8_lossy(&assembled.stderr)
        );
        let result = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                std::thread::current().name().expect("test name"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(
                "CPR_OPENAI_HARNESS_WORKER_DIR",
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("harness-worker"),
            )
            .env("CPR_OPENCODE_SOURCE", output_dir.join("sources/opencode"))
            .env("CPR_PI_SOURCE", output_dir.join("sources/pi"))
            .output()
            .expect("isolated test process");
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    for kind in [
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    ] {
        let origin = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", "Bearer token-account-a"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(harness_sse(), "text/event-stream"),
            )
            .expect(if kind == CODEX_AUTHENTICATION_KIND_PI_OPENAI {
                5
            } else {
                9
            })
            .mount(&origin)
            .await;
        let store = Arc::new(MemoryAccountStore::default());
        seed_harness_as(&store, "acct_affinity_switch_a", kind, "token-account-a").await;
        let affinity = Arc::new(MemorySessionAffinity::default());
        let provider = provider_with_affinity(&store, Arc::clone(&affinity));
        let provider = Arc::new(
            Arc::try_unwrap(provider)
                .unwrap_or_else(|_| panic!("provider owner"))
                .with_harness_test_dial_loopback(*origin.address()),
        );
        for (index, thread, history) in [
            (0, "root_provider_harness", false),
            (1, "child_one", true),
            (2, "child_two", true),
            (3, "child_one", true),
        ] {
            let mut stream = provider
                .clone()
                .execute(
                    harness_request(thread, history),
                    harness_attempt(
                        &format!("req_harness_vertical_{kind}_{index}"),
                        BTreeSet::new(),
                    ),
                )
                .await
                .expect("selected harness");
            let mut completed = false;
            let mut tool = false;
            let mut wire = Vec::new();
            timeout(Duration::from_secs(30), async {
                while let Some(item) = stream.next().await {
                    let event = item.expect("canonical stream");
                    completed |= event
                        .canonical_facts()
                        .iter()
                        .any(|fact| matches!(fact, GatewayEvent::Completed(_)));
                    tool |= event
                        .canonical_facts()
                        .iter()
                        .any(|fact| matches!(fact, GatewayEvent::ToolCallDelta(_)));
                    if let Some(bytes) = event.wire_event().and_then(|wire| wire.raw_sse_frame()) {
                        wire.extend_from_slice(bytes);
                    }
                }
            })
            .await
            .expect("harness response deadline");
            assert!(
                completed && tool,
                "{kind}: missing canonical tool or terminal fact"
            );
            assert!(
                String::from_utf8_lossy(&wire).contains("response.completed"),
                "{kind}: downstream wire"
            );
        }
        let cases: &[(&str, Option<&str>, FastMode, &str, &str)] =
            if kind == CODEX_AUTHENTICATION_KIND_PI_OPENAI {
                &[(
                    "gpt-5.6-luna",
                    Some("flex"),
                    FastMode::Disabled,
                    "gpt-5.6-luna",
                    "flex",
                )]
            } else {
                &[
                    (
                        "gpt-5.6-luna-fast",
                        Some("flex"),
                        FastMode::Disabled,
                        "gpt-5.6-luna",
                        "flex",
                    ),
                    (
                        "gpt-5.6-luna-fast",
                        Some("priority"),
                        FastMode::Disabled,
                        "gpt-5.6-luna",
                        "default",
                    ),
                    (
                        "gpt-5.6-luna-fast",
                        Some("default"),
                        FastMode::Default,
                        "gpt-5.6-luna",
                        "priority",
                    ),
                    (
                        "gpt-5.6-luna-fast",
                        None,
                        FastMode::Enabled,
                        "gpt-5.6-luna",
                        "priority",
                    ),
                    (
                        "gpt-5.6-luna",
                        Some("flex"),
                        FastMode::Enabled,
                        "gpt-5.6-luna",
                        "flex",
                    ),
                ]
            };
        for (index, (selected, tier, mode, _, _)) in cases.iter().enumerate() {
            let mut stream = provider
                .clone()
                .execute(
                    harness_request_with_model(
                        &format!("tier_probe_{index}"),
                        false,
                        selected,
                        *tier,
                    ),
                    harness_attempt_mode(
                        &format!("req_harness_tier_{kind}_{index}"),
                        BTreeSet::new(),
                        *mode,
                    ),
                )
                .await
                .expect("fast selection");
            timeout(Duration::from_secs(30), async {
                while let Some(item) = stream.next().await {
                    item.expect("completed fast response");
                }
            })
            .await
            .expect("fast response deadline");
        }
        let requests = origin.received_requests().await.expect("origin requests");
        assert_eq!(requests.len(), 4 + cases.len());
        for (index, (_, _, _, base, expected_tier)) in cases.iter().enumerate() {
            let sent: Value =
                serde_json::from_slice(&requests[index + 4].body).expect("fast request JSON");
            assert_eq!(
                sent["model"], *base,
                "{kind}: fast variant must use base slug"
            );
            assert_eq!(
                sent["service_tier"], *expected_tier,
                "{kind}: effective Fast policy"
            );
        }
        let headers = |index: usize, name: &str| {
            requests[index]
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let root_session = headers(
            0,
            if kind == CODEX_AUTHENTICATION_KIND_PI_OPENAI {
                "session_id"
            } else {
                "session-id"
            },
        );
        assert!(root_session.is_some());
        let threads = requests
            .iter()
            .map(|request| {
                request
                    .headers
                    .get(if kind == CODEX_AUTHENTICATION_KIND_PI_OPENAI {
                        "x-client-request-id"
                    } else {
                        "thread-id"
                    })
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_ne!(threads[1], threads[2]);
        assert_eq!(threads[1], threads[3]);
        for request in &requests[..4] {
            let sent: Value = serde_json::from_slice(&request.body).expect("upstream JSON");
            assert_eq!(sent["service_tier"], "priority");
            assert_eq!(sent["top_p"], 0.7);
            assert_eq!(sent["parallel_tool_calls"], false);
            assert!(
                !request
                    .body
                    .windows(b"call_account_a".len())
                    .any(|bytes| bytes == b"call_account_a")
            );
        }
        assert!(
            requests
                .iter()
                .skip(1)
                .any(|request| String::from_utf8_lossy(&request.body)
                    .contains("encrypted-account-a"))
        );
        seed_harness_as(&store, "acct_affinity_switch_b", kind, "token-account-b").await;
        let excluded = BTreeSet::from([ProviderAccountId::new("acct_affinity_switch_a").unwrap()]);
        let error = provider
            .clone()
            .execute(
                harness_request("child_two", true),
                harness_attempt(&format!("req_harness_switch_{kind}"), excluded),
            )
            .await
            .err()
            .expect("changed account must reject protected history");
        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert_eq!(error.send_state(), UpstreamSendState::NotSent);
        assert_eq!(
            origin.received_requests().await.unwrap().len(),
            4 + cases.len()
        );
        let domain = if kind == CODEX_AUTHENTICATION_KIND_PI_OPENAI {
            OpenAiSelectionDomain::Pi
        } else {
            OpenAiSelectionDomain::OpenCode
        };
        let key = open_ai_client_session_key(
            &ClientApiKeyId::new("key_openai_contract").unwrap(),
            "root_provider_harness",
            domain,
        )
        .unwrap();
        assert_eq!(
            affinity
                .load(&ProviderKind::new("openai").unwrap(), &key)
                .await
                .unwrap()
                .expect("root bound after refused replay")
                .as_str(),
            "acct_affinity_switch_a"
        );

        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", "Bearer token-account-b"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "17")
                    .set_body_json(
                        json!({"error":{"message":"rate limited","type":"rate_limit_error"}}),
                    ),
            )
            .expect(1)
            .mount(&origin)
            .await;
        let exclude_a = BTreeSet::from([ProviderAccountId::new("acct_affinity_switch_a").unwrap()]);
        let mut stream = provider
            .clone()
            .execute(
                harness_request("rate_probe", false),
                harness_attempt(&format!("req_harness_rate_{kind}"), exclude_a.clone()),
            )
            .await
            .expect("selected second account");
        let rate_error = timeout(Duration::from_secs(30), async {
            while let Some(result) = stream.next().await {
                if let Err(error) = result {
                    return error;
                }
            }
            panic!("429 must end as a stream error")
        })
        .await
        .expect("rate response deadline");
        assert_eq!(rate_error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(rate_error.send_state(), UpstreamSendState::Sent);
        assert_eq!(rate_error.retry_after(), Some(Duration::from_secs(17)));
        let next = provider
            .clone()
            .execute(
                harness_request("rate_probe", false),
                harness_attempt(&format!("req_harness_cooldown_{kind}"), exclude_a.clone()),
            )
            .await;
        assert!(next.is_err(), "cooled B must not be selected again");
        assert_eq!(
            origin.received_requests().await.unwrap().len(),
            5 + cases.len()
        );

        seed_harness_as(&store, "acct_scope_new", kind, "token-account-c").await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", "Bearer token-account-c"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(
                    json!({"error":{"message":"unauthorized","type":"invalid_token"}}),
                ),
            )
            .expect(1)
            .mount(&origin)
            .await;
        let excluded = BTreeSet::from([
            ProviderAccountId::new("acct_affinity_switch_a").unwrap(),
            ProviderAccountId::new("acct_affinity_switch_b").unwrap(),
        ]);
        let mut stream = provider
            .clone()
            .execute(
                harness_request("unauthorized_probe", false),
                harness_attempt(
                    &format!("req_harness_unauthorized_{kind}"),
                    excluded.clone(),
                ),
            )
            .await
            .expect("selected third account");
        let unauthorized = timeout(Duration::from_secs(30), async {
            while let Some(result) = stream.next().await {
                if let Err(error) = result {
                    return error;
                }
            }
            panic!("401 must end as a stream error")
        })
        .await
        .expect("unauthorized deadline");
        assert_eq!(unauthorized.kind(), ProviderErrorKind::Unauthorized);
        assert_eq!(unauthorized.send_state(), UpstreamSendState::Sent);
        let next = provider
            .clone()
            .execute(
                harness_request("unauthorized_probe", false),
                harness_attempt(&format!("req_harness_expired_{kind}"), excluded),
            )
            .await;
        assert!(next.is_err(), "expired C must not be selected again");
        assert_eq!(
            origin.received_requests().await.unwrap().len(),
            6 + cases.len()
        );
    }
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn stored_opencode_response_reuses_raw_handle_only_with_its_account_and_revision() {
    const CHILD: &str = "CPR_TEST_STORED_OPENCODE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../");
        let bundle = tempfile::tempdir().expect("pinned source bundle");
        let assembled = std::process::Command::new("python")
            .arg(root.join("deploy/harness-sources/assemble.py"))
            .arg("assemble")
            .arg("--output")
            .arg(bundle.path().join("openai-harness"))
            .output()
            .expect("assemble pinned sources");
        assert!(
            assembled.status.success(),
            "{}",
            String::from_utf8_lossy(&assembled.stderr)
        );
        let result = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                std::thread::current().name().expect("test name"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(
                "CPR_OPENAI_HARNESS_WORKER_DIR",
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("harness-worker"),
            )
            .env(
                "CPR_OPENCODE_SOURCE",
                bundle.path().join("openai-harness/sources/opencode"),
            )
            .env(
                "CPR_PI_SOURCE",
                bundle.path().join("openai-harness/sources/pi"),
            )
            .output()
            .expect("isolated test process");
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer token-account-a"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(harness_sse(), "text/event-stream"))
        .expect(5)
        .mount(&origin)
        .await;
    let store = Arc::new(MemoryAccountStore::default());
    seed_harness_as(
        &store,
        "acct_affinity_switch_a",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "token-account-a",
    )
    .await;
    seed_harness_as(
        &store,
        "acct_scope_new",
        CODEX_AUTHENTICATION_KIND_PI_OPENAI,
        "token-pi",
    )
    .await;
    let provider = provider_with_affinity(&store, Arc::new(MemorySessionAffinity::default()));
    let provider = Arc::new(
        Arc::try_unwrap(provider)
            .unwrap_or_else(|_| panic!("provider owner"))
            .with_harness_test_dial_loopback(*origin.address()),
    );
    let first = harness_request_with_fields(
        "root_provider_harness",
        false,
        "gpt-5.6-luna",
        None,
        &[
            ("store", json!(true)),
            (
                "client_metadata",
                json!({"x-openai-subagent":"worker","thread_id":"root_provider_harness"}),
            ),
            ("prompt_cache_key", json!("shared-cache")),
        ],
    );
    let mut stream = provider
        .clone()
        .execute(first, harness_attempt("req_store_first", BTreeSet::new()))
        .await
        .expect("first stored call");
    let mut public = None;
    let mut state = None;
    let mut raw = None;
    let mut downstream = Vec::new();
    while let Some(item) = stream.next().await {
        let event = item.expect("first canonical event");
        for fact in event.canonical_facts() {
            if let GatewayEvent::Completed(meta) = fact {
                public = Some(meta.response_id().to_owned());
            }
        }
        state = event.session_update().cloned().or(state);
        raw = event
            .native_upstream_response_id()
            .map(str::to_owned)
            .or(raw);
        if let Some(frame) = event.wire_event().and_then(|wire| wire.raw_sse_frame()) {
            downstream.extend_from_slice(frame);
        }
    }
    let public = public.expect("public completed ID");
    let raw = raw.expect("raw handle");
    let state = state.expect("provider session update");
    assert!(public.starts_with("cpr_oc_v1_"));
    assert_ne!(public, raw);
    assert_eq!(raw, "resp_harness");
    assert_eq!(
        state.payload().get("harness_kind"),
        Some(&json!("opencode"))
    );
    assert_eq!(state.payload().get("credential_revision"), Some(&json!(1)));
    let wire = String::from_utf8(downstream).expect("SSE wire");
    assert!(wire.contains(&public));
    assert!(!wire.contains("resp_harness"));
    let pin = NativeContinuationPin::new(
        PreviousResponseId::new(public.clone()),
        PreviousResponseId::new(raw.clone()),
        ClientApiKeyId::new("key_openai_contract").unwrap(),
        ProviderKind::new("openai").unwrap(),
        ProviderAccountId::new("acct_affinity_switch_a").unwrap(),
    )
    .with_scope(gateway_core::engine::continuation::NativeContinuationScope::Persisted)
    .with_session_state(state.clone());
    let attempt = |request_id: &str, pin: NativeContinuationPin| {
        let owner = ProviderAccountStateOwner::new(
            ProviderKind::new("openai").unwrap(),
            pin.account().clone(),
        );
        AttemptContext::new(
            RequestAttemptContext::new(
                ModelRequestId::new(request_id).unwrap(),
                ClientApiKeyId::new("key_openai_contract").unwrap(),
            ),
            NonZeroU32::MIN,
            SystemTime::now() + Duration::from_secs(30),
            account_policy(),
            AccountAttemptContext::new(BTreeSet::new(), None, Some(owner))
                .with_account_scope(contract_account_scope()),
            Some(ContinuationBinding::Pinned(pin)),
            CancellationToken::new(),
        )
        .with_continuation_attempt(ContinuationAttempt::Native)
        .with_transport(AttemptTransport::Fallback)
    };
    let followup = || {
        harness_request_with_fields(
            "root_provider_harness",
            false,
            "gpt-5.6-luna",
            None,
            &[
                ("store", json!(true)),
                ("previous_response_id", json!(public)),
            ],
        )
    };
    let mut stream = provider
        .clone()
        .execute(followup(), attempt("req_store_second", pin.clone()))
        .await
        .expect("pinned same-account continuation");
    while let Some(item) = stream.next().await {
        item.expect("second canonical event");
    }
    let requests = origin.received_requests().await.expect("mock requests");
    assert_eq!(requests.len(), 2);
    let second: Value = serde_json::from_slice(&requests[1].body).expect("second upstream body");
    assert_eq!(second["previous_response_id"], raw);
    assert_eq!(second["store"], true);
    assert_eq!(second["prompt_cache_key"], Value::Null);
    let first_body: Value = serde_json::from_slice(&requests[0].body).expect("first upstream body");
    assert!(
        first_body["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| key != "shared-cache")
    );

    seed_harness_as(
        &store,
        "acct_affinity_switch_b",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "token-account-b",
    )
    .await;
    let foreign_pin = NativeContinuationPin::new(
        PreviousResponseId::new(public.clone()),
        PreviousResponseId::new(raw.clone()),
        ClientApiKeyId::new("key_openai_contract").unwrap(),
        ProviderKind::new("openai").unwrap(),
        ProviderAccountId::new("acct_affinity_switch_b").unwrap(),
    )
    .with_scope(gateway_core::engine::continuation::NativeContinuationScope::Persisted)
    .with_session_state(state.clone());
    let foreign = provider
        .clone()
        .execute(followup(), attempt("req_foreign_account", foreign_pin))
        .await;
    assert!(
        foreign.is_err(),
        "the original account's state cannot authorize account B"
    );

    let mismatched = |key: &str, value: Value| {
        let mut payload = state.payload().clone();
        payload.insert(key.to_owned(), value);
        NativeContinuationPin::new(
            PreviousResponseId::new(public.clone()),
            PreviousResponseId::new(raw.clone()),
            ClientApiKeyId::new("key_openai_contract").unwrap(),
            ProviderKind::new("openai").unwrap(),
            ProviderAccountId::new("acct_affinity_switch_a").unwrap(),
        )
        .with_scope(gateway_core::engine::continuation::NativeContinuationScope::Persisted)
        .with_session_state(
            gateway_core::operation::ProviderSessionState::new("openai", payload).unwrap(),
        )
    };
    for (index, altered) in [
        mismatched("credential_revision", json!(2)),
        mismatched("account_id", json!("acct_wrong")),
        mismatched("harness_kind", json!("pi")),
    ]
    .into_iter()
    .enumerate()
    {
        let result = provider
            .clone()
            .execute(
                followup(),
                attempt(&format!("req_bad_owner_{index}"), altered),
            )
            .await;
        assert!(
            result.is_err(),
            "mismatched continuation owner must not send"
        );
    }
    let external = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_pin_missing").unwrap(),
            ClientApiKeyId::new("key_openai_contract").unwrap(),
        ),
        NonZeroU32::MIN,
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None)
            .with_account_scope(contract_account_scope()),
        Some(ContinuationBinding::External(PreviousResponseId::new(
            public.clone(),
        ))),
        CancellationToken::new(),
    )
    .with_continuation_attempt(ContinuationAttempt::Native);
    let error = provider
        .clone()
        .execute(followup(), external)
        .await
        .err()
        .expect("missing pin");
    assert_eq!(
        error.kind(),
        ProviderErrorKind::ContinuationRecoveryRequired
    );
    assert_eq!(origin.received_requests().await.unwrap().len(), 2);

    for (index, metadata) in [
        json!({"x-codex-turn-state":"legacy-turn"}),
        json!({"ws_request_header_x_openai_internal_codex_responses_lite":"true"}),
    ]
    .into_iter()
    .enumerate()
    {
        let conflicted = harness_request_with_fields(
            "root_provider_harness",
            false,
            "gpt-5.6-luna",
            None,
            &[
                ("store", json!(true)),
                ("previous_response_id", json!(public)),
                ("client_metadata", metadata),
            ],
        );
        let error = provider
            .clone()
            .execute(
                conflicted,
                attempt(&format!("req_pinned_legacy_state_{index}"), pin.clone()),
            )
            .await
            .err()
            .expect("Codex-only state cannot be resumed on the known OpenCode owner");
        assert_eq!(
            error.kind(),
            ProviderErrorKind::ContinuationRecoveryRequired
        );
        assert_eq!(error.send_state(), UpstreamSendState::NotSent);
    }

    let diagnostic = provider
        .clone()
        .execute(
            harness_request_with_fields(
                "pi_store_not_supported",
                false,
                "gpt-5.6-luna",
                None,
                &[("store", json!(true))],
            ),
            diagnostic_context("req_pi_store_unsupported", "acct_scope_new")
                .with_transport(AttemptTransport::Fallback),
        )
        .await
        .err()
        .expect("explicit pi account cannot persist responses");
    assert_eq!(diagnostic.kind(), ProviderErrorKind::InvalidRequest);
    assert_eq!(diagnostic.send_state(), UpstreamSendState::NotSent);

    for index in 0..3 {
        let mut stream = provider
            .clone()
            .execute(
                harness_request_with_fields(
                    &format!("fresh_stored_thread_{index}"),
                    false,
                    "gpt-5.6-luna",
                    None,
                    &[
                        ("session_id", json!(format!("fresh_stored_session_{index}"))),
                        ("store", json!(true)),
                    ],
                ),
                harness_attempt(
                    &format!("req_mixed_store_{index}"),
                    BTreeSet::from([ProviderAccountId::new("acct_affinity_switch_b").unwrap()]),
                ),
            )
            .await
            .expect("mixed OpenCode/pi pool must select the store-capable account");
        assert_eq!(
            stream.metadata().provider_account_id().as_str(),
            "acct_affinity_switch_a"
        );
        timeout(Duration::from_secs(30), async {
            while let Some(event) = stream.next().await {
                event.expect("store-capable account response");
            }
        })
        .await
        .expect("stored response deadline");
    }
    assert_eq!(origin.received_requests().await.unwrap().len(), 5);
}
