//! Node worker 的启动、帧循环、取消和退出回收

use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use gateway_core::lifecycle::CancellationToken;
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdout, Command},
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};

use super::{
    HarnessError, HarnessEvent, HarnessWorkerConfig, LoadedSource, ParsedResponses, ResponsesCall,
    ResponsesHarness, detail_from,
};
use crate::harness::{
    exchange::{self, DialPlan},
    protocol::{self, BodyWindow, Hello, HttpRequest},
};
use crate::transport::client::build_account_http_client;

const READY_TIMEOUT: Duration = Duration::from_secs(90);

pub struct HarnessSession {
    child: Mutex<Option<Child>>,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    frames: tokio::sync::Mutex<mpsc::UnboundedReceiver<protocol::Incoming>>,
    stderr: Arc<Mutex<String>>,
    secrets: Mutex<Vec<String>>,
    holders: Arc<AtomicUsize>,
    remote_refusal: Arc<AtomicUsize>,
    in_flight: AtomicBool,
    http: Mutex<Option<JoinHandle<Result<(), HarnessError>>>>,
    client: reqwest::Client,
    dial: DialPlan,
    pid: u32,
    node_version: Mutex<String>,
    sources: Mutex<Vec<LoadedSource>>,
}

impl HarnessSession {
    pub async fn open(config: HarnessWorkerConfig) -> Result<Self, HarnessError> {
        let client = build_account_http_client("harness", config.proxy.as_ref())
            .map_err(|_| HarnessError::Upstream)?;
        let mut command = Command::new(&config.node);
        command
            .arg("--experimental-transform-types")
            .arg("--import")
            .arg("./register.mjs")
            .arg("./worker.mjs")
            .current_dir(&config.worker_dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in child_env(&config) {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|_| HarnessError::Spawn)?;
        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take().ok_or(HarnessError::Spawn)?;
        let stdout = child.stdout.take().ok_or(HarnessError::Spawn)?;
        let stderr = child.stderr.take().ok_or(HarnessError::Spawn)?;
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_buf)));
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(drain_stdout(stdout, sender));
        let session = Self {
            child: Mutex::new(Some(child)),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            frames: tokio::sync::Mutex::new(receiver),
            stderr: stderr_buf,
            secrets: Mutex::new(Vec::new()),
            holders: Arc::new(AtomicUsize::new(0)),
            remote_refusal: Arc::new(AtomicUsize::new(0)),
            in_flight: AtomicBool::new(false),
            http: Mutex::new(None),
            client,
            dial: DialPlan {
                allow_remote: config.allow_remote_dial,
                loopback: config.dial_loopback,
            },
            pid,
            node_version: Mutex::new(String::new()),
            sources: Mutex::new(Vec::new()),
        };
        let hello = session.read_hello().await?;
        let (version, loaded) = verify_hello(&config, &hello)?;
        protocol::write_json(&session.stdin, protocol::READY, 0, &serde_json::json!({})).await?;
        *session
            .node_version
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = version;
        *session
            .sources
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = loaded;
        Ok(session)
    }

    pub async fn responses(
        &self,
        call: ResponsesCall,
        cancel: &CancellationToken,
    ) -> Result<ParsedResponses, HarnessError> {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return Err(HarnessError::Protocol);
        }
        let _busy = InFlightGuard(&self.in_flight);
        self.remember_secret(call.bearer.expose_secret());
        let outcome = timeout(call.timeout, self.drive(&call, cancel)).await;
        self.finish_http().await;
        self.scrub();
        match outcome {
            Ok(result) => result,
            Err(_) => Err(HarnessError::Timeout),
        }
    }

    pub fn authorization_holders(&self) -> usize {
        self.holders.load(Ordering::SeqCst)
    }

    pub fn worker_pid(&self) -> u32 {
        self.pid
    }

    pub fn node_version(&self) -> String {
        self.node_version
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn loaded_sources(&self) -> Vec<LoadedSource> {
        self.sources
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn stderr_snapshot(&self) -> String {
        self.redact(
            &self
                .stderr
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
    }

    pub async fn shutdown(self) -> Result<(), HarnessError> {
        self.finish_http().await;
        self.stop_child().await;
        self.scrub();
        Ok(())
    }

    async fn drive(
        &self,
        call: &ResponsesCall,
        cancel: &CancellationToken,
    ) -> Result<ParsedResponses, HarnessError> {
        if call.model.is_empty()
            || call.session_id.is_empty()
            || call.prompt.is_empty()
            || call.bearer.expose_secret().is_empty()
            || !(1..=1_048_576).contains(&call.body_window)
        {
            return Err(HarnessError::Protocol);
        }
        self.remote_refusal.store(0, Ordering::SeqCst);
        let window = Arc::new(BodyWindow::new(call.body_window));
        let temperature = call.temperature.and_then(json_temperature);
        let service_tier = trimmed(call.service_tier.as_deref());
        let session_affinity_id = trimmed(call.identity.session_affinity_id.as_deref());
        let thread_id = trimmed(call.identity.thread_id.as_deref());
        let client_request_id = trimmed(call.identity.client_request_id.as_deref());
        let payload = protocol::CallPayload {
            harness: match call.harness {
                ResponsesHarness::OpenCode => "opencode",
                ResponsesHarness::Pi => "pi",
            },
            model: &call.model,
            base_url: &call.base_url,
            session_id: &call.session_id,
            bearer: call.bearer.expose_secret(),
            prompt: &call.prompt,
            instructions: call.instructions.as_deref(),
            max_output_tokens: call.max_output_tokens,
            temperature,
            service_tier: service_tier.as_deref(),
            session_affinity_id: session_affinity_id.as_deref(),
            thread_id: thread_id.as_deref(),
            client_request_id: client_request_id.as_deref(),
        };
        protocol::write_json(&self.stdin, protocol::CALL, 1, &payload).await?;
        let mut request = None;
        let mut events = Vec::new();
        loop {
            let frame = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = protocol::write_frame(&self.stdin, protocol::CANCEL, 1, &[]).await;
                    return Err(HarnessError::Cancelled);
                }
                frame = self.next_frame() => frame,
            };
            let Some(frame) = frame else {
                return Err(HarnessError::Exited);
            };
            match frame.kind {
                protocol::HTTP_REQUEST => {
                    let parsed: HttpRequest = protocol::from_json(&frame.payload)?;
                    let observed = exchange::observe(&parsed)?;
                    self.spawn_forward(frame.id, observed.clone(), Arc::clone(&window), cancel);
                    request = Some(observed);
                }
                protocol::CREDIT => {
                    let credit: protocol::Credit = protocol::from_json(&frame.payload)?;
                    window.credit(credit.bytes);
                }
                protocol::EVENT => {
                    let event: protocol::WorkerEvent = protocol::from_json(&frame.payload)?;
                    events.push(HarnessEvent {
                        kind: event.kind,
                        event_type: event.event_type,
                        id: event.id,
                        name: event.name,
                        finish: event.finish,
                        response_id: event.response_id,
                    });
                }
                protocol::RESULT => {
                    self.finish_http().await;
                    return Ok(ParsedResponses {
                        request: request.ok_or(HarnessError::Protocol)?,
                        events,
                        credit_waits: window.waits(),
                    });
                }
                protocol::FAILURE => {
                    self.finish_http().await;
                    if self.remote_refusal.load(Ordering::SeqCst) == 1 {
                        return Err(HarnessError::RemoteDial);
                    }
                    let failure: protocol::WorkerFailure = protocol::from_json(&frame.payload)?;
                    if failure.cancelled || cancel.is_cancelled() {
                        return Err(HarnessError::Cancelled);
                    }
                    return Err(HarnessError::Worker {
                        detail: detail_from(&self.redact(&failure.message)),
                    });
                }
                _ => {}
            }
        }
    }

    fn spawn_forward(
        &self,
        http_id: u32,
        observed: super::ObservedUpstreamRequest,
        window: Arc<BodyWindow>,
        cancel: &CancellationToken,
    ) {
        let client = self.client.clone();
        let dial = self.dial.clone();
        let stdin = Arc::clone(&self.stdin);
        let holders = Arc::clone(&self.holders);
        let remote_refusal = Arc::clone(&self.remote_refusal);
        let cancel = cancel.clone();
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
            )
            .await
        });
        *self.http.lock().unwrap_or_else(|error| error.into_inner()) = Some(handle);
    }

    async fn finish_http(&self) {
        let handle = self
            .http
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    async fn read_hello(&self) -> Result<Hello, HarnessError> {
        let frame = timeout(READY_TIMEOUT, self.next_frame())
            .await
            .map_err(|_| HarnessError::Timeout)?;
        let Some(frame) = frame else {
            return Err(HarnessError::Startup {
                detail: self.stderr_snapshot(),
            });
        };
        if frame.kind != protocol::HELLO {
            return Err(HarnessError::Startup {
                detail: self.stderr_snapshot(),
            });
        }
        protocol::from_json(&frame.payload).map_err(|_| HarnessError::Startup {
            detail: self.stderr_snapshot(),
        })
    }

    async fn next_frame(&self) -> Option<protocol::Incoming> {
        self.frames.lock().await.recv().await
    }

    fn remember_secret(&self, secret: &str) {
        if secret.is_empty() {
            return;
        }
        self.secrets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(secret.to_owned());
    }

    fn scrub(&self) {
        let secrets = {
            let mut secrets = self
                .secrets
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *secrets)
        };
        let mut stderr = self
            .stderr
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for secret in secrets {
            if !secret.is_empty() {
                *stderr = stderr.replace(&secret, "[redacted]");
            }
        }
    }

    fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for secret in self
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

    async fn stop_child(&self) {
        let child = self
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl Drop for HarnessSession {
    fn drop(&mut self) {
        if let Some(child) = self
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

async fn drain_stdout(mut stdout: ChildStdout, sender: mpsc::UnboundedSender<protocol::Incoming>) {
    while let Ok(Some(frame)) = read_incoming(&mut stdout).await {
        if sender.send(frame).is_err() {
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

fn child_env(config: &HarnessWorkerConfig) -> Vec<(String, String)> {
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
        config.opencode_checkout.to_string_lossy().into_owned(),
    ));
    values.push((
        "CPR_HARNESS_PI_ROOT".to_owned(),
        config.pi_checkout.to_string_lossy().into_owned(),
    ));
    values.push((
        "CPR_HARNESS_WORKER_DIR".to_owned(),
        config.worker_dir.to_string_lossy().into_owned(),
    ));
    values
}

fn verify_hello(
    config: &HarnessWorkerConfig,
    hello: &Hello,
) -> Result<(String, Vec<LoadedSource>), HarnessError> {
    let version = parse_version(&hello.node).ok_or_else(|| HarnessError::NodeVersion {
        version: hello.node.clone(),
    })?;
    if version < super::MINIMUM_NODE_VERSION {
        return Err(HarnessError::NodeVersion {
            version: hello.node.clone(),
        });
    }
    let expected = [
        (
            "opencode-openai-responses",
            config
                .opencode_checkout
                .join("packages/ai/src/protocols/openai-responses.ts"),
        ),
        (
            "pi-openai-responses",
            config
                .pi_checkout
                .join("packages/ai/src/api/openai-responses.ts"),
        ),
        (
            "pi-openai-responses-shared",
            config
                .pi_checkout
                .join("packages/ai/src/api/openai-responses-shared.ts"),
        ),
    ];
    let mut sources = Vec::with_capacity(expected.len());
    for (id, path) in expected {
        let reported = hello
            .sources
            .iter()
            .find(|source| source.id == id)
            .ok_or(HarnessError::Source)?;
        let actual = file_sha256(&path)?;
        let same_path = same_file(&path, Path::new(&reported.path));
        if !same_path || !eq_hex(&actual, &reported.sha256) {
            return Err(HarnessError::Source);
        }
        sources.push(LoadedSource {
            id: id.to_owned(),
            path,
            sha256: actual,
        });
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

struct InFlightGuard<'a>(&'a AtomicBool);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn json_temperature(value: f64) -> Option<serde_json::Value> {
    if !value.is_finite() {
        return None;
    }
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        return Some(serde_json::Value::Number((value as i64).into()));
    }
    serde_json::Number::from_f64(value).map(serde_json::Value::Number)
}
