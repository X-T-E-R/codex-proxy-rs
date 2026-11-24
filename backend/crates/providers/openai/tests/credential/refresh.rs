use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeZone as _, Utc};
use futures::future::BoxFuture;
use gateway_core::account::{
    AccountErrorReason, AccountFeedbackStats, AccountStateChange, AccountStatus, CredentialState,
    ProviderAccountId, ProviderAccountStore, QuotaAccessChange, QuotaEvidence, QuotaState,
    QuotaWriteOutcome,
};
use gateway_core::engine::{
    AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::policy::ClientApiKeyId;
use gateway_core::provider_ports::{
    ProviderCredentialState, ProviderCredentialStatePort, ProviderLeaseAcquisition,
    ProviderLeasePort, ProviderLeaseRequest, ProviderRefreshPolicy, ProviderRuntimePolicyPort,
    ProviderSchedulingState, ProviderStoreError,
};
use gateway_core::routing::{
    ClientRoutingScope, FrozenAccountScope, ProviderKind, RuntimeAccount, RuntimeAccountDirectory,
};
use provider_openai::OFFICIAL_CODEX_BASE_URL;
use provider_openai::credential::token_client::{
    OpenAiTokenClient, RefreshFailure, TokenClientConfig, TokenPair, TokenRefresher,
};
use provider_openai::credential::{
    CodexAccountFailure, CodexCookiePolicy, CodexCredentialCatalogService, CodexCredentialCodec,
    CodexCredentialQuotaService, CodexCredentialRefreshOutcome, CodexCredentialRefreshService,
    CodexCredentialSelector, CredentialSelectionError, ImportCodexOAuthCredential,
    SelectCodexCredential,
};
use secrecy::{ExposeSecret as _, SecretString};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{
    MemoryAccountStore, MemoryCooldownPort, MemorySessionAffinity, MemorySessionExclusions,
    TestLeaseCoordinator, account_policy, catalog_cache, profile, secret,
};

struct SingleUseRefresher {
    calls: AtomicUsize,
    response: Mutex<Option<TokenPair>>,
}

impl SingleUseRefresher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            response: Mutex::new(Some(TokenPair {
                access_token: Some("refreshed-access-token".to_owned()),
                refresh_token: None,
                id_token: None,
            })),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn with_id_token(id_token: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            response: Mutex::new(Some(TokenPair {
                access_token: Some("refreshed-access-token".to_owned()),
                refresh_token: None,
                id_token: Some(id_token.to_owned()),
            })),
        })
    }

    fn with_access_token(access_token: String) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            response: Mutex::new(Some(TokenPair {
                access_token: Some(access_token),
                refresh_token: None,
                id_token: None,
            })),
        })
    }

    fn without_rotated_tokens() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            response: Mutex::new(Some(TokenPair {
                access_token: None,
                refresh_token: None,
                id_token: None,
            })),
        })
    }
}

#[async_trait]
impl TokenRefresher for SingleUseRefresher {
    async fn refresh(&self, _: &str) -> Result<TokenPair, RefreshFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.response
            .lock()
            .expect("refresh response lock")
            .take()
            .ok_or_else(|| RefreshFailure::Transport {
                message: Some("test refresh response is exhausted".to_owned()),
                upstream: None,
            })
    }
}

struct FailingRefresher {
    failure: RefreshFailure,
}

#[async_trait]
impl TokenRefresher for FailingRefresher {
    async fn refresh(&self, _: &str) -> Result<TokenPair, RefreshFailure> {
        Err(self.failure.clone())
    }
}

struct RefreshLeases;

impl ProviderLeasePort for RefreshLeases {
    fn load_state<'a>(
        &'a self,
        _: &'a ClientApiKeyId,
        _: &'a ProviderKind,
        _: &'a [ProviderAccountId],
    ) -> BoxFuture<'a, Result<ProviderSchedulingState, ProviderStoreError>> {
        Box::pin(async { panic!("scheduled credential refresh does not load scheduling state") })
    }

    fn try_acquire(
        &self,
        request: ProviderLeaseRequest,
    ) -> BoxFuture<'_, Result<ProviderLeaseAcquisition, ProviderStoreError>> {
        Box::pin(async move {
            match request {
                ProviderLeaseRequest::RefreshCapacity(_) | ProviderLeaseRequest::Refresh(_) => {
                    Ok(ProviderLeaseAcquisition::Acquired(Box::new(())))
                }
                ProviderLeaseRequest::Scheduling(_) => {
                    panic!("scheduled credential refresh must not acquire a scheduling lease")
                }
            }
        })
    }
}

