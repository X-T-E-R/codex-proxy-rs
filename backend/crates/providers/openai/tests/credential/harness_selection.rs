//! 同一客户端 API Key 与原会话下，Codex、OpenCode、pi 的选号和亲和隔离

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::future::FutureExt;
use gateway_core::account::{
    AccountWeight, CredentialCasOutcome, CredentialCasUpdate, CredentialRevision, CredentialState,
    NewProviderAccount, ProviderAccount, ProviderAccountId, ProviderAccountStore,
    ProviderAccountUpdate, QuotaState,
};
use gateway_core::engine::continuation::{
    ContinuationBinding, NativeContinuationPin, PreviousResponseId,
};
use gateway_core::engine::{
    AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::policy::ClientApiKeyId;
use gateway_core::provider_ports::{ProviderCooldownPort, ProviderLeasePort};
use gateway_core::routing::{
    ClientRoutingScope, FrozenAccountScope, ProviderKind, RuntimeAccount, RuntimeAccountDirectory,
};
use provider_openai::OFFICIAL_CODEX_BASE_URL;
use provider_openai::credential::{
    CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING, CODEX_AUTHENTICATION_KIND_OAUTH,
    CODEX_AUTHENTICATION_KIND_PI_OPENAI, CodexCookiePolicy, CodexCredentialCodec,
    CodexCredentialData, CodexCredentialQuotaService, CodexCredentialSelector,
    CodexOAuthCredentialData, CredentialSelectionError, ImportCodexOAuthCredential,
    OpenAiSelectionDomain, ResponsesTransport, SelectCodexCredential, open_ai_client_session_key,
};
use provider_openai::transport::profile::{CodexWireProfile, CodexWireProfileState};
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};
use url::Url;

use crate::support::{
    MemoryAccountStore, MemoryCooldownPort, MemorySessionAffinity, TestLeaseCoordinator,
    account_policy, profile, secret,
};

const CLIENT: &str = "key_same_client";
const SESSION: &str = "raw-client-session";

fn legacy_material(client: &str, session: &str, kind: Option<&str>) -> String {
    let mut session_hash = Sha256::new();
    session_hash.update(b"codex-session-affinity-v1\0root-session\0");
    session_hash.update(session.as_bytes());
    let session_key = hex::encode(session_hash.finalize());
    let mut material = format!("{client}\0{session_key}");
    if let Some(kind) = kind {
        material.push('\0');
        material.push_str(kind);
    }
    let mut key = Sha256::new();
    key.update(b"codex-session-affinity-v1\0client-session\0");
    key.update(material.as_bytes());
    hex::encode(key.finalize())
}

