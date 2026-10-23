//! 专用 PostgreSQL 实际验证，不连接生产；环境未设置的跳过不能作为通过证据。

use super::TestDatabase;
use gateway_core::provider_ports::quota_policy::*;
use gateway_store::postgres::PgQuotaPolicyStore;
use uuid::Uuid;

async fn seed(pool: &sqlx::PgPool, id: &str) {
    sqlx::query("insert into provider_accounts(id,provider_kind,name,authentication_kind,provider_credentials_json,credential_revision,has_refresh_token,enabled,credential_state,credential_observed_at,created_at,updated_at) values($1,'openai',$1,'oauth','{}',1,false,true,'ready',now(),now(),now())")
        .bind(id).execute(pool).await.unwrap();
}
fn claim(account: &str, episode: &str, global_revision: u64) -> ResetClaim {
    ResetClaim {
        operation: ResetOperation {
            id: Uuid::new_v4().to_string(),
            account_id: account.into(),
            credential_revision: 1,
            credit_id: None,
            automatic: true,
            episode: Some(episode.into()),
            trigger_percent: Some(99),
            state: "pending".into(),
            result_code: None,
        },
        global_revision,
        account_revision: 0,
    }
}

#[tokio::test]
async fn policy_migration_roundtrip_account_override_and_cas() {
    let Some(db) = TestDatabase::create("quota_policy_roundtrip").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    let store = PgQuotaPolicyStore::new(db.pool.clone());
    let initial = store.global().await.unwrap();
    assert!(!initial.policy.enabled());
    assert_eq!(initial.revision, 0);
    let mut policy = initial.policy;
    policy.secondary.action = QuotaAction::Stop;
    policy.secondary.threshold_percent = 99;
    let saved = store
        .update_global(0, policy.clone(), "test:global")
        .await
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert!(matches!(
        store.update_global(0, policy.clone(), "test").await,
        Err(QuotaPolicyError::Conflict)
    ));
    let inherited = store.account("acct_policy").await.unwrap();
    assert_eq!(inherited.effective_policy, policy);
    assert_eq!(inherited.mode, QuotaPolicyMode::Inherit);
    let disabled = store
        .update_account(
            "acct_policy",
            0,
            QuotaPolicyMode::Disabled,
            None,
            "test:account",
        )
        .await
        .unwrap();
    assert!(!disabled.effective_policy.enabled());
    let mut custom = QuotaPolicy::default();
    custom.primary.action = QuotaAction::ResetThenStop;
    custom.primary.threshold_percent = 90;
    let saved = store
        .update_account(
            "acct_policy",
            disabled.revision,
            QuotaPolicyMode::Custom,
            Some(custom.clone()),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(saved.source, "account");
    assert_eq!(saved.effective_policy, custom);
    assert!(matches!(
        store
            .update_account("acct_policy", 0, QuotaPolicyMode::Inherit, None, "test")
            .await,
        Err(QuotaPolicyError::Conflict)
    ));
    sqlx::query(
        "update provider_accounts set authentication_kind='api_key' where id='acct_policy'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(matches!(
        store.account("acct_policy").await,
        Err(QuotaPolicyError::NotFound)
    ));
    db.close().await;
}

