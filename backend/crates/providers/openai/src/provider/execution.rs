//! OpenAI attempt 的选择、发送与响应流执行。

use gateway_core::metering::{CalculatedCost, Usage};

use super::*;

impl CodexProvider {
    pub(super) async fn execute_image(
        &self,
        image: &ImageRequest,
        candidate: &ProviderCandidate,
        context: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        if image.payload().protocol() != PROVIDER_NAME || candidate.upstream_model().is_some() {
            return Err(provider_error(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::NotSent,
            ));
        }
        let (endpoint_path, response_origin) = match image.kind() {
            ImageRequestKind::Generation => (
                CODEX_IMAGE_GENERATIONS_PATH,
                self.image_generations_url.clone(),
            ),
            ImageRequestKind::Edit => (CODEX_IMAGE_EDITS_PATH, self.image_edits_url.clone()),
        };
        let image_turn_id = image
            .payload()
            .context()
            .get("image_turn_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let session_affinity = derive_codex_endpoint_session_affinity(
            image.payload(),
            context.client_api_key_ref(),
            "session_id",
        );
        self.execute_raw_json_endpoint(
            context,
            RawJsonEndpointRequest {
                response_origin,
                endpoint_path,
                body: image.payload().body().clone(),
                image_turn_id,
                turn_metadata: None,
                override_device_metadata: false,
                session_affinity,
            },
        )
        .await
    }

    pub(super) async fn execute_search(
        &self,
        search: &StandaloneSearchRequest,
        candidate: &ProviderCandidate,
        context: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        if search.payload().protocol() != PROVIDER_NAME || candidate.upstream_model().is_some() {
            return Err(provider_error(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::NotSent,
            ));
        }
        let turn_metadata = search
            .payload()
            .context()
            .get("turn_metadata")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let session_affinity = derive_codex_endpoint_session_affinity(
            search.payload(),
            context.client_api_key_ref(),
            "id",
        );
        let request_body_override = self.request_body_override.snapshot();
        let body =
            apply_standalone_search_override(search.payload().body(), &request_body_override);
        self.execute_raw_json_endpoint(
            context,
            RawJsonEndpointRequest {
                response_origin: self.search_url.clone(),
                endpoint_path: CODEX_ALPHA_SEARCH_PATH,
                body,
                image_turn_id: None,
                turn_metadata,
                override_device_metadata: request_body_override.enabled(),
                session_affinity,
            },
        )
        .await
    }

