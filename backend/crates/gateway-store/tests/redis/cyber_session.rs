//! 验证 Cyber marker 的 Key/线程隔离、NX 与固定 TTL 到期

use gateway_core::{
    policy::ClientApiKeyId,
    provider_ports::{CyberSessionKey, CyberSessionPort, CyberSessionRequest},
};
use gateway_store::redis::RedisCyberSessionRepository;
use redis::aio::ConnectionManager;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn cyber_marker_keeps_original_ttl_and_key_thread_isolation() {
    let Some(url) = crate::support::test_env("CPR_TEST_REDIS_URL") else {
        return;
    };
    let mut connection: ConnectionManager = redis::Client::open(url)
        .unwrap()
        .get_connection_manager()
        .await
        .unwrap();
    let namespace = format!("gateway-store-cyber-test-{}", Uuid::new_v4());
    let repository = RedisCyberSessionRepository::new(connection.clone(), &namespace).unwrap();
    let blocked = key("key-a", "child-a");
    repository
        .record(&blocked, Duration::from_secs(2))
        .await
        .unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{namespace}:*"))
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].contains("child-a"));
    let before: i64 = redis::cmd("PTTL")
        .arg(&keys[0])
        .query_async(&mut connection)
        .await
        .unwrap();
    repository
        .record(&blocked, Duration::from_secs(3600))
        .await
        .unwrap();
    let after: i64 = redis::cmd("PTTL")
        .arg(&keys[0])
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(
        after > 0 && after <= before,
        "NX must retain the first deadline"
    );
    assert!(
        repository
            .contains_any(std::slice::from_ref(&blocked))
            .await
            .unwrap()
    );
    for candidate in [
        key("key-a", "parent"),
        key("key-a", "child-b"),
        key("key-b", "child-a"),
    ] {
        assert!(!repository.contains_any(&[candidate]).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(!repository.contains_any(&[blocked]).await.unwrap());
}

fn key(client: &str, thread: &str) -> CyberSessionKey {
    let digest = match thread {
        "child-a" => [1; 32],
        "child-b" => [2; 32],
        "parent" => [3; 32],
        _ => panic!("unknown synthetic thread"),
    };
    CyberSessionRequest::from_digests(Some(digest), Vec::new())
        .refusal_key(&ClientApiKeyId::new(client).unwrap())
        .unwrap()
}
