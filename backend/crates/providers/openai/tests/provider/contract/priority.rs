//! 所选账号快速模式进入真实 HTTP/WS 正文，以及同请求重试冻结的执行合同

use super::*;
use async_trait::async_trait;
use gateway_core::provider_ports::priority_policy::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct AccountPolicy {
    mode: Mutex<AccountPriorityMode>,
    calls: AtomicUsize,
}

#[async_trait]
impl PriorityPolicyStore for AccountPolicy {
    async fn global(&self) -> Result<GlobalPriorityPolicy, PriorityPolicyError> {
        Err(PriorityPolicyError::Unavailable)
    }
    async fn update_global(
        &self,
        _: u64,
        _: FastMode,
        _: bool,
        _: &str,
    ) -> Result<GlobalPriorityPolicy, PriorityPolicyError> {
        Err(PriorityPolicyError::Unavailable)
    }
    async fn account(&self, _: &str) -> Result<AccountPriorityPolicy, PriorityPolicyError> {
        Err(PriorityPolicyError::Unavailable)
    }
    async fn update_account(
        &self,
        _: &str,
        _: u64,
        _: AccountPriorityMode,
        _: Option<bool>,
        _: &str,
    ) -> Result<AccountPriorityPolicy, PriorityPolicyError> {
        Err(PriorityPolicyError::Unavailable)
    }
    async fn resolve(&self, id: &str, group: FastMode) -> Result<FastMode, PriorityPolicyError> {
        assert_eq!(id, "acct_provider_contract");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.mode.lock().unwrap().resolve(FastMode::Enabled, group))
    }
    async fn observe(
        &self,
        _: &str,
        _: PriorityQuotaObservation,
    ) -> Result<(), PriorityPolicyError> {
        Ok(())
    }
}

fn priority_provider(
    store: &Arc<MemoryAccountStore>,
    base_url: String,
    policy: &Arc<AccountPolicy>,
) -> Arc<CodexProvider> {
    provider_and_quota_with_runtime_ports(
        store,
        Arc::new(MemorySessionAffinity::default()),
        base_url,
        Arc::new(TestLeaseCoordinator::default()),
        0,
        Arc::new(MemoryCooldownPort::new()),
        (crate::support::runtime_policy(), Some(policy.clone())),
    )
    .0
}

fn request(use_websocket: bool, tier: Option<&str>) -> ProviderRequest {
    let mut body = Map::from_iter([
        ("model".into(), json!("gpt-5.4")),
        ("input".into(), json!("hello")),
    ]);
    if let Some(tier) = tier {
        body.insert("service_tier".into(), json!(tier));
    }
    let payload = ProtocolPayload::json_object("openai", body)
        .unwrap()
        .with_context(Map::from_iter([(
            "use_websocket".into(),
            json!(use_websocket),
        )]));
    planned_request_with_presentation(
        "openai",
        Operation::Generate(GenerateRequest::from_protocol_payload(payload)),
        "gpt-5.4",
        Some(
            gateway_core::routing::ModelPresentation::new(None, None).with_service_tiers(vec![
                gateway_core::routing::ModelServiceTier::new("priority", "fast", "Fast"),
            ]),
        ),
    )
}

fn completed() -> Value {
    json!({"type":"response.completed","response":{"id":"resp_priority","model":"gpt-5.4","status":"completed","output":[]}})
}

