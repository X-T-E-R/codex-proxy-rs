//! Node worker 的启动、调用隔离、事件窗口和退出回收

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use gateway_core::lifecycle::CancellationToken;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
    time::{Instant, timeout},
};

use super::{
    HarnessError, HarnessEvent, HarnessWorkerConfig, LoadedSource, ResponseDone, ResponseItem,
    ResponsesCall, ResponsesHarness, detail_from,
};
use crate::harness::{
    exchange::{self, DialPlan},
    protocol::{self, BodyWindow, Hello, HttpRequest},
};
use crate::transport::account_scoped_id;
use crate::transport::client::build_account_http_client;

const READY_TIMEOUT: Duration = Duration::from_secs(90);
const ISOLATE_TIMEOUT: Duration = Duration::from_secs(2);
const FRAME_BOUND: usize = 64;

#[derive(Clone)]
struct Launch {
    node: PathBuf,
    worker_dir: PathBuf,
    opencode_checkout: PathBuf,
    pi_checkout: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SourceManifest {
    opencode_commit: String,
    pi_commit: String,
    files: Vec<PinnedFile>,
}

#[derive(Debug, Deserialize)]
struct PinnedFile {
    root: String,
    path: String,
    sha256: String,
}

struct State {
    launch: Launch,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Arc<AsyncMutex<ChildStdin>>>,
    frames: AsyncMutex<mpsc::Receiver<protocol::Incoming>>,
    stderr: Arc<Mutex<String>>,
    secrets: Mutex<Vec<String>>,
    holders: Arc<AtomicUsize>,
    remote_refusal: Arc<AtomicUsize>,
    in_flight: AtomicBool,
    http: Mutex<Vec<JoinHandle<Result<(), HarnessError>>>>,
    client: reqwest::Client,
    dial: DialPlan,
    pid: AtomicU32,
    node_version: Mutex<String>,
    sources: Mutex<Vec<LoadedSource>>,
    next_call: AtomicU32,
    event_waits: AtomicUsize,
    load_log: PathBuf,
}

pub struct HarnessSession {
    state: Arc<State>,
}

pub struct ResponseStream {
    inbox: mpsc::Receiver<Result<ResponseItem, HarnessError>>,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    call_id: u32,
    cancel: CancellationToken,
}

impl HarnessSession {
    pub async fn open(config: HarnessWorkerConfig) -> Result<Self, HarnessError> {
        let client = build_account_http_client("harness", config.proxy.as_ref())
            .map_err(|_| HarnessError::Upstream)?;
        let load_log = std::env::temp_dir().join(format!(
            "cpr-harness-loads-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&load_log, "").map_err(|_| HarnessError::Spawn)?;
        let launch = Launch {
            node: config.node.clone(),
            worker_dir: config.worker_dir.clone(),
            opencode_checkout: config.opencode_checkout.clone(),
            pi_checkout: config.pi_checkout.clone(),
        };
        let stderr = Arc::new(Mutex::new(String::new()));
        let spawned = spawn_worker(&launch, &load_log, stderr).await?;
        let session = Self {
            state: Arc::new(State {
                launch,
                child: Mutex::new(Some(spawned.child)),
                stdin: Mutex::new(spawned.stdin),
                frames: AsyncMutex::new(spawned.frames),
                stderr: spawned.stderr,
                secrets: Mutex::new(Vec::new()),
                holders: Arc::new(AtomicUsize::new(0)),
                remote_refusal: Arc::new(AtomicUsize::new(0)),
                in_flight: AtomicBool::new(false),
                http: Mutex::new(Vec::new()),
                client,
                dial: DialPlan {
                    allow_remote: config.allow_remote_dial,
                    loopback: config.dial_loopback,
                },
                pid: AtomicU32::new(spawned.pid),
                node_version: Mutex::new(String::new()),
                sources: Mutex::new(Vec::new()),
                next_call: AtomicU32::new(1),
                event_waits: AtomicUsize::new(0),
                load_log,
            }),
        };
        if let Err(error) = session.finish_startup().await {
            session.stop_child().await;
            return Err(error);
        }
        Ok(session)
    }

    pub async fn open_responses(
        &self,
        call: ResponsesCall,
        cancel: &CancellationToken,
    ) -> Result<ResponseStream, HarnessError> {
        if self.state.in_flight.swap(true, Ordering::SeqCst) {
            return Err(HarnessError::Protocol);
        }
        if call.model.is_empty()
            || trimmed(Some(call.installation_id.as_str())).is_none()
            || trimmed(Some(call.session_id.as_str())).is_none()
            || trimmed(Some(call.request_id.as_str())).is_none()
            || call.prompt.is_empty()
            || call.bearer.expose_secret().is_empty()
            || !(1..=1_048_576).contains(&call.body_window)
            || !(1..=1_024).contains(&call.event_window)
        {
            self.state.in_flight.store(false, Ordering::SeqCst);
            return Err(HarnessError::Protocol);
        }
        self.state.event_waits.store(0, Ordering::SeqCst);
        remember_secret(&self.state, call.bearer.expose_secret());
        let call_id = self.state.next_call.fetch_add(1, Ordering::SeqCst);
        let local = CancellationToken::new();
        let (sender, inbox) = mpsc::channel(call.event_window.saturating_add(4));
        let stdin = self
            .state
            .stdin
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let state = Arc::clone(&self.state);
        let user_cancel = cancel.clone();
        let drive_cancel = local.clone();
        tokio::spawn(async move {
            let result = drive(&state, &call, call_id, &user_cancel, &drive_cancel, &sender).await;
            finish_http(&state).await;
            scrub(&state);
            state.in_flight.store(false, Ordering::SeqCst);
            match result {
                Ok(done) => {
                    let _ = sender.send(Ok(ResponseItem::Done(done))).await;
                }
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                }
            }
        });
        Ok(ResponseStream {
            inbox,
            stdin,
            call_id,
            cancel: local,
        })
    }

    pub fn authorization_holders(&self) -> usize {
        self.state.holders.load(Ordering::SeqCst)
    }

    pub fn event_waits(&self) -> usize {
        self.state.event_waits.load(Ordering::SeqCst)
    }

    pub fn worker_pid(&self) -> u32 {
        self.state.pid.load(Ordering::SeqCst)
    }

    pub fn node_version(&self) -> String {
        self.state
            .node_version
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn loaded_sources(&self) -> Vec<LoadedSource> {
        self.state
            .sources
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn stderr_snapshot(&self) -> String {
        redact(
            &self.state,
            &self
                .state
                .stderr
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
    }

    pub async fn shutdown(self) -> Result<(), HarnessError> {
        finish_http(&self.state).await;
        self.stop_child().await;
        scrub(&self.state);
        Ok(())
    }

    async fn finish_startup(&self) -> Result<(), HarnessError> {
        let hello = match timeout(READY_TIMEOUT, next_frame(&self.state)).await {
            Ok(Some(frame)) if frame.kind == protocol::HELLO => protocol::from_json(&frame.payload)
                .map_err(|_| HarnessError::Startup {
                    detail: self.stderr_snapshot(),
                })?,
            Ok(Some(_)) | Ok(None) => {
                return Err(HarnessError::Startup {
                    detail: self.stderr_snapshot(),
                });
            }
            Err(_) => return Err(HarnessError::Timeout),
        };
        let (version, loaded) = verify_hello(&self.state, &hello)?;
        let stdin = self
            .state
            .stdin
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        protocol::write_json(&stdin, protocol::READY, 0, &json!({})).await?;
        *self
            .state
            .node_version
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = version;
        *self
            .state
            .sources
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = loaded;
        Ok(())
    }

    async fn stop_child(&self) {
        stop_child(&self.state).await;
    }
}

impl ResponseStream {
    pub async fn next(&mut self) -> Result<ResponseItem, HarnessError> {
        let item = self.inbox.recv().await.ok_or(HarnessError::Exited)?;
        let item = item?;
        if matches!(item, ResponseItem::Event(_)) {
            let _ = protocol::write_json(
                &self.stdin,
                protocol::EVENT_CREDIT,
                self.call_id,
                &json!({ "slots": 1 }),
            )
            .await;
        }
        Ok(item)
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Drop for HarnessSession {
    fn drop(&mut self) {
        if let Some(child) = self
            .state
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

struct Spawned {
    child: Child,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    frames: mpsc::Receiver<protocol::Incoming>,
    stderr: Arc<Mutex<String>>,
    pid: u32,
}

async fn spawn_worker(
    launch: &Launch,
    load_log: &Path,
    stderr_buf: Arc<Mutex<String>>,
) -> Result<Spawned, HarnessError> {
    let _ = std::fs::write(load_log, "");
    let mut command = Command::new(&launch.node);
    command
        .arg("--experimental-transform-types")
        .arg("--import")
        .arg("./register.mjs")
        .arg("./worker.mjs")
        .current_dir(&launch.worker_dir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in child_env(launch, load_log) {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|_| HarnessError::Spawn)?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take().ok_or(HarnessError::Spawn)?;
    let stdout = child.stdout.take().ok_or(HarnessError::Spawn)?;
    let stderr = child.stderr.take().ok_or(HarnessError::Spawn)?;
    tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_buf)));
    let (sender, frames) = mpsc::channel(FRAME_BOUND);
    tokio::spawn(drain_stdout(stdout, sender));
    Ok(Spawned {
        child,
        stdin: Arc::new(AsyncMutex::new(stdin)),
        frames,
        stderr: stderr_buf,
        pid,
    })
}

async fn drive(
    state: &State,
    call: &ResponsesCall,
    call_id: u32,
    user_cancel: &CancellationToken,
    local_cancel: &CancellationToken,
    outgoing: &mpsc::Sender<Result<ResponseItem, HarnessError>>,
) -> Result<ResponseDone, HarnessError> {
    state.remote_refusal.store(0, Ordering::SeqCst);
    let window = Arc::new(BodyWindow::new(call.body_window));
    let temperature = call.temperature.and_then(json_temperature);
    let service_tier = trimmed(call.service_tier.as_deref());
    let Some(installation_id) = trimmed(Some(call.installation_id.as_str())) else {
        return Err(HarnessError::Protocol);
    };
    let Some(raw_session) = trimmed(Some(call.session_id.as_str())) else {
        return Err(HarnessError::Protocol);
    };
    let raw_affinity = trimmed(call.identity.session_affinity_id.as_deref());
    let session_id = account_scoped_id(&installation_id, &raw_session);
    let session_affinity_id = raw_affinity
        .as_deref()
        .map(|raw| account_scoped_id(&installation_id, raw));
    let Some(request_id) = trimmed(Some(call.request_id.as_str())) else {
        return Err(HarnessError::Protocol);
    };
    let pi = matches!(call.harness, ResponsesHarness::Pi);
    let source_headers = exchange::SourceHeaders {
        pi,
        session_id: if pi {
            session_id.clone()
        } else {
            session_affinity_id
                .clone()
                .unwrap_or_else(|| session_id.clone())
        },
        thread_id: session_id.clone(),
        client_request_id: session_id.clone(),
    };
    let payload = protocol::CallPayload {
        harness: match call.harness {
            ResponsesHarness::OpenCode => "opencode",
            ResponsesHarness::Pi => "pi",
        },
        model: &call.model,
        base_url: &call.base_url,
        session_id: &session_id,
        request_id: &request_id,
        bearer: call.bearer.expose_secret(),
        prompt: &call.prompt,
        instructions: call.instructions.as_deref(),
        max_output_tokens: call.max_output_tokens,
        temperature,
        service_tier: service_tier.as_deref(),
        session_affinity_id: session_affinity_id.as_deref(),
        downstream_headers: &call.downstream_headers,
        event_window: call.event_window,
    };
    let stdin = stdin_handle(state);
    protocol::write_json(&stdin, protocol::CALL, call_id, &payload).await?;
    let mut request_seen = false;
    let deadline = Instant::now() + call.timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return isolate(state, call_id, HarnessError::Timeout).await;
        }
        let frame = tokio::select! {
            biased;
            _ = tokio::time::sleep(remaining) => {
                return isolate(state, call_id, HarnessError::Timeout).await;
            }
            _ = user_cancel.cancelled() => {
                return isolate(state, call_id, HarnessError::Cancelled).await;
            }
            _ = local_cancel.cancelled() => {
                return isolate(state, call_id, HarnessError::Cancelled).await;
            }
            frame = next_frame(state) => frame,
        };
        let Some(frame) = frame else {
            return Err(HarnessError::Exited);
        };
        match frame.kind {
            protocol::HTTP_REQUEST => {
                let parsed: HttpRequest = protocol::from_json(&frame.payload)?;
                if parsed.call_id != call_id || parsed.request_id != request_id {
                    let _ = reject_http(&stdin, frame.id).await;
                    continue;
                }
                let observed = exchange::observe(&parsed)?;
                spawn_forward(
                    state,
                    frame.id,
                    observed.clone(),
                    Arc::clone(&window),
                    local_cancel,
                    call.bearer.expose_secret(),
                    source_headers.clone(),
                );
                request_seen = true;
                deliver(
                    state,
                    call_id,
                    user_cancel,
                    local_cancel,
                    outgoing,
                    ResponseItem::Request(observed),
                )
                .await?;
            }
            protocol::CREDIT => {
                let credit: protocol::Credit = protocol::from_json(&frame.payload)?;
                if credit.call_id == call_id && credit.request_id == request_id {
                    window.credit(credit.bytes);
                }
            }
            protocol::EVENT => {
                if frame.id != call_id {
                    continue;
                }
                let payload: Value = protocol::from_json(&frame.payload)?;
                let event = HarnessEvent {
                    kind: payload
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    event_type: payload
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    id: payload.get("id").and_then(Value::as_str).map(str::to_owned),
                    name: payload
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    finish: payload
                        .get("finish")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    response_id: payload
                        .get("response_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    payload,
                };
                deliver(
                    state,
                    call_id,
                    user_cancel,
                    local_cancel,
                    outgoing,
                    ResponseItem::Event(event),
                )
                .await?;
            }
            protocol::EVENT_WAIT => {
                if frame.id != call_id {
                    continue;
                }
                let wait: protocol::EventWait = protocol::from_json(&frame.payload)?;
                if wait.request_id == request_id {
                    state.event_waits.fetch_add(1, Ordering::SeqCst);
                }
            }
            protocol::RESULT => {
                if frame.id != call_id {
                    continue;
                }
                let result: protocol::WorkerResult = protocol::from_json(&frame.payload)?;
                if result.request_id != request_id || !request_seen {
                    return Err(HarnessError::Protocol);
                }
                finish_http(state).await;
                return Ok(ResponseDone {
                    credit_waits: window.waits(),
                    event_waits: state.event_waits.load(Ordering::SeqCst),
                    abort_listeners: result.abort_listeners,
                });
            }
            protocol::FAILURE => {
                if frame.id != call_id {
                    continue;
                }
                finish_http(state).await;
                if state.remote_refusal.load(Ordering::SeqCst) == 1 {
                    return Err(HarnessError::RemoteDial);
                }
                let failure: protocol::WorkerFailure = protocol::from_json(&frame.payload)?;
                if failure.request_id != request_id {
                    return Err(HarnessError::Protocol);
                }
                if failure.cancelled || user_cancel.is_cancelled() || local_cancel.is_cancelled() {
                    return Err(HarnessError::Cancelled);
                }
                return Err(HarnessError::Worker {
                    detail: detail_from(&redact(state, &failure.message)),
                });
            }
            _ => {}
        }
    }
}

async fn deliver(
    state: &State,
    call_id: u32,
    user_cancel: &CancellationToken,
    local_cancel: &CancellationToken,
    outgoing: &mpsc::Sender<Result<ResponseItem, HarnessError>>,
    item: ResponseItem,
) -> Result<(), HarnessError> {
    if outgoing.try_send(Ok(item)).is_ok() {
        return Ok(());
    }
    let reason = if user_cancel.is_cancelled() || local_cancel.is_cancelled() {
        HarnessError::Cancelled
    } else {
        HarnessError::Protocol
    };
    isolate(state, call_id, reason).await?;
    Ok(())
}

async fn isolate(
    state: &State,
    call_id: u32,
    reason: HarnessError,
) -> Result<ResponseDone, HarnessError> {
    let stdin = stdin_handle(state);
    let _ = protocol::write_frame(&stdin, protocol::CANCEL, call_id, &[]).await;
    finish_http(state).await;
    let settled = timeout(ISOLATE_TIMEOUT, wait_terminal(state, call_id)).await;
    if !matches!(settled, Ok(true)) {
        let _ = restart_worker(state).await;
    }
    Err(reason)
}

async fn wait_terminal(state: &State, call_id: u32) -> bool {
    let stdin = stdin_handle(state);
    loop {
        let Some(frame) = next_frame(state).await else {
            return false;
        };
        match frame.kind {
            protocol::HTTP_REQUEST => {
                let _ = reject_http(&stdin, frame.id).await;
            }
            protocol::RESULT | protocol::FAILURE if frame.id == call_id => return true,
            _ => {}
        }
    }
}

async fn reject_http(stdin: &AsyncMutex<ChildStdin>, http_id: u32) -> Result<(), HarnessError> {
    protocol::write_json(
        stdin,
        protocol::HTTP_HEAD,
        http_id,
        &json!({ "status": 0, "headers": [], "error": "managed dial failed" }),
    )
    .await
}

fn spawn_forward(
    state: &State,
    http_id: u32,
    observed: super::ObservedUpstreamRequest,
    window: Arc<BodyWindow>,
    cancel: &CancellationToken,
    bearer: &str,
    source_headers: exchange::SourceHeaders,
) {
    let client = state.client.clone();
    let dial = state.dial.clone();
    let stdin = stdin_handle(state);
    let holders = Arc::clone(&state.holders);
    let remote_refusal = Arc::clone(&state.remote_refusal);
    let cancel = cancel.clone();
    let bearer = bearer.to_owned();
    let handle = tokio::spawn(async move {
        exchange::forward(
            exchange::ForwardIo {
                client: &client,
                stdin: &stdin,
                window: &window,
                cancel: &cancel,
                holders: &holders,
                remote_refusal: &remote_refusal,
            },
            &observed,
            &dial,
            http_id,
            &bearer,
            &source_headers,
        )
        .await
    });
    state
        .http
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(handle);
}

async fn finish_http(state: &State) {
    let handles =
        std::mem::take(&mut *state.http.lock().unwrap_or_else(|error| error.into_inner()));
    for handle in handles {
        handle.abort();
        let _ = handle.await;
    }
}

async fn restart_worker(state: &State) -> Result<(), HarnessError> {
    finish_http(state).await;
    stop_child(state).await;
    state
        .stderr
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    let spawned = spawn_worker(&state.launch, &state.load_log, Arc::clone(&state.stderr)).await?;
    *state
        .child
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(spawned.child);
    *state
        .stdin
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = spawned.stdin;
    *state.frames.lock().await = spawned.frames;
    state.pid.store(spawned.pid, Ordering::SeqCst);
    let hello = match timeout(READY_TIMEOUT, next_frame(state)).await {
        Ok(Some(frame)) if frame.kind == protocol::HELLO => {
            protocol::from_json::<Hello>(&frame.payload).map_err(|_| HarnessError::Source)?
        }
        _ => return Err(HarnessError::Source),
    };
    let (version, loaded) = verify_hello(state, &hello)?;
    let stdin = stdin_handle(state);
    protocol::write_json(&stdin, protocol::READY, 0, &json!({})).await?;
    *state
        .node_version
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = version;
    *state
        .sources
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = loaded;
    Ok(())
}

async fn next_frame(state: &State) -> Option<protocol::Incoming> {
    state.frames.lock().await.recv().await
}

fn stdin_handle(state: &State) -> Arc<AsyncMutex<ChildStdin>> {
    state
        .stdin
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn remember_secret(state: &State, secret: &str) {
    if secret.is_empty() {
        return;
    }
    state
        .secrets
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(secret.to_owned());
}

fn scrub(state: &State) {
    let secrets = {
        let mut secrets = state
            .secrets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *secrets)
    };
    let mut stderr = state
        .stderr
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for secret in secrets {
        if !secret.is_empty() {
            *stderr = stderr.replace(&secret, "[redacted]");
        }
    }
}

fn redact(state: &State, text: &str) -> String {
    let mut redacted = text.to_owned();
    for secret in state
        .secrets
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
    {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[redacted]");
        }
    }
    redacted
}

async fn stop_child(state: &State) {
    let child = state
        .child
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(mut child) = child {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

async fn drain_stdout(mut stdout: ChildStdout, sender: mpsc::Sender<protocol::Incoming>) {
    while let Ok(Some(frame)) = read_incoming(&mut stdout).await {
        if sender.send(frame).await.is_err() {
            break;
        }
    }
}

async fn read_incoming(stdout: &mut ChildStdout) -> Result<Option<protocol::Incoming>, ()> {
    let mut length_buf = [0; 4];
    if stdout.read_exact(&mut length_buf).await.is_err() {
        return Ok(None);
    }
    let length = u32::from_be_bytes(length_buf) as usize;
    if !(5..=8 * 1024 * 1024).contains(&length) {
        return Err(());
    }
    let mut body = vec![0; length];
    if stdout.read_exact(&mut body).await.is_err() {
        return Ok(None);
    }
    protocol::decode(&body).map(Some).map_err(|_| ())
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr, output: Arc<Mutex<String>>) {
    let mut buffer = [0; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                let mut stored = output.lock().unwrap_or_else(|error| error.into_inner());
                if stored.len() < 64 * 1024 {
                    stored.push_str(&String::from_utf8_lossy(&buffer[..size]));
                    if stored.len() > 64 * 1024 {
                        stored.truncate(64 * 1024);
                    }
                }
            }
        }
    }
}

fn child_env(launch: &Launch, load_log: &Path) -> Vec<(String, String)> {
    let mut values = Vec::new();
    for key in [
        "SYSTEMROOT",
        "WINDIR",
        "SYSTEMDRIVE",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "PATH",
        "USERPROFILE",
    ] {
        if let Ok(value) = std::env::var(key) {
            values.push((key.to_owned(), value));
        }
    }
    values.push((
        "CPR_HARNESS_OPENCODE_ROOT".to_owned(),
        launch.opencode_checkout.to_string_lossy().into_owned(),
    ));
    values.push((
        "CPR_HARNESS_PI_ROOT".to_owned(),
        launch.pi_checkout.to_string_lossy().into_owned(),
    ));
    values.push((
        "CPR_HARNESS_WORKER_DIR".to_owned(),
        launch.worker_dir.to_string_lossy().into_owned(),
    ));
    values.push((
        "CPR_HARNESS_LOAD_LOG".to_owned(),
        load_log.to_string_lossy().into_owned(),
    ));
    values
}

fn pinned_manifest() -> Result<SourceManifest, HarnessError> {
    serde_json::from_str(include_str!("../../harness-worker/source-manifest.json"))
        .map_err(|_| HarnessError::Source)
}

fn verify_hello(state: &State, hello: &Hello) -> Result<(String, Vec<LoadedSource>), HarnessError> {
    let version = parse_version(&hello.node).ok_or_else(|| HarnessError::NodeVersion {
        version: hello.node.clone(),
    })?;
    if version < super::MINIMUM_NODE_VERSION {
        return Err(HarnessError::NodeVersion {
            version: hello.node.clone(),
        });
    }
    let manifest = pinned_manifest()?;
    if manifest.opencode_commit != super::OPENCODE_SOURCE_COMMIT
        || manifest.pi_commit != super::PI_SOURCE_COMMIT
    {
        return Err(HarnessError::Source);
    }
    if hello.sources.is_empty() {
        return Err(HarnessError::Source);
    }
    let mut sources = Vec::with_capacity(hello.sources.len());
    for reported in &hello.sources {
        let (root, relative) = reported.id.split_once(':').ok_or(HarnessError::Source)?;
        let checkout = match root {
            "opencode" => &state.launch.opencode_checkout,
            "pi" => &state.launch.pi_checkout,
            _ => return Err(HarnessError::Source),
        };
        let pinned = manifest
            .files
            .iter()
            .find(|file| file.root == root && file.path == relative)
            .ok_or(HarnessError::Source)?;
        let disk = checkout.join(relative);
        let actual = file_sha256(&disk)?;
        if !same_file(&disk, Path::new(&reported.path))
            || !eq_hex(&actual, &reported.sha256)
            || !eq_hex(&actual, &pinned.sha256)
        {
            return Err(HarnessError::Source);
        }
        sources.push(LoadedSource {
            id: reported.id.clone(),
            path: disk,
            sha256: actual,
        });
    }
    for required in [
        "opencode:packages/ai/src/protocols/openai-responses.ts",
        "opencode:packages/ai/src/llm.ts",
        "pi:packages/ai/src/api/openai-responses.ts",
        "pi:packages/ai/src/api/openai-responses-shared.ts",
    ] {
        if !sources.iter().any(|source| source.id == required) {
            return Err(HarnessError::Source);
        }
    }
    Ok((hello.node.clone(), sources))
}

fn file_sha256(path: &Path) -> Result<String, HarnessError> {
    let bytes = std::fs::read(path).map_err(|_| HarnessError::Source)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn eq_hex(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn json_temperature(value: f64) -> Option<Value> {
    if !value.is_finite() {
        return None;
    }
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        return Some(Value::Number((value as i64).into()));
    }
    serde_json::Number::from_f64(value).map(Value::Number)
}
