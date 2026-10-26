//! OpenCode 与 pi Responses 的 Provider 进程桥
//!
//! 调用方传入已经决定的有效 `service_tier` 和原请求会话身份
//! 模型目录是否支持 priority，以及 Fast `enabled` / `disabled` 如何改写，都不在这里
//! 账号选择、凭据事务、管理端和发行镜像都还没有调用这里
//! Codex OAuth 仍走 `encode_generate_request`，不能靠换 User-Agent 进入这条路径

mod exchange;
mod process;
mod protocol;

pub use process::{HarnessSession, ResponseStream};

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde_json::Value;

use gateway_core::account::OutboundProxy;
use secrecy::SecretString;

pub const OPENCODE_SOURCE_COMMIT: &str = "ab4d1f3ab1a05278b4d0392124b9e7f1c8131f17";
pub const PI_SOURCE_COMMIT: &str = "f9daca3480732d0042b1df3a6e27221f597a620d";
pub const MINIMUM_NODE_VERSION: (u64, u64, u64) = (22, 19, 0);

#[derive(Debug, Clone, thiserror::Error)]
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

/// 当前会话上可跨 step 重复的身份
///
/// `session_affinity_id` 是原始 parent 或 fork affinity，只给 OpenCode 的 `session-id`
/// 桥用 `ResponsesCall::installation_id` 把它和当前会话各映射一次
/// 缺省时 `session-id` 沿用映射后的当前会话
/// `thread-id`、`x-client-request-id` 和 pi 的 `session_id` 都使用映射后的当前会话，不在这里另造
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponsesRequestIdentity {
    pub session_affinity_id: Option<String>,
}

pub struct ResponsesCall {
    pub harness: ResponsesHarness,
    pub model: String,
    pub base_url: String,
    /// 选定账号的持久 installation_id
    /// 桥只用它把原始会话和 affinity 各映射一次，再送进源库
    /// 不是安装级 host ID 或 OAuth client ID，也不从访问令牌推导
    pub installation_id: String,
    /// 原始当前会话 ID
    /// 桥映射后，OpenCode 用它写 `thread-id` 和 `x-client-request-id`，pi 用它写 `session_id`、`x-client-request-id` 和 `prompt_cache_key`
    /// 同一 installation_id 与原值跨调用稳定；换账号必须换 installation_id，不能把已经映射的值再送进来
    pub session_id: String,
    /// CPR Core 这一次执行或 attempt 的请求 ID
    /// 只关联进程帧，并作为 OpenCode `LLMRequest.id`
    /// 不写入上游头，也不代替会话、affinity 或账号身份
    pub request_id: String,
    /// 选账号后已经决定的有效档位
    /// 空白与缺省都不传给源库，源库因此不会补写 `service_tier`
    /// 非空值进入 OpenCode `providerOptions.serviceTier` 与 pi `options.serviceTier`
    pub service_tier: Option<String>,
    pub identity: ResponsesRequestIdentity,
    /// 这一次调用选定账号的访问令牌，pi 原样当作 `apiKey`
    /// 令牌只属于这次调用，不把另一账号的 host、client 或线程身份带进来
    /// 桥不从令牌推导原生 account ID，也不保存 OAuth client ID 或安装 host ID
    /// `provider` 为 openai、`base_url` 精确为 `https://api.openai.com/v1` 且令牌不以 `sk-` 开头时，源码走 ChatGPT 分支
    /// 桥不改写成 `unused`，也不改用下游 `Authorization`
    pub bearer: SecretString,
    /// 下游原请求头
    /// 身份、会话、传输和 hop-by-hop 不进入源库，也不改写源码自己的会话头
    /// 只有源码本来就要发送的非身份扩展头才能透传，当前没有这种头
    pub downstream_headers: Vec<(String, String)>,
    /// 当前文本轮次，历史和工具轮次在这个结构上追加
    pub prompt: String,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub body_window: usize,
    /// 调用方尚未取走的事件上限，worker 在该窗口耗尽时等待
    pub event_window: usize,
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
    /// 源事件原文，后续字段从这里读取
    pub payload: Value,
}

#[derive(Debug)]
pub enum ResponseItem {
    Request(ObservedUpstreamRequest),
    Event(HarnessEvent),
    Done(ResponseDone),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseDone {
    pub credit_waits: usize,
    pub event_waits: usize,
    /// 调用结束时仍挂在取消信号上的等待监听
    pub abort_listeners: usize,
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
    pub event_waits: usize,
    pub abort_listeners: usize,
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
