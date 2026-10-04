//! 本地额度暂停与后台自动重置；不修改上游访问资格，不在推理请求内消费。

use super::{
    CodexAccountQuotaSnapshot, CodexCredentialQuotaService, CodexQuotaWindowKind,
    CodexQuotaWindowRole, CodexResetCreditsError,
};
use crate::transport::CodexRateLimitResetCreditsConsumeResult;
use chrono::{DateTime, Utc};
use gateway_core::{
    account::{CredentialState, ProviderAccount, ProviderAccountId},
    lifecycle::CancellationToken,
    provider_ports::quota_policy::{
        QuotaAction, QuotaPolicy, QuotaPolicyError, ResetClaim, ResetOperation,
    },
};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

struct Decision {
    reason: &'static str,
    paused: bool,
    episode: Option<String>,
    trigger_percent: Option<u8>,
}

fn evaluate(
    policy: &QuotaPolicy,
    snapshot: Option<&CodexAccountQuotaSnapshot>,
    now: SystemTime,
) -> Decision {
    let mut result = Decision {
        reason: "off",
        paused: false,
        episode: None,
        trigger_percent: None,
    };
    if !policy.enabled() {
        return result;
    }
    result.reason = "ready";
    let Some(snapshot) = snapshot else {
        return Decision {
            reason: "quota_unconfirmed",
            paused: true,
            episode: None,
            trigger_percent: None,
        };
    };
    // 官方角色不保证固定周期，只有通用 Codex 短期/周窗口参与对应规则。
    for (rule, role, kind, reason) in [
        (
            &policy.primary,
            CodexQuotaWindowRole::Primary,
            CodexQuotaWindowKind::ShortTerm,
            "threshold_primary",
        ),
        (
            &policy.secondary,
            CodexQuotaWindowRole::Secondary,
            CodexQuotaWindowKind::Weekly,
            "threshold_secondary",
        ),
    ] {
        if rule.action == QuotaAction::Off {
            continue;
        }
        let Some(window) = snapshot.windows().iter().find(|window| {
            window.is_account_wide() && window.role() == role && window.kind() == kind
        }) else {
            continue;
        };
        let fresh = window.reset_at().map(SystemTime::from).map_or_else(
            || {
                now.duration_since(snapshot.observed_at())
                    .unwrap_or_default()
                    < super::QUOTA_SCHEDULING_TTL
            },
            |reset| reset > now,
        );
        let Some(used) = window
            .used_percent()
            .filter(|used| used.is_finite() && fresh)
        else {
            return Decision {
                reason: "quota_unconfirmed",
                paused: true,
                episode: None,
                trigger_percent: None,
            };
        };
        if used >= f64::from(rule.threshold_percent) {
            result.paused = true;
            result.reason = reason;
            if rule.action == QuotaAction::ResetThenStop && result.episode.is_none() {
                // 缺明确边界不消费；用时钟、凭据或配置版本拼 episode 会打开重复耗卡逃逸。
                result.episode = window
                    .reset_at()
                    .map(|reset| format!("{}:{}", reason, reset.timestamp()));
                result.trigger_percent = Some(rule.threshold_percent);
            }
        }
    }
    result
}

fn readback_proves_recovery(op: &ResetOperation, snapshot: &CodexAccountQuotaSnapshot) -> bool {
    let Some(threshold) = op.trigger_percent else {
        return false;
    };
    let (role, kind) = match op.episode.as_deref() {
        Some(episode) if episode.starts_with("threshold_primary:") => (
            CodexQuotaWindowRole::Primary,
            CodexQuotaWindowKind::ShortTerm,
        ),
        Some(episode) if episode.starts_with("threshold_secondary:") => (
            CodexQuotaWindowRole::Secondary,
            CodexQuotaWindowKind::Weekly,
        ),
        _ => return false,
    };
    snapshot.windows().iter().any(|window| {
        window.is_account_wide()
            && window.role() == role
            && window.kind() == kind
            && window
                .reset_at()
                .is_some_and(|at| SystemTime::from(at) > SystemTime::now())
            && window
                .used_percent()
                .is_some_and(|used| used.is_finite() && used < f64::from(threshold))
    })
}

