use super::*;
use crate::admin::{
    AdminService, AdminState, ClientKeyManager, GroupManager, UsageAggregator, create_admin_router,
};
use crate::anthropic::cache_metering::CacheMeter;

fn total_input(usage: &serde_json::Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|field| usage[*field].as_u64().unwrap())
    .sum()
}

async fn api_usage(response: Response, streaming: bool) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    if !streaming {
        return serde_json::from_str::<serde_json::Value>(&body).unwrap()["usage"].clone();
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| {
            if event["usage"].is_object() {
                Some(event["usage"].clone())
            } else if event["response"]["usage"].is_object() {
                Some(event["response"]["usage"].clone())
            } else {
                None
            }
        })
        .last()
        .expect("Final usage")
}

#[tokio::test]
async fn reporting_policy_preserves_total_output_credits_and_records_for_all_routes() {
    for endpoint in ["messages", "cc", "chat", "responses"] {
        for streaming in [false, true] {
            let f = fixture(Mode::CachedUsage).await;
            let keys = Arc::new(ClientKeyManager::new());
            let key = keys.create("reporting".into(), None, None);
            keys.set_cache_read_target_percent(key.id, Some(80));
            let traces = Arc::new(TraceStore::open_in_memory().unwrap());
            let aggregator = Arc::new(UsageAggregator::new());
            let state = f
                .state
                .clone()
                .with_usage(Some(keys.clone()), None, Some(aggregator.clone()))
                .with_cache_meter(Some(Arc::new(CacheMeter::new(None))))
                .with_trace_store(Some(traces.clone()));
            let usage = api_usage(
                request_with_key(
                    state,
                    endpoint,
                    streaming,
                    KeyContext {
                        key_id: key.id,
                        group: None,
                        key_source: TraceKeySource::ClientKey,
                        client_ip: None,
                    },
                )
                .await,
                streaming,
            )
            .await;
            let (rows, count) = traces.query_paged(&TraceQuery {
                limit: 10,
                ..Default::default()
            });
            assert_eq!(count, 1);
            let row = &rows[0];
            assert_eq!(
                row.input_tokens, 2,
                "{endpoint}, stream={streaming}: {usage}"
            );
            let total = row.input_tokens + row.cache_creation_tokens + row.cache_read_tokens;
            assert_eq!(row.cache_read_tokens, (total - 2) * 80 / 100);
            assert_eq!(row.output_tokens, 17);
            assert_eq!(row.usage_source.as_deref(), Some("adjusted"));
            assert_eq!(keys.list()[0].total_input_tokens, 2);
            assert_eq!(aggregator.overview().today_input_tokens, 2);
            let input = usage
                .get("input_tokens")
                .or_else(|| usage.get("prompt_tokens"))
                .unwrap()
                .as_u64()
                .unwrap();
            assert_eq!(
                input,
                if endpoint == "messages" || endpoint == "cc" {
                    2
                } else {
                    total
                },
                "{usage}"
            );
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
        }
    }
}

#[tokio::test]
async fn local_cold_warm_and_no_breakpoint_keep_fixed_input_without_deduction() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for breakpoint in [false, true] {
                let f = fixture(Mode::ContextUsageOnly).await;
                let keys = Arc::new(ClientKeyManager::new());
                let key = keys.create("local".into(), None, None);
                let state = f
                    .state
                    .clone()
                    .with_usage(Some(keys), None, None)
                    .with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
                let mut payload = json!({"model":"claude-opus-5-5","max_tokens":64,"stream":streaming,
                    "system":[{"type":"text","text":"stable prefix ".repeat(80)}],
                    "messages":[{"role":"user","content":"brand new question"}]});
                if breakpoint {
                    payload["system"][0]["cache_control"] = json!({"type":"ephemeral","ttl":"1h"});
                }
                let cold = cache_read_limit_message(&state, key.id, cc, payload.clone()).await;
                let warm = cache_read_limit_message(&state, key.id, cc, payload).await;
                assert_eq!(cold["usage"]["input_tokens"], 2);
                assert_eq!(warm["usage"]["input_tokens"], 2);
                assert_eq!(total_input(&cold["usage"]), total_input(&warm["usage"]));
                assert_eq!(cold["usage"]["cache_read_input_tokens"], 0);
                if breakpoint {
                    assert!(warm["usage"]["cache_read_input_tokens"].as_u64().unwrap() > 0);
                } else {
                    assert_eq!(warm["usage"]["cache_read_input_tokens"], 0);
                }
                assert_eq!(
                    cold["usage"]["output_tokens"],
                    warm["usage"]["output_tokens"]
                );
            }
        }
    }
}

