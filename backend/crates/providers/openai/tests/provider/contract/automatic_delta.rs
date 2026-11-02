//! 从真实 Provider 入口验证上游自动增量、完成输出保真与状态隔离

use super::*;
use tokio::{sync::mpsc, task::JoinSet};

struct Upstream {
    base: String,
    frames: mpsc::UnboundedReceiver<(usize, Value)>,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
enum Reply {
    Output(Vec<Value>),
    Events(Vec<Value>),
    Close,
    Hold,
}

impl Upstream {
    async fn start(outputs: Vec<Vec<Value>>, reject_delta: bool) -> Self {
        Self::script(
            outputs.into_iter().map(Reply::Output).collect(),
            reject_delta,
        )
        .await
    }

    async fn script(outputs: Vec<Reply>, reject_delta: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, frames) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let stop = cancellation.clone();
        let outputs = Arc::new(outputs);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut connection = 0;
            loop {
                let stream = tokio::select! {
                    () = stop.cancelled() => break,
                    accepted = listener.accept() => accepted.unwrap().0,
                };
                let tx = tx.clone();
                let outputs = Arc::clone(&outputs);
                let calls = Arc::clone(&calls);
                connection += 1;
                connections.spawn(async move {
                    let mut stream = stream;
                    let mut prefix = [0_u8; 4];
                    let length = stream.peek(&mut prefix).await.unwrap();
                    if &prefix[..length] == b"POST" {
                        let raw = crate::transport::read_http_request_with_body(&mut stream).await;
                        let head_end = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4;
                        let head = String::from_utf8_lossy(&raw[..head_end]);
                        let decoded = if head.to_ascii_lowercase().contains("content-encoding: zstd") {
                            zstd::stream::decode_all(&raw[head_end..]).unwrap()
                        } else { raw[head_end..].to_vec() };
                        let body = serde_json::from_slice(&decoded).unwrap();
                        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tx.send((connection, body)).unwrap();
                        crate::transport::write_completed_sse_response(&mut stream).await;
                        return;
                    }
                    let mut websocket = accept_codex_test_websocket(stream).await;
                    while let Some(message) = websocket.next().await {
                        let text = match message.unwrap() {
                            Message::Text(text) => text,
                            Message::Ping(bytes) => { websocket.send(Message::Pong(bytes)).await.unwrap(); continue; }
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        let body: Value = serde_json::from_str(&text).unwrap();
                        let call = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tx.send((connection, body.clone())).unwrap();
                        if reject_delta && body.get("previous_response_id").is_some() {
                            websocket.send(Message::Text(json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}).to_string().into())).await.unwrap();
                            break;
                        }
                        let reply = outputs.get(call).cloned().unwrap_or_else(|| Reply::Output(vec![]));
                        let id = format!("resp_auto_{call}");
                        websocket.send(Message::Text(json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}).to_string().into())).await.unwrap();
                        match reply {
                            Reply::Output(output) => {
                                for (index, item) in output.iter().enumerate() {
                                    websocket.send(Message::Text(json!({"type":"response.output_item.done","response_id":id,"output_index":index,"item":item}).to_string().into())).await.unwrap();
                                }
                                // 终态只含身份和 usage，真实输出来自原生 done，不能由文本 delta 补造
                                websocket.send(Message::Text(completed(call, vec![]).to_string().into())).await.unwrap();
                            }
                            Reply::Events(events) => { for event in events { websocket.send(Message::Text(event.to_string().into())).await.unwrap(); } }
                            Reply::Close => { websocket.close(None).await.unwrap(); break; }
                            Reply::Hold => {
                                websocket.send(Message::Text(json!({"type":"response.output_text.delta","response_id":id,"output_index":0,"content_index":0,"delta":"partial"}).to_string().into())).await.unwrap();
                                std::future::pending::<()>().await;
                            }
                        }
                    }
                });
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            base,
            frames,
            cancellation,
            task,
        }
    }

    async fn frame(&mut self) -> (usize, Value) {
        timeout(Duration::from_secs(5), self.frames.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn stop(self) {
        self.cancellation.cancel();
        self.task.await.unwrap();
    }
}

fn operation(input: Vec<Value>, session: &str, options: Map<String, Value>) -> Operation {
    let mut body =
        json!({"model":"gpt-5.4","session_id":session,"input":input,"store":false,"stream":true})
            .as_object()
            .unwrap()
            .clone();
    body.extend(options);
    Operation::Generate(GenerateRequest::from_protocol_payload(ProtocolPayload::json_object("openai", body).unwrap())
        .with_provider_session_state(ProviderSessionState::new("openai", json!({"account_id":"acct_provider_contract","conversation_id":session,"continuation_scope":"connection_local"}).as_object().unwrap().clone()).unwrap()))
}

fn keyed_context(id: &str, key: &str) -> AttemptContext {
    AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new(id).unwrap(),
            ClientApiKeyId::new(key).unwrap(),
        )
        .with_request_location(Some(global_request_location())),
        NonZeroU32::MIN,
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None)
            .with_account_scope(contract_account_scope()),
        None,
        CancellationToken::new(),
    )
}

