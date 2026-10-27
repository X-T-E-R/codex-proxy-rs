//! 管理端连接测试经真实 Core probe 到 HTTP/WS 出站的身份生命周期回归

use super::*;
use async_trait::async_trait;
use futures::SinkExt as _;
use gateway_core::engine::admission::{
    ClientAdmissionDecision, ClientAdmissionError, ClientAdmissionPort, ClientAdmissionRecovery,
    ClientAdmissionRequest, ClientAdmissionRestoreResult,
};
use gateway_core::engine::continuation::{
    NativeContinuationPin, NativeContinuationPort, NativeContinuationStoreError, PreviousResponseId,
};
use gateway_core::engine::execution::{ClientApiKeyUsageSink, DefaultExecutionService};
use gateway_core::engine::probe::{AccountProbe as _, AccountProbeRequest};
use gateway_core::engine::provider::ProviderRegistry;
use gateway_core::engine::{
    AttemptRecord, ExecutionStore, IntermediateFailure, ModelRequestFinalization, NewModelRequest,
    RecoveryReport,
};
use gateway_core::error::StoreError;
use gateway_core::runtime::RuntimeSnapshotHandle;
use gateway_core::upstream::UpstreamSendState;
use reqwest::header::HeaderMap;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type CapturedRequest = (HeaderMap, Value);

fn probe_service(bundle: &provider_openai::ProviderBundle) -> DefaultExecutionService {
    let provider = ProviderKind::new("openai").unwrap();
    let snapshot = RuntimeSnapshot::new(
        ConfigRevision::new(1).unwrap(),
        gateway_core::settings::SettingsValues::new(2, "smart", Default::default(), None, None),
        vec![provider.clone()],
        ["gpt-5.4", "gpt-5.5"]
            .into_iter()
            .map(|model| {
                ProviderModel::new(
                    provider.clone(),
                    UpstreamModelId::new(model).unwrap(),
                    ModelCapabilities::new(
                        BTreeSet::from([gateway_core::operation::OperationKind::Generate]),
                        Some(32_000),
                    )
                    .with_upstream_feature_validation(),
                )
            })
            .collect(),
        Vec::new(),
    )
    .unwrap();
    let unused = Arc::new(UnusedProbePorts);
    DefaultExecutionService::new(
        RuntimeSnapshotHandle::new(snapshot),
        unused.clone(),
        ProviderRegistry::new([bundle.core_provider()]).unwrap(),
        unused.clone(),
        unused.clone(),
        unused,
        Arc::new(RecordingDiagnostics::default()),
    )
}

async fn test_operation(
    bundle: &provider_openai::ProviderBundle,
    model: &str,
    prompt: &str,
    websocket: bool,
) -> Operation {
    let Operation::Generate(generate) = bundle
        .admin_provider()
        .connection_test_operation(&UpstreamModelId::new(model).unwrap(), prompt)
        .await
        .unwrap()
    else {
        panic!("generate operation")
    };
    let payload = generate.protocol_payload();
    let mut context = payload.context().clone();
    context.insert("use_websocket".to_owned(), json!(websocket));
    Operation::Generate(GenerateRequest::from_protocol_payload(
        ProtocolPayload::json_object("openai", payload.body().clone())
            .unwrap()
            .with_context(context),
    ))
}

async fn probe(
    bundle: &provider_openai::ProviderBundle,
    service: &DefaultExecutionService,
    account: &str,
    model: &str,
    prompt: &str,
    websocket: bool,
) {
    service
        .probe(
            AccountProbeRequest {
                account_id: ProviderAccountId::new(account).unwrap(),
                provider_kind: ProviderKind::new("openai").unwrap(),
                upstream_model: UpstreamModelId::new(model).unwrap(),
                operation: test_operation(bundle, model, prompt, websocket).await,
            },
            None,
        )
        .await
        .expect("connection test");
}

