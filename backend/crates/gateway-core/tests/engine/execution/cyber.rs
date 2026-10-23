//! Cyber 入口拒绝、跨 Provider 覆盖、热开关和未知送达的执行回归

use super::*;
use gateway_core::provider_ports::{
    CyberSessionKey, CyberSessionPort, CyberSessionRequest, CyberSessionStoreError,
};
use gateway_core::settings::RuntimeProtectionSettings;
use std::sync::atomic::AtomicU64;

struct PinnedContinuation(&'static str);

impl NativeContinuationPort for PinnedContinuation {
    fn resolve<'a>(
        &'a self,
        client: &'a ClientApiKeyId,
        previous: &'a PreviousResponseId,
    ) -> BoxFuture<'a, Result<Option<NativeContinuationPin>, NativeContinuationStoreError>> {
        Box::pin(async move {
            Ok(Some(NativeContinuationPin::new(
                previous.clone(),
                previous.clone(),
                client.clone(),
                ProviderKind::new(self.0).unwrap(),
                ProviderAccountId::new("acct_other").unwrap(),
            )))
        })
    }
    fn record<'a>(
        &'a self,
        _: NativeContinuationPin,
    ) -> BoxFuture<'a, Result<(), NativeContinuationStoreError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct Markers {
    now: AtomicU64,
    entries: Mutex<BTreeMap<String, u64>>,
    records: AtomicUsize,
}

impl CyberSessionPort for Markers {
    fn contains_any<'a>(
        &'a self,
        keys: &'a [CyberSessionKey],
    ) -> BoxFuture<'a, Result<bool, CyberSessionStoreError>> {
        Box::pin(async move {
            let entries = self.entries.lock().unwrap();
            Ok(keys.iter().any(|key| {
                entries
                    .get(&key.expose_to_store())
                    .is_some_and(|expires| *expires > self.now.load(Ordering::SeqCst))
            }))
        })
    }
    fn record<'a>(
        &'a self,
        key: &'a CyberSessionKey,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), CyberSessionStoreError>> {
        Box::pin(async move {
            self.records.fetch_add(1, Ordering::SeqCst);
            self.entries
                .lock()
                .unwrap()
                .entry(key.expose_to_store())
                .or_insert(self.now.load(Ordering::SeqCst) + ttl.as_secs());
            Ok(())
        })
    }
}

struct PolicyProvider {
    kind: &'static str,
    mode: AtomicUsize,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for PolicyProvider {
    fn name(&self) -> &'static str {
        self.kind
    }
    fn catalog_generation(&self) -> ProviderCatalogGeneration {
        ProviderCatalogGeneration::default()
    }
    async fn query_model_capabilities(
        &self,
    ) -> Result<Vec<ProviderModelCapabilities>, ProviderError> {
        Ok(Vec::new())
    }
    async fn execute(
        self: Arc<Self>,
        request: ProviderRequest,
        _: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        let count = self.calls.fetch_add(1, Ordering::SeqCst);
        let metadata = ProviderCallMetadata::new(
            request.candidate().provider().clone(),
            request.candidate().upstream_model().unwrap().clone(),
            ProviderAccountId::new(if count.is_multiple_of(2) {
                "acct_start"
            } else {
                "acct_other"
            })
            .unwrap(),
            UpstreamTransport::new("websocket").unwrap(),
        );
        let events = match self.mode.load(Ordering::SeqCst) {
            1 => vec![
                Ok(ProviderEvent::canonical(GatewayEvent::Started(
                    ResponseMeta::new("response-ok", "gpt-start"),
                ))),
                Ok(ProviderEvent::canonical(GatewayEvent::Completed(
                    ResponseMeta::new("response-ok", "gpt-start"),
                ))),
            ],
            2 => vec![Err(ProviderError::new(
                ProviderErrorKind::Transport,
                UpstreamSendState::Ambiguous,
            ))],
            _ => vec![Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::Ambiguous,
            )
            .with_status(400)
            .with_client_visible_upstream_error(
                gateway_core::error::ClientVisibleUpstreamError::new(
                    "synthetic policy refusal",
                    Some("cyber_policy".to_owned()),
                    Some("invalid_request_error".to_owned()),
                ),
            ))],
        };
        Ok(ProviderStream::new(
            metadata,
            futures::stream::iter(events),
            (),
        ))
    }
}

