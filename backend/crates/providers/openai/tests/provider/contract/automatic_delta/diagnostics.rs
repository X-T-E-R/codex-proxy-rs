//! 从真实 Provider 边界验证前缀分型、纯元数据输出及原始 full/delta 决策

use super::*;
use gateway_core::diagnostics::TraceContext;

fn traced_context(id: &str) -> (AttemptContext, TraceContext) {
    let trace = TraceContext::new(id);
    let context = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new(id).unwrap(),
            ClientApiKeyId::new("key_auto_diagnostics").unwrap(),
        )
        .with_trace(trace.clone())
        .with_request_location(Some(global_request_location())),
        NonZeroU32::MIN,
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None)
            .with_account_scope(contract_account_scope()),
        None,
        CancellationToken::new(),
    );
    (context, trace)
}

fn prepared(trace: &TraceContext) -> Vec<Value> {
    trace.snapshot().unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["stage"] == "transport.payload.prepared")
        .map(|event| event["data"].clone())
        .collect()
}

#[tokio::test]
async fn prefix_failures_are_classified_without_changing_full_or_delta_frames() {
    let user = json!({"type":"message","role":"user","content":"PRIVATE_SEED"});
    let message = json!({"type":"message","role":"assistant","id":"msg_synthetic","status":"completed","content":[{"type":"output_text","text":"PRIVATE_ANSWER","annotations":[],"logprobs":[],"future":{"id":"PRIVATE_CONTENT_ID"}}],"future":9007199254740993_u64});
    let tool = json!({"type":"function_call","id":"fc_synthetic","status":"completed","call_id":"call_synthetic","name":"synthetic_tool","arguments":"{\"id\":\"PRIVATE_ARGUMENT\"}","future":{"status":"PRIVATE_TOOL_STATUS"}});
    let reasoning = json!({"type":"reasoning","id":"rs_synthetic","status":"completed","content":[{"type":"reasoning_text","text":"PRIVATE_REASONING"}],"summary":[],"encrypted_content":"PRIVATE_CIPHERTEXT","future":{"id":"PRIVATE_REASONING_ID"}});
    let output = vec![message.clone(), tool.clone(), reasoning.clone()];
    let mut projected = reasoning.clone();
    projected.as_object_mut().unwrap().shift_remove("status");
    projected.as_object_mut().unwrap().shift_remove("content");
    let input = vec![
        user.clone(),
        message,
        tool,
        projected,
        json!({"role":"user","content":"PRIVATE_NEXT"}),
    ];
    let mut cases = vec![("exact", input.clone(), Map::new(), "delta", None)];
    let mut shorter = vec![json!({"role":"user","content":"PRIVATE_REWRITTEN"})];
    cases.push((
        "shorter_first",
        shorter.clone(),
        Map::new(),
        "prefix_changed",
        Some("input_shorter"),
    ));
    shorter.extend(input.iter().skip(1).cloned());
    cases.push((
        "old_input_first",
        shorter,
        Map::new(),
        "prefix_changed",
        Some("prior_input_changed"),
    ));
    for (name, index, field) in [
        ("message_id", 1, "id"),
        ("message_status", 1, "status"),
        ("tool_id", 2, "id"),
        ("tool_status", 2, "status"),
        ("reasoning_id", 3, "id"),
    ] {
        let mut changed = input.clone();
        changed[index].as_object_mut().unwrap().shift_remove(field);
        cases.push((
            name,
            changed,
            Map::new(),
            "prefix_changed",
            Some("prior_output_metadata_only"),
        ));
    }
    for field in ["annotations", "logprobs"] {
        let mut changed = input.clone();
        changed[1]["content"][0]
            .as_object_mut()
            .unwrap()
            .shift_remove(field);
        cases.push((
            field,
            changed,
            Map::new(),
            "prefix_changed",
            Some("prior_output_metadata_only"),
        ));
    }
    let mut changed = input.clone();
    changed[1]["id"] = json!("PRIVATE_REGENERATED_ID");
    changed[1]["content"][0]["annotations"] = json!([{"text":"PRIVATE_ANNOTATION"}]);
    changed[2]["status"] = json!("PRIVATE_STATUS");
    cases.push((
        "combined_metadata",
        changed.clone(),
        Map::new(),
        "prefix_changed",
        Some("prior_output_metadata_only"),
    ));
    changed[3]["future"]["id"] = json!("PRIVATE_CHANGED_UNKNOWN");
    cases.push((
        "later_unknown_overrides_metadata",
        changed,
        Map::new(),
        "prefix_changed",
        Some("prior_output_other"),
    ));
    for (name, pointer, replacement) in [
        ("unknown_item", "/1/future", json!(9007199254740994_u64)),
        (
            "unknown_content",
            "/1/content/0/future/id",
            json!("PRIVATE_CHANGED_CONTENT_ID"),
        ),
        ("text", "/1/content/0/text", json!("PRIVATE_CHANGED_TEXT")),
        ("content_type", "/1/content/0/type", json!("input_text")),
        (
            "arguments",
            "/2/arguments",
            json!("{\"id\":\"PRIVATE_CHANGED_ARGUMENT\"}"),
        ),
        (
            "unknown_tool_status",
            "/2/future/status",
            json!("PRIVATE_CHANGED_TOOL_STATUS"),
        ),
        (
            "ciphertext",
            "/3/encrypted_content",
            json!("PRIVATE_CHANGED_CIPHERTEXT"),
        ),
    ] {
        let mut changed = Value::Array(input.clone());
        *changed.pointer_mut(pointer).unwrap() = replacement;
        cases.push((
            name,
            changed.as_array().unwrap().clone(),
            Map::new(),
            "prefix_changed",
            Some("prior_output_other"),
        ));
    }
    let mut changed = input.clone();
    changed[1]["content"][0]["status"] = json!("PRIVATE_NESTED_STATUS");
    cases.push((
        "nonmetadata_content_status",
        changed,
        Map::new(),
        "prefix_changed",
        Some("prior_output_other"),
    ));
    let mut changed = input.clone();
    changed.swap(1, 2);
    cases.push((
        "output_order",
        changed,
        Map::new(),
        "prefix_changed",
        Some("prior_output_other"),
    ));
    cases.push((
        "property_first",
        input.clone(),
        json!({"instructions":"PRIVATE_OPTIONS"})
            .as_object()
            .unwrap()
            .clone(),
        "properties_changed",
        None,
    ));

    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(captured.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    for (name, input, options, reason, category) in &cases {
        let mut upstream = Upstream::start(vec![output.clone()], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let (seed_context, seed_trace) = traced_context("req_prefix_seed");
        run_operation(
            &provider,
            operation(vec![user.clone()], "prefix-diagnostics", Map::new()),
            seed_context,
        )
        .await;
        upstream.frame().await;
        let seed_metadata = prepared(&seed_trace);
        assert_eq!(seed_metadata[0]["deltaReason"], "no_baseline");
        assert!(seed_metadata[0].get("prefixFailure").is_none());
        let id = format!("req_prefix_{name}");
        let (context, trace) = traced_context(&id);
        run_operation(
            &provider,
            operation(input.clone(), "prefix-diagnostics", options.clone()),
            context,
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, 1, "{name}");
        let automatic = *reason == "delta";
        assert_eq!(
            sent.get("previous_response_id").is_some(),
            automatic,
            "{name}"
        );
        assert_eq!(
            sent["input"],
            if automatic {
                json!([input.last().unwrap()])
            } else {
                json!(input)
            },
            "{name}"
        );
        let metadata = prepared(&trace);
        assert_eq!(metadata.len(), 1, "{name}");
        let mut expected = json!({
            "fullPayloadBytes": metadata[0]["fullPayloadBytes"],
            "actualPayloadBytes": serde_json::to_vec(&sent).unwrap().len(),
            "automaticDelta": automatic, "deltaReason": reason,
        });
        if let Some(category) = category {
            expected["prefixFailure"] = json!(category);
        }
        assert_eq!(metadata[0], expected, "{name}");
        let events = captured.json_events();
        let fields = events
            .iter()
            .find_map(|event| {
                let fields = event["fields"].as_object()?;
                (fields.get("message") == Some(&json!("OpenAI upstream payload prepared"))
                    && fields.get("request_id") == Some(&json!(id)))
                .then_some(fields)
            })
            .unwrap();
        let mut expected_log = json!({
            "message":"OpenAI upstream payload prepared", "request_id":id,
            "full_payload_bytes":metadata[0]["fullPayloadBytes"],
            "actual_payload_bytes":metadata[0]["actualPayloadBytes"],
            "automatic_delta":automatic, "delta_reason":reason,
        });
        if let Some(category) = category {
            expected_log["prefix_failure"] = json!(category);
        }
        assert_eq!(fields, expected_log.as_object().unwrap(), "{name}");
        if !automatic {
            assert_eq!(
                metadata[0]["fullPayloadBytes"], metadata[0]["actualPayloadBytes"],
                "{name}"
            );
        }
        upstream.stop().await;
    }
    let logs = serde_json::to_string(&captured.json_events()).unwrap();
    for private in [
        "PRIVATE_ANSWER",
        "PRIVATE_CIPHERTEXT",
        "PRIVATE_ARGUMENT",
        "PRIVATE_REGENERATED_ID",
        "PRIVATE_CHANGED_UNKNOWN",
    ] {
        assert!(
            !logs.contains(private),
            "prepared logs leaked fixture content"
        );
    }
}

#[tokio::test]
async fn metadata_full_fallback_keeps_original_history_and_small_full_rebases_next_turn() {
    for large in [false, true] {
        let size = if large { 9 * 1024 * 1024 } else { 2800 * 1024 };
        let output = json!({"type":"message","id":"msg_large_synthetic","status":"completed","role":"assistant","content":[{"type":"output_text","text":"x".repeat(size),"annotations":[]}]});
        let user = json!({"role":"user","content":if large { "u".repeat(7 * 1024 * 1024) } else { "seed".to_owned() }});
        let mut upstream = Upstream::start(vec![vec![output.clone()]], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        run(
            &provider,
            vec![user.clone()],
            "large-prefix",
            Map::new(),
            "key_auto_diagnostics",
            "req_prefix_large_seed",
        )
        .await;
        upstream.frame().await;
        let mut changed = output;
        changed.as_object_mut().unwrap().shift_remove("id");
        let mut input = vec![user, changed, json!({"role":"user","content":"next"})];
        let (context, trace) = traced_context("req_prefix_large_full");
        assert_eq!(
            run_operation(
                &provider,
                operation(input.clone(), "large-prefix", Map::new()),
                context
            )
            .await,
            if large { "http_sse" } else { "websocket" }
        );
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, if large { 2 } else { 1 });
        assert!(sent.get("previous_response_id").is_none());
        assert_eq!(sent["input"], json!(input));
        let metadata = prepared(&trace);
        assert_eq!(metadata[0]["deltaReason"], "prefix_changed");
        assert_eq!(metadata[0]["prefixFailure"], "prior_output_metadata_only");
        assert_eq!(
            metadata[0]["fullPayloadBytes"],
            metadata[0]["actualPayloadBytes"]
        );
        if !large {
            input.push(json!({"role":"user","content":"after full"}));
            let (context, trace) = traced_context("req_prefix_after_full");
            run_operation(
                &provider,
                operation(input, "large-prefix", Map::new()),
                context,
            )
            .await;
            let (connection, delta) = upstream.frame().await;
            assert_eq!(connection, 1);
            assert_eq!(delta["previous_response_id"], "resp_auto_1");
            assert_eq!(
                delta["input"],
                json!([{"role":"user","content":"after full"}])
            );
            assert!(prepared(&trace)[0].get("prefixFailure").is_none());
        }
        upstream.stop().await;
    }
}

#[tokio::test]
async fn maximum_item_prefix_checks_late_unknown_changes_after_metadata_mismatches() {
    for other in [false, true] {
        let output: Vec<_> = (0..4096).map(|index| json!({
            "type":"message", "id":format!("msg_synthetic_{index}"), "role":"assistant", "status":"completed",
            "content":[{"type":"output_text","text":"x".repeat(512),"annotations":[]}], "future":index,
        })).collect();
        let mut upstream = Upstream::script(
            vec![Reply::Events(vec![completed(0, output.clone())])],
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
            "max-prefix",
            Map::new(),
            "key_auto_diagnostics",
            "req_max_prefix_seed",
        )
        .await;
        upstream.frame().await;
        for mut item in output {
            item.as_object_mut().unwrap().shift_remove("id");
            input.push(item);
        }
        if other {
            input[4096]["future"] = json!("changed");
        }
        input.push(json!({"role":"user","content":"next"}));
        let (context, trace) = traced_context("req_max_prefix_full");
        run_operation(
            &provider,
            operation(input.clone(), "max-prefix", Map::new()),
            context,
        )
        .await;
        let (connection, sent) = upstream.frame().await;
        assert_eq!(connection, 1);
        assert!(sent.get("previous_response_id").is_none());
        assert_eq!(sent["input"], json!(input));
        let metadata = prepared(&trace);
        assert_eq!(metadata[0]["deltaReason"], "prefix_changed");
        assert_eq!(
            metadata[0]["prefixFailure"],
            if other {
                "prior_output_other"
            } else {
                "prior_output_metadata_only"
            }
        );
        upstream.stop().await;
    }
}
