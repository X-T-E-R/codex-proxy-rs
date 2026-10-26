//! 固定上游源码的离线请求、SSE 解析、取消和进程回收

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use gateway_core::{account::OutboundProxy, lifecycle::CancellationToken};
use provider_openai::{
    HarnessError, HarnessEvent, HarnessSession, HarnessWorkerConfig, OPENCODE_SOURCE_COMMIT,
    PI_SOURCE_COMMIT, ParsedResponses, ResponsesCall, ResponsesHarness,
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
    assert_eq!(sources.len(), 3);
    for source in &sources {
        assert_eq!(source.sha256, file_sha256(&source.path), "{}", source.id);
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
    assert!(header(&opencode_result.request.headers, "openai-beta").is_none());
    assert_clean(&session, TOKEN);

    let pi_result = call(&session, ResponsesHarness::Pi, TOKEN_PI).await;
    assert_contract(&pi_result, true);
    let pi_body = json_body(&pi_result.request.body);
    assert!(pi_body.get("max_output_tokens").is_none());
    assert!(pi_body.get("temperature").is_none());
    assert!(pi_body.get("prompt_cache_retention").is_none());
    assert_eq!(pi_body["prompt_cache_key"].as_str(), Some(SESSION));
    let user_agent = header(&pi_result.request.headers, "user-agent").unwrap_or("");
    assert!(user_agent.starts_with("pi/1.4.0 ("), "{user_agent}");
    assert!(user_agent.contains("node/"), "{user_agent}");
    assert_eq!(
        header(&pi_result.request.headers, "session_id"),
        Some(SESSION)
    );
    assert_eq!(
        header(&pi_result.request.headers, "x-client-request-id"),
        Some(SESSION)
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
    let error = session
        .responses(sample_call(ResponsesHarness::Pi, TOKEN), &cancel_call)
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
    let error = session
        .responses(
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
    let error = session
        .responses(
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

async fn call(session: &HarnessSession, harness: ResponsesHarness, token: &str) -> ParsedResponses {
    match session
        .responses(sample_call(harness, token), &CancellationToken::new())
        .await
    {
        Ok(parsed) => parsed,
        Err(error) => panic!("{error}; stderr={}", session.stderr_snapshot()),
    }
}

fn sample_call(harness: ResponsesHarness, token: &str) -> ResponsesCall {
    ResponsesCall {
        harness,
        model: MODEL.to_owned(),
        base_url: "https://api.openai.com/v1".to_owned(),
        session_id: SESSION.to_owned(),
        bearer: SecretString::from(token.to_owned()),
        prompt: PROMPT.to_owned(),
        instructions: Some(INSTRUCTIONS.to_owned()),
        max_output_tokens: Some(20),
        temperature: Some(0.0),
        body_window: 4096,
        timeout: Duration::from_secs(90),
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

fn file_sha256(path: &Path) -> String {
    let output = Command::new(node_binary())
        .arg("-e")
        .arg("const fs=require('node:fs');const crypto=require('node:crypto');process.stdout.write(crypto.createHash('sha256').update(fs.readFileSync(process.argv[1])).digest('hex'))")
        .arg(path)
        .output()
        .expect("hash source");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8")
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
}

struct Origin {
    addr: SocketAddr,
    seen: Arc<Notify>,
    disconnected: Arc<Notify>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

struct RecordedRequest {
    headers: Vec<(String, String)>,
}

impl Origin {
    async fn serve(mode: OriginMode) -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind origin");
        let addr = listener.local_addr().expect("origin addr");
        let seen = Arc::new(Notify::new());
        let disconnected = Arc::new(Notify::new());
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen_task = Arc::clone(&seen);
        let disconnected_task = Arc::clone(&disconnected);
        let recorded_task = Arc::clone(&recorded);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let seen_task = Arc::clone(&seen_task);
                let disconnected_task = Arc::clone(&disconnected_task);
                let recorded_task = Arc::clone(&recorded_task);
                tokio::spawn(handle_origin(
                    socket,
                    mode,
                    seen_task,
                    disconnected_task,
                    recorded_task,
                ));
            }
        });
        Self {
            addr,
            seen,
            disconnected,
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
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let Ok(message) = read_http_request(&mut socket).await else {
        return;
    };
    recorded.lock().expect("records").push(RecordedRequest {
        headers: message.headers,
    });
    match mode {
        OriginMode::Sse => {
            seen.notify_one();
            let body = sse_body();
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
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
    }
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
