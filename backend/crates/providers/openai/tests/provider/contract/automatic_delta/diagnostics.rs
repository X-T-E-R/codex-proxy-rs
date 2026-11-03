//! 从真实 Provider 验证当前续接分型、纯元数据日志及业务冲突回退

use super::*;
use gateway_core::diagnostics::TraceContext;

pub(super) fn traced_context(id: &str) -> (AttemptContext, TraceContext) {
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

pub(super) fn prepared(trace: &TraceContext) -> Vec<Value> {
    trace.snapshot().unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["stage"] == "transport.payload.prepared")
        .map(|event| event["data"].clone())
        .collect()
}

#[tokio::test]
async fn native_claim_and_conflicts_emit_only_bounded_metadata() {
    let user = json!({"type":"message","role":"user","content":"PRIVATE_SEED"});
    let raw = json!({"type":"message","role":"assistant","id":"msg_native","status":"completed","content":[{"type":"output_text","text":"PRIVATE_ANSWER","annotations":[],"logprobs":[],"future":{"id":"PRIVATE_CONTENT_ID"}}],"future":9007199254740993_u64});
    let next = json!({"type":"message","role":"user","content":"PRIVATE_NEXT"});
    let input = vec![user.clone(), raw.clone(), next.clone()];
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(captured.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    for (case, expected_reason, category, claim) in [
        ("strict", "delta", None, None),
        ("metadata", "trusted_delta", None, Some("turn_boundary")),
        (
            "text",
            "prefix_changed",
            Some("prior_output_other"),
            Some("output_conflict"),
        ),
        (
            "unknown",
            "prefix_changed",
            Some("prior_output_other"),
            Some("output_conflict"),
        ),
        ("shorter", "prefix_changed", Some("input_shorter"), None),
        ("prior", "prefix_changed", Some("prior_input_changed"), None),
        ("property", "properties_changed", None, None),
        (
            "boundary",
            "prefix_changed",
            Some("prior_output_metadata_only"),
            Some("turn_boundary_missing"),
        ),
    ] {
        let mut current = input.clone();
        let mut options = Map::new();
        match case {
            "metadata" => {
                current[1]["id"] = json!("PRIVATE_REGENERATED_ID");
                current[1].as_object_mut().unwrap().shift_remove("status");
                current[1]["content"][0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("annotations");
                current[1]["content"][0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("logprobs");
            }
            "text" => current[1]["content"][0]["text"] = json!("PRIVATE_CHANGED_TEXT"),
            "unknown" => {
                current[1]["content"][0]["future"]["id"] = json!("PRIVATE_CHANGED_UNKNOWN")
            }
            "shorter" => current = vec![next.clone()],
            "prior" => current[0] = next.clone(),
            "property" => {
                options.insert("unknown_policy".to_owned(), json!("PRIVATE_OPTIONS"));
            }
            "boundary" => {
                current[1].as_object_mut().unwrap().shift_remove("id");
                current.pop();
            }
            _ => {}
        }
        let mut upstream = Upstream::start(vec![vec![raw.clone()]], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        run(
            &provider,
            vec![user.clone()],
            case,
            Map::new(),
            "key_auto_diagnostics",
            "req_prefix_seed",
        )
        .await;
        upstream.frame().await;
        let id = format!("req_prefix_{case}");
        let (context, trace) = traced_context(&id);
        run_operation(
            &provider,
            operation(current.clone(), case, options),
            context,
        )
        .await;
        let (_, frame) = upstream.frame().await;
        let automatic = matches!(expected_reason, "delta" | "trusted_delta");
        assert_eq!(
            frame.get("previous_response_id").is_some(),
            automatic,
            "{case}"
        );
        assert_eq!(
            frame["input"],
            if automatic {
                json!([next.clone()])
            } else {
                json!(current)
            },
            "{case}"
        );
        let metadata = prepared(&trace);
        let mut expected = json!({
            "fullPayloadBytes": metadata[0]["fullPayloadBytes"],
            "actualPayloadBytes": serde_json::to_vec(&frame).unwrap().len(),
            "automaticDelta": automatic, "deltaReason": expected_reason,
            "actualInputItems": frame["input"].as_array().unwrap().len(), "trustedTemplate": claim,
        });
        if let Some(category) = category {
            expected["prefixFailure"] = json!(category);
        }
        assert_eq!(metadata, vec![expected], "{case}");
        let logs = captured.json_events();
        let fields = logs
            .iter()
            .find_map(|event| {
                let fields = event["fields"].as_object()?;
                (fields.get("message") == Some(&json!("OpenAI upstream payload prepared"))
                    && fields.get("request_id") == Some(&json!(id)))
                .then_some(fields)
            })
            .unwrap();
        let mut expected_log = json!({"message":"OpenAI upstream payload prepared","request_id":id,
            "full_payload_bytes":metadata[0]["fullPayloadBytes"], "actual_payload_bytes":metadata[0]["actualPayloadBytes"],
            "automatic_delta":automatic,"delta_reason":expected_reason});
        if let Some(category) = category {
            expected_log["prefix_failure"] = json!(category);
        }
        if let Some(claim) = claim {
            expected_log["trusted_template"] = json!(claim);
        }
        assert_eq!(fields, expected_log.as_object().unwrap(), "{case}");
        upstream.stop().await;
    }
    let logs = serde_json::to_string(&captured.json_events()).unwrap();
    for private in [
        "PRIVATE_SEED",
        "PRIVATE_ANSWER",
        "PRIVATE_CHANGED_UNKNOWN",
        "PRIVATE_REGENERATED_ID",
    ] {
        assert!(
            !logs.contains(private),
            "prepared logs leaked fixture content"
        );
    }
}

#[tokio::test]
async fn content_edit_full_fallback_keeps_client_history_and_rebases_next_turn() {
    for large in [false, true] {
        let size = if large { 9 * 1024 * 1024 } else { 2800 * 1024 };
        let raw = json!({"type":"message","id":"msg_large","status":"completed","role":"assistant","content":[{"type":"output_text","text":"x".repeat(size),"annotations":[]}]});
        let user = json!({"role":"user","content":if large { "u".repeat(7 * 1024 * 1024) } else { "seed".to_owned() }});
        let mut upstream = Upstream::start(vec![vec![raw.clone()]], false).await;
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
        let mut changed = raw;
        changed["content"][0]["text"] = json!(format!("{}edited", "x".repeat(size)));
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
        let (_, sent) = upstream.frame().await;
        assert!(sent.get("previous_response_id").is_none());
        assert_eq!(sent["input"], json!(input));
        let metadata = prepared(&trace);
        assert_eq!(metadata[0]["deltaReason"], "prefix_changed");
        assert_eq!(metadata[0]["prefixFailure"], "prior_output_other");
        assert_eq!(
            metadata[0]["fullPayloadBytes"],
            metadata[0]["actualPayloadBytes"]
        );
        if !large {
            input.push(json!({"role":"user","content":"after full"}));
            run(
                &provider,
                input,
                "large-prefix",
                Map::new(),
                "key_auto_diagnostics",
                "req_prefix_after_full",
            )
            .await;
            let (_, delta) = upstream.frame().await;
            assert_eq!(delta["previous_response_id"], "resp_auto_1");
            assert_eq!(
                delta["input"],
                json!([{"role":"user","content":"after full"}])
            );
        }
        upstream.stop().await;
    }
}

#[tokio::test]
async fn maximum_item_claim_accepts_metadata_rebuild_but_rejects_late_unknown_conflict() {
    for conflict in [false, true] {
        let output: Vec<_> = (0..4096).map(|index| json!({"type":"message","id":format!("msg_{index}"),"role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"x".repeat(512),"annotations":[]}],"future":index})).collect();
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
        if conflict {
            input[4096]["future"] = json!("changed");
        }
        let next = json!({"role":"user","content":"next"});
        input.push(next.clone());
        run(
            &provider,
            input.clone(),
            "max-prefix",
            Map::new(),
            "key_auto_diagnostics",
            "req_max_prefix_full",
        )
        .await;
        let (_, sent) = upstream.frame().await;
        assert_eq!(sent.get("previous_response_id").is_some(), !conflict);
        assert_eq!(
            sent["input"],
            if conflict {
                json!(input)
            } else {
                json!([next])
            }
        );
        upstream.stop().await;
    }
}
