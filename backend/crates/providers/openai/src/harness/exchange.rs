//! 把 worker 交出的上游请求送到账号 HTTP 客户端
//!
//! 认证头只活在这一次发送里，连接中断或任务结束时随请求值一起释放
//! 发送前核对原始 URL 和唯一账号认证头，回环改写发生在核对之后
//! 身份、会话、传输和 hop-by-hop 不随选定账号的令牌离开；源码自己写出的会话头按本次调用保留

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

#[derive(Clone)]
pub(crate) struct SourceHeaders {
    pub(crate) pi: bool,
    pub(crate) session_id: String,
    pub(crate) thread_id: String,
    pub(crate) client_request_id: String,
}

pub(crate) async fn forward(
    io: ForwardIo<'_>,
    observed: &ObservedUpstreamRequest,
    dial: &DialPlan,
    http_id: u32,
    bearer: &str,
    source: &SourceHeaders,
) -> Result<(), HarnessError> {
    if let Err(error) = permit_upstream(observed, bearer) {
        io.remote_refusal.store(1, Ordering::SeqCst);
        let _ = send_head(io.stdin, http_id, 0, &[]).await;
        return Err(error);
    }
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
        if !outbound_header(name, value, bearer, source) {
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
    if send_head(
        io.stdin,
        http_id,
        status,
        &[(String::from("content-type"), content_type.to_owned())],
    )
    .await
    .is_err()
    {
        return Err(HarnessError::Upstream);
    }
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = io.cancel.cancelled() => {
                let _ = fail_body(io.stdin, http_id).await;
                return Err(HarnessError::Cancelled);
            }
            next = stream.next() => next,
        };
        let Some(next) = next else {
            break;
        };
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(_) => {
                let _ = fail_body(io.stdin, http_id).await;
                return Err(HarnessError::Upstream);
            }
        };
        if let Err(error) =
            write_chunk(io.stdin, http_id, io.window, io.cancel, &chunk, false).await
        {
            let _ = fail_body(io.stdin, http_id).await;
            return Err(error);
        }
    }
    write_chunk(io.stdin, http_id, io.window, io.cancel, &Bytes::new(), true).await
}

fn permit_upstream(observed: &ObservedUpstreamRequest, bearer: &str) -> Result<(), HarnessError> {
    let url = Url::parse(&observed.url).map_err(|_| HarnessError::RemoteDial)?;
    let path = url.path();
    if url.scheme() != "https"
        || url.host_str() != Some("api.openai.com")
        || !(path == "/v1" || path.starts_with("/v1/"))
    {
        return Err(HarnessError::RemoteDial);
    }
    let expected = format!("Bearer {bearer}");
    let mut authorizations = 0;
    for (name, value) in &observed.headers {
        if !is_auth_header(name) {
            continue;
        }
        if !name.eq_ignore_ascii_case("authorization") || value != &expected {
            return Err(HarnessError::RemoteDial);
        }
        authorizations += 1;
    }
    if authorizations != 1 {
        Err(HarnessError::RemoteDial)
    } else {
        Ok(())
    }
}

fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "cf-aig-authorization"
            | "x-api-key"
            | "api-key"
            | "proxy-authorization"
            | "x-openai-actor-authorization"
    )
}

fn outbound_header(name: &str, value: &str, bearer: &str, source: &SourceHeaders) -> bool {
    if hop(name) || downstream_owned(name) {
        return source_claims(name, value, bearer, source);
    }
    true
}

fn source_claims(name: &str, value: &str, bearer: &str, source: &SourceHeaders) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "authorization" => value == format!("Bearer {bearer}"),
        "content-type" | "accept" | "accept-encoding" => true,
        "user-agent" => source.pi && value.starts_with("pi/"),
        "session_id" => source.pi && value == source.session_id,
        "session-id" => !source.pi && value == source.session_id,
        "thread-id" => !source.pi && value == source.thread_id,
        "x-client-request-id" => value == source.client_request_id,
        _ => false,
    }
}

fn downstream_owned(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cf-aig-authorization"
            | "x-api-key"
            | "api-key"
            | "cookie"
            | "cookie2"
            | "chatgpt-account-id"
            | "chatgpt-project-id"
            | "openai-organization"
            | "openai-project"
            | "originator"
            | "version"
            | "user-agent"
            | "session_id"
            | "session-id"
            | "x-session-id"
            | "thread-id"
            | "x-thread-id"
            | "x-client-request-id"
            | "x-request-id"
            | "accept"
            | "accept-encoding"
            | "content-type"
            | "forwarded"
            | "via"
            | "x-real-ip"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
    ) || name.starts_with("x-codex-")
        || name.starts_with("x-openai-")
        || name.starts_with("x-oai-")
        || name.starts_with("openai-")
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

async fn fail_body(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    http_id: u32,
) -> Result<(), HarnessError> {
    protocol::write_frame(
        stdin,
        protocol::HTTP_BODY,
        http_id,
        &[protocol::BODY_FIN | protocol::BODY_FAILED],
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
        payload.push(if last { protocol::BODY_FIN } else { 0 });
        payload.extend_from_slice(&chunk[offset..end]);
        protocol::write_frame(stdin, protocol::HTTP_BODY, http_id, &payload).await?;
        offset = end;
    }
    Ok(())
}
