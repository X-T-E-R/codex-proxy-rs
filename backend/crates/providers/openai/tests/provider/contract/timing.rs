//! 验证 SSE 与 WebSocket 的首字边界、请求级计时原点与独立上游耗时

use std::time::Instant;

use gateway_core::engine::provider::ProviderStream;
use gateway_core::event::ProviderResponseTimings;

use super::*;

fn timed_context(started_at: Instant, attempt: u32) -> AttemptContext {
    AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_output_timing").unwrap(),
            ClientApiKeyId::new("key_openai_contract").unwrap(),
        )
        .with_timing_started_at(started_at),
        NonZeroU32::new(attempt).unwrap(),
        SystemTime::now() + Duration::from_secs(30),
        account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None)
            .with_account_scope(contract_account_scope()),
        None,
        CancellationToken::new(),
    )
}

async fn first_upstream_timings(stream: &mut ProviderStream) -> ProviderResponseTimings {
    timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let Some(observation) = event.unwrap().response_observation()
                && observation.timings().first_event_ms.is_some()
            {
                return observation.timings();
            }
        }
        panic!("missing upstream timing observation");
    })
    .await
    .expect("upstream timing before stream completion")
}

async fn finish_timings(
    stream: &mut ProviderStream,
    mut timings: ProviderResponseTimings,
) -> ProviderResponseTimings {
    timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let Some(observation) = event.unwrap().response_observation() {
                timings = observation.timings();
            }
        }
        timings
    })
    .await
    .expect("complete upstream stream")
}

async fn observe_timing_events(websocket: bool, events: Vec<Value>) -> ProviderResponseTimings {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let (base_url, server) = if websocket {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = crate::transport::accept_codex_test_websocket_with(
                socket,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert!(
                        !request
                            .headers()
                            .contains_key("x-responsesapi-include-timing-metrics")
                    );
                    response.headers_mut().insert(
                        "sec-websocket-extensions",
                        "permessage-deflate".parse().unwrap(),
                    );
                },
            )
            .await;
            ws.next().await.unwrap().unwrap();
            for event in events {
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        (base_url, server)
    } else {
        let (base_url, release, _, server) = paused_chunked_sse_server(
            events[..1]
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
            events[1..]
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
        )
        .await;
        release.send(()).unwrap();
        (base_url, server)
    };
    let operation = if websocket {
        generate_operation()
    } else {
        http_generate_operation()
    };
    let mut stream = provider_with_base_url(&store, base_url)
        .execute(
            planned_request("openai", operation),
            timed_context(Instant::now() - Duration::from_secs(20), 2),
        )
        .await
        .unwrap();
    let timings = finish_timings(&mut stream, ProviderResponseTimings::default()).await;
    server.await.unwrap();
    timings
}

#[tokio::test]
async fn official_response_duration_is_observed_independently_of_the_request_clock() {
    for websocket in [false, true] {
        let created = json!({"type":"response.created","response":{"id":"resp_official_time","created_at":100}});
        let completed = json!({"type":"response.completed","response":{"id":"resp_official_time","status":"completed","created_at":100,"completed_at":107,"output":[],"usage":{"input_tokens":1,"output_tokens":112,"total_tokens":113}}});
        let metrics = json!({"type":"responsesapi.websocket_timing","timing_metrics":{
            "responses_duration_excl_engine_and_client_tool_time_ms":120.25,
            "engine_service_total_ms":6400.0,
        }});
        let wrong_id = json!({"type":"responsesapi.websocket_timing","response_id":"resp_other","timing_metrics":{"engine_service_total_ms":999.0}});
        let invalid = json!({"type":"responsesapi.websocket_timing","timing_metrics":{
            "responses_duration_excl_engine_and_client_tool_time_ms":null,
            "engine_service_total_ms":"100",
        }});
        let stale = json!({"type":"responsesapi.websocket_timing","timing_metrics":{"engine_service_total_ms":999.0}});
        let events = [stale, created, metrics, wrong_id, invalid, completed];
        let timings = observe_timing_events(websocket, events.to_vec()).await;
        assert_eq!(timings.upstream_response_ms, Some(7_000));
        assert_eq!(timings.upstream_api_overhead_ms, Some(120.25));
        assert_eq!(timings.upstream_engine_ms, Some(6400.0));
        assert!(timings.first_event_ms.is_some_and(|value| value >= 20_000));
    }
}

