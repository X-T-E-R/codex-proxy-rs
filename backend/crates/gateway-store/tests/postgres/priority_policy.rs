//! 快速模式实际持久化、凭据隔离和重置自动关闭的 PostgreSQL 合同

use super::TestDatabase;
use chrono::{Duration, Utc};
use gateway_core::{account::FastMode, provider_ports::priority_policy::*};
use gateway_store::postgres::PgPriorityPolicyStore;

#[tokio::test]
async fn global_account_cas_reset_auto_off_and_other_account_isolation() {
    let Some(db) = TestDatabase::create("priority_policy").await else {
        return;
    };
    for id in ["acct_priority", "acct_other"] {
        sqlx::query("insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at) values($1,'openai',$1,'oauth','{}',1,false,true,'ready',now(),now(),now())").bind(id).execute(&db.pool).await.unwrap();
    }
    let store = PgPriorityPolicyStore::new(db.pool.clone());
    store
        .update_global(0, FastMode::Enabled, false, "test")
        .await
        .unwrap();
    assert_eq!(
        store
            .resolve("acct_priority", FastMode::Default)
            .await
            .unwrap(),
        FastMode::Enabled
    );
    let configured = store
        .update_account(
            "acct_priority",
            0,
            AccountPriorityMode::Inherit,
            Some(true),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(configured.status.reason, "awaiting_baseline");
    assert_eq!(
        store
            .update_account(
                "acct_priority",
                0,
                AccountPriorityMode::Disabled,
                Some(false),
                "test"
            )
            .await
            .unwrap_err(),
        PriorityPolicyError::Conflict
    );
    let now = Utc::now() + Duration::seconds(1);
    let baseline = PriorityQuotaObservation {
        credential_revision: 1,
        observed_at: now,
        windows: vec![PriorityResetWindow {
            key: "codex:18000".to_owned(),
            reset_at: now + Duration::hours(1),
            used_percent: 90.0,
        }],
    };
    store
        .observe("acct_priority", baseline.clone())
        .await
        .unwrap();
    assert!(store.account("acct_priority").await.unwrap().status.armed);
    let mut reset = baseline.clone();
    reset.observed_at += Duration::seconds(3);
    reset.windows[0].reset_at += Duration::hours(5);
    reset.windows[0].used_percent = 0.0;
    reset.credential_revision = 2;
    store.observe("acct_priority", reset.clone()).await.unwrap();
    assert_eq!(
        store
            .resolve("acct_priority", FastMode::Default)
            .await
            .unwrap(),
        FastMode::Enabled
    );
    reset.credential_revision = 1;
    store.observe("acct_priority", reset.clone()).await.unwrap();
    store.observe("acct_priority", reset).await.unwrap();
    let saved = PgPriorityPolicyStore::new(db.pool.clone())
        .account("acct_priority")
        .await
        .unwrap();
    assert_eq!(saved.mode, AccountPriorityMode::Disabled);
    assert_eq!(saved.status.reason, "quota_reset");
    assert!(saved.status.last_auto_off_at.is_some());
    assert_eq!(
        store
            .resolve("acct_other", FastMode::Default)
            .await
            .unwrap(),
        FastMode::Enabled
    );
    let count: i64 = sqlx::query_scalar(
        "select count(*) from codex_priority_audit where actor_ref='system:quota_reset'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    db.close().await;
}

#[tokio::test]
async fn global_auto_off_inheritance_overrides_and_reconfiguration_are_per_account() {
    let Some(db) = TestDatabase::create("priority_global_auto_off").await else {
        return;
    };
    for id in ["inherited", "other", "off", "on"] {
        sqlx::query("insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at) values($1,'openai',$1,'oauth','{}',1,false,true,'ready',now(),now(),now())")
            .bind(id).execute(&db.pool).await.unwrap();
    }
    let store = PgPriorityPolicyStore::new(db.pool.clone());
    assert!(!store.global().await.unwrap().auto_off_on_quota_reset);
    assert_eq!(
        store
            .account("inherited")
            .await
            .unwrap()
            .auto_off_on_quota_reset,
        None
    );
    store
        .update_account("off", 0, AccountPriorityMode::Inherit, Some(false), "test")
        .await
        .unwrap();
    store
        .update_account("on", 0, AccountPriorityMode::Enabled, Some(true), "test")
        .await
        .unwrap();
    store
        .update_global(0, FastMode::Enabled, true, "test")
        .await
        .unwrap();
    assert_eq!(
        store
            .update_global(0, FastMode::Enabled, false, "test")
            .await
            .unwrap_err(),
        PriorityPolicyError::Conflict
    );
    let inherited = store.account("inherited").await.unwrap();
    assert!(inherited.effective_auto_off_on_quota_reset);
    assert_eq!(inherited.status.reason, "awaiting_baseline");
    assert!(
        !store
            .account("off")
            .await
            .unwrap()
            .effective_auto_off_on_quota_reset
    );
    let now = Utc::now() + Duration::seconds(1);
    let baseline = PriorityQuotaObservation {
        credential_revision: 1,
        observed_at: now,
        windows: vec![PriorityResetWindow {
            key: "codex:18000".into(),
            reset_at: now + Duration::hours(1),
            used_percent: 95.0,
        }],
    };
    for id in ["inherited", "other", "off", "on"] {
        store.observe(id, baseline.clone()).await.unwrap();
    }
    let mut reset = baseline.clone();
    reset.observed_at += Duration::seconds(1);
    reset.windows[0].reset_at += Duration::hours(5);
    reset.windows[0].used_percent = 7.0;
    store.observe("inherited", reset.clone()).await.unwrap();
    store.observe("off", reset.clone()).await.unwrap();
    assert_eq!(
        store.account("inherited").await.unwrap().mode,
        AccountPriorityMode::Disabled
    );
    assert_eq!(
        store.resolve("other", FastMode::Default).await.unwrap(),
        FastMode::Enabled
    );
    assert_eq!(
        store.resolve("off", FastMode::Default).await.unwrap(),
        FastMode::Enabled
    );
    assert_eq!(
        store.resolve("other", FastMode::Disabled).await.unwrap(),
        FastMode::Disabled
    );
    // 全局保存后不复用旧基线，且不撤销已自动关闭的账号覆盖
    store
        .update_global(1, FastMode::Enabled, false, "test")
        .await
        .unwrap();
    assert_eq!(
        store.account("inherited").await.unwrap().mode,
        AccountPriorityMode::Disabled
    );
    assert!(
        store
            .account("on")
            .await
            .unwrap()
            .effective_auto_off_on_quota_reset
    );
    store.observe("on", reset.clone()).await.unwrap();
    assert_eq!(
        store.account("on").await.unwrap().mode,
        AccountPriorityMode::Enabled
    );
    let mut next = reset.clone();
    next.observed_at += Duration::seconds(1);
    next.windows[0].used_percent = 80.0;
    store.observe("on", next.clone()).await.unwrap();
    next.observed_at += Duration::seconds(1);
    next.windows[0].reset_at += Duration::hours(5);
    next.windows[0].used_percent = 0.0;
    store.observe("on", next).await.unwrap();
    assert_eq!(
        store.account("on").await.unwrap().mode,
        AccountPriorityMode::Disabled
    );
    let operations: i64 = sqlx::query_scalar("select count(*) from codex_reset_operations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        operations, 0,
        "automatic priority disable never consumes a reset card"
    );
    db.close().await;
}
