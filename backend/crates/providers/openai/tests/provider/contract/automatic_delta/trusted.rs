//! 从真实 Provider 验证确定投影、原生历史恢复与三轮客户端视图

use super::*;

fn user(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}

fn raw_text(text: &str) -> Value {
    json!({"type":"message","id":"msg_native","status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}],"future":{"signature":"opaque-native"}})
}

fn canonical(text: &str) -> Value {
    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn interleaved() -> (Vec<Value>, Vec<Value>, Value) {
    let output = vec![
        raw_text("before "),
        json!({"type":"function_call","id":"fc_native","status":"completed","call_id":"call_native","name":"inspect","arguments":"{ \"n\":9007199254740993 }"}),
        raw_text("after"),
    ];
    let client = vec![
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"before ","annotations":[]},{"type":"output_text","text":"after","annotations":[]}]}),
        json!({"type":"function_call","call_id":"call_native","name":"inspect","arguments":"{ \"n\":9007199254740993 }"}),
    ];
    let result = json!({"type":"function_call_output","call_id":"call_native","output":"synthetic result","future":"retain"});
    (output, client, result)
}

#[tokio::test]
async fn canonical_three_to_two_items_then_raw_output_keep_native_history_mapping() {
    let (output, client_output, result) = interleaved();
    let second_output = raw_text("second response");
    let mut upstream =
        Upstream::start(vec![output, vec![second_output.clone()], vec![]], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut client = vec![user("seed")];
    run(
        &provider,
        client.clone(),
        "trusted-three",
        Map::new(),
        "key_trusted",
        "req_trusted_0",
    )
    .await;
    upstream.frame().await;
    client.extend(client_output);
    client.push(result.clone());
    run(
        &provider,
        client.clone(),
        "trusted-three",
        Map::new(),
        "key_trusted",
        "req_trusted_1",
    )
    .await;
    let (connection, delta) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(delta["previous_response_id"], "resp_auto_0");
    assert_eq!(delta["input"], json!([result]));
    // 第三轮恢复 raw output，旧前缀仍是第二轮的客户端视图
    client.extend([second_output, user("third")]);
    run(
        &provider,
        client,
        "trusted-three",
        Map::new(),
        "key_trusted",
        "req_trusted_2",
    )
    .await;
    let (connection, delta) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(delta["previous_response_id"], "resp_auto_1");
    assert_eq!(delta["input"], json!([user("third")]));
    upstream.stop().await;
}

#[tokio::test]
async fn trusted_history_rejection_restores_native_full_and_keeps_client_baseline() {
    let (output, client_output, result) = interleaved();
    let second_output = raw_text("second response");
    let mut upstream = Upstream::start(
        vec![output.clone(), vec![], vec![second_output.clone()]],
        true,
    )
    .await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut client = vec![user("seed")];
    run(
        &provider,
        client.clone(),
        "trusted-reject",
        Map::new(),
        "key_trusted",
        "req_trusted_reject_0",
    )
    .await;
    let (_, first) = upstream.frame().await;
    client.extend(client_output);
    client.push(result.clone());
    run(
        &provider,
        client.clone(),
        "trusted-reject",
        Map::new(),
        "key_trusted",
        "req_trusted_reject_1",
    )
    .await;
    let (connection, delta) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(delta["input"], json!([result.clone()]));
    let (connection, full) = upstream.frame().await;
    assert_eq!(connection, 2);
    let mut native = first["input"].as_array().unwrap().clone();
    native.extend(output);
    native.push(result);
    assert!(full.get("previous_response_id").is_none());
    assert_eq!(full["input"], json!(native));
    client.extend([second_output.clone(), user("third")]);
    run(
        &provider,
        client,
        "trusted-reject",
        Map::new(),
        "key_trusted",
        "req_trusted_reject_2",
    )
    .await;
    let (connection, delta) = upstream.frame().await;
    assert_eq!(connection, 2);
    assert_eq!(delta["previous_response_id"], "resp_auto_2");
    assert_eq!(delta["input"], json!([user("third")]));
    let (_, full) = upstream.frame().await;
    native.extend([second_output, user("third")]);
    assert_eq!(full["input"], json!(native));
    upstream.stop().await;
}

#[tokio::test]
async fn reasoning_summary_rebuild_and_known_typed_signature_omission_restore_raw() {
    for typed in [false, true] {
        let reasoning = json!({"type":"reasoning","id":"rs_native","status":"completed","summary":[{"type":"summary_text","text":"one "},{"type":"summary_text","text":"two"}],"encrypted_content":"opaque-native-ciphertext","signed_extension":"preserved"});
        let tool = json!({"type":"function_call","id":"fc_native","status":"completed","call_id":"call_native","name":"inspect","arguments":"{}"});
        let mut upstream = Upstream::start(vec![vec![reasoning, tool]], true).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("seed")];
        run(
            &provider,
            input.clone(),
            "trusted-reasoning",
            Map::new(),
            "key_trusted",
            "req_reasoning_0",
        )
        .await;
        upstream.frame().await;
        if !typed {
            input.push(json!({"type":"reasoning","summary":[{"type":"summary_text","text":"one two"}],"encrypted_content":"opaque-native-ciphertext"}));
        }
        input.push(json!({"type":"function_call","call_id":"call_native","name":"inspect","arguments":"{}"}));
        let result = json!({"type":"function_call_output","call_id":"call_native","output":"ok"});
        input.push(result.clone());
        run(
            &provider,
            input,
            "trusted-reasoning",
            Map::new(),
            "key_trusted",
            "req_reasoning_1",
        )
        .await;
        let (_, delta) = upstream.frame().await;
        assert_eq!(delta["input"], json!([result]));
        assert_eq!(delta["previous_response_id"], "resp_auto_0");
        let (_, full) = upstream.frame().await;
        assert_eq!(
            full["input"][1]["encrypted_content"],
            "opaque-native-ciphertext"
        );
        assert_eq!(full["input"][1]["signed_extension"], "preserved");
        assert_eq!(full["input"][1]["summary"].as_array().unwrap().len(), 2);
        upstream.stop().await;
    }
}

