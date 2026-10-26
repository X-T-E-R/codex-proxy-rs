//! OpenCode 与 pi 账号的上游 OAuth 执行器
//!
//! 固定版本的 TypeScript 负责授权地址、PKCE、换票和刷新
//! Rust 只保存 pending、client ID 和账号凭据

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use uuid::Uuid;

use super::token_client::OFFICIAL_CODEX_OAUTH_CLIENT_ID;

pub(crate) const HARNESS_METHOD_CODEX: &str = "codex";
pub(crate) const HARNESS_METHOD_OPENCODE: &str = "opencode";
pub(crate) const HARNESS_METHOD_PI: &str = "pi";
pub(crate) const OPENCODE_REDIRECT_URI: &str = "http://127.0.0.1:1456/auth/callback";
pub(crate) const PI_REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
pub(crate) const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub(crate) const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
const AUTHORIZE_PREFIX: &str = "https://auth.openai.com/api/accounts/authorize?";
const RUNNER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/harness-worker/oauth-run.mjs");
const RUNNER_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_RUNNER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HarnessMethod {
    OpenCode,
    Pi,
}

impl HarnessMethod {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::OpenCode => HARNESS_METHOD_OPENCODE,
            Self::Pi => HARNESS_METHOD_PI,
        }
    }

    pub(crate) const fn authentication_kind(self) -> &'static str {
        match self {
            Self::OpenCode => super::CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING,
            Self::Pi => super::CODEX_AUTHENTICATION_KIND_PI_OPENAI,
        }
    }

    pub(crate) const fn redirect_uri(self) -> &'static str {
        match self {
            Self::OpenCode => OPENCODE_REDIRECT_URI,
            Self::Pi => PI_REDIRECT_URI,
        }
    }

    pub(crate) fn from_method(value: &str) -> Option<Self> {
        match value {
            HARNESS_METHOD_OPENCODE => Some(Self::OpenCode),
            HARNESS_METHOD_PI => Some(Self::Pi),
            _ => None,
        }
    }
}

pub(crate) fn method_for_kind(kind: &str) -> Option<HarnessMethod> {
    match kind {
        super::CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING => Some(HarnessMethod::OpenCode),
        super::CODEX_AUTHENTICATION_KIND_PI_OPENAI => Some(HarnessMethod::Pi),
        _ => None,
    }
}

pub(crate) fn is_harness_kind(kind: &str) -> bool {
    method_for_kind(kind).is_some()
}

#[derive(Debug)]
pub(crate) enum HarnessRunnerError {
    Callback,
    Token,
    Upstream,
}

pub(crate) struct HarnessAuthorization {
    pub url: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub redirect_uri: String,
    pub host_id: String,
}

pub(crate) struct HarnessTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub client_id: String,
    pub scopes: String,
    pub id_token: Option<String>,
}

pub(crate) struct HarnessCall {
    pub method: HarnessMethod,
    pub proxy_url: Option<String>,
    pub upstream_origin: Option<String>,
}

pub(crate) fn resolve_host_file(configured: Option<&Path>) -> PathBuf {
    if let Some(path) = configured {
        return path.to_path_buf();
    }
    if let Some(path) = crate::transport::session::published_openai_harness_host_file() {
        return path.to_path_buf();
    }
    if let Some(path) = env_path("CPR_OPENAI_HARNESS_HOST_ID_FILE") {
        return path;
    }
    if let Some(dir) = env_path("CPR_RUNTIME_DATA_DIR") {
        return dir.join("openai_harness_host_id");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("openai_harness_host_id")
}

pub(crate) fn load_or_create_host_uuid(path: &Path) -> Result<Uuid, HarnessRunnerError> {
    if let Some(text) = read_host_text(path)? {
        return parse_host_uuid(&text).ok_or(HarnessRunnerError::Upstream);
    }
    let uuid = Uuid::new_v4();
    write_host_uuid(path, uuid)?;
    if read_host_text(path)?.as_deref() != Some(uuid.to_string().as_str()) {
        return parse_host_uuid(&read_host_text(path)?.unwrap_or_default())
            .ok_or(HarnessRunnerError::Upstream);
    }
    Ok(uuid)
}

pub(crate) fn valid_harness_client_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && value != OFFICIAL_CODEX_OAUTH_CLIENT_ID
        && value != REGISTRATION_CLIENT_ID
}

pub(crate) fn valid_harness_scope(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.chars().any(char::is_control)
        && value
            .split_whitespace()
            .any(|scope| scope == DIRECT_TOKEN_SCOPE)
}

pub(crate) fn valid_host_urn(value: &str) -> bool {
    value
        .strip_prefix("urn:uuid:")
        .and_then(parse_host_uuid)
        .is_some_and(|uuid| value == format!("urn:uuid:{uuid}"))
}

