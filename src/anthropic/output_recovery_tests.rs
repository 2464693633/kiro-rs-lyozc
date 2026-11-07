//! HTTP regressions for bounded no-visible-output recovery and accounting.
use super::*;
use crate::admin::{client_keys::ClientKeyManager, usage_stats::UsageAggregator};
use crate::anthropic::output_recovery::ERROR_TYPE;
use serde_json::Value;

fn payload(streaming: bool) -> Value {
    json!({
        "model":"claude-opus-5-5", "max_tokens":64000, "stream":streaming,
        "system":"Keep the original task constraints.",
        "thinking":{"type":"adaptive"}, "output_config":{"effort":"xhigh"},
        "metadata":{"user_id":"session_550e8400-e29b-41d4-a716-446655440000"},
        "messages":[{"role":"user","content":"Complete the task using the supplied tools if needed."}],
        "tools":[{"name":"Grep","description":"client search", "input_schema":{
            "type":"object","properties":{"pattern":{"type":"string"},"-n":{"type":"boolean"}}
        }}]
    })
}

async fn call(state: AppState, cc: bool, request: Value, key_ctx: KeyContext) -> Response {
    let request = serde_json::from_value(request).unwrap();
    if cc {
        post_messages_cc(
            axum::extract::State(state),
            Extension(key_ctx),
            Json(request),
        )
        .await
    } else {
        post_messages(
            axum::extract::State(state),
            Extension(key_ctx),
            Json(request),
        )
        .await
    }
}

fn events(bytes: &[u8]) -> Vec<Value> {
    parse_sse(std::str::from_utf8(bytes).unwrap())
        .into_iter()
        .map(|(_, event)| event)
        .collect()
}

#[tokio::test]
async fn output_recovery_retries_once_preserves_tools_and_counts_both_attempts() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for legacy in [false, true] {
                let mut f = fixture(Mode::OutputRecovery {
                    outcome: "answer",
                    legacy,
                })
                .await;
                let keys = Arc::new(ClientKeyManager::new());
                let client = keys.create("output recovery".into(), None, None);
                let aggregate = Arc::new(UsageAggregator::new());
                let store = Arc::new(TraceStore::open_in_memory().unwrap());
                f.state = f
                    .state
                    .clone()
                    .with_usage(Some(keys.clone()), None, Some(aggregate.clone()))
                    .with_trace_store(Some(store.clone()));
                let mut request_key = key();
                request_key.key_id = client.id;
                let response = call(f.state.clone(), cc, payload(streaming), request_key).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                assert_eq!(f.calls.load(Ordering::Relaxed), 2);
                assert!(!String::from_utf8_lossy(&bytes).contains("discarded-first-signature"));
                let usage = if streaming {
                    let events = events(&bytes);
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| e["type"] == "message_start")
                            .count(),
                        1
                    );
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| e["type"] == "message_stop")
                            .count(),
                        1
                    );
                    assert!(!events.iter().any(|e| e["type"] == "error"));
                    assert!(
                        events
                            .iter()
                            .any(|e| e["delta"]["text"] == "recovered answer")
                    );
                    events
                        .iter()
                        .find(|e| e["type"] == "message_delta")
                        .unwrap()["usage"]
                        .clone()
                } else {
                    let message: Value = serde_json::from_slice(&bytes).unwrap();
                    assert!(
                        message["content"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|b| b["text"] == "recovered answer")
                    );
                    message["usage"].clone()
                };
                assert_eq!(usage["input_tokens"], 26);
                assert_eq!(usage["output_tokens"], 34);
                assert_eq!(usage["cache_read_input_tokens"], 6);
                assert_eq!(usage["cache_creation_input_tokens"], 4);
                assert_eq!(usage["credit_usage"], 0.25);
                assert_eq!(project_credits(&f), 0.25);
                let billed = keys.list()[0].clone();
                assert_eq!(billed.total_successful_calls, 1);
                assert_eq!(billed.total_output_tokens, 34);
                assert_eq!(billed.total_credits, 0.25);
                assert_eq!(aggregate.overview().today_credits, 0.25);
                let (records, total) = store.query_paged(&TraceQuery {
                    limit: 10,
                    ..Default::default()
                });
                assert_eq!(total, 2);
                assert_eq!(
                    records
                        .iter()
                        .filter(|r| r.error_type.as_deref() == Some(ERROR_TYPE))
                        .count(),
                    1
                );
                assert_eq!(
                    records
                        .iter()
                        .filter(|r| r.final_status == "success")
                        .count(),
                    1
                );
                let bodies = f.bodies.lock();
                assert_eq!(
                    bodies[0]["additionalModelRequestFields"]["output_config"]["effort"],
                    "xhigh"
                );
                assert_eq!(
                    bodies[1]["additionalModelRequestFields"]["output_config"]["effort"],
                    "high"
                );
                let path = "/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools";
                assert_eq!(bodies[0].pointer(path), bodies[1].pointer(path));
                assert_eq!(
                    bodies[0]["conversationState"]["conversationId"],
                    bodies[1]["conversationState"]["conversationId"]
                );
                assert!(
                    bodies[1]
                        .to_string()
                        .contains("Keep the original task constraints.")
                );
            }
        }
    }
}

