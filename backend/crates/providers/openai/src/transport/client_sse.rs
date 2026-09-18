use gateway_core::diagnostics::{StreamCapture, StreamFormat, TraceContext, diagnostic_json};

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt, TryStreamExt};
use gateway_core::provider_ports::turn_state::{
    ModelTurnStateObservationScope, TurnStateObservation, TurnStateSent, TurnStateStore,
};
use gateway_protocol::openai::{
    X_OPENAI_MEMGEN_REQUEST_HEADER,
    events::{self, retry_after_seconds_from_body},
    sse::{SseEventDecoder, SseFrame},
};
use reqwest::{
    Client, Response as ReqwestResponse,
    header::{CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;

use crate::transport::{
    catalog::{
        CodexModelCatalogError, CodexModelCatalogSnapshot, MAX_CODEX_MODEL_CATALOG_BYTES,
        catalog_etag, parse_codex_model_catalog,
    },
    diagnostics::CodexUpstreamSendPhase,
    endpoints::{CODEX_RESPONSES_PATH, endpoint_url},
    headers::websocket_header_pairs,
    profile::CodexWireProfileState,
    protocol::{
        responses::{
            CodexResponsesRequest, ResponsesSseFailure, TransportRequirement, transport_requirement,
        },
        websocket::{
            websocket_audit_artifact_from_attempt, websocket_connection_limit_failure,
            websocket_payload_audit_snapshot,
        },
    },
    response_meta,
    websocket::{
        CodexWebSocketConnection, CodexWebSocketExchangeError, CodexWebSocketPool,
        CodexWebSocketPoolKey, CodexWebSocketStreamingExchange, CodexWebSocketTurnStateUpdateSlot,
        DEFAULT_FAST_PATH_BUDGET_MS, DEFAULT_STREAM_IDLE_TIMEOUT, WebSocketFastPath,
        WebSocketOriginBreaker, execute_prepared_response_create_request_stream,
        post_send_ambiguous, prepare_response_create_request_with_pool, websocket_audit_dir,
        write_websocket_audit_artifact_from_env,
    },
};

use super::client::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodexTurnStateCaptureError {
    #[error("turn state capture request is invalid")]
    InvalidRequest,
    #[error("turn state capture transport failed")]
    Transport,
    #[error("turn state capture upstream rejected the probe")]
    Upstream,
}

impl CodexBackendClient {
    pub(crate) const fn profile_state(&self) -> &CodexWireProfileState {
        &self.profile
    }

    /// 请求只持有自己的画像副本；连接池和 HTTP client 继续共享既有资源。
    pub fn with_request_profile(mut self, profile: super::profile::CodexWireProfile) -> Self {
        self.profile = CodexWireProfileState::new(profile);
        self
    }

    /// 构造客户端。
    pub fn new(
        client: Client,
        base_url: impl Into<String>,
        profile: CodexWireProfileState,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            direct_client: client.clone(),
            client,
            websocket_origin_key: websocket_origin_key(&base_url),
            outbound_proxy: None,
            egress_key: String::new(),
            base_url,
            official_base_url: crate::OFFICIAL_CODEX_BASE_URL.to_owned(),
            protocol: OpenAiUpstreamProtocol::Codex,
            profile,
            websocket_pool: None,
            websocket_origin_breaker: WebSocketOriginBreaker::default(),
            turn_state_store: None,
        }
    }

    /// 为 Responses WebSocket 请求启用连接池。
    pub fn with_websocket_pool(mut self, pool: Arc<CodexWebSocketPool>) -> Self {
        self.websocket_pool = Some(pool);
        self
    }

    pub(crate) fn with_turn_state_store(mut self, store: Option<Arc<dyn TurnStateStore>>) -> Self {
        self.turn_state_store = store;
        self
    }

    /// 驱逐指定账号的 Responses WebSocket 池连接。
    pub async fn evict_websocket_account(&self, account_id: &str) {
        if let Some(pool) = &self.websocket_pool {
            pool.evict_account(account_id).await;
        }
    }

    /// 发送 Responses SSE 请求并返回 live SSE 流（HTTP SSE fallback）。
    pub(crate) async fn create_response_stream_http_sse(
        &self,
        upstream_request: &CodexResponsesRequest,
        context: CodexRequestContext<'_>,
        provider_account_id: Option<&str>,
    ) -> CodexClientResult<CodexBackendStreamingResponse> {
        let headers = self.request_headers_for_http_response(upstream_request, context)?;
        let headers_started_at = Instant::now();
        // OAuth 请求遵循 Codex 压缩合同；API Key 上游使用普通 JSON。
        // Codex 上游只交付 SSE；即使下游请求 `stream: false`，也要上游流式执行，
        // 再由 API 层收集 canonical events 并返回完整 JSON。不能把下游的传输偏好
        // 直接透传给 Codex，否则上游会以 400 拒绝非流式请求。
        let mut upstream_body = upstream_request.body().clone();
        upstream_body.insert("stream".to_owned(), serde_json::Value::Bool(true));
        let body =
            serde_json::to_vec(&upstream_body).map_err(CodexClientError::RequestBodyEncode)?;
        let endpoint = endpoint_url(&self.base_url, self.protocol.responses_path());
        let trace = context
            .trace
            .cloned()
            .unwrap_or_default()
            .exchange("http_sse");
        trace.headers(
            "upstream.request.headers",
            serde_json::json!({
                "method": "POST", "endpoint": CODEX_RESPONSES_PATH,
            }),
            headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes())),
        );
        trace.capture("upstream.request.body", &body);
        let body = zstd::stream::encode_all(std::io::Cursor::new(body), 3)
            .map_err(CodexClientError::RequestCompression)?;
        let sent_turn_state = headers
            .get_all("x-codex-turn-state")
            .iter()
            .next_back()
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let sent_at = chrono::Utc::now();
        let response = self
            .client
            .post(endpoint)
            .headers(headers)
            .header(CONTENT_ENCODING, HeaderValue::from_static("zstd"))
            .body(body)
            .send()
            .await?;
        if let (Some(store), Some(account_id), Some(attempt_index), Some(send), Some(value)) = (
            self.turn_state_store.as_ref(),
            provider_account_id,
            context.attempt_index,
            context.turn_state_send,
            sent_turn_state,
        ) {
            store.enqueue_sent(TurnStateSent {
                request_id: context.request_id.to_owned(),
                attempt_index,
                account_id: account_id.to_owned(),
                identity_revision: send.identity_revision,
                effective_model: send.effective_model.to_owned(),
                value,
                sent_at,
                transport: "http".to_owned(),
                source: send.source.to_owned(),
                generation: send.generation,
                candidate_id: send.candidate_id.map(str::to_owned),
            });
        }
        let upstream_headers_ms = elapsed_duration_millis(headers_started_at.elapsed());
        let http_version = http_version_name(response.version()).to_string();
        let status = response.status();
        trace.headers(
            "upstream.response.headers",
            serde_json::json!({
                "status": status.as_u16(), "httpVersion": http_version,
                "headersMs": upstream_headers_ms,
            }),
            response
                .headers()
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes())),
        );
        let diagnostics = response_meta::diagnostics(Some(status.as_u16()), response.headers());
        let turn_state = response_meta::turn_state(response.headers());
        let turn_state_observation = turn_state.clone().map(CodexObservedTurnState::new);
        let turn_state_update = Arc::new(CodexWebSocketTurnStateUpdateSlot::new());
        let turn_state_observer = Some(HttpSseTurnStateObserver {
            store: self.turn_state_store.clone(),
            account_id: provider_account_id.map(str::to_owned),
            request_id: context.attempt_index.map(|_| context.request_id.to_owned()),
            attempt_index: context.attempt_index,
            client_turn_id: context.turn_id.map(str::to_owned),
            model_scope: context.model_turn_state_observation_scope.cloned(),
            turn_state_update: Arc::clone(&turn_state_update),
        });
        if let (Some(observer), Some(receipt)) = (
            turn_state_observer.as_ref(),
            turn_state_observation.as_ref(),
        ) {
            observer.observe(receipt.clone());
        }
        let set_cookie_headers = response_meta::set_cookie_headers(response.headers());
        let rate_limit_headers = response_meta::rate_limit_headers(response.headers());
        let response_metadata = response_meta::response_metadata(response.headers());
        let retry_after_seconds = retry_after_seconds(response.headers(), None);

        if !status.is_success() {
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .map(|value| value.as_bytes().to_vec());
            let client_headers = response_meta::client_headers(response.headers());
            let raw_body = read_error_response_body(response).await.map_err(|source| {
                CodexClientError::ErrorBodyRead {
                    source,
                    status,
                    diagnostics: Box::new(diagnostics.clone()),
                    transport: CodexBackendTransport::HttpSse,
                    transport_metrics: Box::new(CodexTransportMetrics {
                        upstream_headers_ms: Some(upstream_headers_ms),
                        http_version: Some(http_version.clone()),
                        ..CodexTransportMetrics::default()
                    }),
                }
            })?;
            trace.capture("upstream.error.body", &raw_body);
            let body = String::from_utf8_lossy(&raw_body).into_owned();
            let retry_after_seconds =
                retry_after_seconds.or_else(|| retry_after_seconds_from_body(&body));
            return Err(CodexClientError::Upstream {
                status,
                body,
                client_response: Some(Box::new(CodexClientVisibleUpstreamResponse::new(
                    status,
                    content_type,
                    client_headers,
                    raw_body,
                    turn_state_observation.clone(),
                ))),
                retry_after_seconds,
                diagnostics: Box::new(diagnostics),
                set_cookie_headers,
                rate_limit_headers,
                transport: CodexBackendTransport::HttpSse,
                transport_metrics: Box::new(CodexTransportMetrics {
                    upstream_headers_ms: Some(upstream_headers_ms),
                    http_version: Some(http_version),
                    ..CodexTransportMetrics::default()
                }),
                send_phase: CodexUpstreamSendPhase::AfterPayload,
            });
        }

        let is_json_response = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
        let rate_limit_updates = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (body, cyber_policy_refusal) = if is_json_response {
            let raw_body = read_error_response_body(response).await.map_err(|source| {
                CodexClientError::ErrorBodyRead {
                    source,
                    status,
                    diagnostics: Box::new(diagnostics.clone()),
                    transport: CodexBackendTransport::HttpSse,
                    transport_metrics: Box::new(CodexTransportMetrics {
                        upstream_headers_ms: Some(upstream_headers_ms),
                        http_version: Some(http_version.clone()),
                        ..CodexTransportMetrics::default()
                    }),
                }
            })?;
            trace.capture("upstream.response.body", &raw_body);
            let cyber_policy_refusal = gateway_protocol::openai::is_cyber_policy_refusal_json(
                &String::from_utf8_lossy(&raw_body),
            );
            (
                buffered_http_sse_stream(
                    raw_body,
                    Arc::clone(&rate_limit_updates),
                    turn_state_observer.clone(),
                    trace,
                ),
                cyber_policy_refusal,
            )
        } else {
            (
                http_sse_stream(
                    response,
                    Arc::clone(&rate_limit_updates),
                    turn_state_observer,
                    trace,
                ),
                false,
            )
        };
        Ok(CodexBackendStreamingResponse {
            body,
            transport: CodexBackendTransport::HttpSse,
            websocket_connection_id: None,
            turn_state,
            turn_state_observation,
            set_cookie_headers,
            rate_limit_headers,
            rate_limit_updates: Some(rate_limit_updates),
            turn_state_update: Some(turn_state_update),
            turn_state_observations: None,
            websocket_pool_decision: None,
            diagnostics,
            response_metadata,
            transport_metrics: CodexTransportMetrics {
                upstream_headers_ms: Some(upstream_headers_ms),
                http_version: Some(http_version),
                ..CodexTransportMetrics::default()
            },
            connection_local_continuation: false,
            cyber_policy_refusal,
        })
    }

    /// 发送固定诊断探针；首个 header/event 候选到达后立即释放响应流。
    #[doc(hidden)]
    pub async fn capture_turn_state_http_sse(
        &self,
        upstream_request: &CodexResponsesRequest,
        context: CodexRequestContext<'_>,
    ) -> Result<Option<String>, CodexTurnStateCaptureError> {
        let headers = self
            .request_headers_for_http_response(upstream_request, context)
            .map_err(|_| CodexTurnStateCaptureError::InvalidRequest)?;
        let mut upstream_body = upstream_request.body().clone();
        upstream_body.insert("stream".to_owned(), serde_json::Value::Bool(true));
        let body = serde_json::to_vec(&upstream_body)
            .map_err(|_| CodexTurnStateCaptureError::InvalidRequest)?;
        let body = zstd::stream::encode_all(std::io::Cursor::new(body), 3)
            .map_err(|_| CodexTurnStateCaptureError::InvalidRequest)?;
        let response = self
            .client
            .post(endpoint_url(&self.base_url, CODEX_RESPONSES_PATH))
            .headers(headers)
            .header(CONTENT_ENCODING, HeaderValue::from_static("zstd"))
            .body(body)
            .send()
            .await
            .map_err(|_| CodexTurnStateCaptureError::Transport)?;
        if !response.status().is_success() {
            return Err(CodexTurnStateCaptureError::Upstream);
        }
        if let Some(value) = response_meta::turn_state(response.headers()) {
            return Ok(Some(value));
        }
        let mut decoder = SseEventDecoder::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| CodexTurnStateCaptureError::Transport)?;
            for frame in decoder.push_frames(&chunk) {
                if let Some(value) = turn_state_from_sse_frame(&frame) {
                    return Ok(Some(value));
                }
            }
        }
        for frame in decoder.finish_frames() {
            if let Some(value) = turn_state_from_sse_frame(&frame) {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    pub async fn create_response_stream_with_pool_account(
        &self,
        request: &CodexResponsesRequest,
        context: CodexRequestContext<'_>,
        pool_account_id: Option<&str>,
    ) -> CodexClientResult<CodexBackendStreamingResponse> {
        let prepared = self
            .prepare_response_transport_with_pool_account(request, context, pool_account_id)
            .await?;
        self.create_response_stream_with_prepared(request, context, prepared)
            .await
    }

    /// 在发送 payload 前完成 transport 选择和可取消的 WebSocket opening。
    #[doc(hidden)]
    pub(crate) async fn prepare_response_transport_with_pool_account(
        &self,
        request: &CodexResponsesRequest,
        context: CodexRequestContext<'_>,
        pool_account_id: Option<&str>,
    ) -> CodexClientResult<PreparedResponseTransport> {
        let requirement = transport_requirement(request);
        context.trace.cloned().unwrap_or_default().record(
            "transport.preparing",
            serde_json::json!({
                "requirement": requirement.as_str(),
            }),
        );
        if requirement == TransportRequirement::HttpRequired {
            return Ok(PreparedResponseTransport {
                requirement,
                route: PreparedResponseRoute::Http,
                provider_account_id: pool_account_id.map(str::to_owned),
                metrics: CodexTransportMetrics {
                    decision: Some(CodexTransportDecision::HttpRequired),
                    ..CodexTransportMetrics::default()
                },
            });
        }

        let websocket_request = websocket_upstream_request(request);
        let headers = self.request_headers_for_websocket_response(&websocket_request, context)?;
        let mut websocket_create = CodexWebSocketConnection::responses_create_request_for_path(
            &self.base_url,
            self.protocol.responses_path(),
            &generate_key(),
            websocket_header_pairs(&headers),
            &websocket_request,
        )
        .map_err(CodexClientError::WebSocketEncode)?;
        websocket_create.connection.outbound_proxy = self.outbound_proxy.clone();
        context.trace.cloned().unwrap_or_default().headers(
            "upstream.request.headers",
            serde_json::json!({"transport": "websocket", "phase": "prepared_opening"}),
            websocket_create
                .connection()
                .headers()
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes())),
        );
        // 审计未启用时跳过 artifact 构造：payload 快照会深拷贝整个请求 body，
        // 且位于首字节前的关键路径上。
        if websocket_audit_dir().is_some() {
            let artifact = websocket_audit_artifact_from_attempt(
                &websocket_request,
                websocket_create.connection().opening_audit_snapshot(),
                websocket_payload_audit_snapshot(&websocket_request),
            );
            if let Err(error) = write_websocket_audit_artifact_from_env(&artifact).await {
                tracing::warn!(error = %error, "Failed to write Codex WebSocket audit artifact");
            }
        }
        let connection_profile = websocket_connection_profile(&headers);
        let pool_key =
            self.websocket_pool_key(request, context, pool_account_id, &connection_profile);
        let pool_log_context = pool_key.as_ref().map(WebSocketPoolLogContext::from_key);
        let pool = self.websocket_pool.as_deref().zip(pool_key);
        // 快路径预算与流空闲超时都是 per-request 语义：按请求的当前运行参数读取，
        // 设置换入后对新请求立即生效，进行中的请求保持原冻结值。
        let pool_config = self
            .websocket_pool
            .as_deref()
            .map(CodexWebSocketPool::config_snapshot);
        let fast_path_budget = match requirement {
            TransportRequirement::PersistedContinuation | TransportRequirement::NewChain => {
                Some(pool_config.map_or(
                    Duration::from_millis(DEFAULT_FAST_PATH_BUDGET_MS),
                    |config| config.fast_path_budget,
                ))
            }
            TransportRequirement::ExplicitWebSocketWarmup
            | TransportRequirement::WebSocketNewChain
            | TransportRequirement::ExactWebSocketContinuation
            | TransportRequirement::ExternalUnknown => None,
            TransportRequirement::HttpRequired => None,
        };
        let stream_idle_timeout = pool_config
            .and_then(|config| config.stream_idle_timeout)
            .or(Some(DEFAULT_STREAM_IDLE_TIMEOUT));
        let prepare_started_at = Instant::now();
        let prepared = prepare_response_create_request_with_pool(
            &websocket_create,
            pool,
            &self.websocket_origin_breaker,
            &self.websocket_origin_key,
            fast_path_budget,
            requirement.requires_websocket(),
            stream_idle_timeout,
        )
        .await;
        let prepared = match prepared {
            Ok(WebSocketFastPath::Ready(prepared)) => prepared,
            Ok(WebSocketFastPath::Missed) => {
                let decision = CodexTransportDecision::Http2WebSocketBudgetExhausted;
                let wait_ms = elapsed_duration_millis(prepare_started_at.elapsed());
                context.trace.cloned().unwrap_or_default().record(
                    "transport.fallback",
                    serde_json::json!({
                        "to": "http_sse", "reason": "websocket_fast_path_budget",
                        "requirement": requirement.as_str(), "decision": decision.as_str(),
                        "waitMs": wait_ms, "preconnectContinues": pool_log_context.is_some(),
                    }),
                );
                return Ok(PreparedResponseTransport {
                    requirement,
                    route: PreparedResponseRoute::Http,
                    provider_account_id: pool_account_id.map(str::to_owned),
                    metrics: CodexTransportMetrics {
                        decision: Some(decision),
                        ws_connect_ms: None,
                        transport_decision_wait_ms: Some(wait_ms),
                        ..CodexTransportMetrics::default()
                    },
                });
            }
            Err(error)
                if requirement.allows_pre_send_http_fallback()
                    && let Some(decision) = local_http_fallback_decision(&error) =>
            {
                let wait_ms = elapsed_duration_millis(prepare_started_at.elapsed());
                context.trace.cloned().unwrap_or_default().record(
                    "transport.fallback", serde_json::json!({
                        "to": "http_sse", "reason": "websocket_pre_send_failure",
                        "requirement": requirement.as_str(), "decision": decision.as_str(),
                        "waitMs": wait_ms,
                        "detail": diagnostic_json(&serde_json::json!({"message": error.to_string()})),
                    }),
                );
                return Ok(PreparedResponseTransport {
                    requirement,
                    route: PreparedResponseRoute::Http,
                    provider_account_id: pool_account_id.map(str::to_owned),
                    metrics: CodexTransportMetrics {
                        decision: Some(decision),
                        ws_connect_ms: None,
                        transport_decision_wait_ms: Some(wait_ms),
                        ..CodexTransportMetrics::default()
                    },
                });
            }
            Err(error) => return Err(websocket_exchange_error_to_client_error(error)),
        };
        let decision = websocket_success_decision(requirement, &prepared);
        context.trace.cloned().unwrap_or_default().record(
            "transport.prepared",
            serde_json::json!({
                "decision": decision.as_str(),
                "requirement": requirement.as_str(),
                "connectMs": prepared.connect_elapsed().map(elapsed_duration_millis),
                "waitMs": elapsed_duration_millis(prepared.decision_wait_elapsed()),
            }),
        );
        let metrics = CodexTransportMetrics {
            decision: Some(decision),
            ws_connect_ms: prepared.connect_elapsed().map(elapsed_duration_millis),
            transport_decision_wait_ms: Some(elapsed_duration_millis(
                prepared.decision_wait_elapsed(),
            )),
            upstream_headers_ms: prepared.connect_elapsed().map(elapsed_duration_millis),
            first_event_ms: None,
            http_version: Some("HTTP/1.1".to_string()),
        };
        log_websocket_pool_decision(
            context,
            pool_account_id,
            pool_log_context.as_ref(),
            prepared.pool_decision(),
        );
        Ok(PreparedResponseTransport {
            requirement,
            route: PreparedResponseRoute::WebSocket(Box::new(PreparedWebSocketRoute {
                request: websocket_create,
                prepared,
            })),
            provider_account_id: pool_account_id.map(str::to_owned),
            metrics,
        })
    }

    #[doc(hidden)]
    pub(crate) async fn create_response_stream_with_prepared(
        &self,
        request: &CodexResponsesRequest,
        context: CodexRequestContext<'_>,
        prepared: PreparedResponseTransport,
    ) -> CodexClientResult<CodexBackendStreamingResponse> {
        let PreparedResponseTransport {
            requirement,
            route,
            metrics,
            provider_account_id,
        } = prepared;
        context.trace.cloned().unwrap_or_default().record(
            "transport.selected",
            serde_json::json!({
                "decision": metrics.decision.map(|decision| decision.as_str()),
                "requirement": requirement.as_str(), "waitMs": metrics.transport_decision_wait_ms,
            }),
        );
        match route {
            PreparedResponseRoute::Http => self
                .create_response_stream_http_sse(request, context, provider_account_id.as_deref())
                .await
                .map(|mut response| {
                    merge_preparation_metrics(&mut response.transport_metrics, metrics);
                    response
                }),
            PreparedResponseRoute::WebSocket(route) => {
                let PreparedWebSocketRoute {
                    request: websocket_request,
                    prepared,
                } = *route;
                let turn_state_observer = self
                    .turn_state_store
                    .as_ref()
                    .zip(provider_account_id.as_deref())
                    .zip(context.attempt_index)
                    .map(|((store, account_id), attempt_index)| {
                        super::websocket::CodexWebSocketTurnStateObserver::new(
                            Arc::clone(store),
                            account_id,
                            context.request_id,
                            attempt_index,
                            context.turn_id,
                            context.model_turn_state_observation_scope,
                            context.turn_state_send,
                        )
                    });
                let mut exchange = execute_prepared_response_create_request_stream(
                    &websocket_request,
                    prepared,
                    context
                        .trace
                        .cloned()
                        .unwrap_or_default()
                        .exchange("websocket"),
                    turn_state_observer,
                )
                .await
                .map_err(websocket_exchange_error_to_client_error)?;
                if requirement.allows_connection_restart() {
                    match await_websocket_delivery_boundary(&mut exchange).await {
                        Ok(DeliveryBoundary::Ready) => {}
                        Ok(DeliveryBoundary::ConnectionLimitReached(failure)) => {
                            return Err(CodexClientError::WebSocket(
                                CodexWebSocketExchangeError::ConnectionLimitReached(failure),
                            ));
                        }
                        Err(error) => {
                            return Err(websocket_exchange_error_to_client_error(
                                post_send_ambiguous(error),
                            ));
                        }
                    }
                }
                Ok(CodexBackendStreamingResponse {
                    body: Box::pin(
                        exchange
                            .body
                            .map_err(post_send_ambiguous)
                            .map_err(websocket_exchange_error_to_client_error),
                    ),
                    transport: CodexBackendTransport::WebSocket,
                    websocket_connection_id: Some(exchange.websocket_connection_id),
                    turn_state: exchange.turn_state,
                    turn_state_observation: exchange.turn_state_observation,
                    set_cookie_headers: exchange.set_cookie_headers,
                    rate_limit_headers: exchange.rate_limit_headers,
                    rate_limit_updates: Some(exchange.rate_limit_updates),
                    turn_state_update: Some(exchange.turn_state_update),
                    turn_state_observations: Some(exchange.turn_state_observations),
                    websocket_pool_decision: exchange.pool_decision,
                    diagnostics: exchange.diagnostics,
                    response_metadata: exchange.response_metadata,
                    transport_metrics: metrics,
                    connection_local_continuation: exchange.connection_local_continuation,
                    cyber_policy_refusal: false,
                })
            }
        }
    }

    fn websocket_pool_key(
        &self,
        request: &CodexResponsesRequest,
        context: CodexRequestContext<'_>,
        pool_account_id: Option<&str>,
        connection_profile: &str,
    ) -> Option<CodexWebSocketPoolKey> {
        let account_id = pool_account_id.or(context.account_id)?;
        let conversation_id = request
            .local_conversation_id
            .as_deref()
            .or(request.previous_response_id())?;
        let mut key = CodexWebSocketPoolKey::new(&self.base_url, account_id, conversation_id)
            .with_egress_key(&self.egress_key)
            .with_connection_profile(connection_profile);
        if let Some(connection_id) = request.downstream_websocket_connection_id.as_deref() {
            key = key.with_downstream_connection_id(connection_id);
        }
        Some(key)
    }

    /// 客户端目录按调用方版本协商；后台目录仍使用经过核验的服务端画像版本。
    pub async fn fetch_models_with_context(
        &self,
        context: CodexRequestContext<'_>,
        client_version: Option<&str>,
    ) -> CodexClientResult<CodexModelCatalogSnapshot> {
        let path = match self.protocol {
            OpenAiUpstreamProtocol::Codex => "codex/models",
            OpenAiUpstreamProtocol::ResponsesApi => "models",
        };
        let profile = self.profile.snapshot();
        let headers = self.model_request_headers(&profile, context)?;
        let mut request = self
            .client
            .get(endpoint_url(&self.base_url, path))
            .headers(headers);
        if self.protocol == OpenAiUpstreamProtocol::Codex {
            request = request.query(&[(
                "client_version",
                client_version.unwrap_or(profile.codex_version.as_str()),
            )]);
        }
        let response = request.send().await?;
        let status = response.status();
        let diagnostics = response_meta::diagnostics(Some(status.as_u16()), response.headers());
        let set_cookie_headers = response_meta::set_cookie_headers(response.headers());
        let retry_after_seconds = retry_after_seconds(response.headers(), None);
        let etag = status
            .is_success()
            .then(|| catalog_etag(response.headers()))
            .transpose()?
            .flatten();
        let body = read_model_catalog_body(response).await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&body).into_owned();
            return Err(CodexClientError::Upstream {
                status,
                retry_after_seconds: retry_after_seconds
                    .or_else(|| retry_after_seconds_from_body(&body)),
                body,
                client_response: None,
                diagnostics: Box::new(diagnostics),
                set_cookie_headers,
                rate_limit_headers: Vec::new(),
                transport: CodexBackendTransport::HttpSse,
                transport_metrics: Box::default(),
                send_phase: CodexUpstreamSendPhase::AfterPayload,
            });
        }
        Ok(match self.protocol {
            OpenAiUpstreamProtocol::Codex => parse_codex_model_catalog(&body, etag.as_deref())?,
            OpenAiUpstreamProtocol::ResponsesApi => {
                super::catalog::parse_api_model_catalog(&body, etag.as_deref())?
            }
        })
    }
}