pub(crate) async fn authorize(
    call: &HarnessCall,
    host_uuid: Uuid,
    saved_client_id: Option<&str>,
) -> Result<HarnessAuthorization, HarnessRunnerError> {
    let host_id = format!("urn:uuid:{}", host_uuid.hyphenated());
    let mut request = json!({
        "op": "authorize",
        "harness": call.method.as_str(),
        "proxyUrl": call.proxy_url.as_deref().unwrap_or(""),
        "upstreamOrigin": call.upstream_origin.as_deref().unwrap_or(""),
    });
    let object = request.as_object_mut().expect("authorize request object");
    match call.method {
        HarnessMethod::OpenCode => {
            object.insert("redirectUri".to_owned(), json!(OPENCODE_REDIRECT_URI));
            object.insert("hostId".to_owned(), json!(host_id));
            object.insert(
                "savedClientId".to_owned(),
                saved_client_id
                    .map(|value| json!(value))
                    .unwrap_or(Value::Null),
            );
        }
        HarnessMethod::Pi => {
            object.insert(
                "hostUuid".to_owned(),
                json!(host_uuid.hyphenated().to_string()),
            );
        }
    }
    let value = invoke(&request).await?;
    let authorization = HarnessAuthorization {
        url: required_string(&value, "url")?,
        state: required_string(&value, "state")?,
        nonce: required_string(&value, "nonce")?,
        verifier: required_string(&value, "verifier")?,
        redirect_uri: required_string(&value, "redirectUri")?,
        host_id: required_string(&value, "hostId")?,
    };
    if authorization.redirect_uri != call.method.redirect_uri()
        || authorization.host_id != host_id
        || !acceptable_secret(&authorization.state)
        || !acceptable_secret(&authorization.nonce)
        || !acceptable_secret(&authorization.verifier)
        || !authorize_url_matches(&authorization.url, call.method, &host_id, saved_client_id)
    {
        return Err(HarnessRunnerError::Callback);
    }
    Ok(authorization)
}

pub(crate) async fn exchange(
    call: &HarnessCall,
    callback_url: &str,
    expected_state: &str,
    verifier: &str,
    nonce: &str,
    saved_client_id: Option<&str>,
) -> Result<HarnessTokens, HarnessRunnerError> {
    let value = invoke(&json!({
        "op": "exchange",
        "harness": call.method.as_str(),
        "callbackUrl": callback_url,
        "expectedState": expected_state,
        "verifier": verifier,
        "nonce": nonce,
        "redirectUri": call.method.redirect_uri(),
        "savedClientId": saved_client_id,
        "proxyUrl": call.proxy_url.as_deref().unwrap_or(""),
        "upstreamOrigin": call.upstream_origin.as_deref().unwrap_or(""),
    }))
    .await?;
    parse_tokens(&value)
}

pub(crate) async fn refresh(
    call: &HarnessCall,
    client_id: &str,
    refresh_token: &str,
    access_token: &str,
    scopes: &str,
    expires_at_ms: u64,
) -> Result<HarnessTokens, HarnessRunnerError> {
    let scopes = scopes
        .split_whitespace()
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>();
    let value = invoke(&json!({
        "op": "refresh",
        "harness": call.method.as_str(),
        "clientId": client_id,
        "refreshToken": refresh_token,
        "accessToken": access_token,
        "scopes": scopes,
        "expiresAt": expires_at_ms,
        "proxyUrl": call.proxy_url.as_deref().unwrap_or(""),
        "upstreamOrigin": call.upstream_origin.as_deref().unwrap_or(""),
    }))
    .await?;
    let tokens = parse_tokens(&value)?;
    if tokens.client_id != client_id {
        return Err(HarnessRunnerError::Token);
    }
    Ok(tokens)
}

fn authorize_url_matches(
    value: &str,
    method: HarnessMethod,
    host_id: &str,
    saved_client_id: Option<&str>,
) -> bool {
    if !value.starts_with(AUTHORIZE_PREFIX) || value.contains(OFFICIAL_CODEX_OAUTH_CLIENT_ID) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || url.host_str() != Some("auth.openai.com")
        || url.path() != "/api/accounts/authorize"
    {
        return false;
    }
    let mut client_id = None;
    let mut redirect = None;
    let mut host = None;
    let mut scope = None;
    let mut resource = None;
    let mut hint = None;
    for (key, item) in url.query_pairs() {
        match key.as_ref() {
            "client_id" => client_id = Some(item.into_owned()),
            "redirect_uri" => redirect = Some(item.into_owned()),
            "ext_agent_host_id" => host = Some(item.into_owned()),
            "scope" => scope = Some(item.into_owned()),
            "resource" => resource = Some(item.into_owned()),
            "agent_name_hint" => hint = Some(item.into_owned()),
            _ => {}
        }
    }
    let expected_client = saved_client_id.unwrap_or(REGISTRATION_CLIENT_ID);
    client_id.as_deref() == Some(expected_client)
        && redirect.as_deref() == Some(method.redirect_uri())
        && host.as_deref() == Some(host_id)
        && resource.as_deref() == Some("https://api.openai.com/v1")
        && scope.is_some_and(|scope| valid_harness_scope(&scope))
        && match (method, saved_client_id) {
            (HarnessMethod::OpenCode, Some(_)) => hint.is_none(),
            (HarnessMethod::OpenCode, None) => hint.as_deref() == Some("OpenCode"),
            (HarnessMethod::Pi, _) => hint.as_deref() == Some("Pi"),
        }
}