fn automatic_trigger_threshold(
    policy: &QuotaPolicy,
    op: &ResetOperation,
    snapshot: &CodexAccountQuotaSnapshot,
) -> Option<u8> {
    let (prefix, role, kind, rule) = match op.episode.as_deref()? {
        episode if episode.starts_with("threshold_primary:") => (
            "threshold_primary",
            CodexQuotaWindowRole::Primary,
            CodexQuotaWindowKind::ShortTerm,
            &policy.primary,
        ),
        episode if episode.starts_with("threshold_secondary:") => (
            "threshold_secondary",
            CodexQuotaWindowRole::Secondary,
            CodexQuotaWindowKind::Weekly,
            &policy.secondary,
        ),
        _ => return None,
    };
    if rule.action != QuotaAction::ResetThenStop {
        return None;
    }
    snapshot
        .windows()
        .iter()
        .find(|window| {
            window.is_account_wide()
                && window.role() == role
                && window.kind() == kind
                && window.reset_at().is_some_and(|reset| {
                    SystemTime::from(reset) > SystemTime::now()
                        && op.episode.as_deref()
                            == Some(format!("{}:{}", prefix, reset.timestamp()).as_str())
                })
                && window.used_percent().is_some_and(|used| {
                    used.is_finite() && used >= f64::from(rule.threshold_percent)
                })
        })
        .map(|_| rule.threshold_percent)
}

fn policy_error(error: QuotaPolicyError) -> CodexResetCreditsError {
    CodexResetCreditsError::Policy(error)
}

impl CodexCredentialQuotaService {
    pub(super) async fn terminal_manual_reset(
        &self,
        account_id: &ProviderAccountId,
        credit_id: Option<&str>,
        key: Uuid,
    ) -> Result<Option<CodexRateLimitResetCreditsConsumeResult>, CodexResetCreditsError> {
        let Some(store) = &self.quota_policy else {
            return Ok(None);
        };
        let Some(op) = store
            .reset_operation(&key.to_string())
            .await
            .map_err(policy_error)?
        else {
            return Ok(None);
        };
        if op.account_id != account_id.as_str()
            || op.credit_id.as_deref() != credit_id
            || op.automatic
        {
            return Err(policy_error(QuotaPolicyError::Conflict));
        }
        if op.state == "pending" {
            return Ok(None);
        }
        Ok(Some(CodexRateLimitResetCreditsConsumeResult {
            code: op
                .result_code
                .unwrap_or_else(|| "already_redeemed".to_owned()),
            credit: None,
            action_result: if op.state == "confirmed" {
                crate::transport::reset_credits::CodexResetActionResult::Confirmed
            } else {
                crate::transport::reset_credits::CodexResetActionResult::Rejected
            },
        }))
    }

    async fn complete_readback(&self, op: &ResetOperation, snapshot: &CodexAccountQuotaSnapshot) {
        let Some(store) = &self.quota_policy else {
            return;
        };
        let observed_at = Some(DateTime::<Utc>::from(snapshot.observed_at()));
        // 回读后加载当前策略；配置再变更则 Store CAS 保留屏障，下轮重新回读。
        let result = async {
            let global = store.global().await?;
            let current = store.account(&op.account_id).await?;
            let decision = evaluate(&current.effective_policy, Some(snapshot), SystemTime::now());
            if decision.paused || !readback_proves_recovery(op, snapshot) {
                return Err(QuotaPolicyError::Conflict);
            }
            store
                .confirm_readback(&op.id, global.revision, current.revision)
                .await?;
            Ok::<_, QuotaPolicyError>(decision)
        }
        .await;
        match result {
            Ok(decision) => {
                let _ = store
                    .status(&op.account_id, false, decision.reason, observed_at)
                    .await;
            }
            Err(_) => {
                let _ = store
                    .status(
                        &op.account_id,
                        true,
                        "reset_readback_unconfirmed",
                        observed_at,
                    )
                    .await;
            }
        }
    }

    pub fn with_quota_policy(
        mut self,
        store: Option<Arc<dyn gateway_core::provider_ports::quota_policy::QuotaPolicyStore>>,
    ) -> Self {
        self.quota_policy = store;
        self
    }