async fn seed_account(store: &MemoryAccountStore, id: &str) {
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: id.to_owned(),
            name: id.to_owned(),
            secret: secret(&format!("at-{id}")),
            verified_account: profile(&format!("chatgpt-{id}")),
            next_refresh_at: Some(Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
}

fn http_capture(request: &wiremock::Request) -> CapturedRequest {
    let bytes = if request
        .headers
        .get("content-encoding")
        .is_some_and(|value| value == "zstd")
    {
        zstd::stream::decode_all(request.body.as_slice()).unwrap()
    } else {
        request.body.clone()
    };
    (
        request.headers.clone(),
        serde_json::from_slice(&bytes).unwrap(),
    )
}

fn identity((headers, body): &CapturedRequest) -> (String, String) {
    let metadata = &body["client_metadata"];
    let header: Value =
        serde_json::from_str(headers["x-codex-turn-metadata"].to_str().unwrap()).unwrap();
    let full: Value =
        serde_json::from_str(metadata["x-codex-turn-metadata"].as_str().unwrap()).unwrap();
    for field in ["session_id", "thread_id", "turn_id"] {
        let value = metadata[field].as_str().unwrap();
        Uuid::parse_str(value).expect("account-scoped UUID");
        assert_eq!(header[field], value);
        assert_eq!(full[field], value);
    }
    assert_eq!(
        headers["session-id"].to_str().unwrap(),
        metadata["session_id"]
    );
    assert_eq!(
        headers["thread-id"].to_str().unwrap(),
        metadata["thread_id"]
    );
    assert_eq!(
        headers["x-client-request-id"].to_str().unwrap(),
        metadata["turn_id"]
    );
    assert_eq!(metadata["x-client-request-id"], metadata["turn_id"]);
    assert_eq!(body["prompt_cache_key"], metadata["session_id"]);
    assert_eq!(metadata["session_id"], metadata["thread_id"]);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    assert!(!headers.contains_key("x-codex-turn-state"));
    for container in [body, metadata, &header, &full] {
        for field in [
            "turnState",
            "turn_state",
            "x-codex-turn-state",
            "previous_response_id",
        ] {
            assert!(
                container.get(field).is_none(),
                "stale state {field}: {container}"
            );
        }
    }
    (
        metadata["session_id"].as_str().unwrap().to_owned(),
        metadata["turn_id"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn connection_test_oauth_identity_survives_model_prompt_and_service_changes_over_http_and_ws()
{
    let store = Arc::new(MemoryAccountStore::default());
    for account in ["acct_probe_a", "acct_probe_b"] {
        seed_account(&store, account).await;
    }
    let mut config = valid_config();
    let mut account_sessions = None;
    for websocket in [false, true] {
        let http = MockServer::start().await;
        let (base_url, ws_captures) = if websocket {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let mut captures = Vec::new();
                let mut sockets = Vec::new();
                // 保留旧连接，确保新测试主动独立建连，而不是由 mock 关闭后被动重建
                for _ in 0..6 {
                    let (socket, _) =
                        tokio::time::timeout(Duration::from_secs(10), listener.accept())
                            .await
                            .unwrap()
                            .unwrap();
                    let mut headers = None;
                    let mut socket =
                        crate::transport::accept_codex_test_websocket_with(socket, |request, _| {
                            headers = Some(request.headers().clone());
                        })
                        .await;
                    let message = socket.next().await.unwrap().unwrap();
                    let body = serde_json::from_str(message.to_text().unwrap()).unwrap();
                    captures.push((headers.unwrap(), body));
                    socket.send(Message::Text(json!({"type":"response.metadata", "x-codex-turn-state":"previous-test-state"}).to_string().into())).await.unwrap();
                    socket.send(Message::Text(json!({"type":"response.completed", "response":{
                        "id":"resp_probe", "model":"gpt-5.4", "status":"completed", "output":[]
                    }}).to_string().into())).await.unwrap();
                    sockets.push(socket);
                }
                captures
            });
            (base_url, Some(task))
        } else {
            Mock::given(method("POST"))
                .and(path("/codex/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .insert_header("x-codex-turn-state", "previous-test-state")
                        .set_body_string(COMPLETED_SESSION_SSE),
                )
                .expect(6)
                .mount(&http)
                .await;
            (http.uri(), None)
        };
        config.config.api.base_url = base_url;
        let mut bundle = provider_openai::initialize(
            config.config.clone(),
            provider_ports_with(store.clone(), Arc::new(TestOAuthPending::default())),
        )
        .await
        .unwrap();
        let mut service = probe_service(&bundle);
        let cases = [
            ("acct_probe_a", "gpt-5.4", "first"),
            ("acct_probe_a", "gpt-5.4", "first"),
            ("acct_probe_a", "gpt-5.4", "different text"),
            ("acct_probe_a", "gpt-5.5", "different text"),
            ("acct_probe_b", "gpt-5.4", "first"),
            ("acct_probe_a", "gpt-5.4", "after restart"),
        ];
        for (index, (account, model, prompt)) in cases.iter().enumerate() {
            if index == 5 {
                drop(service);
                drop(bundle);
                bundle = provider_openai::initialize(
                    config.config.clone(),
                    provider_ports_with(store.clone(), Arc::new(TestOAuthPending::default())),
                )
                .await
                .unwrap();
                service = probe_service(&bundle);
            }
            probe(&bundle, &service, account, model, prompt, websocket).await;
        }
        let captures = if let Some(task) = ws_captures {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
        } else {
            http.received_requests()
                .await
                .unwrap()
                .iter()
                .map(http_capture)
                .collect()
        };
        let identities = captures.iter().map(identity).collect::<Vec<_>>();
        for index in [1, 2, 3, 5] {
            assert_eq!(identities[0].0, identities[index].0);
        }
        assert_ne!(identities[0].0, identities[4].0);
        let sessions = (identities[0].0.clone(), identities[4].0.clone());
        if let Some(expected) = &account_sessions {
            assert_eq!(
                &sessions, expected,
                "HTTP/WS share account session identity"
            );
        } else {
            account_sessions = Some(sessions);
        }
        assert_eq!(
            identities
                .iter()
                .map(|id| &id.1)
                .collect::<BTreeSet<_>>()
                .len(),
            cases.len()
        );
        for ((headers, body), (account, model, prompt)) in captures.iter().zip(cases) {
            assert_eq!(headers["chatgpt-account-id"], format!("chatgpt-{account}"));
            assert_eq!(body["model"], model);
            assert_eq!(body["input"][0]["content"][0]["text"], prompt);
            let metadata = body["client_metadata"].to_string();
            assert!(!metadata.contains(account));
            assert!(!metadata.contains("admin_connection_test"));
        }

        // 普通客户端即使把可见诊断 ID 当作 session，也不会进入诊断的源身份域
        let normal = Operation::Generate(GenerateRequest::from_protocol_payload(
            ProtocolPayload::json_object(
                "openai",
                json!({
                    "model":"gpt-5.4", "input":"normal", "session_id":identities[0].0,
                })
                .as_object()
                .unwrap()
                .clone(),
            )
            .unwrap()
            .with_context(Map::from_iter([("use_websocket".to_owned(), json!(false))])),
        ));
        Mock::given(method("POST"))
            .and(path("/codex/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(COMPLETED_SESSION_SSE),
            )
            .with_priority(1)
            .mount(&http)
            .await;
        let normal_provider = initialized_test_provider(store.clone(), http.uri()).await;
        let mut stream = normal_provider
            .execute(
                initialized_provider_request(normal, "acct_probe_a"),
                initialized_attempt_context("req_normal", "acct_probe_a"),
            )
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        let normal_requests = http.received_requests().await.unwrap();
        let (headers, body) = http_capture(normal_requests.last().unwrap());
        assert_ne!(headers["session-id"].to_str().unwrap(), identities[0].0);
        assert!(!headers.contains_key("x-codex-turn-metadata"));
        assert!(body.get("prompt_cache_key").is_none());
    }
}

#[tokio::test]
async fn connection_test_transport_retry_keeps_turn_request_and_required_account() {
    let store = Arc::new(MemoryAccountStore::default());
    for account in ["acct_probe_a", "acct_probe_b"] {
        seed_account(&store, account).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = valid_config();
    config.config.api.base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut captures = Vec::new();
        for index in 0..2 {
            let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut headers = None;
            let mut socket =
                crate::transport::accept_codex_test_websocket_with(socket, |request, _| {
                    headers = Some(request.headers().clone());
                })
                .await;
            let message = socket.next().await.unwrap().unwrap();
            let body = serde_json::from_str(message.to_text().unwrap()).unwrap();
            captures.push((headers.unwrap(), body));
            if index == 0 {
                socket.send(Message::Text(json!({"type":"response.metadata", "x-codex-turn-state":"failed-attempt-state"}).to_string().into())).await.unwrap();
                socket.close(None).await.unwrap();
            } else {
                socket
                    .send(Message::Text(
                        json!({"type":"response.completed", "response":{
                            "id":"resp_retry", "model":"gpt-5.4", "status":"completed", "output":[]
                        }})
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
        captures
    });
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .unwrap();
    probe(
        &bundle,
        &probe_service(&bundle),
        "acct_probe_a",
        "gpt-5.4",
        "retry",
        true,
    )
    .await;
    let captures = server.await.unwrap();
    assert_eq!(identity(&captures[0]), identity(&captures[1]));
    assert_eq!(captures[0].1["input"], captures[1].1["input"]);
    for (headers, _) in captures {
        assert_eq!(headers["chatgpt-account-id"], "chatgpt-acct_probe_a");
    }
}

#[tokio::test]
async fn connection_test_http_failure_returns_the_required_account_error_without_rotation() {
    let store = Arc::new(MemoryAccountStore::default());
    for account in ["acct_probe_a", "acct_probe_b"] {
        seed_account(&store, account).await;
    }
    let http = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": {"message":"temporary unavailable", "type":"server_error"}
        })))
        .expect(1)
        .mount(&http)
        .await;
    let mut config = valid_config();
    config.config.api.base_url = http.uri();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .unwrap();
    let error = probe_service(&bundle)
        .probe(
            AccountProbeRequest {
                account_id: ProviderAccountId::new("acct_probe_a").unwrap(),
                provider_kind: ProviderKind::new("openai").unwrap(),
                upstream_model: UpstreamModelId::new("gpt-5.4").unwrap(),
                operation: test_operation(&bundle, "gpt-5.4", "failure", false).await,
            },
            None,
        )
        .await
        .expect_err("fixed-account refusal must surface");
    assert_eq!(error.upstream_response().unwrap().status(), 503);
    let requests = http.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let capture = http_capture(&requests[0]);
    identity(&capture);
    assert_eq!(capture.0["chatgpt-account-id"], "chatgpt-acct_probe_a");
}

#[tokio::test]
async fn connection_test_api_key_uses_standard_responses_without_codex_identity() {
    let store = Arc::new(MemoryAccountStore::default());
    let http = MockServer::start().await;
    store
        .seed_api_key(
            "acct_api_probe",
            http.uri(),
            provider_openai::credential::ResponsesTransport::Http,
        )
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(COMPLETED_SESSION_SSE),
        )
        .expect(1)
        .mount(&http)
        .await;
    let config = valid_config();
    let bundle = provider_openai::initialize(
        config.config.clone(),
        provider_ports_with(store, Arc::new(TestOAuthPending::default())),
    )
    .await
    .unwrap();
    probe(
        &bundle,
        &probe_service(&bundle),
        "acct_api_probe",
        "gpt-5.4",
        "API key",
        false,
    )
    .await;
    let requests = http.received_requests().await.unwrap();
    let (headers, body) = http_capture(&requests[0]);
    for name in [
        "session-id",
        "thread-id",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "chatgpt-account-id",
        "cookie",
    ] {
        assert!(
            !headers.contains_key(name),
            "unexpected Codex header {name}"
        );
    }
    assert_eq!(
        body,
        json!({"model":"gpt-5.4", "input":[{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"API key"}]}], "stream":true, "store":false})
    );
}

