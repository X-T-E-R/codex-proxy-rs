//! Cyber policy 拒绝后的客户端语义会话隔离策略。

use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::ClientApiKeyId;

const MAX_SESSION_ID_CHARS: usize = 255;
pub const MAX_TRANSCRIPT_LOOKUPS: usize = 256;

/// 一次 runtime snapshot 冻结的 cyber 会话封禁策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CyberSessionBlockPolicy {
    ttl_seconds: NonZeroU32,
}

impl CyberSessionBlockPolicy {
    #[must_use]
    pub const fn new(ttl_seconds: NonZeroU32) -> Self {
        Self { ttl_seconds }
    }

    #[must_use]
    pub const fn ttl(self) -> Duration {
        Duration::from_secs(self.ttl_seconds.get() as u64)
    }
}

/// 已按 Client Key 隔离、只供协调存储使用的不可逆会话键。
#[derive(Clone, PartialEq, Eq)]
pub struct CyberSessionKey([u8; 32]);

impl CyberSessionKey {
    fn scoped(client_api_key_id: &ClientApiKeyId, semantic_digest: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, b"codex-proxy-rs.cyber-session.scope.v1");
        hash_field(&mut hasher, client_api_key_id.as_str().as_bytes());
        hash_field(&mut hasher, semantic_digest);
        Self(hasher.finalize().into())
    }

    /// 只向 Store adapter 暴露不可逆摘要。
    #[must_use]
    pub fn expose_to_store(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for CyberSessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CyberSessionKey(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
enum CyberSessionMode {
    None,
    Explicit([u8; 32]),
    Transcript(Vec<[u8; 32]>),
}

/// API 从一次完整 Responses 请求中提取的不可逆语义会话证据。
#[derive(Clone, PartialEq, Eq)]
pub struct CyberSessionRequest {
    mode: CyberSessionMode,
}

impl Default for CyberSessionRequest {
    fn default() -> Self {
        Self {
            mode: CyberSessionMode::None,
        }
    }
}

impl fmt::Debug for CyberSessionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match &self.mode {
            CyberSessionMode::None => "none",
            CyberSessionMode::Explicit(_) => "explicit",
            CyberSessionMode::Transcript(_) => "transcript",
        };
        formatter
            .debug_struct("CyberSessionRequest")
            .field("mode", &mode)
            .finish()
    }
}

impl CyberSessionRequest {
    /// 按 Responses 原生语义优先级提取显式会话；没有显式身份时才生成完整历史累计摘要。
    #[must_use]
    pub fn from_responses<'a>(
        body: &Map<String, Value>,
        turn_metadata_header: Option<&str>,
        semantic_headers: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let explicit = body
            .get("client_metadata")
            .and_then(Value::as_object)
            .and_then(|metadata| {
                valid_session_value(metadata.get("session_id")).or_else(|| {
                    turn_metadata_session(
                        metadata
                            .get("x-codex-turn-metadata")
                            .or_else(|| metadata.get("X-Codex-Turn-Metadata")),
                    )
                })
            })
            .or_else(|| turn_metadata_header.and_then(parse_turn_metadata_session))
            .or_else(|| semantic_headers.into_iter().find_map(valid_session_id));
        if let Some(session_id) = explicit {
            let mut hasher = Sha256::new();
            hash_field(&mut hasher, b"codex-proxy-rs.cyber-session.explicit.v1");
            hash_field(&mut hasher, session_id.as_bytes());
            return Self {
                mode: CyberSessionMode::Explicit(hasher.finalize().into()),
            };
        }

        Self {
            mode: transcript_digests(body)
                .map(CyberSessionMode::Transcript)
                .unwrap_or(CyberSessionMode::None),
        }
    }

    #[must_use]
    pub fn lookup_keys(&self, client_api_key_id: &ClientApiKeyId) -> Vec<CyberSessionKey> {
        match &self.mode {
            CyberSessionMode::None => Vec::new(),
            CyberSessionMode::Explicit(digest) => {
                vec![CyberSessionKey::scoped(client_api_key_id, digest)]
            }
            CyberSessionMode::Transcript(digests) => digests
                .iter()
                .map(|digest| CyberSessionKey::scoped(client_api_key_id, digest))
                .collect(),
        }
    }

    #[must_use]
    pub fn refusal_key(&self, client_api_key_id: &ClientApiKeyId) -> Option<CyberSessionKey> {
        match &self.mode {
            CyberSessionMode::None => None,
            CyberSessionMode::Explicit(digest) => {
                Some(CyberSessionKey::scoped(client_api_key_id, digest))
            }
            CyberSessionMode::Transcript(digests) => digests
                .last()
                .map(|digest| CyberSessionKey::scoped(client_api_key_id, digest)),
        }
    }
}

fn valid_session_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).and_then(valid_session_id)
}

fn turn_metadata_session(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Object(metadata) => valid_session_value(metadata.get("session_id")),
        Value::String(encoded) => parse_turn_metadata_session(encoded),
        _ => None,
    }
}

fn parse_turn_metadata_session(encoded: &str) -> Option<String> {
    serde_json::from_str::<Value>(encoded)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .and_then(|metadata| valid_session_value(metadata.get("session_id")))
}

fn valid_session_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.chars().take(MAX_SESSION_ID_CHARS + 1).count() <= MAX_SESSION_ID_CHARS
        && !value
            .chars()
            .any(|character| character < '\u{20}' || character == '\u{7f}'))
    .then(|| value.to_owned())
}

fn transcript_digests(body: &Map<String, Value>) -> Option<Vec<[u8; 32]>> {
    let history: Vec<&Value> = if let Some(messages) = body.get("messages") {
        messages.as_array()?.iter().collect()
    } else {
        let input = body.get("input")?;
        match input {
            Value::Array(items) => items.iter().collect(),
            Value::String(value) if !value.is_empty() => vec![input],
            _ => return None,
        }
    };
    if history.is_empty() {
        return None;
    }

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"codex-proxy-rs.cyber-session.transcript.v1");
    for field in ["instructions", "system"] {
        if let Some(value) = body.get(field).and_then(Value::as_str)
            && !value.is_empty()
        {
            hash_field(&mut hasher, field.as_bytes());
            hash_field(&mut hasher, value.as_bytes());
        }
    }

    let mut digests = VecDeque::with_capacity(MAX_TRANSCRIPT_LOOKUPS);
    for item in history {
        let mut canonical = Vec::new();
        write_canonical_json(item, &mut canonical);
        hash_field(&mut hasher, &canonical);
        if digests.len() == MAX_TRANSCRIPT_LOOKUPS {
            digests.pop_front();
        }
        digests.push_back(hasher.clone().finalize().into());
    }
    Some(digests.into())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(value);
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => {
            serde_json::to_writer(output, value).expect("writing JSON to Vec cannot fail");
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output);
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key).expect("writing JSON to Vec cannot fail");
                output.push(b':');
                write_canonical_json(&values[key], output);
            }
            output.push(b'}');
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cyber session store is unavailable")]
pub struct CyberSessionStoreError;

/// 可丢失 Redis 会话封禁状态的 Core 端口。
pub trait CyberSessionPort: Send + Sync {
    fn contains_any<'a>(
        &'a self,
        keys: &'a [CyberSessionKey],
    ) -> BoxFuture<'a, Result<bool, CyberSessionStoreError>>;

    fn record<'a>(
        &'a self,
        key: &'a CyberSessionKey,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), CyberSessionStoreError>>;
}
