//! 统一配额快照到快速模式代次观察的投影与发送前配置解析

use super::*;
use crate::credential::{CODEX_AUTHENTICATION_KIND_OAUTH, harness_oauth::is_harness_kind};
use chrono::{DateTime, Utc};
use gateway_core::{account::FastMode, provider_ports::priority_policy::*};

impl CodexCredentialQuotaService {
    #[must_use]
    pub fn with_priority_policy(mut self, store: Option<Arc<dyn PriorityPolicyStore>>) -> Self {
        self.priority_policy = store;
        self
    }

    pub(crate) async fn resolve_priority(
        &self,
        account: &ProviderAccount,
        group: FastMode,
    ) -> Result<FastMode, PriorityPolicyError> {
        // 新配置仅管理 OAuth，其他认证方式继续原请求/分组合同，不进入不支持的 Store 读取
        if account.authentication_kind() != CODEX_AUTHENTICATION_KIND_OAUTH
            && !is_harness_kind(account.authentication_kind())
        {
            return Ok(group);
        }
        match self.priority_policy.as_ref() {
            Some(store) => store.resolve(account.id().as_str(), group).await,
            None => Ok(group),
        }
    }

    pub(super) async fn observe_priority(&self, snapshot: &CodexAccountQuotaSnapshot) {
        let Some(store) = self.priority_policy.as_ref() else {
            return;
        };
        let windows = snapshot
            .windows()
            .iter()
            .filter(|window| window.is_account_wide())
            .filter_map(|window| {
                let seconds = window.window_seconds().filter(|seconds| *seconds > 0)?;
                let used_percent = window
                    .used_percent()
                    .filter(|used| used.is_finite() && (0.0..=100.0).contains(used))?;
                let reset_at = window.reset_at()?;
                if reset_at <= DateTime::<Utc>::from(snapshot.observed_at()) {
                    return None;
                }
                let duplicate = snapshot
                    .windows()
                    .iter()
                    .filter(|other| {
                        other.source() == window.source() && other.window_seconds() == Some(seconds)
                    })
                    .count()
                    > 1;
                let key = if duplicate {
                    format!("{}:{}:{:?}", window.source(), seconds, window.role())
                } else {
                    format!("{}:{}", window.source(), seconds)
                };
                Some(PriorityResetWindow {
                    key,
                    reset_at,
                    used_percent,
                })
            })
            .collect();
        if let Err(error) = store
            .observe(
                snapshot.account_id().as_str(),
                PriorityQuotaObservation {
                    credential_revision: snapshot.credential_revision().get(),
                    observed_at: DateTime::<Utc>::from(snapshot.observed_at()),
                    windows,
                },
            )
            .await
        {
            tracing::warn!(account_id = snapshot.account_id().as_str(), error = %error, "OpenAI priority quota observation was not persisted");
        }
    }
}