// Probe 不应访问普通请求的持久化、准入、续接或 Key 使用端口
struct UnusedProbePorts;

#[async_trait]
impl ExecutionStore for UnusedProbePorts {
    async fn create_model_request(&self, _: NewModelRequest) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn record_attempt(&self, _: AttemptRecord) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn mark_send_state(
        &self,
        _: &ModelRequestId,
        _: UpstreamSendState,
    ) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn mark_downstream_committed(
        &self,
        _: &ModelRequestId,
        _: SystemTime,
        _: Option<u16>,
    ) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn record_client_status(&self, _: &ModelRequestId, _: u16) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn record_intermediate_failure(&self, _: IntermediateFailure) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn finalize_model_request(&self, _: ModelRequestFinalization) -> Result<(), StoreError> {
        panic!("probe persistence")
    }
    async fn recover_expired(&self, _: SystemTime) -> Result<RecoveryReport, StoreError> {
        panic!("probe persistence")
    }
}

impl ClientAdmissionPort for UnusedProbePorts {
    fn abandon(&self, _: &ClientApiKeyId, _: &ModelRequestId) {
        panic!("probe admission")
    }
    fn admit(
        &self,
        _: ClientAdmissionRequest,
    ) -> BoxFuture<'_, Result<ClientAdmissionDecision, ClientAdmissionError>> {
        panic!("probe admission")
    }
    fn release<'a>(
        &'a self,
        _: &'a ClientApiKeyId,
        _: &'a ModelRequestId,
    ) -> BoxFuture<'a, Result<bool, ClientAdmissionError>> {
        panic!("probe admission")
    }
    fn restore(
        &self,
        _: ClientAdmissionRecovery,
    ) -> BoxFuture<'_, Result<ClientAdmissionRestoreResult, ClientAdmissionError>> {
        panic!("probe admission")
    }
}

impl NativeContinuationPort for UnusedProbePorts {
    fn resolve<'a>(
        &'a self,
        _: &'a ClientApiKeyId,
        _: &'a PreviousResponseId,
    ) -> BoxFuture<'a, Result<Option<NativeContinuationPin>, NativeContinuationStoreError>> {
        panic!("probe continuation")
    }
    fn record(
        &self,
        _: NativeContinuationPin,
    ) -> BoxFuture<'_, Result<(), NativeContinuationStoreError>> {
        panic!("probe continuation")
    }
}

impl ClientApiKeyUsageSink for UnusedProbePorts {
    fn record_used(&self, _: &ClientApiKeyId) {
        panic!("probe Key usage")
    }
}
