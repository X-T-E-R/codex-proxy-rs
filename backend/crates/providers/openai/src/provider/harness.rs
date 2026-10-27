//! 已选号的 OpenCode 与 pi 请求执行及原生 Responses 事件交付

use base64::Engine as _;
use gateway_core::account::AccountHistorySnapshot;
use gateway_core::engine::provider::{
    EventStream, ProviderCallMetadata, ProviderSelectionObservation, ProviderStream,
};
use gateway_core::error::{ProviderError, ProviderErrorKind};
use gateway_core::event::GatewayEvent;
use gateway_core::operation::GenerateRequest;
use gateway_core::routing::{ProviderKind, UpstreamModelId};
use gateway_core::upstream::{UpstreamSendState, UpstreamTransport};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

use super::{
    AttemptContext, CodexAccountFailure, CodexCanonicalDecoder, CodexCanonicalOutcome,
    CodexCredentialLease, CodexProvider, CodexUpstreamDiagnostics, ContinuationBinding,
    MiddlewareHeader, OPENCODE_RESPONSE_ID_PREFIX, OpenAiContinuationScope, OpenAiSessionCapture,
    ReplayBoundary, attach_openai_session_update, continuation_replay_required_error,
    map_canonical_error, provider_error,
};
use crate::credential::harness_oauth::{HarnessMethod, method_for_kind};
use crate::credential::{CodexCredentialSelector, opencode_fast_base};
use crate::harness::{
    HarnessError, HarnessSession, HarnessWorkerConfig, ResponseItem, ResponsesCall,
    ResponsesHarness, ResponsesRequestIdentity,
};
use crate::request_identity::{account_session_with_headers, account_thread_with_headers};

const RESPONSES_BASE_URL: &str = "https://api.openai.com/v1";