fn scope(ids: &[&str]) -> Arc<FrozenAccountScope> {
    let provider = ProviderKind::new("openai").expect("provider");
    let accounts = ids
        .iter()
        .map(|id| {
            (
                ProviderAccountId::new(*id).expect("account"),
                RuntimeAccount::new(provider.clone(), BTreeSet::new()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Arc::new(FrozenAccountScope::new(
        Arc::new(RuntimeAccountDirectory::new(accounts)),
        ClientRoutingScope::all_accounts(),
    ))
}

fn attempt(
    name: &str,
    account_scope: Arc<FrozenAccountScope>,
    continuation: Option<ContinuationBinding>,
) -> AttemptContext {
    AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new(name).expect("request"),
            ClientApiKeyId::new(CLIENT).expect("client"),
        ),
        NonZeroU32::new(1).expect("attempt"),
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None).with_account_scope(account_scope),
        continuation,
        CancellationToken::new(),
    )
}

fn selector(
    store: &Arc<MemoryAccountStore>,
    affinity: Arc<MemorySessionAffinity>,
) -> CodexCredentialSelector {
    let profile = CodexWireProfileState::new(CodexWireProfile {
        client_kind: provider_openai::transport::profile::selection::ClientKind::Desktop,
        originator: "codex_cli_rs".to_owned(),
        codex_version: "0.144.0".to_owned(),
        desktop_version: "1.0.0".to_owned(),
        desktop_build: "1".to_owned(),
        os_type: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        os_version: "6.8".to_owned(),
        terminal: "harness-selection".to_owned(),
        exact_user_agent: None,
        residency: None,
        verified_at: chrono::Utc::now(),
    });
    let quota = Arc::new(CodexCredentialQuotaService::new(
        store.repository(),
        profile,
        reqwest::Client::builder().build().expect("http"),
        OFFICIAL_CODEX_BASE_URL.to_owned(),
        Arc::new(MemoryCooldownPort::new()) as Arc<dyn ProviderCooldownPort>,
        Arc::new(TestLeaseCoordinator::default()) as Arc<dyn ProviderLeasePort>,
        crate::support::runtime_policy(),
    ));
    CodexCredentialSelector::new(
        ProviderKind::new("openai").expect("provider"),
        store.repository(),
        Arc::new(TestLeaseCoordinator::default()),
        affinity,
        quota,
        Arc::new(gateway_core::account::AccountFeedbackStats::default()),
        CodexCookiePolicy::official().expect("cookies"),
    )
}

async fn seed_harness(
    store: &MemoryAccountStore,
    id: &str,
    kind: &str,
    client_id: &str,
    access: &str,
) {
    let credential = CodexCredentialCodec::encode_complete(CodexCredentialData::OAuth(
        CodexOAuthCredentialData {
            transport: ResponsesTransport::Http,
            schema_version: 1,
            principal: None,
            installation_id: uuid::Uuid::new_v4().to_string(),
            access_token: access.to_owned(),
            refresh_token: Some(format!("rt-{access}")),
            id_token: None,
            oauth_client_id: Some(client_id.to_owned()),
            oauth_scope: Some(
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
                    .to_owned(),
            ),
            authentication_kind: kind.to_owned(),
            cookies: Vec::new(),
        },
    ))
    .expect("harness credential");
    let account = ProviderAccount::new(
        ProviderAccountId::new(id).expect("id"),
        ProviderKind::new("openai").expect("provider"),
        id.to_owned(),
        None,
        kind.to_owned(),
        CredentialRevision::new(1).expect("revision"),
        None,
    )
    .with_account_facts(
        true,
        CredentialState::Ready,
        QuotaState::unknown(),
        None,
        None,
    )
    .with_refresh_schedule(true, None);
    store
        .create_account(NewProviderAccount {
            account,
            credential,
            model_access: None,
        })
        .await
        .expect("seed harness");
}

async fn select_domain(
    selector: &CodexCredentialSelector,
    affinity_key: &gateway_core::provider_ports::ProviderSessionAffinityKey,
    domain: OpenAiSelectionDomain,
    model: &str,
    name: &str,
    account_scope: Arc<FrozenAccountScope>,
    continuation: Option<ContinuationBinding>,
) -> Result<provider_openai::credential::CodexCredentialLease, CredentialSelectionError> {
    let url = Url::parse(OFFICIAL_CODEX_BASE_URL).expect("url");
    let attempt = attempt(name, account_scope, continuation);
    selector
        .select_in_domain(
            &SelectCodexCredential {
                upstream_model: model,
                request_url: &url,
                attempt: &attempt,
                session_affinity_key: Some(affinity_key),
            },
            domain,
        )
        .await
}

#[tokio::test]
async fn same_client_session_keeps_three_harness_bindings_apart() {
    let client = ClientApiKeyId::new(CLIENT).expect("client");
    let codex_key =
        open_ai_client_session_key(&client, SESSION, OpenAiSelectionDomain::ProviderDefault)
            .expect("codex key");
    let opencode_key =
        open_ai_client_session_key(&client, SESSION, OpenAiSelectionDomain::OpenCode)
            .expect("opencode key");
    let pi_key =
        open_ai_client_session_key(&client, SESSION, OpenAiSelectionDomain::Pi).expect("pi key");
    assert_eq!(
        codex_key.expose_to_store(),
        legacy_material(CLIENT, SESSION, None)
    );
    assert_eq!(
        opencode_key.expose_to_store(),
        legacy_material(
            CLIENT,
            SESSION,
            Some(CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING)
        )
    );
    assert_eq!(
        pi_key.expose_to_store(),
        legacy_material(CLIENT, SESSION, Some(CODEX_AUTHENTICATION_KIND_PI_OPENAI))
    );
    assert_ne!(codex_key.expose_to_store(), opencode_key.expose_to_store());
    assert_ne!(opencode_key.expose_to_store(), pi_key.expose_to_store());

    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_oauth_credential(ImportCodexOAuthCredential {
            account_id: "acct_codex".to_owned(),
            name: "codex".to_owned(),
            secret: secret("access-codex"),
            verified_account: profile("shared-subject"),
            next_refresh_at: Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
            enabled: true,
        })
        .await;
    seed_harness(
        &store,
        "acct_opencode",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "client-opencode-a",
        "access-opencode",
    )
    .await;
    seed_harness(
        &store,
        "acct_pi",
        CODEX_AUTHENTICATION_KIND_PI_OPENAI,
        "client-pi-a",
        "access-pi",
    )
    .await;
    seed_harness(
        &store,
        "acct_opencode_next",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "client-opencode-b",
        "access-opencode-next",
    )
    .await;
    ProviderAccountStore::set_enabled(
        store.as_ref(),
        &ProviderAccountId::new("acct_opencode_next").expect("id"),
        false,
    )
    .await
    .expect("disable replacement");
    let account_scope = scope(&[
        "acct_codex",
        "acct_opencode",
        "acct_pi",
        "acct_opencode_next",
        "acct_api",
    ]);
    let affinity = Arc::new(MemorySessionAffinity::default());
    let provider = ProviderKind::new("openai").expect("provider");
    let selector = selector(&store, Arc::clone(&affinity));

    let codex = select_domain(
        &selector,
        &codex_key,
        OpenAiSelectionDomain::ProviderDefault,
        "gpt-5.4",
        "req_harness_codex",
        Arc::clone(&account_scope),
        None,
    )
    .await
    .expect("codex selection");
    let opencode = select_domain(
        &selector,
        &opencode_key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_opencode",
        Arc::clone(&account_scope),
        None,
    )
    .await
    .expect("opencode selection");
    let pi = select_domain(
        &selector,
        &pi_key,
        OpenAiSelectionDomain::Pi,
        "gpt-5.4",
        "req_harness_pi",
        Arc::clone(&account_scope),
        None,
    )
    .await
    .expect("pi selection");
    assert_eq!(codex.account_id().as_str(), "acct_codex");
    assert_eq!(
        codex.account().authentication_kind(),
        CODEX_AUTHENTICATION_KIND_OAUTH
    );
    assert_eq!(opencode.account_id().as_str(), "acct_opencode");
    assert_eq!(pi.account_id().as_str(), "acct_pi");
    assert_eq!(
        codex
            .authentication()
            .oauth()
            .expect("codex token")
            .access_token
            .expose_secret(),
        "access-codex"
    );
    assert_eq!(
        opencode
            .authentication()
            .oauth()
            .expect("opencode token")
            .access_token
            .expose_secret(),
        "access-opencode"
    );
    assert_ne!(
        CodexCredentialCodec::decode_complete(
            &ProviderAccountStore::load_current_credential(
                store.as_ref(),
                &ProviderAccountId::new("acct_opencode").expect("id"),
            )
            .await
            .expect("opencode credential")
            .credential
        )
        .expect("decode")
        .installation_id(),
        CodexCredentialCodec::decode_complete(
            &ProviderAccountStore::load_current_credential(
                store.as_ref(),
                &ProviderAccountId::new("acct_pi").expect("id"),
            )
            .await
            .expect("pi credential")
            .credential
        )
        .expect("decode")
        .installation_id()
    );
    drop((codex, opencode, pi));

    let loaded = ProviderAccountStore::load_current_credential(
        store.as_ref(),
        &ProviderAccountId::new("acct_opencode").expect("id"),
    )
    .await
    .expect("current opencode");
    let mut data = CodexCredentialCodec::decode_complete(&loaded.credential).expect("decode");
    let installation_id = data.installation_id().to_owned();
    data.oauth_mut().expect("oauth").access_token = "access-opencode-rotated".to_owned();
    let rotated = CodexCredentialCodec::encode_complete(data).expect("encode");
    assert!(matches!(
        ProviderAccountStore::compare_and_swap_credential(
            store.as_ref(),
            CredentialCasUpdate::new(
                loaded.account.id().clone(),
                loaded.account.revision(),
                ProviderAccountUpdate {
                    account_id: loaded.account.id().clone(),
                    name: loaded.account.name().to_owned(),
                    email: None,
                    plan_type: None,
                },
                rotated,
                true,
                loaded.account.access_token_expires_at(),
                loaded.account.next_refresh_at(),
            )
            .expect("cas")
            .preserving_profile(),
        )
        .await
        .expect("rotate opencode"),
        CredentialCasOutcome::Updated(_)
    ));
    let rebound = select_domain(
        &selector,
        &opencode_key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_reauth",
        Arc::clone(&account_scope),
        None,
    )
    .await
    .expect("reauth selection");
    assert_eq!(rebound.account_id().as_str(), "acct_opencode");
    assert_eq!(
        rebound
            .authentication()
            .oauth()
            .expect("rotated")
            .access_token
            .expose_secret(),
        "access-opencode-rotated"
    );
    assert_eq!(
        CodexCredentialCodec::decode_complete(
            &ProviderAccountStore::load_current_credential(
                store.as_ref(),
                &ProviderAccountId::new("acct_opencode").expect("id"),
            )
            .await
            .expect("rotated credential")
            .credential
        )
        .expect("decode")
        .installation_id(),
        installation_id
    );
    assert_eq!(
        affinity
            .load(&provider, &codex_key)
            .await
            .expect("codex binding")
            .expect("bound")
            .as_str(),
        "acct_codex"
    );
    assert_eq!(
        affinity
            .load(&provider, &pi_key)
            .await
            .expect("pi binding")
            .expect("bound")
            .as_str(),
        "acct_pi"
    );

    let crossed = select_domain(
        &selector,
        &opencode_key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_pin_codex",
        Arc::clone(&account_scope),
        Some(ContinuationBinding::Pinned(NativeContinuationPin::new(
            PreviousResponseId::new("client_response"),
            PreviousResponseId::new("upstream_response"),
            client.clone(),
            provider.clone(),
            ProviderAccountId::new("acct_codex").expect("codex"),
        ))),
    )
    .await;
    assert!(matches!(
        crossed,
        Err(CredentialSelectionError::ContinuationOwnerChanged)
    ));
    let followed = select_domain(
        &selector,
        &opencode_key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_pin_opencode",
        Arc::clone(&account_scope),
        Some(ContinuationBinding::Pinned(NativeContinuationPin::new(
            PreviousResponseId::new("client_response"),
            PreviousResponseId::new("upstream_response"),
            client,
            provider.clone(),
            ProviderAccountId::new("acct_opencode").expect("opencode"),
        ))),
    )
    .await
    .expect("continue opencode");
    assert_eq!(followed.account_id().as_str(), "acct_opencode");
    drop(followed);

    ProviderAccountStore::set_enabled(
        store.as_ref(),
        &ProviderAccountId::new("acct_opencode").expect("id"),
        false,
    )
    .await
    .expect("disable current opencode");
    ProviderAccountStore::set_enabled(
        store.as_ref(),
        &ProviderAccountId::new("acct_opencode_next").expect("id"),
        true,
    )
    .await
    .expect("enable replacement");
    let switched = select_domain(
        &selector,
        &opencode_key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_switch",
        Arc::clone(&account_scope),
        None,
    )
    .await
    .expect("switch opencode");
    assert_eq!(switched.account_id().as_str(), "acct_opencode_next");
    assert_eq!(
        switched
            .authentication()
            .oauth()
            .expect("next")
            .access_token
            .expose_secret(),
        "access-opencode-next"
    );
    assert_eq!(
        affinity
            .load(&provider, &codex_key)
            .await
            .expect("codex binding")
            .expect("bound")
            .as_str(),
        "acct_codex"
    );
    assert_eq!(
        affinity
            .load(&provider, &pi_key)
            .await
            .expect("pi binding")
            .expect("bound")
            .as_str(),
        "acct_pi"
    );
    assert_eq!(
        affinity
            .load(&provider, &opencode_key)
            .await
            .expect("opencode binding")
            .expect("bound")
            .as_str(),
        "acct_opencode_next"
    );

    store
        .seed_api_key(
            "acct_api",
            "https://example.com/v1".to_owned(),
            ResponsesTransport::Http,
        )
        .await;
    let grok = select_domain(
        &selector,
        &codex_key,
        OpenAiSelectionDomain::ProviderDefault,
        "grok-3",
        "req_harness_grok",
        Arc::clone(&account_scope),
        None,
    )
    .await
    .expect("grok uses api key");
    assert_eq!(grok.account_id().as_str(), "acct_api");
    let harness_grok = select_domain(
        &selector,
        &opencode_key,
        OpenAiSelectionDomain::OpenCode,
        "grok-3",
        "req_harness_opencode_grok",
        account_scope,
        None,
    )
    .await;
    assert!(matches!(
        harness_grok,
        Err(CredentialSelectionError::NoEligibleCredential)
    ));
}

#[tokio::test]
async fn reloaded_harness_credential_kind_is_not_admitted() {
    let store = Arc::new(MemoryAccountStore::default());
    seed_harness(
        &store,
        "acct_opencode",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "client-opencode-a",
        "access-opencode",
    )
    .await;
    seed_harness(
        &store,
        "acct_opencode_ok",
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        "client-opencode-ok",
        "access-opencode-ok",
    )
    .await;
    store.set_scheduling(
        "acct_opencode",
        None,
        AccountWeight::new(100).expect("weight"),
    );
    store.on_credential_load(Arc::new(|store, account, _count| {
        if account.as_str() == "acct_opencode" {
            store.set_oauth_authentication_kind(account.as_str(), CODEX_AUTHENTICATION_KIND_OAUTH);
        }
        async { Ok(()) }.boxed()
    }));
    let client = ClientApiKeyId::new(CLIENT).expect("client");
    let key =
        open_ai_client_session_key(&client, SESSION, OpenAiSelectionDomain::OpenCode).expect("key");
    let affinity = Arc::new(MemorySessionAffinity::default());
    let provider = ProviderKind::new("openai").expect("provider");
    let selector = selector(&store, Arc::clone(&affinity));
    let rejected = select_domain(
        &selector,
        &key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_reload",
        scope(&["acct_opencode"]),
        None,
    )
    .await;
    assert!(matches!(
        rejected,
        Err(CredentialSelectionError::NoEligibleCredential)
    ));
    assert!(
        affinity
            .load(&provider, &key)
            .await
            .expect("binding load")
            .is_none()
    );
    let selected = select_domain(
        &selector,
        &key,
        OpenAiSelectionDomain::OpenCode,
        "gpt-5.4",
        "req_harness_reload_peer",
        scope(&["acct_opencode", "acct_opencode_ok"]),
        None,
    )
    .await
    .expect("peer remains eligible");
    assert_eq!(selected.account_id().as_str(), "acct_opencode_ok");
    assert_eq!(
        selected
            .authentication()
            .oauth()
            .expect("peer token")
            .access_token
            .expose_secret(),
        "access-opencode-ok"
    );
    assert_eq!(
        affinity
            .load(&provider, &key)
            .await
            .expect("binding load")
            .expect("bound")
            .as_str(),
        "acct_opencode_ok"
    );
}
