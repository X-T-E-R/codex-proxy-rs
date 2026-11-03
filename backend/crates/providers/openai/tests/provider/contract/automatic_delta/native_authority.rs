//! 真实 Provider 的多表示续接、工具映射及逐轮参数合同

use super::*;

fn user(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}

fn native(turn: usize) -> Vec<Value> {
    vec![
        json!({"type":"reasoning","id":"rs_native","status":"completed","summary":[{"type":"summary_text","text":"think"},{"type":"summary_text","text":"ing"}],"encrypted_content":"gAAA_native","signed":"native-reasoning"}),
        json!({"type":"message","role":"assistant","id":"msg_before","status":"completed","content":[{"type":"output_text","text":"before ","annotations":[{"type":"citation","opaque":"native"}],"logprobs":[{"token":"before"}]}],"signature":"native-message"}),
        json!({"type":"function_call","call_id":format!("native_{turn}_a"),"name":"inspect","arguments":"{ \"n\":9007199254740993 }","unknown":"retain"}),
        json!({"type":"message","role":"assistant","id":"msg_after","status":"completed","content":[{"type":"output_text","text":"after","annotations":[]}]}),
        json!({"type":"function_call","call_id":format!("native_{turn}_b"),"name":"read","arguments":"{}"}),
        json!({"type":"reasoning","summary":[],"encrypted_content":"gAAA_second","signed":"second"}),
        json!({"type":"future_item","opaque":{"signature":"keep-native-order"}}),
    ]
}

