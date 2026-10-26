//! 新 OAuth 账号经过真实 Provider 选号后不可退回旧 Codex 出站路径

use super::*;
use futures::FutureExt as _;
use gateway_core::account::{CredentialRevision, NewProviderAccount, ProviderAccount};
use provider_openai::credential::{
    CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING, CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    CodexCredentialCodec, CodexCredentialData, CodexOAuthCredentialData, OpenAiSelectionDomain,
    ResponsesTransport, open_ai_client_session_key,
};

pub(super) async fn seed_harness(store: &MemoryAccountStore, kind: &str) {
    let credential = CodexCredentialCodec::encode_complete(CodexCredentialData::OAuth(
        CodexOAuthCredentialData {
            transport: ResponsesTransport::Http,
            schema_version: 1,
            principal: None,
            installation_id: uuid::Uuid::new_v4().to_string(),
            access_token: "test-harness-bearer".to_owned(),
            refresh_token: Some("test-refresh".to_owned()),
            id_token: None,
            oauth_client_id: Some("test-client".to_owned()),
            oauth_scope: Some(
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
                    .to_owned(),
            ),
            authentication_kind: kind.to_owned(),
            cookies: Vec::new(),
        },
    ))
    .expect("credential");
    let account = ProviderAccount::new(
        ProviderAccountId::new("acct_provider_contract").expect("id"),
        ProviderKind::new("openai").expect("provider"),
        "test harness".to_owned(),
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
        .expect("account");
}

#[tokio::test]
async fn missing_harness_runtime_fails_closed_after_selecting_either_oauth_account() {
    for kind in [
        CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
        CODEX_AUTHENTICATION_KIND_PI_OPENAI,
    ] {
        for websocket in [false, true] {
            let store = Arc::new(MemoryAccountStore::default());
            seed_harness(&store, kind).await;
            let payload = ProtocolPayload::json_object(
                "openai",
                Map::from_iter([
                    ("model".to_owned(), json!("gpt-5.4")),
                    ("input".to_owned(), json!("hello")),
                    ("session_id".to_owned(), json!("session_runtime_missing")),
                ]),
            )
            .expect("request")
            .with_context(Map::from_iter([(
                "use_websocket".to_owned(),
                json!(websocket),
            )]));
            let operation = Operation::Generate(GenerateRequest::from_protocol_payload(payload));
            let affinity = Arc::new(MemorySessionAffinity::default());
            let error = provider_with_affinity(&store, Arc::clone(&affinity))
                .execute(
                    planned_request("openai", operation),
                    fallback_transport_context(&format!("req_harness_{kind}_{websocket}")),
                )
                .await
                .err()
                .expect("missing runtime must not silently call legacy Codex");
            assert_eq!(
                error.kind(),
                ProviderErrorKind::ProviderInfrastructureUnavailable
            );
            assert_eq!(error.send_state(), UpstreamSendState::NotSent);
            let client = ClientApiKeyId::new("key_openai_contract").expect("client");
            let provider_kind = ProviderKind::new("openai").expect("provider");
            let legacy = open_ai_client_session_key(
                &client,
                "session_runtime_missing",
                OpenAiSelectionDomain::ProviderDefault,
            )
            .expect("legacy key");
            let domain = if kind == CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING {
                OpenAiSelectionDomain::OpenCode
            } else {
                OpenAiSelectionDomain::Pi
            };
            let harness = open_ai_client_session_key(&client, "session_runtime_missing", domain)
                .expect("harness key");
            assert!(
                affinity
                    .load(&provider_kind, &legacy)
                    .await
                    .expect("legacy load")
                    .is_none()
            );
            assert_eq!(
                affinity
                    .load(&provider_kind, &harness)
                    .await
                    .expect("harness load")
                    .expect("selected account bound")
                    .as_str(),
                "acct_provider_contract"
            );
        }
    }
}

#[tokio::test]
async fn mixed_pool_rejects_credential_kind_switch_before_harness_send() {
    let store = Arc::new(MemoryAccountStore::default());
    seed_harness(&store, CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING).await;
    store.on_credential_load(Arc::new(|store, account, _| {
        if account.as_str() == "acct_provider_contract" {
            store.set_oauth_authentication_kind(
                account.as_str(),
                CODEX_AUTHENTICATION_KIND_PI_OPENAI,
            );
        }
        async { Ok(()) }.boxed()
    }));
    let error = provider(&store)
        .execute(
            planned_request("openai", generate_operation()),
            fallback_transport_context("req_harness_kind_switch"),
        )
        .await
        .err()
        .expect("credential cannot switch to the other harness after selection");
    assert_eq!(error.kind(), ProviderErrorKind::NoEligibleAccount);
    assert_eq!(error.send_state(), UpstreamSendState::NotSent);
}