fn turn_state_from_sse_frame(frame: &SseFrame) -> Option<String> {
    frame.events().iter().find_map(|event| {
        if !event.data.contains("turn_state")
            && !event.data.contains("turnState")
            && !event.data.contains("x-codex-turn-state")
        {
            return None;
        }
        let value = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
        [
            "/x-codex-turn-state",
            "/turn_state",
            "/turnState",
            "/headers/x-codex-turn-state",
            "/metadata/x-codex-turn-state",
            "/response/headers/x-codex-turn-state",
            "/response/metadata/x-codex-turn-state",
        ]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(|value| value.as_str()))
        .map(str::to_owned)
    })
}

#[derive(Clone)]
struct HttpSseTurnStateObserver {
    store: Option<Arc<dyn TurnStateStore>>,
    account_id: Option<String>,
    request_id: Option<String>,
    attempt_index: Option<u32>,
    client_turn_id: Option<String>,
    model_scope: Option<ModelTurnStateObservationScope>,
    turn_state_update: CodexTurnStateUpdate,
}

impl HttpSseTurnStateObserver {
    fn observe(&self, receipt: CodexObservedTurnState) {
        let (Some(store), Some(account_id)) = (self.store.as_ref(), self.account_id.as_ref())
        else {
            return;
        };
        store.enqueue_observation(TurnStateObservation {
            id: receipt.id,
            account_id: account_id.clone(),
            request_id: self.request_id.clone(),
            attempt_index: self.attempt_index,
            value: receipt.value,
            observed_at: receipt.observed_at,
            transport: "http".to_owned(),
            upstream_response_id: None,
            client_turn_id: self.client_turn_id.clone(),
            model_scope: self.model_scope.clone(),
        });
    }

