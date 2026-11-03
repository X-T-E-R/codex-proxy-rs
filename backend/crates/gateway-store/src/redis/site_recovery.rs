//! 单部署停止闸的定向 Redis 恢复，认证只消费正常 Store 配置

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use redis::aio::ConnectionManager;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout};

use crate::{StoreConfig, StoreError, StoreResult, redis_unavailable};

const NAMESPACE: &str = "codex-proxy-rs";
const MAX_REQUESTS: usize = 5_000;
const MAX_INPUT: usize = 2 * 1024 * 1024;
const MAX_KEYS: usize = 10_000;
const MAX_SCAN_PAGES: u32 = 1_024;
const TIMEOUT_MILLIS: u64 = 10_000;
const CONTRACT: &str = "cpr-recovery-confirmed/1";

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Command {
    Authenticate,
    ReleaseConfirmed {
        requests: BTreeMap<String, String>,
        max_scan_pages_per_pool: u32,
        timeout_millis: u64,
    },
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
    let command = parse(input)?;
    let bounded = matches!(
        command,
        Command::Authenticate | Command::ReleaseConfirmed { .. }
    );
    let operation = async {
        let client = redis::Client::open(config.redis_url()?)
            .map_err(|source| redis_unavailable("create recovery Redis client", source))?;
        let connection = client
            .get_connection_manager()
            .await
            .map_err(|source| redis_unavailable("connect recovery Redis manager", source))?;
        SiteRecoveryRedis::new(connection)
            .execute_command(command)
            .await
    };
    // 新合同预算包含认证/连接建立，旧 typed 入口保持原使用条件
    if bounded {
        timeout(Duration::from_millis(TIMEOUT_MILLIS), operation)
            .await
            .map_err(|_| refused("recovery elapsed budget exceeded"))?
    } else {
        operation.await
    }
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
        self.execute_command(parse(input)?).await
    }

    async fn execute_command(&self, command: Command) -> StoreResult<String> {
        let mut connection = self.connection.clone();
        let result = match command {
            Command::Authenticate => {
                let _: String = redis::cmd("PING")
                    .query_async(&mut connection)
                    .await
                    .map_err(|source| redis_unavailable("authenticate recovery Redis", source))?;
                json!({"contract": CONTRACT, "maxRequests": MAX_REQUESTS, "maxInputBytes": MAX_INPUT,
                    "maxMatchedKeysPerPool": MAX_KEYS, "maxMembersPerSet": 5000,
                    "maxScanPagesPerPool": MAX_SCAN_PAGES, "timeoutMillis": TIMEOUT_MILLIS,
                    "transportTimeoutSeconds": 15})
            }
            Command::ReleaseConfirmed {
                requests,
                max_scan_pages_per_pool,
                timeout_millis,
            } => {
                validate_requests(&requests)?;
                if max_scan_pages_per_pool == 0
                    || max_scan_pages_per_pool > MAX_SCAN_PAGES
                    || timeout_millis == 0
                    || timeout_millis > TIMEOUT_MILLIS
                {
                    return Err(invalid());
                }
                timeout(
                    Duration::from_millis(timeout_millis),
                    self.release_confirmed(&requests, max_scan_pages_per_pool),
                )
                .await
                .map_err(|_| refused("recovery elapsed budget exceeded"))??
            }
            Command::Scan { cursor, reserved } => {
                let (next, keys) = scan(&mut connection, cursor, reserved).await?;
                json!([next.to_string(), keys])
            }
            Command::ReleaseAccount { key, requests } => {
                validate_account_key(&key)?;
                validate_requests(&requests)?;
                json!(release_account(&mut connection, &key, &requests).await?)
            }
            Command::ReleaseClient { client, requests } => {
                validate_id(&client)?;
                if requests.len() > MAX_REQUESTS || requests.is_empty() {
                    return Err(invalid());
                }
                for request in &requests {
                    validate_id(request)?;
                }
                json!(release_client(&mut connection, &client, &requests).await?)
            }
        };
        serde_json::to_string(&result).map_err(|_| invalid())
    }

    async fn release_confirmed(
        &self,
        requests: &BTreeMap<String, String>,
        max_pages: u32,
    ) -> StoreResult<Value> {
        let started = Instant::now();
        let mut connection = self.connection.clone();
        let mut keys = BTreeSet::new();
        let mut pages = [0_u32; 2];
        let mut matched = [0_usize; 2];
        // 先完成有界发现和输入校验，空页也消耗预算，触界不开始释放
        for (pool, reserved) in [false, true].into_iter().enumerate() {
            let mut cursor = 0;
            loop {
                if pages[pool] >= max_pages {
                    return Err(refused("recovery scan page budget exceeded"));
                }
                pages[pool] += 1;
                let (next, found) = scan(&mut connection, cursor, reserved).await?;
                matched[pool] += found.len();
                if matched[pool] > MAX_KEYS {
                    return Err(refused("recovery key scan exceeds bound"));
                }
                for key in found {
                    validate_account_key(&key)?;
                    keys.insert(key);
                }
                cursor = next;
                if cursor == 0 {
                    break;
                }
            }
        }
        let mut accounts = 0;
        for key in keys {
            accounts += release_account(&mut connection, &key, requests).await?;
        }
        let mut grouped: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (request, client) in requests {
            grouped.entry(client).or_default().push(request.clone());
        }
        let mut clients = 0;
        for (client, requests) in grouped {
            clients += release_client(&mut connection, client, &requests).await?;
        }
        Ok(
            json!({"contract": CONTRACT, "released": {"accountSlots": accounts, "clientSlots": clients},
            "scanPages": pages, "matchedKeys": matched, "elapsedMillis": started.elapsed().as_millis()}),
        )
    }
}