#[tokio::test]
async fn persistent_unknown_readback_budget_cooldown_and_episode_do_not_reset_on_restart() {
    let Some(db) = TestDatabase::create("quota_policy_budget").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    let store = PgQuotaPolicyStore::new(db.pool.clone());
    let mut policy = QuotaPolicy::default();
    policy.secondary.action = QuotaAction::ResetThenStop;
    policy.secondary.threshold_percent = 99;
    store
        .update_global(0, policy.clone(), "test")
        .await
        .unwrap();
    let first = claim("acct_policy", "threshold_secondary:1800000000", 1);
    let a = store.claim_reset(first.clone()).await.unwrap();
    let restarted = PgQuotaPolicyStore::new(db.pool.clone());
    assert_eq!(
        restarted
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .id,
        a.id
    );
    assert_eq!(
        restarted.claim_reset(first.clone()).await.unwrap().id,
        a.id,
        "same key does not count twice"
    );
    assert_eq!(
        restarted
            .account("acct_policy")
            .await
            .unwrap()
            .status
            .auto_attempts_last24h,
        1
    );
    assert!(matches!(
        restarted
            .claim_reset(claim("acct_policy", "threshold_secondary:1800000001", 1))
            .await,
        Err(QuotaPolicyError::Pending)
    ));
    restarted
        .finish_reset(&a.id, "confirmed", Some("reset"))
        .await
        .unwrap();
    assert_eq!(
        restarted
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .state,
        "confirmed"
    );
    assert!(
        matches!(
            restarted
                .claim_reset(claim("acct_policy", "threshold_primary:1800000002", 1))
                .await,
            Err(QuotaPolicyError::Pending)
        ),
        "confirmed without low readback still blocks any next window"
    );
    restarted
        .status("acct_policy", false, "off", None)
        .await
        .unwrap();
    assert!(
        restarted
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .is_some(),
        "config/status does not clear durable barrier"
    );
    restarted.confirm_readback(&a.id, 1, 0).await.unwrap();
    assert!(matches!(
        restarted
            .claim_reset(claim("acct_policy", "threshold_secondary:1800000001", 1))
            .await,
        Err(QuotaPolicyError::Budget)
    ));
    policy.auto_reset.max_attempts_per24h = 10;
    policy.auto_reset.cooldown_seconds = 3600;
    restarted
        .update_global(1, policy.clone(), "test")
        .await
        .unwrap();
    assert!(matches!(
        restarted
            .claim_reset(claim("acct_policy", "threshold_secondary:1800000001", 2))
            .await,
        Err(QuotaPolicyError::Cooldown)
    ));
    sqlx::query("update codex_reset_operations set sent_at=now()-interval '2 hours'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        restarted
            .claim_reset(claim("acct_policy", "threshold_secondary:1800000000", 2))
            .await,
        Err(QuotaPolicyError::Episode)
    ));
    let b = restarted
        .claim_reset(claim("acct_policy", "threshold_secondary:1800000001", 2))
        .await
        .unwrap();
    restarted
        .finish_reset(&b.id, "failed", Some("no_credits"))
        .await
        .unwrap();
    // 凭据与配置换代不能清自动发送预算。
    sqlx::query("update provider_accounts set credential_revision=2 where id='acct_policy'")
        .execute(&db.pool)
        .await
        .unwrap();
    let view = restarted.account("acct_policy").await.unwrap();
    assert_eq!(view.status.auto_attempts_last24h, 2);
    let mut next = claim("acct_policy", "threshold_secondary:1800000002", 2);
    next.operation.credential_revision = 2;
    assert!(matches!(
        restarted.claim_reset(next).await,
        Err(QuotaPolicyError::Cooldown)
    ));
    db.close().await;
}

#[tokio::test]
async fn account_lock_revision_guard_and_concurrent_budget_claims_are_shared() {
    let Some(db) = TestDatabase::create("quota_policy_lock").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    let store = PgQuotaPolicyStore::new(db.pool.clone());
    let other = PgQuotaPolicyStore::new(db.pool.clone());
    let lock = store.lock_reset("acct_policy").await.unwrap();
    assert!(matches!(
        other.lock_reset("acct_policy").await,
        Err(QuotaPolicyError::Conflict)
    ));
    drop(lock);
    let mut policy = QuotaPolicy::default();
    policy.secondary.action = QuotaAction::ResetThenStop;
    store.update_global(0, policy, "test").await.unwrap();
    let first = claim("acct_policy", "threshold_secondary:1800000000", 1);
    let second = claim("acct_policy", "threshold_secondary:1800000001", 1);
    let (a, b) = tokio::join!(store.claim_reset(first.clone()), other.claim_reset(second));
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "transactional claim only one unresolved account op"
    );
    assert_eq!(
        store
            .account("acct_policy")
            .await
            .unwrap()
            .status
            .auto_attempts_last24h,
        1
    );
    let op = store.pending_reset("acct_policy").await.unwrap().unwrap();
    let mut guard = released_lock(&store, "acct_policy").await;
    let stale = ResetClaim {
        operation: op.clone(),
        global_revision: 0,
        account_revision: 0,
    };
    assert!(matches!(
        guard.authorize(&stale).await,
        Err(QuotaPolicyError::Conflict)
    ));
    drop(guard);
    let mut guard = released_lock(&store, "acct_policy").await;
    let valid = ResetClaim {
        operation: op,
        global_revision: 1,
        account_revision: 0,
    };
    guard.authorize(&valid).await.unwrap();
    guard.valid().await.unwrap();
    drop(guard);
    db.close().await;
}

