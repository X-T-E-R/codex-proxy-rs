//! 验证 OpenCode 与 pi 授权、保存、刷新和重新授权不串用 Codex 凭据

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use gateway_admin::model::provider_credentials::{
    AuthorizationMutationTarget, AuthorizationOwnerBinding, PendingAuthorizationMutation,
    ProviderAuthorizationMethod,
};
use gateway_admin::model::{MutationActor, MutationContext};
use gateway_core::account::{OutboundProxy, ProviderAccountId, ProviderAccountStore};
use gateway_core::provider_ports::{
    ProviderLeaseAcquisition, ProviderLeasePort, ProviderLeaseRequest, ProviderSchedulingState,
    ProviderStoreError,
};
use gateway_core::routing::ProviderKind;
use provider_openai::credential::token_client::{
    AuthorizationCodeExchangeError, AuthorizationCodeExchanger, AuthorizationCodeGrant,
    AuthorizationTokenSet, OFFICIAL_CODEX_OAUTH_CLIENT_ID, RefreshFailure, TokenPair, TokenRefresher,
};
use provider_openai::credential::{
    CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING, CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    CodexCredentialAdmin, CodexCredentialAdminService,
    CodexCredentialCodec, CodexOAuthAdmin, CodexOAuthAdminError,
    CodexOAuthAdminService, CodexOAuthPendingClaimOutcome, CodexOAuthPendingStore,
    CodexOAuthPendingStoreError, CompleteCodexOAuthAuthorization, CompletedCodexOAuthCredential,
    ImportCodexOAuthCredential, StartCodexOAuthAuthorization, StoredCodexPendingAuthorization,
};
use provider_openai::transport::profile::CodexWireProfileState;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use url::Url;

use crate::support::{MemoryAccountStore, profile as account_profile, runtime_policy, secret};

const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";

#[derive(Default)]
struct PendingStore {
    pending: Mutex<Option<StoredCodexPendingAuthorization>>,
}

#[async_trait]
impl CodexOAuthPendingStore for PendingStore {
    async fn create(
        &self,
        pending: &provider_openai::credential::CodexPendingAuthorization,
    ) -> Result<(), CodexOAuthPendingStoreError> {
        let mut stored = self.pending.lock().expect("pending lock");
        if stored.is_some() {
            return Err(CodexOAuthPendingStoreError::Conflict);
        }
        *stored = Some(StoredCodexPendingAuthorization {
            flow_id: pending.flow_id().to_owned(),
            owner_ref: pending.owner_ref().to_owned(),
            started_request_ref: pending.started_request_ref().to_owned(),
            name: pending.name().to_owned(),
            expires_at: pending.expires_at(),
            state: pending.state().clone(),
            nonce: pending.nonce().clone(),
            code_verifier: pending.code_verifier().clone(),
            installation_id: pending.installation_id().to_owned(),
            reauthorization_account_id: pending.reauthorization().map(ToString::to_string),
            redirect_uri: pending.redirect_uri().map(str::to_owned),
            host_id: pending.host_id().map(str::to_owned),
            saved_client_id: pending.saved_client_id().map(str::to_owned),
            mutation: pending.mutation().clone(),
        });
        Ok(())
    }

    async fn claim(
        &self,
        _owner_ref: &str,
        _flow_id: &str,
        _claim_ref: &str,
        _claim_ttl: Duration,
    ) -> Result<CodexOAuthPendingClaimOutcome, CodexOAuthPendingStoreError> {
        let Some(pending) = self.pending.lock().expect("pending lock").take() else {
            return Ok(CodexOAuthPendingClaimOutcome::NotFound);
        };
        let pending = provider_openai::credential::CodexPendingAuthorization::from_stored(pending)
            .map_err(|_| CodexOAuthPendingStoreError::InvalidValue)?;
        Ok(CodexOAuthPendingClaimOutcome::Claimed(Box::new(pending)))
    }

    async fn release_claim(
        &self,
        _owner_ref: &str,
        _flow_id: &str,
        _claim_ref: &str,
    ) -> Result<bool, CodexOAuthPendingStoreError> {
        Ok(true)
    }

    async fn consume_claim(
        &self,
        _owner_ref: &str,
        _flow_id: &str,
        _claim_ref: &str,
    ) -> Result<bool, CodexOAuthPendingStoreError> {
        Ok(true)
    }
}