struct RefreshCredentialState;

impl ProviderCredentialStatePort for RefreshCredentialState {
    fn replace(&self, _: ProviderCredentialState) -> BoxFuture<'_, Result<(), ProviderStoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn read<'a>(
        &'a self,
        _: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<Option<ProviderCredentialState>, ProviderStoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn clear<'a>(
        &'a self,
        _: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<bool, ProviderStoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn record_refresh_backoff<'a>(
        &'a self,
        _: &'a ProviderAccountId,
        _: Duration,
    ) -> BoxFuture<'a, Result<u32, ProviderStoreError>> {
        Box::pin(async { Ok(1) })
    }

    fn clear_refresh_backoff<'a>(
        &'a self,
        _: &'a ProviderAccountId,
    ) -> BoxFuture<'a, Result<(), ProviderStoreError>> {
        Box::pin(async { Ok(()) })
    }
}

struct MutableRuntimePolicy {
    policy: Mutex<ProviderRefreshPolicy>,
}

impl MutableRuntimePolicy {
    fn new(margin: Duration) -> Arc<Self> {
        Arc::new(Self {
            policy: Mutex::new(refresh_policy(margin)),
        })
    }

    fn set_margin(&self, margin: Duration) {
        *self.policy.lock().expect("runtime policy lock") = refresh_policy(margin);
    }
}

impl ProviderRuntimePolicyPort for MutableRuntimePolicy {
    fn load_refresh_policy(
        &self,
    ) -> BoxFuture<'_, Result<ProviderRefreshPolicy, ProviderStoreError>> {
        Box::pin(async { Ok(*self.policy.lock().expect("runtime policy lock")) })
    }
}

fn refresh_policy(margin: Duration) -> ProviderRefreshPolicy {
    ProviderRefreshPolicy::try_new(margin, NonZeroU32::new(1).expect("positive concurrency"))
        .expect("valid refresh policy")
}

fn refresh_service(
    store: &Arc<MemoryAccountStore>,
    refresher: Arc<SingleUseRefresher>,
    runtime_policy: Arc<MutableRuntimePolicy>,
) -> CodexCredentialRefreshService {
    CodexCredentialRefreshService::new(
        store.repository(),
        refresher,
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        runtime_policy,
    )
}

async fn seed_refreshable_account(
    store: &Arc<MemoryAccountStore>,
    account_id: &str,
    access_token_expires_at: SystemTime,
    retry_not_before: Option<SystemTime>,
) {
    let mut verified_account = profile(&format!("chatgpt-{account_id}"));
    verified_account.access_token_expires_at = Some(DateTime::<Utc>::from(access_token_expires_at));
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: account_id.to_owned(),
            name: account_id.to_owned(),
            secret: secret(&format!("access-{account_id}")),
            verified_account,
            next_refresh_at: retry_not_before.map(DateTime::<Utc>::from),
            enabled: true,
        })
        .await;
}

#[tokio::test]
async fn scheduled_refresh_uses_the_current_margin_without_persisting_a_normal_schedule() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(1));
    let refresher = SingleUseRefresher::new();
    let service = refresh_service(&store, Arc::clone(&refresher), Arc::clone(&policy));
    let account_id = "acct_dynamic_margin";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_add(Duration::from_secs(120))
            .expect("test expiry"),
        None,
    )
    .await;

    assert!(
        service
            .refresh_due()
            .await
            .expect("refresh cycle below margin")
            .is_empty()
    );
    assert_eq!(refresher.calls(), 0);
    assert!(
        store
            .account(account_id)
            .expect("seeded account")
            .next_refresh_at()
            .is_none()
    );

    policy.set_margin(Duration::from_secs(5 * 60));
    let outcomes = service
        .refresh_due()
        .await
        .expect("refresh cycle inside margin");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Refreshed {
            account_id: refreshed_account_id,
            ..
        }] if refreshed_account_id == account_id
    ));
    assert_eq!(refresher.calls(), 1);
    assert!(
        store
            .account(account_id)
            .expect("refreshed account")
            .next_refresh_at()
            .is_none()
    );
}