    async fn execute_raw_json_endpoint(
        &self,
        context: AttemptContext,
        request: RawJsonEndpointRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let selection_started_at = Instant::now();
        let lease = self
            .selector
            .select_for_provider_endpoint(&SelectCodexProviderEndpointCredential {
                request_url: &request.response_origin,
                attempt: &context,
                session_affinity: request.session_affinity.as_ref(),
            })
            .await
            .map_err(map_selection_error)?;
        let account_selection_wait_ms =
            u64::try_from(selection_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let lease = Arc::new(lease);
        let allows_account_state_mutation = lease.allows_account_state_mutation();
        let provider_kind = ProviderKind::new(PROVIDER_NAME)
            .map_err(|_| provider_error(ProviderErrorKind::Protocol, UpstreamSendState::NotSent))?;
        let metadata = ProviderCallMetadata::for_provider_endpoint(
            provider_kind,
            lease.account_id().clone(),
            UpstreamTransport::new(HTTP_JSON_TRANSPORT).map_err(|_| {
                provider_error(ProviderErrorKind::Protocol, UpstreamSendState::NotSent)
            })?,
        )
        .with_selection_observation(ProviderSelectionObservation::new(
            account_selection_wait_ms,
            lease.capacity_snapshot(),
        ));
        // Standalone Provider 端点没有可证明的账号 owner；Search metadata 必须按
        // 跨账号输入收敛到当前 lease，不能沿用下游声明的账号或 installation identity。
        let turn_metadata = request.turn_metadata.as_deref().and_then(|metadata| {
            crate::transport::request::scope_turn_metadata(
                metadata,
                lease.installation_id(),
                true,
                request.override_device_metadata,
            )
        });
        let events = cold_json_response_stream(ColdJsonResponse {
            client: self.client.for_account(lease.account()).map_err(|_| {
                provider_error(ProviderErrorKind::Unavailable, UpstreamSendState::NotSent)
            })?,
            response_origin: request.response_origin,
            endpoint_path: request.endpoint_path,
            body: request.body,
            image_turn_id: request.image_turn_id,
            turn_metadata,
            context,
            selector: Arc::clone(&self.selector),
            quota: Arc::clone(&self.quota),
            lease: Arc::clone(&lease),
            output_started_at: Instant::now(),
            session_affinity_key: request.session_affinity.map(CodexSessionAffinity::into_key),
        });
        let stream = ProviderStream::new(metadata, events, lease);
        Ok(if allows_account_state_mutation {
            stream.with_filtered_account_feedback(
                Arc::clone(&self.account_feedback),
                openai_failure_affects_account_score,
            )
        } else {
            stream
        })
    }
}

struct RawJsonEndpointRequest {
    response_origin: Url,
    endpoint_path: &'static str,
    body: Bytes,
    image_turn_id: Option<String>,
    turn_metadata: Option<String>,
    override_device_metadata: bool,
    session_affinity: Option<CodexSessionAffinity>,
}

pub(super) struct ColdResponse {
    pub(super) client: CodexBackendClient,
    pub(super) response_origin: Url,
    pub(super) request: CodexResponsesRequest,
    pub(super) upstream_model: UpstreamModelId,
    pub(super) transport_policy: CodexProviderTransport,
    pub(super) context: AttemptContext,
    pub(super) selector: Arc<CodexCredentialSelector>,
    pub(super) quota: Arc<CodexCredentialQuotaService>,
    pub(super) catalog: Arc<CodexCredentialCatalogService>,
    pub(super) lease: Arc<CodexCredentialLease>,
    pub(super) output_started_at: Instant,
    pub(super) session_affinity_key: Option<ProviderSessionAffinityKey>,
    pub(super) session_affinity_key_hash: Option<String>,
    pub(super) session_transport_recovery: CodexSessionTransportRecovery,
    pub(super) websocket_retry_count: u32,
    pub(super) stream_max_retries: u32,
    pub(super) session_capture: Option<OpenAiSessionCapture>,
    pub(super) turn_state_store: Option<Arc<dyn TurnStateStore>>,
}

pub(super) struct ColdJsonResponse {
    pub(super) client: CodexBackendClient,
    pub(super) response_origin: Url,
    pub(super) endpoint_path: &'static str,
    pub(super) body: Bytes,
    pub(super) image_turn_id: Option<String>,
    pub(super) turn_metadata: Option<String>,
    pub(super) context: AttemptContext,
    pub(super) selector: Arc<CodexCredentialSelector>,
    pub(super) quota: Arc<CodexCredentialQuotaService>,
    pub(super) lease: Arc<CodexCredentialLease>,
    pub(super) output_started_at: Instant,
    pub(super) session_affinity_key: Option<ProviderSessionAffinityKey>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct OpenAiSessionState {
    pub(super) account_id: String,
    pub(super) conversation_id: Option<String>,
    #[serde(default)]
    pub(super) turn_state: Option<String>,
    #[serde(default)]
    pub(super) client_turn_id: Option<String>,
    pub(super) continuation_scope: OpenAiContinuationScope,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OpenAiContinuationScope {
    Persisted,
    ConnectionLocal,
    ReplayRequired,
}

pub(super) struct OpenAiSessionCapture {
    pub(super) account_id: String,
    pub(super) conversation_id: Option<String>,
    pub(super) turn_state: Option<String>,
    pub(super) client_turn_id: Option<String>,
    pub(super) response_store: bool,
    pub(super) continuation_scope: Option<OpenAiContinuationScope>,
}

pub(super) fn same_client_turn(previous: Option<&str>, current: Option<&str>) -> bool {
    previous
        .zip(current)
        .is_some_and(|(previous, current)| !previous.is_empty() && previous == current)
}

pub(super) fn decode_openai_session_state(request: &GenerateRequest) -> Option<OpenAiSessionState> {
    request
        .provider_session_state(PROVIDER_NAME)
        .and_then(|state| serde_json::from_value(Value::Object(state.payload().clone())).ok())
}

pub(super) fn encode_openai_session_state(
    state: OpenAiSessionState,
) -> Result<ProviderSessionState, ProviderError> {
    let Value::Object(payload) = serde_json::to_value(state)
        .map_err(|_| provider_error(ProviderErrorKind::Protocol, UpstreamSendState::Sent))?
    else {
        return Err(provider_error(
            ProviderErrorKind::Protocol,
            UpstreamSendState::Sent,
        ));
    };
    ProviderSessionState::new(PROVIDER_NAME, payload)
        .map_err(|_| provider_error(ProviderErrorKind::Protocol, UpstreamSendState::Sent))
}

fn encode_openai_session_capture(
    capture: &OpenAiSessionCapture,
) -> Result<ProviderSessionState, ProviderError> {
    let Some(continuation_scope) = capture.continuation_scope else {
        return Err(provider_error(
            ProviderErrorKind::Protocol,
            UpstreamSendState::Sent,
        ));
    };
    encode_openai_session_state(OpenAiSessionState {
        account_id: capture.account_id.clone(),
        conversation_id: capture.conversation_id.clone(),
        turn_state: capture.turn_state.clone(),
        client_turn_id: capture.client_turn_id.clone(),
        continuation_scope,
    })
}

pub(super) fn attach_openai_session_update(
    events: &mut [ProviderEvent],
    capture: &mut Option<OpenAiSessionCapture>,
) {
    let Some(terminal_index) = events
        .iter()
        .position(|event| terminal_response_output(event).is_some())
    else {
        return;
    };
    let Some(capture) = capture.take() else {
        return;
    };
    let Ok(update) = encode_openai_session_capture(&capture) else {
        return;
    };
    events[terminal_index].attach_session_update(update);
}

pub(super) fn terminal_response_output(event: &ProviderEvent) -> Option<&[Value]> {
    let wire = event.wire_event()?;
    if wire.protocol() != PROVIDER_NAME {
        return None;
    }
    let event_type = wire
        .event_type()
        .or_else(|| wire.data().get("type").and_then(Value::as_str));
    matches!(
        event_type,
        Some("response.completed" | "response.incomplete")
    )
    .then(|| {
        wire.data()
            .pointer("/response/output")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
    })
    .flatten()
}

pub(super) enum CodexHandshakeAttemptError {
    Client(CodexClientError),
    Cancelled,
    Timeout,
}

pub(super) async fn create_response_attempt(
    client: &CodexBackendClient,
    request: &CodexResponsesRequest,
    request_context: CodexRequestContext<'_>,
    account_id: &str,
    deadline: SystemTime,
    cancellation: &CancellationToken,
) -> Result<CodexBackendStreamingResponse, CodexHandshakeAttemptError> {
    let Some(handshake_deadline) = remaining(deadline) else {
        return Err(CodexHandshakeAttemptError::Timeout);
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CodexHandshakeAttemptError::Cancelled),
        _ = tokio::time::sleep(handshake_deadline) => Err(CodexHandshakeAttemptError::Timeout),
        response = client.create_response_stream_with_pool_account(
            request,
            request_context,
            Some(account_id),
        ) => response.map_err(CodexHandshakeAttemptError::Client),
    }
}

pub(super) fn map_handshake_attempt_error(
    error: CodexHandshakeAttemptError,
) -> MappedProviderFailure {
    match error {
        CodexHandshakeAttemptError::Client(error) => map_handshake_error(error),
        CodexHandshakeAttemptError::Cancelled => MappedProviderFailure::plain(provider_error(
            ProviderErrorKind::Cancelled,
            UpstreamSendState::Ambiguous,
        )),
        CodexHandshakeAttemptError::Timeout => MappedProviderFailure::plain(provider_error(
            ProviderErrorKind::Timeout,
            UpstreamSendState::Ambiguous,
        )),
    }
}

pub(super) async fn create_json_attempt(
    request: &ColdJsonResponse,
    account: &ProviderAccount,
    installation_id: &str,
    authorization: &SecretString,
    cookie_header: Option<&SecretString>,
    account_selection: CodexAccountSelectionTelemetry<'_>,
) -> Result<CodexBackendJsonResponse, CodexHandshakeAttemptError> {
    let Some(handshake_deadline) = remaining(request.context.deadline()) else {
        return Err(CodexHandshakeAttemptError::Timeout);
    };
    let request_id = request.context.request_id().as_str();
    let trace = request.context.trace();
    let mut request_context = CodexRequestContext::auxiliary(
        authorization.expose_secret(),
        account.upstream_account_id(),
        request_id,
        Some(installation_id),
    );
    request_context.trace = Some(&trace);
    request_context.cookie_header = cookie_header.map(ExposeSecret::expose_secret);
    request_context.turn_metadata = request.turn_metadata.as_deref();
    request_context.account_selection = account_selection;
    tokio::select! {
        biased;
        _ = request.context.cancellation().cancelled() => Err(CodexHandshakeAttemptError::Cancelled),
        _ = tokio::time::sleep(handshake_deadline) => Err(CodexHandshakeAttemptError::Timeout),
        response = request.client.post_raw_json(
            request.endpoint_path,
            request.body.clone(),
            request.image_turn_id.as_deref(),
            request_context,
        ) => response.map_err(CodexHandshakeAttemptError::Client),
    }
}

pub(super) fn cold_json_response_stream(request: ColdJsonResponse) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let allows_account_state_mutation = request.lease.allows_account_state_mutation();
        let failure_context = OpenAiFailureContext {
            client: &request.client,
            selector: &request.selector,
            quota: &request.quota,
            response_origin: &request.response_origin,
            cyber_policy_scope: None,
            allows_account_state_mutation,
            selection_policy: request.context.account_selection_policy(),
            cyber_session_block_enabled: request.context.cyber_session_block_enabled(),
        };
        let active_account = request.lease.account().clone();
        let cookie_header = build_cookie_header(request.lease.cookies())?;
        let authorization = request
            .lease
            .authentication()
            .authorization_header()
            .map_err(|_| {
                provider_error(
                    ProviderErrorKind::Unauthorized,
                    UpstreamSendState::NotSent,
                )
            })?;
        let account_selection = CodexAccountSelectionTelemetry::new(
            request.lease.affinity_hit(),
            request.lease.escape_reason(),
            request.lease.account_switch(),
        );
        let response = create_json_attempt(
            &request,
            &active_account,
            request.lease.installation_id(),
            &authorization,
            cookie_header.as_ref(),
            account_selection,
        )
        .await;
        if let Err(CodexHandshakeAttemptError::Client(error)) = &response {
            log_client_upstream_error(
                UpstreamErrorLogContext::new(&request.context, &active_account, None),
                error,
            );
        }
        let response = match response.map_err(map_handshake_attempt_error) {
            Ok(response) => response,
            Err(mut failure) => {
                if let Some(observation) = failure.observation.take() {
                    yield ProviderEvent::observation(observation);
                }
                apply_failure(&failure_context, &active_account, &failure).await;
                Err(failure.error)?;
                return;
            }
        };