fn snapshot(kind: &'static str, enabled: bool) -> RuntimeSnapshot {
    let provider = ProviderKind::new(kind).unwrap();
    let directory = Arc::new(RuntimeAccountDirectory::new(
        ["acct_start", "acct_other"]
            .into_iter()
            .map(|id| {
                (
                    ProviderAccountId::new(id).unwrap(),
                    RuntimeAccount::new(provider.clone(), BTreeSet::new()),
                )
            })
            .collect(),
    ));
    let scope = Arc::new(FrozenAccountScope::new(
        Arc::clone(&directory),
        ClientRoutingScope::all_accounts(),
    ));
    RuntimeSnapshot::new(
        ConfigRevision::new(if enabled { 2 } else { 3 }).unwrap(),
        gateway_core::settings::SettingsValues::new(2, "smart", Default::default(), None, None)
            .with_protection(RuntimeProtectionSettings {
                cyber_session_block_enabled: enabled,
                cyber_session_block_ttl_seconds: 10,
                ..Default::default()
            }),
        vec![provider.clone()],
        vec![ProviderModel::new(
            provider,
            UpstreamModelId::new("gpt-start").unwrap(),
            ModelCapabilities::new(BTreeSet::from([OperationKind::Generate]), Some(16_000)),
        )],
        [
            ("key_start_test", "sk_start_test"),
            ("key_other", "sk_other_test"),
        ]
        .into_iter()
        .map(|(id, plaintext)| {
            ClientPolicy::new(
                ClientApiKeyId::new(id).unwrap(),
                PlaintextClientApiKey::new(plaintext).unwrap(),
                Arc::clone(&scope),
                true,
                RateLimits::unlimited(),
            )
        })
        .collect(),
    )
    .unwrap()
    .with_account_directory(directory)
}

fn protected_request(
    service: &DefaultExecutionService,
    transport: ClientTransport,
    thread: &str,
    key: &str,
) -> StartExecution {
    let mut request = super::request(service, transport);
    request.client = service.authenticate(key).unwrap();
    let digest = match thread {
        "child-a" => [1; 32],
        "child-b" => [2; 32],
        "parent" => [3; 32],
        _ => panic!("unknown synthetic thread"),
    };
    request.metadata.cyber_session = CyberSessionRequest::from_digests(Some(digest), Vec::new());
    request
}

#[test]
fn cyber_refusal_blocks_new_attempts_across_transports_and_provider_routes() {
    block_on(async {
        for kind in ["openai", "xai"] {
            for transport in [
                ClientTransport::HttpJson,
                ClientTransport::HttpSse,
                ClientTransport::WebSocket,
            ] {
                let handle = RuntimeSnapshotHandle::new(snapshot(kind, true));
                let store = Arc::new(TrackingExecutionStore::default());
                let markers = Arc::new(Markers::default());
                let provider = Arc::new(PolicyProvider {
                    kind,
                    mode: AtomicUsize::new(0),
                    calls: AtomicUsize::new(0),
                });
                let service = DefaultExecutionService::new(
                    handle,
                    store.clone(),
                    ProviderRegistry::new([provider.clone() as Arc<dyn Provider>]).unwrap(),
                    Arc::new(Admissions::default()),
                    Arc::new(PinnedContinuation(kind)),
                    Arc::new(RecordingClientApiKeyUsage::default()),
                    Arc::new(crate::RecordingDiagnostics::default()),
                )
                .with_cyber_sessions(markers.clone());
                let mut first = service
                    .start(protected_request(
                        &service,
                        transport,
                        "child-a",
                        "sk_start_test",
                    ))
                    .await
                    .unwrap();
                assert!(first.session.collect_uncommitted().await.is_err());
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    1,
                    "unknown delivery must not replay"
                );
                assert_eq!(markers.records.load(Ordering::SeqCst), 1);
                let error = service
                    .start(protected_request(
                        &service,
                        transport,
                        "child-a",
                        "sk_start_test",
                    ))
                    .await
                    .err()
                    .unwrap();
                assert_eq!(error.kind(), GatewayErrorKind::PolicyDenied);
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    1,
                    "blocked before selecting another account"
                );
                {
                    let finalizations = store.finalizations.lock().unwrap();
                    let blocked = finalizations.last().unwrap();
                    assert_eq!(blocked.attempt_count, 0);
                    assert_eq!(blocked.send_state, UpstreamSendState::NotSent);
                    assert_eq!(blocked.client_status_code, Some(403));
                }
                let mut pinned = protected_request(&service, transport, "child-a", "sk_start_test");
                pinned.metadata.previous_response_id =
                    Some(PreviousResponseId::new("resp_previous"));
                let pinned_error = service.start(pinned).await.err().unwrap();
                assert_eq!(pinned_error.kind(), GatewayErrorKind::PolicyDenied);
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    1,
                    "a different fixed continuation account cannot bypass the marker"
                );
                provider.mode.store(1, Ordering::SeqCst);
                for (thread, key) in [
                    ("parent", "sk_start_test"),
                    ("child-b", "sk_start_test"),
                    ("child-a", "sk_other_test"),
                ] {
                    let mut allowed = service
                        .start(protected_request(&service, transport, thread, key))
                        .await
                        .unwrap();
                    allowed.session.collect_uncommitted().await.unwrap();
                    allowed.session.commit_downstream(Some(200)).await.unwrap();
                }
                assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
            }
        }
    });
}