fn parse(input: &[u8]) -> StoreResult<Command> {
    if input.len() > MAX_INPUT {
        return Err(invalid());
    }
    serde_json::from_slice(input).map_err(|_| invalid())
}

async fn scan(
    connection: &mut ConnectionManager,
    cursor: u64,
    reserved: bool,
) -> StoreResult<(u64, Vec<String>)> {
    let scope = if reserved {
        "account-reserved"
    } else {
        "account"
    };
    redis::cmd("SCAN")
        .arg(cursor)
        .arg("MATCH")
        .arg(format!("{NAMESPACE}:lease:{scope}:*:active"))
        .arg("COUNT")
        .arg(100)
        .query_async(connection)
        .await
        .map_err(|source| redis_unavailable("scan stopped owner slots", source))
}

async fn release_account(
    connection: &mut ConnectionManager,
    key: &str,
    requests: &BTreeMap<String, String>,
) -> StoreResult<u64> {
    let approved = serde_json::to_string(requests).map_err(|_| invalid())?;
    redis::Script::new(RELEASE_ACCOUNT)
        .key(key)
        .arg(approved)
        .invoke_async(connection)
        .await
        .map_err(|source| redis_unavailable("release stopped account slots", source))
}

async fn release_client(
    connection: &mut ConnectionManager,
    client: &str,
    requests: &[String],
) -> StoreResult<u64> {
    let tag = super::resource_fingerprint("client admission", client)?;
    let key = format!("{NAMESPACE}:client:{{{tag}}}:active");
    let approved = serde_json::to_string(requests).map_err(|_| invalid())?;
    redis::Script::new(RELEASE_CLIENT)
        .key(key)
        .arg(approved)
        .invoke_async(connection)
        .await
        .map_err(|source| redis_unavailable("release stopped client slots", source))
}

fn validate_account_key(key: &str) -> StoreResult<()> {
    let fingerprint = ["account", "account-reserved"].iter().find_map(|scope| {
        key.strip_prefix(&format!("{NAMESPACE}:lease:{scope}:{{"))
            .and_then(|tail| tail.strip_suffix("}:active"))
    });
    if !fingerprint
        .is_some_and(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_requests(requests: &BTreeMap<String, String>) -> StoreResult<()> {
    if requests.is_empty() || requests.len() > MAX_REQUESTS {
        return Err(invalid());
    }
    for (request, client) in requests {
        validate_id(request)?;
        validate_id(client)?;
    }
    Ok(())
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
    refused("invalid recovery command")
}

fn refused(message: &str) -> StoreError {
    StoreError::InvalidData {
        entity: "single-deployment recovery",
        message: message.to_owned(),
        source: None,
    }
}
