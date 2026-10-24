//! 单部署停止闸的定向 Redis 恢复，认证只消费正常 Store 配置

use std::collections::BTreeMap;

use redis::aio::ConnectionManager;
use serde::Deserialize;

use crate::{StoreConfig, StoreError, StoreResult, redis_unavailable};

const NAMESPACE: &str = "codex-proxy-rs";
const MAX_REQUESTS: usize = 5_000;

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Command {
    Scan {
        cursor: u64,
        reserved: bool,
    },
    ReleaseAccount {
        key: String,
        requests: BTreeMap<String, String>,
    },
    ReleaseClient {
        client: String,
        requests: Vec<String>,
    },
}

const RELEASE_ACCOUNT: &str = r#"
local approved=cjson.decode(ARGV[1])
if redis.call('ZCARD',KEYS[1])>5000 then
 return redis.error_reply('recovery active set exceeds bound')
end
local removed=0
for _,member in ipairs(redis.call('ZRANGE',KEYS[1],0,-1)) do
 local lease=string.match(member,'^[^|]+|([^|]+)|[^|]+$')
 if lease then
  local detail=KEYS[1]..':detail:'..lease
  local raw=redis.call('HGET',detail,'facts')
  if raw then
   local ok,facts=pcall(cjson.decode,raw)
   if ok and type(facts)=='table' and approved[facts.request_id]
    and approved[facts.request_id]==facts.client_key_id then
    removed=removed+redis.call('ZREM',KEYS[1],member)
    redis.call('DEL',detail)
   end
  end
 end
end
return removed
"#;

const RELEASE_CLIENT: &str = r#"
local removed=0
for _,request in ipairs(cjson.decode(ARGV[1])) do
 removed=removed+redis.call('ZREM',KEYS[1],request)
end
return removed
"#;

/// 此入口不启动 PG、迁移、Worker 或模型服务，也不把认证信息放入命令行
pub async fn site_recovery_redis(config: &StoreConfig, input: &[u8]) -> StoreResult<String> {
    let client = redis::Client::open(config.redis_url()?)
        .map_err(|source| redis_unavailable("create recovery Redis client", source))?;
    let connection = client
        .get_connection_manager()
        .await
        .map_err(|source| redis_unavailable("connect recovery Redis manager", source))?;
    SiteRecoveryRedis::new(connection).execute(input).await
}

/// 只提供固定 namespace 的扫描与按已确认请求 ID 定向释放，不接受任意 Redis 命令
pub struct SiteRecoveryRedis {
    connection: ConnectionManager,
}

impl SiteRecoveryRedis {
    #[must_use]
    pub const fn new(connection: ConnectionManager) -> Self {
        Self { connection }
    }

    pub async fn execute(&self, input: &[u8]) -> StoreResult<String> {
        if input.len() > 2 * 1024 * 1024 {
            return Err(invalid());
        }
        let command: Command = serde_json::from_slice(input).map_err(|_| invalid())?;
        let mut connection = self.connection.clone();
        let result = match command {
            Command::Scan { cursor, reserved } => {
                let scope = if reserved {
                    "account-reserved"
                } else {
                    "account"
                };
                let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(format!("{NAMESPACE}:lease:{scope}:*:active"))
                    .arg("COUNT")
                    .arg(100)
                    .query_async(&mut connection)
                    .await
                    .map_err(|source| redis_unavailable("scan stopped owner slots", source))?;
                serde_json::json!([next.to_string(), keys])
            }
            Command::ReleaseAccount { key, requests } => {
                let fingerprint = ["account", "account-reserved"].iter().find_map(|scope| {
                    key.strip_prefix(&format!("{NAMESPACE}:lease:{scope}:{{"))
                        .and_then(|tail| tail.strip_suffix("}:active"))
                });
                if !fingerprint.is_some_and(|value| {
                    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
                }) || requests.len() > MAX_REQUESTS
                    || requests.is_empty()
                {
                    return Err(invalid());
                }
                for (request, client) in &requests {
                    validate_id(request)?;
                    validate_id(client)?;
                }
                let approved = serde_json::to_string(&requests).map_err(|_| invalid())?;
                let count: u64 = redis::Script::new(RELEASE_ACCOUNT)
                    .key(key)
                    .arg(approved)
                    .invoke_async(&mut connection)
                    .await
                    .map_err(|source| redis_unavailable("release stopped account slots", source))?;
                serde_json::json!(count)
            }
            Command::ReleaseClient { client, requests } => {
                validate_id(&client)?;
                if requests.len() > MAX_REQUESTS || requests.is_empty() {
                    return Err(invalid());
                }
                for request in &requests {
                    validate_id(request)?;
                }
                let tag = super::resource_fingerprint("client admission", &client)?;
                let key = format!("{NAMESPACE}:client:{{{tag}}}:active");
                let approved = serde_json::to_string(&requests).map_err(|_| invalid())?;
                let count: u64 = redis::Script::new(RELEASE_CLIENT)
                    .key(key)
                    .arg(approved)
                    .invoke_async(&mut connection)
                    .await
                    .map_err(|source| redis_unavailable("release stopped client slots", source))?;
                serde_json::json!(count)
            }
        };
        serde_json::to_string(&result).map_err(|_| invalid())
    }
}

fn validate_id(value: &str) -> StoreResult<()> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> StoreError {
    StoreError::InvalidData {
        entity: "single-deployment recovery",
        message: "invalid recovery command".to_owned(),
        source: None,
    }
}