// 保留实测计时帧的层级和小数，响应身份使用虚构值，不包含请求正文
fn response_critical_path_event() -> Value {
    json!({"type":"responsesapi.websocket_timing","timing_metrics":{
        "timing_scope":"logical_turn",
        "response_id":"resp_critical_path",
        "pre_inference_ms":211.691468,
        "client_tool_pause_total_ms":3730.088369,
        "total_turn_time_s":25.017284596,
        "num_engine_calls":4,
        "first_sampled_message_ttft_ms":null,
        "critical_path":{
            "scope":"response",
            "coverage":"complete",
            "boundary_type":"actionable_output_item_done",
            "num_engine_calls":1,
            "responses_harness_unblocked_ms":6792.094664,
            "responses_pre_inference_ms":35.747883,
            "engine_wall_ms":6710.68507,
            "client_tool_pause_ms":0.0,
            "responses_other_ms":45.66171099999974,
            "engine_iapi_total_ms":null,
            "taas_request_setup_total_ms":null,
            "taas_provider_wait_total_ms":null,
            "sampling_and_stream_total_ms":null,
            "inference_residual_total_ms":null
        }
    }})
}

fn critical_path_response_events(metrics: Value) -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":"resp_critical_path"}}),
        metrics,
        json!({"type":"response.completed","response":{"id":"resp_critical_path","status":"completed","created_at":100,"completed_at":107,"output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ]
}

#[tokio::test]
async fn response_critical_path_is_observed_without_logical_turn_totals_on_both_transports() {
    for websocket in [false, true] {
        let mut metrics = response_critical_path_event();
        // 同帧即使携带整轮专项值，也不能与响应级 critical path 混算
        metrics["timing_metrics"]["engine_service_total_ms"] = json!(25_000);
        let mut events = critical_path_response_events(metrics);
        let mut wrong_id = response_critical_path_event();
        wrong_id["timing_metrics"]["response_id"] = json!("resp_other");
        wrong_id["timing_metrics"]["critical_path"]["engine_wall_ms"] = json!(999);
        events.insert(2, wrong_id);
        let timings = observe_timing_events(websocket, events).await;
        assert_eq!(timings.upstream_engine_ms, Some(6710.68507));
        assert!((timings.upstream_api_overhead_ms.unwrap() - 81.409594).abs() < 0.000001);
        assert_eq!(timings.upstream_response_ms, Some(7000));
    }
}

#[tokio::test]
async fn critical_path_rejects_mismatched_identity_and_incomplete_or_cumulative_scope() {
    for (pointer, value) in [
        ("/timing_metrics/response_id", json!("resp_other")),
        ("/timing_metrics/response_id", Value::Null),
        ("/timing_metrics/critical_path/scope", json!("logical_turn")),
        ("/timing_metrics/critical_path/scope", Value::Null),
        ("/timing_metrics/critical_path/coverage", json!("partial")),
        ("/timing_metrics/critical_path/coverage", Value::Null),
        (
            "/timing_metrics/critical_path/boundary_type",
            json!("unknown"),
        ),
        ("/timing_metrics/critical_path", Value::Null),
    ] {
        let mut metrics = response_critical_path_event();
        // 正确的顶层 ID 不能掩盖嵌套 ID 的冲突
        metrics["response_id"] = json!("resp_critical_path");
        *metrics.pointer_mut(pointer).unwrap() = value;
        let timings = observe_timing_events(false, critical_path_response_events(metrics)).await;
        assert_eq!(timings.upstream_engine_ms, None, "{pointer}");
        assert_eq!(timings.upstream_api_overhead_ms, None, "{pointer}");
    }
    for identity in ["response_id", "response"] {
        let mut metrics = response_critical_path_event();
        metrics[identity] = if identity == "response" {
            json!({"id":"resp_other"})
        } else {
            json!("resp_other")
        };
        let timings = observe_timing_events(false, critical_path_response_events(metrics)).await;
        assert_eq!(timings.upstream_engine_ms, None);
    }
    let metrics = json!({"type":"responsesapi.websocket_timing","timing_metrics":{
        "timing_scope":"logical_turn",
        "engine_service_total_ms":25_000,
        "responses_duration_excl_engine_and_client_tool_time_ms":500,
    }});
    let timings = observe_timing_events(false, critical_path_response_events(metrics)).await;
    assert_eq!(timings.upstream_engine_ms, None);
    assert_eq!(timings.upstream_api_overhead_ms, None);
}

#[tokio::test]
async fn critical_path_keeps_missing_or_invalid_components_unknown_and_preserves_zero() {
    for value in [Value::Null, json!(-1), json!("45"), json!(1e99)] {
        let mut metrics = response_critical_path_event();
        metrics["timing_metrics"]["critical_path"]["responses_other_ms"] = value;
        let timings = observe_timing_events(false, critical_path_response_events(metrics)).await;
        assert_eq!(timings.upstream_api_overhead_ms, None);
        assert_eq!(timings.upstream_engine_ms, Some(6710.68507));
    }
    for (component, expected) in [(0.0, Some(0.0)), (5e18, None)] {
        let mut metrics = response_critical_path_event();
        let path = &mut metrics["timing_metrics"]["critical_path"];
        path["responses_other_ms"] = json!(component);
        path["responses_pre_inference_ms"] = json!(component);
        path["engine_wall_ms"] = json!(0);
        let timings = observe_timing_events(false, critical_path_response_events(metrics)).await;
        assert_eq!(timings.upstream_api_overhead_ms, expected);
        assert_eq!(timings.upstream_engine_ms, Some(0.0));
    }
}

#[tokio::test]
async fn timing_frames_outside_the_active_response_do_not_enter_request_metrics() {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let timing = json!({"type":"responsesapi.websocket_timing","timing_metrics":{
        "responses_duration_excl_engine_and_client_tool_time_ms":0,
        "engine_service_total_ms":450,
    }});
    let created = json!({"type":"response.created","response":{"id":"resp_boundary"}});
    let completed = json!({"type":"response.completed","response":{"id":"resp_boundary","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}});
    let (base_url, release, _, server) = paused_chunked_sse_server(
        format!("data: {timing}\n\ndata: {created}\n\n"),
        format!("data: {completed}\n\ndata: {timing}\n\n"),
    )
    .await;
    release.send(()).unwrap();
    let mut stream = provider_with_base_url(&store, base_url)
        .execute(
            planned_request("openai", http_generate_operation()),
            timed_context(Instant::now(), 1),
        )
        .await
        .unwrap();
    let timings = finish_timings(&mut stream, ProviderResponseTimings::default()).await;
    assert_eq!(timings.upstream_api_overhead_ms, None);
    assert_eq!(timings.upstream_engine_ms, None);
    server.await.unwrap();
}

#[tokio::test]
async fn sse_keepalive_and_empty_data_do_not_start_ttft() {
    let store = Arc::new(MemoryAccountStore::default());
    create_account(&store, "acct_provider_contract").await;
    let (base_url, release, _, server) = paused_chunked_sse_server(
        ": ping\n\nevent: response.created\n\ndata: \t\n\ndata: [DONE]\n\n".to_owned(),
        CAPTURE_COMPLETED_SSE.to_owned(),
    )
    .await;
    let mut stream = provider_with_base_url(&store, base_url)
        .execute(
            planned_request("openai", http_generate_operation()),
            timed_context(Instant::now() - Duration::from_secs(3), 1),
        )
        .await
        .unwrap();
    let first = first_upstream_timings(&mut stream).await;
    assert_eq!(first.first_token_ms, None);
    release.send(()).unwrap();
    let final_timings = finish_timings(&mut stream, first).await;
    assert!(
        final_timings
            .first_token_ms
            .is_some_and(|value| value >= 3_000)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn first_response_uses_request_clock_on_every_attempt() {
    for attempt in [1, 2] {
        let store = Arc::new(MemoryAccountStore::default());
        create_account(&store, "acct_provider_contract").await;
        let first_chunk = concat!(
            r#"data: {"type":"response.created","response":{"id":"resp_scope_capture","model":"gpt-5.4"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hello"}"#,
            "\n\n",
        )
        .to_owned();
        let (base_url, release, _, server) =
            paused_chunked_sse_server(first_chunk, CAPTURE_COMPLETED_SSE.to_owned()).await;
        // 用已有请求原点模拟选号或之前尝试的等待，不依赖 sleep 的调度精度
        let started_at = Instant::now() - Duration::from_secs(3);
        let mut stream = provider_with_base_url(&store, base_url)
            .execute(
                planned_request("openai", http_generate_operation()),
                timed_context(started_at, attempt),
            )
            .await
            .unwrap();
        let first = first_upstream_timings(&mut stream).await;
        assert!(first.first_event_ms.is_some_and(|value| value >= 3_000));
        assert!(first.first_token_ms.is_some_and(|value| value >= 3_000));
        assert!(first.first_text_ms.is_some_and(|value| value >= 3_000));
        release.send(()).unwrap();
        let final_timings = finish_timings(&mut stream, first).await;
        assert_eq!(final_timings.first_token_ms, first.first_token_ms);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn created_response_starts_ttft_before_output_on_http_and_websocket() {
    for websocket in [false, true] {
        for (output, has_content) in [
            (
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hello"}),
                true,
            ),
            (
                json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"thinking"}),
                true,
            ),
            (
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}),
                true,
            ),
            (
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":""}),
                false,
            ),
            (
                json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":""}),
                false,
            ),
        ] {
            let store = Arc::new(MemoryAccountStore::default());
            create_account(&store, "acct_provider_contract").await;
            let is_text = output["type"] == "response.output_text.delta";
            let is_reasoning = output["type"] == "response.reasoning_summary_text.delta";
            let created = json!({"type":"response.created","response":{"id":"resp_timing","model":"gpt-5.4"}});
            let added = json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","content":[]}});
            let completed = json!({"type":"response.completed","response":{"id":"resp_timing","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}});
            let (base_url, release, server) = if websocket {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base_url = format!("http://{}", listener.local_addr().unwrap());
                let (release, released) = oneshot::channel();
                let server = tokio::spawn(async move {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut ws = crate::transport::accept_codex_test_websocket_with(
                        socket,
                        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                            assert!(
                                !request
                                    .headers()
                                    .contains_key("x-responsesapi-include-timing-metrics")
                            );
                            response.headers_mut().insert(
                                "sec-websocket-extensions",
                                "permessage-deflate".parse().unwrap(),
                            );
                        },
                    )
                    .await;
                    ws.next().await.unwrap().unwrap();
                    ws.send(Message::Text(created.to_string().into()))
                        .await
                        .unwrap();
                    released.await.unwrap();
                    for event in [added, output, completed] {
                        ws.send(Message::Text(event.to_string().into()))
                            .await
                            .unwrap();
                    }
                });
                (base_url, release, server)
            } else {
                let (base_url, release, _, server) = paused_chunked_sse_server(
                    format!("data: {created}\n\n"),
                    format!("data: {added}\n\ndata: {output}\n\ndata: {completed}\n\n"),
                )
                .await;
                (base_url, release, server)
            };
            let operation = if websocket {
                generate_operation()
            } else {
                http_generate_operation()
            };
            let mut stream = provider_with_base_url(&store, base_url)
                .execute(
                    planned_request("openai", operation),
                    timed_context(Instant::now() - Duration::from_secs(3), 2),
                )
                .await
                .unwrap();
            let first = timeout(Duration::from_secs(5), async {
                while let Some(event) = stream.next().await {
                    if let Some(observation) = event.unwrap().response_observation()
                        && observation.timings().first_token_ms.is_some()
                    {
                        return observation.timings();
                    }
                }
                panic!("missing first-response observation");
            })
            .await
            .expect("first response before releasing output");
            assert!(first.first_token_ms.is_some_and(|value| value >= 3_000));
            assert_eq!(first.first_text_ms, None);
            assert_eq!(first.first_reasoning_ms, None);
            release.send(()).unwrap();
            let final_timings = finish_timings(&mut stream, first).await;
            assert_eq!(final_timings.first_token_ms, first.first_token_ms);
            assert_eq!(
                final_timings.first_text_ms.is_some(),
                has_content && is_text
            );
            assert_eq!(
                final_timings.first_reasoning_ms.is_some(),
                has_content && is_reasoning
            );
            server.await.unwrap();
        }
    }
}

fn automatic_timing_operation(next_turn: bool) -> Operation {
    let mut input = vec![json!({"role":"user","content":"seed"})];
    if next_turn {
        input.push(json!({"role":"user","content":"next"}));
    }
    Operation::Generate(
        GenerateRequest::from_protocol_payload(
            ProtocolPayload::json_object(
                "openai",
                json!({"model":"gpt-5.4","session_id":"auto-timing","input":input,"store":false,"stream":true})
                    .as_object().unwrap().clone(),
            ).unwrap(),
        ).with_provider_session_state(
            ProviderSessionState::new(
                "openai",
                json!({"account_id":"acct_provider_contract","conversation_id":"auto-timing","continuation_scope":"connection_local"})
                    .as_object().unwrap().clone(),
            ).unwrap(),
        ),
    )
}

#[tokio::test]
async fn automatic_delta_keeps_received_ttft_and_discards_rejected_attempt_timing_and_identity() {
    for reject_history in [false, true] {
        for reasoning in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let (prelude_sent, prelude_received) = oneshot::channel();
            let (release_prelude, prelude_released) = oneshot::channel();
            let (recovery_sent, recovery_received) = oneshot::channel();
            let (release_output, output_released) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = accept_codex_test_websocket(socket).await;
                let seed: Value = serde_json::from_str(
                    ws.next()
                        .await
                        .unwrap()
                        .unwrap()
                        .into_text()
                        .unwrap()
                        .as_str(),
                )
                .unwrap();
                assert!(seed.get("previous_response_id").is_none());
                ws.send(Message::Text(json!({"type":"response.completed","response":{"id":"resp_timing_seed","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}}).to_string().into())).await.unwrap();
                let delta: Value = serde_json::from_str(
                    ws.next()
                        .await
                        .unwrap()
                        .unwrap()
                        .into_text()
                        .unwrap()
                        .as_str(),
                )
                .unwrap();
                assert_eq!(delta["previous_response_id"], "resp_timing_seed");
                assert_eq!(delta["input"], json!([{"role":"user","content":"next"}]));
                let id = if reject_history {
                    "resp_timing_rejected"
                } else {
                    "resp_timing_success"
                };
                ws.send(Message::Text(json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}).to_string().into())).await.unwrap();
                prelude_sent.send(()).unwrap();
                prelude_released.await.unwrap();
                if reject_history {
                    ws.send(Message::Text(json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"history expired"}}).to_string().into())).await.unwrap();
                    drop(ws);
                    let (socket, _) = listener.accept().await.unwrap();
                    ws = accept_codex_test_websocket(socket).await;
                    let full: Value = serde_json::from_str(
                        ws.next()
                            .await
                            .unwrap()
                            .unwrap()
                            .into_text()
                            .unwrap()
                            .as_str(),
                    )
                    .unwrap();
                    assert!(full.get("previous_response_id").is_none());
                    assert_eq!(
                        full["input"],
                        json!([{"role":"user","content":"seed"},{"role":"user","content":"next"}])
                    );
                    let received_after = Instant::now();
                    ws.send(Message::Text(json!({"type":"response.created","response":{"id":"resp_timing_success","status":"in_progress","output":[]}}).to_string().into())).await.unwrap();
                    recovery_sent.send(received_after).unwrap();
                    output_released.await.unwrap();
                }
                let output = if reasoning {
                    json!({"type":"response.reasoning_summary_text.delta","response_id":"resp_timing_success","output_index":0,"summary_index":0,"delta":"thinking"})
                } else {
                    json!({"type":"response.output_text.delta","response_id":"resp_timing_success","output_index":0,"content_index":0,"delta":"answer"})
                };
                for event in [
                    output,
                    json!({"type":"response.completed","response":{"id":"resp_timing_success","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
                ] {
                    ws.send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
            });
            let store = Arc::new(MemoryAccountStore::default());
            create_account(&store, "acct_provider_contract").await;
            let provider = provider_with_base_url(&store, base_url);
            let mut seed = Arc::clone(&provider)
                .execute(
                    planned_request("openai", automatic_timing_operation(false)),
                    timed_context(Instant::now(), 1),
                )
                .await
                .unwrap();
            finish_timings(&mut seed, ProviderResponseTimings::default()).await;
            drop(seed);
            let started_at = Instant::now() - Duration::from_secs(3);
            let mut stream = provider
                .execute(
                    planned_request("openai", automatic_timing_operation(true)),
                    timed_context(started_at, 2),
                )
                .await
                .unwrap();
            let (first_sent, first_received) = oneshot::channel();
            let collector = tokio::spawn(async move {
                let mut first_sent = Some(first_sent);
                let mut timings = ProviderResponseTimings::default();
                let mut wire = Vec::new();
                while let Some(event) = stream.next().await {
                    let event = event.expect("automatic delta or full recovery succeeds");
                    if let Some(observation) = event.response_observation() {
                        timings = observation.timings();
                        if timings.first_token_ms.is_some()
                            && let Some(first_sent) = first_sent.take()
                        {
                            first_sent.send(timings).unwrap();
                        }
                    }
                    if let Some(raw) = event
                        .wire_event()
                        .and_then(|wire| wire.raw_websocket_message())
                    {
                        wire.push(serde_json::from_str::<Value>(raw).unwrap());
                    }
                }
                (timings, wire)
            });
            timeout(Duration::from_secs(5), prelude_received)
                .await
                .unwrap()
                .unwrap();
            // 显式暂停非前导，制造可辨识的模型等待；不允许用放行时刻替代 created 接收时刻
            tokio::time::sleep(Duration::from_millis(150)).await;
            let prelude_release_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap();
            release_prelude.send(()).unwrap();
            let recovered_after = if reject_history {
                Some(
                    timeout(Duration::from_secs(5), recovery_received)
                        .await
                        .unwrap()
                        .unwrap(),
                )
            } else {
                None
            };
            let first = timeout(Duration::from_secs(5), first_received)
                .await
                .unwrap()
                .unwrap();
            assert!(first.first_token_ms.is_some_and(|value| value >= 3_000));
            assert_eq!(first.first_event_ms, first.first_token_ms);
            assert_eq!(first.first_reasoning_ms, None);
            assert_eq!(first.first_text_ms, None);
            let output_release_ms = if let Some(recovered_after) = recovered_after {
                let recovered_after_ms =
                    u64::try_from(recovered_after.duration_since(started_at).as_millis()).unwrap();
                assert!(first.first_token_ms.unwrap() >= recovered_after_ms);
                assert!(first.first_token_ms.unwrap() >= prelude_release_ms);
                let released_at = u64::try_from(started_at.elapsed().as_millis()).unwrap();
                release_output.send(()).unwrap();
                released_at
            } else {
                assert!(first.first_token_ms.unwrap() < prelude_release_ms);
                prelude_release_ms
            };
            let (final_timings, wire) = timeout(Duration::from_secs(5), collector)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(final_timings.first_event_ms, first.first_event_ms);
            assert_eq!(final_timings.first_token_ms, first.first_token_ms);
            let content_ms = if reasoning {
                assert_eq!(final_timings.first_text_ms, None);
                final_timings.first_reasoning_ms.unwrap()
            } else {
                assert_eq!(final_timings.first_reasoning_ms, None);
                final_timings.first_text_ms.unwrap()
            };
            assert!(content_ms >= output_release_ms);
            assert_eq!(wire.first().unwrap()["type"], "response.created");
            assert_eq!(
                wire.first().unwrap()["response"]["id"],
                "resp_timing_success"
            );
            assert_eq!(wire.last().unwrap()["type"], "response.completed");
            assert!(
                wire.iter()
                    .all(|event| !event.to_string().contains("resp_timing_rejected"))
            );
            server.await.unwrap();
        }
    }
}
