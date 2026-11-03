//! 真实 Provider 的 custom 工具关联、原生恢复与多 reasoning 单项展示认领

use super::*;

fn user(text: &str) -> Value {
    json!({"type":"message","role":"user","content":text})
}

fn message() -> Value {
    json!({"type":"message","id":"msg_native","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]})
}

fn canonical_message() -> Value {
    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]})
}

fn custom(id: &str, input: &str) -> Value {
    json!({"type":"custom_tool_call","id":format!("item_{id}"),"status":"completed","call_id":id,"name":"apply_patch","namespace":"filesystem","input":input,"signature":"native-custom"})
}

fn result(id: &str) -> Value {
    json!({"type":"custom_tool_call_output","call_id":id,"name":"apply_patch","output":[{"type":"input_text","text":"Success"}],"extension":{"retained":true}})
}

#[tokio::test]
async fn custom_result_boundaries_map_actual_tail_and_restore_native_history() {
    for mapped in [false, true] {
        for append_user in [false, true] {
            let call = custom("custom_native", "*** Begin Patch\n*** End Patch");
            let output = vec![message(), call.clone()];
            let mut upstream = Upstream::start(vec![output.clone()], true).await;
            let store = Arc::new(MemoryAccountStore::default());
            create_account(&store, "acct_provider_contract").await;
            let provider = provider_with_base_url(&store, upstream.base.clone());
            let mut input = vec![user("seed")];
            run(
                &provider,
                input.clone(),
                "custom-recovery",
                Map::new(),
                "key_known",
                "req_custom_0",
            )
            .await;
            upstream.frame().await;
            let mut rebuilt = call;
            let alias = if mapped {
                "custom_client"
            } else {
                "custom_native"
            };
            rebuilt["call_id"] = json!(alias);
            let mut client_result = result(alias);
            let mut native_result = result("custom_native");
            if !mapped {
                // 官方执行器省略可选 name，带 name 时同样核实调用关联
                client_result.as_object_mut().unwrap().shift_remove("name");
                native_result.as_object_mut().unwrap().shift_remove("name");
            }
            input.extend([canonical_message(), rebuilt, client_result]);
            let mut native_tail = vec![native_result];
            if append_user {
                input.push(user("next"));
                native_tail.push(user("next"));
            }
            input.push(json!({"role":"assistant","content":"new fragment"}));
            native_tail.push(json!({"role":"assistant","content":"new fragment"}));
            run(
                &provider,
                input.clone(),
                "custom-recovery",
                Map::new(),
                "key_known",
                "req_custom_1",
            )
            .await;
            let (connection, delta) = upstream.frame().await;
            assert_eq!(connection, 1);
            assert_eq!(delta["previous_response_id"], "resp_auto_0");
            assert_eq!(delta["input"], json!(native_tail));
            let mut native_full = vec![user("seed")];
            native_full.extend(output);
            native_full.extend(native_tail);
            let (connection, full) = upstream.frame().await;
            assert_eq!(connection, 2);
            assert!(full.get("previous_response_id").is_none());
            assert_eq!(full["input"], json!(native_full));
            input.push(user("third"));
            native_full.push(user("third"));
            run(
                &provider,
                input,
                "custom-recovery",
                Map::new(),
                "key_known",
                "req_custom_2",
            )
            .await;
            let (connection, delta) = upstream.frame().await;
            assert_eq!(connection, 2);
            assert_eq!(delta["previous_response_id"], "resp_auto_2");
            assert_eq!(delta["input"], json!([user("third")]));
            let (connection, full) = upstream.frame().await;
            assert_eq!(connection, 3);
            assert_eq!(full["input"], json!(native_full));
            upstream.stop().await;
        }
    }
}

