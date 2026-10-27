//! 验证存储型 OpenCode 响应的 SSE 心跳、公开 ID 与发送前错误边界

use super::*;
#[cfg(debug_assertions)]
use provider_openai::credential::CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING;

#[cfg(debug_assertions)]
fn stored_request(input: Value) -> ProviderRequest {
    let payload = ProtocolPayload::json_object(
        "openai",
        Map::from_iter([
            ("model".to_owned(), json!("gpt-5.6-luna")),
            ("input".to_owned(), input),
            ("session_id".to_owned(), json!("session_framing")),
            ("thread_id".to_owned(), json!("thread_framing")),
            ("store".to_owned(), json!(true)),
        ]),
    )
    .expect("stored request");
    planned_request_with_presentation(
        "openai",
        Operation::Generate(GenerateRequest::from_protocol_payload(payload)),
        "gpt-5.6-luna",
        None,
    )
}

#[cfg(debug_assertions)]
fn sse_response(foreign_id: bool) -> String {
    let item = json!({"id":"fc_framing","type":"function_call","call_id":"call_framing","name":"lookup","arguments":"{}"});
    let mut frames = vec![
        json!({"type":"response.created","response":{"id":"resp_framing","status":"in_progress","output":[]}}),
    ];
    if foreign_id {
        frames.push(json!({"type":"response.output_item.added","response_id":"resp_foreign","output_index":0,"item":item}));
    } else {
        frames.push(json!({"type":"response.output_item.added","output_index":0,"item":item}));
        frames.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
        frames.push(json!({"type":"response.completed","response":{"id":"resp_framing","status":"completed","output":[item],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}));
    }
    format!(
        ": heartbeat\n\n{}data: [DONE]\n\n",
        frames
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
    )
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn stored_response_preserves_keepalive_and_rejects_foreign_id_without_sending_invalid_input()
{
    const CHILD: &str = "CPR_TEST_HARNESS_FRAMING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../");
        let bundle = tempfile::tempdir().expect("pinned source bundle");
        let sources = if let Some(path) = std::env::var_os("CPR_HARNESS_TEST_SOURCES") {
            std::path::PathBuf::from(path)
        } else {
            let output = std::process::Command::new("python")
                .arg(root.join("deploy/harness-sources/assemble.py"))
                .arg("assemble")
                .arg("--output")
                .arg(bundle.path().join("openai-harness"))
                .output()
                .expect("assemble pinned sources");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            bundle.path().join("openai-harness/sources")
        };
        let worker = std::env::var_os("CPR_OPENAI_HARNESS_WORKER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("harness-worker")
            });
        let result = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                std::thread::current().name().expect("test name"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CPR_OPENAI_HARNESS_WORKER_DIR", worker)
            .env("CPR_OPENCODE_SOURCE", sources.join("opencode"))
            .env("CPR_PI_SOURCE", sources.join("pi"))
            .output()
            .expect("isolated test process");
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }

    let origin = MockServer::start().await;
    let store = Arc::new(MemoryAccountStore::default());
    super::harness::seed_harness(&store, CODEX_AUTHENTICATION_KIND_CHATGPT_TOKEN_SHARING).await;
    let provider = provider(&store);
    let provider = Arc::new(
        Arc::try_unwrap(provider)
            .unwrap_or_else(|_| panic!("provider owner"))
            .with_harness_test_dial_loopback(*origin.address()),
    );
    let healthy = Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_response(false), "text/event-stream"),
        )
        .expect(1)
        .mount_as_scoped(&origin)
        .await;
    let mut response = provider
        .clone()
        .execute(
            stored_request(json!("hello")),
            diagnostic_context("req_framing_healthy", "acct_provider_contract")
                .with_transport(AttemptTransport::Fallback),
        )
        .await
        .expect("stored response");
    let mut wire = Vec::new();
    timeout(Duration::from_secs(30), async {
        while let Some(item) = response.next().await {
            let event = item.expect("canonical response");
            if let Some(frame) = event.wire_event().and_then(|wire| wire.raw_sse_frame()) {
                wire.extend_from_slice(frame);
            }
        }
    })
    .await
    .expect("stored response deadline");
    let wire = String::from_utf8(wire).expect("SSE wire");
    assert!(wire.starts_with(": heartbeat\n\n"));
    assert!(wire.contains("cpr_oc_v1_"));
    assert!(!wire.contains("resp_framing"));
    drop(healthy);

    let invalid_input = stored_request(
        json!([{"type":"message","role":"user","content":[{"type":"input_audio","data":"unsupported"}]}]),
    );
    let attempt = diagnostic_context("req_framing_invalid", "acct_provider_contract")
        .with_transport(AttemptTransport::Fallback);
    let error = match provider.clone().execute(invalid_input, attempt).await {
        Err(error) => error,
        Ok(mut stream) => timeout(Duration::from_secs(30), async {
            loop {
                match stream.next().await {
                    Some(Err(error)) => break error,
                    Some(Ok(_)) => continue,
                    None => panic!("unsupported content completed without error"),
                }
            }
        })
        .await
        .expect("invalid input deadline"),
    };
    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert_eq!(error.send_state(), UpstreamSendState::NotSent);
    assert_eq!(origin.received_requests().await.expect("requests").len(), 1);

    let mismatched = Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_response(true), "text/event-stream"),
        )
        .expect(1)
        .mount_as_scoped(&origin)
        .await;
    let mut response = provider
        .execute(
            stored_request(json!("hello again")),
            diagnostic_context("req_framing_foreign", "acct_provider_contract")
                .with_transport(AttemptTransport::Fallback),
        )
        .await
        .expect("foreign response begins");
    let error = timeout(Duration::from_secs(30), async {
        loop {
            match response.next().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => continue,
                None => panic!("foreign response ID completed without error"),
            }
        }
    })
    .await
    .expect("foreign response deadline");
    assert_eq!(error.kind(), ProviderErrorKind::Protocol);
    assert_eq!(origin.received_requests().await.expect("requests").len(), 2);
    drop(mismatched);
}