#[tokio::test]
async fn output_recovery_second_failure_is_explicit_and_does_not_count_success() {
    for streaming in [false, true] {
        let mut f = fixture(Mode::OutputRecovery {
            outcome: "always",
            legacy: false,
        })
        .await;
        let keys = Arc::new(ClientKeyManager::new());
        let client = keys.create("failed output recovery".into(), None, None);
        f.state = f.state.clone().with_usage(Some(keys.clone()), None, None);
        let mut request_key = key();
        request_key.key_id = client.id;
        let response = call(f.state.clone(), false, payload(streaming), request_key).await;
        assert_eq!(
            response.status(),
            if streaming {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            }
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        if streaming {
            let events = events(&bytes);
            assert_eq!(events.iter().filter(|e| e["type"] == "error").count(), 1);
            assert_eq!(events.last().unwrap()["error"]["type"], ERROR_TYPE);
            assert!(
                !events
                    .iter()
                    .any(|e| e["type"] == "message_start" || e["type"] == "message_stop")
            );
        } else {
            let error: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(error["error"]["type"], ERROR_TYPE);
        }
        assert_eq!(f.calls.load(Ordering::Relaxed), 2);
        assert_eq!(project_credits(&f), 0.25);
        let billed = keys.list()[0].clone();
        assert_eq!(billed.total_successful_calls, 0);
        assert_eq!(billed.total_credits, 0.25);
        assert_eq!(billed.total_output_tokens, 34);
    }
}

#[tokio::test]
async fn output_recovery_delivers_a_recovered_client_tool_once() {
    for streaming in [false, true] {
        let f = fixture(Mode::OutputRecovery {
            outcome: "tool",
            legacy: false,
        })
        .await;
        let response = call(f.state.clone(), false, payload(streaming), key()).await;
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        if streaming {
            let events = events(&bytes);
            let tools: Vec<_> = events
                .iter()
                .filter(|e| e["content_block"]["type"] == "tool_use")
                .collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["content_block"]["id"], "recovered-tool");
            assert_eq!(tools[0]["content_block"]["name"], "Grep");
            let args: String = events
                .iter()
                .filter_map(|e| e["delta"]["partial_json"].as_str())
                .collect();
            assert_eq!(
                serde_json::from_str::<Value>(&args).unwrap(),
                json!({"pattern":"hello","-n":true})
            );
        } else {
            let message: Value = serde_json::from_slice(&bytes).unwrap();
            let tools: Vec<_> = message["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["input"], json!({"pattern":"hello","-n":true}));
        }
        assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    }
}

#[tokio::test]
async fn output_recovery_never_retries_answers_refusals_or_transport_failures() {
    for mode in [
        Mode::OutputRecovery {
            outcome: "visible",
            legacy: false,
        },
        Mode::OutputRecovery {
            outcome: "refusal",
            legacy: false,
        },
        Mode::Interrupted,
    ] {
        for streaming in [false, true] {
            let f = fixture(mode).await;
            let response = call(f.state.clone(), false, payload(streaming), key()).await;
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            assert_eq!(
                f.bodies.lock()[0]["additionalModelRequestFields"]["output_config"]["effort"],
                "xhigh"
            );
        }
    }
}

#[tokio::test]
async fn output_recovery_client_cancellation_stops_work_and_settles_partial_costs() {
    let mut f = fixture(Mode::OutputRecovery {
        outcome: "stall",
        legacy: false,
    })
    .await;
    let keys = Arc::new(ClientKeyManager::new());
    let client = keys.create("cancelled recovery".into(), None, None);
    f.state = f.state.clone().with_usage(Some(keys.clone()), None, None);
    let mut request_key = key();
    request_key.key_id = client.id;
    let response = call(f.state.clone(), false, payload(true), request_key).await;
    // Allow the outer stream to receive the metering frame while holding its
    // reasoning. The fixture never completes, so recovery must not start.
    tokio::time::sleep(Duration::from_millis(40)).await;
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), async {
        while keys.list()[0].total_credits == 0.0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    assert_eq!(project_credits(&f), 0.125);
    assert_eq!(keys.list()[0].total_credits, 0.125);
    assert_eq!(keys.list()[0].total_successful_calls, 0);
}

#[tokio::test]
async fn output_recovery_nested_accounting_does_not_settle_parent_usage_twice() {
    use crate::anthropic::{
        handlers::UsageRecordHook,
        identity_guard::{Settlement, UsageCapture},
    };
    for success in [false, true] {
        let mut f = fixture(Mode::OutputRecovery {
            outcome: if success { "answer" } else { "always" },
            legacy: false,
        })
        .await;
        let keys = Arc::new(ClientKeyManager::new());
        let client = keys.create("nested recovery".into(), None, None);
        f.state = f.state.clone().with_usage(Some(keys.clone()), None, None);
        let parent = Arc::new(UsageCapture::default());
        let hook = UsageRecordHook::from_state(&f.state, client.id, "claude-opus-5-5".into());
        let mut settlement = Settlement::new(hook, parent.clone());
        f.state.usage_capture = Some(parent);
        let mut request_key = key();
        request_key.key_id = client.id;
        let response = call(f.state.clone(), false, payload(true), request_key).await;
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(keys.list()[0].total_credits, 0.0);
        settlement.finish(success);
        assert_eq!(keys.list()[0].total_credits, 0.25);
        assert_eq!(keys.list()[0].total_output_tokens, 34);
        assert_eq!(keys.list()[0].total_successful_calls, u64::from(success));
        assert_eq!(project_credits(&f), 0.25);
    }
}

#[tokio::test]
async fn output_recovery_preserves_structured_output_in_json_and_buffered_sse() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::OutputRecovery {
                outcome: "structured",
                legacy: false,
            })
            .await;
            let mut request = payload(streaming);
            request["output_config"]["format"] = json!({"type":"json_schema", "schema":{
                "type":"object", "properties":{"ok":{"type":"boolean"}}, "required":["ok"], "additionalProperties":false
            }});
            let response = call(f.state.clone(), cc, request, key()).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let (text, usage) = if streaming {
                let events = events(&bytes);
                assert!(!events.iter().any(|event| event["type"] == "error"));
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["type"] == "message_stop")
                        .count(),
                    1
                );
                let text: String = events
                    .iter()
                    .filter_map(|event| event["delta"]["text"].as_str())
                    .collect();
                let usage = events
                    .iter()
                    .find(|event| event["type"] == "message_delta")
                    .unwrap()["usage"]
                    .clone();
                (text, usage)
            } else {
                let message: Value = serde_json::from_slice(&bytes).unwrap();
                let text: String = message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect();
                (text, message["usage"].clone())
            };
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                json!({"ok":true})
            );
            assert_eq!(usage["output_tokens"], 34);
            assert_eq!(f.calls.load(Ordering::Relaxed), 2);
            assert_eq!(project_credits(&f), 0.25);
        }
    }
}

