//! 固定上游源码的离线请求、SSE 解析、取消和进程回收
//! 有效 `service_tier`、本次 request ID 与会话身份也在这里对照两边真实请求
//! 超时隔离、失败后的同会话重试、事件窗口、发送核对和源码清单也在这里

use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gateway_core::{account::OutboundProxy, lifecycle::CancellationToken};
use provider_openai::{
    HarnessError, HarnessEvent, HarnessSession, HarnessWorkerConfig, OPENCODE_SOURCE_COMMIT,
    PI_SOURCE_COMMIT, ParsedResponses, ResponseItem, ResponsesCall, ResponsesHarness,
    ResponsesRequestIdentity, account_scoped_id,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::timeout,
};

const TOKEN: &str = "bridge-sentinel-token";
const TOKEN_PI: &str = "bridge-sentinel-token-b";
const SESSION: &str = "ses_harness_bridge";
const INSTALLATION: &str = "inst_harness_bridge";

fn scoped(original: &str) -> String {
    account_scoped_id(INSTALLATION, original)
}
const REQUEST: &str = "req_harness_bridge";
const MODEL: &str = "gpt-4.1-mini";
const PROMPT: &str = "Say hello.";
const INSTRUCTIONS: &str = "You are concise.";

#[tokio::test]
async fn pinned_sources_complete_opencode_and_pi_responses() {
    let (opencode, pi) = checkouts();
    assert_eq!(git_head(&opencode), OPENCODE_SOURCE_COMMIT);
    assert_eq!(git_head(&pi), PI_SOURCE_COMMIT);
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    assert!(node_at_least(&session.node_version(), 22, 19, 0));
    let sources = session.loaded_sources();
    assert!(sources.len() > 3, "loaded {}", sources.len());
    let manifest = source_manifest();
    assert_eq!(
        manifest["opencode_commit"].as_str(),
        Some(OPENCODE_SOURCE_COMMIT)
    );
    assert_eq!(manifest["pi_commit"].as_str(), Some(PI_SOURCE_COMMIT));
    let files = manifest["files"].as_array().expect("manifest files");
    assert_eq!(files.len(), sources.len());
    for required in [
        "opencode:packages/ai/src/protocols/openai-responses.ts",
        "opencode:packages/ai/src/llm.ts",
        "pi:packages/ai/src/api/openai-responses.ts",
        "pi:packages/ai/src/api/openai-responses-shared.ts",
    ] {
        assert!(
            sources.iter().any(|source| source.id == required),
            "{required}"
        );
    }
    let hashes = sha256_files(
        &sources
            .iter()
            .map(|source| source.path.clone())
            .collect::<Vec<_>>(),
    );
    assert_eq!(hashes.len(), sources.len());
    for (source, digest) in sources.iter().zip(hashes) {
        let (root, relative) = source.id.split_once(':').expect("source id");
        let pinned = files
            .iter()
            .find(|file| file["root"] == root && file["path"] == relative)
            .unwrap_or_else(|| panic!("manifest missing {}", source.id));
        assert_eq!(
            pinned["sha256"].as_str(),
            Some(digest.as_str()),
            "{}",
            source.id
        );
        assert_eq!(source.sha256, digest, "{}", source.id);
    }

    let opencode_result = call(&session, ResponsesHarness::OpenCode, TOKEN).await;
    assert_contract(&opencode_result, false);
    assert!(opencode_result.credit_waits >= 1);
    assert_eq!(
        json_body(&opencode_result.request.body)["max_output_tokens"].as_u64(),
        Some(20)
    );
    assert_eq!(
        json_body(&opencode_result.request.body)["temperature"].as_f64(),
        Some(0.0)
    );
    assert_eq!(
        tool_event(&opencode_result.events).id.as_deref(),
        Some("call_bridge")
    );
    assert_eq!(
        done_event(&opencode_result.events).finish.as_deref(),
        Some("tool-calls")
    );
    assert_eq!(
        done_event(&opencode_result.events).response_id.as_deref(),
        Some("resp_bridge")
    );
    let scoped_session = scoped(SESSION);
    assert!(header(&opencode_result.request.headers, "openai-beta").is_none());
    assert!(header(&opencode_result.request.headers, "x-stainless-lang").is_none());
    assert_eq!(
        header(&opencode_result.request.headers, "session-id"),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        header(&opencode_result.request.headers, "thread-id"),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        header(&opencode_result.request.headers, "x-client-request-id"),
        Some(scoped_session.as_str())
    );
    assert_clean(&session, TOKEN);

    let pi_result = call(&session, ResponsesHarness::Pi, TOKEN_PI).await;
    assert_contract(&pi_result, true);
    let pi_body = json_body(&pi_result.request.body);
    assert!(pi_body.get("max_output_tokens").is_none());
    assert!(pi_body.get("temperature").is_none());
    assert!(pi_body.get("prompt_cache_retention").is_none());
    assert_eq!(
        pi_body["prompt_cache_key"].as_str(),
        Some(scoped_session.as_str())
    );
    let user_agent = header(&pi_result.request.headers, "user-agent").unwrap_or("");
    assert!(user_agent.starts_with("pi/1.4.0 ("), "{user_agent}");
    assert!(user_agent.contains("node/"), "{user_agent}");
    assert!(header(&pi_result.request.headers, "x-stainless-lang").is_some());
    assert!(header(&pi_result.request.headers, "x-stainless-package-version").is_some());
    assert!(header(&pi_result.request.headers, "x-stainless-runtime").is_some());
    assert_eq!(
        header(&pi_result.request.headers, "session_id"),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        header(&pi_result.request.headers, "x-client-request-id"),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        tool_event(&pi_result.events).id.as_deref(),
        Some("call_bridge|fc_item")
    );
    assert_eq!(
        done_event(&pi_result.events).finish.as_deref(),
        Some("toolUse")
    );
    assert_eq!(
        done_event(&pi_result.events).response_id.as_deref(),
        Some("resp_bridge")
    );
    assert_clean(&session, TOKEN_PI);

    let pid = session.worker_pid();
    session.shutdown().await.expect("shutdown");
    assert!(!process_alive(pid));
    let origin_headers = origin.headers_for_last();
    assert_eq!(header(&origin_headers, "x-cpr-harness-managed"), Some("1"));
}

#[tokio::test]
async fn configured_proxy_reaches_origin() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let proxy = Proxy::serve(origin.addr).await;
    let proxy_url = OutboundProxy::parse(&format!("http://{}", proxy.addr)).expect("proxy url");
    let session = open_session(worker_config(&opencode, &pi, origin.addr, Some(proxy_url))).await;
    let parsed = call(&session, ResponsesHarness::OpenCode, TOKEN).await;
    assert_eq!(
        json_body(&parsed.request.body)["max_output_tokens"].as_u64(),
        Some(20)
    );
    assert_eq!(
        tool_event(&parsed.events).id.as_deref(),
        Some("call_bridge")
    );
    assert!(
        proxy.saw_absolute_http(),
        "proxy did not observe an absolute-form request"
    );
    assert_eq!(
        header(&origin.headers_for_last(), "x-cpr-harness-managed"),
        Some("1")
    );
    assert_clean(&session, TOKEN);
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn cancel_leaves_no_authorization_holder() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Stall).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let cancel = CancellationToken::new();
    let cancel_call = cancel.clone();
    let seen = Arc::clone(&origin.seen);
    let cancel_task = tokio::spawn(async move {
        timeout(Duration::from_secs(60), seen.notified())
            .await
            .expect("origin did not observe the request");
        cancel.cancel();
    });
    let error = drive_call(
        &session,
        sample_call(ResponsesHarness::Pi, TOKEN),
        &cancel_call,
    )
    .await
    .expect_err("stall should not complete");
    cancel_task.await.expect("cancel task");
    assert!(matches!(error, HarnessError::Cancelled), "{error}");
    assert_eq!(session.authorization_holders(), 0);
    assert!(!session.stderr_snapshot().contains(TOKEN));
    assert!(!error.to_string().contains(TOKEN));
    let headers = origin.headers_for_last();
    let authorization = header(&headers, "authorization").unwrap_or("");
    assert!(authorization.eq_ignore_ascii_case(&format!("Bearer {TOKEN}")));
    session.shutdown().await.expect("shutdown");
    timeout(Duration::from_secs(5), origin.disconnected.notified())
        .await
        .expect("origin connection stayed open");
}

#[tokio::test]
async fn worker_exit_releases_authorization_holder() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Stall).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let pid = session.worker_pid();
    let seen = Arc::clone(&origin.seen);
    let killer = tokio::spawn(async move {
        timeout(Duration::from_secs(60), seen.notified())
            .await
            .expect("origin did not observe the request");
        kill_pid(pid);
    });
    let error = drive_call(
        &session,
        sample_call(ResponsesHarness::OpenCode, TOKEN),
        &CancellationToken::new(),
    )
    .await
    .expect_err("killed worker should not complete");
    killer.await.expect("killer");
    assert_eq!(session.authorization_holders(), 0);
    assert!(!session.stderr_snapshot().contains(TOKEN));
    assert!(!error.to_string().contains(TOKEN));
    assert!(!process_alive(pid));
    session.shutdown().await.expect("shutdown");
    timeout(Duration::from_secs(5), origin.disconnected.notified())
        .await
        .expect("origin connection stayed open after worker exit");
}