struct IdleExchanger;

#[async_trait]
impl AuthorizationCodeExchanger for IdleExchanger {
    async fn exchange_authorization_code(
        &self,
        _grant: AuthorizationCodeGrant,
    ) -> Result<AuthorizationTokenSet, AuthorizationCodeExchangeError> {
        panic!("harness authorization must not use the Codex token client");
    }
}

struct PanicRefresher;

#[async_trait]
impl TokenRefresher for PanicRefresher {
    async fn refresh(&self, _: &str) -> Result<TokenPair, RefreshFailure> {
        panic!("harness refresh must not use the Codex token client");
    }

    async fn refresh_with_proxy(
        &self,
        _: &str,
        _: Option<&OutboundProxy>,
    ) -> Result<TokenPair, RefreshFailure> {
        panic!("harness refresh must not use the Codex token client");
    }
}

struct RefreshLeases;

impl ProviderLeasePort for RefreshLeases {
    fn load_state<'a>(
        &'a self,
        _: &'a gateway_core::policy::ClientApiKeyId,
        _: &'a ProviderKind,
        _: &'a [ProviderAccountId],
        _: gateway_core::provider_ports::ProviderConcurrencyPool,
    ) -> BoxFuture<'a, Result<ProviderSchedulingState, ProviderStoreError>> {
        Box::pin(async { panic!("harness refresh does not load scheduling state") })
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
                    panic!("harness refresh must not acquire a scheduling lease")
                }
            }
        })
    }
}

struct MockUpstream {
    child: tokio::process::Child,
    origin: String,
    proxy: String,
}

impl MockUpstream {
    async fn start() -> Self {
        let mut child = tokio::process::Command::new("node")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/harness-worker/oauth-mock-upstream.mjs"
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start oauth mock");
        let stdout = child.stdout.take().expect("mock stdout");
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let line = lines
            .next_line()
            .await
            .expect("mock address")
            .expect("mock address line");
        let address: Value = serde_json::from_str(&line).expect("mock address json");
        let origin_port = address["origin"].as_u64().expect("origin port");
        let proxy_port = address["proxy"].as_u64().expect("proxy port");
        Self {
            child,
            origin: format!("http://127.0.0.1:{origin_port}"),
            proxy: format!("http://127.0.0.1:{proxy_port}"),
        }
    }

    async fn expect(&self, client_id: &str, code: &str, redirect: &str, nonce: &str, refresh: &str, access: &str) {
        reqwest::Client::new()
            .post(format!("{}/expect", self.origin))
            .json(&json!({
                "clientId": client_id,
                "code": code,
                "redirectUri": redirect,
                "nonce": nonce,
                "refreshToken": refresh,
                "accessToken": access,
            }))
            .send()
            .await
            .expect("register mock client")
            .error_for_status()
            .expect("mock accepted the client");
    }

    async fn recorded(&self) -> Vec<Value> {
        reqwest::Client::new()
            .get(format!("{}/recorded", self.origin))
            .send()
            .await
            .expect("read mock log")
            .json()
            .await
            .expect("mock log json")
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn wire_profile() -> CodexWireProfileState {
    CodexWireProfileState::new(Default::default())
}

fn create_mutation(method: Option<&str>, proxy: Option<&str>) -> PendingAuthorizationMutation {
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "harness-oauth-test".to_owned(),
    };
    let mutation = PendingAuthorizationMutation::new(
        ProviderKind::new("openai").expect("provider"),
        AuthorizationMutationTarget::Create {
            name: "harness account".to_owned(),
        },
        AuthorizationOwnerBinding::from_context(&context),
    );
    let mutation = match method {
        Some(method) => mutation.with_authorization_method(Some(
            ProviderAuthorizationMethod::new(method.to_owned()).expect("method"),
        )),
        None => mutation,
    };
    match proxy {
        Some(proxy) => mutation.with_outbound_proxy(Some(
            OutboundProxy::parse(proxy).expect("proxy"),
        )),
        None => mutation,
    }
}

fn reauthorize_mutation(account_id: ProviderAccountId) -> PendingAuthorizationMutation {
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "harness-reauth-test".to_owned(),
    };
    PendingAuthorizationMutation::new(
        ProviderKind::new("openai").expect("provider"),
        AuthorizationMutationTarget::Reauthorize { account_id },
        AuthorizationOwnerBinding::from_context(&context),
    )
}

