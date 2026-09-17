use std::{sync::Arc, time::Duration};

use gateway_core::account::OutboundProxy;
use provider_openai::transport::{CodexBackendClient, build_fresh_capture_http_client};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

use super::{codex_request, request_context, test_wire_profile};

#[tokio::test]
async fn capture_returns_on_header_drops_body_and_uses_fresh_proxy_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_url = format!("http://{}", listener.local_addr().expect("proxy address"));
    let proxy = OutboundProxy::parse(&proxy_url).expect("managed proxy URL");
    let body_dropped = Arc::new(Notify::new());
    let server = tokio::spawn({
        let body_dropped = Arc::clone(&body_dropped);
        async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept fresh connection");
                read_http_request(&mut stream).await;
                let value = "H".repeat(292);
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-codex-turn-state: {value}\r\ncontent-length: 1048576\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write capture header");
                let mut byte = [0_u8; 1];
                tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte))
                    .await
                    .expect("client drops unread response promptly")
                    .expect("read close");
                body_dropped.notify_one();
            }
        }
    });
    for index in 0..2 {
        let http = build_fresh_capture_http_client(&proxy).expect("fresh capture client");
        let backend = CodexBackendClient::new(
            http,
            "http://upstream.invalid/backend-api",
            test_wire_profile(),
        );
        let request = codex_request("gpt-test", "", Vec::new());
        let request_id = format!("capture-{index}");
        let value = backend
            .capture_turn_state_http_sse(
                &request,
                request_context(&request_id, Some("chatgpt-account")),
            )
            .await
            .expect("capture header")
            .expect("turn state header");
        assert_eq!(value, "H".repeat(292));
        tokio::time::timeout(Duration::from_secs(1), body_dropped.notified())
            .await
            .expect("response body was cancelled");
    }
    server.await.expect("proxy server");
}

#[tokio::test]
async fn capture_returns_on_first_sse_state_event_without_terminal_body() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("upstream address")
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        read_http_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .expect("write headers");
        let value = "E".repeat(292);
        let event = format!(
            "event: response.created\ndata: {{\"metadata\":{{\"x-codex-turn-state\":\"{value}\"}}}}\n\n"
        );
        stream
            .write_all(format!("{:X}\r\n{event}\r\n", event.len()).as_bytes())
            .await
            .expect("write state event");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .build()
        .expect("HTTP client");
    let backend = CodexBackendClient::new(client, base_url, test_wire_profile());
    let value = tokio::time::timeout(
        Duration::from_secs(1),
        backend.capture_turn_state_http_sse(
            &codex_request("gpt-test", "", Vec::new()),
            request_context("capture-event", Some("chatgpt-account")),
        ),
    )
    .await
    .expect("returns before terminal body")
    .expect("capture event")
    .expect("turn state event");
    assert_eq!(value, "E".repeat(292));
    server.abort();
}

async fn read_http_request(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).await.expect("read request");
        assert!(count > 0, "request closed before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or_default();
    while bytes.len() - header_end < content_length {
        let count = stream.read(&mut buffer).await.expect("read request body");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..count]);
    }
}
