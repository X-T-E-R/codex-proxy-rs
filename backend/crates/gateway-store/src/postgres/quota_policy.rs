//! 额度策略、跨实例重置互斥和发送前预算的 PostgreSQL 适配。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::provider_ports::quota_policy::*;
use sqlx::{PgConnection, PgPool, Row};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub struct PgQuotaPolicyStore {
    pool: PgPool,
    reset_slots: Arc<Semaphore>,
}

impl PgQuotaPolicyStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            reset_slots: Arc::new(Semaphore::new(2)),
        }
    }

    async fn supported(&self, id: &str) -> Result<(), QuotaPolicyError> {
        let supported: bool = sqlx::query_scalar("select exists(select 1 from provider_accounts where id=$1 and provider_kind='openai' and authentication_kind='oauth')")
            .bind(id).fetch_one(&self.pool).await.map_err(unavailable)?;
        if supported {
            Ok(())
        } else {
            Err(QuotaPolicyError::NotFound)
        }
    }
}

fn unavailable(_: sqlx::Error) -> QuotaPolicyError {
    QuotaPolicyError::Unavailable
}
fn decode(value: serde_json::Value) -> Result<QuotaPolicy, QuotaPolicyError> {
    let policy: QuotaPolicy =
        serde_json::from_value(value).map_err(|_| QuotaPolicyError::Invalid)?;
    if policy.valid() {
        Ok(policy)
    } else {
        Err(QuotaPolicyError::Invalid)
    }
}
fn mode_text(mode: QuotaPolicyMode) -> &'static str {
    match mode {
        QuotaPolicyMode::Inherit => "inherit",
        QuotaPolicyMode::Disabled => "disabled",
        QuotaPolicyMode::Custom => "custom",
    }
}

struct PgResetGuard {
    connection: PgConnection,
    _slot: OwnedSemaphorePermit,
}

#[async_trait]
impl ResetOperationGuard for PgResetGuard {
    async fn authorize(&mut self, claim: &ResetClaim) -> Result<(), QuotaPolicyError> {
        // 发送期间保持当前凭据/配置的共享行锁；不允许更新越过消费准备边界。
        sqlx::query("begin")
            .execute(&mut self.connection)
            .await
            .map_err(unavailable)?;
        let account = sqlx::query("select credential_revision, enabled, credential_state from provider_accounts where id=$1 for share")
            .bind(&claim.operation.account_id).fetch_optional(&mut self.connection).await.map_err(unavailable)?.ok_or(QuotaPolicyError::NotFound)?;
        if account.get::<i64, _>("credential_revision") as u64
            != claim.operation.credential_revision
        {
            return Err(QuotaPolicyError::Conflict);
        }
        if claim.operation.automatic {
            if !account.get::<bool, _>("enabled")
                || account.get::<String, _>("credential_state") != "ready"
            {
                return Err(QuotaPolicyError::Conflict);
            }
            let global = sqlx::query(
                "select revision,policy_json from codex_quota_policy_global where id=1 for share",
            )
            .fetch_one(&mut self.connection)
            .await
            .map_err(unavailable)?;
            let configured = sqlx::query("select revision,mode,policy_json from codex_quota_policy_accounts where account_id=$1 for share")
                .bind(&claim.operation.account_id).fetch_one(&mut self.connection).await.map_err(unavailable)?;
            if global.get::<i64, _>("revision") as u64 != claim.global_revision
                || configured.get::<i64, _>("revision") as u64 != claim.account_revision
            {
                return Err(QuotaPolicyError::Conflict);
            }
            let policy = match configured.get::<&str, _>("mode") {
                "inherit" => decode(global.get("policy_json"))?,
                "custom" => decode(configured.get("policy_json"))?,
                _ => return Err(QuotaPolicyError::Conflict),
            };
            let action = match claim.operation.episode.as_deref() {
                Some(episode) if episode.starts_with("threshold_primary:") => policy.primary.action,
                Some(episode) if episode.starts_with("threshold_secondary:") => {
                    policy.secondary.action
                }
                _ => return Err(QuotaPolicyError::Conflict),
            };
            if action != QuotaAction::ResetThenStop {
                return Err(QuotaPolicyError::Conflict);
            }
        }
        Ok(())
    }

