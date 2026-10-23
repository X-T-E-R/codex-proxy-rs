//! Key 隔离的 Cyber 语义会话身份与可丢失 TTL 状态端口

use std::{fmt, time::Duration};

use futures::future::BoxFuture;
use sha2::{Digest, Sha256};

use crate::policy::ClientApiKeyId;

pub const MAX_TRANSCRIPT_LOOKUPS: usize = 256;

/// 不保存客户端正文或原始线程身份的会话键
#[derive(Clone, PartialEq, Eq)]
pub struct CyberSessionKey([u8; 32]);

impl CyberSessionKey {
    fn scoped(client: &ClientApiKeyId, digest: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, b"codex-proxy-rs.cyber-session.scope.v1");
        hash_field(&mut hasher, client.as_str().as_bytes());
        hash_field(&mut hasher, digest);
        Self(hasher.finalize().into())
    }

    #[must_use]
    pub fn expose_to_store(&self) -> String {
        use std::fmt::Write as _;
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        encoded
    }
}

impl fmt::Debug for CyberSessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CyberSessionKey([OPAQUE])")
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct CyberSessionRequest {
    explicit: Option<[u8; 32]>,
    transcript: Vec<[u8; 32]>,
}

impl fmt::Debug for CyberSessionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CyberSessionRequest")
            .field("explicit", &self.explicit.is_some())
            .field("transcript_prefixes", &self.transcript.len())
            .finish()
    }
}

impl CyberSessionRequest {
    /// 入口拥有协议解析；Core 只接受不可逆语义摘要并在 Key 范围内隔离
    #[must_use]
    pub fn from_digests(explicit: Option<[u8; 32]>, mut transcript: Vec<[u8; 32]>) -> Self {
        if explicit.is_some() {
            transcript.clear();
        } else if transcript.len() > MAX_TRANSCRIPT_LOOKUPS {
            transcript.drain(..transcript.len() - MAX_TRANSCRIPT_LOOKUPS);
        }
        Self {
            explicit,
            transcript,
        }
    }

    #[must_use]
    pub fn lookup_keys(&self, client: &ClientApiKeyId) -> Vec<CyberSessionKey> {
        self.explicit
            .iter()
            .chain(self.transcript.iter())
            .map(|digest| CyberSessionKey::scoped(client, digest))
            .collect()
    }

    #[must_use]
    pub fn refusal_key(&self, client: &ClientApiKeyId) -> Option<CyberSessionKey> {
        self.explicit
            .as_ref()
            .or_else(|| self.transcript.last())
            .map(|digest| CyberSessionKey::scoped(client, digest))
    }
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(value);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cyber session store is unavailable")]
pub struct CyberSessionStoreError;

/// marker 只由首次真实拒绝建立，重复拒绝不续期，关闭开关和成功均不删除
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
