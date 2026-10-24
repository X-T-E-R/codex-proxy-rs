//! 停止闸 Redis 定向释放的真实认证、隔离、续作与输入边界

use gateway_store::redis::SiteRecoveryRedis;
use serde_json::json;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn site_recovery_releases_only_confirmed_request_and_client_slots() {
    let Some(url) = crate::support::test_env("CPR_TEST_REDIS_URL") else {
        return;
    };
    let mut connection = redis::Client::open(url)
        .unwrap()
        .get_connection_manager()
        .await
        .unwrap();
    let repository = SiteRecoveryRedis::new(connection.clone());
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let request = format!("req_old_{unique}");
    let other = format!("req_alive_{unique}");
    let client = format!("key_{unique}");
    let fingerprint = hex::encode(Sha256::digest(unique.as_bytes()));
    let active = format!("codex-proxy-rs:lease:account:{{{fingerprint}}}:active");
    let client_tag = hex::encode(Sha256::digest(client.as_bytes()));
    let client_active = format!("codex-proxy-rs:client:{{{client_tag}}}:active");
    let rpm = format!("codex-proxy-rs:client:{{{client_tag}}}:requests");
    for (lease, rid, key) in [
        ("old", request.as_str(), client.as_str()),
        ("alive", other.as_str(), client.as_str()),
        ("wrongkey", request.as_str(), "key_other"),
    ] {
        let _: i64 = redis::cmd("ZADD")
            .arg(&active)
            .arg(9_000_000_000_000_i64)
            .arg(format!("owner|{lease}|1"))
            .query_async(&mut connection)
            .await
            .unwrap();
        let _: i64 = redis::cmd("HSET")
            .arg(format!("{active}:detail:{lease}"))
            .arg("facts")
            .arg(json!({"request_id":rid,"client_key_id":key}).to_string())
            .query_async(&mut connection)
            .await
            .unwrap();
    }
    let _: i64 = redis::cmd("ZADD")
        .arg(&active)
        .arg(9_000_000_000_000_i64)
        .arg("owner|missing|1")
        .query_async(&mut connection)
        .await
        .unwrap();
    for key in [&client_active, &rpm] {
        let _: i64 = redis::cmd("ZADD")
            .arg(key)
            .arg(9_000_000_000_000_i64)
            .arg(&request)
            .arg(9_000_000_000_000_i64)
            .arg(&other)
            .query_async(&mut connection)
            .await
            .unwrap();
    }
    let input =
        json!({"kind":"releaseAccount", "key":active, "requests":{(request.clone()):client}})
            .to_string();
    assert_eq!(repository.execute(input.as_bytes()).await.unwrap(), "1");
    assert_eq!(
        repository.execute(input.as_bytes()).await.unwrap(),
        "0",
        "中断后重复释放幂等"
    );
    let remaining: Vec<String> = redis::cmd("ZRANGE")
        .arg(&active)
        .arg(0)
        .arg(-1)
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 3);
    for lease in ["alive", "wrongkey", "missing"] {
        assert!(remaining.contains(&format!("owner|{lease}|1")));
    }
    let input = json!({"kind":"releaseClient", "client":client, "requests":[request]}).to_string();
    assert_eq!(repository.execute(input.as_bytes()).await.unwrap(), "1");
    assert_eq!(repository.execute(input.as_bytes()).await.unwrap(), "0");
    let survivors: Vec<String> = redis::cmd("ZRANGE")
        .arg(&client_active)
        .arg(0)
        .arg(-1)
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(survivors, vec![other]);
    let rpm_count: u64 = redis::cmd("ZCARD")
        .arg(&rpm)
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(rpm_count, 2, "不清 RPM 历史");
    for invalid in [
        json!({"kind":"releaseAccount","key":rpm,"requests":{"req_x":"key_x"}}),
        json!({"kind":"releaseClient","client":"key_x","requests":[]}),
        json!({"kind":"eval","script":"FLUSHDB"}),
    ] {
        assert!(
            repository
                .execute(invalid.to_string().as_bytes())
                .await
                .is_err()
        );
    }
    let keys = [&active, &client_active, &rpm]
        .into_iter()
        .cloned()
        .chain(["old", "alive", "wrongkey"].map(|lease| format!("{active}:detail:{lease}")))
        .collect::<Vec<_>>();
    let _: i64 = redis::cmd("DEL")
        .arg(keys)
        .query_async(&mut connection)
        .await
        .unwrap();
}

#[tokio::test]
async fn site_recovery_connects_with_normal_config_and_refuses_wrong_credentials() {
    let Some(url) = crate::support::test_env("CPR_TEST_REDIS_URL") else {
        return;
    };
    let redis_config = crate::support::config_connection(url::Url::parse(&url).unwrap());
    let config = json!({"database":{"url":"postgres://unused@127.0.0.1/unused","password":"unused_database_password"},
        "redis":redis_config});
    let mut valid: gateway_store::StoreConfig = serde_json::from_value(config.clone()).unwrap();
    valid
        .resolve_and_validate(std::path::Path::new("test-recovery-data"))
        .unwrap();
    let input = br#"{"kind":"scan","cursor":0,"reserved":false}"#;
    let response = gateway_store::redis::site_recovery_redis(&valid, input)
        .await
        .unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&response)
            .unwrap()
            .is_array()
    );
    let mut wrong = config;
    wrong["redis"]["password"] = json!("wrong_recovery_password");
    let mut wrong: gateway_store::StoreConfig = serde_json::from_value(wrong).unwrap();
    wrong
        .resolve_and_validate(std::path::Path::new("test-recovery-data"))
        .unwrap();
    let error = gateway_store::redis::site_recovery_redis(&wrong, input)
        .await
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("wrong_recovery_password"));
}
