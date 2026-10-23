//! 全局与账号快速模式、配额代次观察和持久自动关闭的中立合同

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::account::FastMode;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountPriorityMode {
    #[default]
    Inherit,
    Enabled,
    Disabled,
}

impl AccountPriorityMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    #[must_use]
    pub const fn resolve(self, global: FastMode, group: FastMode) -> FastMode {
        if matches!(group, FastMode::Disabled) {
            return FastMode::Disabled;
        }
        match self {
            Self::Enabled => FastMode::Enabled,
            Self::Disabled => FastMode::Disabled,
            Self::Inherit => match global {
                FastMode::Default => group,
                mode => mode,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalPriorityPolicy {
    pub revision: u64,
    pub mode: FastMode,
    pub auto_off_on_quota_reset: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityResetWindow {
    pub key: String,
    pub reset_at: DateTime<Utc>,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityQuotaObservation {
    pub credential_revision: u64,
    pub observed_at: DateTime<Utc>,
    pub windows: Vec<PriorityResetWindow>,
}

/// 只有同身份、严格更新的观测跨越已绑定窗口边界，才确认新代次
#[must_use]
pub fn confirmed_reset(
    previous: &PriorityQuotaObservation,
    next: &PriorityQuotaObservation,
) -> bool {
    previous.credential_revision == next.credential_revision
        && next.observed_at > previous.observed_at
        && previous.windows.iter().any(|old| {
            next.windows.iter().any(|new| {
                old.key == new.key
                    && old.used_percent.is_finite()
                    && old.used_percent > 1.0
                    && new.used_percent.is_finite()
                    && (0.0..=1.0).contains(&new.used_percent)
                    && new.reset_at > old.reset_at + chrono::Duration::seconds(2)
            })
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityPolicyStatus {
    pub armed: bool,
    pub last_observed_at: Option<DateTime<Utc>>,
    pub last_auto_off_at: Option<DateTime<Utc>>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPriorityPolicy {
    pub account_id: String,
    pub revision: u64,
    pub mode: AccountPriorityMode,
    pub auto_off_on_quota_reset: Option<bool>,
    pub effective_auto_off_on_quota_reset: bool,
    pub effective_mode: FastMode,
    pub global: GlobalPriorityPolicy,
    pub status: PriorityPolicyStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PriorityPolicyError {
    #[error("priority policy revision changed")]
    Conflict,
    #[error("OpenAI OAuth account not found")]
    NotFound,
    #[error("priority policy store unavailable")]
    Unavailable,
}

#[async_trait]
pub trait PriorityPolicyStore: Send + Sync {
    async fn global(&self) -> Result<GlobalPriorityPolicy, PriorityPolicyError>;
    async fn update_global(
        &self,
        expected: u64,
        mode: FastMode,
        auto_off: bool,
        actor: &str,
    ) -> Result<GlobalPriorityPolicy, PriorityPolicyError>;
    async fn account(&self, id: &str) -> Result<AccountPriorityPolicy, PriorityPolicyError>;
    async fn update_account(
        &self,
        id: &str,
        expected: u64,
        mode: AccountPriorityMode,
        auto_off: Option<bool>,
        actor: &str,
    ) -> Result<AccountPriorityPolicy, PriorityPolicyError>;
    /// 一次读取所选账号配置；group 为请求已冻结的禁止上限
    async fn resolve(&self, id: &str, group: FastMode) -> Result<FastMode, PriorityPolicyError>;
    /// Store 在账号/配置行锁内验证当前凭据版本与观测时间并提交系统审计
    async fn observe(
        &self,
        id: &str,
        observation: PriorityQuotaObservation,
    ) -> Result<(), PriorityPolicyError>;
}