fn service(
    store: Arc<MemoryAccountStore>,
    pending: Arc<PendingStore>,
    host_file: &std::path::Path,
    origin: &str,
) -> CodexOAuthAdminService {
    CodexOAuthAdminService::new(
        pending,
        Arc::new(IdleExchanger),
        store,
        CodexCredentialAdmin,
        wire_profile(),
    )
    .with_harness_host_file(host_file)
    .with_harness_token_origin(origin.to_owned())
}

fn query_value(url: &Url, name: &str) -> String {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("missing {name}"))
}

struct SavedHarness {
    installation_id: String,
    client_id: String,
    access_token: String,
}

async fn load_saved(store: &MemoryAccountStore, id: &str) -> SavedHarness {
    let account_id = ProviderAccountId::new(id.to_owned()).expect("account id");
    let loaded = store
        .load_current_credential(&account_id)
        .await
        .expect("load account");
    let data = CodexCredentialCodec::decode_complete(&loaded.credential).expect("credential");
    let oauth = data.oauth().expect("oauth credential");
    SavedHarness {
        installation_id: oauth.installation_id.clone(),
        client_id: oauth.oauth_client_id.clone().expect("client id"),
        access_token: oauth.access_token.clone(),
    }
}

#[tokio::test]
async fn harness_accounts_authorize_refresh_and_reauthorize_without_crossing_codex() {
    let mock = MockUpstream::start().await;
    let host_file = std::env::temp_dir().join(format!(
        "cpr-openai-harness-host-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_codex_shared".to_owned(),
            name: "codex".to_owned(),
            secret: secret("codex-access"),
            verified_account: account_profile("shared-subject"),
            next_refresh_at: None,
            enabled: true,
        })
        .await;
    let pending = Arc::new(PendingStore::default());
    let oauth = service(Arc::clone(&store), Arc::clone(&pending), &host_file, &mock.origin);

    let codex_started = oauth
        .start_authorization(StartCodexOAuthAuthorization {
            mutation: create_mutation(None, None),
        })
        .await
        .expect("codex start");
    assert!(codex_started.authorization_url.contains("chatgpt.com/codex/desktop-auth"));
    assert!(codex_started.authorization_url.contains(OFFICIAL_CODEX_OAUTH_CLIENT_ID));
    // 丢弃这条 Codex pending，后面的 harness create 需要空的测试存储
    pending.pending.lock().expect("pending lock").take();

    let first = oauth
        .start_authorization(StartCodexOAuthAuthorization {
            mutation: create_mutation(Some("opencode"), Some(&mock.proxy)),
        })
        .await
        .expect("opencode start");
    let second_service = service(
        Arc::clone(&store),
        Arc::new(PendingStore::default()),
        &host_file,
        &mock.origin,
    );
    let second = second_service
        .start_authorization(StartCodexOAuthAuthorization {
            mutation: create_mutation(Some("pi"), Some(&mock.proxy)),
        })
        .await
        .expect("pi start");
    let first_url = Url::parse(&first.authorization_url).expect("opencode url");
    let second_url = Url::parse(&second.authorization_url).expect("pi url");
    assert_eq!(
        query_value(&first_url, "ext_agent_host_id"),
        query_value(&second_url, "ext_agent_host_id")
    );
    assert_eq!(query_value(&first_url, "agent_name_hint"), "OpenCode");
    assert_eq!(query_value(&second_url, "agent_name_hint"), "Pi");
    assert_eq!(
        query_value(&first_url, "redirect_uri"),
        "http://127.0.0.1:1456/auth/callback"
    );
    assert_eq!(
        query_value(&second_url, "redirect_uri"),
        "http://127.0.0.1:1455/auth/callback"
    );
    assert_eq!(query_value(&first_url, "client_id"), "dynamic_agent_client");
    assert_eq!(query_value(&second_url, "client_id"), "dynamic_agent_client");

    let opencode_client = "client-opencode-a";
    let pi_client = "client-pi-a";
    mock.expect(
        opencode_client,
        "code-opencode",
        &query_value(&first_url, "redirect_uri"),
        &query_value(&first_url, "nonce"),
        "refresh-opencode",
        "access-opencode",
    )
    .await;
    let opencode_completed = oauth
        .complete_authorization(CompleteCodexOAuthAuthorization {
            owner_ref: "test-owner".to_owned(),
            flow_id: first.flow_id,
            callback_url: SecretString::from(format!(
                "{}?code=code-opencode&state={}&client_id={opencode_client}",
                query_value(&first_url, "redirect_uri"),
                query_value(&first_url, "state")
            )),
        })
        .await
        .expect("complete opencode");
    let CompletedCodexOAuthCredential::Create(opencode_account) = opencode_completed.credential
    else {
        panic!("opencode create");
    };
    let opencode_id = opencode_account.account.id().as_str().to_owned();
    assert_eq!(
        opencode_account.account.authentication_kind(),
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING
    );
    assert!(opencode_account.account.upstream_user_id().is_none());
    store
        .create_account(*opencode_account)
        .await
        .expect("save opencode");

    mock.expect(
        pi_client,
        "code-pi",
        &query_value(&second_url, "redirect_uri"),
        &query_value(&second_url, "nonce"),
        "refresh-pi",
        "access-pi",
    )
    .await;
    let pi_completed = second_service
        .complete_authorization(CompleteCodexOAuthAuthorization {
            owner_ref: "test-owner".to_owned(),
            flow_id: second.flow_id,
            callback_url: SecretString::from(format!(
                "{}?code=code-pi&state={}&client_id={pi_client}",
                query_value(&second_url, "redirect_uri"),
                query_value(&second_url, "state")
            )),
        })
        .await
        .expect("complete pi");
    let CompletedCodexOAuthCredential::Create(pi_account) = pi_completed.credential else {
        panic!("pi create");
    };
    let pi_id = pi_account.account.id().as_str().to_owned();
    assert_eq!(
        pi_account.account.authentication_kind(),
        CODEX_AUTHENTICATION_KIND_PI_OPENAI
    );
    assert_ne!(opencode_id, pi_id);
    assert_ne!(opencode_id, "acct_codex_shared");
    store.create_account(*pi_account).await.expect("save pi");

    let opencode = load_saved(&store, &opencode_id).await;
    let pi = load_saved(&store, &pi_id).await;
    let codex = store.account("acct_codex_shared").expect("codex account");
    assert_eq!(codex.upstream_account_id(), Some("shared-subject"));
    assert_ne!(opencode.installation_id, pi.installation_id);
    assert_ne!(opencode.client_id, pi.client_id);
    assert_eq!(opencode.client_id, opencode_client);
    assert_eq!(pi.client_id, pi_client);

    let admin = CodexCredentialAdminService::new(
        Arc::new(PanicRefresher),
        Arc::new(RefreshLeases),
        runtime_policy(),
        Arc::new(crate::RecordingDiagnostics::default()),
    )
    .with_harness_token_origin(mock.origin.clone());
    refresh_account(&admin, &store, &opencode_id).await;
    refresh_account(&admin, &store, &pi_id).await;
    let recorded = mock.recorded().await;
    let refreshes: Vec<_> = recorded
        .iter()
        .filter(|call| call["grant"] == "refresh_token")
        .collect();
    assert!(refreshes.iter().any(|call| {
        call["clientId"] == opencode_client && call["refreshToken"] == "refresh-opencode" && call["proxy"] == "1"
    }));
    assert!(refreshes.iter().any(|call| {
        call["clientId"] == pi_client && call["refreshToken"] == "refresh-pi" && call["proxy"] == "1"
    }));
    assert!(recorded.iter().all(|call| {
        !call["body"]
            .as_str()
            .unwrap_or("")
            .contains(OFFICIAL_CODEX_OAUTH_CLIENT_ID)
    }));
    let opencode_after = load_saved(&store, &opencode_id).await;
    let pi_after = load_saved(&store, &pi_id).await;
    assert_ne!(opencode_after.access_token, pi_after.access_token);
    assert_eq!(opencode_after.client_id, opencode_client);
    assert_eq!(pi_after.client_id, pi_client);
    assert_eq!(opencode_after.installation_id, opencode.installation_id);
    assert_eq!(pi_after.installation_id, pi.installation_id);

    let reauth_pending = Arc::new(PendingStore::default());
    let reauth = service(Arc::clone(&store), Arc::clone(&reauth_pending), &host_file, &mock.origin);
    let started = reauth
        .start_authorization(StartCodexOAuthAuthorization {
            mutation: reauthorize_mutation(ProviderAccountId::new(opencode_id.clone()).expect("id")),
        })
        .await
        .expect("opencode reauth start");
    let reauth_url = Url::parse(&started.authorization_url).expect("reauth url");
    assert_eq!(query_value(&reauth_url, "client_id"), opencode_client);
    assert!(reauth_url.query_pairs().all(|(key, _)| key != "agent_name_hint"));
    assert!(
        reauth_pending
            .pending
            .lock()
            .expect("pending")
            .as_ref()
            .expect("stored")
            .mutation
            .authorization_method()
            .to_string_exists("opencode")
    );
    let mismatched = reauth
        .complete_authorization(CompleteCodexOAuthAuthorization {
            owner_ref: "test-owner".to_owned(),
            flow_id: started.flow_id.clone(),
            callback_url: SecretString::from(format!(
                "{}?code=code-mismatch&state={}&client_id=client-other",
                query_value(&reauth_url, "redirect_uri"),
                query_value(&reauth_url, "state")
            )),
        })
        .await;
    assert!(matches!(
        mismatched,
        Err(CodexOAuthAdminError::TokenRejected) | Err(CodexOAuthAdminError::CallbackRejected)
    ));
    assert_eq!(load_saved(&store, &opencode_id).await.access_token, opencode_after.access_token);

    let _ = std::fs::remove_file(host_file);
}