async fn released_lock(store: &PgQuotaPolicyStore, id: &str) -> Box<dyn ResetOperationGuard> {
    // PgConnection Drop 通过连接关闭释放会话锁，允许本地 TCP 清理的短暂异步延迟。
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match store.lock_reset(id).await {
                Ok(guard) => return guard,
                Err(QuotaPolicyError::Conflict) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(error) => panic!("lock release failed: {error}"),
            }
        }
    })
    .await
    .expect("session lock must release after guard drop")
}

#[tokio::test]
async fn only_explicit_manual_auth_rejection_can_retry_same_key_after_credential_rotation() {
    let Some(db) = TestDatabase::create("quota_policy_manual_revision").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    let store = PgQuotaPolicyStore::new(db.pool.clone());
    let mut first = claim("acct_policy", "unused", 0);
    first.operation.automatic = false;
    first.operation.episode = None;
    first.operation.trigger_percent = None;
    store.claim_reset(first.clone()).await.unwrap();
    sqlx::query("update provider_accounts set credential_revision=2 where id='acct_policy'")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut rotated = first.clone();
    rotated.operation.credential_revision = 2;
    assert!(
        matches!(
            store.claim_reset(rotated.clone()).await,
            Err(QuotaPolicyError::Conflict)
        ),
        "unknown operation cannot move to new credentials"
    );
    store
        .finish_reset(
            &first.operation.id,
            "pending",
            Some("credential_refresh_required"),
        )
        .await
        .unwrap();
    let retry = store.claim_reset(rotated).await.unwrap();
    assert_eq!(retry.id, first.operation.id);
    assert_eq!(retry.credential_revision, 2);
    assert_eq!(retry.state, "pending");
    assert!(matches!(
        store
            .claim_reset(claim("acct_policy", "threshold_secondary:1800000000", 0))
            .await,
        Err(QuotaPolicyError::Conflict)
    ));
    db.close().await;
}

#[tokio::test]
async fn review_r2_terminal_manual_claim_after_rotation_is_read_only_but_unknown_stays_fenced() {
    let Some(db) = TestDatabase::create("quota_policy_terminal").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    seed(&db.pool, "acct_other").await;
    let store = PgQuotaPolicyStore::new(db.pool.clone());
    let mut command = claim("acct_policy", "unused", 0);
    command.operation.automatic = false;
    command.operation.episode = None;
    command.operation.trigger_percent = None;
    command.operation.credit_id = Some("card".into());
    store.claim_reset(command.clone()).await.unwrap();
    store
        .finish_reset(&command.operation.id, "confirmed", Some("reset"))
        .await
        .unwrap();
    sqlx::query("update provider_accounts set credential_revision=2,credential_state='expired' where id='acct_policy'").execute(&db.pool).await.unwrap();
    let mut retry = command.clone();
    retry.operation.credential_revision = 2;
    let terminal = store.claim_reset(retry.clone()).await.unwrap();
    assert_eq!(terminal.state, "confirmed");
    assert_eq!(
        terminal.credential_revision, 1,
        "historical terminal revision is immutable"
    );
    assert_eq!(
        store
            .reset_operation(&terminal.id)
            .await
            .unwrap()
            .unwrap()
            .credential_revision,
        1
    );
    retry.operation.credit_id = Some("different-card".into());
    assert!(matches!(
        store.claim_reset(retry.clone()).await,
        Err(QuotaPolicyError::Conflict)
    ));
    retry.operation = command.operation.clone();
    retry.operation.account_id = "acct_other".into();
    assert!(matches!(
        store.claim_reset(retry).await,
        Err(QuotaPolicyError::Conflict)
    ));
    let mut unknown = command;
    unknown.operation.id = Uuid::new_v4().to_string();
    unknown.operation.credential_revision = 2;
    store.claim_reset(unknown.clone()).await.unwrap();
    sqlx::query("update provider_accounts set credential_revision=3 where id='acct_policy'")
        .execute(&db.pool)
        .await
        .unwrap();
    unknown.operation.credential_revision = 3;
    assert!(matches!(
        store.claim_reset(unknown).await,
        Err(QuotaPolicyError::Conflict)
    ));
    db.close().await;
}

