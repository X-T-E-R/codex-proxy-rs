//! Codex 通用额度窗口策略及不可逆重置操作的中立持久化合同。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaAction {
    #[default]
    Off,
    Stop,
    ResetThenStop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaRule {
    pub action: QuotaAction,
    pub threshold_percent: u8,
}

impl Default for QuotaRule {
    fn default() -> Self {
        Self {
            action: QuotaAction::Off,
            threshold_percent: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutoResetLimits {
    pub max_attempts_per24h: u8,
    pub cooldown_seconds: u32,
}

impl Default for AutoResetLimits {
    fn default() -> Self {
        Self {
            max_attempts_per24h: 1,
            cooldown_seconds: 86_400,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaPolicy {
    pub primary: QuotaRule,
    pub secondary: QuotaRule,
    pub auto_reset: AutoResetLimits,
}

impl QuotaPolicy {
    pub fn valid(&self) -> bool {
        (1..=100).contains(&self.primary.threshold_percent)
            && (1..=100).contains(&self.secondary.threshold_percent)
            && (1..=10).contains(&self.auto_reset.max_attempts_per24h)
            && (3_600..=86_400).contains(&self.auto_reset.cooldown_seconds)
    }

    pub fn enabled(&self) -> bool {
        self.primary.action != QuotaAction::Off || self.secondary.action != QuotaAction::Off
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPolicyMode {
    #[default]
    Inherit,
    Disabled,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalQuotaPolicy {
    pub revision: u64,
    pub policy: QuotaPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPolicyStatus {
    pub paused: bool,
    pub reason: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub last_auto_attempt_at: Option<DateTime<Utc>>,
    pub auto_attempts_last24h: u64,
    pub pending_operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuotaPolicy {
    pub account_id: String,
    pub revision: u64,
    pub mode: QuotaPolicyMode,
    pub policy: Option<QuotaPolicy>,
    pub effective_policy: QuotaPolicy,
    pub source: String,
    pub status: QuotaPolicyStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuotaPolicyError {
    #[error("quota policy is invalid")]
    Invalid,
    #[error("quota policy revision changed")]
    Conflict,
    #[error("Codex OAuth account not found")]
    NotFound,
    #[error("quota policy store unavailable")]
    Unavailable,
    #[error("reset operation is pending")]
    Pending,
    #[error("automatic reset budget exhausted")]
    Budget,
    #[error("automatic reset cooldown active")]
    Cooldown,
    #[error("automatic reset episode already attempted")]
    Episode,
}

/// 账号互斥由 Store guard 持有；临近发送再次确认连接/锁仍有效。
#[async_trait]
pub trait ResetOperationGuard: Send + Sync {
    async fn authorize(&mut self, claim: &ResetClaim) -> Result<(), QuotaPolicyError>;
    async fn valid(&mut self) -> Result<(), QuotaPolicyError>;
}

#[derive(Debug, Clone)]
pub struct ResetOperation {
    pub id: String,
    pub account_id: String,
    pub credential_revision: u64,
    pub credit_id: Option<String>,
    pub automatic: bool,
    pub episode: Option<String>,
    pub trigger_percent: Option<u8>,
    pub state: String,
    pub result_code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResetClaim {
    pub operation: ResetOperation,
    pub global_revision: u64,
    pub account_revision: u64,
}

#[async_trait]
pub trait QuotaPolicyStore: Send + Sync {
    async fn global(&self) -> Result<GlobalQuotaPolicy, QuotaPolicyError>;
    async fn update_global(
        &self,
        expected: u64,
        policy: QuotaPolicy,
        audit: &str,
    ) -> Result<GlobalQuotaPolicy, QuotaPolicyError>;
    async fn account(&self, id: &str) -> Result<AccountQuotaPolicy, QuotaPolicyError>;
    async fn update_account(
        &self,
        id: &str,
        expected: u64,
        mode: QuotaPolicyMode,
        policy: Option<QuotaPolicy>,
        audit: &str,
    ) -> Result<AccountQuotaPolicy, QuotaPolicyError>;
    async fn status(
        &self,
        id: &str,
        paused: bool,
        reason: &str,
        observed_at: Option<DateTime<Utc>>,
    ) -> Result<(), QuotaPolicyError>;
    async fn lock_reset(&self, id: &str) -> Result<Box<dyn ResetOperationGuard>, QuotaPolicyError>;
    async fn claim_reset(&self, claim: ResetClaim) -> Result<ResetOperation, QuotaPolicyError>;
    async fn finish_reset(
        &self,
        id: &str,
        state: &str,
        code: Option<&str>,
    ) -> Result<(), QuotaPolicyError>;
    /// 只读幂等结果查询，不授权消费或改变凭据代次。
    async fn reset_operation(&self, id: &str) -> Result<Option<ResetOperation>, QuotaPolicyError>;
    async fn pending_reset(&self, id: &str) -> Result<Option<ResetOperation>, QuotaPolicyError>;
    /// 回读判断对应的配置版本必须在同一事务持锁验证后才能清屏障。
    async fn confirm_readback(
        &self,
        operation_id: &str,
        global_revision: u64,
        account_revision: u64,
    ) -> Result<(), QuotaPolicyError>;
}