#[test]
fn cyber_disabled_success_does_not_clear_marker_and_expiry_reopens_the_session() {
    block_on(async {
        let handle = RuntimeSnapshotHandle::new(snapshot("openai", true));
        let markers = Arc::new(Markers::default());
        let provider = Arc::new(PolicyProvider {
            kind: "openai",
            mode: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let service = DefaultExecutionService::new(
            handle.clone(),
            Arc::new(TrackingExecutionStore::default()),
            ProviderRegistry::new([provider.clone() as Arc<dyn Provider>]).unwrap(),
            Arc::new(Admissions::default()),
            Arc::new(UnusedContinuation),
            Arc::new(RecordingClientApiKeyUsage::default()),
            Arc::new(crate::RecordingDiagnostics::default()),
        )
        .with_cyber_sessions(markers.clone());
        let mut first = service
            .start(protected_request(
                &service,
                ClientTransport::HttpSse,
                "child-a",
                "sk_start_test",
            ))
            .await
            .unwrap();
        assert!(first.session.collect_uncommitted().await.is_err());
        handle.publish(snapshot("openai", false));
        let mut disabled_refusal = service
            .start(protected_request(
                &service,
                ClientTransport::HttpSse,
                "child-b",
                "sk_start_test",
            ))
            .await
            .unwrap();
        assert!(
            disabled_refusal
                .session
                .collect_uncommitted()
                .await
                .is_err()
        );
        assert_eq!(
            markers.records.load(Ordering::SeqCst),
            1,
            "disabled protection must not record a new marker"
        );
        provider.mode.store(1, Ordering::SeqCst);
        let mut allowed = service
            .start(protected_request(
                &service,
                ClientTransport::HttpSse,
                "child-a",
                "sk_start_test",
            ))
            .await
            .unwrap();
        allowed.session.collect_uncommitted().await.unwrap();
        allowed.session.commit_downstream(Some(200)).await.unwrap();
        handle.publish(snapshot("openai", true));
        assert!(
            service
                .start(protected_request(
                    &service,
                    ClientTransport::HttpSse,
                    "child-a",
                    "sk_start_test"
                ))
                .await
                .is_err()
        );
        assert_eq!(markers.records.load(Ordering::SeqCst), 1);
        markers.now.store(11, Ordering::SeqCst);
        let mut expired = service
            .start(protected_request(
                &service,
                ClientTransport::HttpSse,
                "child-a",
                "sk_start_test",
            ))
            .await
            .unwrap();
        expired.session.collect_uncommitted().await.unwrap();
        expired.session.commit_downstream(Some(200)).await.unwrap();
    });
}

#[test]
fn ambiguous_non_policy_failure_does_not_poison_the_session_or_replay() {
    block_on(async {
        let markers = Arc::new(Markers::default());
        let provider = Arc::new(PolicyProvider {
            kind: "openai",
            mode: AtomicUsize::new(2),
            calls: AtomicUsize::new(0),
        });
        let service = DefaultExecutionService::new(
            RuntimeSnapshotHandle::new(snapshot("openai", true)),
            Arc::new(TrackingExecutionStore::default()),
            ProviderRegistry::new([provider.clone() as Arc<dyn Provider>]).unwrap(),
            Arc::new(Admissions::default()),
            Arc::new(UnusedContinuation),
            Arc::new(RecordingClientApiKeyUsage::default()),
            Arc::new(crate::RecordingDiagnostics::default()),
        )
        .with_cyber_sessions(markers.clone());
        for expected in 1..=2 {
            let mut started = service
                .start(protected_request(
                    &service,
                    ClientTransport::HttpJson,
                    "child-a",
                    "sk_start_test",
                ))
                .await
                .unwrap();
            assert!(started.session.collect_uncommitted().await.is_err());
            assert_eq!(provider.calls.load(Ordering::SeqCst), expected);
        }
        assert_eq!(markers.records.load(Ordering::SeqCst), 0);
    });
}