trait MethodText {
    fn to_string_exists(&self, method: &str) -> bool;
}

impl MethodText for gateway_admin::model::provider_credentials::AuthorizationMethodSelection<'_> {
    fn to_string_exists(&self, method: &str) -> bool {
        matches!(self, Self::Selected(value) if value.as_str() == method)
    }
}

async fn refresh_account(admin: &CodexCredentialAdminService, store: &Arc<MemoryAccountStore>, id: &str) {
    let account_id = ProviderAccountId::new(id.to_owned()).expect("id");
    let current = store
        .load_current_credential(&account_id)
        .await
        .expect("current credential");
    let prepared = admin.manual_refresh(current).await.expect("refresh harness account");
    let (_, update, _, guard) = prepared.into_parts();
    store
        .compare_and_swap_credential(update)
        .await
        .expect("commit refreshed credential");
    drop(guard);
}

#[tokio::test]
async fn harness_import_keeps_client_material_and_skips_codex_identity() {
    let service = CodexCredentialAdminService::new(
        Arc::new(PanicRefresher),
        Arc::new(RefreshLeases),
        runtime_policy(),
        Arc::new(crate::RecordingDiagnostics::default()),
    );
    let prepared = service
        .prepare_import_document_with_proxy(
            json!({
                "authentication_kind": CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
                "client_id": "client-imported",
                "oauth_scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
                "accessToken": "access-imported",
                "refreshToken": "refresh-imported",
                "userId": "user-from-document",
                "accountId": "account-from-document",
                "email": "person@example.com"
            }),
            None,
        )
        .await
        .expect("import harness credential");
    let account = &prepared.accounts()[0];
    assert_eq!(
        account.account.authentication_kind(),
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING
    );
    assert!(account.account.upstream_user_id().is_none());
    assert!(account.account.upstream_account_id().is_none());
    let data = CodexCredentialCodec::decode_complete(&account.credential).expect("decode");
    let oauth = data.oauth().expect("oauth");
    assert_eq!(oauth.oauth_client_id.as_deref(), Some("client-imported"));
    assert!(oauth.oauth_scope.as_deref().unwrap().contains(DIRECT_SCOPE));
    assert!(oauth.principal.is_none());
    assert_eq!(
        oauth.transport,
        provider_openai::credential::ResponsesTransport::Http
    );

    let rejected = service
        .prepare_import_document_with_proxy(
            json!({
                "authentication_kind": CODEX_AUTHENTICATION_KIND_PI_OPENAI,
                "client_id": OFFICIAL_CODEX_OAUTH_CLIENT_ID,
                "oauth_scope": "openid chatgpt.tokens.use.direct",
                "accessToken": "access",
                "refreshToken": "refresh"
            }),
            None,
        )
        .await;
    assert!(rejected.is_err());
}