#[tokio::test]
async fn review_r3_readback_cas_locks_config_and_preserves_barrier_on_global_or_account_change() {
    let Some(db) = TestDatabase::create("quota_policy_readback_cas").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    let store = std::sync::Arc::new(PgQuotaPolicyStore::new(db.pool.clone()));
    let mut policy = QuotaPolicy::default();
    policy.secondary.action = QuotaAction::ResetThenStop;
    store
        .update_global(0, policy.clone(), "test")
        .await
        .unwrap();
    let op = store
        .claim_reset(claim("acct_policy", "threshold_secondary:1800000000", 1))
        .await
        .unwrap();
    store
        .finish_reset(&op.id, "confirmed", Some("reset"))
        .await
        .unwrap();
    // 配置写入先持锁：旧回读必须等待提交后看见 revision 冲突，不能同时清屏障。
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("update codex_quota_policy_global set revision=revision+1 where id=1")
        .execute(&mut *tx)
        .await
        .unwrap();
    let other = store.clone();
    let key = op.id.clone();
    let mut confirmation = tokio::spawn(async move { other.confirm_readback(&key, 1, 0).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut confirmation)
            .await
            .is_err(),
        "confirm must wait for configuration transaction"
    );
    tx.commit().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), confirmation)
            .await
            .unwrap()
            .unwrap(),
        Err(QuotaPolicyError::Conflict)
    ));
    assert!(store.pending_reset("acct_policy").await.unwrap().is_some());
    store
        .update_account(
            "acct_policy",
            0,
            QuotaPolicyMode::Custom,
            Some(policy),
            "test",
        )
        .await
        .unwrap();
    assert!(matches!(
        store.confirm_readback(&op.id, 2, 0).await,
        Err(QuotaPolicyError::Conflict)
    ));
    assert!(store.pending_reset("acct_policy").await.unwrap().is_some());
    store.confirm_readback(&op.id, 2, 1).await.unwrap();
    assert!(store.pending_reset("acct_policy").await.unwrap().is_none());
    assert_eq!(
        store
            .account("acct_policy")
            .await
            .unwrap()
            .status
            .auto_attempts_last24h,
        1
    );
    db.close().await;
}

#[tokio::test]
async fn review_r1_pending_guard_rejects_current_original_rule_stop_even_with_other_auto() {
    let Some(db) = TestDatabase::create("quota_policy_pending_action").await else {
        return;
    };
    seed(&db.pool, "acct_policy").await;
    let store = PgQuotaPolicyStore::new(db.pool.clone());
    let mut policy = QuotaPolicy::default();
    policy.secondary.action = QuotaAction::ResetThenStop;
    store
        .update_global(0, policy.clone(), "test")
        .await
        .unwrap();
    let op = store
        .claim_reset(claim("acct_policy", "threshold_secondary:1800000000", 1))
        .await
        .unwrap();
    policy.secondary.action = QuotaAction::Stop;
    policy.primary.action = QuotaAction::ResetThenStop;
    store.update_global(1, policy, "test").await.unwrap();
    let current = ResetClaim {
        operation: op.clone(),
        global_revision: 2,
        account_revision: 0,
    };
    let mut guard = store.lock_reset("acct_policy").await.unwrap();
    assert!(matches!(
        guard.authorize(&current).await,
        Err(QuotaPolicyError::Conflict)
    ));
    drop(guard);
    assert_eq!(
        store
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .id,
        op.id
    );
    assert_eq!(
        store
            .account("acct_policy")
            .await
            .unwrap()
            .status
            .auto_attempts_last24h,
        1
    );
    db.close().await;
}
