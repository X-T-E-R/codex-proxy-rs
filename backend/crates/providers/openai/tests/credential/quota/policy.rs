//! 使用 mock 上游验证窗口策略和不可逆消费的真实调用链。

use super::*;
use async_trait::async_trait;
use gateway_core::account::{AccountFeedbackStats, ProviderAccountId};
use gateway_core::engine::{
    AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::policy::ClientApiKeyId;
use gateway_core::provider_ports::quota_policy::*;
use gateway_core::routing::{
    ClientRoutingScope, FrozenAccountScope, ProviderKind, RuntimeAccount, RuntimeAccountDirectory,
};
use provider_openai::credential::{
    CodexCookiePolicy, CodexCredentialSelector, SelectCodexCredential,
};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use url::Url;
use uuid::Uuid;

struct State {
    global: GlobalQuotaPolicy,
    mode: QuotaPolicyMode,
    custom: Option<QuotaPolicy>,
    status: QuotaPolicyStatus,
    ops: BTreeMap<String, ResetOperation>,
    readbacks: BTreeSet<String>,
}
struct PolicyStore {
    state: Mutex<State>,
    locked: Arc<AtomicBool>,
    lease_valid: Arc<AtomicBool>,
    fail_finish: AtomicBool,
    fail_readback: AtomicBool,
    change_on_readback: AtomicBool,
    claim_delay_ms: AtomicUsize,
}
impl PolicyStore {
    fn new(action: QuotaAction) -> Arc<Self> {
        let mut policy = QuotaPolicy::default();
        policy.secondary.action = action;
        policy.secondary.threshold_percent = 99;
        Arc::new(Self {
            state: Mutex::new(State {
                global: GlobalQuotaPolicy {
                    revision: 0,
                    policy,
                },
                mode: QuotaPolicyMode::Inherit,
                custom: None,
                status: QuotaPolicyStatus::default(),
                ops: BTreeMap::new(),
                readbacks: BTreeSet::new(),
            }),
            locked: Arc::new(AtomicBool::new(false)),
            lease_valid: Arc::new(AtomicBool::new(true)),
            fail_finish: AtomicBool::new(false),
            fail_readback: AtomicBool::new(false),
            change_on_readback: AtomicBool::new(false),
            claim_delay_ms: AtomicUsize::new(0),
        })
    }
}
struct Guard {
    locked: Arc<AtomicBool>,
    valid: Arc<AtomicBool>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.locked.store(false, Ordering::SeqCst);
    }
}
#[async_trait]
impl ResetOperationGuard for Guard {
    async fn authorize(&mut self, _: &ResetClaim) -> Result<(), QuotaPolicyError> {
        self.valid().await
    }
    async fn valid(&mut self) -> Result<(), QuotaPolicyError> {
        if self.valid.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(QuotaPolicyError::Conflict)
        }
    }
}
#[async_trait]
impl QuotaPolicyStore for PolicyStore {
    async fn global(&self) -> Result<GlobalQuotaPolicy, QuotaPolicyError> {
        Ok(self.state.lock().unwrap().global.clone())
    }
    async fn update_global(
        &self,
        expected: u64,
        policy: QuotaPolicy,
        _: &str,
    ) -> Result<GlobalQuotaPolicy, QuotaPolicyError> {
        let mut st = self.state.lock().unwrap();
        if expected != st.global.revision {
            return Err(QuotaPolicyError::Conflict);
        }
        if !policy.valid() {
            return Err(QuotaPolicyError::Invalid);
        }
        st.global = GlobalQuotaPolicy {
            revision: expected + 1,
            policy,
        };
        Ok(st.global.clone())
    }
    async fn account(&self, id: &str) -> Result<AccountQuotaPolicy, QuotaPolicyError> {
        let st = self.state.lock().unwrap();
        let effective = match st.mode {
            QuotaPolicyMode::Inherit => st.global.policy.clone(),
            QuotaPolicyMode::Disabled => QuotaPolicy::default(),
            QuotaPolicyMode::Custom => st.custom.clone().unwrap(),
        };
        let mut status = st.status.clone();
        status.pending_operation_id = st
            .ops
            .values()
            .find(|op| op.state == "pending" || st.readbacks.contains(&op.id))
            .map(|op| op.id.clone());
        Ok(AccountQuotaPolicy {
            account_id: id.into(),
            revision: 0,
            mode: st.mode,
            policy: st.custom.clone(),
            effective_policy: effective,
            source: "global".into(),
            status,
        })
    }
    async fn update_account(
        &self,
        id: &str,
        _: u64,
        mode: QuotaPolicyMode,
        policy: Option<QuotaPolicy>,
        _: &str,
    ) -> Result<AccountQuotaPolicy, QuotaPolicyError> {
        {
            let mut st = self.state.lock().unwrap();
            st.mode = mode;
            st.custom = policy;
        }
        self.account(id).await
    }
    async fn status(
        &self,
        _: &str,
        paused: bool,
        reason: &str,
        at: Option<chrono::DateTime<Utc>>,
    ) -> Result<(), QuotaPolicyError> {
        let mut st = self.state.lock().unwrap();
        st.status.paused = paused;
        st.status.reason = reason.into();
        st.status.observed_at = at;
        Ok(())
    }
    async fn lock_reset(&self, _: &str) -> Result<Box<dyn ResetOperationGuard>, QuotaPolicyError> {
        if self.locked.swap(true, Ordering::SeqCst) {
            return Err(QuotaPolicyError::Conflict);
        }
        Ok(Box::new(Guard {
            locked: self.locked.clone(),
            valid: self.lease_valid.clone(),
        }))
    }
    async fn claim_reset(&self, claim: ResetClaim) -> Result<ResetOperation, QuotaPolicyError> {
        let delay = self.claim_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
        let mut st = self.state.lock().unwrap();
        let op = claim.operation;
        if let Some(existing) = st.ops.get(&op.id) {
            if existing.credit_id != op.credit_id || existing.automatic != op.automatic {
                return Err(QuotaPolicyError::Conflict);
            }
            return Ok(existing.clone());
        }
        if st
            .ops
            .values()
            .any(|p| p.state == "pending" || st.readbacks.contains(&p.id))
        {
            return Err(QuotaPolicyError::Pending);
        }
        if claim.global_revision != st.global.revision {
            return Err(QuotaPolicyError::Conflict);
        }
        if op.automatic && st.ops.values().any(|p| p.automatic) {
            return Err(QuotaPolicyError::Budget);
        }
        st.ops.insert(op.id.clone(), op.clone());
        Ok(op)
    }
    async fn finish_reset(
        &self,
        id: &str,
        state: &str,
        code: Option<&str>,
    ) -> Result<(), QuotaPolicyError> {
        if self.fail_finish.load(Ordering::SeqCst) {
            return Err(QuotaPolicyError::Unavailable);
        }
        let mut st = self.state.lock().unwrap();
        let op = st.ops.get_mut(id).unwrap();
        op.state = state.into();
        op.result_code = code.map(str::to_owned);
        if op.automatic && state == "confirmed" {
            st.readbacks.insert(id.into());
        }
        Ok(())
    }
    async fn reset_operation(&self, id: &str) -> Result<Option<ResetOperation>, QuotaPolicyError> {
        Ok(self.state.lock().unwrap().ops.get(id).cloned())
    }
    async fn pending_reset(&self, _: &str) -> Result<Option<ResetOperation>, QuotaPolicyError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .ops
            .values()
            .find(|op| op.state == "pending" || st.readbacks.contains(&op.id))
            .cloned())
    }
    async fn confirm_readback(
        &self,
        id: &str,
        global_revision: u64,
        _: u64,
    ) -> Result<(), QuotaPolicyError> {
        if self.fail_readback.load(Ordering::SeqCst) {
            return Err(QuotaPolicyError::Unavailable);
        }
        let mut st = self.state.lock().unwrap();
        if self.change_on_readback.swap(false, Ordering::SeqCst) {
            st.global.revision += 1;
            st.global.policy.primary.action = QuotaAction::ResetThenStop;
            st.global.policy.primary.threshold_percent = 90;
        }
        if st.global.revision != global_revision {
            return Err(QuotaPolicyError::Conflict);
        }
        st.readbacks.remove(id);
        Ok(())
    }
}