    fn observe_value(&self, value: String) {
        self.observe(CodexObservedTurnState::new(value.clone()));
        self.turn_state_update.publish(value);
    }
}

/// 首个可投递帧前的交付边界结果。
enum DeliveryBoundary {
    /// 已越过边界，可开始向下游投递。
    Ready,
    /// 首个可投递帧是上游连接寿命限制错误。
    ConnectionLimitReached(Box<ResponsesSseFailure>),
}

async fn await_websocket_delivery_boundary(
    exchange: &mut CodexWebSocketStreamingExchange,
) -> Result<DeliveryBoundary, CodexWebSocketExchangeError> {
    let mut prelude = Vec::new();
    let turn_state_update = Arc::clone(&exchange.turn_state_update);
    loop {
        let next = tokio::select! {
            biased;
            next = exchange.body.next() => next,
            () = turn_state_update.wait_until_pending() => {
                let remaining =
                    std::mem::replace(&mut exchange.body, Box::pin(futures::stream::empty()));
                exchange.body =
                    Box::pin(futures::stream::iter(prelude.into_iter().map(Ok)).chain(remaining));
                return Ok(DeliveryBoundary::Ready);
            },
        };
        match next {
            Some(Ok(frame)) if is_websocket_lifecycle_prelude(&frame) => prelude.push(frame),
            Some(Ok(frame)) => {
                let connection_limit_failure = websocket_connection_limit_failure(&frame);
                prelude.push(frame);
                let remaining =
                    std::mem::replace(&mut exchange.body, Box::pin(futures::stream::empty()));
                exchange.body =
                    Box::pin(futures::stream::iter(prelude.into_iter().map(Ok)).chain(remaining));
                return Ok(if let Some(failure) = connection_limit_failure {
                    DeliveryBoundary::ConnectionLimitReached(Box::new(failure))
                } else {
                    DeliveryBoundary::Ready
                });
            }
            Some(Err(error)) => return Err(error),
            None => {
                return Err(CodexWebSocketExchangeError::closed_before_terminal_on(
                    exchange.websocket_connection_id,
                    None,
                    None,
                    None,
                ));
            }
        }
    }
}