        if allows_account_state_mutation {
            request.selector.reset_overload_streak(&active_account);
        }
        if allows_account_state_mutation && let Some(key) = request.session_affinity_key.as_ref() {
            // JSON 已完整接收；在首个 yield 前提交亲和迁移，避免下游取消漏掉更新。
            request.selector.update_session_affinity(
                key,
                request.lease.affinity_expected_account_id(),
                active_account.id(),
            ).await;
        }
        let mut metrics = response.transport_metrics.clone();
        metrics.first_event_ms = Some(
            i64::try_from(request.output_started_at.elapsed().as_millis()).unwrap_or(i64::MAX),
        );
        if let Some(observation) = codex_response_observation(
            CodexBackendTransport::HttpJson,
            &response.diagnostics,
            &response.response_metadata,
            &metrics,
            None,
            openai_response_timings(&metrics, &response.response_metadata),
        ) {
            yield ProviderEvent::observation(observation);
        }
        if allows_account_state_mutation {
            synchronize_passive_quota_headers(
                &request.quota,
                &active_account,
                &response.rate_limit_headers,
            )
            .await;
            if !response.set_cookie_headers.is_empty()
                && let Err(error) = request
                    .selector
                    .capture_response_cookies(
                        &active_account,
                        &request.response_origin,
                        &response.set_cookie_headers,
                    )
                    .await
            {
                tracing::warn!(
                    account_id = %active_account.id(),
                    error = %error,
                    "Failed to persist OpenAI provider endpoint response cookies"
                );
            }
        }

        let response_meta =
            ResponseMeta::for_provider_endpoint(request.context.request_id().as_str());
        yield ProviderEvent::canonical(GatewayEvent::Started(response_meta.clone()));
        if matches!(request.endpoint_path, CODEX_IMAGE_GENERATIONS_PATH | CODEX_IMAGE_EDITS_PATH)
            && let Some((usage, cost)) = image_response_metering(&request.body, &response.body)
        {
            yield ProviderEvent::canonical(GatewayEvent::Usage(usage));
            if let Some(cost) = cost {
                yield ProviderEvent::canonical(GatewayEvent::CalculatedCost(cost));
            }
        }
        let wire = ProtocolWireEvent::raw_json(PROVIDER_NAME, response.body).map_err(|_| {
            provider_error(ProviderErrorKind::Protocol, UpstreamSendState::Sent)
        })?;
        yield ProviderEvent::wire(wire);
        yield ProviderEvent::canonical(GatewayEvent::Completed(
            response_meta.with_finish_reason(FinishReason::Stop),
        ));
    })
}