fn usage(primary: f64, secondary: f64, seconds: u64) -> serde_json::Value {
    json!({"rate_limit":{"allowed":true,"primary_window":{"used_percent":primary,"limit_window_seconds":18000,"reset_at":Utc::now().timestamp()+10000},"secondary_window":{"used_percent":secondary,"limit_window_seconds":seconds,"reset_at":Utc::now().timestamp()+100000}},"credits":{"has_credits":true,"unlimited":false,"balance":"9007199254740993.125"}})
}
fn service(
    accounts: &Arc<MemoryAccountStore>,
    policy: Arc<PolicyStore>,
    url: String,
) -> Arc<CodexCredentialQuotaService> {
    Arc::new(
        quota_service_with_base_url(
            accounts,
            reqwest::Client::builder()
                .no_proxy()
                // mock 路由失配时也不得请求真实官方账号接口。
                .resolve("chatgpt.com", "127.0.0.1:9".parse().unwrap())
                .build()
                .unwrap(),
            url,
        )
        .with_quota_policy(Some(policy)),
    )
}

async fn selected(
    accounts: &Arc<MemoryAccountStore>,
    quota: Arc<CodexCredentialQuotaService>,
) -> bool {
    let leases = Arc::new(crate::support::TestLeaseCoordinator::default());
    let selector = CodexCredentialSelector::new(
        ProviderKind::new("openai").unwrap(),
        accounts.repository(),
        leases,
        Arc::new(crate::support::MemorySessionAffinity::default()),
        Arc::new(crate::support::MemorySessionExclusions::default()),
        quota,
        Arc::new(AccountFeedbackStats::default()),
        CodexCookiePolicy::official().unwrap(),
    );
    let id = ProviderAccountId::new("acct_policy").unwrap();
    let scope = Arc::new(FrozenAccountScope::new(
        Arc::new(RuntimeAccountDirectory::new(BTreeMap::from([(
            id,
            RuntimeAccount::new(ProviderKind::new("openai").unwrap(), BTreeSet::new()),
        )]))),
        ClientRoutingScope::all_accounts(),
    ));
    let attempt = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_policy").unwrap(),
            ClientApiKeyId::new("key_policy").unwrap(),
        ),
        NonZeroU32::new(1).unwrap(),
        SystemTime::now() + Duration::from_secs(10),
        crate::support::account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None).with_account_scope(scope),
        None,
        CancellationToken::new(),
    );
    selector
        .select(&SelectCodexCredential {
            upstream_model: "gpt-5.4",
            request_url: &Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap(),
            attempt: &attempt,
            session_affinity_key: None,
        })
        .await
        .is_ok()
}