fn is_websocket_lifecycle_prelude(frame: &[u8]) -> bool {
    frame.starts_with(b"event: response.created\n")
        || frame.starts_with(b"event: response.in_progress\n")
}

async fn read_model_catalog_body(response: ReqwestResponse) -> CodexClientResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CODEX_MODEL_CATALOG_BYTES as u64)
    {
        return Err(CodexModelCatalogError::ResponseTooLarge.into());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let Some(next_len) = body.len().checked_add(chunk.len()) else {
            return Err(CodexModelCatalogError::ResponseTooLarge.into());
        };
        if next_len > MAX_CODEX_MODEL_CATALOG_BYTES {
            return Err(CodexModelCatalogError::ResponseTooLarge.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn websocket_connection_profile(headers: &HeaderMap) -> String {
    ["originator", "user-agent", X_OPENAI_MEMGEN_REQUEST_HEADER]
        .map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        })
        .join("\0")
}

fn http_sse_stream(
    response: ReqwestResponse,
    rate_limit_updates: CodexRateLimitUpdates,
    turn_state_observer: Option<HttpSseTurnStateObserver>,
    trace: TraceContext,
) -> CodexBackendSseStream {
    let stream: CodexBackendSseStream =
        Box::pin(response.bytes_stream().map_err(CodexClientError::Http));
    let stream: CodexBackendSseStream =
        Box::pin(futures::stream::unfold(Some(stream), |stream| async move {
            let mut stream = stream?;
            match tokio::time::timeout(UPSTREAM_STREAM_IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(chunk)) => Some((chunk, Some(stream))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(CodexClientError::StreamIdleTimeout {
                        timeout: UPSTREAM_STREAM_IDLE_TIMEOUT,
                    }),
                    None,
                )),
            }
        }));
    let stream = Box::pin(async_stream::stream! {
        let mut stream = stream;
        let mut capture = StreamCapture::new(trace.clone(), StreamFormat::Sse);
        while let Some(chunk) = stream.next().await {
            match &chunk {
                Ok(bytes) => capture.push(bytes),
                Err(error) => trace.record("upstream.read.failed", diagnostic_json(&serde_json::json!({"error": error.to_string()}))),
            }
            let failed = chunk.is_err();
            yield chunk;
            if failed { return; }
        }
        capture.finish();
    });
    observe_http_sse_updates(stream, rate_limit_updates, turn_state_observer)
}

