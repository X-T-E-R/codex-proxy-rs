//! 快速模式配置、账号配额代次 CAS 与系统自动关闭审计的 PostgreSQL 适配

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::{account::FastMode, provider_ports::priority_policy::*};
use sqlx::{PgPool, Row};

pub struct PgPriorityPolicyStore {
    pool: PgPool,
}

impl PgPriorityPolicyStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn unavailable(_: sqlx::Error) -> PriorityPolicyError {
    PriorityPolicyError::Unavailable
}

fn mode(value: &str) -> Result<AccountPriorityMode, PriorityPolicyError> {
    match value {
        "inherit" => Ok(AccountPriorityMode::Inherit),
        "enabled" => Ok(AccountPriorityMode::Enabled),
        "disabled" => Ok(AccountPriorityMode::Disabled),
        _ => Err(PriorityPolicyError::Unavailable),
    }
}

#[async_trait]
impl PriorityPolicyStore for PgPriorityPolicyStore {
    async fn global(&self) -> Result<GlobalPriorityPolicy, PriorityPolicyError> {
        let row = sqlx::query(
            "select revision,mode,auto_off_on_quota_reset from codex_priority_global where id=1",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(GlobalPriorityPolicy {
            revision: row.get::<i64, _>("revision") as u64,
            mode: FastMode::parse(row.get("mode")).ok_or(PriorityPolicyError::Unavailable)?,
            auto_off_on_quota_reset: row.get("auto_off_on_quota_reset"),
        })
    }

    async fn update_global(
        &self,
        expected: u64,
        mode: FastMode,
        auto_off: bool,
        actor: &str,
    ) -> Result<GlobalPriorityPolicy, PriorityPolicyError> {
        let expected = i64::try_from(expected).map_err(|_| PriorityPolicyError::Conflict)?;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row = sqlx::query("update codex_priority_global set revision=revision+1,mode=$2,auto_off_on_quota_reset=$3,updated_at=clock_timestamp() where id=1 and revision=$1 returning revision")
            .bind(expected).bind(mode.as_str()).bind(auto_off).fetch_optional(&mut *tx).await.map_err(unavailable)?.ok_or(PriorityPolicyError::Conflict)?;
        let revision: i64 = row.get("revision");
        sqlx::query(
            "insert into codex_priority_audit(revision,actor_ref,details) values($1,$2,$3)",
        )
        .bind(revision)
        .bind(actor)
        .bind(serde_json::json!({"mode":mode,"autoOffOnQuotaReset":auto_off}))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(GlobalPriorityPolicy {
            revision: revision as u64,
            mode,
            auto_off_on_quota_reset: auto_off,
        })
    }

    async fn account(&self, id: &str) -> Result<AccountPriorityPolicy, PriorityPolicyError> {
        let row = sqlx::query("select g.mode global_mode,g.revision global_revision,g.auto_off_on_quota_reset global_auto_off,p.revision,p.mode,p.auto_off_on_quota_reset,p.baseline,p.baseline_global_revision,p.last_auto_off_at,p.reason from provider_accounts a cross join codex_priority_global g left join codex_priority_accounts p on p.account_id=a.id where a.id=$1 and a.provider_kind='openai' and a.authentication_kind='oauth' and g.id=1")
            .bind(id).fetch_optional(&self.pool).await.map_err(unavailable)?.ok_or(PriorityPolicyError::NotFound)?;
        let global_revision: i64 = row.get("global_revision");
        let global = GlobalPriorityPolicy {
            revision: global_revision as u64,
            mode: FastMode::parse(row.get("global_mode"))
                .ok_or(PriorityPolicyError::Unavailable)?,
            auto_off_on_quota_reset: row.get("global_auto_off"),
        };
        let account_mode = mode(row.get::<Option<&str>, _>("mode").unwrap_or("inherit"))?;
        let auto_off: Option<bool> = row.get("auto_off_on_quota_reset");
        let effective_auto_off = auto_off.unwrap_or(global.auto_off_on_quota_reset);
        let baseline = row
            .get::<Option<serde_json::Value>, _>("baseline")
            .map(serde_json::from_value::<PriorityQuotaObservation>)
            .transpose()
            .map_err(|_| PriorityPolicyError::Unavailable)?;
        let armed = effective_auto_off
            && account_mode != AccountPriorityMode::Disabled
            && row.get::<Option<i64>, _>("baseline_global_revision") == Some(global_revision)
            && baseline
                .as_ref()
                .is_some_and(|value| !value.windows.is_empty());
        let reason = if account_mode == AccountPriorityMode::Disabled {
            row.get::<Option<String>, _>("reason")
                .unwrap_or_else(|| "off".to_owned())
        } else if armed {
            "armed".to_owned()
        } else if effective_auto_off {
            "awaiting_baseline".to_owned()
        } else {
            "off".to_owned()
        };
        Ok(AccountPriorityPolicy {
            account_id: id.to_owned(),
            revision: row.get::<Option<i64>, _>("revision").unwrap_or(0) as u64,
            mode: account_mode,
            auto_off_on_quota_reset: auto_off,
            effective_auto_off_on_quota_reset: effective_auto_off,
            effective_mode: account_mode.resolve(global.mode, FastMode::Default),
            global,
            status: PriorityPolicyStatus {
                armed,
                last_observed_at: baseline.map(|value| value.observed_at),
                last_auto_off_at: row.get("last_auto_off_at"),
                reason,
            },
        })
    }

    async fn update_account(
        &self,
        id: &str,
        expected: u64,
        mode: AccountPriorityMode,
        auto_off: Option<bool>,
        actor: &str,
    ) -> Result<AccountPriorityPolicy, PriorityPolicyError> {
        let expected = i64::try_from(expected).map_err(|_| PriorityPolicyError::Conflict)?;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let global_auto_off: bool = sqlx::query_scalar(
            "select auto_off_on_quota_reset from codex_priority_global where id=1 for share",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        let supported: Option<String> = sqlx::query_scalar("select id from provider_accounts where id=$1 and provider_kind='openai' and authentication_kind='oauth' for share")
            .bind(id).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        supported.ok_or(PriorityPolicyError::NotFound)?;
        sqlx::query(
            "insert into codex_priority_accounts(account_id) values($1) on conflict do nothing",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        let row = sqlx::query("update codex_priority_accounts set revision=revision+1,mode=$3,auto_off_on_quota_reset=$4,baseline=null,baseline_global_revision=null,reason=case when coalesce($4,$5) and $3 <> 'disabled' then 'awaiting_baseline' else 'off' end,updated_at=clock_timestamp() where account_id=$1 and revision=$2 returning revision")
            .bind(id).bind(expected).bind(mode.as_str()).bind(auto_off).bind(global_auto_off).fetch_optional(&mut *tx).await.map_err(unavailable)?.ok_or(PriorityPolicyError::Conflict)?;
        sqlx::query("insert into codex_priority_audit(account_id,revision,actor_ref,details) values($1,$2,$3,$4)")
            .bind(id).bind(row.get::<i64, _>("revision")).bind(actor).bind(serde_json::json!({"mode":mode,"autoOffOnQuotaReset":auto_off})).execute(&mut *tx).await.map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        self.account(id).await
    }

    async fn resolve(&self, id: &str, group: FastMode) -> Result<FastMode, PriorityPolicyError> {
        let row = sqlx::query("select g.mode global_mode,p.mode from codex_priority_global g left join codex_priority_accounts p on p.account_id=$1 where g.id=1")
            .bind(id).fetch_one(&self.pool).await.map_err(unavailable)?;
        let global =
            FastMode::parse(row.get("global_mode")).ok_or(PriorityPolicyError::Unavailable)?;
        Ok(mode(row.get::<Option<&str>, _>("mode").unwrap_or("inherit"))?.resolve(global, group))
    }

    async fn observe(
        &self,
        id: &str,
        observation: PriorityQuotaObservation,
    ) -> Result<(), PriorityPolicyError> {
        if observation.windows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        // 全局、凭据、账号配置采用同一锁顺序，阻止重配置和旧身份观测交错
        let global = sqlx::query("select revision,auto_off_on_quota_reset,updated_at from codex_priority_global where id=1 for share")
            .fetch_one(&mut *tx).await.map_err(unavailable)?;
        let global_revision: i64 = global.get("revision");
        let global_updated_at: DateTime<Utc> = global.get("updated_at");
        let revision: Option<i64> = sqlx::query_scalar("select credential_revision from provider_accounts where id=$1 and provider_kind='openai' and authentication_kind='oauth' for share")
            .bind(id).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        if revision.and_then(|value| u64::try_from(value).ok())
            != Some(observation.credential_revision)
        {
            return Ok(());
        }
        let mut row = sqlx::query("select revision,mode,auto_off_on_quota_reset,baseline,baseline_global_revision,updated_at from codex_priority_accounts where account_id=$1 for update")
            .bind(id).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        if row.is_none() && global.get::<bool, _>("auto_off_on_quota_reset") {
            sqlx::query("insert into codex_priority_accounts(account_id,updated_at) values($1,$2) on conflict do nothing")
                .bind(id).bind(global_updated_at).execute(&mut *tx).await.map_err(unavailable)?;
            row = sqlx::query("select revision,mode,auto_off_on_quota_reset,baseline,baseline_global_revision,updated_at from codex_priority_accounts where account_id=$1 for update")
                .bind(id).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        }
        let Some(row) = row else { return Ok(()) };
        let auto_off = row
            .get::<Option<bool>, _>("auto_off_on_quota_reset")
            .unwrap_or_else(|| global.get("auto_off_on_quota_reset"));
        if !auto_off
            || row.get::<&str, _>("mode") == "disabled"
            || observation.observed_at
                <= global_updated_at.max(row.get::<DateTime<Utc>, _>("updated_at"))
        {
            return Ok(());
        }
        // 改过全局配置后先绑定新观察，迟到的旧代次不得自动关闭
        let previous =
            if row.get::<Option<i64>, _>("baseline_global_revision") == Some(global_revision) {
                row.get::<Option<serde_json::Value>, _>("baseline")
                    .map(serde_json::from_value::<PriorityQuotaObservation>)
                    .transpose()
                    .map_err(|_| PriorityPolicyError::Unavailable)?
            } else {
                None
            };
        if previous
            .as_ref()
            .is_some_and(|previous| observation.observed_at <= previous.observed_at)
        {
            return Ok(());
        }
        let reset = previous
            .as_ref()
            .is_some_and(|previous| confirmed_reset(previous, &observation));
        let policy_revision: i64 = row.get("revision");
        sqlx::query("update codex_priority_accounts set baseline=$2,baseline_global_revision=$4,mode=case when $3 then 'disabled' else mode end,revision=revision+case when $3 then 1 else 0 end,last_auto_off_at=case when $3 then clock_timestamp() else last_auto_off_at end,reason=case when $3 then 'quota_reset' else 'armed' end where account_id=$1")
            .bind(id).bind(sqlx::types::Json(&observation)).bind(reset).bind(global_revision).execute(&mut *tx).await.map_err(unavailable)?;
        if reset {
            sqlx::query("insert into codex_priority_audit(account_id,revision,actor_ref,details) values($1,$2,'system:quota_reset',$3)")
                .bind(id).bind(policy_revision + 1).bind(serde_json::json!({"previous":previous,"current":observation,"mode":"disabled","globalRevision":global_revision})).execute(&mut *tx).await.map_err(unavailable)?;
        }
        tx.commit().await.map_err(unavailable)
    }
}