async fn run(
    provider: &Arc<CodexProvider>,
    input: Vec<Value>,
    session: &str,
    options: Map<String, Value>,
    key: &str,
    id: &str,
) -> String {
    run_operation(
        provider,
        operation(input, session, options),
        keyed_context(id, key),
    )
    .await
}

async fn run_operation(
    provider: &Arc<CodexProvider>,
    operation: Operation,
    context: AttemptContext,
) -> String {
    let mut stream = Arc::clone(provider)
        .execute(planned_request("openai", operation), context)
        .await
        .unwrap();
    let mut completed = false;
    let mut transport = None;
    while let Some(event) = stream.next().await {
        let event = event.expect("Provider response");
        if let Some(observation) = event.response_observation() {
            transport = Some(observation.transport().as_str().to_owned());
        }
        completed |= event
            .wire_event()
            .is_some_and(|wire| wire.event_type() == Some("response.completed"));
    }
    assert!(completed);
    transport.expect("actual transport observation")
}

pub(super) async fn assert_oauth_frame_threshold() {
    const THRESHOLD: usize = 15 * 1024 * 1024;
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let seed_text = "中文\n\"\\";
    let input = |text: String| {
        vec![json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})]
    };
    run(
        &provider,
        input(seed_text.to_owned()),
        "frame-boundary",
        Map::new(),
        "key_auto",
        "req_frame_seed",
    )
    .await;
    let (_, seed) = upstream.frame().await;
    let baseline = serde_json::to_vec(&seed).unwrap().len();
    for (bytes, expected) in [(THRESHOLD - 1, "websocket"), (THRESHOLD, "http_sse")] {
        let input = input(format!("{seed_text}{}", "x".repeat(bytes - baseline)));
        let actual = run(
            &provider,
            input.clone(),
            "frame-boundary",
            Map::new(),
            "key_auto",
            &format!("req_frame_{bytes}"),
        )
        .await;
        assert_eq!(actual, expected);
        let (_, sent) = upstream.frame().await;
        assert_eq!(sent["input"], json!(input));
        assert!(sent.get("previous_response_id").is_none());
        if expected == "websocket" {
            assert_eq!(serde_json::to_vec(&sent).unwrap().len(), bytes);
        }
    }
    upstream.stop().await;
}

