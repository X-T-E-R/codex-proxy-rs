//! 把 worker 交出的上游请求送到账号 HTTP 客户端
//!
//! 认证头只活在这一次发送里，连接中断或任务结束时随请求值一起释放

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures::StreamExt;
use gateway_core::lifecycle::CancellationToken;
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use tokio::process::ChildStdin;

use super::{HarnessError, ObservedUpstreamRequest};
use crate::harness::protocol::{self, BodyWindow, HttpRequest};

const MANAGED_HEADER: &str = "x-cpr-harness-managed";

#[derive(Clone)]
pub(crate) struct DialPlan {
    pub(crate) allow_remote: bool,
    pub(crate) loopback: Option<SocketAddr>,
}

pub(crate) struct HeaderHold {
    live: Arc<AtomicUsize>,
}

impl HeaderHold {
    pub(crate) fn enter(live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::SeqCst);
        Self {
            live: Arc::clone(live),
        }
    }
}

impl Drop for HeaderHold {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn observe(request: &HttpRequest) -> Result<ObservedUpstreamRequest, HarnessError> {
    let body = STANDARD
        .decode(request.body_base64.as_bytes())
        .map_err(|_| HarnessError::Protocol)?;
    Ok(ObservedUpstreamRequest {
        method: request.method.clone(),
        url: request.url.clone(),
        headers: request.headers.clone(),
        body,
    })
}

pub(crate) struct ForwardIo<'a> {
    pub(crate) client: &'a Client,
    pub(crate) stdin: &'a tokio::sync::Mutex<ChildStdin>,
    pub(crate) window: &'a BodyWindow,
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) holders: &'a Arc<AtomicUsize>,
    pub(crate) remote_refusal: &'a AtomicUsize,
}

pub(crate) async fn forward(
    io: ForwardIo<'_>,
    observed: &ObservedUpstreamRequest,
    dial: &DialPlan,
    http_id: u32,
) -> Result<(), HarnessError> {
    let target = match dial_url(&observed.url, dial) {
        Ok(target) => target,
        Err(error) => {
            if matches!(error, HarnessError::RemoteDial) {
                io.remote_refusal.store(1, Ordering::SeqCst);
            }
            let _ = send_head(io.stdin, http_id, 0, &[]).await;
            return Err(error);
        }
    };
    let _hold = HeaderHold::enter(io.holders);
    let method =
        Method::from_bytes(observed.method.as_bytes()).map_err(|_| HarnessError::Upstream)?;
    let mut headers = HeaderMap::new();
    for (name, value) in &observed.headers {
        if hop(name) {
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| HarnessError::Upstream)?;
        let value =
            HeaderValue::from_bytes(value.as_bytes()).map_err(|_| HarnessError::Upstream)?;
        headers.append(name, value);
    }
    headers.insert(MANAGED_HEADER, HeaderValue::from_static("1"));
    let response = io
        .client
        .request(method, target)
        .headers(headers)
        .body(observed.body.clone())
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            let _ = send_head(io.stdin, http_id, 0, &[]).await;
            return Err(HarnessError::Upstream);
        }
    };
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/event-stream");
    send_head(
        io.stdin,
        http_id,
        status,
        &[(String::from("content-type"), content_type.to_owned())],
    )
    .await?;
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = io.cancel.cancelled() => return Err(HarnessError::Cancelled),
            next = stream.next() => next,
        };
        let Some(next) = next else {
            break;
        };
        let chunk = next.map_err(|_| HarnessError::Upstream)?;
        write_chunk(io.stdin, http_id, io.window, io.cancel, &chunk, false).await?;
    }
    write_chunk(io.stdin, http_id, io.window, io.cancel, &Bytes::new(), true).await
}

fn hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length"
            | "host"
            | "connection"
            | "transfer-encoding"
            | "keep-alive"
            | "upgrade"
            | "proxy-connection"
            | "te"
            | "trailer"
    )
}

fn dial_url(original: &str, dial: &DialPlan) -> Result<Url, HarnessError> {
    let parsed = Url::parse(original).map_err(|_| HarnessError::Upstream)?;
    let target = if let Some(addr) = dial.loopback {
        let query = parsed
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        Url::parse(&format!("http://{addr}{}{query}", parsed.path()))
            .map_err(|_| HarnessError::Upstream)?
    } else {
        parsed
    };
    if !dial.allow_remote && !is_loopback(&target) {
        return Err(HarnessError::RemoteDial);
    }
    Ok(target)
}

fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

async fn send_head(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    http_id: u32,
    status: u16,
    headers: &[(String, String)],
) -> Result<(), HarnessError> {
    protocol::write_json(
        stdin,
        protocol::HTTP_HEAD,
        http_id,
        &serde_json::json!({
            "status": status,
            "headers": headers,
            "error": if status == 0 { "managed dial failed" } else { "" },
        }),
    )
    .await
}

async fn write_chunk(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    http_id: u32,
    window: &BodyWindow,
    cancel: &CancellationToken,
    chunk: &Bytes,
    fin: bool,
) -> Result<(), HarnessError> {
    if chunk.is_empty() {
        let mut payload = vec![u8::from(fin)];
        if !fin {
            return Ok(());
        }
        payload.truncate(1);
        return protocol::write_frame(stdin, protocol::HTTP_BODY, http_id, &payload).await;
    }
    let mut offset = 0;
    while offset < chunk.len() {
        let room = window.acquire(1024, cancel).await?;
        let end = offset + room.min(chunk.len() - offset);
        let mut payload = Vec::with_capacity(1 + end - offset);
        let last = fin && end == chunk.len();
        payload.push(u8::from(last));
        payload.extend_from_slice(&chunk[offset..end]);
        protocol::write_frame(stdin, protocol::HTTP_BODY, http_id, &payload).await?;
        offset = end;
    }
    Ok(())
}