#[tokio::test]
async fn mixed_tool_categories_and_custom_conflicts_keep_associations() {
    for case in [
        "valid",
        "input",
        "name",
        "category",
        "result_category",
        "result_name",
        "namespace",
        "order",
        "orphan",
        "duplicate",
        "missing",
        "ambiguous",
        "history_alias",
        "history_native",
    ] {
        let mut output = vec![
            message(),
            custom("custom_native", "patch A"),
            json!({"type":"function_call","call_id":"function_native","name":"inspect","arguments":"{ \"n\":9007199254740993 }"}),
            custom("custom_second", "patch B"),
        ];
        if case == "ambiguous" {
            output[3]["input"] = output[1]["input"].clone();
        }
        let mut upstream = Upstream::start(vec![output.clone(), output.clone()], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("seed")];
        run(
            &provider,
            input.clone(),
            "mixed-tools",
            Map::new(),
            "key_known",
            "req_mixed_0",
        )
        .await;
        upstream.frame().await;
        let mut rebuilt = output;
        rebuilt[0] = canonical_message();
        rebuilt[1]["call_id"] = json!("custom_client");
        rebuilt[2]["call_id"] = json!("function_client");
        rebuilt[3]["call_id"] = json!("second_client");
        let mut tail = vec![
            result("second_client"),
            json!({"type":"function_call_output","call_id":"function_client","output":"inspected"}),
            result("custom_client"),
            user("next"),
        ];
        match case {
            "input" => rebuilt[1]["input"] = json!("edited patch"),
            "name" => rebuilt[1]["name"] = json!("other"),
            "category" => {
                rebuilt[1]["type"] = json!("function_call");
                rebuilt[1]["arguments"] = rebuilt[1]["input"].clone();
                rebuilt[1].as_object_mut().unwrap().shift_remove("input");
            }
            "result_category" => tail[2]["type"] = json!("function_call_output"),
            "result_name" => tail[2]["name"] = json!("other"),
            "namespace" => rebuilt[1]["namespace"] = json!("other"),
            "order" => rebuilt.swap(1, 2),
            "orphan" => tail[2]["call_id"] = json!("orphan"),
            "duplicate" => tail.insert(1, tail[0].clone()),
            "missing" => {
                tail.remove(0);
            }
            _ => {}
        }
        input.extend(rebuilt);
        input.extend(tail.clone());
        run(
            &provider,
            input.clone(),
            "mixed-tools",
            Map::new(),
            "key_known",
            "req_mixed_1",
        )
        .await;
        let (_, frame) = upstream.frame().await;
        if matches!(case, "valid" | "history_alias" | "history_native") {
            tail[0]["call_id"] = json!("custom_second");
            tail[1]["call_id"] = json!("function_native");
            tail[2]["call_id"] = json!("custom_native");
            assert_eq!(frame["previous_response_id"], "resp_auto_0", "{case}");
            assert_eq!(frame["input"], json!(tail), "{case}");
        } else {
            assert!(frame.get("previous_response_id").is_none(), "{case}");
            assert_eq!(frame["input"], json!(input), "{case}");
        }
        if matches!(case, "history_alias" | "history_native") {
            // 上轮原生 patch 与客户端前缀中的两套历史 ID 都不可被新增调用复用
            let mut next = vec![
                message(),
                custom("custom_native", "patch A"),
                json!({"type":"function_call","call_id":"function_native","name":"inspect","arguments":"{ \"n\":9007199254740993 }"}),
                custom("custom_second", "patch B"),
            ];
            next[0] = canonical_message();
            let reused = if case == "history_alias" {
                "custom_client"
            } else {
                "custom_native"
            };
            input.extend(next);
            input.extend([
                result("custom_native"),
                json!({"type":"function_call_output","call_id":"function_native","output":"ok"}),
                result("custom_second"),
                user("third"),
                custom(reused, "new patch"),
                result(reused),
            ]);
            run(
                &provider,
                input.clone(),
                "mixed-tools",
                Map::new(),
                "key_known",
                "req_mixed_2",
            )
            .await;
            let (_, frame) = upstream.frame().await;
            assert!(frame.get("previous_response_id").is_none(), "{case}");
            assert_eq!(frame["input"], json!(input), "{case}");
        }
        upstream.stop().await;
    }
}

fn reasoning_output() -> Vec<Value> {
    vec![
        json!({"type":"reasoning","id":"rs_first","summary":[{"type":"summary_text","text":"first summary"}],"content":[{"type":"reasoning_text","text":"first content"}],"encrypted_content":"gAAA_first","signature":"first-signature"}),
        json!({"type":"reasoning","id":"rs_second","summary":[{"type":"summary_text","text":"second summary"}],"content":[{"type":"reasoning_text","text":"second content"}],"encrypted_content":"gAAA_second","signature":"second-signature"}),
        message(),
    ]
}