    async fn valid(&mut self) -> Result<(), QuotaPolicyError> {
        sqlx::query("select 1")
            .execute(&mut self.connection)
            .await
            .map_err(unavailable)?;
        Ok(())
    }
}

#[async_trait]
impl QuotaPolicyStore for PgQuotaPolicyStore {
    async fn global(&self) -> Result<GlobalQuotaPolicy, QuotaPolicyError> {
        let row =
            sqlx::query("select revision, policy_json from codex_quota_policy_global where id=1")
                .fetch_one(&self.pool)
                .await
                .map_err(unavailable)?;
        Ok(GlobalQuotaPolicy {
            revision: row.get::<i64, _>("revision") as u64,
            policy: decode(row.get("policy_json"))?,
        })
    }

    async fn update_global(
        &self,
        expected: u64,
        policy: QuotaPolicy,
        audit: &str,
    ) -> Result<GlobalQuotaPolicy, QuotaPolicyError> {
        if !policy.valid() {
            return Err(QuotaPolicyError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let revision: Option<i64> = sqlx::query_scalar("update codex_quota_policy_global set revision=revision+1,policy_json=$2,updated_at=now() where id=1 and revision=$1 returning revision")
            .bind(expected as i64).bind(sqlx::types::Json(&policy)).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        let revision = revision.ok_or(QuotaPolicyError::Conflict)?;
        sqlx::query("insert into codex_quota_policy_audit(revision,actor_ref) values($1,$2)")
            .bind(revision)
            .bind(audit)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(GlobalQuotaPolicy {
            revision: revision as u64,
            policy,
        })
    }

    async fn account(&self, id: &str) -> Result<AccountQuotaPolicy, QuotaPolicyError> {
        self.supported(id).await?;
        let global = self.global().await?;
        let row = sqlx::query("select revision,mode,policy_json,paused,reason,observed_at from codex_quota_policy_accounts where account_id=$1")
            .bind(id).fetch_optional(&self.pool).await.map_err(unavailable)?;
        let mut view = AccountQuotaPolicy {
            account_id: id.to_owned(),
            revision: 0,
            mode: QuotaPolicyMode::Inherit,
            policy: None,
            effective_policy: global.policy,
            source: "global".to_owned(),
            status: QuotaPolicyStatus {
                reason: "off".to_owned(),
                ..Default::default()
            },
        };
        if let Some(row) = row {
            view.revision = row.get::<i64, _>("revision") as u64;
            view.mode = match row.get::<&str, _>("mode") {
                "inherit" => QuotaPolicyMode::Inherit,
                "disabled" => QuotaPolicyMode::Disabled,
                "custom" => QuotaPolicyMode::Custom,
                _ => return Err(QuotaPolicyError::Invalid),
            };
            view.policy = row
                .get::<Option<serde_json::Value>, _>("policy_json")
                .map(decode)
                .transpose()?;
            view.status.paused = row.get("paused");
            view.status.reason = row.get("reason");
            view.status.observed_at = row.get("observed_at");
        }
        match view.mode {
            QuotaPolicyMode::Inherit => {}
            QuotaPolicyMode::Disabled => {
                view.effective_policy = QuotaPolicy::default();
                view.source = "disabled".to_owned();
            }
            QuotaPolicyMode::Custom => {
                view.effective_policy = view.policy.clone().ok_or(QuotaPolicyError::Invalid)?;
                view.source = "account".to_owned();
            }
        }
        let budget = sqlx::query("select count(*) filter(where sent_at > now()-interval '24 hours') as count, max(sent_at) as last from codex_reset_operations where account_id=$1 and automatic")
            .bind(id).fetch_one(&self.pool).await.map_err(unavailable)?;
        view.status.auto_attempts_last24h = budget.get::<i64, _>("count") as u64;
        view.status.last_auto_attempt_at = budget.get("last");
        view.status.pending_operation_id = self.pending_reset(id).await?.map(|op| op.id);
        Ok(view)
    }

    async fn update_account(
        &self,
        id: &str,
        expected: u64,
        mode: QuotaPolicyMode,
        policy: Option<QuotaPolicy>,
        audit: &str,
    ) -> Result<AccountQuotaPolicy, QuotaPolicyError> {
        self.supported(id).await?;
        if (mode == QuotaPolicyMode::Custom) != policy.is_some()
            || policy.as_ref().is_some_and(|p| !p.valid())
        {
            return Err(QuotaPolicyError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        sqlx::query(
            "insert into codex_quota_policy_accounts(account_id) values($1) on conflict do nothing",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        let revision: Option<i64> = sqlx::query_scalar("update codex_quota_policy_accounts set revision=revision+1,mode=$3,policy_json=$4,updated_at=now() where account_id=$1 and revision=$2 returning revision")
            .bind(id).bind(expected as i64).bind(mode_text(mode)).bind(policy.map(sqlx::types::Json)).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        let revision = revision.ok_or(QuotaPolicyError::Conflict)?;
        sqlx::query(
            "insert into codex_quota_policy_audit(account_id,revision,actor_ref) values($1,$2,$3)",
        )
        .bind(id)
        .bind(revision)
        .bind(audit)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        self.account(id).await
    }

    async fn status(
        &self,
        id: &str,
        paused: bool,
        reason: &str,
        observed_at: Option<DateTime<Utc>>,
    ) -> Result<(), QuotaPolicyError> {
        sqlx::query("insert into codex_quota_policy_accounts(account_id,paused,reason,observed_at) values($1,$2,$3,$4) on conflict(account_id) do update set paused=$2,reason=$3,observed_at=$4")
            .bind(id).bind(paused).bind(reason).bind(observed_at).execute(&self.pool).await.map_err(unavailable)?;
        Ok(())
    }

    async fn lock_reset(&self, id: &str) -> Result<Box<dyn ResetOperationGuard>, QuotaPolicyError> {
        let slot = self
            .reset_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| QuotaPolicyError::Conflict)?;
        // 与既有代理导入保护相同：detach 防止仍持锁的连接返回共享池。
        let mut connection = self.pool.acquire().await.map_err(unavailable)?.detach();
        let acquired: bool =
            sqlx::query_scalar("select pg_try_advisory_lock(hashtextextended($1,739220))")
                .bind(id)
                .fetch_one(&mut connection)
                .await
                .map_err(unavailable)?;
        if !acquired {
            return Err(QuotaPolicyError::Conflict);
        }
        Ok(Box::new(PgResetGuard {
            connection,
            _slot: slot,
        }))
    }

    async fn claim_reset(&self, claim: ResetClaim) -> Result<ResetOperation, QuotaPolicyError> {
        let op = claim.operation;
        let id = Uuid::parse_str(&op.id).map_err(|_| QuotaPolicyError::Invalid)?;
        if id.get_version_num() != 4 || id.to_string() != op.id {
            return Err(QuotaPolicyError::Invalid);
        }
        // 已确认手动命令是只读事实，不需要当前凭据授权，也不迁移历史 revision。
        if let Some(existing) = self.reset_operation(&op.id).await?
            && !existing.automatic
            && existing.state != "pending"
        {
            if existing.account_id != op.account_id
                || existing.credit_id != op.credit_id
                || op.automatic
            {
                return Err(QuotaPolicyError::Conflict);
            }
            return Ok(existing);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        // 账号行锁串行化所有预算 claim，即使调用方遗漏进程内锁也不能超额。
        let row = sqlx::query("select credential_revision,enabled,credential_state from provider_accounts where id=$1 for update")
            .bind(&op.account_id).fetch_optional(&mut *tx).await.map_err(unavailable)?.ok_or(QuotaPolicyError::NotFound)?;
        if row.get::<i64, _>("credential_revision") as u64 != op.credential_revision {
            return Err(QuotaPolicyError::Conflict);
        }
        if let Some(existing) = sqlx::query("select id,account_id,credential_revision,credit_id,automatic,episode,trigger_percent,state,result_code from codex_reset_operations where id=$1")
            .bind(id.to_string()).fetch_optional(&mut *tx).await.map_err(unavailable)? {
            let mut existing = operation(existing);
            if existing.account_id != op.account_id || existing.credit_id != op.credit_id || existing.automatic != op.automatic { return Err(QuotaPolicyError::Conflict); }
            if existing.credential_revision != op.credential_revision {
                // 仅明确的首次手动 401 可在令牌刷新后继续同键；未知结果不得越过凭据代次。
                if existing.automatic || existing.state != "pending" || existing.result_code.as_deref() != Some("credential_refresh_required") {
                    return Err(QuotaPolicyError::Conflict);
                }
                sqlx::query("update codex_reset_operations set credential_revision=$2,updated_at=now() where id=$1")
                    .bind(&existing.id).bind(op.credential_revision as i64).execute(&mut *tx).await.map_err(unavailable)?;
                existing.credential_revision = op.credential_revision;
                tx.commit().await.map_err(unavailable)?;
            }
            return Ok(existing);
        }
        let pending: bool = sqlx::query_scalar("select exists(select 1 from codex_reset_operations where account_id=$1 and (state='pending' or needs_readback))")
            .bind(&op.account_id).fetch_one(&mut *tx).await.map_err(unavailable)?;
        if pending {
            return Err(QuotaPolicyError::Pending);
        }
        if op.automatic {
            if !row.get::<bool, _>("enabled") || row.get::<String, _>("credential_state") != "ready"
            {
                return Err(QuotaPolicyError::Conflict);
            }
            let global = sqlx::query(
                "select revision,policy_json from codex_quota_policy_global where id=1 for share",
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
            sqlx::query("insert into codex_quota_policy_accounts(account_id) values($1) on conflict do nothing").bind(&op.account_id).execute(&mut *tx).await.map_err(unavailable)?;
            let account = sqlx::query("select revision,mode,policy_json from codex_quota_policy_accounts where account_id=$1 for share").bind(&op.account_id).fetch_one(&mut *tx).await.map_err(unavailable)?;
            if global.get::<i64, _>("revision") as u64 != claim.global_revision
                || account.get::<i64, _>("revision") as u64 != claim.account_revision
            {
                return Err(QuotaPolicyError::Conflict);
            }
            let policy = match account.get::<&str, _>("mode") {
                "inherit" => decode(global.get("policy_json"))?,
                "custom" => decode(account.get("policy_json"))?,
                _ => return Err(QuotaPolicyError::Conflict),
            };
            if policy.primary.action != QuotaAction::ResetThenStop
                && policy.secondary.action != QuotaAction::ResetThenStop
            {
                return Err(QuotaPolicyError::Conflict);
            }
            let episode: bool = sqlx::query_scalar("select exists(select 1 from codex_reset_operations where account_id=$1 and automatic and episode=$2)").bind(&op.account_id).bind(&op.episode).fetch_one(&mut *tx).await.map_err(unavailable)?;
            if episode {
                return Err(QuotaPolicyError::Episode);
            }
            let budget = sqlx::query("select count(*) filter(where sent_at > now()-interval '24 hours') as count, max(sent_at) as last, now() as database_now from codex_reset_operations where account_id=$1 and automatic").bind(&op.account_id).fetch_one(&mut *tx).await.map_err(unavailable)?;
            if budget.get::<i64, _>("count") >= i64::from(policy.auto_reset.max_attempts_per24h) {
                return Err(QuotaPolicyError::Budget);
            }
            if budget
                .get::<Option<DateTime<Utc>>, _>("last")
                .is_some_and(|at| {
                    budget
                        .get::<DateTime<Utc>, _>("database_now")
                        .signed_duration_since(at)
                        .num_seconds()
                        < i64::from(policy.auto_reset.cooldown_seconds)
                })
            {
                return Err(QuotaPolicyError::Cooldown);
            }
        }
        sqlx::query("insert into codex_reset_operations(id,account_id,credential_revision,credit_id,automatic,episode,trigger_percent,state) values($1,$2,$3,$4,$5,$6,$7,'pending')")
            .bind(id.to_string()).bind(&op.account_id).bind(op.credential_revision as i64).bind(&op.credit_id).bind(op.automatic).bind(&op.episode).bind(op.trigger_percent.map(i16::from)).execute(&mut *tx).await.map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(op)
    }

    async fn finish_reset(
        &self,
        id: &str,
        state: &str,
        code: Option<&str>,
    ) -> Result<(), QuotaPolicyError> {
        if !matches!(state, "pending" | "confirmed" | "failed") {
            return Err(QuotaPolicyError::Invalid);
        }
        let id = Uuid::parse_str(id).map_err(|_| QuotaPolicyError::Invalid)?;
        // 结果和回读屏障同事务落盘，崩溃不能跳过“成功但额度仍高”的防连耗状态。
        sqlx::query("with changed as (
            update codex_reset_operations set state=$2,result_code=$3,needs_readback=(automatic and $2='confirmed'),updated_at=now()
            where id=$1 and state='pending' returning account_id,automatic
        ) insert into codex_quota_policy_accounts(account_id,paused,reason)
          select account_id,true,case when $2='confirmed' then 'reset_readback_unconfirmed' else 'reset_failed' end
          from changed where automatic and $2 <> 'pending'
          on conflict(account_id) do update set paused=true,reason=excluded.reason")
            .bind(id.to_string()).bind(state).bind(code).execute(&self.pool).await.map_err(unavailable)?;
        Ok(())
    }

    async fn reset_operation(&self, id: &str) -> Result<Option<ResetOperation>, QuotaPolicyError> {
        sqlx::query("select id,account_id,credential_revision,credit_id,automatic,episode,trigger_percent,state,result_code from codex_reset_operations where id=$1")
            .bind(id).fetch_optional(&self.pool).await.map(|row| row.map(operation)).map_err(unavailable)
    }

    async fn pending_reset(&self, id: &str) -> Result<Option<ResetOperation>, QuotaPolicyError> {
        sqlx::query("select id,account_id,credential_revision,credit_id,automatic,episode,trigger_percent,state,result_code from codex_reset_operations where account_id=$1 and (state='pending' or needs_readback)")
            .bind(id).fetch_optional(&self.pool).await.map(|row| row.map(operation)).map_err(unavailable)
    }

    async fn confirm_readback(
        &self,
        id: &str,
        global_revision: u64,
        account_revision: u64,
    ) -> Result<(), QuotaPolicyError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        // 与配置写入在同一行锁边界串行；不能把旧策略判断提交为新策略的恢复证明。
        let global: i64 = sqlx::query_scalar(
            "select revision from codex_quota_policy_global where id=1 for share",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        let account: Option<i64> = sqlx::query_scalar("select revision from codex_quota_policy_accounts where account_id=(select account_id from codex_reset_operations where id=$1) for share")
            .bind(id).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        if global as u64 != global_revision
            || account.map(|revision| revision as u64) != Some(account_revision)
        {
            return Err(QuotaPolicyError::Conflict);
        }
        let changed = sqlx::query("update codex_reset_operations set needs_readback=false,updated_at=now() where id=$1 and state='confirmed' and needs_readback")
            .bind(id).execute(&mut *tx).await.map_err(unavailable)?.rows_affected();
        if changed != 1 {
            return Err(QuotaPolicyError::Conflict);
        }
        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }
}

fn operation(row: sqlx::postgres::PgRow) -> ResetOperation {
    ResetOperation {
        id: row.get("id"),
        account_id: row.get("account_id"),
        credential_revision: row.get::<i64, _>("credential_revision") as u64,
        credit_id: row.get("credit_id"),
        automatic: row.get("automatic"),
        episode: row.get("episode"),
        trigger_percent: row
            .get::<Option<i16>, _>("trigger_percent")
            .map(|value| value as u8),
        state: row.get("state"),
        result_code: row.get("result_code"),
    }
}