#[tokio::test]
async fn scheduled_refresh_rotates_tokens_while_quota_is_exhausted() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let service = refresh_service(&store, SingleUseRefresher::new(), policy);
    let account_id = "acct_exhausted_quota_refresh";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_add(Duration::from_secs(120))
            .expect("test expiry"),
        None,
    )
    .await;
    let account = store.account(account_id).expect("seeded account");
    assert_eq!(
        store
            .apply_quota_access(QuotaAccessChange {
                account_id: account.id().clone(),
                expected_revision: account.revision(),
                state: QuotaState::exhausted(
                    QuotaEvidence::UsageLimitReached,
                    SystemTime::now(),
                    None,
                ),
            })
            .await
            .expect("persist exhausted quota"),
        QuotaWriteOutcome::Updated
    );

    let outcomes = service.refresh_due().await.expect("refresh cycle");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Refreshed {
            account_id: refreshed_account_id,
            ..
        }] if refreshed_account_id == account_id
    ));
    let account = store.account(account_id).expect("refreshed account");
    assert!(account.quota().is_exhausted());
    let loaded = store
        .load_credential(account.id(), account.revision())
        .await
        .expect("refreshed credential");
    let runtime = CodexCredentialCodec::decode(&loaded.credential).expect("runtime credential");
    let oauth = runtime.authentication.oauth().expect("OAuth credential");
    assert_eq!(oauth.access_token.expose_secret(), "refreshed-access-token");
    let expected_refresh_token = format!("rt-access-{account_id}");
    assert_eq!(
        oauth
            .refresh_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some(expected_refresh_token.as_str())
    );
}

#[tokio::test]
async fn scheduled_refresh_persists_the_original_upstream_error_message() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let upstream_message = "Refresh token has already been used.";
    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(FailingRefresher {
            failure: RefreshFailure::InvalidGrant {
                message: Some(upstream_message.to_owned()),
                upstream: None,
            },
        }),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        policy,
    );
    let account_id = "acct_invalid_refresh_token";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_add(Duration::from_secs(120))
            .expect("test expiry"),
        None,
    )
    .await;

    let outcomes = service.refresh_due().await.expect("refresh cycle");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Invalidated {
            account_id: invalidated_account_id,
        }] if invalidated_account_id == account_id
    ));
    assert_eq!(
        store
            .account(account_id)
            .expect("invalidated account")
            .last_error_message(),
        Some(upstream_message)
    );
}

#[tokio::test]
async fn scheduled_refresh_persists_retryable_message_inside_the_two_hour_window() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let upstream_message = "Invalid refresh token.";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {
                "message": upstream_message,
                "type": "invalid_request_error",
                "code": "invalid_refresh_token"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(OpenAiTokenClient::new(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test HTTP client"),
            TokenClientConfig {
                client_id: "test-public-client".to_owned(),
                token_endpoint: format!("{}/oauth/token", server.uri()),
            },
            provider_openai::OpenAiConfig::default().wire_profile_state(),
        )),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        policy,
    );
    let account_id = "acct_retryable_unauthorized";
    let expires_at = SystemTime::now()
        .checked_sub(Duration::from_secs(30 * 60))
        .expect("expired access token");
    seed_refreshable_account(&store, account_id, expires_at, None).await;

    let outcomes = service.refresh_due().await.expect("refresh cycle");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Transient {
            account_id: deferred_account_id,
        }] if deferred_account_id == account_id
    ));
    let account = store.account(account_id).expect("deferred account");
    assert_eq!(account.credential_state(), CredentialState::Ready);
    assert_eq!(
        account.last_error_reason(),
        Some(AccountErrorReason::AccessTokenExpired)
    );
    assert_eq!(account.last_error_message(), Some(upstream_message));
    let retry_at = account.next_refresh_at().expect("next refresh attempt");
    assert!(retry_at > SystemTime::now());
    assert!(
        retry_at
            <= expires_at
                .checked_add(Duration::from_secs(2 * 60 * 60))
                .expect("recovery deadline")
    );
    let projection = account.status_projection(SystemTime::now(), None);
    assert_eq!(projection.status, AccountStatus::Error);
    assert_eq!(projection.error_message.as_deref(), Some(upstream_message));
}

#[tokio::test]
async fn scheduled_refresh_uses_the_final_message_after_the_two_hour_window() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let upstream_message = "Invalid refresh token.";
    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(FailingRefresher {
            failure: RefreshFailure::Transport {
                message: Some(upstream_message.to_owned()),
                upstream: None,
            },
        }),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        policy,
    );
    let account_id = "acct_exhausted_unauthorized";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_sub(Duration::from_secs(2 * 60 * 60 + 60))
            .expect("expired recovery window"),
        None,
    )
    .await;

    let outcomes = service.refresh_due().await.expect("refresh cycle");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Invalidated {
            account_id: invalidated_account_id,
        }] if invalidated_account_id == account_id
    ));
    let account = store.account(account_id).expect("invalidated account");
    assert_eq!(account.credential_state(), CredentialState::Expired);
    assert_eq!(account.last_error_message(), Some(upstream_message));
}