#[tokio::test]
async fn weekly_stop_uses_raw_boundary_not_rank_and_preserves_allowed_and_credits() {
    for (used, expected) in [
        (98.999, true),
        (99.0, false),
        (99.001, false),
        (100.0, false),
    ] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage(100.0, used, 604800)))
            .mount(&server)
            .await;
        let quota = service(&accounts, PolicyStore::new(QuotaAction::Stop), server.uri());
        let snapshot = quota
            .refresh_account(&ProviderAccountId::new("acct_policy").unwrap())
            .await
            .unwrap();
        assert_eq!(snapshot.quota().access(), QuotaAccessState::Allowed);
        assert_eq!(
            snapshot.credits().unwrap().balance.as_deref(),
            Some("9007199254740993.125")
        );
        assert_eq!(
            selected(&accounts, quota).await,
            expected,
            "weekly used={used}"
        );
        assert_eq!(
            accounts.account("acct_policy").unwrap().quota().access(),
            QuotaAccessState::Allowed
        );
    }
}

#[tokio::test]
async fn monthly_secondary_and_code_review_do_not_trigger_weekly_stop_and_disabled_restores() {
    for seconds in [2592000, 604800] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        let mut value = usage(100.0, if seconds == 604800 { 98.0 } else { 100.0 }, seconds);
        value["additional_rate_limits"] = json!([{"limit_name":"code_review","rate_limit":{"allowed":false,"secondary_window":{"used_percent":100,"limit_window_seconds":604800}}}]);
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(&server)
            .await;
        let policy = PolicyStore::new(QuotaAction::Stop);
        let quota = service(&accounts, policy.clone(), server.uri());
        quota
            .refresh_account(&ProviderAccountId::new("acct_policy").unwrap())
            .await
            .unwrap();
        assert!(selected(&accounts, quota.clone()).await);
        policy
            .update_account("acct_policy", 0, QuotaPolicyMode::Disabled, None, "")
            .await
            .unwrap();
        assert!(selected(&accounts, quota).await);
    }
}