fn error_turn_state(error: &CodexClientError) -> Option<(CodexObservedTurnState, &'static str)> {
    let (response, transport) = match error {
        CodexClientError::Upstream {
            client_response: Some(response),
            transport,
            ..
        } => (response.as_ref(), *transport),
        CodexClientError::WebSocket(CodexWebSocketExchangeError::Upstream(failure)) => (
            failure.client_response.as_deref()?,
            CodexBackendTransport::WebSocket,
        ),
        _ => return None,
    };
    let receipt = match response.turn_state_observation() {
        Some(receipt) => receipt.clone(),
        None => {
            let value = response
                .client_headers()
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-codex-turn-state"))?;
            CodexObservedTurnState::new(std::str::from_utf8(&value.1).ok()?.to_owned())
        }
    };
    Some((
        receipt,
        if transport == CodexBackendTransport::WebSocket {
            "websocket"
        } else {
            "http"
        },
    ))
}

fn observe_turn_state(
    store: Option<&Arc<dyn TurnStateStore>>,
    account_id: &str,
    receipt: &CodexObservedTurnState,
    transport: &str,
    upstream_response_id: Option<&str>,
    client_turn_id: Option<&str>,
) {
    let Some(store) = store else { return };
    store.enqueue_observation(TurnStateObservation {
        id: receipt.id.clone(),
        account_id: account_id.to_owned(),
        value: receipt.value.clone(),
        observed_at: receipt.observed_at,
        transport: transport.to_owned(),
        upstream_response_id: upstream_response_id.map(str::to_owned),
        client_turn_id: client_turn_id.map(str::to_owned),
    });
}

fn observe_received_turn_state(
    store: Option<&Arc<dyn TurnStateStore>>,
    transport: CodexBackendTransport,
    account_id: &str,
    receipt: Option<&CodexObservedTurnState>,
    upstream_response_id: Option<&str>,
    client_turn_id: Option<&str>,
) {
    if let Some(receipt) = receipt {
        observe_turn_state(
            store,
            account_id,
            receipt,
            match transport {
                CodexBackendTransport::WebSocket => "websocket",
                _ => "http",
            },
            upstream_response_id,
            client_turn_id,
        );
    }
}

async fn take_turn_state_observations(
    updates: Option<&CodexWebSocketTurnStateObservations>,
) -> Vec<CodexObservedTurnState> {
    let Some(updates) = updates else {
        return Vec::new();
    };
    std::mem::take(&mut *updates.lock().await)
}

async fn current_turn_state_response_id(
    decoder_response_id: Option<&str>,
    websocket_response_id: Option<&CodexWebSocketTurnStateResponseId>,
) -> Option<String> {
    if let Some(response_id) = decoder_response_id {
        return Some(response_id.to_owned());
    }
    let response_id = websocket_response_id?;
    response_id.lock().await.clone()
}

async fn drain_turn_state_observations(
    updates: Option<&CodexWebSocketTurnStateObservations>,
    received: &mut VecDeque<CodexObservedTurnState>,
    store: Option<&Arc<dyn TurnStateStore>>,
    account_id: &str,
    decoder_response_id: Option<&str>,
    websocket_response_id: Option<&CodexWebSocketTurnStateResponseId>,
    client_turn_id: Option<&str>,
) {
    let upstream_response_id =
        current_turn_state_response_id(decoder_response_id, websocket_response_id).await;
    for receipt in take_turn_state_observations(updates).await {
        observe_turn_state(
            store,
            account_id,
            &receipt,
            "websocket",
            upstream_response_id.as_deref(),
            client_turn_id,
        );
        if upstream_response_id.is_none() {
            if received.len() == MAX_PENDING_TURN_STATE_RECEIPTS {
                received.pop_front();
            }
            received.push_back(receipt);
        }
    }
}

fn enrich_turn_state_observations(
    received: &VecDeque<CodexObservedTurnState>,
    store: Option<&Arc<dyn TurnStateStore>>,
    account_id: &str,
    transport: CodexBackendTransport,
    upstream_response_id: &str,
    client_turn_id: Option<&str>,
) {
    for receipt in received {
        observe_received_turn_state(
            store,
            transport,
            account_id,
            Some(receipt),
            Some(upstream_response_id),
            client_turn_id,
        );
    }
}

fn image_response_metering(
    request_body: &[u8],
    body: &[u8],
) -> Option<(Usage, Option<CalculatedCost>)> {
    // 只保留 usage，跳过通常很大的 base64 图片；原始响应仍按字节透传。
    #[derive(Deserialize)]
    struct ImageUsageEnvelope {
        usage: Option<Value>,
    }

    let raw = serde_json::from_slice::<ImageUsageEnvelope>(body)
        .ok()?
        .usage?;
    let mut usage = Usage::new();
    usage.input_tokens = raw.get("input_tokens").and_then(Value::as_u64);
    usage.output_tokens = raw.get("output_tokens").and_then(Value::as_u64);
    usage.cached_tokens = raw
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64);
    usage.image_input_tokens = raw
        .pointer("/input_tokens_details/image_tokens")
        .and_then(Value::as_u64);
    usage.image_output_tokens = raw
        .pointer("/output_tokens_details/image_tokens")
        .and_then(Value::as_u64);
    // 总量是上游独立报告的事实；图片明细是总输入/输出的子集，不能再次相加。
    usage.total_tokens = raw.get("total_tokens").and_then(Value::as_u64);
    let cost = crate::transport::usage::image_calculated_cost(request_body, &raw);
    (usage != Usage::default()).then_some((usage, cost))
}

