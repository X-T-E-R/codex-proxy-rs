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
    AttemptContext, CodexCanonicalDecoder, CodexCanonicalOutcome, CodexCredentialLease,
    CodexProvider, CodexUpstreamDiagnostics, MiddlewareHeader, ReplayBoundary, map_canonical_error,
    provider_error,
};
use crate::credential::harness_oauth::{HarnessMethod, method_for_kind};
use crate::harness::{
    HarnessError, HarnessSession, HarnessWorkerConfig, ResponseItem, ResponsesCall,
    ResponsesHarness, ResponsesRequestIdentity,
};
use crate::request_identity::account_session_with_headers;

const RESPONSES_BASE_URL: &str = "https://api.openai.com/v1";

impl CodexProvider {
    #[expect(clippy::too_many_arguments)]
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
        let raw_thread = headers
            .iter()
            .find(|header| header.name().eq_ignore_ascii_case("thread-id"))
            .and_then(|header| std::str::from_utf8(header.value()).ok())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned);
        let session_id = raw_thread
            .or_else(|| raw_session.clone())
            .unwrap_or_else(|| context.request_id().as_str().to_owned());
        let identity = ResponsesRequestIdentity {
            session_affinity_id: raw_session.filter(|parent| parent != &session_id),
        };
        let mut body = generate.protocol_payload().body().clone();
        body.insert("model".to_owned(), Value::String(model.as_str().to_owned()));
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
            model: model.as_str().to_owned(),
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
        let runtime = HarnessWorkerConfig {
            node: PathBuf::from("node"),
            worker_dir: paths.worker_dir,
            opencode_checkout: paths.opencode_checkout,
            pi_checkout: paths.pi_checkout,
            proxy: lease.account().outbound_proxy().cloned(),
            allow_remote_dial: true,
            dial_loopback: None,
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
        let events = harness_response_stream(runtime, request, context, model, service_tier);
        let stream = ProviderStream::new(metadata, events, Arc::new(lease));
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

fn harness_response_stream(
    runtime: HarnessWorkerConfig,
    request: ResponsesCall,
    context: AttemptContext,
    model: UpstreamModelId,
    service_tier: Option<String>,
) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let session = HarnessSession::open(runtime).await.map_err(|error| {
            tracing::error!(error = %error, "OpenAI Responses harness could not start");
            harness_error(error, UpstreamSendState::NotSent)
        })?;
        let mut response = session.open_responses(request, context.cancellation()).await
            .map_err(|error| harness_error(error, UpstreamSendState::NotSent))?;
        let mut decoder = CodexCanonicalDecoder::new(model.as_str())
            .with_trace(context.trace())
            .with_pricing(context.pricing().get("openai")
                .and_then(|models| models.get(model.as_str())).cloned())
            .with_requested_service_tier(service_tier.as_deref())
            .with_raw_sse_passthrough();
        let mut sent = UpstreamSendState::NotSent;
        let mut status = None;
        let mut terminal_events = Vec::new();
        let mut completed = false;
        loop {
            let next = response.next().await;
            match next {
                Ok(ResponseItem::Request(_)) => sent = UpstreamSendState::Ambiguous,
                Ok(ResponseItem::Event(event)) if event.kind == "head" => {
                    status = event.payload.get("status").and_then(Value::as_u64);
                }
                Ok(ResponseItem::Event(event)) if event.kind == "wire" => {
                    let raw = event.payload.get("frame_base64")
                        .and_then(Value::as_str)
                        .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
                        .ok_or_else(|| provider_error(ProviderErrorKind::Protocol, sent))?;
                    let events = match decoder.push(&raw) {
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
                    for event in terminal_events { yield event; }
                    break;
                }
                Ok(ResponseItem::Event(_)) => {},
                Err(error) => {
                    tracing::warn!(error = %error, upstream_status = ?status,
                        "OpenAI Responses harness call failed");
                    Err(harness_error_with_status(error, sent, status))?;
                    return;
                }
            }
        }
        drop(response);
        session.shutdown().await.map_err(|error| harness_error(error, sent))?;
    })
}

fn harness_error_with_status(
    error: HarnessError,
    sent: UpstreamSendState,
    status: Option<u64>,
) -> ProviderError {
    let kind = match status {
        Some(401) => ProviderErrorKind::Unauthorized,
        Some(403) => ProviderErrorKind::PermissionDenied,
        Some(429) => ProviderErrorKind::RateLimited,
        Some(400..=499) => ProviderErrorKind::InvalidRequest,
        Some(500..=599) => ProviderErrorKind::Unavailable,
        _ => return harness_error(error, sent),
    };
    provider_error(kind, sent)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_input_is_invalid_request_without_upstream_send() {
        let error = harness_error(
            HarnessError::InvalidInput {
                detail: "unsupported content".to_owned(),
            },
            UpstreamSendState::NotSent,
        );
        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert_eq!(error.send_state(), UpstreamSendState::NotSent);
    }
}