impl CodexProvider {
    pub(super) fn execute_harness_generate(
        self: Arc<Self>,
        generate: GenerateRequest,
        headers: Vec<MiddlewareHeader>,
        model: UpstreamModelId,
        context: AttemptContext,
        lease: CodexCredentialLease,
        account_selection_wait_ms: u64,
    ) -> Result<ProviderStream, ProviderError> {
        let method = method_for_kind(lease.account().authentication_kind()).ok_or_else(|| {
            provider_error(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::NotSent,
            )
        })?;
        let paths = self
            .harness_runtime
            .as_ref()
            .map_err(|error| {
                tracing::error!(error = %error, "OpenAI Responses harness runtime unavailable");
                provider_error(
                    ProviderErrorKind::ProviderInfrastructureUnavailable,
                    UpstreamSendState::NotSent,
                )
                .with_retry_prohibited()
            })?
            .clone();
        let bearer = lease
            .authentication()
            .oauth()
            .ok_or_else(|| {
                provider_error(ProviderErrorKind::Unauthorized, UpstreamSendState::NotSent)
            })?
            .access_token
            .clone();
        let raw_session = account_session_with_headers(
            generate.protocol_payload().body(),
            generate.protocol_payload().context(),
            &headers,
        );
        let raw_thread = account_thread_with_headers(
            generate.protocol_payload().body(),
            generate.protocol_payload().context(),
            &headers,
        );
        let session_id = raw_thread
            .or_else(|| raw_session.clone())
            .unwrap_or_else(|| context.request_id().as_str().to_owned());
        let identity = ResponsesRequestIdentity {
            session_affinity_id: raw_session.filter(|parent| parent != &session_id),
        };
        let mut body = generate.protocol_payload().body().clone();
        if body.get("store").is_some_and(|value| !value.is_boolean())
            || method == HarnessMethod::Pi && body.get("store") == Some(&Value::Bool(true))
        {
            return Err(provider_error(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::NotSent,
            )
            .with_retry_prohibited()
            .with_diagnostic(super::ProviderDiagnostic::new(
                "fixed pi Responses does not support stored continuation",
            )));
        }
        let store_response =
            method == HarnessMethod::OpenCode && body.get("store") == Some(&Value::Bool(true));
        let native_owner =
            if let Some(public) = body.get("previous_response_id").and_then(Value::as_str) {
                let pinned = context.continuation().and_then(ContinuationBinding::pinned);
                let state = pinned
                    .and_then(|pin| pin.session_state())
                    .filter(|state| state.provider() == "openai")
                    .and_then(|state| {
                        serde_json::from_value::<super::OpenAiSessionState>(Value::Object(
                            state.payload().clone(),
                        ))
                        .ok()
                    });
                let Some((pin, _)) = pinned.zip(state).filter(|(pin, state)| {
                    method == HarnessMethod::OpenCode
                    && public.starts_with(OPENCODE_RESPONSE_ID_PREFIX)
                    && pin.previous_response_id().as_str() == public
                    && pin.scope()
                        == gateway_core::engine::continuation::NativeContinuationScope::Persisted
                    && pin.provider().as_str() == "openai"
                    && pin.matches_client(context.client_api_key_ref())
                    && pin.account() == lease.account_id()
                    && state.harness_kind.as_deref() == Some("opencode")
                    && state.account_id == lease.account_id().as_str()
                    && state.credential_revision == Some(lease.account().revision().get())
                    && state.continuation_scope == OpenAiContinuationScope::Persisted
                }) else {
                    return Err(continuation_replay_required_error("scope_unavailable"));
                };
                body.insert(
                    "previous_response_id".to_owned(),
                    Value::String(pin.upstream_response_id().as_str().to_owned()),
                );
                true
            } else {
                false
            };
        if has_account_bound_history(&body)
            && !native_owner
            && (!lease.affinity_hit() || lease.account_switch())
        {
            return Err(provider_error(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::NotSent,
            )
            .with_retry_prohibited()
            .with_diagnostic(super::ProviderDiagnostic::new(
                "account-bound Responses history cannot be replayed without its original account",
            )));
        }
        // 会话身份和子代理标记已经由权威解析参与选号，不能作为上游业务字段发送
        if let Some(metadata) = body.remove("client_metadata") {
            let recognized = metadata.as_object().is_some_and(|fields| {
                fields.keys().all(|key| {
                    matches!(
                        key.as_str(),
                        "session_id"
                            | "thread_id"
                            | "turn_id"
                            | "turnMetadata"
                            | "turn_metadata"
                            | "x-codex-turn-metadata"
                            | "x-openai-subagent"
                            | "x-codex-installation-id"
                            | "installation_id"
                            | "installationId"
                    )
                })
            });
            if !recognized {
                return Err(provider_error(
                    ProviderErrorKind::InvalidRequest,
                    UpstreamSendState::NotSent,
                )
                .with_retry_prohibited()
                .with_diagnostic(super::ProviderDiagnostic::new(
                    "unsupported client_metadata in fixed Responses harness",
                )));
            }
        }
        body.remove("session_id");
        body.remove("thread_id");
        if body
            .get("prompt_cache_key")
            .is_some_and(|key| key.as_str().is_none_or(|key| key.trim().is_empty()))
        {
            return Err(provider_error(
                ProviderErrorKind::InvalidRequest,
                UpstreamSendState::NotSent,
            )
            .with_retry_prohibited()
            .with_diagnostic(super::ProviderDiagnostic::new("invalid prompt_cache_key")));
        }
        let request_model = if method == HarnessMethod::OpenCode {
            opencode_fast_base(model.as_str()).unwrap_or(model.as_str())
        } else {
            model.as_str()
        };
        if request_model != model.as_str() {
            let tier = body
                .get("service_tier")
                .and_then(Value::as_str)
                .map(str::trim);
            let disabled = context.account_fast_mode(lease.account_id().as_str())
                == Some(gateway_core::account::FastMode::Disabled);
            let target = match tier {
                Some(tier)
                    if tier.eq_ignore_ascii_case("priority")
                        || tier.eq_ignore_ascii_case("fast") =>
                {
                    Some(if disabled { "default" } else { "priority" })
                }
                None if !disabled => Some("priority"),
                Some(tier) if !disabled && tier.eq_ignore_ascii_case("default") => Some("priority"),
                _ => None,
            };
            if let Some(target) = target {
                body.insert("service_tier".to_owned(), Value::String(target.to_owned()));
            }
        }
        body.insert("model".to_owned(), Value::String(request_model.to_owned()));
        // 当前 call 的账号映射在桥内执行，不能先调用 Codex 的正文账号归一
        let service_tier = body
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let request = ResponsesCall {
            harness: match method {
                HarnessMethod::OpenCode => ResponsesHarness::OpenCode,
                HarnessMethod::Pi => ResponsesHarness::Pi,
            },
            model: request_model.to_owned(),
            base_url: RESPONSES_BASE_URL.to_owned(),
            installation_id: lease.installation_id().to_owned(),
            session_id,
            request_id: context.request_id().as_str().to_owned(),
            service_tier: service_tier.clone(),
            identity,
            bearer,
            downstream_headers: Vec::new(),
            responses_body: Some(Value::Object(body.clone())),
            prompt: String::new(),
            instructions: None,
            max_output_tokens: None,
            temperature: None,
            body_window: 256 * 1024,
            event_window: 16,
            timeout: None,
        };
        #[cfg(debug_assertions)]
        let dial_loopback = self.harness_test_dial_loopback;
        #[cfg(not(debug_assertions))]
        let dial_loopback = None;
        let runtime = HarnessWorkerConfig {
            node: PathBuf::from("node"),
            worker_dir: paths.worker_dir,
            opencode_checkout: paths.opencode_checkout,
            pi_checkout: paths.pi_checkout,
            proxy: lease.account().outbound_proxy().cloned(),
            allow_remote_dial: dial_loopback.is_none(),
            dial_loopback,
        };
        let kind = ProviderKind::new("openai")
            .map_err(|_| provider_error(ProviderErrorKind::Protocol, UpstreamSendState::NotSent))?;
        let metadata = ProviderCallMetadata::new(
            kind,
            model.clone(),
            lease.account_id().clone(),
            UpstreamTransport::new("http_sse").map_err(|_| {
                provider_error(ProviderErrorKind::Protocol, UpstreamSendState::NotSent)
            })?,
        )
        .with_account_snapshot(Some(AccountHistorySnapshot::from_account(lease.account())))
        .with_selection_observation(ProviderSelectionObservation::new(
            account_selection_wait_ms,
            lease.capacity_snapshot(),
        ));
        let can_update_account = lease.allows_account_state_mutation();
        let capture = store_response.then(|| OpenAiSessionCapture {
            account_id: lease.account_id().as_str().to_owned(),
            credential_revision: Some(lease.account().revision().get()),
            conversation_id: None,
            turn_state: None,
            client_turn_id: None,
            response_store: true,
            continuation_scope: Some(OpenAiContinuationScope::Persisted),
            harness_kind: Some("opencode".to_owned()),
        });
        let lease = Arc::new(lease);
        let events = harness_response_stream(
            runtime,
            request,
            context,
            HarnessResponseModel {
                model,
                service_tier,
            },
            capture,
            Arc::clone(&self.selector),
            Arc::clone(&lease),
        );
        let stream = ProviderStream::new(metadata, events, lease);
        Ok(if can_update_account {
            stream.with_filtered_account_feedback(
                Arc::clone(&self.account_feedback),
                super::openai_failure_affects_account_score,
            )
        } else {
            stream
        })
    }
}