#[tokio::test]
async fn scheduled_refresh_success_clears_a_deferred_failure_message() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let account_id = "acct_recovered_after_backoff";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_sub(Duration::from_secs(60))
            .expect("expired access token"),
        None,
    )
    .await;
    let account = store.account(account_id).expect("seeded account");
    store
        .apply_state_change(AccountStateChange {
            account_id: account.id().clone(),
            expected_revision: account.revision(),
            credential_state: CredentialState::Ready,
            observed_at: SystemTime::now(),
            error_reason: Some(AccountErrorReason::AccessTokenExpired),
            message: Some("Invalid refresh token.".to_owned()),
        })
        .await
        .expect("seed deferred failure");
    let service = refresh_service(&store, SingleUseRefresher::new(), policy);

    let outcomes = service.refresh_due().await.expect("refresh cycle");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Refreshed {
            account_id: refreshed_account_id,
            ..
        }] if refreshed_account_id == account_id
    ));
    let account = store.account(account_id).expect("refreshed account");
    assert_eq!(account.credential_state(), CredentialState::Ready);
    assert_eq!(account.last_error_reason(), None);
    assert_eq!(account.last_error_message(), None);
}

#[tokio::test]
async fn scheduled_refresh_persists_a_returned_id_token() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let refresher = SingleUseRefresher::with_id_token("header.rotated-id.signature");
    let service = refresh_service(&store, refresher, policy);
    let account_id = "acct_rotated_id_token";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_add(Duration::from_secs(120))
            .expect("test expiry"),
        None,
    )
    .await;

    service.refresh_due().await.expect("refresh cycle");

    let account = store.account(account_id).expect("refreshed account");
    let loaded = store
        .load_credential(account.id(), account.revision())
        .await
        .expect("refreshed credential");
    let runtime = CodexCredentialCodec::decode(&loaded.credential).expect("runtime credential");
    assert_eq!(
        runtime
            .authentication
            .oauth()
            .and_then(|oauth| oauth.id_token.as_ref())
            .map(SecretString::expose_secret),
        Some("header.rotated-id.signature")
    );
}

#[tokio::test]
async fn scheduled_refresh_uses_the_rotated_access_token_jwt_expiration() {
    let expires_at = Utc
        .timestamp_opt(2_000_000_000, 0)
        .single()
        .expect("valid test timestamp");
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({"exp": expires_at.timestamp()}))
            .expect("test JWT payload"),
    );
    let refresher = SingleUseRefresher::with_access_token(format!(
        "unverified-header.{payload}.unverified-signature"
    ));
    let store = Arc::new(MemoryAccountStore::default());
    let service = refresh_service(
        &store,
        refresher,
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );
    let account_id = "acct_rotated_access_expiration";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now()
            .checked_add(Duration::from_secs(120))
            .expect("test expiry"),
        None,
    )
    .await;

    service.refresh_due().await.expect("refresh cycle");

    assert_eq!(
        store
            .account(account_id)
            .expect("refreshed account")
            .access_token_expires_at()
            .map(DateTime::<Utc>::from),
        Some(expires_at)
    );
}

#[tokio::test]
async fn scheduled_refresh_preserves_the_stored_token_set_when_rotation_fields_are_omitted() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let refresher = SingleUseRefresher::without_rotated_tokens();
    let service = refresh_service(&store, refresher, policy);
    let account_id = "acct_omitted_token_rotation";
    let expires_at = SystemTime::now()
        .checked_add(Duration::from_secs(120))
        .expect("test expiry");
    seed_refreshable_account(&store, account_id, expires_at, None).await;

    service.refresh_due().await.expect("refresh cycle");

    let account = store.account(account_id).expect("refreshed account");
    assert_eq!(account.access_token_expires_at(), Some(expires_at));
    let loaded = store
        .load_credential(account.id(), account.revision())
        .await
        .expect("refreshed credential");
    let runtime = CodexCredentialCodec::decode(&loaded.credential).expect("runtime credential");
    let oauth = runtime.authentication.oauth().expect("OAuth credential");
    assert_eq!(
        oauth.access_token.expose_secret(),
        format!("access-{account_id}")
    );
    let expected_refresh_token = format!("rt-access-{account_id}");
    assert_eq!(
        oauth
            .refresh_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some(expected_refresh_token.as_str())
    );
}