fn rebuilt(turn: usize, style: usize) -> Vec<Value> {
    let mut result = match style {
        0 => vec![json!({"role":"assistant","id":"client-generated","content":"before after"})],
        1 => vec![
            json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"be"},{"type":"output_text","text":"fore "}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"after"}]}),
        ],
        _ => vec![
            json!({"type":"message","role":"assistant","content":[{"type":"input_text","text":"before ","annotations":[]},{"type":"input_text","text":"after","logprobs":[]}]}),
        ],
    };
    if style != 2 {
        result.push(json!({"type":"reasoning","id":"client-reasoning","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"gAAA_native"}));
    }
    result.extend([
        json!({"type":"function_call","call_id":format!("client_{turn}_a"),"name":"inspect","arguments":"{ \"n\":9007199254740993 }"}),
        json!({"type":"function_call","call_id":format!("client_{turn}_b"),"name":"read","arguments":"{}"}),
    ]);
    result
}

fn tail(turn: usize, native_ids: bool) -> Vec<Value> {
    let prefix = if native_ids { "native" } else { "client" };
    vec![
        json!({"type":"function_call_output","call_id":format!("{prefix}_{turn}_b"),"output":"read result","extension":"retain-result"}),
        json!({"type":"function_call_output","call_id":format!("{prefix}_{turn}_a"),"output":"inspect result"}),
        user("next user"),
        json!({"type":"message","role":"assistant","content":"new assistant fragment"}),
    ]
}

#[tokio::test]
async fn canonical_variants_keep_three_turn_native_history_and_complete_mapped_tail() {
    for style in 0..3 {
        let mut upstream = Upstream::start(vec![native(0), native(1), native(2)], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("private seed")];
        for turn in 0..3 {
            if turn > 0 {
                input.extend(rebuilt(turn - 1, (style + turn - 1) % 3));
                input.extend(tail(turn - 1, false));
            }
            let (context, trace) =
                diagnostics::traced_context(&format!("req_native_{style}_{turn}"));
            run_operation(
                &provider,
                operation(input.clone(), "native-authority", Map::new()),
                context,
            )
            .await;
            let (connection, frame) = upstream.frame().await;
            assert_eq!(connection, 1);
            if turn == 0 {
                assert!(frame.get("previous_response_id").is_none());
            } else {
                assert_eq!(
                    frame["previous_response_id"],
                    format!("resp_auto_{}", turn - 1)
                );
                assert_eq!(
                    frame["input"],
                    json!(tail(turn - 1, true)),
                    "style={style}/turn={turn}"
                );
                assert_eq!(
                    diagnostics::prepared(&trace)[0]["deltaReason"],
                    "trusted_delta"
                );
                assert_eq!(
                    diagnostics::prepared(&trace)[0]["trustedTemplate"],
                    "turn_boundary"
                );
            }
        }
        upstream.stop().await;
    }
}

#[tokio::test]
async fn regenerated_tool_ids_restore_native_results_and_continue_after_reconnect() {
    let mut upstream = Upstream::start(vec![native(0), vec![], native(1)], true).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![user("private seed")];
    let mut full = input.clone();
    run(
        &provider,
        input.clone(),
        "native-recovery",
        Map::new(),
        "key_native",
        "req_native_recover_0",
    )
    .await;
    upstream.frame().await;
    for turn in 0..2 {
        input.extend(rebuilt(turn, turn));
        input.extend(tail(turn, false));
        full.extend(native(turn));
        full.extend(tail(turn, true));
        run(
            &provider,
            input.clone(),
            "native-recovery",
            Map::new(),
            "key_native",
            &format!("req_native_recover_{}", turn + 1),
        )
        .await;
        let (connection, delta) = upstream.frame().await;
        assert_eq!(connection, turn + 1);
        assert_eq!(delta["input"], json!(tail(turn, true)));
        let (connection, recovered) = upstream.frame().await;
        assert_eq!(connection, turn + 2);
        assert!(recovered.get("previous_response_id").is_none());
        let mut compatible = full.clone();
        for item in &mut compatible {
            if item["type"] == "reasoning" {
                item.as_object_mut().unwrap().shift_remove("status");
            }
        }
        assert_eq!(recovered["input"], json!(compatible));
    }
    upstream.stop().await;
}

#[tokio::test]
async fn current_generation_parameters_reach_delta_while_model_and_history_edits_rebuild() {
    let mut upstream = Upstream::start(vec![], false).await;
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let provider = provider_with_base_url(&store, upstream.base.clone());
    let mut input = vec![user("seed")];
    let options = [
        json!({"instructions":"first","tools":[{"type":"function","name":"inspect","parameters":{"type":"object"}}],"reasoning":{"effort":"low"}}),
        json!({"instructions":"second","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}],"tool_choice":"none","reasoning":{"effort":"high"},"text":{"format":{"type":"text"}},"parallel_tool_calls":false,"include":["reasoning.encrypted_content"]}),
        json!({"model":"gpt-5.2","instructions":"new model"}),
        json!({"model":"gpt-5.2","unknown_history_policy":"new"}),
        json!({"model":"gpt-5.2","unknown_history_policy":"new"}),
    ];
    for (turn, options) in options.into_iter().enumerate() {
        if turn == 4 {
            input[0] = user("edited seed");
        }
        let request = planned_request_for_model(
            "openai",
            operation(
                input.clone(),
                "native-properties",
                options.as_object().unwrap().clone(),
            ),
            options["model"].as_str().unwrap_or("gpt-5.4"),
        );
        let mut stream = Arc::clone(&provider)
            .execute(
                request,
                keyed_context(&format!("req_native_properties_{turn}"), "key_native"),
            )
            .await
            .unwrap();
        let mut completed = false;
        while let Some(event) = stream.next().await {
            completed |= event
                .unwrap()
                .wire_event()
                .is_some_and(|wire| wire.event_type() == Some("response.completed"));
        }
        assert!(completed);
        let (_, frame) = upstream.frame().await;
        if turn == 1 {
            assert_eq!(frame["previous_response_id"], "resp_auto_0");
            assert_eq!(frame["input"], json!([input.last().unwrap()]));
        } else {
            assert!(frame.get("previous_response_id").is_none(), "turn={turn}");
            assert_eq!(frame["input"], json!(input));
        }
        for (key, value) in options.as_object().unwrap() {
            assert_eq!(frame[key], *value, "turn={turn}/field={key}");
        }
        input.push(user(&format!("next {turn}")));
    }
    upstream.stop().await;
}

#[tokio::test]
async fn native_authority_rejects_explicit_content_signature_and_tool_conflicts() {
    for case in [
        "text",
        "signature",
        "ciphertext",
        "arguments",
        "tool_order",
        "orphan",
        "duplicate",
        "missing",
        "opaque_duplicate",
        "opaque_order",
        "ambiguous_tools",
        "annotation",
        "unknown_annotations",
        "compaction",
        "prior_user",
        "fork",
    ] {
        let mut original = native(0);
        if case == "opaque_order" {
            original.push(json!({"type":"other_future_item","opaque":"second"}));
        }
        if case == "ambiguous_tools" {
            original[4]["name"] = original[2]["name"].clone();
            original[4]["arguments"] = original[2]["arguments"].clone();
        }
        let mut upstream = Upstream::start(vec![original.clone()], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("seed")];
        run(
            &provider,
            input.clone(),
            "native-negative",
            Map::new(),
            "key_native",
            "req_native_negative_0",
        )
        .await;
        upstream.frame().await;
        let mut output = rebuilt(0, 0);
        let mut next = tail(0, false);
        match case {
            "text" => output[0]["content"] = json!("edited answer"),
            "signature" => output[0]["signature"] = json!("changed-signature"),
            "ciphertext" => output[1]["encrypted_content"] = json!("changed-ciphertext"),
            "arguments" => output[2]["arguments"] = json!("{\"n\":9007199254740992}"),
            "tool_order" => output.swap(2, 3),
            "orphan" => next[0]["call_id"] = json!("orphan"),
            "duplicate" => next.insert(1, next[0].clone()),
            "missing" => {
                next.remove(0);
            }
            "opaque_duplicate" => {
                output.extend([original[6].clone(), original[6].clone()]);
            }
            "opaque_order" => {
                output.extend([original[7].clone(), original[6].clone()]);
            }
            "ambiguous_tools" => {
                output[3]["name"] = output[2]["name"].clone();
                output[3]["arguments"] = output[2]["arguments"].clone();
            }
            "annotation" => {
                output[0]["content"] = json!([{"type":"output_text","text":"before after","annotations":[{"opaque":"edited"}]}]);
            }
            "unknown_annotations" => output[0]["annotations"] = json!([]),
            "prior_user" => input[0] = user("changed seed"),
            _ => {}
        }
        input.extend(output);
        input.extend(next);
        if case == "compaction" {
            input = vec![
                json!({"type":"compaction","encrypted_content":"new-window"}),
                user("next"),
            ];
        }
        run(
            &provider,
            input.clone(),
            if case == "fork" {
                "other-session"
            } else {
                "native-negative"
            },
            Map::new(),
            "key_native",
            "req_native_negative_1",
        )
        .await;
        let (_, frame) = upstream.frame().await;
        assert!(frame.get("previous_response_id").is_none(), "{case}");
        assert_eq!(frame["input"], json!(input), "{case}");
        upstream.stop().await;
    }
}

#[tokio::test]
async fn historical_client_and_native_call_ids_cannot_be_reused_in_new_tail() {
    for reused in ["client_0_a", "native_0_a"] {
        let mut upstream = Upstream::start(vec![native(0), native(1)], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("seed")];
        for turn in 0..3 {
            if turn > 0 {
                input.extend(rebuilt(turn - 1, 0));
                input.extend(tail(turn - 1, false));
            }
            if turn == 2 {
                input.extend([
                    json!({"type":"function_call","call_id":reused,"name":"inspect","arguments":"{}"}),
                    json!({"type":"function_call_output","call_id":reused,"output":"new result"}),
                ]);
            }
            run(
                &provider,
                input.clone(),
                "historical-call",
                Map::new(),
                "key_native",
                &format!("req_historical_call_{turn}"),
            )
            .await;
            let (_, frame) = upstream.frame().await;
            if turn == 1 {
                assert_eq!(frame["previous_response_id"], "resp_auto_0");
                assert_eq!(frame["input"], json!(tail(0, true)));
            } else {
                assert!(
                    frame.get("previous_response_id").is_none(),
                    "{reused}/turn={turn}"
                );
                assert_eq!(frame["input"], json!(input));
            }
        }
        upstream.stop().await;
    }
}