pub(super) fn cold_response_stream(response: ColdResponse) -> EventStream {
    let ColdResponse {
        client,
        response_origin,
        request,
        upstream_model,
        transport_policy,
        context,
        selector,
        quota,
        catalog,
        lease,
        output_started_at,
        session_affinity_key,
        session_affinity_key_hash,
        session_transport_recovery,
        websocket_retry_count,
        stream_max_retries,
        mut session_capture,
        turn_state_store,
    } = response;
    Box::pin(async_stream::try_stream! {
        let cyber_policy_scope = lease.cyber_policy_scope().cloned();
        let allows_account_state_mutation = lease.allows_account_state_mutation();
        let failure_context = OpenAiFailureContext {
            client: &client,
            selector: &selector,
            quota: &quota,
            response_origin: &response_origin,
            cyber_policy_scope: cyber_policy_scope.as_ref(),
            allows_account_state_mutation,
            selection_policy: context.account_selection_policy(),
            cyber_session_block_enabled: context.cyber_session_block_enabled(),
        };
        let mut active_account = lease.account().clone();
        let cookie_header = build_cookie_header(lease.cookies())?;
        let authorization = lease
            .authentication()
            .authorization_header()
            .map_err(|_| {
                provider_error(
                    ProviderErrorKind::Unauthorized,
                    UpstreamSendState::NotSent,
                )
            })?;
        let request_id = context.request_id().as_str().to_owned();
        let cancellation = context.cancellation().clone();
        let account_selection = CodexAccountSelectionTelemetry::new(
            lease.affinity_hit(),
            lease.escape_reason(),
            lease.account_switch(),
        );
        let request_transport_requirement = transport_requirement(&request);
        let trace = context.trace();
        let response = create_response_attempt(
            &client,
            &request,
            codex_request_context(
                &request,
                &request_id,
                &active_account,
                lease.installation_id(),
                &authorization,
                cookie_header.as_ref(),
                account_selection,
            ).with_trace(&trace),
            active_account.id().as_str(),
            context.deadline(),
            &cancellation,
        )
        .await;
        let websocket_failure_policy = match &response {
            Err(CodexHandshakeAttemptError::Client(error))
                if transport_policy == CodexProviderTransport::PreferWebSocket =>
            {
                websocket_client_failure_policy(error)
            }
            _ => None,
        };
        if let Err(CodexHandshakeAttemptError::Client(error)) = &response {
            if let (Some(store), Some((receipt, transport))) =
                (turn_state_store.as_ref(), error_turn_state(error))
            {
                observe_turn_state(
                    Some(store),
                    active_account.id().as_str(),
                    &receipt,
                    transport,
                    None,
                    request.client_turn_id.as_deref(),
                );
            }
            log_client_upstream_error(
                UpstreamErrorLogContext::new(&context, &active_account, None),
                error,
            );
        }
        let response = response.map_err(map_handshake_attempt_error);
        let response = match response {
            Ok(response) => response,
            Err(mut failure) => {
                if let Some(policy) = websocket_failure_policy {
                    apply_websocket_recovery_policy(
                        &mut failure,
                        WebSocketRecoveryContext {
                            policy,
                            requirement: request_transport_requirement,
                            retry_count: websocket_retry_count,
                            max_retries: stream_max_retries,
                            request_id: context.request_id().as_str(),
                            attempt_index: context.attempt_index().get(),
                            account_id: active_account.id().as_str(),
                            session_affinity_key: session_affinity_key.as_ref(),
                            session_affinity_key_hash: session_affinity_key_hash.as_deref(),
                            session_transport_recovery: &session_transport_recovery,
                        },
                    );
                }
                if let Some(observation) = failure.observation.take() {
                    yield ProviderEvent::observation(observation);
                }
                apply_failure(&failure_context, &active_account, &failure)
                .await;
                Err(failure.error)?;
                return;
            }
        };
        let initial_turn_state_observation = response.turn_state_observation.clone();
        observe_received_turn_state(
            turn_state_store.as_ref(),
            response.transport,
            active_account.id().as_str(),
            initial_turn_state_observation.as_ref(),
            None,
            request.client_turn_id.as_deref(),
        );
        if !accepts_backend_transport(transport_policy, response.transport) {
            let failure = MappedProviderFailure::plain(provider_error(
                ProviderErrorKind::Protocol,
                UpstreamSendState::Sent,
            ));
            apply_failure(&failure_context, &active_account, &failure)
            .await;
            Err(failure.error)?;
            return;
        }
        if let Some(capture) = session_capture.as_mut() {
            capture.continuation_scope = Some(if capture.response_store {
                OpenAiContinuationScope::Persisted
            } else if response.transport == CodexBackendTransport::WebSocket
                && response.connection_local_continuation
            {
                OpenAiContinuationScope::ConnectionLocal
            } else {
                OpenAiContinuationScope::ReplayRequired
            });
            capture.turn_state = response.turn_state.clone().or(capture.turn_state.clone());
        }
        let mut observation_state = OpenAiResponseObservationState::from_backend_response(
            &response,
            &request,
        );
        if let Some(observation) = observation_state.observation(None) {
            yield ProviderEvent::observation(observation);
        }
        if let Some(etag) = response.response_metadata.models_etag.as_deref()
            && let Err(error) = catalog.observe_response_etag(etag)
        {
            tracing::warn!(
                error = %error,
                "OpenAI model ETag observation was rejected"
            );
        }
        if allows_account_state_mutation
            && !response.set_cookie_headers.is_empty()
            && let Ok(outcome) = selector
                .capture_response_cookies(
                    &active_account,
                    &response_origin,
                    &response.set_cookie_headers,
                )
                .await
                && let Some(revision) = outcome.credential_revision
                && let Ok(current) = selector.current_account(active_account.id()).await
                && current.revision().get() == revision
        {
            active_account = current;
        }
        let response_transport = response.transport;
        let response_cyber_policy_refusal = response.cyber_policy_refusal;
        let websocket_connection_id = response.websocket_connection_id;
        let mut body = response.body;
        let mut failure_diagnostics = response.diagnostics.clone();
        if response_transport == CodexBackendTransport::WebSocket {
            // opening ID 标识连接，不可作为缺失请求级错误头时的当前请求 ID。
            failure_diagnostics.request_id = None;
        }
        let failure_set_cookie_headers = response.set_cookie_headers.clone();
        let failure_rate_limit_headers = response.rate_limit_headers.clone();
        let mut passive_quota_observation =
            OpenAiPassiveQuotaObservation::new(response.rate_limit_headers);
        let rate_limit_updates = response.rate_limit_updates;
        let turn_state_updates = response.turn_state_update;
        let turn_state_observation_updates = response.turn_state_observations;
        let turn_state_response_id = response.turn_state_response_id;
        let mut received_turn_state_observations = initial_turn_state_observation
            .into_iter()
            .collect::<VecDeque<_>>();
        let mut turn_state_observations_enriched = false;
        // OpenAI 线路为透明代理：HTTP SSE 与 WebSocket 两条上游均启用 raw 透传，
        // 下游按字节转发上游原文，避免 serde 往返改写数值/精度（大整数→f64、logprobs 等）。
        // WS 帧由 reducer 以 encode_sse_event(&event, raw) 逐字节内嵌上游原始 JSON
        // （transport/protocol/websocket.rs），push_frames 抽出的 data 即上游原文。
        let mut decoder = CodexCanonicalDecoder::new(upstream_model.as_str())
            .with_requested_service_tier(request.service_tier())
            .with_request_tool_pricing(upstream_model.as_str(), request.tools())
            .with_raw_sse_passthrough();
        let mut pre_commit_events = PreCommitClientEvents::new();
        loop {
            let Some(stream_deadline) = remaining(context.deadline()) else {
                drain_turn_state_observations(
                    turn_state_observation_updates.as_ref(),
                    &mut received_turn_state_observations,
                    turn_state_store.as_ref(),
                    active_account.id().as_str(),
                    decoder.response_id(),
                    turn_state_response_id.as_ref(),
                    request.client_turn_id.as_deref(),
                )
                .await;
                if allows_account_state_mutation {
                    synchronize_passive_quota(
                        &quota,
                        &active_account,
                        passive_quota_observation.rate_limits(),
                    )
                    .await;
                }
                Err(provider_error(ProviderErrorKind::Timeout, UpstreamSendState::Sent))?;
                return;
            };
            let replay_grace_deadline = pre_commit_events.replay_grace_deadline();
            let next = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(MappedProviderFailure::plain(provider_error(
                    ProviderErrorKind::Cancelled,
                    UpstreamSendState::Sent,
                ))),
                _ = tokio::time::sleep(stream_deadline) => Err(MappedProviderFailure::plain(provider_error(
                    ProviderErrorKind::Timeout,
                    UpstreamSendState::Sent,
                ))),
                _ = wait_for_replay_grace(replay_grace_deadline) => Ok(PreCommitPoll::GraceElapsed),
                chunk = body.next() => match chunk {
                    Some(Ok(chunk)) => Ok(PreCommitPoll::Upstream(Some(chunk))),
                    Some(Err(error)) => {
                        log_client_upstream_error(
                            UpstreamErrorLogContext::new(
                                &context,
                                &active_account,
                                websocket_connection_id,
                            ),
                            &error,
                        );
                        Err(map_stream_error(error))
                    }
                    None => Ok(PreCommitPoll::Upstream(None)),
                },
            };
            let next = match next {
                Ok(PreCommitPoll::Upstream(next)) => next,
                Ok(PreCommitPoll::GraceElapsed) => {
                    for event in pre_commit_events.commit_pending() {
                        yield event;
                    }
                    continue;
                }
                Err(mut failure) => {
                    drain_turn_state_observations(
                        turn_state_observation_updates.as_ref(),
                        &mut received_turn_state_observations,
                        turn_state_store.as_ref(),
                        active_account.id().as_str(),
                        decoder.response_id(),
                        turn_state_response_id.as_ref(),
                        request.client_turn_id.as_deref(),
                    )
                    .await;
                    let updates = take_rate_limit_updates(rate_limit_updates.as_ref()).await;
                    let rate_limits_changed = if updates.is_empty() {
                        false
                    } else {
                        passive_quota_observation.observe(&updates);
                        let update_headers = rate_limit_update_headers(&updates);
                        observation_state.merge_rate_limit_headers(&update_headers)
                    };
                    let turn_state_merge = merge_turn_state_update(
                        turn_state_updates.as_ref(),
                        &mut session_capture,
                        &mut observation_state,
                    )
                    .await;
                    let observation_event = if rate_limits_changed || turn_state_merge.is_some() {
                        observation_state.observation(None).map(ProviderEvent::observation)
                    } else {
                        None
                    };
                    if failure.websocket_transport_retryable
                        && response_transport == CodexBackendTransport::WebSocket
                        && (matches!(
                            failure.error.send_state(),
                            UpstreamSendState::Sent | UpstreamSendState::Ambiguous
                        ) || !pre_commit_events.is_committed())
                    {
                        apply_websocket_recovery_policy(
                            &mut failure,
                            WebSocketRecoveryContext {
                                policy: WebSocketFailurePolicy::Budgeted,
                                requirement: request_transport_requirement,
                                retry_count: websocket_retry_count,
                                max_retries: stream_max_retries,
                                request_id: context.request_id().as_str(),
                                attempt_index: context.attempt_index().get(),
                                account_id: active_account.id().as_str(),
                                session_affinity_key: session_affinity_key.as_ref(),
                                session_affinity_key_hash: session_affinity_key_hash.as_deref(),
                                session_transport_recovery: &session_transport_recovery,
                            },
                        );
                    }
                    if let Some(event) = observation_event {
                        yield event;
                    }
                    if allows_account_state_mutation {
                        synchronize_passive_quota(
                            &quota,
                            &active_account,
                            passive_quota_observation.rate_limits(),
                        )
                        .await;
                    }
                    apply_failure(&failure_context, &active_account, &failure)
                    .await;
                    Err(failure.error)?;
                    return;
                }
            };
            let Some(chunk) = next else { break; };
            let updates = take_rate_limit_updates(rate_limit_updates.as_ref()).await;
            let rate_limits_changed = if updates.is_empty() {
                false
            } else {
                passive_quota_observation.observe(&updates);
                observation_state.merge_rate_limit_headers(&rate_limit_update_headers(&updates))
            };
            drain_turn_state_observations(
                turn_state_observation_updates.as_ref(),
                &mut received_turn_state_observations,
                turn_state_store.as_ref(),
                active_account.id().as_str(),
                decoder.response_id(),
                turn_state_response_id.as_ref(),
                request.client_turn_id.as_deref(),
            )
            .await;
            let turn_state_merge = merge_turn_state_update(
                turn_state_updates.as_ref(),
                &mut session_capture,
                &mut observation_state,
            )
            .await;
            let turn_state_changed = turn_state_merge.unwrap_or(false);
            let first_event_changed =
                observation_state.observe_stream_chunk(&chunk, output_started_at);
            let chunk_len = chunk.len();
            let (mut events, canonical_failure) = match decoder.push(&chunk) {
                CodexCanonicalOutcome::Events(events) => (events, None),
                CodexCanonicalOutcome::Failed(failure) => {
                    let (events, error, semantic_output_seen) = failure.into_parts();
                    (events, Some((error, semantic_output_seen)))
                }
            };
            let observed_response_id = current_turn_state_response_id(
                decoder.response_id(),
                turn_state_response_id.as_ref(),
            )
            .await;
            if !turn_state_observations_enriched
                && let Some(response_id) = observed_response_id.as_deref()
            {
                enrich_turn_state_observations(
                    &received_turn_state_observations,
                    turn_state_store.as_ref(),
                    active_account.id().as_str(),
                    response_transport,
                    response_id,
                    request.client_turn_id.as_deref(),
                );
                received_turn_state_observations.clear();
                turn_state_observations_enriched = true;
            }
            pre_commit_events.observe_chunk(chunk_len);
            let service_tier_changed = observation_state
                .observe_upstream_service_tier(decoder.response_service_tier());
            let terminal_failure = canonical_failure.map(|(error, semantic_output_seen)| {
                log_canonical_upstream_error(
                    UpstreamErrorLogContext::new(
                        &context,
                        &active_account,
                        websocket_connection_id,
                    ),
                    response_transport,
                    &error,
                );
                let atomic_upstream_failure = matches!(&error, CodexCanonicalError::Upstream(_));
                let mut failure = map_canonical_error(
                        error,
                        &failure_diagnostics,
                        &failure_set_cookie_headers,
                        &failure_rate_limit_headers,
                        ReplayBoundary::from_semantic_output(
                            semantic_output_seen || pre_commit_events.is_committed(),
                        ),
                    );
                if response_cyber_policy_refusal {
                    failure.error = failure.error.with_cyber_policy_refusal();
                }
                (failure, atomic_upstream_failure)
            });
            let timing_signals = decoder.take_timing_signals();
            let timing_changed = first_event_changed
                || observation_state
                    .observe_timing_signals(timing_signals, output_started_at);
            let completed = events
                .iter()
                .flat_map(ProviderEvent::canonical_facts)
                .any(|event| matches!(event, GatewayEvent::Completed(_)));
            let terminal_changed = completed
                && observation_state.mark_completed(terminal_response_is_incomplete(&events));
            if response_transport == CodexBackendTransport::WebSocket
                && completed && terminal_failure.is_none()
                && let Some(key) = session_affinity_key.as_ref()
            {
                session_transport_recovery.websocket_succeeded(key);
            }
            if allows_account_state_mutation && (completed || terminal_failure.is_some()) {
                synchronize_passive_quota(
                    &quota,
                    &active_account,
                    passive_quota_observation.rate_limits(),
                )
                .await;
            }
            if let Some((failure, _)) = terminal_failure.as_ref() {
                apply_failure(&failure_context, &active_account, failure)
                .await;
            }
            attach_openai_session_update(&mut events, &mut session_capture);
            if allows_account_state_mutation && completed && terminal_failure.is_none() {
                // 完成事件一旦交给下游，Core 可以立刻停止轮询 Provider stream；
                // 在此之前持久化亲和关系，保证成功请求不会因流被提前 drop 而丢失绑定。
                selector
                    .record_success(
                        &active_account,
                        session_affinity_key.as_ref(),
                        lease.affinity_expected_account_id(),
                    )
                    .await;
                selector
                    .observe_cyber_policy_success(cyber_policy_scope.as_ref())
                    .await;
            }
            if (rate_limits_changed
                || service_tier_changed
                || timing_changed
                || turn_state_changed
                || terminal_changed
                || (response_transport == CodexBackendTransport::WebSocket && terminal_failure.is_some()))
                && let Some(observation) = observation_state.observation(
                    terminal_failure.as_ref().map(|(failure, _)| &failure.error)
                )
            {
                yield ProviderEvent::observation(observation);
            }
            if let Some((mut failure, atomic_upstream_failure)) = terminal_failure {
                let failure_after_commit =
                    timing_signals.semantic_output || pre_commit_events.is_committed();
                if failure_after_commit {
                    for event in pre_commit_events.commit(events) {
                        yield event;
                    }
                } else if atomic_upstream_failure {
                    failure.error = failure
                        .error
                        .with_atomic_client_events(pre_commit_events.take_for_failure(events));
                }
                Err(failure.error)?;
                return;
            }
            let events = pre_commit_events.stage(events, timing_signals, completed);
            for event in events {
                yield event;
            }
            if completed {
                return;
            }
        }
        let (mut events, canonical_failure) = match decoder.finish() {
            CodexCanonicalOutcome::Events(events) => (events, None),
            CodexCanonicalOutcome::Failed(failure) => {
                let (events, error, semantic_output_seen) = failure.into_parts();
                (events, Some((error, semantic_output_seen)))
            }
        };
        let terminal_failure = canonical_failure.map(|(error, semantic_output_seen)| {
            log_canonical_upstream_error(
                UpstreamErrorLogContext::new(
                    &context,
                    &active_account,
                    websocket_connection_id,
                ),
                response_transport,
                &error,
            );
            let atomic_upstream_failure = matches!(&error, CodexCanonicalError::Upstream(_));
            let mut failure = map_canonical_error(
                    error,
                    &failure_diagnostics,
                    &failure_set_cookie_headers,
                    &failure_rate_limit_headers,
                    ReplayBoundary::from_semantic_output(
                        semantic_output_seen || pre_commit_events.is_committed(),
                    ),
                );
            if response_cyber_policy_refusal {
                failure.error = failure.error.with_cyber_policy_refusal();
            }
            (failure, atomic_upstream_failure)
        });
        let timing_signals = decoder.take_timing_signals();
        let service_tier_changed = observation_state
            .observe_upstream_service_tier(decoder.response_service_tier());
        let timing_changed = observation_state
            .observe_timing_signals(timing_signals, output_started_at);
        let updates = take_rate_limit_updates(rate_limit_updates.as_ref()).await;
        let rate_limits_changed = if updates.is_empty() {
            false
        } else {
            passive_quota_observation.observe(&updates);
            observation_state.merge_rate_limit_headers(&rate_limit_update_headers(&updates))
        };
        drain_turn_state_observations(
            turn_state_observation_updates.as_ref(),
            &mut received_turn_state_observations,
            turn_state_store.as_ref(),
            active_account.id().as_str(),
            decoder.response_id(),
            turn_state_response_id.as_ref(),
            request.client_turn_id.as_deref(),
        )
        .await;
        let observed_response_id = current_turn_state_response_id(
            decoder.response_id(),
            turn_state_response_id.as_ref(),
        )
        .await;
        if !turn_state_observations_enriched
            && let Some(response_id) = observed_response_id.as_deref()
        {
            enrich_turn_state_observations(
                &received_turn_state_observations,
                turn_state_store.as_ref(),
                active_account.id().as_str(),
                response_transport,
                response_id,
                request.client_turn_id.as_deref(),
            );
            received_turn_state_observations.clear();
        }
        if allows_account_state_mutation {
            synchronize_passive_quota(
                &quota,
                &active_account,
                passive_quota_observation.rate_limits(),
            )
            .await;
        }
        if let Some((failure, _)) = terminal_failure.as_ref() {
            apply_failure(&failure_context, &active_account, failure)
            .await;
        }
        let turn_state_changed = merge_turn_state_update(
            turn_state_updates.as_ref(),
            &mut session_capture,
            &mut observation_state,
        )
        .await
        .unwrap_or(false);
        attach_openai_session_update(&mut events, &mut session_capture);
        let completed = events
            .iter()
            .flat_map(ProviderEvent::canonical_facts)
            .any(|event| matches!(event, GatewayEvent::Completed(_)));
        let terminal_changed = completed
            && observation_state.mark_completed(terminal_response_is_incomplete(&events));
        if response_transport == CodexBackendTransport::WebSocket
            && completed && terminal_failure.is_none()
            && let Some(key) = session_affinity_key.as_ref()
        {
            session_transport_recovery.websocket_succeeded(key);
        }
        if allows_account_state_mutation && completed && terminal_failure.is_none() {
            // 同上：尾部 finish() 也可能产出 completed，亲和记录必须先于任何下游 yield。
            selector
                .record_success(
                    &active_account,
                    session_affinity_key.as_ref(),
                    lease.affinity_expected_account_id(),
                )
                .await;
            selector
                .observe_cyber_policy_success(cyber_policy_scope.as_ref())
                .await;
        }
        if (service_tier_changed
            || timing_changed
            || rate_limits_changed
            || turn_state_changed
            || terminal_changed
            || (response_transport == CodexBackendTransport::WebSocket && terminal_failure.is_some()))
            && let Some(observation) = observation_state.observation(
                terminal_failure.as_ref().map(|(failure, _)| &failure.error)
            )
        {
            yield ProviderEvent::observation(observation);
        }
        if let Some((mut failure, atomic_upstream_failure)) = terminal_failure {
            let failure_after_commit =
                timing_signals.semantic_output || pre_commit_events.is_committed();
            if failure_after_commit {
                for event in pre_commit_events.commit(events) {
                    yield event;
                }
            } else if atomic_upstream_failure {
                failure.error = failure
                    .error
                    .with_atomic_client_events(pre_commit_events.take_for_failure(events));
            }
            Err(failure.error)?;
            return;
        }
        let events = pre_commit_events.finish(events, timing_signals, completed);
        for event in events {
            yield event;
        }
    })
}

async fn merge_turn_state_update(
    updates: Option<&CodexTurnStateUpdate>,
    session_capture: &mut Option<OpenAiSessionCapture>,
    observation_state: &mut OpenAiResponseObservationState,
) -> Option<bool> {
    let updates = updates?;
    let turn_state = updates.lock().await.take()?;
    if let Some(capture) = session_capture.as_mut() {
        capture.turn_state = Some(turn_state.clone());
    }
    Some(observation_state.merge_client_header("x-codex-turn-state", &turn_state))
}