#[tokio::test]
async fn scheduled_refresh_respects_retry_not_before_until_it_has_elapsed() {
    let store = Arc::new(MemoryAccountStore::default());
    let policy = MutableRuntimePolicy::new(Duration::from_secs(5 * 60));
    let refresher = SingleUseRefresher::new();
    let service = refresh_service(&store, Arc::clone(&refresher), policy);
    let expires_at = SystemTime::now()
        .checked_add(Duration::from_secs(120))
        .expect("test expiry");
    seed_refreshable_account(
        &store,
        "acct_future_retry",
        expires_at,
        Some(
            SystemTime::now()
                .checked_add(Duration::from_secs(60))
                .expect("future retry"),
        ),
    )
    .await;

    assert!(
        service
            .refresh_due()
            .await
            .expect("refresh cycle before retry")
            .is_empty()
    );
    assert_eq!(refresher.calls(), 0);

    seed_refreshable_account(
        &store,
        "acct_elapsed_retry",
        expires_at,
        Some(
            SystemTime::now()
                .checked_sub(Duration::from_secs(1))
                .expect("past retry"),
        ),
    )
    .await;
    let outcomes = service
        .refresh_due()
        .await
        .expect("refresh cycle after retry");

    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Refreshed {
            account_id,
            ..
        }] if account_id == "acct_elapsed_retry"
    ));
    assert_eq!(refresher.calls(), 1);
}

fn selector_for_store(store: &Arc<MemoryAccountStore>) -> CodexCredentialSelector {
    let provider = ProviderKind::new("openai").expect("provider");
    let wire_profile = provider_openai::OpenAiConfig::default().wire_profile_state();
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("test HTTP client");
    let catalog = Arc::new(CodexCredentialCatalogService::new(
        store.repository(),
        wire_profile.clone(),
        http.clone(),
        OFFICIAL_CODEX_BASE_URL.to_owned(),
        catalog_cache(),
    ));
    let quota = Arc::new(CodexCredentialQuotaService::new(
        store.repository(),
        wire_profile,
        http,
        OFFICIAL_CODEX_BASE_URL.to_owned(),
        Arc::new(MemoryCooldownPort::new()),
    ));
    CodexCredentialSelector::new(
        provider,
        store.repository(),
        Arc::new(TestLeaseCoordinator::default()),
        Arc::new(MemorySessionAffinity::default()),
        Arc::new(MemorySessionExclusions::default()),
        catalog,
        quota,
        Arc::new(AccountFeedbackStats::default()),
        CodexCookiePolicy::official().expect("official cookie policy"),
    )
}

fn live_attempt_for(account_id: &str) -> AttemptContext {
    let provider = ProviderKind::new("openai").expect("provider");
    let directory = Arc::new(RuntimeAccountDirectory::new(BTreeMap::from([(
        ProviderAccountId::new(account_id).expect("account id"),
        RuntimeAccount::new(provider, BTreeSet::new()),
    )])));
    AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_early_401").expect("request id"),
            ClientApiKeyId::new("key_early_401").expect("key id"),
        ),
        NonZeroU32::new(1).expect("attempt"),
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None).with_account_scope(Arc::new(
            FrozenAccountScope::new(directory, ClientRoutingScope::all_accounts()),
        )),
        None,
        CancellationToken::new(),
    )
}

fn jwt_with_exp(expires_at: DateTime<Utc>) -> String {
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({"exp": expires_at.timestamp()}))
            .expect("test JWT payload"),
    );
    format!("unverified-header.{payload}.unverified-signature")
}