#[tokio::test]
async fn output_recovery_thinking_alias_also_recovers_without_changing_normal_effort() {
    let f = fixture(Mode::OutputRecovery {
        outcome: "answer",
        legacy: false,
    })
    .await;
    let mut request = payload(false);
    request["model"] = json!("claude-opus-5-5-thinking");
    request.as_object_mut().unwrap().remove("thinking");
    let response = call(f.state.clone(), false, request, key()).await;
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    let bodies = f.bodies.lock();
    assert_eq!(
        bodies[0]["additionalModelRequestFields"]["output_config"]["effort"],
        "xhigh"
    );
    assert_eq!(
        bodies[1]["additionalModelRequestFields"]["output_config"]["effort"],
        "high"
    );
}

#[tokio::test]
async fn output_recovery_stream_stays_incremental_after_first_text() {
    let f = fixture(Mode::Normal(Duration::from_millis(800))).await;
    let response = call(f.state.clone(), false, payload(true), key()).await;
    let mut body = response.into_body().into_data_stream();
    let mut wire = Vec::new();
    tokio::time::timeout(Duration::from_millis(350), async {
        loop {
            let chunk = body.next().await.unwrap().unwrap();
            wire.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&wire).contains("text_delta") {
                break;
            }
        }
    })
    .await
    .expect("recovery wrapper buffered answer text until upstream EOF");
    while let Some(chunk) = body.next().await {
        wire.extend_from_slice(&chunk.unwrap());
    }
    assert!(
        events(&wire)
            .iter()
            .any(|event| event["type"] == "message_stop")
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
}