#[tokio::test]
async fn unobserved_policy_account_pauses_without_consumption_and_off_preserves_baseline() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    let policy = PolicyStore::new(QuotaAction::Stop);
    let quota = service(&accounts, policy.clone(), "http://127.0.0.1:1".into());
    assert!(!selected(&accounts, quota.clone()).await);
    policy
        .update_global(0, QuotaPolicy::default(), "")
        .await
        .unwrap();
    assert!(selected(&accounts, quota).await);
    assert!(policy.state.lock().unwrap().ops.is_empty());
}

#[tokio::test]
async fn auto_unknown_restart_confirms_same_key_and_high_readback_never_chains() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(10.0, 99.0, 604800)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/rate-limit-reset-credits"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"available_count":100,"credits":[]})),
        )
        .mount(&server)
        .await;
    let sends = Arc::new(AtomicUsize::new(0));
    let count = sends.clone();
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(move |_: &wiremock::Request| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_string("not-json")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"code":"already_redeemed"}))
            }
        })
        .mount(&server)
        .await;
    let policy = PolicyStore::new(QuotaAction::ResetThenStop);
    service(&accounts, policy.clone(), server.uri())
        .synchronize()
        .await
        .unwrap();
    assert_eq!(
        sends.load(Ordering::SeqCst),
        1,
        "status={:?}, requests={:?}",
        policy.account("acct_policy").await.unwrap().status,
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path())
            .collect::<Vec<_>>()
    );
    let original = policy.pending_reset("acct_policy").await.unwrap().unwrap();
    assert_eq!(original.state, "pending");
    service(&accounts, policy.clone(), server.uri())
        .synchronize()
        .await
        .unwrap();
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    assert_eq!(
        policy
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .state,
        "confirmed"
    );
    service(&accounts, policy.clone(), server.uri())
        .synchronize()
        .await
        .unwrap();
    assert_eq!(
        sends.load(Ordering::SeqCst),
        2,
        "high usage must not spend next card"
    );
    let requests = server.received_requests().await.unwrap();
    let bodies = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[0]["redeem_request_id"], original.id);
    assert_eq!(policy.state.lock().unwrap().ops.len(), 1);
}

#[tokio::test]
async fn no_inventory_expired_card_cancelled_worker_and_lost_guard_never_send() {
    for scenario in ["none", "expired", "cancelled", "lease"] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage(10.0, 100.0, 604800)))
            .mount(&server)
            .await;
        let cards = if scenario == "expired" {
            json!([{"id":"card","status":"available","expires_at":"2000-01-01T00:00:00Z"}])
        } else {
            json!([])
        };
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"available_count":if scenario=="none"{0}else{100},"credits":cards}),
            ))
            .mount(&server)
            .await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        if scenario == "lease" {
            policy.lease_valid.store(false, Ordering::SeqCst);
        }
        let cancel = CancellationToken::new();
        if scenario == "cancelled" {
            cancel.cancel();
        }
        service(&accounts, policy, server.uri())
            .synchronize_with_cancellation(&cancel)
            .await
            .unwrap();
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method.as_str() != "POST"),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn malformed_credits_do_not_discard_valid_quota_or_forge_zero() {
    for credits in [
        json!(null),
        json!(false),
        json!({"has_credits":true,"unlimited":false,"balance":null}),
        json!({"has_credits":true,"unlimited":false,"balance":0}),
        json!({"has_credits":true,"unlimited":false,"balance":12.75}),
    ] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        let mut value = usage(10.0, 98.0, 604800);
        value["credits"] = credits.clone();
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(&server)
            .await;
        let quota = service(&accounts, PolicyStore::new(QuotaAction::Stop), server.uri());
        let snapshot = quota
            .refresh_account(&ProviderAccountId::new("acct_policy").unwrap())
            .await
            .unwrap();
        assert_eq!(snapshot.windows().len(), 2);
        assert_eq!(snapshot.quota().access(), QuotaAccessState::Allowed);
        let expected = credits
            .get("balance")
            .and_then(|v| v.as_number())
            .map(ToString::to_string);
        assert_eq!(
            snapshot.credits().and_then(|c| c.balance.as_ref()),
            expected.as_ref()
        );
    }
}