#[tokio::test]
async fn reporting_policy_search_and_recovery_reserve_input_once_for_client_request() {
    for streaming in [false, true] {
        for recovering in [false, true] {
            let f = fixture(if recovering {
                Mode::OutputRecovery {
                    outcome: "answer",
                    legacy: false,
                }
            } else {
                Mode::Search
            })
            .await;
            let keys = Arc::new(ClientKeyManager::new());
            let key = keys.create("multi-round".into(), None, None);
            keys.set_cache_read_target_percent(key.id, Some(80));
            let state = f
                .state
                .clone()
                .with_usage(Some(keys.clone()), None, None)
                .with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
            let mut payload = json!({"model":"claude-opus-5-5","max_tokens":64000,"stream":streaming,
                "messages":[{"role":"user","content":"Complete the task."}]});
            if recovering {
                payload["thinking"] = json!({"type":"adaptive"});
            } else {
                payload["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]);
            }
            let response = post_messages(
                axum::extract::State(state),
                Extension(KeyContext {
                    key_id: key.id,
                    group: None,
                    key_source: TraceKeySource::ClientKey,
                    client_ip: None,
                }),
                Json(serde_json::from_value(payload).unwrap()),
            )
            .await;
            let usage = api_usage(response, streaming).await;
            assert_eq!(usage["input_tokens"], 2, "{usage}");
            assert_eq!(usage["output_tokens"], 34);
            let total = total_input(&usage);
            assert_eq!(
                usage["cache_read_input_tokens"],
                (total - 2) * 80 / 100,
                "recovering={recovering}, streaming={streaming}: {usage}"
            );
            assert_eq!(keys.list()[0].total_input_tokens, 2);
            assert_eq!(keys.list()[0].total_successful_calls, 1);
            if recovering {
                assert_eq!(total, 36);
                assert_eq!(usage["credit_usage"], 0.25);
                assert_eq!(project_credits(&f), 0.25);
            } else {
                assert_eq!(f.calls.load(Ordering::Relaxed), 3);
            }
        }
    }
}

#[tokio::test]
async fn reporting_admin_persists_and_old_deduction_config_is_inert() {
    let path = std::env::temp_dir().join(format!("reporting-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(
        &path,
        r#"{"region":"eu-central-1","inputTokenDeduction":1900}"#,
    )
    .unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.reported_input_tokens, 2);
    assert!(
        serde_json::to_value(&config)
            .unwrap()
            .get("inputTokenDeduction")
            .is_none()
    );
    let f = fixture_with_config(Mode::Metered { incomplete: false }, config).await;
    let manager = f.state.kiro_provider.as_ref().unwrap().token_manager();
    let state = AdminState::new(
        "fixture-admin",
        AdminService::new(manager.clone(), vec![]),
        Arc::new(ClientKeyManager::new()),
        Arc::new(UsageAggregator::new()),
        Arc::new(TraceStore::open_in_memory().unwrap()),
        Arc::new(GroupManager::new()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, create_admin_router(state))
            .await
            .unwrap()
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("{base}/config/input-token-deduction"))
            .header("x-api-key", "fixture-admin")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let url = format!("{base}/config/reported-input-tokens");
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        json!({}),
        json!({"reportedInputTokens":-1}),
        json!({"reportedInputTokens":1001}),
        json!({"reportedInputTokens":1.5}),
        json!({"reportedInputTokens":2,"extra":true}),
    ] {
        assert!(
            client
                .put(&url)
                .header("x-api-key", "fixture-admin")
                .json(&invalid)
                .send()
                .await
                .unwrap()
                .status()
                .is_client_error()
        );
    }
    for value in [0, 2, 1000, 2] {
        assert_eq!(
            client
                .put(&url)
                .header("x-api-key", "fixture-admin")
                .json(&json!({"reportedInputTokens":value}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(manager.reported_input_tokens(), value);
        assert_eq!(Config::load(&path).unwrap().reported_input_tokens, value);
    }
    std::fs::write(&path, "invalid json").unwrap();
    assert_eq!(
        client
            .put(&url)
            .header("x-api-key", "fixture-admin")
            .json(&json!({"reportedInputTokens":0}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(manager.reported_input_tokens(), 2);
    server.abort();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn reporting_policy_preserves_real_credits_and_disabled_metering_keeps_native_usage() {
    for streaming in [false, true] {
        for enabled in [false, true] {
            let f = fixture(Mode::Metered { incomplete: false }).await;
            let keys = Arc::new(ClientKeyManager::new());
            let key = keys.create("credits".into(), None, None);
            keys.set_cache_metering(key.id, enabled);
            keys.set_cache_read_target_percent(key.id, Some(80));
            let state = f
                .state
                .clone()
                .with_usage(Some(keys.clone()), None, None)
                .with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
            let usage = api_usage(
                request_with_key(
                    state,
                    "messages",
                    streaming,
                    KeyContext {
                        key_id: key.id,
                        group: None,
                        key_source: TraceKeySource::ClientKey,
                        client_ip: None,
                    },
                )
                .await,
                streaming,
            )
            .await;
            assert_eq!(total_input(&usage), 13);
            assert_eq!(usage["input_tokens"], if enabled { 2 } else { 13 });
            assert_eq!(usage["output_tokens"], 17);
            assert_eq!(usage["credit_usage"], 0.125);
            assert_eq!(keys.list()[0].total_credits, 0.125);
            assert_eq!(project_credits(&f), 0.125);
        }
    }
}

#[tokio::test]
async fn reporting_policy_global_switch_restores_native_usage_and_request_snapshot_is_stable() {
    let f = fixture(Mode::CachedUsage).await;
    let keys = Arc::new(ClientKeyManager::new());
    let key = keys.create("global-switch".into(), None, None);
    keys.set_cache_read_target_percent(key.id, Some(80));
    let meter = Arc::new(CacheMeter::new(None));
    let state = f
        .state
        .clone()
        .with_usage(Some(keys.clone()), None, None)
        .with_cache_meter(Some(meter.clone()));
    let payload = serde_json::from_value(json!({"model":"claude-opus-5-5","max_tokens":64,
        "messages":[{"role":"user","content":"hello"}]}))
    .unwrap();
    let in_flight = state.compute_cache_usage_for_key(&payload, key.id).await;
    keys.set_cache_read_target_percent(key.id, Some(20));
    let original = in_flight.split_against_total(10_002);
    assert_eq!(original, (2, 2_000, 8_000));
    assert_eq!(
        state
            .compute_cache_usage_for_key(&payload, key.id)
            .await
            .split_against_total(10_002),
        (2, 8_000, 2_000)
    );
    meter.set_enabled(false);
    for streaming in [false, true] {
        let usage = api_usage(
            request_with_key(
                state.clone(),
                "messages",
                streaming,
                KeyContext {
                    key_id: key.id,
                    group: None,
                    key_source: TraceKeySource::ClientKey,
                    client_ip: None,
                },
            )
            .await,
            streaming,
        )
        .await;
        assert_eq!(usage["input_tokens"], 0);
        assert_eq!(usage["cache_creation_input_tokens"], 4);
        assert_eq!(usage["cache_read_input_tokens"], 18224);
        assert_eq!(usage["output_tokens"], 17);
    }
}