#[tokio::test]
async fn missing_pi_checkout_does_not_complete() {
    let (opencode, _) = checkouts();
    let missing = tempfile::tempdir().expect("tempdir");
    let origin = Origin::serve(OriginMode::Sse).await;
    let mut config = worker_config(&opencode, missing.path(), origin.addr, None);
    config.pi_checkout = missing.path().to_path_buf();
    match HarnessSession::open(config).await {
        Ok(session) => {
            let stderr = session.stderr_snapshot();
            let _ = session.shutdown().await;
            panic!("missing pi checkout started a session: {stderr}");
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                matches!(error, HarnessError::Startup { .. } | HarnessError::Source),
                "{text}"
            );
            assert!(
                text.contains("ENOENT")
                    || text.contains("no such file")
                    || text.contains("openai-responses"),
                "{text}"
            );
            assert!(!text.contains("ERR_UNSUPPORTED_ESM_URL_SCHEME"), "{text}");
            assert!(!text.contains("\"ok\":true"));
            assert!(!text.contains(TOKEN));
        }
    }
    assert!(origin.recorded.lock().expect("records").is_empty());
}

#[tokio::test]
async fn disabled_remote_dial_does_not_leave_authorization() {
    let (opencode, pi) = checkouts();
    let mut config = worker_config(&opencode, &pi, SocketAddr::from(([127, 0, 0, 1], 9)), None);
    config.dial_loopback = None;
    config.allow_remote_dial = false;
    let session = open_session(config).await;
    let error = drive_call(
        &session,
        sample_call(ResponsesHarness::OpenCode, TOKEN),
        &CancellationToken::new(),
    )
    .await
    .expect_err("remote dial should fail");
    assert!(matches!(error, HarnessError::RemoteDial), "{error}");
    assert_eq!(session.authorization_holders(), 0);
    assert!(!session.stderr_snapshot().contains(TOKEN));
    assert!(!error.to_string().contains(TOKEN));
    session.shutdown().await.expect("shutdown");
}

fn assert_contract(parsed: &ParsedResponses, pi: bool) {
    let request = &parsed.request;
    let url = reqwest::Url::parse(&request.url).expect("request url");
    assert_eq!(url.host_str(), Some("api.openai.com"));
    assert!(url.path().ends_with("/responses"), "{}", url.path());
    assert_eq!(request.method.to_ascii_uppercase(), "POST");
    let authorization = header(&request.headers, "authorization").unwrap_or("");
    let token = if pi { TOKEN_PI } else { TOKEN };
    let names: Vec<&str> = request
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        authorization.eq_ignore_ascii_case(&format!("Bearer {token}")),
        "authorization={authorization:?} names={names:?}"
    );
    let content_type = header(&request.headers, "content-type").unwrap_or("");
    assert!(
        content_type
            .to_ascii_lowercase()
            .contains("application/json"),
        "{content_type}"
    );
    assert!(header(&request.headers, "x-cpr-harness-managed").is_none());
    let body = json_body(&request.body);
    assert_eq!(body["model"].as_str(), Some(MODEL));
    assert_eq!(body["store"].as_bool(), Some(false));
    assert_eq!(body["stream"].as_bool(), Some(true));
    assert!(body.get("service_tier").is_none());
    let rendered = String::from_utf8_lossy(&request.body);
    assert!(rendered.contains(PROMPT), "{rendered}");
    if !pi {
        assert_eq!(body["instructions"].as_str(), Some(INSTRUCTIONS));
        let include = body["include"].as_array().cloned().unwrap_or_default();
        assert!(
            include
                .iter()
                .any(|item| item.as_str() == Some("reasoning.encrypted_content"))
        );
    }
    let debugged = format!("{request:?}");
    assert!(!debugged.contains(token));
}

fn assert_clean(session: &HarnessSession, token: &str) {
    assert_eq!(session.authorization_holders(), 0);
    assert!(!session.stderr_snapshot().contains(token));
}

fn tool_event(events: &[HarnessEvent]) -> &HarnessEvent {
    events
        .iter()
        .find(|event| event.kind == "tool")
        .expect("tool event")
}

fn done_event(events: &[HarnessEvent]) -> &HarnessEvent {
    events
        .iter()
        .find(|event| event.kind == "done")
        .expect("done event")
}

fn json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("json body")
}