#[tokio::test]
async fn unknown_confirmation_rejections_do_not_unlock_a_new_card_or_budget() {
    for rejection in ["no_credit", "nothing_to_reset", "http429", "future_code"] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage(10.0, 99.0, 604800)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"available_count":100,"credits":[]})),
            )
            .mount(&server)
            .await;
        let sends = Arc::new(AtomicUsize::new(0));
        let count = sends.clone();
        Mock::given(method("POST"))
            .and(path("/api/codex/rate-limit-reset-credits/consume"))
            .respond_with(move |_: &wiremock::Request| {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200).set_body_string("invalid-json")
                } else if rejection == "http429" {
                    ResponseTemplate::new(429).set_body_json(json!({"code":"slow_down"}))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"code":rejection}))
                }
            })
            .mount(&server)
            .await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        let original = policy.pending_reset("acct_policy").await.unwrap().unwrap();
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 2, "{rejection}");
        let pending = policy.pending_reset("acct_policy").await.unwrap().unwrap();
        assert_eq!(pending.id, original.id);
        assert_eq!(
            pending.state, "pending",
            "confirmation rejection is not prior failure proof"
        );
        assert_eq!(policy.state.lock().unwrap().ops.len(), 1);
        policy
            .update_global(0, QuotaPolicy::default(), "")
            .await
            .unwrap();
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        assert_eq!(
            sends.load(Ordering::SeqCst),
            2,
            "off must not replay pending"
        );
        assert!(policy.pending_reset("acct_policy").await.unwrap().is_some());
    }
}

#[tokio::test]
async fn sent_success_with_result_store_failure_is_unknown_and_keeps_original_manual_key() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    let server = MockServer::start().await;
    let policy = PolicyStore::new(QuotaAction::Off);
    policy.fail_finish.store(true, Ordering::SeqCst);
    let persisted = policy.clone();
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert!(
                persisted
                    .state
                    .lock()
                    .unwrap()
                    .ops
                    .contains_key(body["redeem_request_id"].as_str().unwrap()),
                "claim must exist before send"
            );
            ResponseTemplate::new(200).set_body_json(json!({"code":"reset"}))
        })
        .mount(&server)
        .await;
    let quota = service(&accounts, policy.clone(), server.uri());
    let id = ProviderAccountId::new("acct_policy").unwrap();
    let key = uuid::Uuid::new_v4();
    assert!(matches!(
        quota.consume_reset_credit(&id, None, key).await,
        Err(provider_openai::credential::CodexResetCreditsError::ConsumeResultUnknown)
    ));
    assert_eq!(
        policy
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .id,
        key.to_string()
    );
    assert!(matches!(
        quota
            .consume_reset_credit(&id, None, uuid::Uuid::new_v4())
            .await,
        Err(provider_openai::credential::CodexResetCreditsError::Policy(
            QuotaPolicyError::Pending
        ))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn configuration_changed_during_inventory_preparation_cannot_consume() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(10.0, 99.0, 604800)))
        .mount(&server)
        .await;
    let policy = PolicyStore::new(QuotaAction::ResetThenStop);
    let change = policy.clone();
    Mock::given(method("GET"))
        .and(path("/api/codex/rate-limit-reset-credits"))
        .respond_with(move |_: &wiremock::Request| {
            let mut st = change.state.lock().unwrap();
            st.global.revision += 1;
            st.global.policy = QuotaPolicy::default();
            ResponseTemplate::new(200).set_body_json(json!({"available_count":100,"credits":[]}))
        })
        .mount(&server)
        .await;
    service(&accounts, policy.clone(), server.uri())
        .synchronize()
        .await
        .unwrap();
    assert!(policy.state.lock().unwrap().ops.is_empty());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method.as_str() != "POST")
    );
}