fn has_account_bound_history(body: &serde_json::Map<String, Value>) -> bool {
    // 只有已证明的同账号会话可重放不透明的 reasoning 和上游关联 ID
    // 无法证明归属时不能删除这些字段继续发送，那会破坏工具结果的配对
    fn bound(value: &Value) -> bool {
        match value {
            Value::Object(fields) => fields.iter().any(|(key, value)| {
                matches!(
                    key.as_str(),
                    "id" | "call_id"
                        | "encrypted_content"
                        | "thinkingSignature"
                        | "textSignature"
                        | "providerMetadata"
                ) && !value.is_null()
                    || bound(value)
            }),
            Value::Array(items) => items.iter().any(bound),
            _ => false,
        }
    }
    body.get("input").is_some_and(bound)
}

struct HarnessResponseModel {
    model: UpstreamModelId,
    service_tier: Option<String>,
}

fn harness_response_stream(
    runtime: HarnessWorkerConfig,
    request: ResponsesCall,
    context: AttemptContext,
    response_model: HarnessResponseModel,
    mut capture: Option<OpenAiSessionCapture>,
    selector: Arc<CodexCredentialSelector>,
    lease: Arc<CodexCredentialLease>,
) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let installation_id = request.installation_id.clone();
        let public_ids = capture.is_some();
        let mut response_ids = None;
        let session = HarnessSession::open(runtime).await.map_err(|error| {
            tracing::error!(error = %error, "OpenAI Responses harness could not start");
            harness_error(error, UpstreamSendState::NotSent)
        })?;
        let mut response = session.open_responses(request, context.cancellation()).await
            .map_err(|error| harness_error(error, UpstreamSendState::NotSent))?;
        let mut decoder = CodexCanonicalDecoder::new(response_model.model.as_str())
            .with_trace(context.trace())
            .with_pricing(context.pricing().get("openai")
                .and_then(|models| models.get(response_model.model.as_str())).cloned())
            .with_requested_service_tier(response_model.service_tier.as_deref())
            .with_raw_sse_passthrough();
        let mut sent = UpstreamSendState::NotSent;
        let mut status = None;
        let mut retry_after = None;
        let mut terminal_events = Vec::new();
        let mut completed = false;
        loop {
            let next = response.next().await;
            match next {
                Ok(ResponseItem::Request(_)) => sent = UpstreamSendState::Ambiguous,
                Ok(ResponseItem::Event(event)) if event.kind == "head" => {
                    status = event.payload.get("status").and_then(Value::as_u64);
                    if status.is_some_and(|status| status > 0) {
                        sent = UpstreamSendState::Sent;
                    }
                    retry_after = event.payload.get("headers").and_then(Value::as_array)
                        .and_then(|headers| headers.iter().find_map(|header| {
                            let pair = header.as_array()?;
                            if pair.first()?.as_str()?.eq_ignore_ascii_case("retry-after") {
                                gateway_protocol::openai::parse_retry_after_seconds(pair.get(1)?.as_str()?)
                            } else {
                                None
                            }
                        }));
                }
                Ok(ResponseItem::Event(event)) if event.kind == "wire" => {
                    let raw = event.payload.get("frame_base64")
                        .and_then(Value::as_str)
                        .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
                        .ok_or_else(|| provider_error(ProviderErrorKind::Protocol, sent))?;
                    let frame = if public_ids {
                        public_response_frame(&raw, &installation_id, &mut response_ids)
                            .map_err(|_| provider_error(ProviderErrorKind::Protocol, sent))?
                    } else { raw };
                    let events = match decoder.push(&frame) {
                        CodexCanonicalOutcome::Events(events) => events,
                        CodexCanonicalOutcome::Failed(failure) => {
                            let (events, error, semantic_output_seen) = failure.into_parts();
                            for event in events { yield event; }
                            Err(map_canonical_error(error, &CodexUpstreamDiagnostics::default(), &[],
                                ReplayBoundary::from_semantic_output(semantic_output_seen)).error)?;
                            return;
                        }
                    };
                    for event in events {
                        if event.canonical_facts().iter().any(|fact| matches!(fact, GatewayEvent::Completed(_))) {
                            completed = true;
                            terminal_events.push(event);
                        } else {
                            yield event;
                        }
                    }
                }
                Ok(ResponseItem::Done(_)) => {
                    match decoder.finish() {
                        CodexCanonicalOutcome::Events(events) => {
                            for event in events {
                                if event.canonical_facts().iter().any(|fact| matches!(fact, GatewayEvent::Completed(_))) {
                                    completed = true;
                                }
                                terminal_events.push(event);
                            }
                        }
                        CodexCanonicalOutcome::Failed(failure) => {
                            let (events, error, semantic_output_seen) = failure.into_parts();
                            for event in events { yield event; }
                            Err(map_canonical_error(error, &CodexUpstreamDiagnostics::default(), &[],
                                ReplayBoundary::from_semantic_output(semantic_output_seen)).error)?;
                            return;
                        }
                    }
                    if !completed {
                        Err(provider_error(ProviderErrorKind::Protocol, sent))?;
                    }
                    if let Some((raw, _)) = response_ids.as_ref() {
                        for event in &mut terminal_events {
                            if event.canonical_facts().iter().any(|fact| matches!(fact, GatewayEvent::Completed(_))) {
                                event.attach_native_upstream_response_id(raw.clone());
                            }
                        }
                        attach_openai_session_update(&mut terminal_events, &mut capture);
                    } else if public_ids {
                        Err(provider_error(ProviderErrorKind::Protocol, sent))?;
                    }
                    for event in terminal_events { yield event; }
                    break;
                }
                Ok(ResponseItem::Event(_)) => {},
                Err(error) => {
                    tracing::warn!(error = %error, upstream_status = ?status,
                        "OpenAI Responses harness call failed");
                    let error = harness_error_with_status(error, sent, status, retry_after);
                    if lease.allows_account_state_mutation() && sent == UpstreamSendState::Sent {
                        let failure = match error.kind() {
                            ProviderErrorKind::Unauthorized => Some(CodexAccountFailure::CredentialExpired),
                            ProviderErrorKind::RateLimited => Some(CodexAccountFailure::RateLimited {
                                retry_after: error.retry_after(),
                            }),
                            _ => None,
                        };
                        if let Some(failure) = failure {
                            let _ = selector.record_failure(lease.account(), failure, None).await;
                        }
                    }
                    Err(error)?;
                    return;
                }
            }
        }
        drop(response);
        session.shutdown().await.map_err(|error| harness_error(error, sent))?;
    })
}

