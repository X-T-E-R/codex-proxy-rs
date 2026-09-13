use std::time::Duration;

use gateway_core::policy::{ClientApiKeyId, CyberSessionPort, CyberSessionRequest};
use gateway_store::redis::RedisCyberSessionRepository;
use redis::aio::ConnectionManager;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn cyber_session_record_uses_fixed_ttl_and_expires() {
    let Some((repository, mut connection, namespace)) = repository().await else {
        return;
    };
    let key = explicit_key("key-a", "session-a");
    repository
        .record(&key, Duration::from_secs(3))
        .await
        .expect("record block");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    repository
        .record(&key, Duration::from_secs(30))
        .await
        .expect("repeat block");

    assert!(
        repository
            .contains_any(std::slice::from_ref(&key))
            .await
            .unwrap()
    );
    let keys = namespace_keys(&mut connection, &namespace).await;
    assert_eq!(keys.len(), 1);
    let ttl = redis::cmd("PTTL")
        .arg(&keys[0])
        .query_async::<i64>(&mut connection)
        .await
        .expect("read TTL");
    assert!(
        (1..=2_500).contains(&ttl),
        "repeated write refreshed TTL: {ttl}"
    );

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert!(!repository.contains_any(&[key]).await.unwrap());
}

#[tokio::test]
async fn cyber_session_lookup_is_key_and_session_isolated() {
    let Some((repository, _connection, _namespace)) = repository().await else {
        return;
    };
    let blocked = explicit_key("key-a", "session-a");
    repository
        .record(&blocked, Duration::from_secs(30))
        .await
        .expect("record block");

    assert!(repository.contains_any(&[blocked]).await.unwrap());
    assert!(
        !repository
            .contains_any(&[explicit_key("key-a", "session-b")])
            .await
            .unwrap()
    );
    assert!(
        !repository
            .contains_any(&[explicit_key("key-b", "session-a")])
            .await
            .unwrap()
    );
}

fn explicit_key(client_key: &str, session: &str) -> gateway_core::policy::CyberSessionKey {
    let Value::Object(body) = json!({"client_metadata":{"session_id":session},"input":"hello"})
    else {
        unreachable!()
    };
    CyberSessionRequest::from_responses(&body, None, std::iter::empty())
        .refusal_key(&ClientApiKeyId::new(client_key).expect("client key"))
        .expect("explicit key")
}

async fn repository() -> Option<(RedisCyberSessionRepository, ConnectionManager, String)> {
    let redis_url = crate::support::test_env("CPR_TEST_REDIS_URL")?;
    let client = redis::Client::open(redis_url).expect("valid CPR_TEST_REDIS_URL");
    let connection = client
        .get_connection_manager()
        .await
        .expect("connect test Redis");
    let namespace = format!("gateway-store-cyber-session-test-{}", Uuid::new_v4());
    let repository =
        RedisCyberSessionRepository::new(connection.clone(), &namespace).expect("valid namespace");
    Some((repository, connection, namespace))
}

async fn namespace_keys(connection: &mut ConnectionManager, namespace: &str) -> Vec<String> {
    redis::cmd("KEYS")
        .arg(format!("{namespace}:*"))
        .query_async(connection)
        .await
        .expect("list isolated keys")
}