#[tokio::test]
async fn confirmed_readback_requires_original_window_and_low_all_active_rules() {
    for recovery in ["missing", "other_high", "low"] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        let was_sent = Arc::new(AtomicBool::new(false));
        let usage_after = was_sent.clone();
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(move |_: &wiremock::Request| {
                let mut value = if usage_after.load(Ordering::SeqCst) {
                    usage(
                        if recovery == "other_high" { 95.0 } else { 10.0 },
                        10.0,
                        604800,
                    )
                } else {
                    usage(10.0, 99.0, 604800)
                };
                if usage_after.load(Ordering::SeqCst) && recovery == "missing" {
                    value["rate_limit"]
                        .as_object_mut()
                        .unwrap()
                        .remove("secondary_window");
                }
                ResponseTemplate::new(200).set_body_json(value)
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"available_count":100,"credits":[]})),
            )
            .mount(&server)
            .await;
        let set_sent = was_sent.clone();
        Mock::given(method("POST"))
            .and(path("/api/codex/rate-limit-reset-credits/consume"))
            .respond_with(move |_: &wiremock::Request| {
                set_sent.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({"code":"reset"}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        {
            let mut st = policy.state.lock().unwrap();
            st.global.policy.primary.action = QuotaAction::Stop;
            st.global.policy.primary.threshold_percent = 90;
        }
        let quota = service(&accounts, policy.clone(), server.uri());
        quota.synchronize().await.unwrap();
        let pending = policy.pending_reset("acct_policy").await.unwrap();
        assert_eq!(pending.is_none(), recovery == "low", "{recovery}");
        if let Some(op) = pending {
            assert_eq!(op.state, "confirmed");
        }
        assert_eq!(selected(&accounts, quota).await, recovery == "low");
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        assert_eq!(
            policy.state.lock().unwrap().ops.len(),
            1,
            "readback must not become a new consume"
        );
    }
}

#[tokio::test]
async fn disabled_and_untrusted_credential_accounts_never_auto_consume() {
    for state in [
        CredentialState::Unknown,
        CredentialState::Expired,
        CredentialState::Invalid,
        CredentialState::Banned,
        CredentialState::Ready,
    ] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account_with_enabled(&accounts, "acct_policy", state != CredentialState::Ready)
            .await;
        if state != CredentialState::Ready {
            let account = accounts.account("acct_policy").unwrap();
            persist_credential_state(&accounts, &account, state).await;
        }
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage(100.0, 100.0, 604800)))
            .mount(&server)
            .await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        assert!(policy.state.lock().unwrap().ops.is_empty(), "{state:?}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method.as_str() != "POST")
        );
    }
}

fn pending_operation(episode: String) -> ResetOperation {
    ResetOperation {
        id: Uuid::new_v4().to_string(),
        account_id: "acct_policy".into(),
        credential_revision: 1,
        credit_id: None,
        automatic: true,
        episode: Some(episode),
        trigger_percent: Some(99),
        state: "pending".into(),
        result_code: None,
    }
}

#[tokio::test]
async fn review_r1_pending_original_rule_must_still_authorize_auto_even_when_other_rule_enabled() {
    for interrupted in [true, false] {
        for change in ["stop", "original_off_other_auto", "disabled"] {
            let accounts = Arc::new(MemoryAccountStore::default());
            create_account(&accounts, "acct_policy").await;
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/codex/usage"))
                .respond_with(ResponseTemplate::new(200).set_body_json(usage(95.0, 99.0, 604800)))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/codex/rate-limit-reset-credits"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"available_count":100,"credits":[]})),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/codex/rate-limit-reset-credits/consume"))
                .respond_with(ResponseTemplate::new(200).set_body_string("unknown-json"))
                .mount(&server)
                .await;
            let policy = PolicyStore::new(QuotaAction::ResetThenStop);
            policy.lease_valid.store(!interrupted, Ordering::SeqCst);
            service(&accounts, policy.clone(), server.uri())
                .synchronize()
                .await
                .unwrap();
            let original = policy.pending_reset("acct_policy").await.unwrap().unwrap();
            let before = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method.as_str() == "POST")
                .count();
            assert_eq!(before, usize::from(!interrupted));
            {
                let mut st = policy.state.lock().unwrap();
                st.global.revision += 1;
                match change {
                    "stop" => st.global.policy.secondary.action = QuotaAction::Stop,
                    "original_off_other_auto" => {
                        st.global.policy.secondary.action = QuotaAction::Off;
                        st.global.policy.primary.action = QuotaAction::ResetThenStop;
                        st.global.policy.primary.threshold_percent = 90;
                    }
                    _ => st.mode = QuotaPolicyMode::Disabled,
                }
            }
            policy.lease_valid.store(true, Ordering::SeqCst);
            service(&accounts, policy.clone(), server.uri())
                .synchronize()
                .await
                .unwrap();
            assert_eq!(
                server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|r| r.method.as_str() == "POST")
                    .count(),
                before,
                "interrupted={interrupted}, change={change}"
            );
            assert_eq!(
                policy
                    .pending_reset("acct_policy")
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                original.id
            );
            assert_eq!(
                policy.state.lock().unwrap().ops.len(),
                1,
                "claim/budget history stays"
            );
        }
    }
}