#[tokio::test]
async fn partial_reasoning_display_restores_each_native_item_and_continues() {
    for style in [
        "both",
        "none",
        "second_only",
        "first_display_omitted",
        "regenerated_id",
        "split",
    ] {
        let output = reasoning_output();
        let mut upstream = Upstream::start(vec![output.clone()], true).await;
        let store = Arc::new(MemoryAccountStore::default());
        store
            .seed_api_key(
                "acct_provider_contract",
                upstream.base.clone(),
                provider_openai::credential::ResponsesTransport::PreferWebsocket,
            )
            .await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("seed")];
        run(
            &provider,
            input.clone(),
            "reasoning-items",
            Map::new(),
            "key_known",
            "req_reasoning_items_0",
        )
        .await;
        upstream.frame().await;
        let mut reasoning = match style {
            "both" | "first_display_omitted" => output[..2].to_vec(),
            "none" => vec![],
            _ => vec![output[1].clone()],
        };
        match style {
            "first_display_omitted" => {
                reasoning[0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("summary");
                reasoning[0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("content");
            }
            "regenerated_id" => reasoning[0]["id"] = json!("client-reasoning"),
            "split" => {
                reasoning[0]["summary"] = json!([{"type":"summary_text","text":"second "},{"type":"summary_text","text":"summary"}]);
                reasoning[0]["content"] = json!([{"type":"reasoning_text","text":"second "},{"type":"reasoning_text","text":"content"}]);
            }
            _ => {}
        }
        input.extend(reasoning);
        input.extend([canonical_message(), user("next")]);
        run(
            &provider,
            input.clone(),
            "reasoning-items",
            Map::new(),
            "key_known",
            "req_reasoning_items_1",
        )
        .await;
        let (_, delta) = upstream.frame().await;
        assert_eq!(delta["previous_response_id"], "resp_auto_0", "{style}");
        assert_eq!(delta["input"], json!([user("next")]), "{style}");
        let mut native = vec![user("seed")];
        native.extend(output);
        native.push(user("next"));
        let (_, full) = upstream.frame().await;
        assert!(full.get("previous_response_id").is_none());
        assert_eq!(full["input"], json!(native), "{style}");
        input.push(user("third"));
        native.push(user("third"));
        run(
            &provider,
            input,
            "reasoning-items",
            Map::new(),
            "key_known",
            "req_reasoning_items_2",
        )
        .await;
        let (_, delta) = upstream.frame().await;
        assert_eq!(delta["previous_response_id"], "resp_auto_2");
        assert_eq!(delta["input"], json!([user("third")]));
        let (_, full) = upstream.frame().await;
        assert_eq!(full["input"], json!(native));
        upstream.stop().await;
    }
}

#[tokio::test]
async fn explicit_reasoning_item_edits_and_cross_item_substrings_rebuild() {
    for case in [
        "summary",
        "content",
        "ciphertext",
        "signature",
        "known_id",
        "reorder",
        "duplicate",
        "combined_summary",
        "substring",
    ] {
        let output = reasoning_output();
        let mut upstream = Upstream::start(vec![output.clone()], false).await;
        let store = Arc::new(MemoryAccountStore::default());
        store
            .seed_api_key(
                "acct_provider_contract",
                upstream.base.clone(),
                provider_openai::credential::ResponsesTransport::PreferWebsocket,
            )
            .await;
        let provider = provider_with_base_url(&store, upstream.base.clone());
        let mut input = vec![user("seed")];
        run(
            &provider,
            input.clone(),
            "reasoning-conflict",
            Map::new(),
            "key_known",
            "req_reasoning_conflict_0",
        )
        .await;
        upstream.frame().await;
        let mut rebuilt = vec![output[1].clone()];
        match case {
            "summary" => rebuilt[0]["summary"][0]["text"] = json!("edited summary"),
            "content" => rebuilt[0]["content"][0]["text"] = json!("edited content"),
            "ciphertext" => rebuilt[0]["encrypted_content"] = json!("gAAA_changed"),
            "signature" => rebuilt[0]["signature"] = json!("changed"),
            "known_id" => rebuilt[0]["id"] = json!("rs_first"),
            "reorder" => rebuilt.push(output[0].clone()),
            "duplicate" => rebuilt.push(rebuilt[0].clone()),
            "combined_summary" => {
                rebuilt[0]["summary"][0]["text"] = json!("first summarysecond summary")
            }
            "substring" => rebuilt[0]["summary"][0]["text"] = json!("summary"),
            _ => unreachable!(),
        }
        input.extend(rebuilt);
        input.extend([canonical_message(), user("next")]);
        run(
            &provider,
            input.clone(),
            "reasoning-conflict",
            Map::new(),
            "key_known",
            "req_reasoning_conflict_1",
        )
        .await;
        let (_, full) = upstream.frame().await;
        assert!(full.get("previous_response_id").is_none(), "{case}");
        assert_eq!(full["input"], json!(input), "{case}");
        upstream.stop().await;
    }
}
