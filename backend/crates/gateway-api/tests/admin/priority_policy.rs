//! 快速模式真实路由的继承覆盖、全局默认和 CAS wire 合同

use super::*;
use axum::http::StatusCode;
use gateway_core::{account::FastMode, provider_ports::priority_policy::*};

#[derive(Default)]
pub(super) struct MemoryPriority(Mutex<Option<GlobalPriorityPolicy>>);

#[async_trait]
impl PriorityPolicyStore for MemoryPriority {
    async fn global(&self) -> Result<GlobalPriorityPolicy, PriorityPolicyError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(GlobalPriorityPolicy {
                revision: 0,
                mode: FastMode::Default,
                auto_off_on_quota_reset: false,
            }))
    }
    async fn update_global(
        &self,
        expected: u64,
        mode: FastMode,
        auto_off: bool,
        _: &str,
    ) -> Result<GlobalPriorityPolicy, PriorityPolicyError> {
        let mut saved = self.0.lock().unwrap();
        if saved.as_ref().map_or(0, |value| value.revision) != expected {
            return Err(PriorityPolicyError::Conflict);
        }
        let value = GlobalPriorityPolicy {
            revision: expected + 1,
            mode,
            auto_off_on_quota_reset: auto_off,
        };
        *saved = Some(value.clone());
        Ok(value)
    }
    async fn account(&self, id: &str) -> Result<AccountPriorityPolicy, PriorityPolicyError> {
        let global = self.global().await?;
        Ok(AccountPriorityPolicy {
            account_id: id.into(),
            revision: 0,
            mode: AccountPriorityMode::Inherit,
            auto_off_on_quota_reset: None,
            effective_auto_off_on_quota_reset: global.auto_off_on_quota_reset,
            effective_mode: global.mode,
            global,
            status: PriorityPolicyStatus {
                armed: false,
                last_observed_at: None,
                last_auto_off_at: None,
                reason: "awaiting_baseline".into(),
            },
        })
    }
    async fn update_account(
        &self,
        id: &str,
        expected: u64,
        mode: AccountPriorityMode,
        auto_off: Option<bool>,
        _: &str,
    ) -> Result<AccountPriorityPolicy, PriorityPolicyError> {
        if expected != 0 {
            return Err(PriorityPolicyError::Conflict);
        }
        let mut view = self.account(id).await?;
        view.revision = 1;
        view.mode = mode;
        view.effective_mode = mode.resolve(view.global.mode, FastMode::Default);
        view.auto_off_on_quota_reset = auto_off;
        view.effective_auto_off_on_quota_reset =
            auto_off.unwrap_or(view.global.auto_off_on_quota_reset);
        Ok(view)
    }
    async fn resolve(&self, _: &str, _: FastMode) -> Result<FastMode, PriorityPolicyError> {
        Err(PriorityPolicyError::Unavailable)
    }
    async fn observe(
        &self,
        _: &str,
        _: PriorityQuotaObservation,
    ) -> Result<(), PriorityPolicyError> {
        Err(PriorityPolicyError::Unavailable)
    }
}

#[tokio::test]
async fn priority_http_global_auto_off_and_nullable_account_overrides() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let initial = quota_policy::request(&fixture, "/api/admin/priority-policy", None, true).await;
    assert_eq!(initial.1["data"]["autoOffOnQuotaReset"], false);
    let command =
        serde_json::json!({"expectedRevision":0,"mode":"enabled","autoOffOnQuotaReset":true});
    let saved = quota_policy::request(
        &fixture,
        "/api/admin/priority-policy/update",
        Some(command.clone()),
        true,
    )
    .await;
    assert_eq!(saved.0, StatusCode::OK);
    assert_eq!(saved.1["data"]["autoOffOnQuotaReset"], true);
    assert_eq!(
        quota_policy::request(
            &fixture,
            "/api/admin/priority-policy/update",
            Some(command),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    for (override_value, effective) in [
        (serde_json::Value::Null, true),
        (serde_json::json!(false), false),
        (serde_json::json!(true), true),
    ] {
        let saved = quota_policy::request(&fixture, "/api/admin/accounts/priority-policy/update", Some(serde_json::json!({"accountId":"acct_test","expectedRevision":0,"mode":"inherit","autoOffOnQuotaReset":override_value})), true).await;
        assert_eq!(saved.0, StatusCode::OK);
        assert_eq!(saved.1["data"]["autoOffOnQuotaReset"], override_value);
        assert_eq!(saved.1["data"]["effectiveAutoOffOnQuotaReset"], effective);
    }
    let disabled = quota_policy::request(&fixture, "/api/admin/accounts/priority-policy/update", Some(serde_json::json!({"accountId":"acct_test","expectedRevision":0,"mode":"disabled","autoOffOnQuotaReset":null})), true).await;
    assert_eq!(disabled.1["data"]["effectiveMode"], "disabled");
}