#[tokio::test]
async fn review_r2_terminal_manual_result_is_read_only_even_after_rotation_and_token_expiry() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    create_account(&accounts, "acct_other").await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":"reset"})))
        .expect(1)
        .mount(&server)
        .await;
    let policy = PolicyStore::new(QuotaAction::Off);
    let quota = service(&accounts, policy.clone(), server.uri());
    let id = ProviderAccountId::new("acct_policy").unwrap();
    let key = Uuid::new_v4();
    assert_eq!(
        quota
            .consume_reset_credit(&id, Some("card"), key)
            .await
            .unwrap()
            .action_result
            .as_str(),
        "confirmed"
    );
    let account = accounts.account("acct_policy").unwrap();
    accounts
        .repository()
        .rotate_refreshed_oauth_secret(
            &account,
            secret("new-token"),
            Some(SystemTime::now() - Duration::from_secs(1)),
            None,
        )
        .await
        .unwrap();
    assert!(accounts.account("acct_policy").unwrap().revision().get() > account.revision().get());
    let repeated = quota.consume_reset_credit(&id, Some("card"), key).await;
    assert!(
        repeated.is_ok(),
        "terminal lookup must not require current token: {repeated:?}"
    );
    assert_eq!(repeated.unwrap().action_result.as_str(), "confirmed");
    assert_eq!(
        policy.state.lock().unwrap().ops[&key.to_string()].credential_revision,
        account.revision().get(),
        "terminal revision never migrates"
    );
    assert!(
        quota
            .consume_reset_credit(&id, Some("other-card"), key)
            .await
            .is_err()
    );
    assert!(
        quota
            .consume_reset_credit(
                &ProviderAccountId::new("acct_other").unwrap(),
                Some("card"),
                key
            )
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn review_r3_both_readback_entries_observe_policy_changed_during_usage_read() {
    for entry in ["immediate", "worker"] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        let server = MockServer::start().await;
        let sent = Arc::new(AtomicBool::new(entry == "worker"));
        if entry == "worker" {
            let mut op = pending_operation(format!(
                "threshold_secondary:{}",
                Utc::now().timestamp() + 100000
            ));
            op.state = "confirmed".into();
            let mut st = policy.state.lock().unwrap();
            st.readbacks.insert(op.id.clone());
            st.ops.insert(op.id.clone(), op);
        }
        let changed = policy.clone();
        let was_sent = sent.clone();
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(move |_: &wiremock::Request| {
                if was_sent.load(Ordering::SeqCst) {
                    let mut st = changed.state.lock().unwrap();
                    st.global.revision += 1;
                    st.global.policy.primary.action = QuotaAction::ResetThenStop;
                    st.global.policy.primary.threshold_percent = 90;
                    ResponseTemplate::new(200).set_body_json(usage(95.0, 10.0, 604800))
                } else {
                    ResponseTemplate::new(200).set_body_json(usage(10.0, 99.0, 604800))
                }
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"available_count":100,"credits":[]})),
            )
            .mount(&server)
            .await;
        let set_sent = sent.clone();
        Mock::given(method("POST"))
            .and(path("/api/codex/rate-limit-reset-credits/consume"))
            .respond_with(move |_: &wiremock::Request| {
                set_sent.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({"code":"reset"}))
            })
            .mount(&server)
            .await;
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        assert!(
            policy.pending_reset("acct_policy").await.unwrap().is_some(),
            "entry={entry}: current high primary must preserve barrier"
        );
        assert_eq!(
            policy.account("acct_policy").await.unwrap().status.reason,
            "reset_readback_unconfirmed"
        );
        assert_eq!(policy.state.lock().unwrap().ops.len(), 1);
    }
}

