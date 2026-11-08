use super::*;
use crate::admin::{
    AdminService, AdminState, ClientKeyManager, GroupManager, UsageAggregator, create_admin_router,
};
use crate::anthropic::cache_metering::{CacheMeter, CacheUsage};

fn total_input(usage: &serde_json::Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|key| usage[*key].as_u64().unwrap())
    .sum()
}

fn deduction_config(value: u32) -> Config {
    let mut config = Config::default();
    config.input_token_deduction = value;
    config
}

async fn api_usage(response: Response, streaming: bool) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    if !streaming {
        return serde_json::from_str::<serde_json::Value>(&body).unwrap()["usage"].clone();
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
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
        .expect("final usage snapshot")
}

#[tokio::test]
async fn input_token_deduction_admin_persists_validates_and_preserves_request_snapshots() {
    let path = std::env::temp_dir().join(format!(
        "input-token-deduction-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, r#"{"region":"eu-central-1"}"#).unwrap();
    let f = fixture_with_config(
        Mode::Metered { incomplete: false },
        Config::load(&path).unwrap(),
    )
    .await;
    let manager = f.state.kiro_provider.as_ref().unwrap().token_manager();
    let old = f
        .state
        .compute_cache_usage_for_key(
            &serde_json::from_value(json!({
                "model":"gpt-5.6-sol","max_tokens":64,"messages":[{"role":"user","content":"hello"}]
            }))
            .unwrap(),
            0,
        )
        .await;
    let state = AdminState::new(
        "fixture-admin",
        AdminService::new(manager.clone(), vec![]),
        Arc::new(ClientKeyManager::new()),
        Arc::new(UsageAggregator::new()),
        Arc::new(TraceStore::open_in_memory().unwrap()),
        Arc::new(GroupManager::new()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/config/input-token-deduction",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, create_admin_router(state))
            .await
            .unwrap()
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .put(&url)
            .json(&json!({"inputTokenDeduction":2000}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        json!({}),
        json!({"inputTokenDeduction":null}),
        json!({"inputTokenDeduction":-1}),
        json!({"inputTokenDeduction":1.5}),
        json!({"inputTokenDeduction":"2000"}),
        json!({"inputTokenDeduction":1_000_001}),
        json!({"inputTokenDeduction":2000,"extra":true}),
    ] {
        let response = client
            .put(&url)
            .header("x-api-key", "fixture-admin")
            .json(&invalid)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error(), "{invalid}");
        assert_eq!(manager.input_token_deduction(), 0);
        assert_eq!(Config::load(&path).unwrap().input_token_deduction, 0);
    }
    for value in [2000, 1_000_000, 0, 2000] {
        let response = client
            .put(&url)
            .header("x-api-key", "fixture-admin")
            .json(&json!({"inputTokenDeduction":value}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap(),
            json!({"inputTokenDeduction":value})
        );
        let current: serde_json::Value = client
            .get(&url)
            .header("x-api-key", "fixture-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(current, json!({"inputTokenDeduction":value}));
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.region, "eu-central-1");
        let restarted = MultiTokenManager::new(reloaded, vec![], None, None, false).unwrap();
        assert_eq!(restarted.input_token_deduction(), value);
    }
    assert_eq!(old.split_against_total(2500), (2500, 0, 0));
    let request = serde_json::from_value(json!({"model":"gpt-5.6-sol","max_tokens":64,"messages":[{"role":"user","content":"hello"}]})).unwrap();
    assert_eq!(
        f.state
            .compute_cache_usage_for_key(&request, 0)
            .await
            .split_against_total(2500),
        (500, 0, 0)
    );
    std::fs::write(&path, "invalid json").unwrap();
    let failed = client
        .put(&url)
        .header("x-api-key", "fixture-admin")
        .json(&json!({"inputTokenDeduction":0}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(manager.input_token_deduction(), 2000);
    server.abort();
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn input_token_deduction_conserves_cache_splits_ttl_limits_and_provider_usage() {
    use crate::kiro::model::events::TokenUsage;
    let cache = CacheUsage {
        cache_read: 50,
        cache_covered_est: 80,
        prompt_total_est: 100,
        cache_creation_1h_est: 20,
        ..Default::default()
    }
    .with_input_token_deduction(2000);
    for total in [-1, 0, 1, 1999, 2000, 2001, 12_000, i32::MAX] {
        let expected = i64::from(total.max(0)).saturating_sub(2000).max(0);
        for limit in [None, Some(0), Some(60), Some(100)] {
            let limited = cache.with_read_limit_percent(limit);
            let (input, write, read) = limited.split_against_total(total);
            assert_eq!(
                i64::from(input) + i64::from(write) + i64::from(read),
                expected
            );
            assert_eq!(
                limited.creation_breakdown(write, false).unwrap().total(),
                write as u64
            );
            if let Some(limit) = limit {
                assert!(i64::from(read) * 100 <= expected * i64::from(limit));
            }
        }
    }
    for original in [
        TokenUsage {
            uncached_input_tokens: 1000,
            cache_write_input_tokens: 2000,
            cache_read_input_tokens: 7000,
            output_tokens: 17,
        },
        TokenUsage {
            uncached_input_tokens: 0,
            cache_write_input_tokens: 4,
            cache_read_input_tokens: 18224,
            output_tokens: 17,
        },
        TokenUsage {
            uncached_input_tokens: i32::MAX,
            cache_write_input_tokens: i32::MAX,
            cache_read_input_tokens: i32::MAX,
            output_tokens: 17,
        },
        TokenUsage {
            uncached_input_tokens: -1,
            cache_write_input_tokens: 2,
            cache_read_input_tokens: 3,
            output_tokens: 17,
        },
    ] {
        let original = original.sanitized();
        let adjusted = cache.reported_provider_usage(original);
        let total = |u: TokenUsage| {
            i64::from(u.uncached_input_tokens)
                + i64::from(u.cache_write_input_tokens)
                + i64::from(u.cache_read_input_tokens)
        };
        assert_eq!(total(adjusted), (total(original) - 2000).max(0));
        assert_eq!(adjusted.output_tokens, original.output_tokens);
        assert!(adjusted.uncached_input_tokens <= original.uncached_input_tokens);
        assert!(adjusted.cache_write_input_tokens <= original.cache_write_input_tokens);
        assert!(adjusted.cache_read_input_tokens <= original.cache_read_input_tokens);
        assert_eq!(
            CacheUsage::default().reported_provider_usage(original),
            original
        );
    }
}

#[tokio::test]
async fn input_token_deduction_matches_all_api_formats_backend_records_and_actual_credits() {
    for endpoint in ["messages", "cc", "chat", "responses"] {
        for streaming in [false, true] {
            for (deduction, expected) in [(0, 13), (7, 6), (2000, 0)] {
                let f = fixture_with_config(
                    Mode::Metered { incomplete: false },
                    deduction_config(deduction),
                )
                .await;
                let keys = Arc::new(ClientKeyManager::new());
                let key = keys.create("adjusted".into(), None, None);
                keys.set_cache_metering(key.id, false);
                let aggregator = Arc::new(UsageAggregator::new());
                let traces = Arc::new(TraceStore::open_in_memory().unwrap());
                let state = f
                    .state
                    .clone()
                    .with_usage(Some(keys.clone()), None, Some(aggregator.clone()))
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
                let input = usage
                    .get("input_tokens")
                    .or_else(|| usage.get("prompt_tokens"))
                    .unwrap();
                let output = usage
                    .get("output_tokens")
                    .or_else(|| usage.get("completion_tokens"))
                    .unwrap();
                assert_eq!(input, expected, "{endpoint} stream={streaming} {usage}");
                assert_eq!(output, 17);
                let stored = &keys.list()[0];
                assert_eq!(stored.total_input_tokens, expected);
                assert_eq!(stored.total_output_tokens, 17);
                assert_eq!(stored.total_credits, 0.125);
                assert_eq!(project_credits(&f), 0.125);
                assert_eq!(aggregator.overview().today_input_tokens, expected);
                let (records, count) = traces.query_paged(&TraceQuery {
                    limit: 10,
                    ..Default::default()
                });
                assert_eq!(count, 1);
                assert_eq!(records[0].input_tokens, expected);
                assert_eq!(records[0].output_tokens, 17);
                assert_eq!(records[0].credits, 0.125);
                assert_eq!(
                    records[0].usage_source.as_deref(),
                    Some(if deduction == 0 {
                        "provider"
                    } else {
                        "adjusted"
                    })
                );
                assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            }
        }
    }
}

#[tokio::test]
async fn input_token_deduction_reduces_context_before_simulated_cache_and_preserves_ttl() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for simulate in [false, true] {
                let payload = json!({"model":"claude-opus-5-5","max_tokens":64,"stream":streaming,
                    "system":[{"type":"text","text":"stable prefix ".repeat(80),"cache_control":{"type":"ephemeral","ttl":"1h"}}],
                    "messages":[{"role":"user","content":"hello"}]});
                let mut totals = Vec::new();
                for deduction in [0, 2000] {
                    let f =
                        fixture_with_config(Mode::ContextUsageOnly, deduction_config(deduction))
                            .await;
                    let keys = Arc::new(ClientKeyManager::new());
                    let key = keys.create("context".into(), None, None);
                    keys.set_cache_metering(key.id, simulate);
                    keys.set_cache_read_limit_percent(key.id, Some(60));
                    let state = f
                        .state
                        .clone()
                        .with_usage(Some(keys.clone()), None, None)
                        .with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
                    let cold = cache_read_limit_message(&state, key.id, cc, payload.clone()).await;
                    let warm = cache_read_limit_message(&state, key.id, cc, payload.clone()).await;
                    assert_eq!(
                        cold["usage"]["output_tokens"],
                        warm["usage"]["output_tokens"]
                    );
                    let total = total_input(&warm["usage"]);
                    assert_eq!(total_input(&cold["usage"]), total);
                    assert!(
                        warm["usage"]["cache_read_input_tokens"].as_u64().unwrap() * 100
                            <= total * 60
                    );
                    assert_eq!(
                        cold["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
                        cold["usage"]["cache_creation_input_tokens"]
                    );
                    assert_eq!(
                        cold["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
                        0
                    );
                    let stored = &keys.list()[0];
                    assert_eq!(
                        stored.total_input_tokens
                            + stored.total_cache_creation_tokens
                            + stored.total_cache_read_tokens,
                        total * 2
                    );
                    totals.push(total);
                }
                assert_eq!(totals[0] - totals[1], 2000);
            }
        }
    }
}

#[tokio::test]
async fn input_token_deduction_applies_once_per_search_round_before_aggregation() {
    for streaming in [false, true] {
        let f = fixture_with_config(Mode::Search, deduction_config(7)).await;
        let aggregator = Arc::new(UsageAggregator::new());
        let state = f
            .state
            .clone()
            .with_usage(None, None, Some(aggregator.clone()));
        let response = compatibility_request(
            state,
            false,
            json!({"model":"gpt-5.6-sol","max_tokens":256,"stream":streaming,
            "messages":[{"role":"user","content":"search rust release"}],
            "tools":[{"type":"web_search_20250305","name":"web_search"}]}),
        )
        .await;
        let usage = api_usage(response, streaming).await;
        assert_eq!(usage["input_tokens"], 12, "{usage}");
        assert_eq!(usage["output_tokens"], 34);
        assert_eq!(aggregator.overview().today_input_tokens, 12);
        assert_eq!(
            f.calls.load(Ordering::Relaxed),
            3,
            "two model calls and one search"
        );
    }
}

#[tokio::test]
async fn input_token_deduction_recovery_aggregates_adjusted_attempts_without_changing_credits() {
    for streaming in [false, true] {
        let f = fixture_with_config(
            Mode::OutputRecovery {
                outcome: "answer",
                legacy: false,
            },
            deduction_config(7),
        )
        .await;
        let keys = Arc::new(ClientKeyManager::new());
        let key = keys.create("recovery".into(), None, None);
        let state = f.state.clone().with_usage(Some(keys.clone()), None, None);
        let response = post_messages(axum::extract::State(state), Extension(KeyContext {
            key_id:key.id, group:None, key_source:TraceKeySource::ClientKey, client_ip:None,
        }), Json(serde_json::from_value(json!({"model":"claude-opus-5-5","max_tokens":64000,"stream":streaming,
            "thinking":{"type":"adaptive"},"messages":[{"role":"user","content":"Complete the task."}]})).unwrap())).await;
        let usage = api_usage(response, streaming).await;
        assert_eq!(
            total_input(&usage),
            22,
            "two attempts, each reporting 18 - 7 input tokens"
        );
        assert_eq!(usage["output_tokens"], 34);
        assert_eq!(usage["credit_usage"], 0.25);
        assert_eq!(project_credits(&f), 0.25);
        let stored = &keys.list()[0];
        assert_eq!(
            stored.total_input_tokens
                + stored.total_cache_creation_tokens
                + stored.total_cache_read_tokens,
            22
        );
        assert_eq!(stored.total_successful_calls, 1);
        assert_eq!(stored.total_credits, 0.25);
        assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    }
}