#[tokio::test]
async fn account_override_reaches_http_and_websocket_with_group_cap_and_explicit_flex() {
    for websocket in [false, true] {
        for (account, group, requested, expected) in [
            (
                AccountPriorityMode::Inherit,
                FastMode::Default,
                None,
                Some("priority"),
            ),
            (
                AccountPriorityMode::Disabled,
                FastMode::Default,
                Some("priority"),
                Some("default"),
            ),
            (
                AccountPriorityMode::Enabled,
                FastMode::Disabled,
                Some("priority"),
                Some("default"),
            ),
            (
                AccountPriorityMode::Enabled,
                FastMode::Default,
                Some("flex"),
                Some("flex"),
            ),
        ] {
            let store = Arc::new(MemoryAccountStore::default());
            create_account(&store, "acct_provider_contract").await;
            let policy = Arc::new(AccountPolicy {
                mode: Mutex::new(account),
                calls: AtomicUsize::new(0),
            });
            let (base_url, http_server, ws_server) = if websocket {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let task = tokio::spawn(async move {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = crate::transport::accept_codex_test_websocket(socket).await;
                    let sent = socket.next().await.unwrap().unwrap();
                    let body: Value = serde_json::from_str(sent.to_text().unwrap()).unwrap();
                    socket
                        .send(Message::Text(completed().to_string().into()))
                        .await
                        .unwrap();
                    body
                });
                (url, None, Some(task))
            } else {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/codex/responses"))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_raw(
                            format!("data: {}\n\n", completed()),
                            "text/event-stream",
                        ),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                (server.uri(), Some(server), None)
            };
            let mut stream = priority_provider(&store, base_url, &policy)
                .execute(
                    request(websocket, requested),
                    context_with_fast_policy("req_priority_wire", CancellationToken::new(), group),
                )
                .await
                .unwrap();
            while let Some(event) = stream.next().await {
                event.unwrap();
            }
            let body = if let Some(server) = http_server {
                let requests = server.received_requests().await.unwrap();
                captured_request_body(&requests[0])
            } else {
                ws_server.unwrap().await.unwrap()
            };
            assert_eq!(body.get("service_tier").and_then(Value::as_str), expected);
            assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn same_request_reuses_account_tier_after_auto_off_but_next_request_observes_disable() {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {}\n\n", completed()), "text/event-stream"),
        )
        .expect(3)
        .mount(&server)
        .await;
    let policy = Arc::new(AccountPolicy {
        mode: Mutex::new(AccountPriorityMode::Enabled),
        calls: AtomicUsize::new(0),
    });
    let provider = priority_provider(&store, server.uri(), &policy);
    let frozen = context_with_fast_policy(
        "req_before_auto_off",
        CancellationToken::new(),
        FastMode::Default,
    );
    for (index, context) in [
        frozen.clone(),
        frozen,
        context_with_fast_policy(
            "req_after_auto_off",
            CancellationToken::new(),
            FastMode::Default,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            *policy.mode.lock().unwrap() = AccountPriorityMode::Disabled;
        }
        let mut stream = provider
            .clone()
            .execute(request(false, None), context)
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    }
    let requests = server.received_requests().await.unwrap();
    let tiers: Vec<_> = requests
        .iter()
        .map(captured_request_body)
        .map(|body| body.get("service_tier").cloned())
        .collect();
    assert_eq!(
        tiers,
        vec![Some(json!("priority")), Some(json!("priority")), None]
    );
    assert_eq!(policy.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn api_key_account_keeps_native_group_tier_without_querying_oauth_priority_policy() {
    let store = Arc::new(MemoryAccountStore::default());
    let server = MockServer::start().await;
    store
        .seed_api_key(
            "acct_provider_contract",
            server.uri(),
            provider_openai::credential::ResponsesTransport::Http,
        )
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {}\n\n", completed()), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let policy = Arc::new(AccountPolicy {
        mode: Mutex::new(AccountPriorityMode::Disabled),
        calls: AtomicUsize::new(0),
    });
    let mut stream = priority_provider(&store, server.uri(), &policy)
        .execute(
            request(false, None),
            context_with_fast_policy(
                "req_api_key_priority",
                CancellationToken::new(),
                FastMode::Enabled,
            ),
        )
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        captured_request_body(&requests[0])["service_tier"],
        "priority"
    );
    assert_eq!(policy.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn harness_account_priority_policy_freezes_per_account_before_runtime_failure() {
    for kind in [
        provider_openai::credential::CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        provider_openai::credential::CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    ] {
        for (account_mode, group, effective) in [
            (
                AccountPriorityMode::Inherit,
                FastMode::Default,
                FastMode::Enabled,
            ),
            (
                AccountPriorityMode::Disabled,
                FastMode::Default,
                FastMode::Disabled,
            ),
            (
                AccountPriorityMode::Enabled,
                FastMode::Disabled,
                FastMode::Disabled,
            ),
        ] {
            let store = Arc::new(MemoryAccountStore::default());
            super::harness::seed_harness_with_models(
                &store,
                "acct_provider_contract",
                kind,
                "test-harness-bearer",
                vec!["gpt-5.4".to_owned()],
            )
            .await;
            let policy = Arc::new(AccountPolicy {
                mode: Mutex::new(account_mode),
                calls: AtomicUsize::new(0),
            });
            let context = context_with_fast_policy(
                "req_harness_priority_policy",
                CancellationToken::new(),
                group,
            );
            let error = priority_provider(&store, OFFICIAL_CODEX_BASE_URL.to_owned(), &policy)
                .execute(request(false, None), context.clone())
                .await
                .err()
                .expect("missing harness worker runtime");
            assert_eq!(
                error.kind(),
                ProviderErrorKind::ProviderInfrastructureUnavailable
            );
            assert_eq!(
                context.account_fast_mode("acct_provider_contract"),
                Some(effective)
            );
            assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn mixed_harness_catalog_uses_selected_pi_model_tier_not_shared_opencode_tier() {
    use gateway_core::account::{CredentialRevision, NewProviderAccount, ProviderAccount};
    use provider_openai::credential::{
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING, CODEX_AUTHENTICATION_KIND_PI_OPENAI,
        CodexCredentialCodec, CodexCredentialData, CodexOAuthCredentialData, ResponsesTransport,
    };

    const CHILD: &str = "CPR_TEST_PI_SOURCE_TIER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../");
        let bundle = tempfile::tempdir().unwrap();
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
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                std::thread::current().name().unwrap(),
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

    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer pi-test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"type":"response.created","response":{"id":"resp_pi_tier","status":"in_progress","output":[]}}),
                json!({"type":"response.completed","response":{"id":"resp_pi_tier","model":"gpt-5.6-terra","status":"completed","output":[],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}})),
            "text/event-stream",
        ))
        .expect(3)
        .mount(&origin)
        .await;
    let store = Arc::new(MemoryAccountStore::default());
    for (id, kind, token) in [
        (
            "acct_affinity_switch_a",
            CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
            "opencode-test-token",
        ),
        (
            "acct_provider_contract",
            CODEX_AUTHENTICATION_KIND_PI_OPENAI,
            "pi-test-token",
        ),
    ] {
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
        .unwrap();
        let account = ProviderAccount::new(
            ProviderAccountId::new(id).unwrap(),
            ProviderKind::new("openai").unwrap(),
            id.to_owned(),
            None,
            kind.to_owned(),
            CredentialRevision::new(1).unwrap(),
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
            .unwrap();
    }
    let policy = Arc::new(AccountPolicy {
        mode: Mutex::new(AccountPriorityMode::Enabled),
        calls: AtomicUsize::new(0),
    });
    let provider = Arc::new(
        Arc::try_unwrap(priority_provider(
            &store,
            OFFICIAL_CODEX_BASE_URL.to_owned(),
            &policy,
        ))
        .unwrap_or_else(|_| panic!("provider owner"))
        .with_harness_test_dial_loopback(*origin.address()),
    );
    for (index, (mode, explicit)) in [
        (FastMode::Enabled, None),
        (FastMode::Enabled, Some("priority")),
        (FastMode::Disabled, Some("priority")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut body = Map::from_iter([
            ("model".to_owned(), json!("gpt-5.6-terra")),
            ("input".to_owned(), json!("hello")),
            (
                "session_id".to_owned(),
                json!(format!("pi_source_tier_{index}")),
            ),
        ]);
        if let Some(tier) = explicit {
            body.insert("service_tier".to_owned(), json!(tier));
        }
        let request = planned_request_with_presentation(
            "openai",
            Operation::Generate(GenerateRequest::from_protocol_payload(
                ProtocolPayload::json_object("openai", body).unwrap(),
            )),
            "gpt-5.6-terra",
            Some(
                gateway_core::routing::ModelPresentation::new(None, None)
                    .with_service_tiers(vec![ModelServiceTier::new("priority", "fast", "Fast")]),
            ),
        );
        let context = AttemptContext::new(
            RequestAttemptContext::new(
                ModelRequestId::new(format!("req_pi_source_tier_{index}")).unwrap(),
                ClientApiKeyId::new("key_openai_contract").unwrap(),
            )
            .with_fast_mode(mode),
            NonZeroU32::MIN,
            SystemTime::now() + Duration::from_secs(30),
            account_policy(),
            AccountAttemptContext::new(
                BTreeSet::from([ProviderAccountId::new("acct_affinity_switch_a").unwrap()]),
                None,
                None,
            )
            .with_account_scope(contract_account_scope()),
            None,
            CancellationToken::new(),
        )
        .with_transport(AttemptTransport::Fallback);
        let mut stream = provider
            .clone()
            .execute(request, context)
            .await
            .expect("selected pi worker");
        while let Some(event) = stream.next().await {
            event.expect("pi response");
        }
    }
    let requests = origin.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let tiers = requests
        .iter()
        .map(captured_request_body)
        .map(|body| body.get("service_tier").cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        tiers,
        vec![None, Some(json!("priority")), Some(json!("default"))]
    );
}