#[tokio::test]
async fn early_revoked_oauth_heals_via_refresh_due_and_reenters_live_selection() {
    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_early_401_heal";
    let future_expiry = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
    seed_refreshable_account(
        &store,
        account_id,
        future_expiry,
        Some(SystemTime::now() + Duration::from_secs(30 * 60)),
    )
    .await;

    let selector = selector_for_store(&store);
    let request_url =
        Url::parse("https://chatgpt.com/backend-api/codex/responses").expect("request URL");
    let attempt = live_attempt_for(account_id);
    let lease = selector
        .select(&SelectCodexCredential {
            upstream_model: "gpt-5.4",
            request_url: &request_url,
            attempt: &attempt,
            session_affinity_key: None,
        })
        .await
        .expect("select healthy account");

    selector
        .record_failure(
            lease.account(),
            CodexAccountFailure::CredentialExpired,
            Some("Encountered invalidated oauth token for user, failing request".to_owned()),
        )
        .await
        .expect("record early 401");

    // 进入恢复期后必须退出实时路由，且清掉陈旧退避时间等待立即刷新。
    let recovering = store.account(account_id).expect("recovering account");
    assert_eq!(recovering.credential_state(), CredentialState::Ready);
    assert_eq!(
        recovering.last_error_reason(),
        Some(AccountErrorReason::AccessTokenExpired)
    );
    assert_eq!(recovering.next_refresh_at(), None);
    assert_eq!(
        recovering.status_projection(SystemTime::now(), None).status,
        AccountStatus::Error
    );
    let blocked_attempt = live_attempt_for(account_id);
    assert!(matches!(
        selector
            .select(&SelectCodexCredential {
                upstream_model: "gpt-5.4",
                request_url: &request_url,
                attempt: &blocked_attempt,
                session_affinity_key: None,
            })
            .await,
        Err(CredentialSelectionError::NoEligibleCredential)
    ));

    // 后台刷新周期使用有效 RT 换出新 AT 后自动自愈并重新回到实时流量池。
    let rotated_exp = Utc::now() + chrono::Duration::days(10);
    let refresher = SingleUseRefresher::with_access_token(jwt_with_exp(rotated_exp));
    let service = refresh_service(
        &store,
        Arc::clone(&refresher),
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );
    let outcomes = service.refresh_due().await.expect("refresh due");
    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Refreshed {
            account_id: refreshed_id,
            ..
        }] if refreshed_id == account_id
    ));
    assert_eq!(refresher.calls(), 1);

    let healed = store.account(account_id).expect("healed account");
    assert_eq!(healed.credential_state(), CredentialState::Ready);
    assert_eq!(healed.last_error_reason(), None);
    assert_eq!(healed.last_error_message(), None);
    assert_eq!(
        healed.status_projection(SystemTime::now(), None).status,
        AccountStatus::Normal
    );

    let resumed_attempt = live_attempt_for(account_id);
    let resumed = selector
        .select(&SelectCodexCredential {
            upstream_model: "gpt-5.4",
            request_url: &request_url,
            attempt: &resumed_attempt,
            session_affinity_key: None,
        })
        .await
        .expect("healed account must re-enter live traffic");
    assert_eq!(resumed.account().id().as_str(), account_id);
}

#[tokio::test]
async fn early_revoked_oauth_expires_immediately_on_invalid_grant() {
    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_early_401_invalid_grant";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60),
        None,
    )
    .await;
    let selector = selector_for_store(&store);
    let account = store.account(account_id).expect("seeded account");
    selector
        .record_failure(
            &account,
            CodexAccountFailure::CredentialExpired,
            Some("Encountered invalidated oauth token for user, failing request".to_owned()),
        )
        .await
        .expect("record early 401");

    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(FailingRefresher {
            failure: RefreshFailure::InvalidGrant {
                message: Some(
                    "Your refresh token has already been used to generate a new access token."
                        .to_owned(),
                ),
                upstream: None,
            },
        }),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );

    let first_outcomes = service.refresh_due().await.expect("first refresh cycle");
    assert!(matches!(
        first_outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Invalidated {
            account_id: invalidated_id,
        }] if invalidated_id == account_id
    ));
    let terminal = store.account(account_id).expect("terminal account");
    assert_eq!(terminal.credential_state(), CredentialState::Expired);
    assert_eq!(
        terminal.last_error_reason(),
        Some(AccountErrorReason::CredentialExpired)
    );

    // 终态后不再进入任何刷新重试循环。
    let second_outcomes = service.refresh_due().await.expect("second refresh cycle");
    assert!(second_outcomes.is_empty());
}

