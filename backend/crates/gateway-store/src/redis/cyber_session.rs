//! Client Key 隔离的 cyber 语义会话封禁状态。

use std::time::Duration;

use futures::future::BoxFuture;
use gateway_core::policy::{
    CyberSessionKey, CyberSessionPort, CyberSessionStoreError, MAX_TRANSCRIPT_LOOKUPS,
};
use redis::aio::ConnectionManager;

use crate::StoreResult;

use super::namespace;

const CONTAINS_ANY_SCRIPT: &str = r#"
for _, key in ipairs(KEYS) do
  if redis.call('PTTL', key) > 0 then
    return 1
  end
end
return 0
"#;

#[derive(Clone)]
pub struct RedisCyberSessionRepository {
    connection: ConnectionManager,
    namespace: String,
}

impl RedisCyberSessionRepository {
    pub fn new(connection: ConnectionManager, key_namespace: &str) -> StoreResult<Self> {
        Ok(Self {
            connection,
            namespace: format!("{}:cyber-session:v1", namespace(key_namespace)?),
        })
    }

    fn key(&self, key: &CyberSessionKey) -> String {
        let digest = key.expose_to_store();
        format!("{}:{{{digest}}}", self.namespace)
    }
}

impl CyberSessionPort for RedisCyberSessionRepository {
    fn contains_any<'a>(
        &'a self,
        keys: &'a [CyberSessionKey],
    ) -> BoxFuture<'a, Result<bool, CyberSessionStoreError>> {
        Box::pin(async move {
            if keys.is_empty() {
                return Ok(false);
            }
            if keys.len() > MAX_TRANSCRIPT_LOOKUPS {
                return Err(CyberSessionStoreError);
            }
            let script = redis::Script::new(CONTAINS_ANY_SCRIPT);
            let mut invocation = script.prepare_invoke();
            for key in keys {
                invocation.key(self.key(key));
            }
            let mut connection = self.connection.clone();
            invocation
                .invoke_async::<u64>(&mut connection)
                .await
                .map(|matched| matched != 0)
                .map_err(|_| CyberSessionStoreError)
        })
    }

    fn record<'a>(
        &'a self,
        key: &'a CyberSessionKey,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), CyberSessionStoreError>> {
        Box::pin(async move {
            let ttl_seconds = u32::try_from(ttl.as_secs())
                .ok()
                .filter(|seconds| *seconds > 0)
                .ok_or(CyberSessionStoreError)?;
            let mut connection = self.connection.clone();
            redis::cmd("SET")
                .arg(self.key(key))
                .arg("1")
                .arg("NX")
                .arg("EX")
                .arg(ttl_seconds)
                .query_async::<Option<String>>(&mut connection)
                .await
                .map(|_| ())
                .map_err(|_| CyberSessionStoreError)
        })
    }
}