fn json_body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn assert_attempt_stays_off_the_wire(headers: &[(String, String)], request_id: &str) {
    for (name, value) in headers {
        assert_ne!(value, request_id, "{name}");
        assert_ne!(name.to_ascii_lowercase(), "x-request-id");
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

async fn call(session: &HarnessSession, harness: ResponsesHarness, token: &str) -> ParsedResponses {
    expect_call(
        session,
        drive_call(
            session,
            sample_call(harness, token),
            &CancellationToken::new(),
        )
        .await,
    )
}

fn sample_call(harness: ResponsesHarness, token: &str) -> ResponsesCall {
    ResponsesCall {
        harness,
        model: MODEL.to_owned(),
        base_url: "https://api.openai.com/v1".to_owned(),
        installation_id: INSTALLATION.to_owned(),
        session_id: SESSION.to_owned(),
        request_id: REQUEST.to_owned(),
        bearer: SecretString::from(token.to_owned()),
        responses_body: None,
        prompt: PROMPT.to_owned(),
        instructions: Some(INSTRUCTIONS.to_owned()),
        max_output_tokens: Some(20),
        temperature: Some(0.0),
        service_tier: None,
        identity: ResponsesRequestIdentity::default(),
        downstream_headers: Vec::new(),
        body_window: 4096,
        event_window: 8,
        timeout: Some(Duration::from_secs(90)),
    }
}

#[tokio::test]
async fn pi_branch_follows_account_token_not_downstream_authorization() {
    const API_KEY: &str = "sk-bridge-branch-key";
    const DOWNSTREAM_KEY: &str = "sk-downstream-client";
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let downstream = vec![
        (
            "Authorization".to_owned(),
            format!("Bearer {DOWNSTREAM_KEY}"),
        ),
        (
            "CF-AIG-Authorization".to_owned(),
            "downstream-aig".to_owned(),
        ),
        ("x-request-id".to_owned(), "req_keep_downstream".to_owned()),
    ];

    let mut oauth = sample_call(ResponsesHarness::Pi, TOKEN);
    oauth.service_tier = Some("priority".to_owned());
    oauth.downstream_headers = downstream.clone();
    let oauth_result = complete(&session, oauth).await;
    let oauth_body = json_body(&oauth_result.request.body);
    assert!(oauth_body.get("max_output_tokens").is_none());
    assert!(oauth_body.get("temperature").is_none());
    assert!(oauth_body.get("prompt_cache_retention").is_none());
    assert_eq!(oauth_body["service_tier"].as_str(), Some("priority"));
    assert_account_token(&oauth_result.request.headers, TOKEN);
    let scoped_session = scoped(SESSION);
    assert!(header(&oauth_result.request.headers, "x-request-id").is_none());
    assert_eq!(
        header(&oauth_result.request.headers, "session_id"),
        Some(scoped_session.as_str())
    );
    assert!(header(&oauth_result.request.headers, "x-stainless-lang").is_some());
    assert_clean(&session, TOKEN);

    let mut api_key = sample_call(ResponsesHarness::Pi, API_KEY);
    api_key.service_tier = Some("flex".to_owned());
    api_key.downstream_headers = downstream.clone();
    let api_key_result = complete(&session, api_key).await;
    let api_key_body = json_body(&api_key_result.request.body);
    assert_eq!(api_key_body["max_output_tokens"].as_u64(), Some(20));
    assert_eq!(api_key_body["temperature"].as_f64(), Some(0.0));
    assert_eq!(api_key_body["service_tier"].as_str(), Some("flex"));
    assert_account_token(&api_key_result.request.headers, API_KEY);
    assert!(header(&api_key_result.request.headers, "x-request-id").is_none());
    assert_clean(&session, API_KEY);

    let mut opencode_call = sample_call(ResponsesHarness::OpenCode, TOKEN);
    opencode_call.downstream_headers = downstream;
    let opencode_result = complete(&session, opencode_call).await;
    let opencode_body = json_body(&opencode_result.request.body);
    assert_eq!(opencode_body["max_output_tokens"].as_u64(), Some(20));
    assert_account_token(&opencode_result.request.headers, TOKEN);
    assert!(header(&opencode_result.request.headers, "x-request-id").is_none());
    assert_eq!(
        header(&opencode_result.request.headers, "session-id"),
        Some(scoped_session.as_str())
    );
    assert!(header(&opencode_result.request.headers, "openai-beta").is_none());
    assert_clean(&session, TOKEN);
    let stderr = session.stderr_snapshot();
    assert!(!stderr.contains("unused"));
    assert!(!stderr.contains(DOWNSTREAM_KEY));
    session.shutdown().await.expect("shutdown");
}

fn assert_account_token(headers: &[(String, String)], token: &str) {
    let authorization = header(headers, "authorization").unwrap_or("");
    assert_eq!(authorization, format!("Bearer {token}"));
    assert!(!authorization.contains("unused"));
    assert!(header(headers, "cf-aig-authorization").is_none());
}

fn assert_source_session(headers: &[(String, String)], pi: bool) {
    let session = scoped(SESSION);
    if pi {
        assert_eq!(header(headers, "session_id"), Some(session.as_str()));
        assert_eq!(
            header(headers, "x-client-request-id"),
            Some(session.as_str())
        );
        let user_agent = header(headers, "user-agent").unwrap_or("");
        assert!(user_agent.starts_with("pi/1.4.0 ("), "{user_agent}");
        assert!(header(headers, "x-stainless-lang").is_some());
        assert!(header(headers, "x-stainless-package-version").is_some());
        assert!(header(headers, "x-stainless-runtime").is_some());
        assert!(header(headers, "session-id").is_none());
        assert!(header(headers, "thread-id").is_none());
    } else {
        assert_eq!(header(headers, "session-id"), Some(session.as_str()));
        assert_eq!(header(headers, "thread-id"), Some(session.as_str()));
        assert_eq!(
            header(headers, "x-client-request-id"),
            Some(session.as_str())
        );
        assert!(header(headers, "session_id").is_none());
        assert!(header(headers, "openai-beta").is_none());
    }
    assert!(headers.iter().all(|(name, _)| {
        let lower = name.to_ascii_lowercase();
        !lower.starts_with("x-openai-") && !lower.starts_with("openai-")
    }));
}

fn assert_no_downstream_identity(headers: &[(String, String)]) {
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        assert!(
            !matches!(
                lower.as_str(),
                "cookie"
                    | "chatgpt-account-id"
                    | "chatgpt-project-id"
                    | "originator"
                    | "version"
                    | "openai-organization"
                    | "openai-project"
                    | "cf-aig-authorization"
                    | "x-api-key"
                    | "x-request-id"
            ),
            "{name}"
        );
        assert!(!lower.starts_with("x-codex-"), "{name}");
        assert!(!lower.starts_with("x-openai-"), "{name}");
        assert!(!lower.starts_with("x-oai-"), "{name}");
        assert!(!value.contains("downstream-"), "{name}={value}");
    }
}

#[tokio::test]
async fn downstream_identity_headers_do_not_override_either_source() {
    const DOWNSTREAM_KEY: &str = "sk-downstream-client";
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let downstream = vec![
        (
            "Authorization".to_owned(),
            format!("Bearer {DOWNSTREAM_KEY}"),
        ),
        ("Cookie".to_owned(), "acct=downstream-cookie".to_owned()),
        (
            "chatgpt-account-id".to_owned(),
            "acct_downstream".to_owned(),
        ),
        ("originator".to_owned(), "downstream-originator".to_owned()),
        ("version".to_owned(), "downstream-version".to_owned()),
        ("User-Agent".to_owned(), "downstream-ua/0".to_owned()),
        ("session_id".to_owned(), "ses_downstream".to_owned()),
        (
            "x-client-request-id".to_owned(),
            "req_downstream".to_owned(),
        ),
        ("session-id".to_owned(), "ses_downstream_oc".to_owned()),
        ("thread-id".to_owned(), "thr_downstream".to_owned()),
        (
            "x-codex-turn-state".to_owned(),
            "downstream-turn".to_owned(),
        ),
        (
            "x-openai-internal-codex-residency".to_owned(),
            "downstream-residency".to_owned(),
        ),
        (
            "openai-organization".to_owned(),
            "org_downstream".to_owned(),
        ),
        ("openai-project".to_owned(), "proj_downstream".to_owned()),
        ("x-api-key".to_owned(), "downstream-api-key".to_owned()),
        ("api-key".to_owned(), "downstream-second-auth".to_owned()),
    ];
    for harness in [ResponsesHarness::Pi, ResponsesHarness::OpenCode] {
        let pi_path = harness == ResponsesHarness::Pi;
        let mut call = sample_call(harness, if pi_path { TOKEN_PI } else { TOKEN });
        call.downstream_headers = downstream.clone();
        let parsed = complete(&session, call).await;
        let token = if pi_path { TOKEN_PI } else { TOKEN };
        assert_account_token(&parsed.request.headers, token);
        assert_source_session(&parsed.request.headers, pi_path);
        assert_no_downstream_identity(&parsed.request.headers);
        let wire = origin.headers_for_last();
        assert_account_token(&wire, token);
        assert_source_session(&wire, pi_path);
        assert_no_downstream_identity(&wire);
        assert_eq!(header(&wire, "x-cpr-harness-managed"), Some("1"));
        assert_clean(&session, token);
    }
    let stderr = session.stderr_snapshot();
    assert!(!stderr.contains(DOWNSTREAM_KEY));
    assert!(!stderr.contains("downstream-"));
    assert!(!stderr.contains("unused"));
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn service_tier_and_request_identity_follow_both_sources() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let identity = ResponsesRequestIdentity {
        session_affinity_id: Some("ses_parent_affinity".to_owned()),
    };
    for tier in ["priority", "flex"] {
        let opencode_request = format!("req_oc_{tier}");
        let opencode_result = call_with(
            &session,
            ResponsesHarness::OpenCode,
            TOKEN,
            Some(tier),
            &opencode_request,
            identity.clone(),
        )
        .await;
        let opencode_body = json_body(&opencode_result.request.body);
        assert_eq!(opencode_body["service_tier"].as_str(), Some(tier));
        assert_eq!(opencode_body["max_output_tokens"].as_u64(), Some(20));
        assert_eq!(opencode_body["temperature"].as_f64(), Some(0.0));
        let parent = scoped("ses_parent_affinity");
        let scoped_session = scoped(SESSION);
        assert_eq!(
            header(&opencode_result.request.headers, "session-id"),
            Some(parent.as_str())
        );
        assert_eq!(
            header(&opencode_result.request.headers, "thread-id"),
            Some(scoped_session.as_str())
        );
        assert_eq!(
            header(&opencode_result.request.headers, "x-client-request-id"),
            Some(scoped_session.as_str())
        );
        assert_attempt_stays_off_the_wire(&opencode_result.request.headers, &opencode_request);
        assert!(!json_body_text(&opencode_result.request.body).contains(&opencode_request));
        assert!(header(&opencode_result.request.headers, "openai-beta").is_none());
        assert_clean(&session, TOKEN);

        let pi_request = format!("req_pi_{tier}");
        let pi_result = call_with(
            &session,
            ResponsesHarness::Pi,
            TOKEN_PI,
            Some(tier),
            &pi_request,
            identity.clone(),
        )
        .await;
        let pi_body = json_body(&pi_result.request.body);
        assert_eq!(pi_body["service_tier"].as_str(), Some(tier));
        assert!(pi_body.get("max_output_tokens").is_none());
        assert!(pi_body.get("temperature").is_none());
        assert!(pi_body.get("prompt_cache_retention").is_none());
        let scoped_session = scoped(SESSION);
        assert_eq!(
            pi_body["prompt_cache_key"].as_str(),
            Some(scoped_session.as_str())
        );
        assert_eq!(
            header(&pi_result.request.headers, "session_id"),
            Some(scoped_session.as_str())
        );
        assert_eq!(
            header(&pi_result.request.headers, "x-client-request-id"),
            Some(scoped_session.as_str())
        );
        assert!(header(&pi_result.request.headers, "x-stainless-lang").is_some());
        assert!(header(&pi_result.request.headers, "thread-id").is_none());
        assert!(header(&pi_result.request.headers, "session-id").is_none());
        let user_agent = header(&pi_result.request.headers, "user-agent").unwrap_or("");
        assert!(user_agent.starts_with("pi/1.4.0 ("), "{user_agent}");
        assert_attempt_stays_off_the_wire(&pi_result.request.headers, &pi_request);
        assert!(!json_body_text(&pi_result.request.body).contains(&pi_request));
        assert_clean(&session, TOKEN_PI);
    }

    let blank = call_with(
        &session,
        ResponsesHarness::OpenCode,
        TOKEN,
        Some("  "),
        "req_oc_blank",
        ResponsesRequestIdentity::default(),
    )
    .await;
    assert!(json_body(&blank.request.body).get("service_tier").is_none());
    let scoped_session = scoped(SESSION);
    assert_eq!(
        header(&blank.request.headers, "session-id"),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        header(&blank.request.headers, "thread-id"),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        header(&blank.request.headers, "x-client-request-id"),
        Some(scoped_session.as_str())
    );
    assert_attempt_stays_off_the_wire(&blank.request.headers, "req_oc_blank");
    let blank_pi = call_with(
        &session,
        ResponsesHarness::Pi,
        TOKEN_PI,
        Some("  "),
        "req_pi_blank",
        identity,
    )
    .await;
    assert!(
        json_body(&blank_pi.request.body)
            .get("service_tier")
            .is_none()
    );
    assert_eq!(
        json_body(&blank_pi.request.body)["prompt_cache_key"].as_str(),
        Some(scoped_session.as_str())
    );
    assert_eq!(
        header(&blank_pi.request.headers, "x-client-request-id"),
        Some(scoped_session.as_str())
    );
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn pi_shared_cache_key_keeps_distinct_source_sessions_across_turns() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let cache = scoped("shared-cache");
    let mut session_headers = Vec::new();
    for raw_session in ["ses_alpha", "ses_beta"] {
        let actual = scoped(raw_session);
        for turn in 1..=2 {
            let mut call = sample_call(ResponsesHarness::Pi, TOKEN_PI);
            call.session_id = raw_session.to_owned();
            call.request_id = format!("req_{raw_session}_{turn}");
            call.prompt.clear();
            call.responses_body = Some(json!({
                "model": MODEL,
                "input": format!("{raw_session} turn {turn}"),
                "prompt_cache_key": "shared-cache",
            }));
            call.downstream_headers = vec![
                ("session_id".to_owned(), "untrusted-session".to_owned()),
                (
                    "x-client-request-id".to_owned(),
                    "untrusted-request".to_owned(),
                ),
            ];
            let result = complete(&session, call).await;
            assert_eq!(json_body(&result.request.body)["prompt_cache_key"], cache);
            for name in ["session_id", "x-client-request-id"] {
                assert_eq!(header(&result.request.headers, name), Some(actual.as_str()));
                assert_eq!(
                    result
                        .request
                        .headers
                        .iter()
                        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                        .count(),
                    1
                );
            }
            assert_account_token(&result.request.headers, TOKEN_PI);
            session_headers.push(
                header(&result.request.headers, "session_id")
                    .unwrap()
                    .to_owned(),
            );
        }
    }
    assert_eq!(session_headers[0], session_headers[1]);
    assert_eq!(session_headers[2], session_headers[3]);
    assert_ne!(session_headers[0], session_headers[2]);

    let mut implicit = sample_call(ResponsesHarness::Pi, TOKEN_PI);
    implicit.session_id = "ses_default".to_owned();
    implicit.request_id = "req_default_cache".to_owned();
    let result = complete(&session, implicit).await;
    let default_session = scoped("ses_default");
    assert_eq!(
        json_body(&result.request.body)["prompt_cache_key"],
        default_session
    );
    for name in ["session_id", "x-client-request-id"] {
        assert_eq!(
            header(&result.request.headers, name),
            Some(default_session.as_str())
        );
        assert_eq!(
            result
                .request
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .count(),
            1
        );
    }
    {
        let records = origin.recorded.lock().expect("requests");
        assert_eq!(records.len(), 5);
        for (index, record) in records.iter().enumerate() {
            let expected = if index < 2 {
                &session_headers[0]
            } else if index < 4 {
                &session_headers[2]
            } else {
                &default_session
            };
            for name in ["session_id", "x-client-request-id"] {
                assert_eq!(header(&record.headers, name), Some(expected.as_str()));
                assert_eq!(
                    record
                        .headers
                        .iter()
                        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                        .count(),
                    1
                );
            }
        }
    }
    session.shutdown().await.expect("shutdown");
}

async fn complete(session: &HarnessSession, call: ResponsesCall) -> ParsedResponses {
    expect_call(
        session,
        drive_call(session, call, &CancellationToken::new()).await,
    )
}

async fn call_with(
    session: &HarnessSession,
    harness: ResponsesHarness,
    token: &str,
    service_tier: Option<&str>,
    request_id: &str,
    identity: ResponsesRequestIdentity,
) -> ParsedResponses {
    let mut call = sample_call(harness, token);
    call.service_tier = service_tier.map(str::to_owned);
    call.request_id = request_id.to_owned();
    call.identity = identity;
    expect_call(
        session,
        drive_call(session, call, &CancellationToken::new()).await,
    )
}

fn expect_call(
    session: &HarnessSession,
    result: Result<ParsedResponses, HarnessError>,
) -> ParsedResponses {
    match result {
        Ok(parsed) => parsed,
        Err(error) => panic!("{error}; stderr={}", session.stderr_snapshot()),
    }
}

async fn drive_call(
    session: &HarnessSession,
    call: ResponsesCall,
    cancel: &CancellationToken,
) -> Result<ParsedResponses, HarnessError> {
    let mut stream = session.open_responses(call, cancel).await?;
    let mut request = None;
    let mut events = Vec::new();
    loop {
        match stream.next().await? {
            ResponseItem::Request(value) => request = Some(value),
            ResponseItem::Event(event) => events.push(event),
            ResponseItem::Done(done) => {
                return Ok(ParsedResponses {
                    request: request.ok_or(HarnessError::Protocol)?,
                    events,
                    credit_waits: done.credit_waits,
                    event_waits: done.event_waits,
                    abort_listeners: done.abort_listeners,
                });
            }
        }
    }
}

fn source_manifest() -> Value {
    serde_json::from_str(include_str!("../../harness-worker/source-manifest.json"))
        .expect("source manifest")
}

fn sha256_files(paths: &[PathBuf]) -> Vec<String> {
    let mut child = Command::new(node_binary())
        .arg("-e")
        .arg("const fs=require('node:fs');const crypto=require('node:crypto');const data=fs.readFileSync(0,'utf8');for (const line of data.split(/\\r?\\n/)) { if (!line) continue; process.stdout.write(crypto.createHash('sha256').update(fs.readFileSync(line)).digest('hex')+'\\n'); }")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hash sources");
    {
        let stdin = child.stdin.as_mut().expect("hash stdin");
        for path in paths {
            writeln!(stdin, "{}", path.display()).expect("hash path");
        }
    }
    let output = child.wait_with_output().expect("hash sources");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn installation_id_scopes_repeat_calls_parent_and_the_other_account() {
    const RAW_PARENT: &str = "ses_raw_parent";
    const INSTALL_B: &str = "inst_account_b";
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let mut missing = sample_call(ResponsesHarness::OpenCode, TOKEN);
    missing.request_id = "  ".to_owned();
    let rejected = session
        .open_responses(missing, &CancellationToken::new())
        .await;
    assert!(
        matches!(rejected, Err(HarnessError::Protocol)),
        "blank request id started a call"
    );
    let mut missing_installation = sample_call(ResponsesHarness::OpenCode, TOKEN);
    missing_installation.installation_id = " ".to_owned();
    let rejected = session
        .open_responses(missing_installation, &CancellationToken::new())
        .await;
    assert!(
        matches!(rejected, Err(HarnessError::Protocol)),
        "blank installation id started a call"
    );
    assert!(origin.recorded.lock().expect("records").is_empty());

    let parent_a = scoped(RAW_PARENT);
    let current_a = scoped(SESSION);
    let parent_b = account_scoped_id(INSTALL_B, RAW_PARENT);
    let current_b = account_scoped_id(INSTALL_B, SESSION);
    assert_ne!(parent_a, current_a);
    assert_ne!(current_a, current_b);
    assert_ne!(
        current_a,
        account_scoped_id(INSTALLATION, current_a.as_str())
    );
    let identity = ResponsesRequestIdentity {
        session_affinity_id: Some(RAW_PARENT.to_owned()),
    };

    let mut first = sample_call(ResponsesHarness::OpenCode, TOKEN);
    first.request_id = "req_account_a1".to_owned();
    first.prompt = "prompt-a1".to_owned();
    first.identity = identity.clone();
    let parsed_a1 = complete(&session, first).await;
    assert_account_token(&parsed_a1.request.headers, TOKEN);
    assert_eq!(
        header(&parsed_a1.request.headers, "session-id"),
        Some(parent_a.as_str())
    );
    assert_eq!(
        header(&parsed_a1.request.headers, "thread-id"),
        Some(current_a.as_str())
    );
    assert_eq!(
        header(&parsed_a1.request.headers, "x-client-request-id"),
        Some(current_a.as_str())
    );
    assert_ne!(
        header(&parsed_a1.request.headers, "session-id"),
        Some(SESSION)
    );
    assert_ne!(
        header(&parsed_a1.request.headers, "session-id"),
        Some(RAW_PARENT)
    );
    assert_upstream_ids(&parsed_a1.events, false);
    assert_attempt_stays_off_the_wire(&parsed_a1.request.headers, "req_account_a1");

    let mut repeated = sample_call(ResponsesHarness::OpenCode, TOKEN);
    repeated.request_id = "req_account_a2".to_owned();
    repeated.prompt = "prompt-a2".to_owned();
    repeated.identity = identity.clone();
    let parsed_a2 = complete(&session, repeated).await;
    assert_account_token(&parsed_a2.request.headers, TOKEN);
    assert_eq!(
        header(&parsed_a2.request.headers, "session-id"),
        header(&parsed_a1.request.headers, "session-id")
    );
    assert_eq!(
        header(&parsed_a2.request.headers, "thread-id"),
        header(&parsed_a1.request.headers, "thread-id")
    );
    assert_eq!(
        header(&parsed_a2.request.headers, "x-client-request-id"),
        header(&parsed_a1.request.headers, "x-client-request-id")
    );
    assert!(json_body_text(&parsed_a2.request.body).contains("prompt-a2"));
    assert!(!json_body_text(&parsed_a2.request.body).contains("prompt-a1"));
    assert_attempt_stays_off_the_wire(&parsed_a2.request.headers, "req_account_a1");
    assert_attempt_stays_off_the_wire(&parsed_a2.request.headers, "req_account_a2");

    let mut same_account_other_bearer = sample_call(ResponsesHarness::OpenCode, TOKEN_PI);
    same_account_other_bearer.request_id = "req_same_account_bearer".to_owned();
    same_account_other_bearer.prompt = "prompt-bearer".to_owned();
    same_account_other_bearer.identity = identity.clone();
    let parsed_bearer = complete(&session, same_account_other_bearer).await;
    assert_account_token(&parsed_bearer.request.headers, TOKEN_PI);
    assert_eq!(
        header(&parsed_bearer.request.headers, "session-id"),
        Some(parent_a.as_str())
    );
    assert_eq!(
        header(&parsed_bearer.request.headers, "thread-id"),
        Some(current_a.as_str())
    );
    assert_eq!(
        header(&parsed_bearer.request.headers, "x-client-request-id"),
        Some(current_a.as_str())
    );

    let mut other_account = sample_call(ResponsesHarness::OpenCode, TOKEN);
    other_account.installation_id = INSTALL_B.to_owned();
    other_account.request_id = "req_account_b".to_owned();
    other_account.prompt = "prompt-b".to_owned();
    other_account.identity = identity;
    let parsed_b = complete(&session, other_account).await;
    assert_account_token(&parsed_b.request.headers, TOKEN);
    assert_eq!(
        header(&parsed_b.request.headers, "session-id"),
        Some(parent_b.as_str())
    );
    assert_eq!(
        header(&parsed_b.request.headers, "thread-id"),
        Some(current_b.as_str())
    );
    assert_eq!(
        header(&parsed_b.request.headers, "x-client-request-id"),
        Some(current_b.as_str())
    );
    assert_ne!(
        header(&parsed_b.request.headers, "thread-id"),
        Some(current_a.as_str())
    );
    assert!(json_body_text(&parsed_b.request.body).contains("prompt-b"));
    assert!(!json_body_text(&parsed_b.request.body).contains("prompt-a2"));
    assert_attempt_stays_off_the_wire(&parsed_b.request.headers, "req_account_a1");
    assert_attempt_stays_off_the_wire(&parsed_b.request.headers, "req_account_b");

    let mut pi_call = sample_call(ResponsesHarness::Pi, TOKEN);
    pi_call.request_id = "req_account_pi".to_owned();
    pi_call.prompt = "prompt-pi".to_owned();
    let parsed_pi = complete(&session, pi_call).await;
    assert_account_token(&parsed_pi.request.headers, TOKEN);
    assert_eq!(
        header(&parsed_pi.request.headers, "session_id"),
        Some(current_a.as_str())
    );
    assert_eq!(
        header(&parsed_pi.request.headers, "x-client-request-id"),
        Some(current_a.as_str())
    );
    assert_eq!(
        json_body(&parsed_pi.request.body)["prompt_cache_key"].as_str(),
        Some(current_a.as_str())
    );
    assert!(header(&parsed_pi.request.headers, "session-id").is_none());
    assert!(header(&parsed_pi.request.headers, "thread-id").is_none());
    assert_upstream_ids(&parsed_pi.events, true);
    assert!(json_body_text(&parsed_pi.request.body).contains("prompt-pi"));
    assert!(!json_body_text(&parsed_pi.request.body).contains("prompt-b"));
    assert_attempt_stays_off_the_wire(&parsed_pi.request.headers, "req_account_a1");
    assert_attempt_stays_off_the_wire(&parsed_pi.request.headers, "req_account_pi");

    let records = origin.recorded.lock().expect("records").clone();
    assert_eq!(records.len(), 5, "records={}", records.len());
    let bearer_a = format!("Bearer {TOKEN}");
    let bearer_other = format!("Bearer {TOKEN_PI}");
    assert_eq!(
        header(&records[0].headers, "authorization"),
        Some(bearer_a.as_str())
    );
    assert_eq!(
        header(&records[1].headers, "authorization"),
        Some(bearer_a.as_str())
    );
    assert_eq!(
        header(&records[2].headers, "authorization"),
        Some(bearer_other.as_str())
    );
    assert_eq!(
        header(&records[3].headers, "authorization"),
        Some(bearer_a.as_str())
    );
    assert_eq!(
        header(&records[4].headers, "authorization"),
        Some(bearer_a.as_str())
    );
    assert_eq!(
        header(&records[0].headers, "thread-id"),
        header(&records[1].headers, "thread-id")
    );
    assert_eq!(
        header(&records[0].headers, "thread-id"),
        Some(current_a.as_str())
    );
    assert_eq!(
        header(&records[2].headers, "thread-id"),
        Some(current_a.as_str())
    );
    assert_eq!(
        header(&records[3].headers, "thread-id"),
        Some(current_b.as_str())
    );
    assert_eq!(
        header(&records[4].headers, "session_id"),
        Some(current_a.as_str())
    );
    for record in &records {
        for request_id in [
            "req_account_a1",
            "req_account_a2",
            "req_same_account_bearer",
            "req_account_b",
            "req_account_pi",
        ] {
            assert_attempt_stays_off_the_wire(&record.headers, request_id);
        }
        assert!(!json_body_text(&record.body).contains("req_account_"));
        assert!(!json_body_text(&record.body).contains("req_bridge"));
        assert!(!json_body_text(&record.body).contains(RAW_PARENT));
        assert!(!json_body_text(&record.body).contains(SESSION));
    }
    assert_clean(&session, TOKEN);
    assert_clean(&session, TOKEN_PI);
    session.shutdown().await.expect("shutdown");
}

fn assert_upstream_ids(events: &[HarnessEvent], pi: bool) {
    let tool = tool_event(events);
    assert_eq!(
        tool.id.as_deref(),
        Some(if pi {
            "call_bridge|fc_item"
        } else {
            "call_bridge"
        })
    );
    assert_eq!(
        done_event(events).response_id.as_deref(),
        Some("resp_bridge")
    );
}

fn assert_scoped_pi(parsed: &ParsedResponses, scoped: &str, token: &str) {
    assert_account_token(&parsed.request.headers, token);
    assert_eq!(header(&parsed.request.headers, "session_id"), Some(scoped));
    assert_eq!(
        header(&parsed.request.headers, "x-client-request-id"),
        Some(scoped)
    );
    assert_eq!(
        json_body(&parsed.request.body)["prompt_cache_key"].as_str(),
        Some(scoped)
    );
    assert!(header(&parsed.request.headers, "session-id").is_none());
    assert!(header(&parsed.request.headers, "thread-id").is_none());
    assert_raw_scope_absent(&parsed.request.headers, &parsed.request.body);
}

fn assert_raw_scope_absent(headers: &[(String, String)], body: &[u8]) {
    let text = json_body_text(body);
    for raw in [
        "ses_downstream_original",
        "ses_downstream_parent",
        "req_downstream",
        "thr_downstream",
    ] {
        assert!(!text.contains(raw), "{raw}");
        assert!(
            headers.iter().all(|(_, value)| !value.contains(raw)),
            "{raw}"
        );
    }
}

#[tokio::test]
async fn account_scoped_session_stays_stable_until_the_account_changes() {
    const RAW_SESSION: &str = "ses_downstream_original";
    const RAW_PARENT: &str = "ses_downstream_parent";
    let scoped_a = account_scoped_id("inst_account_a", RAW_SESSION);
    let affinity_a = account_scoped_id("inst_account_a", RAW_PARENT);
    let scoped_b = account_scoped_id("inst_account_b", RAW_SESSION);
    assert_eq!(scoped_a, account_scoped_id("inst_account_a", RAW_SESSION));
    assert_ne!(scoped_a, scoped_b);
    assert_ne!(scoped_a, affinity_a);
    assert_ne!(
        scoped_a,
        account_scoped_id("inst_account_a", scoped_a.as_str())
    );

    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let downstream = vec![
        ("session_id".to_owned(), RAW_SESSION.to_owned()),
        (
            "x-client-request-id".to_owned(),
            "req_downstream".to_owned(),
        ),
        ("session-id".to_owned(), RAW_PARENT.to_owned()),
        ("thread-id".to_owned(), "thr_downstream".to_owned()),
    ];

    let mut first = sample_call(ResponsesHarness::Pi, TOKEN);
    first.installation_id = "inst_account_a".to_owned();
    first.session_id = RAW_SESSION.to_owned();
    first.request_id = "req_scope_a".to_owned();
    first.prompt = "prompt-scope-a".to_owned();
    first.downstream_headers = downstream.clone();
    let parsed_first = complete(&session, first).await;
    assert_scoped_pi(&parsed_first, &scoped_a, TOKEN);
    assert_upstream_ids(&parsed_first.events, true);
    assert_attempt_stays_off_the_wire(&parsed_first.request.headers, "req_scope_a");

    let mut second = sample_call(ResponsesHarness::Pi, TOKEN);
    second.installation_id = "inst_account_a".to_owned();
    second.session_id = RAW_SESSION.to_owned();
    second.request_id = "req_scope_a2".to_owned();
    second.prompt = "prompt-scope-a2".to_owned();
    second.downstream_headers = downstream.clone();
    let parsed_second = complete(&session, second).await;
    assert_scoped_pi(&parsed_second, &scoped_a, TOKEN);
    assert_upstream_ids(&parsed_second.events, true);
    assert!(json_body_text(&parsed_second.request.body).contains("prompt-scope-a2"));
    assert!(!json_body_text(&parsed_second.request.body).contains("prompt-scope-a\""));

    let mut third = sample_call(ResponsesHarness::Pi, TOKEN);
    third.installation_id = "inst_account_b".to_owned();
    third.session_id = RAW_SESSION.to_owned();
    third.request_id = "req_scope_b".to_owned();
    third.prompt = "prompt-scope-b".to_owned();
    third.downstream_headers = downstream.clone();
    let parsed_third = complete(&session, third).await;
    assert_scoped_pi(&parsed_third, &scoped_b, TOKEN);
    assert_upstream_ids(&parsed_third.events, true);
    assert_ne!(
        header(&parsed_third.request.headers, "session_id"),
        Some(scoped_a.as_str())
    );

    let mut fourth = sample_call(ResponsesHarness::OpenCode, TOKEN);
    fourth.installation_id = "inst_account_a".to_owned();
    fourth.session_id = RAW_SESSION.to_owned();
    fourth.request_id = "req_scope_oc".to_owned();
    fourth.prompt = "prompt-scope-oc".to_owned();
    fourth.identity = ResponsesRequestIdentity {
        session_affinity_id: Some(RAW_PARENT.to_owned()),
    };
    fourth.downstream_headers = downstream;
    let parsed_fourth = complete(&session, fourth).await;
    assert_account_token(&parsed_fourth.request.headers, TOKEN);
    assert_eq!(
        header(&parsed_fourth.request.headers, "session-id"),
        Some(affinity_a.as_str())
    );
    assert_eq!(
        header(&parsed_fourth.request.headers, "thread-id"),
        Some(scoped_a.as_str())
    );
    assert_eq!(
        header(&parsed_fourth.request.headers, "x-client-request-id"),
        Some(scoped_a.as_str())
    );
    assert_raw_scope_absent(&parsed_fourth.request.headers, &parsed_fourth.request.body);
    assert_upstream_ids(&parsed_fourth.events, false);
    assert_attempt_stays_off_the_wire(&parsed_fourth.request.headers, "req_scope_oc");

    let records = origin.recorded.lock().expect("records").clone();
    assert_eq!(records.len(), 4, "records={}", records.len());
    assert_eq!(
        header(&records[0].headers, "session_id"),
        Some(scoped_a.as_str())
    );
    assert_eq!(
        header(&records[1].headers, "session_id"),
        Some(scoped_a.as_str())
    );
    assert_eq!(
        header(&records[0].headers, "x-client-request-id"),
        header(&records[1].headers, "x-client-request-id")
    );
    assert_eq!(
        header(&records[2].headers, "session_id"),
        Some(scoped_b.as_str())
    );
    assert_eq!(
        header(&records[3].headers, "session-id"),
        Some(affinity_a.as_str())
    );
    assert_eq!(
        header(&records[3].headers, "thread-id"),
        Some(scoped_a.as_str())
    );
    for record in &records {
        assert_raw_scope_absent(&record.headers, &record.body);
        assert_eq!(header(&record.headers, "x-cpr-harness-managed"), Some("1"));
    }
    assert_clean(&session, TOKEN);
    assert_clean(&session, TOKEN_PI);
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn timeout_isolates_the_call_before_the_next_request() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::HoldFirst).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let mut first = sample_call(ResponsesHarness::Pi, TOKEN);
    first.prompt = "prompt-a".to_owned();
    first.request_id = "req_attempt_a".to_owned();
    first.timeout = Some(Duration::from_secs(5));
    let mut stream = session
        .open_responses(first, &CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    timeout(Duration::from_secs(20), async {
        loop {
            if !origin.recorded.lock().expect("records").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("origin did not see the first request");
    let error = timeout(Duration::from_secs(20), async {
        loop {
            match stream.next().await {
                Ok(_) => {}
                Err(error) => return error,
            }
        }
    })
    .await
    .expect("timed out call did not finish");
    assert!(matches!(error, HarnessError::Timeout), "{error}");
    assert_eq!(session.authorization_holders(), 0);
    assert!(!session.stderr_snapshot().contains(TOKEN));
    drop(stream);

    {
        let mut second = sample_call(ResponsesHarness::Pi, TOKEN_PI);
        second.prompt = "prompt-b".to_owned();
        second.request_id = "req_attempt_b".to_owned();
        let cancel_second = CancellationToken::new();
        let second_call = drive_call(&session, second, &cancel_second);
        tokio::pin!(second_call);
        let mut released = false;
        let outcome = timeout(Duration::from_secs(30), async {
            loop {
                if !released && origin.recorded.lock().expect("records").len() >= 2 {
                    origin.release.notify_one();
                    released = true;
                }
                tokio::select! {
                    biased;
                    result = &mut second_call => return result,
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .expect("second call hung");
        assert!(
            released,
            "second request was not sent before the late release"
        );
        let parsed =
            outcome.unwrap_or_else(|error| panic!("{error}; stderr={}", session.stderr_snapshot()));
        let body = String::from_utf8_lossy(&parsed.request.body);
        assert!(body.contains("prompt-b"), "{body}");
        assert!(!body.contains("prompt-a"), "{body}");
        let bearer_a = format!("Bearer {TOKEN}");
        let bearer_b = format!("Bearer {TOKEN_PI}");
        assert_eq!(
            header(&parsed.request.headers, "authorization"),
            Some(bearer_b.as_str())
        );
        assert_eq!(
            done_event(&parsed.events).response_id.as_deref(),
            Some("resp_bridge")
        );
        let rendered = format!("{parsed:?}");
        assert!(!rendered.contains("resp_late_a"), "{rendered}");
        let records = origin.recorded.lock().expect("records").clone();
        assert!(records.len() >= 2, "records={}", records.len());
        assert!(String::from_utf8_lossy(&records[0].body).contains("prompt-a"));
        assert_eq!(
            header(&records[0].headers, "authorization"),
            Some(bearer_a.as_str())
        );
        assert!(String::from_utf8_lossy(&records[1].body).contains("prompt-b"));
        assert!(!String::from_utf8_lossy(&records[1].body).contains("prompt-a"));
        assert_eq!(
            header(&records[1].headers, "authorization"),
            Some(bearer_b.as_str())
        );
        let scoped_session = scoped(SESSION);
        assert_eq!(
            header(&records[0].headers, "session_id"),
            Some(scoped_session.as_str())
        );
        assert_eq!(
            header(&records[1].headers, "session_id"),
            Some(scoped_session.as_str())
        );
        assert_attempt_stays_off_the_wire(&records[0].headers, "req_attempt_a");
        assert_attempt_stays_off_the_wire(&records[0].headers, "req_attempt_b");
        assert_attempt_stays_off_the_wire(&records[1].headers, "req_attempt_a");
        assert_attempt_stays_off_the_wire(&records[1].headers, "req_attempt_b");
        assert_attempt_stays_off_the_wire(&parsed.request.headers, "req_attempt_a");
        assert_attempt_stays_off_the_wire(&parsed.request.headers, "req_attempt_b");
        assert_clean(&session, TOKEN);
        assert_clean(&session, TOKEN_PI);
    }
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn upstream_failure_allows_another_call_on_the_same_session() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::DropFirst).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let started = std::time::Instant::now();
    let error = timeout(
        Duration::from_secs(20),
        drive_call(
            &session,
            sample_call(ResponsesHarness::OpenCode, TOKEN),
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("failed upstream hung")
    .expect_err("dropped upstream should fail");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "elapsed {:?}",
        started.elapsed()
    );
    assert!(
        matches!(error, HarnessError::Worker { .. } | HarnessError::Upstream),
        "{error}"
    );
    assert_eq!(session.authorization_holders(), 0);
    let parsed = call(&session, ResponsesHarness::OpenCode, TOKEN).await;
    assert_contract(&parsed, false);
    assert_eq!(
        tool_event(&parsed.events).id.as_deref(),
        Some("call_bridge")
    );
    assert_clean(&session, TOKEN);
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn event_window_waits_until_the_caller_pulls() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let mut limited = sample_call(ResponsesHarness::OpenCode, TOKEN);
    limited.event_window = 1;
    let cancel = CancellationToken::new();
    let mut stream = session
        .open_responses(limited, &cancel)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    timeout(Duration::from_secs(30), async {
        loop {
            if session.event_waits() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("event window did not wait");
    cancel.cancel();
    let mut events = 0usize;
    let error = timeout(Duration::from_secs(20), async {
        loop {
            match stream.next().await {
                Ok(ResponseItem::Event(_)) => events += 1,
                Ok(_) => {}
                Err(error) => return error,
            }
        }
    })
    .await
    .expect("cancel did not finish the waiting call");
    assert!(matches!(error, HarnessError::Cancelled), "{error}");
    assert!(events <= 3, "buffered events={events}");
    assert_eq!(session.authorization_holders(), 0);
    let parsed = call(&session, ResponsesHarness::OpenCode, TOKEN).await;
    assert_contract(&parsed, false);
    assert!(parsed.events.len() > 3, "events={}", parsed.events.len());
    assert_clean(&session, TOKEN);
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn event_credit_drops_the_abort_listener() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let mut limited = sample_call(ResponsesHarness::OpenCode, TOKEN);
    limited.event_window = 1;
    let cancel = CancellationToken::new();
    let mut stream = session
        .open_responses(limited, &cancel)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let mut request = None;
    let mut events = Vec::new();
    let mut done = None;
    let mut credited = 0usize;
    timeout(Duration::from_secs(40), async {
        loop {
            if done.is_some() {
                break;
            }
            let ahead = session.event_waits() > credited;
            if ahead || credited >= 3 {
                match stream.next().await {
                    Ok(ResponseItem::Request(value)) => request = Some(value),
                    Ok(ResponseItem::Event(event)) => {
                        if ahead {
                            credited += 1;
                        }
                        events.push(event);
                    }
                    Ok(ResponseItem::Done(value)) => done = Some(value),
                    Err(error) => panic!("{error}; stderr={}", session.stderr_snapshot()),
                }
            } else {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        }
    })
    .await
    .expect("event credits did not finish");
    assert!(
        credited >= 3,
        "credited={credited} waits={}",
        session.event_waits()
    );
    let done = done.expect("done");
    assert!(done.event_waits >= 3, "waits={}", done.event_waits);
    assert_eq!(done.abort_listeners, 0);
    assert!(request.is_some());
    assert_upstream_ids(&events, false);
    assert_account_token(&request.expect("request").headers, TOKEN);
    assert_clean(&session, TOKEN);
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn upstream_permit_rejects_other_hosts_and_conflicting_auth() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind permit listener");
    let addr = listener.local_addr().expect("permit addr");
    let (opencode, pi) = checkouts();
    let session = open_session(worker_config(&opencode, &pi, addr, None)).await;
    let cases = [
        ("https://evil.example/v1", Vec::new()),
        ("https://api.openai.com.evil.example/v1", Vec::new()),
        ("http://api.openai.com/v1", Vec::new()),
    ];
    for (base_url, headers) in cases {
        let mut call = sample_call(ResponsesHarness::OpenCode, TOKEN);
        call.base_url = base_url.to_owned();
        call.downstream_headers = headers;
        call.timeout = Some(Duration::from_secs(20));
        let error = drive_call(&session, call, &CancellationToken::new())
            .await
            .expect_err(base_url);
        assert!(
            matches!(error, HarnessError::RemoteDial),
            "{base_url}: {error}"
        );
        assert!(
            timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "{base_url} dialed the test listener"
        );
        assert_eq!(session.authorization_holders(), 0);
        assert!(!session.stderr_snapshot().contains(TOKEN));
        assert!(!error.to_string().contains(TOKEN));
    }
    session.shutdown().await.expect("shutdown");

    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let mut call = sample_call(ResponsesHarness::OpenCode, TOKEN);
    call.downstream_headers = vec![
        ("x-api-key".to_owned(), "sk-conflict-header".to_owned()),
        ("api-key".to_owned(), "second-auth".to_owned()),
        (
            "Authorization".to_owned(),
            "Bearer sk-duplicate-auth".to_owned(),
        ),
    ];
    let parsed = complete(&session, call).await;
    assert_account_token(&parsed.request.headers, TOKEN);
    assert!(header(&parsed.request.headers, "x-api-key").is_none());
    assert!(header(&parsed.request.headers, "api-key").is_none());
    let wire = origin.headers_for_last();
    assert_account_token(&wire, TOKEN);
    assert!(header(&wire, "x-api-key").is_none());
    assert!(header(&wire, "api-key").is_none());
    assert_eq!(header(&wire, "x-cpr-harness-managed"), Some("1"));
    assert_eq!(origin.recorded.lock().expect("records").len(), 1);
    let stderr = session.stderr_snapshot();
    assert!(!stderr.contains("sk-conflict-header"));
    assert!(!stderr.contains("second-auth"));
    assert!(!stderr.contains("sk-duplicate-auth"));
    assert!(!stderr.contains(TOKEN));
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn drifted_checkout_is_rejected() {
    let (opencode, pi) = checkouts();
    let manifest = source_manifest();
    let files = manifest["files"].as_array().expect("manifest files");
    let opencode_dir = tempfile::tempdir().expect("opencode copy");
    let pi_dir = tempfile::tempdir().expect("pi copy");
    copy_manifest_files(files, "opencode", &opencode, opencode_dir.path());
    copy_manifest_files(files, "pi", &pi, pi_dir.path());
    let target = pi_dir
        .path()
        .join("packages/ai/src/api/openai-responses.ts");
    let mut bytes = std::fs::read(&target).expect("read pinned pi source");
    bytes.push(b'\n');
    std::fs::write(&target, bytes).expect("drift pinned pi source");
    let origin = Origin::serve(OriginMode::Sse).await;
    let config = worker_config(opencode_dir.path(), pi_dir.path(), origin.addr, None);
    match HarnessSession::open(config).await {
        Ok(session) => {
            let stderr = session.stderr_snapshot();
            let _ = session.shutdown().await;
            panic!("drifted checkout started a session: {stderr}");
        }
        Err(error) => {
            assert!(matches!(error, HarnessError::Source), "{error}");
            assert!(!error.to_string().contains(TOKEN));
        }
    }
    assert!(origin.recorded.lock().expect("records").is_empty());
}

fn copy_manifest_files(files: &[Value], root_name: &str, source: &Path, dest: &Path) {
    for file in files {
        if file["root"].as_str() != Some(root_name) {
            continue;
        }
        let relative = file["path"].as_str().expect("manifest path");
        let from = source.join(relative);
        let to = dest.join(relative);
        std::fs::create_dir_all(to.parent().expect("parent")).expect("create source dir");
        std::fs::copy(&from, &to).unwrap_or_else(|error| panic!("copy {relative}: {error}"));
    }
}

fn worker_config(
    opencode: &Path,
    pi: &Path,
    origin: SocketAddr,
    proxy: Option<OutboundProxy>,
) -> HarnessWorkerConfig {
    HarnessWorkerConfig {
        node: node_binary(),
        worker_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("harness-worker"),
        opencode_checkout: opencode.to_path_buf(),
        pi_checkout: pi.to_path_buf(),
        proxy,
        allow_remote_dial: false,
        dial_loopback: Some(origin),
    }
}

async fn open_session(config: HarnessWorkerConfig) -> HarnessSession {
    match HarnessSession::open(config).await {
        Ok(session) => session,
        Err(error) => panic!("{error}"),
    }
}

fn checkouts() -> (PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../../../../sources");
    let opencode = root.join("opencode/checkout");
    let pi = root.join("pi/checkout");
    assert!(
        opencode
            .join("packages/ai/src/protocols/openai-responses.ts")
            .is_file(),
        "{}",
        opencode.display()
    );
    assert!(
        pi.join("packages/ai/src/api/openai-responses.ts").is_file(),
        "{}",
        pi.display()
    );
    (opencode, pi)
}

fn node_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CPR_HARNESS_NODE") {
        return PathBuf::from(path);
    }
    let finder = if cfg!(windows) { "where.exe" } else { "which" };
    let output = Command::new(finder)
        .arg("node")
        .output()
        .expect("locate node");
    assert!(output.status.success(), "node is not on PATH");
    let text = String::from_utf8_lossy(&output.stdout);
    PathBuf::from(text.lines().next().expect("node path").trim())
}

fn git_head(path: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn node_at_least(version: &str, major: u64, minor: u64, patch: u64) -> bool {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let parsed = (
        parts
            .next()
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or(0),
        parts
            .next()
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or(0),
        parts
            .next()
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or(0),
    );
    parsed >= (major, minor, patch)
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid_text = pid.to_string();
    if cfg!(windows) {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid_text}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&output.stdout).contains(&pid_text)
    } else {
        Command::new("kill")
            .args(["-0", &pid_text])
            .status()
            .is_ok_and(|status| status.success())
    }
}

fn kill_pid(pid: u32) {
    let pid_text = pid.to_string();
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid_text])
            .status()
    } else {
        Command::new("kill").args(["-9", &pid_text]).status()
    };
    let _ = status;
}

#[derive(Clone, Copy)]
enum OriginMode {
    Sse,
    Stall,
    HoldFirst,
    DropFirst,
}

struct Origin {
    addr: SocketAddr,
    seen: Arc<Notify>,
    disconnected: Arc<Notify>,
    release: Arc<Notify>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

#[derive(Clone)]
struct RecordedRequest {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Origin {
    async fn serve(mode: OriginMode) -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind origin");
        let addr = listener.local_addr().expect("origin addr");
        let seen = Arc::new(Notify::new());
        let disconnected = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen_task = Arc::clone(&seen);
        let disconnected_task = Arc::clone(&disconnected);
        let release_task = Arc::clone(&release);
        let recorded_task = Arc::clone(&recorded);
        let arrivals = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let seen_task = Arc::clone(&seen_task);
                let disconnected_task = Arc::clone(&disconnected_task);
                let release_task = Arc::clone(&release_task);
                let recorded_task = Arc::clone(&recorded_task);
                let arrivals = Arc::clone(&arrivals);
                tokio::spawn(handle_origin(
                    socket,
                    mode,
                    seen_task,
                    disconnected_task,
                    release_task,
                    recorded_task,
                    arrivals,
                ));
            }
        });
        Self {
            addr,
            seen,
            disconnected,
            release,
            recorded,
        }
    }

    fn headers_for_last(&self) -> Vec<(String, String)> {
        self.recorded
            .lock()
            .expect("records")
            .last()
            .map(|record| record.headers.clone())
            .unwrap_or_default()
    }
}

async fn handle_origin(
    mut socket: TcpStream,
    mode: OriginMode,
    seen: Arc<Notify>,
    disconnected: Arc<Notify>,
    release: Arc<Notify>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    arrivals: Arc<AtomicUsize>,
) {
    let Ok(message) = read_http_request(&mut socket).await else {
        return;
    };
    let arrival = arrivals.fetch_add(1, Ordering::SeqCst);
    recorded.lock().expect("records").push(RecordedRequest {
        headers: message.headers,
        body: message.body,
    });
    match mode {
        OriginMode::Sse => {
            seen.notify_one();
            write_sse(&mut socket).await;
        }
        OriginMode::Stall => {
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .await;
            seen.notify_one();
            let mut buffer = [0; 8];
            let _ = socket.read(&mut buffer).await;
            disconnected.notify_one();
        }
        OriginMode::HoldFirst if arrival == 0 => {
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                .await;
            seen.notify_one();
            let mut buffer = [0; 8];
            tokio::select! {
                _ = release.notified() => {
                    let _ = socket.write_all(late_sse().as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                _ = socket.read(&mut buffer) => {}
            }
            disconnected.notify_one();
        }
        OriginMode::DropFirst if arrival == 0 => {
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                .await;
            seen.notify_one();
            drop(socket);
            disconnected.notify_one();
        }
        OriginMode::HoldFirst => {
            seen.notify_one();
            tokio::time::sleep(Duration::from_millis(200)).await;
            write_sse(&mut socket).await;
        }
        OriginMode::DropFirst => {
            seen.notify_one();
            write_sse(&mut socket).await;
        }
    }
}

async fn write_sse(socket: &mut TcpStream) {
    let body = sse_body();
    let header = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(header.as_bytes()).await;
    let _ = socket.write_all(&body).await;
    let _ = socket.shutdown().await;
}

fn late_sse() -> String {
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_late_a\",\"status\":\"completed\",\"output\":[]}}\n\n".to_owned()
}

struct Proxy {
    addr: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
}

impl Proxy {
    async fn serve(origin: SocketAddr) -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        let targets = Arc::new(Mutex::new(Vec::new()));
        let targets_task = Arc::clone(&targets);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let targets_task = Arc::clone(&targets_task);
                tokio::spawn(handle_proxy(socket, origin, targets_task));
            }
        });
        Self { addr, targets }
    }

    fn saw_absolute_http(&self) -> bool {
        self.targets
            .lock()
            .expect("targets")
            .iter()
            .any(|target| target.starts_with("http://") && target.contains("/responses"))
    }
}

async fn handle_proxy(mut client: TcpStream, origin: SocketAddr, targets: Arc<Mutex<Vec<String>>>) {
    let Ok(message) = read_http_request(&mut client).await else {
        return;
    };
    targets
        .lock()
        .expect("targets")
        .push(message.target.clone());
    let Ok(mut upstream) = TcpStream::connect(origin).await else {
        return;
    };
    let forwarded = rewrite_origin_form(&message);
    if upstream.write_all(&forwarded).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut upstream, &mut client).await;
}

struct HttpMessage {
    target: String,
    headers: Vec<(String, String)>,
    head: Vec<u8>,
    body: Vec<u8>,
}

async fn read_http_request(socket: &mut TcpStream) -> Result<HttpMessage, ()> {
    let mut buffered = Vec::new();
    let mut chunk = [0; 8192];
    let header_end = loop {
        let size = socket.read(&mut chunk).await.map_err(|_| ())?;
        if size == 0 {
            return Err(());
        }
        buffered.extend_from_slice(&chunk[..size]);
        if let Some(position) = find_subsequence(&buffered, b"\r\n\r\n") {
            break position;
        }
        if buffered.len() > 4_000_000 {
            return Err(());
        }
    };
    let head = buffered[..header_end].to_vec();
    let header_text = String::from_utf8_lossy(&head);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffered[header_end + 4..].to_vec();
    while body.len() < length {
        let size = socket.read(&mut chunk).await.map_err(|_| ())?;
        if size == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..size]);
    }
    body.truncate(length);
    Ok(HttpMessage {
        target,
        headers,
        head,
        body,
    })
}

fn rewrite_origin_form(message: &HttpMessage) -> Vec<u8> {
    let header_text = String::from_utf8_lossy(&message.head);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("POST / HTTP/1.1");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("POST");
    let target = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let path = absolute_path(target);
    let mut rendered = format!("{method} {path} {version}");
    for line in lines {
        rendered.push_str("\r\n");
        rendered.push_str(line);
    }
    rendered.push_str("\r\n\r\n");
    let mut bytes = rendered.into_bytes();
    bytes.extend_from_slice(&message.body);
    bytes
}

fn absolute_path(target: &str) -> String {
    let Some(rest) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
    else {
        return target.to_owned();
    };
    rest.find('/')
        .map(|index| rest[index..].to_owned())
        .unwrap_or_else(|| "/".to_owned())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn sse_body() -> Vec<u8> {
    let mut body = String::new();
    while body.len() < 64 * 1024 {
        body.push_str(": pad\n");
    }
    body.push('\n');
    let item = |arguments: &str| {
        json!({
            "id": "fc_item",
            "type": "function_call",
            "call_id": "call_bridge",
            "name": "lookup",
            "arguments": arguments,
        })
    };
    let events = [
        json!({"type":"response.created","response":{"id":"resp_bridge","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item": item("")}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_item","delta":"{\"q\":\"x\"}"}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_item","arguments":"{\"q\":\"x\"}"}),
        json!({"type":"response.output_item.done","output_index":0,"item": item("{\"q\":\"x\"}")}),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_bridge",
                "status": "completed",
                "output": [item("{\"q\":\"x\"}")],
                "usage": {"input_tokens": 8, "output_tokens": 4, "total_tokens": 12}
            }
        }),
    ];
    for event in events {
        body.push_str(&format!("data: {event}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

#[tokio::test]
async fn complete_transcript_uses_each_sources_history_tools_and_wire_events() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    for (harness, bearer) in [
        (ResponsesHarness::OpenCode, TOKEN),
        (ResponsesHarness::Pi, TOKEN_PI),
    ] {
        let mut request = sample_call(harness, bearer);
        request.prompt.clear();
        request.service_tier = Some("priority".to_owned());
        request.responses_body = Some(json!({
            "model": MODEL,
            "instructions": "Work with prior context",
            "input": [
                {"type":"message","role":"system","content":[{"type":"input_text","text":"System update"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Earlier user"}]},
                {"type":"reasoning","id":"rs_history","encrypted_content":"encrypted-history","summary":[]},
                {"type":"function_call","id":"fc_history","call_id":"call_history","name":"lookup","arguments":"{\"query\":\"before\"}"},
                {"type":"function_call_output","call_id":"call_history","output":"previous result"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Current turn"}]}
            ],
            "tools": [
                {"type":"function","name":"lookup","description":"Look up a value","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}},
                {"type":"function","name":"dangerous","description":"Do not authorize","parameters":{"type":"object"}}
            ],
            "tool_choice": {"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"lookup"}]},
            "prompt_cache_key": "explicit-history-cache",
            "reasoning": {"effort":"low","summary":"auto"},
            "top_p": 0.7,
            "parallel_tool_calls": false,
            "service_tier": "priority"
        }));
        let result = complete(&session, request).await;
        let sent = json_body(&result.request.body);
        assert_eq!(sent["service_tier"], "priority");
        assert_eq!(sent["top_p"], 0.7);
        assert_eq!(sent["parallel_tool_calls"], false);
        assert!(
            sent["input"].as_array().expect("input").len() >= 5,
            "{sent}"
        );
        assert!(sent.to_string().contains("Earlier user"));
        assert!(sent.to_string().contains("Current turn"));
        assert!(
            sent.to_string()
                .matches(&format!("call_{}", scoped("call_history")))
                .count()
                >= 2
        );
        assert!(
            sent.to_string()
                .contains(&format!("fc_{}", scoped("fc_history")))
        );
        assert!(!sent.to_string().contains("call_history"));
        assert!(sent.to_string().contains("encrypted-history"));
        assert_eq!(
            sent["tool_choice"],
            json!({"type":"allowed_tools","mode":"required", "tools":[{"type":"function","name":"lookup"}]})
        );
        assert_eq!(sent["tools"][0]["name"], "lookup");
        assert_eq!(sent["tools"][1]["name"], "dangerous");
        assert_eq!(sent["prompt_cache_key"], scoped("explicit-history-cache"));
        let outbound_wire = result
            .events
            .iter()
            .filter(|event| event.kind == "wire")
            .flat_map(|event| {
                STANDARD
                    .decode(event.payload["frame_base64"].as_str().expect("raw frame"))
                    .expect("base64 SSE frame")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            outbound_wire,
            sse_body(),
            "upstream SSE bytes must be preserved"
        );
        assert!(
            result
                .events
                .iter()
                .any(|event| event.kind == "head" && event.payload["status"] == 200)
        );
        assert_clean(&session, bearer);
    }
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn unsupported_response_input_fails_before_dial_for_both_sources() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    for (harness, bearer) in [
        (ResponsesHarness::OpenCode, TOKEN),
        (ResponsesHarness::Pi, TOKEN_PI),
    ] {
        let mut request = sample_call(harness, bearer);
        request.prompt.clear();
        request.responses_body = Some(json!({
            "model": MODEL,
            "input": [{"type":"message","role":"user","content":[{"type":"input_audio","data":"not-a-supported-inline-part"}]}],
        }));
        let error = drive_call(&session, request, &CancellationToken::new())
            .await
            .expect_err("unsupported input must fail before request reaches origin");
        assert!(
            matches!(error, HarnessError::InvalidInput { .. }),
            "{error}"
        );
        let mut request = sample_call(harness, bearer);
        request.prompt.clear();
        request.responses_body = Some(json!({
            "model": MODEL,
            "input": "hello",
            "unsupported_safety_override": "must-not-disappear",
        }));
        let error = drive_call(&session, request, &CancellationToken::new())
            .await
            .expect_err("unsupported option must fail before upstream send");
        assert!(
            matches!(error, HarnessError::InvalidInput { .. }),
            "{error}"
        );
        let mut request = sample_call(harness, bearer);
        request.prompt.clear();
        request.responses_body = Some(json!({
            "model": MODEL,
            "input": "hello",
            "tools": [{"type":"function","name":"dangerous","parameters":{"type":"object"}}],
            "tool_choice": {"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"safe"}]}
        }));
        let error = drive_call(&session, request, &CancellationToken::new())
            .await
            .expect_err("undeclared allowed tool must fail before dial");
        assert!(
            matches!(error, HarnessError::InvalidInput { .. }),
            "{error}"
        );
    }
    assert!(origin.recorded.lock().expect("origin requests").is_empty());
    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn large_image_and_tool_result_cross_managed_frames_without_losing_content() {
    let (opencode, pi) = checkouts();
    let origin = Origin::serve(OriginMode::Sse).await;
    let session = open_session(worker_config(&opencode, &pi, origin.addr, None)).await;
    let image = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(vec![0x42; 5 * 1024 * 1024])
    );
    let output = "x".repeat(9 * 1024 * 1024);
    for (harness, bearer) in [
        (ResponsesHarness::OpenCode, TOKEN),
        (ResponsesHarness::Pi, TOKEN_PI),
    ] {
        for (index, input) in [
            json!([{"type":"message","role":"user","content":[{"type":"input_image","image_url":image}]}]),
            json!([
                {"type":"function_call","id":"fc_big","call_id":"call_big","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_big","output":output},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Continue"}]}
            ]),
        ].into_iter().enumerate() {
            let mut call = sample_call(harness, bearer);
            call.prompt.clear();
            call.request_id = format!("req_large_{harness:?}_{index}");
            call.responses_body = Some(json!({"model":MODEL,"input":input}));
            let result = timeout(Duration::from_secs(120), complete(&session, call))
                .await.expect("large managed request deadline");
            let sent = json_body(&result.request.body);
            if index == 0 {
                assert!(sent.to_string().contains("data:image/png;base64,"));
                assert!(result.request.body.len() > 6 * 1024 * 1024);
            } else {
                assert!(sent.to_string().contains(&"x".repeat(64)));
                assert!(result.request.body.len() > 8 * 1024 * 1024);
            }
        }
    }
    assert_eq!(origin.recorded.lock().expect("origin requests").len(), 4);
    session.shutdown().await.expect("shutdown");
}