fn buffered_http_sse_stream(
    body: bytes::Bytes,
    rate_limit_updates: CodexRateLimitUpdates,
    turn_state_observer: Option<HttpSseTurnStateObserver>,
    trace: TraceContext,
) -> CodexBackendSseStream {
    let stream = Box::pin(futures::stream::once(async move { Ok(body) }));
    let stream = Box::pin(async_stream::stream! {
        let mut stream = stream;
        let mut capture = StreamCapture::new(trace, StreamFormat::Sse);
        while let Some(chunk) = stream.next().await {
            if let Ok(bytes) = &chunk {
                capture.push(bytes);
            }
            yield chunk;
        }
        capture.finish();
    });
    observe_http_sse_updates(stream, rate_limit_updates, turn_state_observer)
}

fn observe_http_sse_updates(
    stream: CodexBackendSseStream,
    rate_limit_updates: CodexRateLimitUpdates,
    turn_state_observer: Option<HttpSseTurnStateObserver>,
) -> CodexBackendSseStream {
    Box::pin(futures::stream::unfold(
        (
            stream,
            SseEventDecoder::default(),
            rate_limit_updates,
            turn_state_observer,
        ),
        |(mut stream, mut decoder, rate_limit_updates, turn_state_observer)| async move {
            match stream.next().await {
                Some(chunk) => {
                    if let Ok(bytes) = &chunk {
                        append_http_sse_updates(
                            decoder.push_frames(bytes),
                            &rate_limit_updates,
                            turn_state_observer.as_ref(),
                        )
                        .await;
                    }
                    Some((
                        chunk,
                        (stream, decoder, rate_limit_updates, turn_state_observer),
                    ))
                }
                None => {
                    append_http_sse_updates(
                        decoder.finish_frames(),
                        &rate_limit_updates,
                        turn_state_observer.as_ref(),
                    )
                    .await;
                    None
                }
            }
        },
    ))
}

async fn append_http_sse_updates(
    frames: Vec<SseFrame>,
    rate_limit_updates: &CodexRateLimitUpdates,
    turn_state_observer: Option<&HttpSseTurnStateObserver>,
) {
    let mut observations = Vec::new();
    for frame in frames {
        if let Some(value) = turn_state_from_sse_frame(&frame)
            && let Some(observer) = turn_state_observer
        {
            observer.observe_value(value);
        }
        for event in frame.events() {
            if event
                .event
                .as_deref()
                .is_some_and(|event| event != "codex.rate_limits")
            {
                continue;
            }
            let Some(rate_limits) = events::parse_rate_limits_event_raw(&event.data) else {
                continue;
            };
            observations.push(rate_limits);
        }
    }
    if !observations.is_empty() {
        rate_limit_updates.lock().await.extend(observations);
    }
}