fn parse_tokens(value: &Value) -> Result<HarnessTokens, HarnessRunnerError> {
    let access_token = required_string(value, "accessToken")?;
    let refresh_token = required_string(value, "refreshToken")?;
    let client_id = required_string(value, "clientId")?;
    let scopes = match value.get("scopes") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().ok_or(HarnessRunnerError::Token))
            .collect::<Result<Vec<_>, _>>()?
            .join(" "),
        Some(Value::String(scopes)) => scopes.clone(),
        _ => return Err(HarnessRunnerError::Token),
    };
    let expires_at_ms = value
        .get("expiresAt")
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get("expiresAt")
                .and_then(Value::as_f64)
                .map(|value| value as u64)
        })
        .ok_or(HarnessRunnerError::Token)?;
    let expires_at = DateTime::<Utc>::from_timestamp_millis(
        i64::try_from(expires_at_ms).map_err(|_| HarnessRunnerError::Token)?,
    )
    .ok_or(HarnessRunnerError::Token)?;
    let id_token = match value.get("idToken") {
        None | Some(Value::Null) => None,
        Some(Value::String(token)) if !token.is_empty() => Some(token.clone()),
        _ => return Err(HarnessRunnerError::Token),
    };
    if access_token.is_empty()
        || refresh_token.is_empty()
        || !valid_harness_client_id(&client_id)
        || !valid_harness_scope(&scopes)
    {
        return Err(HarnessRunnerError::Token);
    }
    Ok(HarnessTokens {
        access_token,
        refresh_token,
        expires_at,
        client_id,
        scopes,
        id_token,
    })
}

async fn invoke(request: &Value) -> Result<Value, HarnessRunnerError> {
    let body = serde_json::to_vec(request).map_err(|_| HarnessRunnerError::Upstream)?;
    let mut child = Command::new("node")
        .arg("--experimental-strip-types")
        .arg(RUNNER)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| HarnessRunnerError::Upstream)?;
    let mut stdin = child.stdin.take().ok_or(HarnessRunnerError::Upstream)?;
    let mut stdout = child.stdout.take().ok_or(HarnessRunnerError::Upstream)?;
    let mut stderr = child.stderr.take().ok_or(HarnessRunnerError::Upstream)?;
    let stdout_task = tokio::spawn(async move {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output).await;
        output
    });
    let stderr_task = tokio::spawn(async move {
        let mut output = Vec::new();
        let _ = stderr.read_to_end(&mut output).await;
        output
    });
    let wrote = stdin.write_all(&body).await;
    drop(stdin);
    if wrote.is_err() {
        return Err(HarnessRunnerError::Upstream);
    }
    let status = match tokio::time::timeout(RUNNER_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        _ => {
            let _ = child.start_kill();
            return Err(HarnessRunnerError::Upstream);
        }
    };
    let output = stdout_task
        .await
        .map_err(|_| HarnessRunnerError::Upstream)?;
    let _stderr = stderr_task
        .await
        .map_err(|_| HarnessRunnerError::Upstream)?;
    if output.len() > MAX_RUNNER_BYTES {
        return Err(HarnessRunnerError::Upstream);
    }
    let value: Value = serde_json::from_slice(&output).map_err(|_| HarnessRunnerError::Upstream)?;
    if let Some(error) = value.get("error") {
        return Err(match error.get("code").and_then(Value::as_str) {
            Some("callback") => HarnessRunnerError::Callback,
            Some("token") => HarnessRunnerError::Token,
            _ => HarnessRunnerError::Upstream,
        });
    }
    if !status.success() {
        return Err(HarnessRunnerError::Upstream);
    }
    Ok(value)
}

fn required_string(value: &Value, key: &str) -> Result<String, HarnessRunnerError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .ok_or(HarnessRunnerError::Token)
}

fn acceptable_secret(value: &str) -> bool {
    (16..=512).contains(&value.len()) && !value.chars().any(char::is_control)
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn parse_host_uuid(value: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(value.trim()).ok()?;
    if uuid.get_version_num() == 4 && value.trim() == uuid.to_string() {
        Some(uuid)
    } else {
        None
    }
}

fn read_host_text(path: &Path) -> Result<Option<String>, HarnessRunnerError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(HarnessRunnerError::Upstream),
    }
}

fn write_host_uuid(path: &Path, uuid: Uuid) -> Result<(), HarnessRunnerError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|_| HarnessRunnerError::Upstream)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(uuid.to_string().as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| HarnessRunnerError::Upstream)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(HarnessRunnerError::Upstream),
    }
}