/// 只对已明确存储的 OpenCode 响应重写公开 ID，保留 SSE 帧的其他行
fn public_response_frame(
    frame: &[u8],
    installation_id: &str,
    ids: &mut Option<(String, String)>,
) -> Result<Vec<u8>, ()> {
    let text = std::str::from_utf8(frame).map_err(|_| ())?;
    let Some(start) = text.find("data: ") else {
        return Ok(frame.to_vec());
    };
    let start = start + "data: ".len();
    let end = start + text[start..].find('\n').ok_or(())?;
    let data = text[start..end].trim_end_matches('\r');
    if data == "[DONE]" {
        return Ok(frame.to_vec());
    }
    let mut event: Value = serde_json::from_str(data).map_err(|_| ())?;
    if let Some(raw) = event
        .pointer("/response/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if let Some((original, _)) = ids.as_ref() {
            if original != &raw {
                return Err(());
            }
        } else {
            let public = format!(
                "{OPENCODE_RESPONSE_ID_PREFIX}{}",
                crate::transport::account_scoped_id(installation_id, &raw)
            );
            *ids = Some((raw, public));
        }
        if let Some(id) = event.pointer_mut("/response/id") {
            *id = Value::String(ids.as_ref().ok_or(())?.1.clone());
        }
    }
    if let Some(id) = event.get_mut("response_id") {
        let (raw, public) = ids.as_ref().ok_or(())?;
        if id.as_str() != Some(raw) {
            return Err(());
        }
        *id = Value::String(public.clone());
    }
    let serialized = serde_json::to_string(&event).map_err(|_| ())?;
    let mut output = Vec::with_capacity(frame.len() + OPENCODE_RESPONSE_ID_PREFIX.len() + 40);
    output.extend_from_slice(&frame[..start]);
    output.extend_from_slice(serialized.as_bytes());
    output.extend_from_slice(&frame[end..]);
    Ok(output)
}