#[tokio::test]
async fn early_revoked_oauth_bounds_transient_backoff_and_expires_after_two_hours() {
    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_early_401_transient_window";
    let future_expiry = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
    seed_refreshable_account(&store, account_id, future_expiry, None).await;
    let selector = selector_for_store(&store);
    let account = store.account(account_id).expect("seeded account");
    selector
        .record_failure(
            &account,
            CodexAccountFailure::CredentialExpired,
            Some("Encountered invalidated oauth token for user, failing request".to_owned()),
        )
        .await
        .expect("record early 401");

    // 将首次观测时刻拨到 1 小时 59 分 50 秒前，验证退避上限锚定于 observed_at + 2h，
    // 而不是三天后的 access_token_expires_at + 2h。
    let initial_observed_at = SystemTime::now() - Duration::from_secs(2 * 60 * 60 - 10);
    let expected_deadline = initial_observed_at + Duration::from_secs(2 * 60 * 60);
    store.set_credential_observed_at(account_id, initial_observed_at);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": {
                "message": "Upstream temporarily unavailable",
                "type": "server_error",
                "code": "server_error"
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(OpenAiTokenClient::new(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test HTTP client"),
            TokenClientConfig {
                client_id: "test-public-client".to_owned(),
                token_endpoint: format!("{}/oauth/token", server.uri()),
            },
            provider_openai::OpenAiConfig::default().wire_profile_state(),
        )),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );

    let first_outcomes = service.refresh_due().await.expect("first transient cycle");
    assert!(matches!(
        first_outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Transient {
            account_id: deferred_id,
        }] if deferred_id == account_id
    ));
    let deferred = store.account(account_id).expect("deferred account");
    assert_eq!(deferred.credential_state(), CredentialState::Ready);
    assert_eq!(
        deferred.last_error_reason(),
        Some(AccountErrorReason::AccessTokenExpired)
    );
    assert_eq!(
        deferred.credential_observed_at(),
        Some(initial_observed_at),
        "transient backoff must not slide credential_observed_at forward"
    );
    let retry_at = deferred.next_refresh_at().expect("retry_at scheduled");
    assert!(
        retry_at <= expected_deadline,
        "retry_at must be clamped to observed_at + 2h rather than future JWT exp"
    );

    // 退避未到期且窗口未满时不得立即重打上游。
    store.set_next_refresh_at(
        account_id,
        Some(SystemTime::now() + Duration::from_secs(60)),
    );
    assert!(
        service
            .refresh_due()
            .await
            .expect("backoff cycle")
            .is_empty()
    );

    // 满 2 小时后即使 next_refresh_at 仍在未来也必须强制收敛并标记 Expired。
    store.set_credential_observed_at(
        account_id,
        SystemTime::now() - Duration::from_secs(2 * 60 * 60 + 60),
    );
    let final_outcomes = service
        .refresh_due()
        .await
        .expect("exhausted recovery window cycle");
    assert!(matches!(
        final_outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Invalidated {
            account_id: invalidated_id,
        }] if invalidated_id == account_id
    ));
    let terminal = store.account(account_id).expect("terminal account");
    assert_eq!(terminal.credential_state(), CredentialState::Expired);
    assert_eq!(
        terminal.last_error_reason(),
        Some(AccountErrorReason::CredentialExpired)
    );
}

#[tokio::test]
async fn proactive_prerefresh_transient_failure_keeps_healthy_access_token_normal() {
    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_proactive_transient";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now() + Duration::from_secs(2 * 60),
        None,
    )
    .await;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": {
                "message": "temporary token endpoint outage",
                "type": "server_error",
                "code": "server_error"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(OpenAiTokenClient::new(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test HTTP client"),
            TokenClientConfig {
                client_id: "test-public-client".to_owned(),
                token_endpoint: format!("{}/oauth/token", server.uri()),
            },
            provider_openai::OpenAiConfig::default().wire_profile_state(),
        )),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );

    let outcomes = service
        .refresh_due()
        .await
        .expect("proactive refresh cycle");
    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Transient {
            account_id: deferred_id,
        }] if deferred_id == account_id
    ));
    let account = store
        .account(account_id)
        .expect("account after transient pre-refresh");
    assert_eq!(account.credential_state(), CredentialState::Ready);
    assert_eq!(account.last_error_reason(), None);
    assert_eq!(
        account.status_projection(SystemTime::now(), None).status,
        AccountStatus::Normal
    );
    assert!(account.next_refresh_at().is_some());
}

