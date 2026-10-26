//! 子进程长度前缀帧
//!
//! 响应体窗口由 worker 在消费字节后归还，宿主在窗口耗尽时停止向上游读取

use std::sync::Mutex;

use tokio::sync::Notify;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::ChildStdin};

use super::HarnessError;

pub(crate) const HELLO: u8 = 1;
pub(crate) const READY: u8 = 2;
pub(crate) const CALL: u8 = 3;
pub(crate) const HTTP_REQUEST: u8 = 4;
pub(crate) const HTTP_HEAD: u8 = 5;
pub(crate) const HTTP_BODY: u8 = 6;
pub(crate) const CREDIT: u8 = 7;
pub(crate) const EVENT: u8 = 8;
pub(crate) const RESULT: u8 = 9;
pub(crate) const CANCEL: u8 = 10;
pub(crate) const FAILURE: u8 = 11;
const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Incoming {
    pub(crate) kind: u8,
    pub(crate) id: u32,
    pub(crate) payload: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Hello {
    pub(crate) node: String,
    pub(crate) sources: Vec<HelloSource>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HelloSource {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body_base64: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Credit {
    pub(crate) bytes: usize,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkerFailure {
    pub(crate) cancelled: bool,
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkerEvent {
    pub(crate) kind: String,
    #[serde(rename = "type")]
    pub(crate) event_type: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) finish: Option<String>,
    pub(crate) response_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CallPayload<'a> {
    pub(crate) harness: &'a str,
    pub(crate) model: &'a str,
    pub(crate) base_url: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) bearer: &'a str,
    pub(crate) prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<Value>,
}

pub(crate) struct BodyWindow {
    inner: Mutex<BodyWindowInner>,
    notify: Notify,
}

struct BodyWindowInner {
    remaining: usize,
    capacity: usize,
    waits: usize,
}

impl BodyWindow {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(BodyWindowInner {
                remaining: capacity,
                capacity,
                waits: 0,
            }),
            notify: Notify::new(),
        }
    }

    pub(crate) fn credit(&self, bytes: usize) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.remaining = inner.remaining.saturating_add(bytes).min(inner.capacity);
        drop(inner);
        self.notify.notify_one();
    }

    pub(crate) async fn acquire(
        &self,
        want: usize,
        cancel: &gateway_core::lifecycle::CancellationToken,
    ) -> Result<usize, HarnessError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
                if inner.remaining > 0 {
                    let taken = want.max(1).min(inner.remaining);
                    inner.remaining -= taken;
                    return Ok(taken);
                }
                inner.waits = inner.waits.saturating_add(1);
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(HarnessError::Cancelled),
                _ = &mut notified => {}
            }
        }
    }

    pub(crate) fn waits(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .waits
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Incoming, HarnessError> {
    if bytes.len() < 5 || bytes.len() > MAX_FRAME {
        return Err(HarnessError::Protocol);
    }
    let id = u32::from_be_bytes(bytes[1..5].try_into().map_err(|_| HarnessError::Protocol)?);
    Ok(Incoming {
        kind: bytes[0],
        id,
        payload: bytes[5..].to_vec(),
    })
}

pub(crate) async fn write_frame(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    kind: u8,
    id: u32,
    payload: &[u8],
) -> Result<(), HarnessError> {
    if payload.len() + 5 > MAX_FRAME {
        return Err(HarnessError::Protocol);
    }
    let mut frame = Vec::with_capacity(4 + 5 + payload.len());
    let length = u32::try_from(5 + payload.len()).map_err(|_| HarnessError::Protocol)?;
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(payload);
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&frame)
        .await
        .map_err(|_| HarnessError::Protocol)
}

pub(crate) async fn write_json(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    kind: u8,
    id: u32,
    value: &impl Serialize,
) -> Result<(), HarnessError> {
    let payload = serde_json::to_vec(value).map_err(|_| HarnessError::Protocol)?;
    write_frame(stdin, kind, id, &payload).await
}

pub(crate) fn from_json<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, HarnessError> {
    serde_json::from_slice(payload).map_err(|_| HarnessError::Protocol)
}