#[tokio::test]
async fn review_r4_inventory_crossing_trigger_window_boundary_never_sends_or_replays_claim() {
    for crosses in [true, false] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let server = MockServer::start().await;
        let mut value = usage(10.0, 99.0, 604800);
        value["rate_limit"]["secondary_window"]["reset_at"] =
            json!(Utc::now().timestamp() + if crosses { 2 } else { 100000 });
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"available_count":100,"credits":[]}))
                    .set_delay(Duration::from_millis(if crosses { 2200 } else { 20 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/codex/rate-limit-reset-credits/consume"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":"reset"})))
            .mount(&server)
            .await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        let post_count = || async {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method.as_str() == "POST")
                .count()
        };
        assert_eq!(
            post_count().await,
            usize::from(!crosses),
            "crosses={crosses}"
        );
        if crosses {
            service(&accounts, policy.clone(), server.uri())
                .synchronize()
                .await
                .unwrap();
            assert_eq!(
                post_count().await,
                0,
                "expired unsent pending cannot bypass trigger on restart"
            );
        }
    }
}

#[tokio::test]
async fn review_r3_cas_race_or_store_failure_never_marks_readback_ready() {
    for failure in ["revision", "store"] {
        let accounts = Arc::new(MemoryAccountStore::default());
        create_account(&accounts, "acct_policy").await;
        let policy = PolicyStore::new(QuotaAction::ResetThenStop);
        policy
            .change_on_readback
            .store(failure == "revision", Ordering::SeqCst);
        policy
            .fail_readback
            .store(failure == "store", Ordering::SeqCst);
        let mut op = pending_operation(format!(
            "threshold_secondary:{}",
            Utc::now().timestamp() + 100000
        ));
        op.state = "confirmed".into();
        {
            let mut st = policy.state.lock().unwrap();
            st.readbacks.insert(op.id.clone());
            st.ops.insert(op.id.clone(), op.clone());
        }
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage(95.0, 10.0, 604800)))
            .mount(&server)
            .await;
        service(&accounts, policy.clone(), server.uri())
            .synchronize()
            .await
            .unwrap();
        assert_eq!(
            policy
                .pending_reset("acct_policy")
                .await
                .unwrap()
                .unwrap()
                .id,
            op.id
        );
        assert_eq!(
            policy.account("acct_policy").await.unwrap().status.reason,
            "reset_readback_unconfirmed",
            "{failure}"
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method.as_str() != "POST")
        );
    }
}

#[tokio::test]
async fn review_r4_initial_claim_records_effective_threshold_after_worker_policy_changes() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    let policy = PolicyStore::new(QuotaAction::ResetThenStop);
    let changed = policy.clone();
    let reads = Arc::new(AtomicUsize::new(0));
    let first_read = reads.clone();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(move |_: &wiremock::Request| {
            if first_read.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut st = changed.state.lock().unwrap();
                st.global.revision += 1;
                st.global.policy.secondary.threshold_percent = 90;
            }
            ResponseTemplate::new(200).set_body_json(usage(10.0, 99.0, 604800))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/rate-limit-reset-credits"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"available_count":100,"credits":[]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(200).set_body_string("unknown-json"))
        .expect(1)
        .mount(&server)
        .await;
    service(&accounts, policy.clone(), server.uri())
        .synchronize()
        .await
        .unwrap();
    assert_eq!(
        policy
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .trigger_percent,
        Some(90)
    );
}

#[tokio::test]
async fn review_r4_pending_confirmation_claim_delay_cannot_cross_original_trigger_boundary() {
    let accounts = Arc::new(MemoryAccountStore::default());
    create_account(&accounts, "acct_policy").await;
    let policy = PolicyStore::new(QuotaAction::ResetThenStop);
    let reset = Utc::now().timestamp() + 2;
    let op = pending_operation(format!("threshold_secondary:{reset}"));
    policy
        .state
        .lock()
        .unwrap()
        .ops
        .insert(op.id.clone(), op.clone());
    policy.claim_delay_ms.store(2200, Ordering::SeqCst);
    let server = MockServer::start().await;
    let mut value = usage(10.0, 99.0, 604800);
    value["rate_limit"]["secondary_window"]["reset_at"] = json!(reset);
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":"reset"})))
        .expect(0)
        .mount(&server)
        .await;
    service(&accounts, policy.clone(), server.uri())
        .synchronize()
        .await
        .unwrap();
    assert_eq!(
        policy
            .pending_reset("acct_policy")
            .await
            .unwrap()
            .unwrap()
            .id,
        op.id
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method.as_str() != "POST")
    );
}