#[tokio::test]
async fn early_revoked_refresh_omitting_access_token_persists_rotated_rt_and_keeps_isolation() {
    struct PartialRotationRefresher;

    #[async_trait]
    impl TokenRefresher for PartialRotationRefresher {
        async fn refresh(&self, _: &str) -> Result<TokenPair, RefreshFailure> {
            Ok(TokenPair {
                access_token: None,
                refresh_token: Some("rt-rotated-single-use".to_owned()),
                id_token: None,
            })
        }
    }

    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_early_401_partial_rotation";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60),
        None,
    )
    .await;
    let selector = selector_for_store(&store);
    let account = store.account(account_id).expect("seeded account");
    selector
        .record_failure(
            &account,
            CodexAccountFailure::CredentialExpired,
            Some("Encountered invalidated oauth token for user, failing request".to_owned()),
        )
        .await
        .expect("record early 401");
    let first_observed = store
        .account(account_id)
        .expect("recovering account")
        .credential_observed_at()
        .expect("observed_at");

    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(PartialRotationRefresher),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );

    let outcomes = service.refresh_due().await.expect("partial refresh cycle");
    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Transient {
            account_id: deferred_id,
        }] if deferred_id == account_id
    ));

    // 新轮换的一次性 RT 必须落盘保存，且旧 AT 的隔离标记和首次观测时间不得被清除或顺延。
    let after = store
        .account(account_id)
        .expect("account after partial refresh");
    assert_eq!(after.credential_state(), CredentialState::Ready);
    assert_eq!(
        after.last_error_reason(),
        Some(AccountErrorReason::AccessTokenExpired)
    );
    assert_eq!(after.credential_observed_at(), Some(first_observed));
    assert!(after.next_refresh_at().is_some());
    assert_eq!(
        after.status_projection(SystemTime::now(), None).status,
        AccountStatus::Error
    );
    let loaded = store
        .load_credential(after.id(), after.revision())
        .await
        .expect("load rotated credential");
    let runtime = CodexCredentialCodec::decode(&loaded.credential).expect("runtime credential");
    let oauth = runtime.authentication.oauth().expect("OAuth credential");
    assert_eq!(
        oauth
            .refresh_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("rt-rotated-single-use")
    );
}

#[tokio::test]
async fn concurrent_early_401_during_inflight_refresh_still_commits_rotated_tokens() {
    struct Interleaved401Refresher {
        store: Arc<MemoryAccountStore>,
        account_id: String,
    }

    #[async_trait]
    impl TokenRefresher for Interleaved401Refresher {
        async fn refresh(&self, _: &str) -> Result<TokenPair, RefreshFailure> {
            // 模拟刷新请求发出后、收到响应前，另一条流量触发 401 并落库 Ready + AccessTokenExpired。
            let selector = selector_for_store(&self.store);
            let current = self
                .store
                .account(&self.account_id)
                .expect("current account");
            selector
                .record_failure(
                    &current,
                    CodexAccountFailure::CredentialExpired,
                    Some(
                        "Encountered invalidated oauth token for user, failing request".to_owned(),
                    ),
                )
                .await
                .expect("record interleaved 401");
            Ok(TokenPair {
                access_token: Some(jwt_with_exp(Utc::now() + chrono::Duration::days(10))),
                refresh_token: Some("rt-fresh-after-interleaved-401".to_owned()),
                id_token: None,
            })
        }
    }

    let store = Arc::new(MemoryAccountStore::default());
    let account_id = "acct_interleaved_401_refresh";
    seed_refreshable_account(
        &store,
        account_id,
        SystemTime::now() + Duration::from_secs(2 * 60),
        None,
    )
    .await;

    let service = CodexCredentialRefreshService::new(
        store.repository(),
        Arc::new(Interleaved401Refresher {
            store: Arc::clone(&store),
            account_id: account_id.to_owned(),
        }),
        Arc::new(RefreshLeases),
        Arc::new(RefreshCredentialState),
        MutableRuntimePolicy::new(Duration::from_secs(5 * 60)),
    );

    let outcomes = service
        .refresh_due()
        .await
        .expect("interleaved refresh cycle");
    assert!(matches!(
        outcomes.as_slice(),
        [CodexCredentialRefreshOutcome::Refreshed {
            account_id: refreshed_id,
            ..
        }] if refreshed_id == account_id
    ));

    let healed = store.account(account_id).expect("healed account");
    assert_eq!(healed.credential_state(), CredentialState::Ready);
    assert_eq!(healed.last_error_reason(), None);
    assert_eq!(healed.last_error_message(), None);
    let loaded = store
        .load_credential(healed.id(), healed.revision())
        .await
        .expect("load committed credential");
    let runtime = CodexCredentialCodec::decode(&loaded.credential).expect("runtime credential");
    let oauth = runtime.authentication.oauth().expect("OAuth credential");
    assert_eq!(
        oauth
            .refresh_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("rt-fresh-after-interleaved-401")
    );
}