#[tokio::test]
async fn trusted_projection_edits_and_unsupported_native_shapes_stay_full() {
    for case in [
        "text",
        "opaque",
        "id",
        "status",
        "role",
        "annotations",
        "logprobs",
        "unknown",
        "multiple_reasoning",
        "assistant_tail",
    ] {
        let mut raw = vec![raw_text("answer")];
        let mut rebuilt = canonical("answer");
        let mut tail = user("next");
        match case {
            "text" => rebuilt["content"][0]["text"] = json!("edited"),
            "opaque" => rebuilt["future"] = json!({"signature":"edited"}),
            "id" => rebuilt["id"] = json!("msg_changed"),
            "status" => rebuilt["status"] = json!("in_progress"),
            "role" => rebuilt["role"] = json!("user"),
            "annotations" => {
                raw[0]["content"][0]["annotations"] = json!([{"type":"future_annotation"}])
            }
            "logprobs" => raw[0]["content"][0]["logprobs"] = json!([{"token":"synthetic"}]),
            "unknown" => raw.push(json!({"type":"future_item","opaque":"synthetic"})),
            "multiple_reasoning" => raw.extend([
                json!({"type":"reasoning","summary":[]}),
                json!({"type":"reasoning","summary":[]}),
            ]),
            "assistant_tail" => tail = canonical("new assistant fragment"),
            _ => unreachable!(),
        }
        let mut upstream = Upstream::start(vec![raw, vec![]], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        run(
            &provider,
            vec![user("seed")],
            case,
            Map::new(),
            "key_trusted",
            "req_negative_0",
        )
        .await;
        upstream.frame().await;
        let input = vec![user("seed"), rebuilt, tail];
        run(
            &provider,
            input.clone(),
            case,
            Map::new(),
            "key_trusted",
            "req_negative_1",
        )
        .await;
        let (_, full) = upstream.frame().await;
        assert!(full.get("previous_response_id").is_none(), "{case}");
        assert_eq!(full["input"], json!(input), "{case}");
        upstream.stop().await;
    }
}

#[tokio::test]
async fn large_canonical_full_uses_small_delta_and_oversized_tail_restores_native_http() {
    for large_tail in [false, true] {
        let text = if large_tail {
            "small".to_owned()
        } else {
            "o".repeat(9 * 1024 * 1024)
        };
        let raw = raw_text(&text);
        let mut upstream = Upstream::start(vec![vec![raw.clone()], vec![]], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let seed = user(&"u".repeat(if large_tail { 1024 } else { 7 * 1024 * 1024 }));
        run(
            &provider,
            vec![seed.clone()],
            "trusted-large",
            Map::new(),
            "key_trusted",
            "req_trusted_large_0",
        )
        .await;
        let (_, first) = upstream.frame().await;
        let tail = user(&"n".repeat(if large_tail { 16 * 1024 * 1024 } else { 4 }));
        let input = vec![seed, canonical(&text), tail.clone()];
        assert!(serde_json::to_vec(&input).unwrap().len() > 15 * 1024 * 1024);
        let transport = run(
            &provider,
            input,
            "trusted-large",
            Map::new(),
            "key_trusted",
            "req_trusted_large_1",
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        if large_tail {
            assert_eq!(transport, "http_sse");
            assert_eq!(sent["input"], json!([first["input"][0].clone(), raw, tail]));
            assert!(sent.get("previous_response_id").is_none());
        } else {
            assert_eq!(transport, "websocket");
            assert_eq!(connection, 1);
            assert_eq!(sent["previous_response_id"], "resp_auto_0");
            assert_eq!(sent["input"], json!([tail]));
            assert!(serde_json::to_vec(&sent).unwrap().len() < 4096);
        }
        upstream.stop().await;
    }
}

#[tokio::test]
async fn tool_fields_and_result_correlation_are_required_for_trusted_recognition() {
    for case in [
        "call_id",
        "name",
        "arguments",
        "orphan",
        "duplicate",
        "missing",
    ] {
        let (output, mut client, result) = interleaved();
        match case {
            "call_id" | "name" => client[1][case] = json!("changed"),
            "arguments" => client[1][case] = json!("{ }"),
            _ => {}
        }
        let mut upstream = Upstream::start(vec![output, vec![]], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        run(
            &provider,
            vec![user("seed")],
            case,
            Map::new(),
            "key_trusted",
            "req_tool_negative_0",
        )
        .await;
        upstream.frame().await;
        let mut input = vec![user("seed")];
        input.extend(client);
        if case != "missing" {
            let mut result = result;
            if case == "orphan" {
                result["call_id"] = json!("orphan");
            }
            input.push(result.clone());
            if case == "duplicate" {
                input.push(result);
            }
        }
        input.push(user("next"));
        run(
            &provider,
            input.clone(),
            case,
            Map::new(),
            "key_trusted",
            "req_tool_negative_1",
        )
        .await;
        let (_, full) = upstream.frame().await;
        assert!(full.get("previous_response_id").is_none(), "{case}");
        assert_eq!(full["input"], json!(input), "{case}");
        upstream.stop().await;
    }
}

#[tokio::test]
async fn dual_view_budget_exhaustion_sends_client_full_then_releases_for_small_chain() {
    let text = "o".repeat(17 * 1024 * 1024);
    let mut upstream =
        Upstream::start(vec![vec![raw_text(&text)], vec![], vec![], vec![]], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    run(
        &provider,
        vec![user("seed")],
        "trusted-budget",
        Map::new(),
        "key_trusted",
        "req_budget_0",
    )
    .await;
    upstream.frame().await;
    let input = vec![user("seed"), canonical(&text), user("next")];
    assert_eq!(
        run(
            &provider,
            input.clone(),
            "trusted-budget",
            Map::new(),
            "key_trusted",
            "req_budget_1"
        )
        .await,
        "http_sse"
    );
    let (_, full) = upstream.frame().await;
    assert!(full.get("previous_response_id").is_none());
    assert_eq!(full["input"], json!(input));
    // HTTP回退的snapshot与旧原生patch都要释放，不能卡住后续小请求
    let small = vec![user("new baseline")];
    run(
        &provider,
        small.clone(),
        "trusted-budget",
        Map::new(),
        "key_trusted",
        "req_budget_2",
    )
    .await;
    upstream.frame().await;
    let mut next = small;
    next.push(user("small next"));
    run(
        &provider,
        next,
        "trusted-budget",
        Map::new(),
        "key_trusted",
        "req_budget_3",
    )
    .await;
    let (_, delta) = upstream.frame().await;
    assert_eq!(delta["previous_response_id"], "resp_auto_2");
    assert_eq!(delta["input"], json!([user("small next")]));
    upstream.stop().await;
}

struct CloseAtPrepared {
    close: Arc<tokio::sync::Notify>,
    closed: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    fired: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct PreparedEvent {
    target: bool,
    prepared: bool,
}

impl tracing::field::Visit for PreparedEvent {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "request_id" {
            self.target = value == "req_presend_1";
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.prepared = format!("{value:?}") == "OpenAI upstream payload prepared";
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CloseAtPrepared {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = PreparedEvent::default();
        event.record(&mut fields);
        if fields.target
            && fields.prepared
            && !self.fired.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.close.notify_one();
            // 独立runtime线程完成close握手，确认原socket已退出后才允许发送继续
            self.closed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("close handshake before send");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_pre_send_disconnect_recovers_native_full_without_sending_suffix_to_new_socket() {
    use tracing_subscriber::layer::SubscriberExt;
    let (output, client_output, result) = interleaved();
    let close = Arc::new(tokio::sync::Notify::new());
    let (closed_tx, closed) = std::sync::mpsc::channel();
    let mut upstream = Upstream::script(
        vec![
            Reply::CloseAfterOutput(output.clone(), Arc::clone(&close), closed_tx),
            Reply::Output(vec![]),
        ],
        false,
    )
    .await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    run(
        &provider,
        vec![user("seed")],
        "trusted-presend",
        Map::new(),
        "key_auto_diagnostics",
        "req_presend_0",
    )
    .await;
    let (_, first) = upstream.frame().await;
    let mut input = vec![user("seed")];
    input.extend(client_output);
    input.push(result.clone());
    let layer = CloseAtPrepared {
        close,
        closed: std::sync::Mutex::new(closed),
        fired: std::sync::atomic::AtomicBool::new(false),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let (context, trace) = diagnostics::traced_context("req_presend_1");
    run_operation(
        &provider,
        operation(input, "trusted-presend", Map::new()),
        context,
    )
    .await;
    let (connection, full) = upstream.frame().await;
    assert!(connection > 1);
    assert!(full.get("previous_response_id").is_none());
    let mut native = first["input"].as_array().unwrap().clone();
    native.extend(output);
    native.push(result);
    assert_eq!(full["input"], json!(native));
    assert!(
        upstream.frames.try_recv().is_err(),
        "old socket received no delta"
    );
    let prepared = diagnostics::prepared(&trace);
    assert_eq!(prepared[0]["deltaReason"], "trusted_delta");
    assert_eq!(prepared[0]["actualInputItems"], 1);
    assert!(
        trace.snapshot().unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["stage"] == "upstream.send.failed"
                && event["data"]["sendState"] == "not_sent")
    );
    upstream.stop().await;
}

#[tokio::test]
async fn trusted_partial_output_does_not_replay_after_history_error() {
    let (output, client_output, result) = interleaved();
    let mut upstream = Upstream::script(vec![Reply::Output(output), Reply::Events(vec![
        json!({"type":"response.output_text.delta","response_id":"resp_auto_1","output_index":0,"content_index":0,"delta":"executed"}),
        json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"synthetic error after execution"}})
    ])], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url_and_retry_budget(&store, upstream.base.clone(), 0);
    run(
        &provider,
        vec![user("seed")],
        "trusted-partial",
        Map::new(),
        "key_trusted",
        "req_partial_trusted_0",
    )
    .await;
    upstream.frame().await;
    let mut input = vec![user("seed")];
    input.extend(client_output);
    input.push(result);
    let mut stream = Arc::clone(&provider)
        .execute(
            planned_request("openai", operation(input, "trusted-partial", Map::new())),
            keyed_context("req_partial_trusted_1", "key_trusted"),
        )
        .await
        .unwrap();
    let mut partial = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => {
                partial |= event
                    .wire_event()
                    .is_some_and(|wire| wire.event_type() == Some("response.output_text.delta"))
            }
            Err(_) => break,
        }
    }
    drop(stream);
    assert!(partial);
    let (connection, sent) = upstream.frame().await;
    assert_eq!(connection, 1);
    assert_eq!(sent["previous_response_id"], "resp_auto_0");
    assert!(upstream.frames.try_recv().is_err());
    upstream.stop().await;
}

#[tokio::test]
async fn trusted_tail_preserves_new_assistant_and_rejects_reused_historical_call_id() {
    for reuse in [false, true] {
        let mut upstream = Upstream::start(vec![vec![raw_text("answer")]], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let seed = vec![
            user("seed"),
            json!({"type":"function_call","call_id":"call_older","name":"inspect","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"call_older","output":"old result"}),
        ];
        run(
            &provider,
            seed.clone(),
            "trusted-tail",
            Map::new(),
            "key_trusted",
            "req_trusted_tail_0",
        )
        .await;
        upstream.frame().await;
        let mut tail = vec![user("new user"), canonical("new assistant fragment")];
        if reuse {
            tail.extend([json!({"type":"function_call","call_id":"call_older","name":"inspect","arguments":"{}"}),json!({"type":"function_call_output","call_id":"call_older","output":"new result"})]);
        }
        let mut input = seed;
        input.push(canonical("answer"));
        input.extend(tail.clone());
        run(
            &provider,
            input.clone(),
            "trusted-tail",
            Map::new(),
            "key_trusted",
            "req_trusted_tail_1",
        )
        .await;
        let (_, sent) = upstream.frame().await;
        if reuse {
            assert!(sent.get("previous_response_id").is_none());
            assert_eq!(sent["input"], json!(input));
        } else {
            assert_eq!(sent["previous_response_id"], "resp_auto_0");
            assert_eq!(sent["input"], json!(tail));
        }
        upstream.stop().await;
    }
}