fn harness_error_with_status(
    error: HarnessError,
    sent: UpstreamSendState,
    status: Option<u64>,
    retry_after: Option<u64>,
) -> ProviderError {
    let kind = match status {
        Some(401) => ProviderErrorKind::Unauthorized,
        Some(403) => ProviderErrorKind::PermissionDenied,
        Some(429) => ProviderErrorKind::RateLimited,
        Some(400..=499) => ProviderErrorKind::InvalidRequest,
        Some(500..=599) => ProviderErrorKind::Unavailable,
        _ => return harness_error(error, sent),
    };
    let mut error = provider_error(kind, sent);
    if let Some(status) = status.and_then(|status| u16::try_from(status).ok()) {
        error = error.with_status(status);
    }
    if let Some(seconds) = retry_after {
        error = error.with_retry_after(std::time::Duration::from_secs(seconds));
    }
    error
}

fn harness_error(error: HarnessError, sent: UpstreamSendState) -> ProviderError {
    let kind = match error {
        HarnessError::Cancelled => ProviderErrorKind::Cancelled,
        HarnessError::Timeout => ProviderErrorKind::Timeout,
        HarnessError::Source
        | HarnessError::Startup { .. }
        | HarnessError::NodeVersion { .. }
        | HarnessError::Spawn
        | HarnessError::Exited => ProviderErrorKind::ProviderInfrastructureUnavailable,
        HarnessError::Protocol => ProviderErrorKind::Protocol,
        HarnessError::InvalidInput { .. } => ProviderErrorKind::InvalidRequest,
        HarnessError::Worker { .. } | HarnessError::Upstream | HarnessError::RemoteDial => {
            ProviderErrorKind::Unavailable
        }
    };
    provider_error(kind, sent)
}