#[tokio::test]
async fn full_text_tool_and_opaque_reasoning_histories_become_native_deltas() {
    let user =
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]});
    let text = json!({"type":"message","role":"assistant","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"answer","annotations":[]}],"extension":{"signature":"opaque"}});
    let tool = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"inspect","arguments":"{}","status":"completed"});
    let reasoning: Value = serde_json::from_str(r#"{"type":"reasoning","id":"rs_1","status":"completed","content":[{"type":"reasoning_text","text":"reason"}],"encrypted_content":"opaque-ciphertext","unknown":9007199254740993}"#).unwrap();
    let outputs = vec![
        vec![text.clone()],
        vec![tool.clone()],
        vec![reasoning.clone()],
        vec![],
    ];
    let mut upstream = Upstream::start(outputs, false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut history = vec![user];
    for (turn, output) in [text, tool, reasoning].into_iter().enumerate() {
        let metadata = json!({"client_metadata":{"turn_id":format!("turn_{turn}")},"unknown_business":{"integer":9007199254740993_u64}}).as_object().unwrap().clone();
        run(
            &provider,
            history.clone(),
            "auto-chain",
            metadata,
            "key_auto",
            &format!("req_auto_{turn}"),
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, 1);
        if turn == 0 {
            assert!(sent.get("previous_response_id").is_none());
            assert_eq!(sent["input"], json!(history));
        } else {
            assert_eq!(
                sent["previous_response_id"],
                format!("resp_auto_{}", turn - 1)
            );
            assert_eq!(sent["input"], json!([history.last().unwrap()]));
        }
        assert_eq!(
            sent["unknown_business"]["integer"].to_string(),
            "9007199254740993"
        );
        history.push(output);
        history.push(if turn == 1 { json!({"type":"function_call_output","call_id":"call_1","output":"tool result","unknown":"retain"}) }
            else { json!({"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}) });
    }
    run(&provider, history.clone(), "auto-chain", json!({"client_metadata":{"turn_id":"last"},"unknown_business":{"integer":9007199254740993_u64}}).as_object().unwrap().clone(), "key_auto", "req_auto_3").await;
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_2");
    assert_eq!(sent["input"], json!([history.last().unwrap()]));
    upstream.stop().await;
}

#[tokio::test]
async fn large_full_history_uses_small_actual_frame_before_oauth_limit() {
    let output = json!({"type":"message","id":"large","role":"assistant","content":[{"type":"output_text","text":"o".repeat(9 * 1024 * 1024)}]});
    let mut upstream = Upstream::start(vec![vec![output.clone()], vec![]], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![json!({"role":"user","content":"u".repeat(7 * 1024 * 1024)})];
    run(
        &provider,
        input.clone(),
        "large-auto",
        Map::new(),
        "key_auto",
        "req_auto_large_0",
    )
    .await;
    upstream.frame().await;
    input.extend([output, json!({"role":"user","content":"next"})]);
    assert!(serde_json::to_vec(&input).unwrap().len() > 15 * 1024 * 1024);
    run(
        &provider,
        input,
        "large-auto",
        Map::new(),
        "key_auto",
        "req_auto_large_1",
    )
    .await;
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_0");
    assert_eq!(sent["input"], json!([{"role":"user","content":"next"}]));
    assert!(serde_json::to_vec(&sent).unwrap().len() < 4096);
    upstream.stop().await;
}

#[tokio::test]
async fn changed_prefix_parameters_and_unknown_fields_start_full_chains() {
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![json!({"role":"user","content":"seed"})];
    for (turn, options) in [
        json!({}),
        json!({"instructions":"changed"}),
        json!({"tools":[]}),
        json!({"unknown_business":"changed"}),
        json!({}),
    ]
    .into_iter()
    .enumerate()
    {
        if turn == 4 {
            input = vec![json!({"type":"compaction","encrypted_content":"compact-state"})];
        }
        run(
            &provider,
            input.clone(),
            "auto-change",
            options.as_object().unwrap().clone(),
            "key_auto",
            &format!("req_auto_change_{turn}"),
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, 1);
        assert!(sent.get("previous_response_id").is_none());
        assert_eq!(sent["input"], json!(input));
        input.push(json!({"role":"user","content":format!("turn {turn}")}));
    }
    upstream.stop().await;
}

#[tokio::test]
async fn client_key_and_session_boundaries_never_share_automatic_baselines() {
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = Vec::new();
    for (turn, key, session, expected_connection) in [
        (0, "key_a", "same", 1),
        (1, "key_b", "same", 2),
        (2, "key_a", "other", 3),
        (3, "key_a", "same", 1),
    ] {
        input.push(json!({"role":"user","content":format!("turn {turn}")}));
        run(
            &provider,
            input.clone(),
            session,
            Map::new(),
            key,
            &format!("req_auto_scope_{turn}"),
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, expected_connection);
        if turn == 3 {
            assert_eq!(sent["previous_response_id"], "resp_auto_0");
            assert_eq!(sent["input"].as_array().unwrap().len(), 3);
        } else {
            assert!(sent.get("previous_response_id").is_none());
            assert_eq!(sent["input"], json!(input));
        }
    }
    upstream.stop().await;
}

#[tokio::test]
async fn automatic_history_rejection_recovers_full_once_on_a_fresh_connection() {
    let mut upstream = Upstream::start(vec![], true).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![json!({"role":"user","content":"first"})];
    run(
        &provider,
        input.clone(),
        "reject-auto",
        Map::new(),
        "key_auto",
        "req_auto_reject_0",
    )
    .await;
    upstream.frame().await;
    input.push(json!({"role":"user","content":"next"}));
    run(
        &provider,
        input.clone(),
        "reject-auto",
        Map::new(),
        "key_auto",
        "req_auto_reject_1",
    )
    .await;
    let (old_connection, delta) = upstream.frame().await;
    let (new_connection, full) = upstream.frame().await;
    assert_eq!(old_connection, 1);
    assert_eq!(new_connection, 2);
    assert_eq!(delta["previous_response_id"], "resp_auto_0");
    assert!(full.get("previous_response_id").is_none());
    assert_eq!(full["input"], json!(input));
    upstream.stop().await;
}

pub(super) async fn assert_large_transport_contracts() {
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    for (case, expected) in [
        ("new", "http_sse"),
        ("warmup", "websocket"),
        ("native", "websocket"),
    ] {
        let mut body = json!({"model":"gpt-5.4","session_id":"large-contracts","input":[],"instructions":"x".repeat(16 * 1024 * 1024),"store":false}).as_object().unwrap().clone();
        let mut state = Some(ProviderSessionState::new("openai", json!({"account_id":"acct_provider_contract","conversation_id":"large-contracts","continuation_scope":"connection_local"}).as_object().unwrap().clone()).unwrap());
        let mut request_context = Map::new();
        let mut attempt = context(&format!("req_large_{case}"), CancellationToken::new());
        match case {
            "new" => {
                request_context.insert(
                    "downstream_websocket_connection_id".to_owned(),
                    json!("ws_large"),
                );
            }
            "warmup" => {
                body.insert("generate".to_owned(), json!(false));
            }
            "native" => {
                body.insert("previous_response_id".to_owned(), json!("client_previous"));
                state = Some(ProviderSessionState::new("openai", json!({"account_id":"acct_provider_contract","continuation_scope":"persisted"}).as_object().unwrap().clone()).unwrap());
                attempt = pinned_continuation_context(
                    "req_large_native",
                    "acct_provider_contract",
                    "client_previous",
                    "upstream_previous",
                    1,
                    ContinuationAttempt::Native,
                );
            }
            _ => unreachable!(),
        }
        let mut generate = GenerateRequest::from_protocol_payload(
            ProtocolPayload::json_object("openai", body)
                .unwrap()
                .with_context(request_context),
        );
        if let Some(state) = state {
            generate = generate.with_provider_session_state(state);
        }
        assert_eq!(
            run_operation(&provider, Operation::Generate(generate), attempt).await,
            expected,
            "{case}"
        );
        let (_, sent) = upstream.frame().await;
        assert_eq!(
            sent["instructions"].as_str().unwrap().len(),
            16 * 1024 * 1024
        );
        if case == "native" {
            assert_eq!(sent["previous_response_id"], "upstream_previous");
        }
    }
    upstream.stop().await;
}

fn completed(call: usize, output: Vec<Value>) -> Value {
    json!({"type":"response.completed","response":{"id":format!("resp_auto_{call}"),"model":"gpt-5.4","status":"completed","output":output,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})
}

#[tokio::test]
async fn native_terminal_snapshot_is_a_complete_opaque_baseline() {
    let output = json!({"type":"reasoning","id":"rs_terminal","encrypted_content":"ciphertext","unknown":{"nested":[9007199254740993_u64]}});
    let mut upstream = Upstream::script(
        vec![Reply::Events(vec![completed(0, vec![output.clone()])])],
        false,
    )
    .await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![json!({"role":"user","content":"seed"})];
    run(
        &provider,
        input.clone(),
        "terminal-auto",
        Map::new(),
        "key_auto",
        "req_terminal_0",
    )
    .await;
    upstream.frame().await;
    input.extend([output, json!({"role":"user","content":"next"})]);
    run(
        &provider,
        input,
        "terminal-auto",
        Map::new(),
        "key_auto",
        "req_terminal_1",
    )
    .await;
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_0");
    assert_eq!(sent["input"], json!([{"role":"user","content":"next"}]));
    upstream.stop().await;
}

#[tokio::test]
async fn missing_done_incomplete_failed_and_disconnect_cannot_publish_baselines() {
    let missing_done = Reply::Events(vec![
        json!({"type":"response.output_text.delta","response_id":"resp_auto_0","output_index":0,"content_index":0,"delta":"text is not a complete item"}),
        completed(0, vec![]),
    ]);
    let incomplete = Reply::Events(vec![
        json!({"type":"response.incomplete","response":{"id":"resp_auto_0","status":"incomplete","output":[],"incomplete_details":{"reason":"interrupted"}}}),
    ]);
    let failed = Reply::Events(vec![
        json!({"type":"response.failed","response":{"id":"resp_auto_0","status":"failed","error":{"code":"invalid_request_error","message":"rejected"}}}),
    ]);
    for (case, reply) in [
        ("missing_done", missing_done),
        ("incomplete", incomplete),
        ("failed", failed),
        ("disconnect", Reply::Close),
    ] {
        let mut upstream = Upstream::script(vec![reply], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url_and_retry_budget(&store, upstream.base.clone(), 0);
        let mut stream = Arc::clone(&provider)
            .execute(
                planned_request(
                    "openai",
                    operation(
                        vec![json!({"role":"user","content":"seed"})],
                        case,
                        Map::new(),
                    ),
                ),
                keyed_context("req_invalid_baseline", "key_auto"),
            )
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            if event.is_err() {
                break;
            }
        }
        drop(stream);
        upstream.frame().await;
        let input = vec![
            json!({"role":"user","content":"seed"}),
            json!({"role":"user","content":"next"}),
        ];
        run(
            &provider,
            input.clone(),
            case,
            Map::new(),
            "key_auto",
            "req_after_invalid",
        )
        .await;
        let (_, sent) = upstream.frame().await;
        assert!(sent.get("previous_response_id").is_none(), "{case}");
        assert_eq!(sent["input"], json!(input), "{case}");
        upstream.stop().await;
    }
}

#[tokio::test]
async fn semantic_output_before_history_rejection_does_not_replay() {
    let mut upstream = Upstream::script(vec![Reply::Output(vec![]), Reply::Events(vec![
        json!({"type":"response.output_text.delta","response_id":"resp_auto_1","output_index":0,"content_index":0,"delta":"already executed"}),
        json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"not found after partial output"}}),
    ])], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url_and_retry_budget(&store, upstream.base.clone(), 0);
    let mut input = vec![json!({"role":"user","content":"seed"})];
    run(
        &provider,
        input.clone(),
        "partial-reject",
        Map::new(),
        "key_auto",
        "req_partial_0",
    )
    .await;
    upstream.frame().await;
    input.push(json!({"role":"user","content":"next"}));
    let mut stream = Arc::clone(&provider)
        .execute(
            planned_request("openai", operation(input, "partial-reject", Map::new())),
            keyed_context("req_partial_1", "key_auto"),
        )
        .await
        .unwrap();
    let mut semantic = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => {
                semantic |= event
                    .wire_event()
                    .is_some_and(|wire| wire.event_type() == Some("response.output_text.delta"))
            }
            Err(_) => break,
        }
    }
    drop(stream);
    assert!(semantic);
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_0");
    assert!(
        upstream.frames.try_recv().is_err(),
        "partial output must not create a replay attempt"
    );
    upstream.stop().await;
}

#[tokio::test]
async fn eviction_and_cancellation_drop_socket_local_baselines() {
    let mut upstream = Upstream::script(
        vec![Reply::Output(vec![]), Reply::Output(vec![]), Reply::Hold],
        false,
    )
    .await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let (provider, pool) = provider_with_capacity_tracking(
        &store,
        upstream.base.clone(),
        Arc::new(MemoryCooldownPort::new()),
    );
    let mut input = vec![json!({"role":"user","content":"seed"})];
    run(
        &provider,
        input.clone(),
        "evict-auto",
        Map::new(),
        "key_openai_contract",
        "req_evict_0",
    )
    .await;
    upstream.frame().await;
    pool.evict_account("acct_provider_contract").await;
    input.push(json!({"role":"user","content":"next"}));
    run(
        &provider,
        input.clone(),
        "evict-auto",
        Map::new(),
        "key_openai_contract",
        "req_evict_1",
    )
    .await;
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 2);
    assert_eq!(sent["input"], json!(input));
    assert!(sent.get("previous_response_id").is_none());
    let cancellation = CancellationToken::new();
    let mut stream = Arc::clone(&provider)
        .execute(
            planned_request("openai", operation(input.clone(), "evict-auto", Map::new())),
            context("req_cancel_auto", cancellation.clone()),
        )
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event
            .unwrap()
            .wire_event()
            .is_some_and(|wire| wire.event_type() == Some("response.output_text.delta"))
        {
            break;
        }
    }
    cancellation.cancel();
    while let Some(event) = stream.next().await {
        if event.is_err() {
            break;
        }
    }
    drop(stream);
    upstream.frame().await;
    input.push(json!({"role":"user","content":"after cancel"}));
    run(
        &provider,
        input.clone(),
        "evict-auto",
        Map::new(),
        "key_openai_contract",
        "req_cancel_after",
    )
    .await;
    let (connection, sent) = upstream.frame().await;
    assert!(connection > 2);
    assert!(sent.get("previous_response_id").is_none());
    assert_eq!(sent["input"], json!(input));
    upstream.stop().await;
    pool.shutdown().await;
}

#[tokio::test]
async fn selected_account_and_credential_revision_isolate_automatic_history() {
    use gateway_core::account::{CredentialCasOutcome, CredentialCasUpdate, ProviderAccountUpdate};
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    for account in ["acct_provider_contract", "acct_scope_other"] {
        create_account(&store, account).await;
    }
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![json!({"role":"user","content":"seed"})];
    for (turn, account, connection, delta) in [
        (0, "acct_provider_contract", 1, false),
        (1, "acct_scope_other", 2, false),
        (2, "acct_provider_contract", 1, true),
    ] {
        let body = json!({"model":"gpt-5.4","session_id":"account-auto","input":input,"store":false,"stream":true}).as_object().unwrap().clone();
        let state = ProviderSessionState::new("openai", json!({"account_id":account,"conversation_id":"account-auto","continuation_scope":"connection_local"}).as_object().unwrap().clone()).unwrap();
        let operation = Operation::Generate(
            GenerateRequest::from_protocol_payload(
                ProtocolPayload::json_object("openai", body).unwrap(),
            )
            .with_provider_session_state(state),
        );
        run_operation(
            &provider,
            operation,
            diagnostic_context(&format!("req_account_auto_{turn}"), account),
        )
        .await;
        let (actual_connection, sent) = upstream.frame().await;
        assert_eq!(actual_connection, connection);
        assert_eq!(sent.get("previous_response_id").is_some(), delta);
        input.push(json!({"role":"user","content":format!("turn {turn}")}));
    }
    let account = store.account("acct_provider_contract").unwrap();
    let loaded = store.load_current_credential(account.id()).await.unwrap();
    let update = CredentialCasUpdate::new(
        account.id().clone(),
        account.revision(),
        ProviderAccountUpdate {
            account_id: account.id().clone(),
            name: account.name().to_owned(),
            email: None,
            plan_type: None,
        },
        loaded.credential,
        false,
        None,
        None,
    )
    .unwrap();
    assert!(matches!(
        store.compare_and_swap_credential(update).await.unwrap(),
        CredentialCasOutcome::Updated(_)
    ));
    run_operation(
        &provider,
        operation(input.clone(), "account-auto", Map::new()),
        diagnostic_context("req_revision_auto", "acct_provider_contract"),
    )
    .await;
    let (_, sent) = upstream.frame().await;
    assert!(sent.get("previous_response_id").is_none());
    assert_eq!(sent["input"], json!(input));
    upstream.stop().await;
}

#[tokio::test]
async fn explicit_native_previous_keeps_its_input_and_socket_contract() {
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    run(
        &provider,
        vec![json!({"role":"user","content":"seed"})],
        "native-auto",
        Map::new(),
        "key_openai_contract",
        "req_native_seed",
    )
    .await;
    upstream.frame().await;
    let input = vec![
        json!({"type":"function_call_output","call_id":"native_call","output":"opaque native suffix","unknown":9007199254740993_u64}),
    ];
    run_operation(
        &provider,
        operation(
            input.clone(),
            "native-auto",
            json!({"previous_response_id":"client_previous"})
                .as_object()
                .unwrap()
                .clone(),
        ),
        pinned_continuation_context(
            "req_native_auto",
            "acct_provider_contract",
            "client_previous",
            "resp_auto_0",
            1,
            ContinuationAttempt::Native,
        ),
    )
    .await;
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_0");
    assert_eq!(sent["input"], json!(input));
    upstream.stop().await;
}

#[tokio::test]
async fn exhausted_snapshot_budget_keeps_normal_api_key_full_requests_usable() {
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    store
        .seed_api_key(
            "acct_provider_contract",
            upstream.base.clone(),
            provider_openai::credential::ResponsesTransport::PreferWebsocket,
        )
        .await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![json!({"role":"user","content":"x".repeat(33 * 1024 * 1024)})];
    for turn in 0..2 {
        run(
            &provider,
            input.clone(),
            "budget-auto",
            Map::new(),
            "key_auto",
            &format!("req_budget_{turn}"),
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, 1);
        assert!(sent.get("previous_response_id").is_none());
        assert_eq!(sent["input"], json!(input));
        input.push(json!({"role":"user","content":"next"}));
    }
    upstream.stop().await;
}

#[tokio::test]
async fn failure_terminal_with_output_cannot_trigger_full_recovery() {
    let partial = json!({"type":"message","id":"partial_message","role":"assistant","content":[{"type":"output_text","text":"already executed"}]});
    let mut upstream = Upstream::script(vec![Reply::Output(vec![]), Reply::Events(vec![json!({"type":"response.failed","response":{"id":"resp_auto_1","status":"failed","output":[partial],"error":{"code":"previous_response_not_found","message":"history rejection with output"}}})])], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url_and_retry_budget(&store, upstream.base.clone(), 0);
    let mut input = vec![json!({"role":"user","content":"seed"})];
    run(
        &provider,
        input.clone(),
        "terminal-reject",
        Map::new(),
        "key_auto",
        "req_terminal_reject_0",
    )
    .await;
    upstream.frame().await;
    input.push(json!({"role":"user","content":"next"}));
    let mut stream = Arc::clone(&provider)
        .execute(
            planned_request("openai", operation(input, "terminal-reject", Map::new())),
            keyed_context("req_terminal_reject_1", "key_auto"),
        )
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event.is_err() {
            break;
        }
    }
    drop(stream);
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_0");
    assert!(
        upstream.frames.try_recv().is_err(),
        "terminal output must not create a replay attempt"
    );
    upstream.stop().await;
}
