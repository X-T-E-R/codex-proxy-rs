//! 管理员策略 API 的真实 router → Admin → Store mock 合同。

use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use gateway_core::provider_ports::quota_policy::*;
use tower::ServiceExt as _;

#[derive(Default)]
pub(super) struct MemoryPolicy(Mutex<Option<GlobalQuotaPolicy>>);
#[async_trait]
impl QuotaPolicyStore for MemoryPolicy {
    async fn global(&self) -> Result<GlobalQuotaPolicy, QuotaPolicyError> {
        Ok(self.0.lock().unwrap().clone().unwrap_or(GlobalQuotaPolicy {
            revision: 0,
            policy: QuotaPolicy::default(),
        }))
    }
    async fn update_global(
        &self,
        revision: u64,
        policy: QuotaPolicy,
        _: &str,
    ) -> Result<GlobalQuotaPolicy, QuotaPolicyError> {
        if !policy.valid() {
            return Err(QuotaPolicyError::Invalid);
        }
        let mut current = self.0.lock().unwrap();
        if current.as_ref().map_or(0, |p| p.revision) != revision {
            return Err(QuotaPolicyError::Conflict);
        }
        let saved = GlobalQuotaPolicy {
            revision: revision + 1,
            policy,
        };
        *current = Some(saved.clone());
        Ok(saved)
    }
    async fn account(&self, id: &str) -> Result<AccountQuotaPolicy, QuotaPolicyError> {
        if id == "acct_unsupported" {
            return Err(QuotaPolicyError::NotFound);
        }
        Ok(AccountQuotaPolicy {
            account_id: id.into(),
            revision: 0,
            mode: QuotaPolicyMode::Inherit,
            policy: None,
            effective_policy: self.global().await?.policy,
            source: "global".into(),
            status: QuotaPolicyStatus {
                reason: "off".into(),
                ..Default::default()
            },
        })
    }
    async fn update_account(
        &self,
        id: &str,
        expected: u64,
        mode: QuotaPolicyMode,
        policy: Option<QuotaPolicy>,
        _: &str,
    ) -> Result<AccountQuotaPolicy, QuotaPolicyError> {
        if expected != 0 {
            return Err(QuotaPolicyError::Conflict);
        }
        if (mode == QuotaPolicyMode::Custom) != policy.is_some()
            || policy.as_ref().is_some_and(|p| !p.valid())
        {
            return Err(QuotaPolicyError::Invalid);
        }
        let mut view = self.account(id).await?;
        view.revision = 1;
        view.mode = mode;
        view.policy = policy.clone();
        view.effective_policy = match mode {
            QuotaPolicyMode::Custom => policy.unwrap(),
            QuotaPolicyMode::Disabled => QuotaPolicy::default(),
            QuotaPolicyMode::Inherit => view.effective_policy,
        };
        view.source = match mode {
            QuotaPolicyMode::Custom => "account",
            QuotaPolicyMode::Disabled => "disabled",
            QuotaPolicyMode::Inherit => "global",
        }
        .into();
        Ok(view)
    }
    async fn status(
        &self,
        _: &str,
        _: bool,
        _: &str,
        _: Option<DateTime<Utc>>,
    ) -> Result<(), QuotaPolicyError> {
        Err(QuotaPolicyError::Unavailable)
    }
    async fn lock_reset(&self, _: &str) -> Result<Box<dyn ResetOperationGuard>, QuotaPolicyError> {
        Err(QuotaPolicyError::Unavailable)
    }
    async fn claim_reset(&self, _: ResetClaim) -> Result<ResetOperation, QuotaPolicyError> {
        Err(QuotaPolicyError::Unavailable)
    }
    async fn finish_reset(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<(), QuotaPolicyError> {
        Err(QuotaPolicyError::Unavailable)
    }
    async fn reset_operation(&self, _: &str) -> Result<Option<ResetOperation>, QuotaPolicyError> {
        Ok(None)
    }
    async fn pending_reset(&self, _: &str) -> Result<Option<ResetOperation>, QuotaPolicyError> {
        Ok(None)
    }
    async fn confirm_readback(&self, _: &str, _: u64, _: u64) -> Result<(), QuotaPolicyError> {
        Err(QuotaPolicyError::Unavailable)
    }
}

async fn request(
    fixture: &AdminTestFixture,
    uri: &str,
    body: Option<serde_json::Value>,
    auth: bool,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .uri(uri)
        .header("x-request-id", "req_quota_policy");
    if auth {
        request = request.header(header::COOKIE, "cpr_session=valid-session");
    }
    let body = if let Some(body) = body {
        request = request
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = gateway_api::admin::router::<AdminTestState>()
        .with_state(fixture.state())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn quota_policy_http_saves_raw_threshold_and_shared_read_view_with_cas() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    assert_eq!(
        request(&fixture, "/api/admin/quota-policy", None, false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let initial = request(&fixture, "/api/admin/quota-policy", None, true).await;
    assert_eq!(initial.0, StatusCode::OK);
    assert_eq!(initial.1["data"]["policy"]["secondary"]["action"], "off");
    let mut policy = serde_json::to_value(QuotaPolicy::default()).unwrap();
    policy["secondary"] = serde_json::json!({"action":"stop","thresholdPercent":99});
    let command = serde_json::json!({"expectedRevision":0,"policy":policy});
    let saved = request(
        &fixture,
        "/api/admin/quota-policy/update",
        Some(command.clone()),
        true,
    )
    .await;
    assert_eq!(saved.0, StatusCode::OK);
    assert_eq!(saved.1["data"]["revision"], 1);
    assert_eq!(saved.1["data"]["policy"], policy);
    assert_eq!(
        request(
            &fixture,
            "/api/admin/quota-policy/update",
            Some(command),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let account = request(
        &fixture,
        "/api/admin/accounts/quota-policy?accountId=acct_test",
        None,
        true,
    )
    .await;
    assert_eq!(account.0, StatusCode::OK);
    assert_eq!(account.1["data"]["mode"], "inherit");
    assert_eq!(account.1["data"]["effectivePolicy"], policy);
    assert_eq!(account.1["data"]["status"]["autoAttemptsLast24h"], 0);
    assert!(account.1["data"]["status"]["pendingOperationId"].is_null());
    let disabled = request(&fixture, "/api/admin/accounts/quota-policy/update", Some(serde_json::json!({"accountId":"acct_test","expectedRevision":0,"mode":"disabled","policy":null})), true).await;
    assert_eq!(disabled.0, StatusCode::OK);
    assert_eq!(disabled.1["data"]["source"], "disabled");
    assert_eq!(
        disabled.1["data"]["effectivePolicy"]["secondary"]["action"],
        "off"
    );
}

#[tokio::test]
async fn quota_policy_http_rejects_invalid_limits_modes_and_unsupported_accounts() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (field, value) in [
        ("maxAttemptsPer24h", 0),
        ("maxAttemptsPer24h", 11),
        ("cooldownSeconds", 3599),
        ("cooldownSeconds", 86401),
    ] {
        let mut policy = serde_json::to_value(QuotaPolicy::default()).unwrap();
        policy["autoReset"][field] = value.into();
        let response = request(
            &fixture,
            "/api/admin/quota-policy/update",
            Some(serde_json::json!({"expectedRevision":0,"policy":policy})),
            true,
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST, "{field}={value}");
        assert_eq!(response.1["code"], 40001);
    }
    for uri in [
        "/api/admin/accounts/quota-policy",
        "/api/admin/accounts/quota-policy?accountId=bad",
        "/api/admin/accounts/quota-policy?accountId=acct_test&refresh=true",
        "/api/admin/accounts/quota-policy?accountId=acct_unsupported",
    ] {
        assert_eq!(
            request(&fixture, uri, None, true).await.0,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    let bad = request(&fixture, "/api/admin/accounts/quota-policy/update", Some(serde_json::json!({"accountId":"acct_test","expectedRevision":0,"mode":"custom","policy":null})), true).await;
    assert_eq!(bad.0, StatusCode::BAD_REQUEST);
}