    pub(crate) async fn policy_blocks(&self, account: &ProviderAccount) -> bool {
        if account.authentication_kind() != crate::credential::CODEX_AUTHENTICATION_KIND_OAUTH {
            return false;
        }
        let Some(store) = &self.quota_policy else {
            return false;
        };
        let Ok(view) = store.account(account.id().as_str()).await else {
            return true;
        };
        if !view.effective_policy.enabled() {
            return false;
        }
        let snapshot = self.read_snapshot_for(account).await.ok().flatten();
        let decision = evaluate(&view.effective_policy, snapshot.as_ref(), SystemTime::now());
        decision.paused || view.status.pending_operation_id.is_some()
    }

    /// 每账号最多一分钟一次复核；低于阈值或新窗口权威观察才恢复。
    pub(crate) async fn synchronize_policy(&self, cancellation: &CancellationToken) {
        let Some(store) = &self.quota_policy else {
            return;
        };
        let Ok(accounts) = self.repository.list_for_provider().await else {
            return;
        };
        for account in accounts {
            if cancellation.is_cancelled() {
                return;
            }
            if account.authentication_kind() != crate::credential::CODEX_AUTHENTICATION_KIND_OAUTH
                || !account.enabled()
                || account.credential_state() != CredentialState::Ready
            {
                continue;
            }
            let Ok(view) = store.account(account.id().as_str()).await else {
                continue;
            };
            if !view.effective_policy.enabled() {
                let _ = store
                    .status(account.id().as_str(), false, "off", None)
                    .await;
                continue;
            }
            {
                let mut attempts = self.policy_refreshes.lock().await;
                if attempts
                    .get(account.id())
                    .is_some_and(|last| last.elapsed() < Duration::from_secs(60))
                {
                    continue;
                }
                attempts.insert(account.id().clone(), Instant::now());
            }
            // 强制回读而非按缓存 TTL 放行；失败保持暂停，不触发重置。
            let snapshot = match self.refresh_account(account.id()).await {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    let _ = store
                        .status(account.id().as_str(), true, "quota_unconfirmed", None)
                        .await;
                    continue;
                }
            };
            let observed_at = Some(DateTime::<Utc>::from(snapshot.observed_at()));
            let decision = evaluate(&view.effective_policy, Some(&snapshot), SystemTime::now());
            if view.status.pending_operation_id.is_some() {
                if let Ok(Some(op)) = store.pending_reset(account.id().as_str()).await {
                    if op.state == "confirmed" {
                        // 已成功的操作只回读；即使配置/窗口变化，也不能转换成下一次消费。
                        self.complete_readback(&op, &snapshot).await;
                    } else {
                        let _ = store
                            .status(account.id().as_str(), true, "reset_pending", observed_at)
                            .await;
                        // 重启/未知恢复只确认已保存的同键同命令，不按新窗口生成下一卡。
                        if op.automatic && op.credential_revision == account.revision().get() {
                            let _ = self.perform_reset(op, cancellation).await;
                        }
                    }
                }
                continue;
            }
            if !decision.paused {
                let _ = store
                    .status(account.id().as_str(), false, decision.reason, observed_at)
                    .await;
                continue;
            }
            if view.status.reason == "reset_readback_unconfirmed" {
                // 成功回读仍高/失败必须先看到全部生效窗口低于阈值，不能连环耗卡。
                let _ = store
                    .status(
                        account.id().as_str(),
                        true,
                        "reset_readback_unconfirmed",
                        observed_at,
                    )
                    .await;
                continue;
            }
            let _ = store
                .status(account.id().as_str(), true, decision.reason, observed_at)
                .await;
            let Some(episode) = decision.episode else {
                continue;
            };
            let op = ResetOperation {
                id: Uuid::new_v4().to_string(),
                account_id: account.id().as_str().to_owned(),
                credential_revision: account.revision().get(),
                credit_id: None,
                automatic: true,
                episode: Some(episode),
                trigger_percent: decision.trigger_percent,
                state: "pending".to_owned(),
                result_code: None,
            };
            match self.perform_reset(op, cancellation).await {
                Err(CodexResetCreditsError::Policy(QuotaPolicyError::Budget)) => {
                    let _ = store
                        .status(
                            account.id().as_str(),
                            true,
                            "auto_budget_exhausted",
                            observed_at,
                        )
                        .await;
                }
                Err(CodexResetCreditsError::Policy(QuotaPolicyError::Cooldown)) => {
                    let _ = store
                        .status(account.id().as_str(), true, "auto_cooldown", observed_at)
                        .await;
                }
                _ => {}
            }
        }
    }

    pub(super) async fn perform_reset(
        &self,
        requested: ResetOperation,
        cancellation: &CancellationToken,
    ) -> Result<CodexRateLimitResetCreditsConsumeResult, CodexResetCreditsError> {
        let Some(store) = &self.quota_policy else {
            return Err(policy_error(QuotaPolicyError::Unavailable));
        };
        let mut guard = store
            .lock_reset(&requested.account_id)
            .await
            .map_err(policy_error)?;
        let id = ProviderAccountId::new(requested.account_id.clone())
            .map_err(|_| CodexResetCreditsError::InvalidCredentialData)?;
        let account = self.reset_credit_account(&id).await?;
        if account.revision().get() != requested.credential_revision {
            return Err(policy_error(QuotaPolicyError::Conflict));
        }
        let global = store.global().await.map_err(policy_error)?;
        let view = store
            .account(&requested.account_id)
            .await
            .map_err(policy_error)?;
        let mut claim = ResetClaim {
            operation: requested,
            global_revision: global.revision,
            account_revision: view.revision,
        };
        let previous = store
            .pending_reset(&claim.operation.account_id)
            .await
            .map_err(policy_error)?;
        let mut trigger_snapshot = None;
        if claim.operation.automatic {
            if cancellation.is_cancelled()
                || !account.enabled()
                || account.credential_state() != CredentialState::Ready
            {
                return Err(policy_error(QuotaPolicyError::Conflict));
            }
            // 同键确认仍须当前规则启用；关闭配置不会自动重放不可逆命令。
            let fresh = self
                .refresh_account(&id)
                .await
                .map_err(|_| policy_error(QuotaPolicyError::Unavailable))?;
            let threshold =
                automatic_trigger_threshold(&view.effective_policy, &claim.operation, &fresh)
                    .ok_or_else(|| policy_error(QuotaPolicyError::Conflict))?;
            trigger_snapshot = Some(fresh);
            if previous.is_none() {
                // 记录实际首次 claim 的触发阈值，不沿用 worker 读取的旧配置。
                claim.operation.trigger_percent = Some(threshold);
                // 锁内最终核对库存；只有首次发送选号，同键确认绝不改卡片命令。
                let inventory = self.list_reset_credits(&id).await?;
                if inventory.available_count == 0 {
                    return Err(policy_error(QuotaPolicyError::Conflict));
                }
                claim.operation.credit_id = if inventory.credits.is_empty() {
                    None
                } else {
                    let card = inventory
                        .credits
                        .into_iter()
                        .find(|card| {
                            card.status.as_deref() == Some("available")
                                && card.expires_at.is_none_or(|at| at > Utc::now())
                        })
                        .ok_or_else(|| policy_error(QuotaPolicyError::Conflict))?;
                    Some(card.id)
                };
            }
        }
        claim.operation = store
            .claim_reset(claim.clone())
            .await
            .map_err(policy_error)?;
        if claim.operation.state != "pending" {
            return Ok(CodexRateLimitResetCreditsConsumeResult {
                code: claim
                    .operation
                    .result_code
                    .unwrap_or_else(|| "already_redeemed".to_owned()),
                credit: None,
                action_result: if claim.operation.state == "confirmed" {
                    crate::transport::reset_credits::CodexResetActionResult::Confirmed
                } else {
                    crate::transport::reset_credits::CodexResetActionResult::Rejected
                },
            });
        }
        guard.authorize(&claim).await.map_err(policy_error)?;
        // 强制额度回读会更新账号 quota 事实；在发送行锁内重新加载，不沿用回读前的账号快照。
        let account = self.reset_credit_account(&id).await?;
        if account.revision().get() != claim.operation.credential_revision {
            return Err(policy_error(QuotaPolicyError::Conflict));
        }
        let credential = self
            .repository
            .load_runtime_credential(&account)
            .await
            .map_err(|_| CodexResetCreditsError::InvalidCredentialData)?;
        let prepared = super::PreparedCodexRuntimeCredential {
            account,
            credential,
        };
        let client = crate::transport::CodexBackendClient::new(
            self.http.clone(),
            self.base_url.clone(),
            self.profile.clone(),
        );
        guard.valid().await.map_err(policy_error)?;
        if cancellation.is_cancelled() {
            return Err(policy_error(QuotaPolicyError::Conflict));
        }
        let key = Uuid::parse_str(&claim.operation.id)
            .map_err(|_| CodexResetCreditsError::InvalidCredentialData)?;
        // 库存/claim/授权可以跨自然窗口边界；发送前在配置行锁内再次核对原触发证据。
        if let Some(snapshot) = trigger_snapshot.as_ref()
            && automatic_trigger_threshold(&view.effective_policy, &claim.operation, snapshot)
                .is_none()
        {
            return Err(policy_error(QuotaPolicyError::Conflict));
        }
        let result = super::consume_reset_credit_once(
            &client,
            &prepared,
            &format!("reset_credit_consume_{}", Uuid::now_v7().simple()),
            claim.operation.credit_id.as_deref(),
            key,
        )
        .await
        .map_err(|error| super::map_reset_credit_attempt_error(error, true));
        // 释放共享行锁后再落结果/回读；操作已持久化，连接丢失仍留 pending。
        drop(guard);
        let confirming_unknown = previous.as_ref().is_some_and(|op| {
            op.state == "pending"
                && op.result_code.as_deref() != Some("credential_refresh_required")
        });
        match result {
            Ok(mut result) => {
                let confirmed = result.code == "reset"
                    || (result.code == "already_redeemed" && previous.is_some());
                let rejected = !confirming_unknown
                    && matches!(
                        result.code.as_str(),
                        "no_credit" | "nothing_to_reset" | "already_redeemed"
                    );
                result.action_result = if confirmed {
                    crate::transport::reset_credits::CodexResetActionResult::Confirmed
                } else if rejected {
                    crate::transport::reset_credits::CodexResetActionResult::Rejected
                } else {
                    crate::transport::reset_credits::CodexResetActionResult::Unknown
                };
                if !confirmed && !rejected {
                    // 未识别业务码保留原码与未决命令；不能猜成失败再换新键消费。
                    let _ = store
                        .status(&claim.operation.account_id, true, "reset_pending", None)
                        .await;
                    return Ok(result);
                }
                store
                    .finish_reset(
                        &claim.operation.id,
                        if confirmed { "confirmed" } else { "failed" },
                        Some(&result.code),
                    )
                    .await
                    // 已发送但无法持久化结果时，调用方必须保留原键，不能按依赖错误换键。
                    .map_err(|_| CodexResetCreditsError::ConsumeResultUnknown)?;
                if claim.operation.automatic {
                    let reason = if confirmed {
                        "reset_readback_unconfirmed"
                    } else {
                        "reset_failed"
                    };
                    let _ = store
                        .status(&claim.operation.account_id, true, reason, None)
                        .await;
                    if confirmed {
                        let _ = self.list_reset_credits(&id).await;
                        if let Ok(snapshot) = self.refresh_account(&id).await {
                            self.complete_readback(&claim.operation, &snapshot).await;
                        }
                    }
                }
                Ok(result)
            }
            Err(mut error) => {
                if confirming_unknown {
                    // 本次拒绝不能证明上一次未知消费没有完成，继续保留原键屏障。
                    error = CodexResetCreditsError::ConsumeResultUnknown;
                }
                let refresh_retry = !claim.operation.automatic
                    && previous.is_none()
                    && matches!(
                        error,
                        CodexResetCreditsError::CredentialRefreshRequired { .. }
                    );
                if !matches!(error, CodexResetCreditsError::ConsumeResultUnknown) {
                    store
                        .finish_reset(
                            &claim.operation.id,
                            if refresh_retry { "pending" } else { "failed" },
                            refresh_retry.then_some("credential_refresh_required"),
                        )
                        .await
                        .map_err(|_| CodexResetCreditsError::ConsumeResultUnknown)?;
                }
                let _ = store
                    .status(
                        &claim.operation.account_id,
                        true,
                        if refresh_retry
                            || matches!(error, CodexResetCreditsError::ConsumeResultUnknown)
                        {
                            "reset_pending"
                        } else {
                            "reset_failed"
                        },
                        None,
                    )
                    .await;
                Err(error)
            }
        }
    }
}
