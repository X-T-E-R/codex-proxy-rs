//! OpenCode 与 pi Responses 的 Provider 进程桥
//!
//! 调用方传入已经决定的有效 `service_tier` 和原请求会话身份
//! 模型目录是否支持 priority，以及 Fast `enabled` / `disabled` 如何改写，都不在这里
//! 账号选择、凭据事务、管理端和发行镜像都还没有调用这里
//! Codex OAuth 仍走 `encode_generate_request`，不能靠换 User-Agent 进入这条路径

mod exchange;
mod process;
mod protocol;

pub use process::HarnessSession;

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use gateway_core::account::OutboundProxy;
use secrecy::SecretString;

pub const OPENCODE_SOURCE_COMMIT: &str = "ab4d1f3ab1a05278b4d0392124b9e7f1c8131f17";
pub const PI_SOURCE_COMMIT: &str = "f9daca3480732d0042b1df3a6e27221f597a620d";
pub const MINIMUM_NODE_VERSION: (u64, u64, u64) = (22, 19, 0);

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness node process failed to start")]
    Spawn,
    #[error("harness worker was not ready: {detail}")]
    Startup { detail: String },
    #[error("harness node {version} is below 22.19.0")]
    NodeVersion { version: String },
    #[error("harness source did not match the configured checkout")]
    Source,
    #[error("harness worker failed: {detail}")]
    Worker { detail: String },
    #[error("harness worker exited")]
    Exited,
    #[error("harness call timed out")]
    Timeout,
    #[error("harness call was cancelled")]
    Cancelled,
    #[error("harness remote dial is disabled")]
    RemoteDial,
    #[error("harness upstream request failed")]
    Upstream,
    #[error("harness protocol was interrupted")]
    Protocol,
}

#[derive(Clone)]
pub struct HarnessWorkerConfig {
    pub node: PathBuf,
    pub worker_dir: PathBuf,
    pub opencode_checkout: PathBuf,
    pub pi_checkout: PathBuf,
    pub proxy: Option<OutboundProxy>,
    pub allow_remote_dial: bool,
    pub dial_loopback: Option<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesHarness {
    OpenCode,
    Pi,
}

/// 下游原请求的会话身份
///
/// OpenCode 写成 `session-id`、`thread-id`、`x-client-request-id`
/// 缺省字段沿用 `ResponsesCall::session_id`，不另造 UUID
/// pi 只消费 `session_id`：源码同时写 `session_id`、`x-client-request-id` 和 `prompt_cache_key`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponsesRequestIdentity {
    pub session_affinity_id: Option<String>,
    pub thread_id: Option<String>,
    pub client_request_id: Option<String>,
}

pub struct ResponsesCall {
    pub harness: ResponsesHarness,
    pub model: String,
    pub base_url: String,
    pub session_id: String,
    /// 选账号后已经决定的有效档位
    /// 空白与缺省都不传给源库，源库因此不会补写 `service_tier`
    /// 非空值进入 OpenCode `providerOptions.serviceTier` 与 pi `options.serviceTier`
    pub service_tier: Option<String>,
    pub identity: ResponsesRequestIdentity,
    pub bearer: SecretString,
    pub prompt: String,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub body_window: usize,
    pub timeout: Duration,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ObservedUpstreamRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for ObservedUpstreamRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservedUpstreamRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &redacted_headers(&self.headers))
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEvent {
    pub kind: String,
    pub event_type: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub finish: Option<String>,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSource {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug)]
pub struct ParsedResponses {
    pub request: ObservedUpstreamRequest,
    pub events: Vec<HarnessEvent>,
    pub credit_waits: usize,
}

fn redacted_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let hidden = matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "cookie"
            );
            (
                name.clone(),
                if hidden {
                    "[redacted]".to_owned()
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}

pub(crate) fn detail_from(text: &str) -> String {
    const LIMIT: usize = 1500;
    let mut detail = String::new();
    for ch in text.chars() {
        if detail.len() + ch.len_utf8() > LIMIT {
            break;
        }
        detail.push(ch);
    }
    detail
}
