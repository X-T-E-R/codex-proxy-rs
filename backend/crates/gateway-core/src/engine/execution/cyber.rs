//! 入口的 Cyber TTL 检查、首次真实拒绝记录和本地零尝试终态

use super::COORDINATION_TIMEOUT;
use crate::{
    engine::{
        ExecutionOutcome, ExecutionStore, ModelRequestFinalization, NewModelRequest,
        UpstreamSendState,
    },
    error::{ClientVisibleUpstreamError, GatewayError, GatewayErrorKind},
    provider_ports::{CyberSessionKey, CyberSessionPort},
};
use futures::{FutureExt as _, future::BoxFuture, pin_mut, select_biased};
use futures_timer::Delay;
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

pub(super) struct CyberRefusalRecord {
    pub port: Arc<dyn CyberSessionPort>,
    pub key: CyberSessionKey,
    pub ttl: Duration,
}

impl CyberRefusalRecord {
    pub fn into_future(self) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let record = self.port.record(&self.key, self.ttl).fuse();
            let timeout = Delay::new(COORDINATION_TIMEOUT).fuse();
            pin_mut!(record, timeout);
            select_biased! {
                result = record => if let Err(error) = result {
                    tracing::warn!(%error, "Cyber 会话封禁写入失败，当前拒绝仍保持终止");
                },
                _ = timeout => tracing::warn!("Cyber 会话封禁写入超时，当前拒绝仍保持终止"),
            }
        })
    }
}

pub(super) async fn is_blocked(
    port: Option<&dyn CyberSessionPort>,
    keys: &[CyberSessionKey],
) -> bool {
    let Some(port) = port.filter(|_| !keys.is_empty()) else {
        return false;
    };
    let lookup = port.contains_any(keys).fuse();
    let timeout = Delay::new(COORDINATION_TIMEOUT).fuse();
    pin_mut!(lookup, timeout);
    select_biased! {
        result = lookup => result.unwrap_or_else(|error| {
            tracing::warn!(%error, "Cyber 会话封禁读取失败，按可丢失状态 fail-open");
            false
        }),
        _ = timeout => {
            tracing::warn!("Cyber 会话封禁读取超时，按可丢失状态 fail-open");
            false
        },
    }
}

pub(super) fn blocked_error() -> GatewayError {
    const MESSAGE: &str = "this session is blocked after an upstream cyber policy refusal";
    GatewayError::new(GatewayErrorKind::PolicyDenied, MESSAGE).with_client_visible_upstream_error(
        ClientVisibleUpstreamError::new(
            MESSAGE,
            Some("session_blocked_by_cyber_policy".to_owned()),
            Some("permission_error".to_owned()),
        ),
    )
}

pub(super) async fn persist_local_block(
    store: &dyn ExecutionStore,
    request: NewModelRequest,
    error: GatewayError,
) {
    let request_id = request.id.clone();
    let write = async {
        store.create_model_request(request).await?;
        store
            .finalize_model_request(ModelRequestFinalization {
                request_id: request_id.clone(),
                outcome: ExecutionOutcome::Failed,
                send_state: UpstreamSendState::NotSent,
                attempt_count: 0,
                downstream_committed_at: None,
                client_status_code: Some(403),
                client_response_id: None,
                upstream_status_code: None,
                upstream_request_id: None,
                upstream_response_id: None,
                upstream_transport: None,
                http_version: None,
                websocket_pool: None,
                service_tier: None,
                upstream_response_model: None,
                provider_metadata_json: None,
                diagnostic_trace_json: None,
                error: Some(error),
                provider_error_code: None,
                error_details: None,
                failure_observation: Default::default(),
                retry_after_ms: None,
                usage: Default::default(),
                image_generation_succeeded: None,
                cost: crate::metering::CostEstimate::unavailable(),
                timings: Default::default(),
                completed_at: SystemTime::now(),
            })
            .await
    }
    .fuse();
    let timeout = Delay::new(COORDINATION_TIMEOUT).fuse();
    pin_mut!(write, timeout);
    select_biased! {
        result = write => if let Err(error) = result {
            tracing::warn!(request_id = request_id.as_str(), %error, "本地 Cyber 拒绝记录写入失败");
        },
        _ = timeout => tracing::warn!(request_id = request_id.as_str(), "本地 Cyber 拒绝记录写入超时"),
    }
}
