//! Real HTTP fixtures for gateway regressions and repeatable latency measurements.
use super::{
    handlers::{post_messages, post_messages_cc},
    middleware::{AppState, KeyContext},
    openai::post_chat_completions,
    responses::post_responses,
};
use crate::{
    admin::trace_db::{TraceKeySource, TraceQuery, TraceStore},
    kiro::{
        endpoint::{KiroEndpoint, RequestContext},
        model::credentials::KiroCredentials,
        parser::crc::crc32,
        provider::KiroProvider,
        token_manager::MultiTokenManager,
    },
    model::config::{Config, ToolCompatibilityMode},
};
use axum::{
    Extension, Json, Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use parking_lot::Mutex;
use serde_json::json;

#[path = "chat_stream_gateway_tests.rs"]
mod chat_stream_tests;
#[path = "output_recovery_tests.rs"]
mod output_recovery_tests;
#[path = "input_token_deduction_tests.rs"]
mod input_token_deduction_tests;
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Mode {
    OutputRecovery { outcome: &'static str, legacy: bool },
    ReviewSuspended,
    ReviewRepetition,
    ReviewEndTurn,
    Normal(Duration),
    DelayedJson(Duration),
    Metered { incomplete: bool },
    SemanticIdentity {
        classification: &'static str,
        reply: &'static str,
        review: &'static str,
        tool: bool,
    },
    Interrupted,
    Overloaded,
    Large,
    Truncated,
    InvalidFrame,
    Empty,
    Tools,
    Execution {
        client_tool: bool,
    },
    ExecutionRateLimit {
        failures: usize,
        retry_after: &'static str,
    },
    ExecutionSchema,
    ExecutionSearch,
    GatedChatText,
    GatedToolArguments,
    CompactionRetry,
    Filtered,
    Stalled,
    SlowHeaders,
    SlowErrorBody,
    Structured(&'static str),
    SlowStructured(&'static str),
    StructuredWithReasoning(&'static str),
    StructuredWithSignatureOnly(&'static str),
    Signature {
        only: bool,
    },
    Search,
    HiddenReasoning,
    CachedUsage,
    MetadataFirst,
    Active,
    /// 上游内容过滤拦截：只有 reasoning，`metadataEvent.stopReason = CONTENT_FILTERED`，
    /// 随后 contextUsage 100%（验证拒答不被上下文溢出覆盖）。帧序列取自线上抓包。
    Refused,
    RefusedWithSearch,
    RefusedWithPartialTool,
    RefusedWithInvalidTool,
    RefusedWithEmptyTool,
    RefusedBare,
    /// 上游不下发 tokenUsage，只给 contextUsage：走本地计量回退路径。
    ContextUsageOnly,
    /// 上游按给定片段依次返回 assistantResponseEvent，随后 metadataEvent（用量 13/17）。
    Reply(&'static [&'static str]),
    ReplyWithCredits(&'static [&'static str]),
    /// 同 `Reply`，但先返回一段原生 reasoning。
    ReplyWithReasoning(&'static [&'static str]),
    /// 正文以字面 `<thinking>` 开头（L7：extractThinking=false 时流式不能拆出思考块）。
    ThinkingText,
    /// 正文之后上游下发 ThrottlingException 异常帧（M3）。
    Throttled,
    /// web_search 循环里工具参数是半截 JSON（L10）。
    SearchInvalidToolJson,
    /// 只有正文、没有 metadataEvent.tokenUsage：线上 Kiro 对 claude-opus/sonnet-5.5 的实际形态
    /// （traces.db 里 usage_source 全是 none），缓存用量只能来自本地 CacheMeter。
    NoProviderUsage,
}

struct Fixture {
    state: AppState,
    calls: Arc<AtomicUsize>,
    connections: Arc<Mutex<HashSet<SocketAddr>>>,
    close_headers: Arc<AtomicUsize>,
    /// 上游收到的 Kiro 请求体（JSON）。
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    release_tool_arguments: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

#[tokio::test]
async fn tool_mode_admin_switch_updates_gateway_and_preserves_in_flight_snapshot() {
    use crate::admin::{
        AdminService, AdminState, ClientKeyManager, GroupManager, UsageAggregator,
        create_admin_router,
    };

    let path = std::env::temp_dir().join(format!("tool-mode-api-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&path, r#"{"toolCompatibilityMode":"claude-code"}"#).unwrap();
    let fixture = fixture_with_config(Mode::Reply(&["ok"]), Config::load(&path).unwrap()).await;
    let manager = fixture.state.kiro_provider.as_ref().unwrap().token_manager();
    let state = AdminState::new(
        "fixture-admin-only",
        AdminService::new(manager.clone(), vec!["fixture".into()]),
        Arc::new(ClientKeyManager::new()),
        Arc::new(UsageAggregator::new()),
        Arc::new(TraceStore::open_in_memory().unwrap()),
        Arc::new(GroupManager::new()),
    );
    let app = create_admin_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/config/tool-compatibility", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(client.get(&url).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        client.put(&url).json(&json!({"mode":"raw"})).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let initial: serde_json::Value = client.get(&url).header("x-api-key", "fixture-admin-only")
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(initial, json!({"mode":"claude-code"}));
    for invalid in [json!({"mode":"invalid"}), json!({}), json!({"mode":"raw","extra":true})] {
        let response = client.put(&url).header("x-api-key", "fixture-admin-only")
            .json(&invalid).send().await.unwrap();
        assert!(response.status().is_client_error());
        assert_eq!(manager.tool_compatibility_mode(), ToolCompatibilityMode::ClaudeCode);
    }
    let in_flight = fixture.state.clone().snapshot_tool_compatibility_mode();
    for (mode, name) in [("raw", "Write"), ("claude-code", "fs_write"), ("raw", "Write")] {
        let response = client.put(&url).header("x-api-key", "fixture-admin-only")
            .json(&json!({"mode":mode})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<serde_json::Value>().await.unwrap(), json!({"mode":mode}));
        for endpoint in ["messages", "cc", "chat", "responses"] {
            for streaming in [false, true] {
                let mut payload = json!({
                    "model":"gpt-5.6-sol", "max_tokens":64, "stream":streaming,
                    "messages":[{"role":"user","content":"hello"}],
                    "input":[{"role":"user","content":"hello"}],
                    "tools":[{"name":"Write","description":"fixture", "input_schema":{
                        "type":"object", "properties":{"file_path":{"type":"string"},"content":{"type":"string"}}
                    }}]
                });
                let parameters = payload["tools"][0]["input_schema"].clone();
                let response = if endpoint == "chat" {
                    payload["tools"] = json!([{"type":"function", "function":{
                        "name":"Write", "description":"fixture", "parameters":parameters
                    }}]);
                    post_chat_completions(axum::extract::State(fixture.state.clone()), Extension(key()), HeaderMap::new(), Json(serde_json::from_value(payload).unwrap())).await
                } else if endpoint == "responses" {
                    payload["tools"] = json!([{
                        "type":"function", "name":"Write", "description":"fixture", "parameters":parameters
                    }]);
                    post_responses(axum::extract::State(fixture.state.clone()), Extension(key()), HeaderMap::new(), Json(serde_json::from_value(payload).unwrap())).await
                } else if endpoint == "cc" {
                    post_messages_cc(axum::extract::State(fixture.state.clone()), Extension(key()), Json(serde_json::from_value(payload).unwrap())).await
                } else {
                    post_messages(axum::extract::State(fixture.state.clone()), Extension(key()), Json(serde_json::from_value(payload).unwrap())).await
                };
                assert_eq!(response.status(), StatusCode::OK);
                to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                let bodies = fixture.bodies.lock();
                let tools = &bodies.last().unwrap()["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"];
                assert_eq!(tools[0]["toolSpecification"]["name"], name, "{tools}");
            }
        }
    }
    assert_eq!(in_flight.tool_compatibility_mode, ToolCompatibilityMode::ClaudeCode);
    assert_eq!(Config::load(&path).unwrap().tool_compatibility_mode, ToolCompatibilityMode::Raw);
    std::fs::write(&path, "invalid json").unwrap();
    let response = client.put(&url).header("x-api-key", "fixture-admin-only")
        .json(&json!({"mode":"claude-code"})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(manager.tool_compatibility_mode(), ToolCompatibilityMode::Raw);
    server.abort();
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn integration_repeat_guard_records_error() {
    let mut fixture = fixture(Mode::ReviewRepetition).await;
    let store = Arc::new(TraceStore::open_in_memory().unwrap());
    fixture.state = fixture.state.clone().with_trace_store(Some(store.clone()));
    let response = request(fixture.state.clone(), "messages", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("repeated-token loop"));
    let mut records = Vec::new();
    for _ in 0..50 {
        records = store.query_paged(&TraceQuery { limit: 10, ..Default::default() }).0;
        if !records.is_empty() { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_status, "error");
}

async fn assert_ungrouped_key_usage(is_system: bool) {
    use crate::admin::client_keys::ClientKeyManager;

    let fixture = fixture(Mode::ReplyWithCredits(&["hello"])).await;
    let path = std::env::temp_dir().join(format!("client-key-usage-{}.json", uuid::Uuid::new_v4()));
    let manager = Arc::new(ClientKeyManager::load(&path).unwrap());
    let plaintext = if is_system {
        manager.sync_system_key("system".into(), None, "fixture-system-key".into());
        "fixture-system-key"
    } else {
        manager.create_with_key("client".into(), None, None, "fixture-client-key".into());
        "fixture-client-key"
    };
    let key_id = manager.list()[0].id;
    assert_eq!(key_id == 0, is_system);
    assert_eq!(manager.group_of(key_id), None);

    let app = super::router::create_router_with_shared_provider(
        fixture.state.kiro_provider.clone(),
        false,
        ToolCompatibilityMode::Raw,
        Some(manager.clone()),
        None,
        None,
        None,
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let mut calls = 0;
    for endpoint in [
        "/v1/messages",
        "/cc/v1/messages",
        "/v1/chat/completions",
        "/v1/responses",
    ] {
        for streaming in [false, true] {
            let payload = json!({
                "model": "gpt-5.6-sol",
                "max_tokens": 64,
                "stream": streaming,
                "messages": [{"role": "user", "content": "hello"}],
                "input": [{"role": "user", "content": "hello"}]
            });
            let response = client
                .post(format!("http://{address}{endpoint}"))
                .header("x-api-key", plaintext)
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{endpoint}, stream={streaming}"
            );
            let body = response.text().await.unwrap();
            assert!(
                body.contains("hello"),
                "{endpoint}, stream={streaming}: {body}"
            );
            calls += 1;
            let reloaded = ClientKeyManager::load(&path).unwrap();
            for snapshot in [manager.list(), reloaded.list()] {
                let entry = snapshot.iter().find(|entry| entry.id == key_id).unwrap();
                assert_eq!(entry.group, None);
                assert_eq!(entry.total_calls, calls);
                assert_eq!(
                    (
                        entry.total_input_tokens,
                        entry.total_output_tokens,
                        entry.total_credits,
                    ),
                    (13 * calls, 17 * calls, 0.25 * calls as f64),
                    "key={key_id}, {endpoint}, stream={streaming}"
                );
                assert_eq!(entry.total_cache_read_tokens, 3 * calls);
                assert_eq!(entry.total_cache_creation_tokens, 2 * calls);
            }
        }
    }
    assert_eq!(fixture.calls.load(Ordering::Relaxed), calls as usize);
    server.abort();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn gateway_ungrouped_system_key_records_and_persists_usage() {
    assert_ungrouped_key_usage(true).await;
}

#[tokio::test]
async fn gateway_ungrouped_client_key_records_and_persists_usage() {
    assert_ungrouped_key_usage(false).await;
}

#[tokio::test]
async fn integration_native_end_turn_matches_streaming_and_non_streaming() {
    let fixture = fixture(Mode::ReviewEndTurn).await;
    for streaming in [true, false] {
        let response = request(fixture.state.clone(), "messages", streaming).await;
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("\"stop_reason\":\"end_turn\""), "streaming={streaming}: {body}");
    }
}

#[tokio::test]
async fn integration_cc_websearch_receives_custom_headers() {
    use crate::model::custom_headers::{CustomHeaderRule, CustomHeadersManager};
    let fixture = fixture(Mode::Normal(Duration::ZERO)).await;
    let state = fixture.state.clone().with_custom_headers(Some(CustomHeadersManager::new(vec![
        CustomHeaderRule { key: "X-Review-Trace".to_string(), value: "{trace_id}".to_string(), enabled: true },
    ])));
    for streaming in [true, false] {
        let payload = json!({"model":"gpt-5.6-sol","max_tokens":64,"stream":streaming,
            "messages":[{"role":"user","content":"search example"}],
            "tools":[{"type":"web_search_20250305","name":"web_search"}]});
        let response = post_messages_cc(axum::extract::State(state.clone()), Extension(key()), Json(serde_json::from_value(payload).unwrap())).await;
        assert!(response.headers().get("x-review-trace").is_some());
    }
}

#[tokio::test]
async fn integration_cached_model_alias_is_not_rejected_by_preflight() {
    let fixture = fixture(Mode::Normal(Duration::ZERO)).await;
    let manager = fixture.state.kiro_provider.as_ref().unwrap().token_manager();
    manager.set_test_models_fetcher(Arc::new(|_| Box::pin(async {
        Ok(serde_json::from_value(json!({"models": [{"modelId": "claude-sonnet-4.6"}]})).unwrap())
    })));
    manager.get_available_models_for(1).await.unwrap();
    let payload = json!({"model":"claude-sonnet-4-6-thinking", "max_tokens":64,
        "messages":[{"role":"user","content":"hello"}]});
    let response = post_messages(axum::extract::State(fixture.state.clone()), Extension(key()), Json(serde_json::from_value(payload).unwrap())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn integration_no_credentials_after_upstream_failure_does_not_create_trace() {
    for streaming in [true, false] {
        let mut fixture = fixture(Mode::ReviewSuspended).await;
        let store = Arc::new(TraceStore::open_in_memory().unwrap());
        fixture.state = fixture.state.clone().with_trace_store(Some(store.clone()));
        let response = request(fixture.state.clone(), "messages", streaming).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let (_, total) = store.query_paged(&TraceQuery { limit: 10, ..Default::default() });
        assert_eq!(total, 0);
    }
}

#[tokio::test]
async fn integration_mixed_websearch_no_credentials_does_not_create_trace() {
    for streaming in [true, false] {
        let mut fixture = fixture(Mode::ReviewSuspended).await;
        let store = Arc::new(TraceStore::open_in_memory().unwrap());
        fixture.state = fixture.state.clone().with_trace_store(Some(store.clone()));
        let payload = json!({"model":"gpt-5.6-sol","max_tokens":64,"stream":streaming,
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{"type":"web_search_20250305","name":"web_search"},
                {"name":"run","description":"run","input_schema":{"type":"object"}}]});
        let response = post_messages(axum::extract::State(fixture.state.clone()), Extension(key()), Json(serde_json::from_value(payload).unwrap())).await;
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("No available credentials"));
        let (_, total) = store.query_paged(&TraceQuery { limit: 10, ..Default::default() });
        assert_eq!(total, 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LocalEndpoint(String);
impl KiroEndpoint for LocalEndpoint {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn api_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn mcp_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn decorate_api(
        &self,
        req: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        req
    }
    fn decorate_mcp(
        &self,
        req: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        req
    }
    fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
        body.to_owned()
    }
}

fn exception_frame(exception_type: &str, payload: &str) -> Bytes {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "exception"), (":exception-type", exception_type)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    out.extend(headers);
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    Bytes::from(out)
}

fn frame(event: &str, payload: serde_json::Value) -> Bytes {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", event)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    out.extend(headers);
    out.extend(payload);
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    Bytes::from(out)
}

async fn fixture(mode: Mode) -> Fixture {
    fixture_with_config(mode, Config::default()).await
}

async fn fixture_with_config(mode: Mode, config: Config) -> Fixture {
    let calls = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(Mutex::new(HashSet::new()));
    let close_headers = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen_bodies = bodies.clone();
    let release_tool_arguments = Arc::new(tokio::sync::Notify::new());
    let release_arguments = release_tool_arguments.clone();
    let (seen_calls, seen_connections, seen_close) =
        (calls.clone(), connections.clone(), close_headers.clone());
    let app = Router::new().route("/", post(move |ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes| {
        let (calls, connections, close) = (seen_calls.clone(), seen_connections.clone(), seen_close.clone());
        let bodies = seen_bodies.clone();
        let release_arguments = release_arguments.clone();
        async move {
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            bodies.lock().push(request.clone());
            calls.fetch_add(1, Ordering::Relaxed);
            connections.lock().insert(peer);
            if headers.get("connection").is_some_and(|v| v == "close") { close.fetch_add(1, Ordering::Relaxed); }
            let current = request.pointer("/conversationState/currentMessage/userInputMessage/content").and_then(serde_json::Value::as_str).unwrap_or("");
            let classification = current.contains("identity-guard/classify-v1");
            let review = current.contains("identity-guard/review-v1");
            if classification || review {
                let text = match mode {
                    Mode::SemanticIdentity { classification: value, review: checked, .. } => if classification { value } else { checked },
                    _ => if classification { r#"{"kind":"other","language":"zh","model":false,"route":false,"constrained":false}"# } else { r#"{"edits":[]}"# },
                };
                let mut bytes = frame("assistantResponseEvent", json!({"content":text})).to_vec();
                if !matches!(mode, Mode::NoProviderUsage) {
                    bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})));
                }
                if matches!(mode, Mode::Metered { .. }) {
                    bytes.extend_from_slice(&frame("meteringEvent", json!({"unit":"credit","unitPlural":"credits","usage":0.125})));
                }
                return Response::new(Body::from(bytes));
            }
            if let Mode::OutputRecovery { outcome, legacy } = mode {
                let recovering = request.to_string().contains("The previous generation ended after reasoning");
                let signature = if recovering { "recovery-signature" } else { "discarded-first-signature" };
                let mut bytes = if legacy {
                    frame("assistantResponseEvent", json!({"content":"<thinking>fixture private reasoning</thinking>\n\n"})).to_vec()
                } else {
                    frame("reasoningContentEvent", json!({"text":"fixture private reasoning", "signature":signature})).to_vec()
                };
                if outcome == "visible" || (recovering && matches!(outcome, "answer" | "structured")) {
                    bytes.extend_from_slice(&frame("assistantResponseEvent", json!({"content":if outcome == "structured" { "{\"ok\":true}" } else { "recovered answer" }})));
                } else if recovering && outcome == "tool" {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"Grep", "toolUseId":"recovered-tool", "input":"{\"pattern\":\"hello\",\"-n\":true}", "stop":true})));
                }
                bytes.extend_from_slice(&frame("metadataEvent", json!({"stopReason": if outcome == "refusal" { "CONTENT_FILTERED" } else { "END_TURN" }, "tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})));
                bytes.extend_from_slice(&frame("meteringEvent", json!({"unit":"credit","unitPlural":"credits","usage":0.125})));
                if outcome == "stall" {
                    return Response::new(Body::from_stream(
                        stream::once(async move { Ok::<_, Infallible>(Bytes::from(bytes)) })
                            .chain(stream::pending::<Result<Bytes, Infallible>>())
                    ));
                }
                return Response::new(Body::from(bytes));
            }
            if let Mode::Metered { incomplete } = mode {
                let mut bytes = frame("assistantResponseEvent", json!({"content":"metered answer"})).to_vec();
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17}})));
                bytes.extend_from_slice(&frame("meteringEvent", json!({"unit":"credit","unitPlural":"credits","usage":0.125})));
                if incomplete { bytes.extend_from_slice(&[0, 1]); }
                return Response::new(Body::from(bytes));
            }
            if let Mode::SemanticIdentity { reply, tool, .. } = mode {
                let mut bytes = frame("assistantResponseEvent", json!({"content":reply})).to_vec();
                if tool {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"run","toolUseId":"client_1","input":"{\"command\":\"echo Kiro\"}","stop":true})));
                }
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::Overloaded) {
                return Response::builder().status(500).body(Body::from(r#"{"message":"high load","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#)).unwrap();
            }
            if matches!(mode, Mode::ReviewSuspended) {
                return Response::builder().status(403).body(Body::from(r#"{"reason":"TEMPORARILY_SUSPENDED"}"#)).unwrap();
            }
            if matches!(mode, Mode::SlowHeaders) { tokio::time::sleep(Duration::from_secs(4)).await; }
            if matches!(mode, Mode::SlowErrorBody) {
                return Response::builder().status(500).body(Body::from_stream(stream::pending::<Result<Bytes, Infallible>>())).unwrap();
            }
            if matches!(mode, Mode::Empty) { return Response::new(Body::empty()); }
            if matches!(mode, Mode::ContextUsageOnly) {
                let mut bytes = frame("assistantResponseEvent", json!({"content":"fixture answer"})).to_vec();
                bytes.extend_from_slice(&frame("contextUsageEvent", json!({"contextUsagePercentage":5.0})));
                return Response::new(Body::from(bytes));
            }
            if let Mode::Signature { only } = mode {
                let mut reasoning = json!({"signature":super::signature_compat::fixture_signature()});
                if !only { reasoning["text"] = json!("fixture reasoning"); }
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&frame("reasoningContentEvent", reasoning));
                bytes.extend_from_slice(&frame("assistantResponseEvent", json!({"content":"fixture answer"})));
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17}})));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::HiddenReasoning | Mode::CachedUsage) {
                let mut bytes = Vec::new();
                if matches!(mode, Mode::HiddenReasoning) {
                    bytes.extend_from_slice(&frame("reasoningContentEvent", json!({"text":"I'm being asked about my training cutoff; hidden reasoning.","signature":"upstream-signature"})));
                }
                bytes.extend_from_slice(&frame("assistantResponseEvent", json!({"content":"2025-01"})));
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":0,"outputTokens":17,"cacheReadInputTokens":18224,"cacheWriteInputTokens":4}})));
                return Response::new(Body::from(bytes));
            }
            if let Mode::Structured(text) | Mode::SlowStructured(text) | Mode::StructuredWithReasoning(text) | Mode::StructuredWithSignatureOnly(text) = mode {
                if matches!(mode, Mode::SlowStructured(_)) { tokio::time::sleep(Duration::from_secs(2)).await; }
                let mut bytes = Vec::new();
                if matches!(mode, Mode::StructuredWithReasoning(_)) {
                    bytes.extend_from_slice(&frame("reasoningContentEvent", json!({"text":"private structured reasoning","signature":"private-structured-signature"})));
                }
                if matches!(mode, Mode::StructuredWithSignatureOnly(_)) {
                    bytes.extend_from_slice(&frame("reasoningContentEvent", json!({"signature":"opaque-upstream-signature-only"})));
                }
                bytes.extend_from_slice(&frame("assistantResponseEvent", json!({"content":text})));
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17}})));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::Search) {
                if request["jsonrpc"] == "2.0" {
                    assert_eq!(request["params"]["arguments"]["query"], "rust release");
                    return axum::Json(json!({"jsonrpc":"2.0","id":request["id"],"result":{"isError":false,
                        "content":[{"type":"text","text":json!({"results":[{"title":"Rust release",
                            "url":"https://example.test/rust", "snippet":"search evidence"}]}).to_string()}]}})).into_response();
                }
                let mut bytes = if request.to_string().contains("search evidence") {
                    frame("assistantResponseEvent", json!({"content":"Synthesized answer using search evidence."})).to_vec()
                } else {
                    frame("toolUseEvent", json!({"name":"web_search","toolUseId":"search_1",
                        "input":"{\"query\":\"rust release\"}","stop":true})).to_vec()
                };
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17}})));
                return Response::new(Body::from(bytes));
            }
            if let Mode::Reply(pieces) | Mode::ReplyWithReasoning(pieces) | Mode::ReplyWithCredits(pieces) = mode {
                let mut bytes = Vec::new();
                if matches!(mode, Mode::ReplyWithReasoning(_)) {
                    bytes.extend_from_slice(&frame("reasoningContentEvent", json!({"text":"Let me think about the goal."})));
                }
                for piece in pieces { bytes.extend_from_slice(&frame("assistantResponseEvent", json!({"content": piece}))); }
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})));
                if matches!(mode, Mode::ReplyWithCredits(_)) {
                    bytes.extend_from_slice(&frame("meteringEvent", json!({"unit":"credit","unitPlural":"credits","usage":0.25})));
                }
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::Filtered) {
                let mut bytes = Vec::new();
                for part in [
                    frame("reasoningContentEvent", json!({"text":"I should summarize the result."})),
                    frame("metadataEvent", json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"detail":"private provider detail"},"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})),
                ] { bytes.extend_from_slice(&part); }
                let chunks: Vec<_> = bytes.chunks(7).map(Bytes::copy_from_slice).collect();
                return Response::new(Body::from_stream(stream::iter(chunks.into_iter().map(Ok::<_, Infallible>))));
            }
            if matches!(mode, Mode::Large) {
                let frames = (0..18000).map(|_| frame("assistantResponseEvent", json!({"content": "x".repeat(1024)})));
                return Response::new(Body::from_stream(stream::iter(frames.map(Ok::<_, Infallible>))));
            }
            if matches!(mode, Mode::Truncated | Mode::InvalidFrame) {
                let mut bytes = frame("assistantResponseEvent", json!({"content": "damaged"})).to_vec();
                if matches!(mode, Mode::Truncated) { bytes.truncate(bytes.len() - 4); }
                else { let last = bytes.len() - 1; bytes[last] ^= 1; }
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::ExecutionSearch) {
                if request["jsonrpc"] == "2.0" {
                    return axum::Json(json!({"jsonrpc":"2.0","id":request["id"],"result":{"isError":false,
                        "content":[{"type":"text","text":json!({"results":[{"title":"Rust release","url":"https://example.test/rust","snippet":"search evidence"}]}).to_string()}]}})).into_response();
                }
                let mut bytes=if request.to_string().contains("No command was run.") {
                    assert!(request.to_string().contains("search evidence"),"Search evidence was lost between bridge rounds");
                    frame("assistantResponseEvent",json!({"content":"The search evidence was preserved; execution was unavailable."})).to_vec()
                } else if request.to_string().contains("search evidence") {
                    frame("toolUseEvent",json!({"name":"__kiro_server_bash_execution","toolUseId":"execution_1","input":"{\"command\":\"python3 -c 'print(35)'\"}","stop":true})).to_vec()
                } else {
                    frame("toolUseEvent",json!({"name":"web_search","toolUseId":"search_1","input":"{\"query\":\"rust release\"}","stop":true})).to_vec()
                };
                bytes.extend_from_slice(&frame("metadataEvent",json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17}})));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::Execution { .. } | Mode::ExecutionSchema | Mode::ExecutionRateLimit { .. }) {
                if let Mode::ExecutionRateLimit { failures, retry_after } = mode {
                    if request.to_string().contains("No command was run.") && calls.load(Ordering::Relaxed) <= failures + 1 {
                        return Response::builder().status(429).header("retry-after",retry_after)
                            .body(Body::from(r#"{"message":"Too many requests","reason":"USER_REQUEST_RATE_EXCEEDED"}"#)).unwrap();
                    }
                }
                let client_tool=matches!(mode, Mode::Execution { client_tool:true });
                let mut bytes = if request.to_string().contains("No command was run.") {
                    frame("assistantResponseEvent", json!({"content":if matches!(mode,Mode::ExecutionSchema) { "{\"ok\":true}" } else { "The execution backend is unavailable; no command was run." }})).to_vec()
                } else {
                    frame("toolUseEvent", json!({"name":"__kiro_server_bash_execution","toolUseId":"execution_1","input":"{\"command\":\"python3 -c 'print(5*7)'\"}","stop":true})).to_vec()
                };
                if matches!(mode,Mode::ExecutionSchema) && !request.to_string().contains("No command was run.") {
                    let mut preface=frame("assistantResponseEvent",json!({"content":"I will try the tool first. "})).to_vec();
                    preface.extend(bytes); bytes=preface;
                }
                if client_tool {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"run","toolUseId":"client_1","input":"{\"command\":\"client action\"}","stop":true})));
                }
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17}})));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::Throttled) {
                let mut bytes = frame("assistantResponseEvent", json!({"content": "partial answer"})).to_vec();
                bytes.extend_from_slice(&exception_frame("ThrottlingException", r#"{"message":"Rate exceeded for account 123456789012"}"#));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::SearchInvalidToolJson) {
                let bytes = frame("toolUseEvent", json!({"name":"run","toolUseId":"run_1","input":"{\"command\":\"ls","stop":true})).to_vec();
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::ThinkingText) {
                let mut bytes = frame("assistantResponseEvent", json!({"content": "<thinking>先想想</thinking>\n\n答案"})).to_vec();
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":5,"outputTokens":3,"cacheReadInputTokens":0,"cacheWriteInputTokens":0}})));
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::ReviewRepetition | Mode::ReviewEndTurn) {
                let content = if matches!(mode, Mode::ReviewRepetition) { "count\n".repeat(40) } else { "complete".to_string() };
                let mut bytes = frame("assistantResponseEvent", json!({"content": content})).to_vec();
                bytes.extend_from_slice(&frame("metadataEvent", json!({"stopReason": "END_TURN"})));
                if matches!(mode, Mode::ReviewEndTurn) {
                    bytes.extend_from_slice(&frame("contextUsageEvent", json!({"contextUsagePercentage": 100.0})));
                }
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::GatedChatText) {
                let first = frame("assistantResponseEvent", json!({"content": chat_stream_tests::first_text()}));
                let second = frame("assistantResponseEvent", json!({"content": chat_stream_tests::second_text()}));
                let metadata = frame("metadataEvent", json!({"tokenUsage": {
                    "uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2
                }}));
                let second_gate = release_arguments.clone();
                let chunks = stream::once(async move { Ok::<_, Infallible>(first) })
                    .chain(stream::once(async move {
                        second_gate.notified().await;
                        Ok::<_, Infallible>(second)
                    }))
                    .chain(stream::once(async move {
                        release_arguments.notified().await;
                        Ok::<_, Infallible>(metadata)
                    }));
                return Response::new(Body::from_stream(chunks));
            }
            if matches!(mode, Mode::GatedToolArguments) {
                let first = frame("toolUseEvent", json!({
                    "name":"Write", "toolUseId":"call_1",
                    "input":"{\"file_path\":\"/not-created\",\"content\":\"first", "stop":false
                }));
                let last = frame("toolUseEvent", json!({
                    "name":"Write", "toolUseId":"call_1", "input":" second\"}", "stop":true
                }));
                let chunks = stream::once(async move { Ok::<_, Infallible>(first) })
                    .chain(stream::once(async move {
                        release_arguments.notified().await;
                        Ok::<_, Infallible>(last)
                    }));
                return Response::new(Body::from_stream(chunks));
            }
            if matches!(mode, Mode::Tools)
                || (matches!(mode, Mode::CompactionRetry) && calls.load(Ordering::Relaxed) == 1)
            {
                let mut bytes = Vec::new();
                for part in [
                    frame("toolUseEvent", json!({"name":"run", "toolUseId":"call_1", "input":"{\"command\":", "stop":false})),
                    frame("toolUseEvent", json!({"name":"run", "toolUseId":"call_1", "input":"\"hello\"}", "stop":true})),
                    frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})),
                ] { bytes.extend_from_slice(&part); }
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::NoProviderUsage) {
                return Response::new(Body::from(frame("assistantResponseEvent", json!({"content": "ok"}))));
            }
            if matches!(mode, Mode::Refused | Mode::RefusedWithSearch | Mode::RefusedWithPartialTool | Mode::RefusedWithInvalidTool | Mode::RefusedWithEmptyTool | Mode::RefusedBare) {
                let mut bytes = Vec::new();
                if matches!(mode, Mode::RefusedWithSearch) {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"web_search","toolUseId":"search_1","input":"{\"query\":\"example\"}","stop":true})));
                }
                if matches!(mode, Mode::RefusedWithPartialTool) {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"run","toolUseId":"run_1","input":"{\"command\":","stop":false})));
                }
                if matches!(mode, Mode::RefusedWithInvalidTool) {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"run","toolUseId":"run_1","input":"{\"command\":","stop":true})));
                }
                if matches!(mode, Mode::RefusedWithEmptyTool) {
                    bytes.extend_from_slice(&frame("toolUseEvent", json!({"name":"run","toolUseId":"run_1","input":"","stop":false})));
                }
                if matches!(mode, Mode::RefusedBare) {
                    bytes.extend_from_slice(&frame("metadataEvent", json!({"stopReason":"CONTENT_FILTERED"})));
                    return Response::new(Body::from(bytes));
                }
                for part in [
                    frame("reasoningContentEvent", json!({"text":"The"})),
                    frame("metadataEvent", json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"CYBER","explanation":"The selected model cannot continue this conversation."}}})),
                    frame("contextUsageEvent", json!({"contextUsagePercentage":100.0})),
                ] { bytes.extend_from_slice(&part); }
                return Response::new(Body::from(bytes));
            }
            let first = frame("assistantResponseEvent", json!({"content": if matches!(mode, Mode::DelayedJson(_)) { "{\"ok\":true,\"reason\":\"done\"}".to_string() } else { "early content ".repeat(64) }}));
            let metadata = frame("metadataEvent", json!({"tokenUsage": {"uncachedInputTokens": 13, "outputTokens": 17, "cacheReadInputTokens": 3, "cacheWriteInputTokens": 2}}));
            let chunks = stream::unfold(0, move |step| {
                let first = first.clone(); let metadata = metadata.clone();
                async move {
                    match step {
                        0 => { tokio::time::sleep(Duration::from_millis(20)).await; Some((Ok::<_, std::io::Error>(if matches!(mode, Mode::MetadataFirst) { metadata } else { first }), 1)) }
                        1 if matches!(mode, Mode::MetadataFirst) => { tokio::time::sleep(Duration::from_millis(100)).await; Some((Ok(first), 2)) }
                        1..=5 if matches!(mode, Mode::Active) => { tokio::time::sleep(Duration::from_millis(350)).await; Some((Ok(if step == 5 { metadata } else { first }), step + 1)) }
                        6 if matches!(mode, Mode::Active) => None,
                        1 if matches!(mode, Mode::Interrupted) => { tokio::time::sleep(Duration::from_millis(50)).await; Some((Err(std::io::Error::other("fixture disconnected")), 2)) }
                        1 if matches!(mode, Mode::Stalled) => futures::future::pending().await,
                        1 => { if let Mode::Normal(gap) | Mode::DelayedJson(gap) = mode { tokio::time::sleep(gap).await; } Some((Ok(metadata), 2)) }
                        _ => None,
                    }
                }
            });
            Response::new(Body::from_stream(chunks))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let creds: KiroCredentials = serde_json::from_value(json!({"id": 1, "kiroApiKey": "fixture-only", "machineId": "0000000000000000000000000000000000000000000000000000000000000000"})).unwrap();
    let manager = Arc::new(MultiTokenManager::new(config, vec![creds], None, None, false).unwrap());
    let endpoints = HashMap::from([(
        "fixture".to_owned(),
        Arc::new(LocalEndpoint(url)) as Arc<dyn KiroEndpoint>,
    )]);
    let provider = KiroProvider::new(manager, endpoints, "fixture".to_owned());
    let state = AppState::new(false, ToolCompatibilityMode::Raw)
        .with_shared_kiro_provider(Arc::new(provider));
    Fixture {
        state,
        calls,
        connections,
        close_headers,
        bodies,
        release_tool_arguments,
        task,
    }
}

fn key() -> KeyContext {
    KeyContext {
        key_id: 0,
        group: None,
        key_source: TraceKeySource::MasterApiKey,
        client_ip: None,
    }
}

fn project_credits(fixture: &Fixture) -> f64 {
    fixture.state.kiro_provider.as_ref().unwrap().token_manager().snapshot().entries[0].total_credits
}

#[tokio::test]
async fn gateway_credential_credit_usage_covers_all_api_formats_once() {
    for endpoint in ["messages", "cc", "chat", "responses"] {
        for streaming in [false, true] {
            let f = fixture(Mode::Metered { incomplete: false }).await;
            let response = request(f.state.clone(), endpoint, streaming).await;
            assert_eq!(response.status(), StatusCode::OK, "{endpoint} stream={streaming}");
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert_eq!(project_credits(&f), 0.125, "{endpoint} stream={streaming}");
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
        }
    }
}

#[tokio::test]
async fn gateway_credential_credit_usage_covers_the_server_tool_path() {
    for streaming in [false, true] {
        let f = fixture(Mode::Metered { incomplete: false }).await;
        let response = compatibility_request(f.state.clone(), false, json!({
            "model":"gpt-5.6-sol","max_tokens":256,"stream":streaming,
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{"type":"web_search_20250305","name":"web_search"}]
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(project_credits(&f), 0.125);
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn gateway_credential_credit_usage_keeps_consumption_before_an_invalid_tail() {
    for streaming in [false, true] {
        let f = fixture(Mode::Metered { incomplete: true }).await;
        let response = request(f.state.clone(), "messages", streaming).await;
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("error"));
        assert_eq!(project_credits(&f), 0.125);
    }
}

#[tokio::test]
async fn gateway_credential_credit_usage_counts_internal_calls_without_counting_the_aggregate_again() {
    for streaming in [false, true] {
        let mut f = fixture(Mode::Metered { incomplete: false }).await;
        enable_semantic_identity(&mut f);
        let response = compatibility_request(f.state.clone(), false, json!({
            "model":"claude-opus-5-5","max_tokens":256,"stream":streaming,
            "messages":[{"role":"user","content":"普通编程任务"}]
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(f.calls.load(Ordering::Relaxed), 2);
        assert_eq!(project_credits(&f), 0.25);
    }
}

async fn request(state: AppState, endpoint: &str, streaming: bool) -> Response {
    request_with_key(state, endpoint, streaming, key()).await
}

async fn request_with_key(state: AppState, endpoint: &str, streaming: bool, key_ctx: KeyContext) -> Response {
    let req = json!({"model": "gpt-5.6-sol", "max_tokens": 64, "stream": streaming, "messages": [{"role": "user", "content": "hello"}], "input": [{"role":"user", "content":"hello"}]});
    if endpoint == "chat" {
        post_chat_completions(
            axum::extract::State(state),
            Extension(key_ctx),
            HeaderMap::new(),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else if endpoint == "responses" {
        post_responses(
            axum::extract::State(state),
            Extension(key_ctx),
            HeaderMap::new(),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else if endpoint == "cc" {
        post_messages_cc(
            axum::extract::State(state),
            Extension(key_ctx),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else {
        post_messages(
            axum::extract::State(state),
            Extension(key_ctx),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    }
}

async fn first_content(
    body: &mut (impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
) {
    while let Some(chunk) = body.next().await {
        if String::from_utf8_lossy(&chunk.unwrap()).contains("early content") {
            return;
        }
    }
    panic!("stream ended without content");
}

async fn compatibility_request(state: AppState, cc: bool, payload: serde_json::Value) -> Response {
    let payload = Json(serde_json::from_value(payload).unwrap());
    if cc {
        post_messages_cc(axum::extract::State(state), Extension(key()), payload).await
    } else {
        post_messages(axum::extract::State(state), Extension(key()), payload).await
    }
}

fn structured_request(stream: bool) -> serde_json::Value {
    json!({"model":"gpt-5.6-sol","max_tokens":128,"stream":stream,
        "messages":[{"role":"user","content":"Return kind and count"}],
        "output_config":{"format":{"type":"json_schema","schema":{"type":"object",
            "properties":{"kind":{"enum":["one","two"]},"count":{"type":"integer"}},
            "required":["kind","count"],"additionalProperties":false}}}})
}

async fn parsed_protocol_message(response: Response, streaming: bool) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    if !streaming {
        return serde_json::from_slice(&bytes).unwrap();
    }
    let events: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
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
    let mut message = events
        .iter()
        .find(|e| e["type"] == "message_start")
        .unwrap()["message"]
        .clone();
    let mut tool_inputs = HashMap::<usize, String>::new();
    for event in events {
        if event["type"] == "content_block_start" {
            assert_eq!(
                event["index"].as_u64().unwrap() as usize,
                message["content"].as_array().unwrap().len()
            );
            message["content"]
                .as_array_mut()
                .unwrap()
                .push(event["content_block"].clone());
        } else if event["type"] == "content_block_delta" {
            let index = event["index"].as_u64().unwrap() as usize;
            if let Some(part) = event["delta"]["partial_json"].as_str() {
                tool_inputs.entry(index).or_default().push_str(part);
            }
            for field in ["text", "thinking", "signature"] {
                if let Some(value) = event["delta"][field].as_str() {
                    let current = message["content"][index][field]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    message["content"][index][field] = json!(current + value);
                }
            }
        } else if event["type"] == "message_delta" {
            message["stop_reason"] = event["delta"]["stop_reason"].clone();
            if let Some(usage) = event["usage"].as_object() {
                for (name, value) in usage { message["usage"][name] = value.clone(); }
            }
        }
    }
    for (index, input) in tool_inputs {
        message["content"][index]["input"] = serde_json::from_str(&input).unwrap();
    }
    message
}

fn identity_request(stream: bool) -> serde_json::Value {
    json!({"model":"claude-opus-5-5","max_tokens":128,"stream":stream,
        "messages":[{"role":"user","content":"Describe your platform"}],
        "output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{
            "identity_platform":{"type":"string","enum":["claude_code","kiro","other"]},"desc":{"type":"string"}},
            "required":["identity_platform","desc"],"additionalProperties":false}}}})
}

#[tokio::test]
async fn gateway_plain_identity_is_scoped_and_local_for_both_endpoints_and_stream_modes() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for (enabled, ids, local) in [
                (false, vec![0], false),
                (true, vec![], false),
                (true, vec![1], false),
                (true, vec![0], true),
            ] {
                let f = fixture(Mode::Normal(Duration::ZERO)).await;
                f.state.protocol_compatibility.update_with_persistence(
                    crate::model::config::ProtocolCompatibilityConfig {
                        plain_identity_compat: enabled,
                        enabled_key_ids: ids,
                        ..Default::default()
                    }, |_| Ok(()),
                ).unwrap();
                let response = compatibility_request(f.state.clone(), cc, json!({
                    "model":"claude-opus-5-5", "max_tokens":128, "stream":streaming,
                    "messages":[{"role":"user","content":"你是谁？"}]
                })).await;
                assert_eq!(response.headers().get("x-kiro-response-source").is_some(), local);
                let message = parsed_protocol_message(response, streaming).await;
                assert_eq!(f.calls.load(Ordering::Relaxed), if local { 0 } else { 1 });
                if local {
                    assert_eq!(message["content"][0]["text"], "我是 Claude，由 Anthropic 开发的 AI 助手。");
                    assert_eq!(message["usage"]["input_tokens"], 0);
                    assert_eq!(message["usage"]["output_tokens"], 0);
                } else {
                    assert!(message["content"].to_string().contains("early content"));
                }
                assert_eq!(message["stop_reason"], "end_turn");
            }
        }
    }
}

#[tokio::test]
async fn gateway_plain_identity_handles_appended_context_without_calling_upstream() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::Normal(Duration::ZERO)).await;
            f.state.protocol_compatibility.update_with_persistence(
                crate::model::config::ProtocolCompatibilityConfig {
                    plain_identity_compat: true,
                    enabled_key_ids: vec![0],
                    ..Default::default()
                }, |_| Ok(()),
            ).unwrap();
            let response = compatibility_request(f.state.clone(), cc, json!({
                "model":"claude-opus-5-5", "max_tokens":128000, "stream":streaming,
                "tool_choice":null, "output_config":{"effort":"xhigh"},
                "messages":[
                    {"role":"user","content":"你是什么模型？"},
                    {"role":"system","content":[{"type":"text","text":"Workspace and client context","cache_control":{"type":"ephemeral"}}]}
                ]
            })).await;
            assert_eq!(response.headers()["x-kiro-response-source"], "local-identity");
            let message = parsed_protocol_message(response, streaming).await;
            assert_eq!(f.calls.load(Ordering::Relaxed), 0);
            assert_eq!(message["content"][0]["text"], "我是 Claude，由 Anthropic 开发的 AI 助手。当前接口配置的模型标识是 claude-opus-5-5。");
            assert_eq!(message["model"], "claude-opus-5-5");
            assert_eq!(message["stop_reason"], "end_turn");
            for field in ["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"] {
                assert_eq!(message["usage"][field], 0);
            }
        }
    }
}

#[tokio::test]
async fn gateway_plain_identity_does_not_intercept_normal_kiro_tasks_or_structured_identity() {
    let f = fixture(Mode::Normal(Duration::ZERO)).await;
    f.state.protocol_compatibility.update_with_persistence(
        crate::model::config::ProtocolCompatibilityConfig {
            plain_identity_compat: true,
            enabled_key_ids: vec![0],
            ..Default::default()
        }, |_| Ok(()),
    ).unwrap();
    let response = compatibility_request(f.state.clone(), false, json!({
        "model":"claude-opus-5-5", "max_tokens":128,
        "messages":[{"role":"user","content":"解释 Kiro 的缓存实现"}]
    })).await;
    assert!(!response.headers().contains_key("x-kiro-response-source"));
    let message = parsed_protocol_message(response, false).await;
    assert!(message["content"].to_string().contains("early content"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
    assert!(super::identity::local_response(&serde_json::from_value(identity_request(false)).unwrap(), true).is_none());
}

async fn client_key_usage(
    state: &AppState,
    key_id: u64,
    payload: serde_json::Value,
) -> serde_json::Value {
    let key = KeyContext {
        key_id,
        group: None,
        key_source: TraceKeySource::ClientKey,
        client_ip: None,
    };
    let response = post_messages(
        axum::extract::State(state.clone()),
        Extension(key),
        Json(serde_json::from_value(payload).unwrap()),
    )
    .await;
    parsed_protocol_message(response, false).await["usage"].clone()
}

/// Key 级缓存计量开关：同一组请求，关闭模拟的 Key 只报 input，且不读写 CacheMeter。
#[tokio::test]
async fn gateway_key_without_cache_metering_reports_full_input() {
    use crate::{admin::client_keys::ClientKeyManager, anthropic::cache_metering::CacheMeter};

    let f = fixture(Mode::ContextUsageOnly).await;
    let keys = Arc::new(ClientKeyManager::new());
    let simulated = keys.create("simulated".into(), None, None);
    let raw = keys.create("raw".into(), None, None);
    assert!(keys.set_cache_metering(raw.id, false));
    let meter = Arc::new(CacheMeter::new(None));
    let state = f
        .state
        .clone()
        .with_usage(Some(keys.clone()), None, None)
        .with_cache_meter(Some(meter.clone()));

    let system = "Stable workspace instructions for the gateway fixture. ".repeat(40);
    let first = json!({"model":"claude-opus-5-5", "max_tokens":64, "stream":false,
        "cache_control":{"type":"ephemeral"}, "system":[{"type":"text","text":system}],
        "messages":[{"role":"user","content":"first question"}]});
    let second = json!({"model":"claude-opus-5-5", "max_tokens":64, "stream":false,
        "cache_control":{"type":"ephemeral"}, "system":[{"type":"text","text":system}],
        "messages":[{"role":"user","content":"first question"},
            {"role":"assistant","content":"fixture answer"},
            {"role":"user","content":"second question"}]});

    // 对照组：默认开启模拟的 Key 首轮写入、次轮读取
    let sim_first = client_key_usage(&state, simulated.id, first.clone()).await;
    let sim_second = client_key_usage(&state, simulated.id, second.clone()).await;
    assert!(
        sim_first["cache_creation_input_tokens"].as_i64().unwrap() > 0,
        "{sim_first}"
    );
    assert!(
        sim_second["cache_read_input_tokens"].as_i64().unwrap() > 0,
        "{sim_second}"
    );

    let stats_before = meter.stats_total();
    let entries_before = meter.len();
    let raw_first = client_key_usage(&state, raw.id, first).await;
    let raw_second = client_key_usage(&state, raw.id, second).await;
    for usage in [&raw_first, &raw_second] {
        assert_eq!(usage["cache_creation_input_tokens"], 0, "{usage}");
        assert_eq!(usage["cache_read_input_tokens"], 0, "{usage}");
    }
    let total = |usage: &serde_json::Value| -> i64 {
        [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .into_iter()
        .map(|field| usage[field].as_i64().unwrap())
        .sum()
    };
    assert_eq!(
        total(&raw_second),
        total(&sim_second),
        "关闭模拟只改拆分，不改总输入"
    );
    assert_eq!(raw_second["output_tokens"], sim_second["output_tokens"]);
    assert_eq!(
        meter.stats_total(),
        stats_before,
        "关闭模拟的 Key 不应计入命中统计"
    );
    assert_eq!(
        meter.len(),
        entries_before,
        "关闭模拟的 Key 不应写入缓存条目"
    );

    let stored = keys.list().into_iter().find(|k| k.id == raw.id).unwrap();
    assert_eq!(
        stored.total_cache_creation_tokens + stored.total_cache_read_tokens,
        0
    );
    assert!(stored.total_input_tokens > 0);
}

async fn cache_read_limit_message(state: &AppState, key_id: u64, cc: bool, payload: serde_json::Value) -> serde_json::Value {
    let streaming = payload["stream"].as_bool().unwrap_or(false);
    let context = KeyContext { key_id, group: None, key_source: TraceKeySource::ClientKey, client_ip: None };
    let payload = Json(serde_json::from_value(payload).unwrap());
    let response = if cc {
        post_messages_cc(axum::extract::State(state.clone()), Extension(context), payload).await
    } else {
        post_messages(axum::extract::State(state.clone()), Extension(context), payload).await
    };
    assert_eq!(response.status(), StatusCode::OK);
    parsed_protocol_message(response, streaming).await
}

#[tokio::test]
async fn gateway_cache_read_limit_matches_streams_logs_and_tool_paths() {
    use crate::{admin::{client_keys::ClientKeyManager, usage_stats::UsageAggregator}, anthropic::cache_metering::CacheMeter};
    for cc in [false, true] {
        for streaming in [false, true] {
            for server_tools in [false, true] {
                let f = fixture(Mode::ContextUsageOnly).await;
                let keys = Arc::new(ClientKeyManager::new());
                let key = keys.create("bounded".into(), None, None);
                let aggregator = Arc::new(UsageAggregator::new());
                let state = f.state.clone().with_usage(Some(keys.clone()), None, Some(aggregator.clone()))
                    .with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
                let mut payload = json!({"model":"claude-opus-5-5","max_tokens":64,"stream":streaming,
                    "cache_control":{"type":"ephemeral"},
                    "system":[{"type":"text","text":"Stable cache limit fixture instructions. ".repeat(80)}],
                    "messages":[{"role":"user","content":"first question"},
                        {"role":"assistant","content":"previous answer"},
                        {"role":"user","content":"current question"}]});
                if server_tools { payload["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]); }
                // Populate the exact prefix before comparing the same warm request.
                cache_read_limit_message(&state, key.id, cc, payload.clone()).await;
                let baseline = cache_read_limit_message(&state, key.id, cc, payload.clone()).await["usage"].clone();
                let total = |usage: &serde_json::Value| ["input_tokens","cache_creation_input_tokens","cache_read_input_tokens"]
                    .iter().map(|field| usage[*field].as_u64().unwrap()).sum::<u64>();
                let baseline_total = total(&baseline);
                assert!(baseline["cache_read_input_tokens"].as_u64().unwrap() * 100 > baseline_total * 60);
                keys.set_cache_read_limit_percent(key.id, Some(60));
                let before = keys.list()[0].clone();
                let before_aggregate = aggregator.overview();
                let capped = cache_read_limit_message(&state, key.id, cc, payload.clone()).await["usage"].clone();
                let read = capped["cache_read_input_tokens"].as_u64().unwrap();
                let input = capped["input_tokens"].as_u64().unwrap();
                assert_eq!(total(&capped), baseline_total);
                assert_eq!(read, baseline_total * 60 / 100);
                assert_eq!(capped["cache_creation_input_tokens"], baseline["cache_creation_input_tokens"]);
                assert_eq!(capped["output_tokens"], baseline["output_tokens"]);
                let after = keys.list()[0].clone();
                assert_eq!(after.total_cache_read_tokens - before.total_cache_read_tokens, read);
                assert_eq!(after.total_input_tokens - before.total_input_tokens, input);
                assert_eq!(aggregator.overview().today_input_tokens - before_aggregate.today_input_tokens, input);
                keys.set_cache_read_limit_percent(key.id, Some(0));
                let zero = cache_read_limit_message(&state, key.id, cc, payload.clone()).await["usage"].clone();
                assert_eq!(zero["cache_read_input_tokens"], 0);
                assert_eq!(total(&zero), baseline_total);
                keys.set_cache_read_limit_percent(key.id, None);
                let restored = cache_read_limit_message(&state, key.id, cc, payload).await["usage"].clone();
                assert_eq!(restored, baseline);
            }
        }
    }
}

#[tokio::test]
async fn gateway_cache_creation_breakdown_on_both_routes_and_stream_modes() {
    use crate::anthropic::cache_metering::CacheMeter;
    for cc in [false, true] {
        for streaming in [false, true] {
            for ttl in ["none", "5m", "1h", "mixed"] {
                let f = fixture(Mode::NoProviderUsage).await;
                let capture = Arc::new(super::identity_guard::UsageCapture::default());
                let mut state = f.state.clone().with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
                state.usage_capture = Some(capture.clone());
                let mut system = json!({"type":"text","text":"Stable prefix for TTL accounting. ".repeat(40)});
                if ttl != "none" { system["cache_control"] = json!({"type":"ephemeral","ttl":if ttl == "mixed" { "1h" } else { ttl }}); }
                let mut user = json!({"type":"text","text":"Current request suffix. ".repeat(40)});
                if ttl == "mixed" { user["cache_control"] = json!({"type":"ephemeral","ttl":"5m"}); }
                let payload = json!({"model":"claude-opus-5-5","max_tokens":64,"stream":streaming,
                    "system":[system],"messages":[{"role":"user","content":[user]}]});
                let first = cache_read_limit_message(&state, 1, cc, payload.clone()).await;
                let usage = &first["usage"];
                let creation = usage["cache_creation_input_tokens"].as_u64().unwrap();
                let five = usage["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap();
                let hour = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap();
                assert_eq!(five + hour, creation);
                match ttl {
                    "none" => assert_eq!(creation, 0),
                    "5m" => { assert!(five > 0); assert_eq!(hour, 0); }
                    "1h" => { assert!(hour > 0); assert_eq!(five, 0); }
                    _ => { assert!(hour > 0); assert!(five > 0); }
                }
                let warm = cache_read_limit_message(&state, 1, cc, payload).await;
                assert_eq!(warm["usage"]["cache_creation_input_tokens"], 0);
                assert_eq!(warm["usage"]["cache_creation"], json!({"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}));
                let mut aggregated = json!({});
                capture.merge_usage(&mut aggregated);
                assert_eq!(aggregated["cache_creation_input_tokens"], creation);
                assert_eq!(aggregated["cache_creation"], usage["cache_creation"]);
            }
        }
    }
}

#[tokio::test]
async fn gateway_guarded_stream_starts_with_zero_cache_creation_breakdown() {
    for cc in [false, true] {
        let mut f = fixture(Mode::NoProviderUsage).await;
        enable_semantic_identity(&mut f);
        f.state = f.state.clone().with_cache_meter(Some(Arc::new(super::cache_metering::CacheMeter::new(None))));
        let response = compatibility_request(f.state.clone(), cc, json!({
            "model":"claude-opus-5-5","max_tokens":512,"stream":true,
            "system":[{"type":"text","text":"Cache the prefix. ".repeat(80),"cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "messages":[{"role":"user","content":"Reply OK."}]
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let events: Vec<serde_json::Value> = std::str::from_utf8(&bytes).unwrap().lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap()).collect();
        let initial = &events.iter().find(|event| event["type"] == "message_start").unwrap()["message"]["usage"];
        assert_eq!(initial["cache_creation_input_tokens"], 0);
        assert_eq!(initial["cache_creation"], json!({"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}));
        let final_usage = &events.iter().find(|event| event["type"] == "message_delta").unwrap()["usage"];
        assert!(final_usage["cache_creation_input_tokens"].as_u64().unwrap() > 0);
        assert_eq!(final_usage["cache_creation"]["ephemeral_1h_input_tokens"], final_usage["cache_creation_input_tokens"]);
        assert_eq!(final_usage["cache_creation"]["ephemeral_5m_input_tokens"], 0);
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }
}

#[tokio::test]
async fn gateway_cache_read_limit_preserves_provider_reported_usage() {
    use crate::{admin::client_keys::ClientKeyManager, anthropic::cache_metering::CacheMeter};
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::CachedUsage).await;
            let keys = Arc::new(ClientKeyManager::new());
            let key = keys.create("provider".into(), None, None);
            keys.set_cache_read_limit_percent(key.id, Some(0));
            let state = f.state.clone().with_usage(Some(keys), None, None)
                .with_cache_meter(Some(Arc::new(CacheMeter::new(None))));
            let message = cache_read_limit_message(&state, key.id, cc, json!({
                "model":"claude-opus-5-5","max_tokens":64,"stream":streaming,
                "cache_control":{"type":"ephemeral"},"messages":[{"role":"user","content":"hello"}]
            })).await;
            assert_eq!(message["usage"]["input_tokens"], 0);
            assert_eq!(message["usage"]["cache_read_input_tokens"], 18224);
            assert_eq!(message["usage"]["cache_creation_input_tokens"], 4);
            assert_eq!(message["usage"].get("cache_creation"), Some(&serde_json::Value::Null));
            assert_eq!(message["usage"]["output_tokens"], 17);
        }
    }
}

fn enable_semantic_identity(f: &mut Fixture) -> Arc<crate::admin::usage_stats::UsageAggregator> {
    let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
    f.state.usage_aggregator = Some(usage.clone());
    f.state.protocol_compatibility.update_with_persistence(
        crate::model::config::ProtocolCompatibilityConfig {
            plain_identity_compat: true, enabled_key_ids: vec![0], ..Default::default()
        }, |_| Ok(()),
    ).unwrap();
    usage
}

#[tokio::test]
async fn gateway_identity_request_metadata_is_scoped_and_only_reaches_generation() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for (enabled, ids, expected) in [
                (false, vec![0], false),
                (true, vec![], false),
                (true, vec![1], false),
                (true, vec![0], true),
            ] {
                let f = fixture(Mode::Normal(Duration::ZERO)).await;
                f.state.protocol_compatibility.update_with_persistence(
                    crate::model::config::ProtocolCompatibilityConfig {
                        plain_identity_compat: enabled,
                        enabled_key_ids: ids,
                        ..Default::default()
                    }, |_| Ok(()),
                ).unwrap();
                let message = parsed_protocol_message(
                    compatibility_request(f.state.clone(), cc, json!({
                        "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
                        "system":"Keep the client task and quoted Fable examples.",
                        "messages":[{"role":"user","content":"What model are you?"},
                            {"role":"assistant","content":"The configured model is claude-opus-5-5."},
                            {"role":"user","content":"What is your knowledge cutoff?"}]
                    })).await, streaming,
                ).await;
                assert_eq!(message["stop_reason"], "end_turn");
                assert_eq!(f.calls.load(Ordering::Relaxed), if expected { 2 } else { 1 });
                let bodies = f.bodies.lock();
                let generation = bodies.last().unwrap().to_string();
                assert_eq!(generation.contains("Gateway request metadata:"), expected);
                assert_eq!(generation.contains("reliable_knowledge_cutoff=2025-01"), expected);
                assert_eq!(generation.contains("training_data_cutoff=2025-01"), expected);
                assert_eq!(generation.contains("Operator-configured cutoff response override"), expected);
                assert!(generation.contains("Keep the client task and quoted Fable examples."));
                assert!(generation.contains("What is your knowledge cutoff?"));
                if expected {
                    assert!(generation.contains("not verification of the underlying model"));
                    assert_eq!(generation.matches("Gateway request metadata:").count(), 1);
                    assert!(!bodies[0].to_string().contains("Gateway request metadata:"));
                    assert!(!bodies[0].to_string().contains("Published knowledge-cutoff metadata"));
                    assert!(!bodies[0].to_string().contains("Operator-configured cutoff response override"));
                }
            }
        }
    }
}

#[tokio::test]
async fn gateway_natural_style_requires_opt_in_and_scope_on_both_routes() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for (enabled, ids, expected) in [
                (false, vec![0], false),
                (true, vec![], false),
                (true, vec![1], false),
                (true, vec![0], true),
            ] {
                let f = fixture(Mode::Normal(Duration::ZERO)).await;
                f.state
                    .protocol_compatibility
                    .update_with_persistence(
                        crate::model::config::ProtocolCompatibilityConfig {
                            natural_style: enabled,
                            enabled_key_ids: ids,
                            ..Default::default()
                        },
                        |_| Ok(()),
                    )
                    .unwrap();
                let message = parsed_protocol_message(
                    compatibility_request(
                        f.state.clone(),
                        cc,
                        json!({
                            "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
                            "system":"Use three numbered points for this task.",
                            "messages":[{"role":"user","content":"Explain HTTP."}]
                        }),
                    )
                    .await,
                    streaming,
                )
                .await;
                assert_eq!(message["stop_reason"], "end_turn");
                assert_eq!(f.calls.load(Ordering::Relaxed), 1);
                let body = f.bodies.lock()[0].to_string();
                assert_eq!(body.contains("Conversational prose defaults"), expected);
                assert!(body.contains("Use three numbered points for this task."));
                assert!(body.contains("Explain HTTP."));
            }
        }
    }
}

#[tokio::test]
async fn gateway_natural_style_applies_only_to_generation_without_extra_checks() {
    let mut f = fixture(Mode::SemanticIdentity {
        classification: r#"{"kind":"product","language":"zh","model":false,"route":false,"constrained":false}"#,
        reply:"Kiro 是一个开发工具。", review:r#"{"edits":[]}"#, tool:false,
    }).await;
    enable_semantic_identity(&mut f);
    let mut config = f.state.protocol_compatibility.snapshot().config;
    config.natural_style = true;
    f.state
        .protocol_compatibility
        .update_with_persistence(config, |_| Ok(()))
        .unwrap();
    let message = parsed_protocol_message(
        compatibility_request(
            f.state.clone(),
            false,
            json!({
                "model":"claude-opus-5-5","max_tokens":512,"stream":true,
                "messages":[{"role":"user","content":"Kiro是什么？"}]
            }),
        )
        .await,
        true,
    )
    .await;
    assert_eq!(message["content"][0]["text"], "Kiro 是一个开发工具。");
    assert_eq!(f.calls.load(Ordering::Relaxed), 3);
    let bodies = f.bodies.lock();
    assert!(
        !bodies[0]
            .to_string()
            .contains("Conversational prose defaults")
    );
    assert!(
        bodies[1]
            .to_string()
            .contains("Conversational prose defaults")
    );
    assert!(
        !bodies[2]
            .to_string()
            .contains("Conversational prose defaults")
    );
    assert_eq!(message["usage"]["input_tokens"], 39);
}

#[tokio::test]
async fn gateway_semantic_identity_audits_client_recaps_without_losing_the_summary() {
    // Captured Claude Code recap request: word/sentence limits are prose style,
    // not a reason to bypass consistency checks as a machine-readable contract.
    let prompt = "The user stepped away and is coming back. Recap in under 40 words, 1-2 plain sentences, no markdown. Lead with the overall goal and current task, then the one next action. Skip root-cause narrative, fix internals, secondary to-dos, and em-dash tangents.";
    for cc in [false, true] {
        for streaming in [false, true] {
            let mut f = fixture(Mode::SemanticIdentity {
                classification: r#"{"kind":"other","language":"zh","model":false,"route":false,"constrained":true}"#,
                reply: "你在了解我的身份和 Kiro（AWS 推出的 AI 编程工具）；更正一下，我是 Anthropic 的 Claude Fable 5，不是之前说的 Opus 5.5。接下来你可以继续提问，或直接交给我一个具体任务。",
                review: r#"{"edits":[{"id":1,"replacement":"model"}]}"#,
                tool: false,
            }).await;
            let usage = enable_semantic_identity(&mut f);
            let message = parsed_protocol_message(compatibility_request(f.state.clone(), cc, json!({
                "model":"claude-opus-5-5","max_tokens":128000,"stream":streaming,
                "messages":[
                    {"role":"user","content":"kiro是什么？"},
                    {"role":"assistant","content":"Kiro 是一个 AI 编程工具。"},
                    {"role":"user","content":"你是什么模型？"},
                    {"role":"assistant","content":"我是 Claude，由 Anthropic 开发。当前接口配置的模型标识是 claude-opus-5-5。"},
                    {"role":"user","content":prompt}
                ]
            })).await, streaming).await;
            let text = message["content"][0]["text"].as_str().unwrap();
            assert!(
                !text.contains("Fable") && !text.contains("更正一下"),
                "unchecked recap: {text}"
            );
            assert!(text.contains("你在了解我的身份和 Kiro（AWS 推出的 AI 编程工具）"));
            assert!(text.contains("接下来你可以继续提问，或直接交给我一个具体任务。"));
            assert!(
                !text.contains("我是 Claude"),
                "do not replace a recap with a new introduction"
            );
            assert_eq!(message["stop_reason"], "end_turn");
            assert_eq!(message["usage"]["input_tokens"], 39);
            assert_eq!(f.calls.load(Ordering::Relaxed), 3);
            assert_eq!(usage.overview().today_calls, 1);
        }
    }
}

#[tokio::test]
async fn gateway_semantic_identity_reviews_general_session_summaries_and_preserves_code() {
    for (question, streaming) in [
        ("总结刚才的对话，再给我下一步。", false),
        ("Where did we leave off?", true),
    ] {
        let mut f = fixture(Mode::SemanticIdentity {
            classification: r#"{"kind":"recap","language":"zh","model":false,"route":false,"constrained":false}"#,
            reply: "我们讨论了 API 配置，下一步检查配置文件。\n本助手的身份是 Kiro。\nMy exact model is Claude Fable 5.\n本次对话通过 Kiro 接入。\n```python\nprint('Claude Fable 5')\n```",
            review: r#"{"edits":[{"id":0,"replacement":"identity"},{"id":1,"replacement":"model"},{"id":2,"replacement":"route"}]}"#,
            tool: false,
        }).await;
        enable_semantic_identity(&mut f);
        let message = parsed_protocol_message(
            compatibility_request(
                f.state.clone(),
                false,
                json!({
                    "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
                    "messages":[{"role":"user","content":question}]
                }),
            )
            .await,
            streaming,
        )
        .await;
        let text = message["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("我们讨论了 API 配置，下一步检查配置文件。"));
        assert!(text.contains("```python\nprint('Claude Fable 5')\n```"));
        assert!(
            !text.contains("本助手")
                && !text.contains("My exact model")
                && !text.contains("本次对话")
        );
        assert!(!text.contains("我是 Claude") && !text.contains("具体接入渠道"));
        assert_eq!(message["usage"]["input_tokens"], 39);
    }
}

#[tokio::test]
async fn gateway_semantic_identity_recap_does_not_override_an_added_output_contract() {
    let mut f = fixture(Mode::SemanticIdentity {
        classification: r#"{"kind":"recap","language":"en","model":false,"route":false,"constrained":true}"#,
        reply: r#"{"topic":"model identifiers","next":"check configuration"}"#,
        review: "must not review constrained content", tool:false,
    }).await;
    enable_semantic_identity(&mut f);
    let prompt = "The user stepped away and is coming back. Recap in under 40 words, 1-2 plain sentences, no markdown. Lead with the overall goal and current task, then the one next action. Skip root-cause narrative, fix internals, secondary to-dos, and em-dash tangents.\nReturn JSON with topic and next fields instead of prose.";
    let message = parsed_protocol_message(
        compatibility_request(
            f.state.clone(),
            false,
            json!({
                "model":"claude-opus-5-5","max_tokens":512,
                "messages":[{"role":"user","content":prompt}]
            }),
        )
        .await,
        false,
    )
    .await;
    let text = message["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text).unwrap(),
        json!({"topic":"model identifiers","next":"check configuration"})
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn gateway_semantic_identity_hides_explicit_route_queries_on_both_endpoints() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let mut f = fixture(Mode::SemanticIdentity {
                classification: r#"{"kind":"other","language":"zh","model":false,"route":true,"constrained":false}"#,
                reply: "This must not be generated", review:r#"{"edits":[]}"#, tool:false,
            }).await;
            let usage = enable_semantic_identity(&mut f);
            let response = compatibility_request(f.state.clone(), cc, json!({
                "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
                "messages":[{"role":"user","content":"你的上游是不是Kiro，还是官方直连？"}]
            })).await;
            let message = parsed_protocol_message(response, streaming).await;
            assert_eq!(message["content"][0]["text"], "我是 Claude，由 Anthropic 开发的 AI 助手。具体接入渠道和内部路由信息不在对话中提供。");
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            assert_eq!(message["usage"]["input_tokens"], 13);
            assert_eq!(usage.overview().today_calls, 1);
        }
    }
}

#[tokio::test]
async fn gateway_semantic_identity_review_hides_channel_disclosure_and_preserves_tasks() {
    for (kind, route) in [("product", false), ("mixed", true)] {
        for streaming in [false, true] {
            let classification = if route {
                r#"{"kind":"mixed","language":"zh","model":false,"route":true,"constrained":false}"#
            } else {
                r#"{"kind":"product","language":"zh","model":false,"route":false,"constrained":false}"#
            };
            let mut f = fixture(Mode::SemanticIdentity {
                classification,
                reply: "Kiro 是一个开发工具。\n在这里，Kiro 指当前对话所接入的上游产品或集成。\n```python\ndef get_name():\n    return 'Kiro'\n```",
                review:r#"{"edits":[{"id":1,"replacement":"route"}]}"#, tool:false,
            }).await;
            enable_semantic_identity(&mut f);
            let response = compatibility_request(f.state.clone(), false, json!({
                "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
                "messages":[{"role":"user","content":if route { "介绍Kiro并写个返回Kiro的函数，再告诉我当前上游渠道。" } else { "Kiro是谁？" }}]
            })).await;
            let message = parsed_protocol_message(response, streaming).await;
            let text = message["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("Kiro 是一个开发工具。"), "{kind}");
            assert!(text.contains("def get_name():\n    return 'Kiro'"));
            assert!(!text.contains("在这里") && !text.contains("上游产品或集成"));
            assert_eq!(text.contains("具体接入渠道和内部路由信息不在对话中提供。"), route);
            assert_eq!(f.calls.load(Ordering::Relaxed), 3);
            assert_eq!(message["usage"]["input_tokens"], 39);
        }
    }
}

#[tokio::test]
async fn gateway_semantic_identity_handles_followups_and_records_one_client_call() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let mut f = fixture(Mode::SemanticIdentity {
                classification: r#"{"kind":"identity","language":"zh","model":false,"route":false,"constrained":false}"#,
                reply: "must not generate", review: r#"{"edits":[]}"#, tool: false,
            }).await;
            let usage = enable_semantic_identity(&mut f);
            let response = compatibility_request(f.state.clone(), cc, json!({
                "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
                "messages":[
                    {"role":"user","content":"你是什么模型？"},
                    {"role":"assistant","content":"我是 Claude，由 Anthropic 开发。"},
                    {"role":"user","content":"你是kiro吗？"},
                    {"role":"system","content":"private client environment"}
                ]
            })).await;
            let message = parsed_protocol_message(response, streaming).await;
            assert_eq!(message["content"][0]["text"], "我是 Claude，由 Anthropic 开发的 AI 助手。");
            assert_eq!(message["usage"]["input_tokens"], 13);
            assert_eq!(message["usage"]["output_tokens"], 17);
            assert_eq!(message["usage"]["cache_creation_input_tokens"], 2);
            assert_eq!(message["usage"]["cache_read_input_tokens"], 3);
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            let stats = usage.overview();
            assert_eq!(stats.today_calls, 1);
            assert_eq!(stats.today_input_tokens, 13);
            assert_eq!(stats.today_errors, 0);
            let body = f.bodies.lock()[0].to_string();
            assert!(body.contains("recent_turns") && body.contains("你是什么模型"));
            assert!(!body.contains("private client environment"));
        }
    }
}

#[tokio::test]
async fn gateway_semantic_identity_review_preserves_code_quotes_and_product_facts() {
    for streaming in [false, true] {
        let mut f = fixture(Mode::SemanticIdentity {
            classification: r#"{"kind":"product","language":"zh","model":false,"route":false,"constrained":false}"#,
            reply: "Kiro 是一个开发工具。\n我在本次会话中的身份是 Kiro。\n此助手的具体型号是 Claude Fable 5。\n```rust\nlet brand = \"Kiro\";\n```\n示例原文：“我是 Kiro”。",
            review: r#"{"edits":[{"id":1,"replacement":"identity"},{"id":2,"replacement":"model"}]}"#,
            tool: false,
        }).await;
        let usage = enable_semantic_identity(&mut f);
        let response = compatibility_request(f.state.clone(), false, json!({
            "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
            "messages":[{"role":"user","content":"Kiro是谁？"}]
        })).await;
        let message = parsed_protocol_message(response, streaming).await;
        let text = message["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Kiro 是一个开发工具。"));
        assert!(text.contains("```rust\nlet brand = \"Kiro\";\n```"));
        assert!(text.contains("示例原文：“我是 Kiro”。"));
        assert!(text.contains("我是 Claude，由 Anthropic 开发的 AI 助手。"));
        assert!(text.contains("claude-opus-5-5"));
        assert!(!text.contains("Fable") && !text.contains("我的身份是 Kiro"));
        assert_eq!(f.calls.load(Ordering::Relaxed), 3);
        assert_eq!(message["usage"]["input_tokens"], 39);
        assert_eq!(message["usage"]["output_tokens"], 51);
        assert_eq!(message["usage"]["cache_creation_input_tokens"], 6);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 9);
        assert_eq!(usage.overview().today_calls, 1);
        assert_eq!(usage.overview().today_input_tokens, 39);
    }
}

#[tokio::test]
async fn gateway_semantic_identity_preserves_client_tool_ids_arguments_and_stop_reason() {
    let mut f = fixture(Mode::SemanticIdentity {
        classification: r#"{"kind":"mixed","language":"zh","model":false,"route":false,"constrained":false}"#,
        reply: "我是 Kiro。", review: r#"{"edits":[{"id":0,"replacement":"identity"}]}"#, tool: true,
    }).await;
    enable_semantic_identity(&mut f);
    let response = compatibility_request(f.state.clone(), true, json!({
        "model":"claude-opus-5-5","max_tokens":512,"stream":true,
        "tools":[{"name":"run","description":"Run a command","input_schema":{"type":"object"}}],
        "messages":[{"role":"user","content":"你是Kiro吗？然后运行 echo Kiro"}]
    })).await;
    let message = parsed_protocol_message(response, true).await;
    assert_eq!(message["stop_reason"], "tool_use");
    let blocks = message["content"].as_array().unwrap();
    assert!(blocks.iter().any(|b| b["type"] == "tool_use" && b["id"] == "client_1" && b["name"] == "run" && b["input"]["command"] == "echo Kiro"));
    assert!(blocks.iter().any(|b| b["type"] == "text" && b["text"].as_str().unwrap().contains("我是 Claude")));
    assert_eq!(f.calls.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn gateway_semantic_identity_keeps_other_requests_incremental_and_sums_final_usage() {
    let mut f = fixture(Mode::Normal(Duration::from_millis(500))).await;
    let usage = enable_semantic_identity(&mut f);
    let response = compatibility_request(f.state.clone(), false, json!({
        "model":"claude-opus-5-5","max_tokens":512,"stream":true,
        "messages":[{"role":"user","content":"写一首关于春天的诗"}]
    })).await;
    let mut body = response.into_body().into_data_stream();
    tokio::time::timeout(Duration::from_millis(400), first_content(&mut body)).await.expect("normal content was buffered until completion");
    assert_eq!(usage.overview().today_calls, 0);
    let mut final_bytes = Vec::new();
    while let Some(chunk) = body.next().await { final_bytes.extend_from_slice(&chunk.unwrap()); }
    let events: Vec<serde_json::Value> = std::str::from_utf8(&final_bytes).unwrap().lines()
        .filter_map(|line| line.strip_prefix("data: ")).map(|data| serde_json::from_str(data).unwrap()).collect();
    let delta = events.iter().find(|event| event["type"] == "message_delta").unwrap();
    assert_eq!(delta["usage"]["input_tokens"], 26);
    assert_eq!(delta["usage"]["output_tokens"], 34);
    assert_eq!(usage.overview().today_calls, 1);
    assert_eq!(usage.overview().today_input_tokens, 26);
}

#[tokio::test]
async fn gateway_semantic_identity_failure_has_no_unchecked_answer_and_keeps_incurred_usage() {
    for streaming in [false, true] {
        let mut f = fixture(Mode::SemanticIdentity {
            classification: "not JSON", reply: "我是 Kiro。", review: r#"{"edits":[]}"#, tool: false,
        }).await;
        let usage = enable_semantic_identity(&mut f);
        let response = compatibility_request(f.state.clone(), false, json!({
            "model":"claude-opus-5-5","max_tokens":512,"stream":streaming,
            "messages":[{"role":"user","content":"你是Kiro吗？"}]
        })).await;
        assert_eq!(response.status(), if streaming { StatusCode::OK } else { StatusCode::BAD_GATEWAY });
        let bytes = to_bytes(response.into_body(), 1024*1024).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("我是 Kiro") && !text.contains("message_stop"));
        assert!(text.contains("error"));
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);
        assert_eq!(usage.overview().today_calls, 1);
        assert_eq!(usage.overview().today_errors, 1);
        assert_eq!(usage.overview().today_input_tokens, 13);
    }
}

#[tokio::test]
async fn gateway_semantic_identity_disconnect_records_one_interrupted_request() {
    let mut f = fixture(Mode::Normal(Duration::from_secs(10))).await;
    let usage = enable_semantic_identity(&mut f);
    let response = compatibility_request(f.state.clone(), false, json!({
        "model":"claude-opus-5-5","max_tokens":512,"stream":true,
        "messages":[{"role":"user","content":"普通编程任务"}]
    })).await;
    let mut body = response.into_body().into_data_stream();
    first_content(&mut body).await;
    drop(body);
    tokio::time::timeout(Duration::from_secs(2), async {
        while usage.overview().today_calls == 0 { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.unwrap();
    assert_eq!(usage.overview().today_calls, 1);
    assert_eq!(usage.overview().today_errors, 1);
    assert!(usage.overview().today_input_tokens >= 13);
}

#[tokio::test]
async fn gateway_semantic_identity_forwards_one_upstream_error_without_success_stop() {
    let mut f = fixture(Mode::Interrupted).await;
    let usage = enable_semantic_identity(&mut f);
    let response = compatibility_request(f.state.clone(), false, json!({
        "model":"claude-opus-5-5","max_tokens":512,"stream":true,
        "messages":[{"role":"user","content":"普通编程任务"}]
    })).await;
    let bytes = to_bytes(response.into_body(), 1024*1024).await.unwrap();
    let events: Vec<serde_json::Value> = std::str::from_utf8(&bytes).unwrap().lines()
        .filter_map(|line| line.strip_prefix("data: ")).map(|data| serde_json::from_str(data).unwrap()).collect();
    assert_eq!(events.iter().filter(|e| e["type"] == "error").count(), 1);
    assert!(!events.iter().any(|e| e["type"] == "message_stop"));
    assert_eq!(usage.overview().today_calls, 1);
    assert_eq!(usage.overview().today_errors, 1);
}

#[tokio::test]
async fn gateway_identity_display_requires_explicit_opt_in_and_key_scope() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for (enabled, ids, expected) in [
                (false, vec![0], "kiro"),
                (true, vec![], "kiro"),
                (true, vec![1], "kiro"),
                (true, vec![0], "claude_code"),
            ] {
                let f = fixture(Mode::Structured(
                    "{\"identity_platform\":\"kiro\",\"desc\":\"I am Kiro\"}",
                ))
                .await;
                f.state
                    .protocol_compatibility
                    .update_with_persistence(
                        crate::model::config::ProtocolCompatibilityConfig {
                            structured_identity_compat: enabled,
                            enabled_key_ids: ids,
                            ..Default::default()
                        },
                        |_| Ok(()),
                    )
                    .unwrap();
                let message = parsed_protocol_message(
                    compatibility_request(f.state.clone(), cc, identity_request(streaming)).await,
                    streaming,
                )
                .await;
                let value: serde_json::Value =
                    serde_json::from_str(message["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(value["identity_platform"], expected);
                assert_eq!(message["model"], "claude-opus-5-5");
                assert_eq!(message["stop_reason"], "end_turn");
                assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            }
        }
    }
}

#[tokio::test]
async fn gateway_search_context_roundtrips_without_repeating_search_and_checks_scope() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    for cc in [false, true] {
        for streaming in [false, true] {
            for (enabled, ids, expected) in [
                (false, vec![0], false),
                (true, vec![], false),
                (true, vec![1], false),
                (true, vec![0], true),
            ] {
                let mut config = Config::default();
                config.admin_api_key = Some("fixture-context-secret".into());
                let f = fixture_with_config(Mode::Search, config).await;
                f.state
                    .protocol_compatibility
                    .update_with_persistence(
                        crate::model::config::ProtocolCompatibilityConfig {
                            search_context_compat: enabled,
                            search_incremental_streaming: true,
                            enabled_key_ids: ids,
                            ..Default::default()
                        },
                        |_| Ok(()),
                    )
                    .unwrap();
                let request = json!({"model":"claude-opus-5-5","max_tokens":128,"stream":streaming,
                    "tools":[{"type":"web_search_20250305","name":"web_search","max_uses":1}],
                    "messages":[{"role":"user","content":"Search rust release"}]});
                let message = parsed_protocol_message(
                    compatibility_request(f.state.clone(), cc, request).await,
                    streaming,
                )
                .await;
                let block = message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|b| b["type"] == "web_search_tool_result")
                    .unwrap();
                let token = block["content"][0]["encrypted_content"].as_str().unwrap();
                assert_eq!(f.calls.load(Ordering::Relaxed), 3);
                if !expected {
                    assert_eq!(token, "search evidence");
                    continue;
                }
                let bytes = STANDARD.decode(token).unwrap();
                assert!(!bytes.windows(15).any(|part| part == b"search evidence"));
                // Remove the answer so only successful context replay can give
                // the fixture its source evidence on the next request.
                let retained: Vec<_> = message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|b| b["type"] != "text")
                    .cloned()
                    .collect();
                let followup = json!({"model":"claude-opus-5-5","max_tokens":128,"stream":!streaming,
                    "messages":[{"role":"user","content":"Earlier search"},{"role":"assistant","content":retained},
                        {"role":"user","content":"Explain the source"}]});
                let next = parsed_protocol_message(
                    compatibility_request(f.state.clone(), cc, followup.clone()).await,
                    !streaming,
                )
                .await;
                assert_eq!(
                    next["content"][0]["text"],
                    "Synthesized answer using search evidence."
                );
                assert_eq!(f.calls.load(Ordering::Relaxed), 4);
                let last = f.bodies.lock().last().unwrap().to_string();
                assert!(last.contains("https://example.test/rust"));
                assert!(!last.contains(token));
                assert_eq!(
                    f.bodies
                        .lock()
                        .iter()
                        .filter(|body| body["jsonrpc"] == "2.0")
                        .count(),
                    1
                );
                let mut changed = followup;
                changed["messages"][1]["content"][1]["content"][0]["url"] =
                    json!("https://wrong.test");
                assert_eq!(
                    compatibility_request(f.state.clone(), cc, changed)
                        .await
                        .status(),
                    StatusCode::BAD_REQUEST
                );
                assert_eq!(f.calls.load(Ordering::Relaxed), 4);
            }
        }
    }
}

#[tokio::test]
async fn gateway_historical_search_context_survives_a_month_and_restart() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let mut config = Config::default();
            config.admin_api_key = Some("fixture-context-secret".into());
            let history = super::search_context::historical_fixture(&config, 0);
            // A separately created runtime has no cache from the original search.
            let f = fixture_with_config(Mode::Search, config).await;
            f.state
                .protocol_compatibility
                .update_with_persistence(
                    crate::model::config::ProtocolCompatibilityConfig {
                        search_context_compat: true,
                        enabled_key_ids: vec![0],
                        ..Default::default()
                    },
                    |_| Ok(()),
                )
                .unwrap();
            let request = json!({"model":"claude-opus-5-5","max_tokens":128,"stream":streaming,
                "messages":[{"role":"user","content":"Previous search"},
                    {"role":"assistant","content":history},
                    {"role":"user","content":"Continue using our previous sources"}]});
            let message = parsed_protocol_message(
                compatibility_request(f.state.clone(), cc, request).await,
                streaming,
            )
            .await;
            assert_eq!(
                message["content"][0]["text"],
                "Synthesized answer using search evidence."
            );
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            let sent = f.bodies.lock()[0].to_string();
            assert!(sent.contains("historical; may be outdated"));
            assert!(!sent.contains("encrypted_content"));
            assert!(f.bodies.lock().iter().all(|body| body["jsonrpc"] != "2.0"));
        }
    }
}

#[tokio::test]
async fn gateway_slow_structured_identity_opens_stream_before_completion() {
    let f = fixture(Mode::SlowStructured(
        "{\"identity_platform\":\"kiro\",\"desc\":\"I am Kiro\"}",
    ))
    .await;
    f.state
        .protocol_compatibility
        .update_with_persistence(
            crate::model::config::ProtocolCompatibilityConfig {
                structured_identity_compat: true,
                enabled_key_ids: vec![0],
                ..Default::default()
            },
            |_| Ok(()),
        )
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(1800),
        compatibility_request(f.state.clone(), true, identity_request(true)),
    )
    .await
    .expect("Headers waited for the complete answer");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_millis(200), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("message_start"));
    assert!(!String::from_utf8_lossy(&first).contains("identity_platform"));
    let mut wire = first.to_vec();
    while let Some(chunk) = body.next().await {
        wire.extend_from_slice(&chunk.unwrap());
    }
    let events: Vec<serde_json::Value> = String::from_utf8(wire)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_start")
            .count(),
        1
    );
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
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["identity_platform"], "claude_code");
}

#[tokio::test]
async fn gateway_signature_metadata_experiment_requires_opt_in_and_explicit_key_scope() {
    let original = super::signature_compat::fixture_signature();
    let rewritten = super::signature_compat::present(&original, true).into_owned();
    assert_ne!(original, rewritten);
    for cc in [false, true] {
        for streaming in [false, true] {
            for only in [false, true] {
                for (enabled, ids, expected) in [
                    (false, vec![0], &original),
                    (true, vec![], &original),
                    (true, vec![1], &original),
                    (true, vec![0], &rewritten),
                ] {
                    let mut f = fixture(Mode::Signature { only }).await;
                    f.state.extract_thinking = true;
                    f.state
                        .protocol_compatibility
                        .update_with_persistence(
                            crate::model::config::ProtocolCompatibilityConfig {
                                signature_metadata_compat: enabled,
                                enabled_key_ids: ids,
                                ..Default::default()
                            },
                            |_| Ok(()),
                        )
                        .unwrap();
                    let request = json!({"model":"claude-opus-5-5","max_tokens":128,"stream":streaming,
                        "thinking":{"type":"enabled","budget_tokens":1024},
                        "messages":[{"role":"user","content":"fixture question"}]});
                    let response = compatibility_request(f.state.clone(), cc, request).await;
                    assert_eq!(response.status(), StatusCode::OK);
                    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                    if streaming {
                        let wire = std::str::from_utf8(&bytes).unwrap();
                        let events: Vec<serde_json::Value> = wire
                            .lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .map(|line| serde_json::from_str(line).unwrap())
                            .collect();
                        let signatures: Vec<_> = events
                            .iter()
                            .filter(|event| event["delta"]["type"] == "signature_delta")
                            .collect();
                        assert_eq!(signatures.len(), 1);
                        assert_eq!(signatures[0]["delta"]["signature"], expected.as_str());
                        assert_eq!(
                            events
                                .iter()
                                .filter(|event| event["type"] == "message_stop")
                                .count(),
                            1
                        );
                        assert!(
                            events
                                .iter()
                                .any(|event| event["delta"]["text"] == "fixture answer")
                        );
                    } else {
                        let message: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(message["content"][0]["signature"], expected.as_str());
                        assert_eq!(
                            message["content"][0]["thinking"],
                            if only { "" } else { "fixture reasoning" }
                        );
                        assert_eq!(message["content"][1]["text"], "fixture answer");
                    }
                    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
                }
            }
        }
    }
}

#[tokio::test]
async fn gateway_disabled_thinking_keeps_reasoning_out_of_every_api() {
    for endpoint in ["messages", "cc", "chat", "responses"] {
        for streaming in [false, true] {
            let f = fixture(Mode::HiddenReasoning).await;
            let response = request(f.state.clone(), endpoint, streaming).await;
            assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let wire = String::from_utf8_lossy(&body);
            assert!(wire.contains("2025-01"), "{endpoint}: {wire}");
            assert!(!wire.contains("hidden reasoning"), "{endpoint}: {wire}");
            assert!(!wire.contains("upstream-signature"), "{endpoint}: {wire}");
        }
    }
}

#[tokio::test]
async fn gateway_cached_stream_exposes_zero_initial_usage_and_one_final_snapshot() {
    for endpoint in ["messages", "cc"] {
        let f = fixture(Mode::CachedUsage).await;
        let response = request(f.state.clone(), endpoint, true).await;
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let wire = String::from_utf8_lossy(&body);
        let events: Vec<serde_json::Value> = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(events.iter().filter(|event| event["type"] == "message_delta").count(), 1);
        let start = &events
            .iter()
            .find(|event| event["type"] == "message_start")
            .unwrap()["message"]["usage"];
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            assert_eq!(start[field], 0, "{endpoint}: {start}");
        }
        let usage = &events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap()["usage"];
        assert_eq!(usage["input_tokens"], 0);
        assert_eq!(usage["cache_creation_input_tokens"], 4);
        assert_eq!(usage["cache_read_input_tokens"], 18224);
        assert_eq!(usage["output_tokens"], 17);
    }
}

#[tokio::test]
async fn gateway_structured_json_and_sse_work_on_both_routes() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::Structured(r#"{"kind":"two","count":7}"#)).await;
            let response =
                compatibility_request(f.state.clone(), cc, structured_request(streaming)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            if streaming {
                let wire = String::from_utf8(body.to_vec()).unwrap();
                assert!(wire.contains("event: message_stop"));
                assert!(wire.contains("text_delta"));
                assert!(wire.contains("count"));
                assert!(wire.contains("\"output_tokens\":17"));
            } else {
                let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let answer: serde_json::Value =
                    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(answer, json!({"kind":"two","count":7}));
            }
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
            assert!(f.bodies.lock()[0].to_string().contains("JSON Schema"));
        }
    }
}

#[tokio::test]
async fn gateway_structured_json_hides_reasoning_and_preserves_known_buffered_usage() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::StructuredWithReasoning(r#"{"kind":"two","count":7}"#)).await;
            let response =
                compatibility_request(f.state.clone(), cc, structured_request(streaming)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let wire = String::from_utf8_lossy(&bytes);
            assert!(!wire.contains("private structured reasoning"));
            assert!(!wire.contains("private-structured-signature"));
            assert!(wire.contains("count"));
            if streaming {
                let events: Vec<serde_json::Value> = wire
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|data| serde_json::from_str(data).unwrap())
                    .collect();
                let usage = &events
                    .iter()
                    .find(|event| event["type"] == "message_start")
                    .unwrap()["message"]["usage"];
                assert_eq!(usage["input_tokens"], 13, "buffered input is already known");
                assert_eq!(usage["output_tokens"], 0);
            }
        }
    }
}

#[tokio::test]
async fn gateway_preserves_signature_only_blocks_with_and_without_structured_output() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for structured in [false, true] {
                let mut f = fixture(Mode::StructuredWithSignatureOnly(
                    r#"{"kind":"two","count":7}"#,
                ))
                .await;
                f.state.extract_thinking = true;
                let mut payload = structured_request(streaming);
                if !structured {
                    payload.as_object_mut().unwrap().remove("output_config");
                }
                payload["thinking"] = json!({"type":"enabled","budget_tokens":1024});
                let response = compatibility_request(f.state.clone(), cc, payload).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                if streaming {
                    let wire = String::from_utf8_lossy(&bytes);
                    let events: Vec<serde_json::Value> = wire
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .map(|data| serde_json::from_str(data).unwrap())
                        .collect();
                    let block = events
                        .iter()
                        .find(|event| {
                            event["type"] == "content_block_start"
                                && event["content_block"]["type"] == "thinking"
                        })
                        .expect("signed empty thinking block must survive");
                    assert_eq!(block["content_block"]["thinking"], "");
                    let signatures: Vec<_> = events
                        .iter()
                        .filter(|event| event["delta"]["type"] == "signature_delta")
                        .collect();
                    assert_eq!(signatures.len(), 1);
                    assert_eq!(
                        signatures[0]["delta"]["signature"],
                        "opaque-upstream-signature-only"
                    );
                } else {
                    let message: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let block = message["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|block| block["type"] == "thinking")
                        .expect("signed empty thinking block must survive");
                    assert_eq!(block["thinking"], "");
                    assert_eq!(block["signature"], "opaque-upstream-signature-only");
                }
            }
        }
    }
}

#[tokio::test]
async fn gateway_cached_usage_is_converted_once_by_chat_and_responses() {
    for endpoint in ["chat", "responses"] {
        let f = fixture(Mode::CachedUsage).await;
        let response = request(f.state.clone(), endpoint, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let wire = String::from_utf8_lossy(&body);
        let events: Vec<serde_json::Value> = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        if endpoint == "chat" {
            let usage = &events
                .iter()
                .rev()
                .find(|event| event["usage"].is_object())
                .unwrap()["usage"];
            assert_eq!(usage["prompt_tokens"], 18228);
            assert_eq!(usage["completion_tokens"], 17);
            assert_eq!(usage["total_tokens"], 18245);
        } else {
            let usage = &events
                .iter()
                .find(|event| event["type"] == "response.completed")
                .unwrap()["response"]["usage"];
            assert_eq!(usage["input_tokens"], 18228);
            assert_eq!(usage["output_tokens"], 17);
            assert_eq!(usage["total_tokens"], 18245);
        }
    }
}

#[tokio::test]
async fn gateway_invalid_structured_output_fails_and_records_usage() {
    for cc in [false, true] {
        let mut f = fixture(Mode::Structured("I am Kiro")).await;
        let store = Arc::new(TraceStore::open_in_memory().unwrap());
        f.state = f.state.clone().with_trace_store(Some(store.clone()));
        let response = compatibility_request(f.state.clone(), cc, structured_request(true)).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("upstream_output_schema_error"));
        let (records, _) = store.query_paged(&TraceQuery {
            limit: 5,
            ..Default::default()
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].final_status, "error");
        assert_eq!(records[0].output_tokens, 17);
    }
}

#[tokio::test]
async fn gateway_pdf_text_reaches_upstream_on_both_routes() {
    use base64::Engine;
    for cc in [false, true] {
        let f = fixture(Mode::Normal(Duration::ZERO)).await;
        let payload = json!({"model":"gpt-5.6-sol","max_tokens":64,
            "messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64",
                "media_type":"application/pdf","data":base64::engine::general_purpose::STANDARD.encode(super::documents::test_pdf("PDF72ABC"))}},
                {"type":"text","text":"Read the attached code"}]}]});
        let response = compatibility_request(f.state.clone(), cc, payload).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(f.bodies.lock()[0].to_string().contains("PDF72ABC"));
    }
}

#[tokio::test]
async fn gateway_unsupported_server_execution_and_invalid_schemas_never_reach_upstream() {
    for cc in [false, true] {
        let f = fixture(Mode::Normal(Duration::ZERO)).await;
        let mut execution = structured_request(false);
        execution["tools"] = json!([{"type":"code_execution_20250825","name":"code_execution"}]);
        assert_eq!(
            compatibility_request(f.state.clone(), cc, execution)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut schema = structured_request(false);
        schema["output_config"]["format"]["schema"] = json!({"$ref":"http://127.0.0.1/private"});
        assert_eq!(
            compatibility_request(f.state.clone(), cc, schema)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
    }
}

fn execution_request(streaming: bool) -> serde_json::Value {
    json!({"model":"claude-opus-5-5","max_tokens":128,"stream":streaming,
        "messages":[{"role":"user","content":"Use the server tool to compute five times seven"}],
        "tools":[{"type":"code_execution_20250825","name":"code_execution"}]})
}

fn enable_execution_bridge(state: &AppState) {
    state
        .protocol_compatibility
        .update_with_persistence(
            crate::model::config::ProtocolCompatibilityConfig {
                code_execution_mode: crate::model::config::CodeExecutionMode::Bridge,
                search_incremental_streaming: true,
                signature_metadata_compat: false,
                structured_identity_compat: false,
                plain_identity_compat: false,
                natural_style: false,
                search_context_compat: false,
                enabled_key_ids: vec![],
            },
            |_| Ok(()),
        )
        .unwrap();
}

async fn rate_limited_execution_fixture(failures: usize, retry_after: &'static str) -> Fixture {
    let mut config = Config::default();
    config.account_throttle_failover = false;
    config.model_api_429_retry_enabled = false;
    let fixture = fixture_with_config(
        Mode::ExecutionRateLimit {
            failures,
            retry_after,
        },
        config,
    )
    .await;
    enable_execution_bridge(&fixture.state);
    fixture
}

#[tokio::test]
async fn gateway_execution_continuation_rate_limit_recovers_without_repeating_tool_calls() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = rate_limited_execution_fixture(1, "1").await;
            let response =
                compatibility_request(f.state.clone(), cc, execution_request(streaming)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let wire = String::from_utf8(bytes.to_vec()).unwrap();
            let blocks: Vec<serde_json::Value> = if streaming {
                let events: Vec<serde_json::Value> = wire
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|data| serde_json::from_str(data).unwrap())
                    .collect();
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["type"] == "message_start")
                        .count(),
                    1
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["type"] == "message_stop")
                        .count(),
                    1
                );
                assert!(events.iter().all(|event| event["type"] != "error"));
                events
                    .iter()
                    .filter(|event| event["type"] == "content_block_start")
                    .enumerate()
                    .map(|(index, event)| {
                        assert_eq!(event["index"], index);
                        event["content_block"].clone()
                    })
                    .collect()
            } else {
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["content"]
                    .as_array()
                    .unwrap()
                    .clone()
            };
            assert_eq!(
                blocks
                    .iter()
                    .filter(|block| block["type"] == "server_tool_use")
                    .count(),
                1
            );
            assert_eq!(
                blocks
                    .iter()
                    .filter(|block| block["type"] == "bash_code_execution_tool_result")
                    .count(),
                1
            );
            assert_eq!(f.calls.load(Ordering::Relaxed), 3);
            let bodies = f.bodies.lock();
            assert_eq!(
                bodies
                    .iter()
                    .filter(|body| !body.to_string().contains("No command was run."))
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn gateway_execution_stream_preserves_completed_tools_when_retry_after_exceeds_deadline() {
    let f = rate_limited_execution_fixture(10, "120").await;
    let response = compatibility_request(f.state.clone(), false, execution_request(true)).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let events: Vec<serde_json::Value> = String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let result = events
        .iter()
        .position(|event| event["content_block"]["type"] == "bash_code_execution_tool_result")
        .unwrap();
    let failure = events
        .iter()
        .position(|event| event["type"] == "error")
        .unwrap();
    assert!(result < failure);
    assert_eq!(events[failure]["error"]["type"], "rate_limit_error");
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "message_stop" && event["type"] != "message_delta")
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn gateway_execution_disconnect_during_backoff_stops_retries() {
    let f = rate_limited_execution_fixture(10, "1").await;
    let response = compatibility_request(f.state.clone(), false, execution_request(true)).await;
    let mut body = response.into_body().into_data_stream();
    let mut saw_result = false;
    while let Some(chunk) = body.next().await {
        if String::from_utf8_lossy(&chunk.unwrap()).contains("bash_code_execution_tool_result") {
            saw_result = true;
            break;
        }
    }
    assert!(saw_result);
    tokio::time::timeout(Duration::from_secs(1), async {
        while f.calls.load(Ordering::Relaxed) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(body);
    tokio::time::sleep(Duration::from_millis(1250)).await;
    assert_eq!(f.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn gateway_execution_bridge_returns_unavailable_and_continues_without_fabricating_success() {
    for cc in [false,true] {
        for streaming in [false,true] {
            let mut f=fixture(Mode::Execution { client_tool:false }).await;
            let manager = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
            manager.sync_system_key("system".into(), None, "execution-billing-fixture".into());
            f.state = f.state.clone().with_usage(Some(manager.clone()), None, None);
            enable_execution_bridge(&f.state);
            let response=compatibility_request(f.state.clone(),cc,execution_request(streaming)).await;
            assert_eq!(response.status(),StatusCode::OK);
            let bytes=to_bytes(response.into_body(),1024*1024).await.unwrap();
            let wire=String::from_utf8_lossy(&bytes);
            assert!(wire.contains("bash_code_execution_tool_result_error"),"{wire}");
            assert!(wire.contains("unavailable"),"{wire}");
            assert!(wire.contains("no command was run"),"{wire}");
            assert!(!wire.contains("return_code"),"{wire}");
            assert!(!wire.contains("__kiro_server"),"{wire}");
            assert_eq!(f.calls.load(Ordering::Relaxed),2);
            let message=if streaming {
                let events:Vec<serde_json::Value>=wire.lines().filter_map(|line|line.strip_prefix("data: ")).map(|line|serde_json::from_str(line).unwrap()).collect();
                assert_eq!(events.iter().filter(|e|e["type"]=="message_start").count(),1);
                assert_eq!(events.iter().filter(|e|e["type"]=="message_stop").count(),1);
                events.into_iter().find(|e|e["type"]=="message_delta").unwrap()
            } else { serde_json::from_slice(&bytes).unwrap() };
            assert_eq!(message["usage"]["input_tokens"],26);
            assert_eq!(message["usage"]["output_tokens"],34);
            let billed = manager.list().remove(0);
            assert_eq!(billed.total_successful_calls, 1);
            assert_eq!(billed.total_input_tokens, 26);
            assert_eq!(billed.total_output_tokens, 34);
        }
    }
}

#[tokio::test]
async fn gateway_execution_declaration_does_not_invent_calls_and_scope_is_enforced() {
    let f=fixture(Mode::Normal(Duration::ZERO)).await;
    enable_execution_bridge(&f.state);
    let response=compatibility_request(f.state.clone(),false,execution_request(false)).await;
    assert_eq!(response.status(),StatusCode::OK);
    let bytes=to_bytes(response.into_body(),1024*1024).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("server_tool_use"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    f.state
        .protocol_compatibility
        .update_with_persistence(
            crate::model::config::ProtocolCompatibilityConfig {
                code_execution_mode: crate::model::config::CodeExecutionMode::Bridge,
                search_incremental_streaming: false,
                enabled_key_ids: vec![u64::MAX],
                signature_metadata_compat: false,
                structured_identity_compat: false,
                plain_identity_compat: false,
                natural_style: false,
                search_context_compat: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        compatibility_request(f.state.clone(), false, execution_request(false))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn gateway_execution_bridge_preserves_client_tool_turns() {
    let f=fixture(Mode::Execution { client_tool:true }).await;
    enable_execution_bridge(&f.state);
    let mut request=execution_request(false);
    request["tools"].as_array_mut().unwrap().push(json!({"name":"run","description":"A client action","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}));
    let response=compatibility_request(f.state.clone(),false,request).await;
    assert_eq!(response.status(),StatusCode::OK);
    let message:serde_json::Value=serde_json::from_slice(&to_bytes(response.into_body(),1024*1024).await.unwrap()).unwrap();
    assert_eq!(message["stop_reason"],"tool_use");
    assert!(message["content"].as_array().unwrap().iter().any(|b|b["type"]=="tool_use" && b["name"]=="run"));
    assert!(message["content"].as_array().unwrap().iter().any(|b|b["type"]=="bash_code_execution_tool_result"));
    assert_eq!(f.calls.load(Ordering::Relaxed),1);
}

#[tokio::test]
async fn gateway_execution_bridge_revalidates_the_aggregated_structured_answer() {
    let mut f=fixture(Mode::ExecutionSchema).await;
    let manager = Arc::new(crate::admin::client_keys::ClientKeyManager::new());
    manager.sync_system_key("system".into(), None, "execution-billing-fixture".into());
    f.state = f.state.clone().with_usage(Some(manager.clone()), None, None);
    enable_execution_bridge(&f.state);
    let mut request=execution_request(false);
    request["output_config"]=json!({"format":{"type":"json_schema","schema":{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}},"additionalProperties":false}}});
    let response=compatibility_request(f.state.clone(),false,request).await;
    assert_eq!(response.status(),StatusCode::BAD_GATEWAY);
    let bytes=to_bytes(response.into_body(),1024*1024).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("upstream_output_schema_error"));
    assert_eq!(f.calls.load(Ordering::Relaxed),2);
    let billed = manager.list().remove(0);
    assert_eq!(billed.total_successful_calls, 0);
    assert_eq!(billed.total_output_tokens, 34);
}

#[tokio::test]
async fn gateway_search_zero_budget_returns_tool_error_without_calling_search_backend() {
    for streaming in [false,true] {
        let f=fixture(Mode::Search).await;
        let request=json!({"model":"claude-opus-5-5","max_tokens":128,"stream":streaming,
            "messages":[{"role":"user","content":"Search for a rust release"}],
            "tools":[{"type":"web_search_20250305","name":"web_search","max_uses":0}]});
        let response=compatibility_request(f.state.clone(),false,request).await;
        assert_eq!(response.status(),StatusCode::OK);
        let bytes=to_bytes(response.into_body(),1024*1024).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("max_uses_exceeded"));
        assert!(f.bodies.lock().iter().all(|body|body["jsonrpc"]!="2.0"));
    }
}

#[tokio::test]
async fn gateway_execution_and_search_preserve_context_and_search_budget_across_rounds() {
    for wrapped in [false, true] {
        let mut config = Config::default();
        config.admin_api_key = Some("fixture-search-secret".into());
        let f = fixture_with_config(Mode::ExecutionSearch, config).await;
        enable_execution_bridge(&f.state);
        if wrapped {
            f.state
                .protocol_compatibility
                .update_with_persistence(
                    crate::model::config::ProtocolCompatibilityConfig {
                        code_execution_mode: crate::model::config::CodeExecutionMode::Bridge,
                        search_context_compat: true,
                        enabled_key_ids: vec![0],
                        ..Default::default()
                    },
                    |_| Ok(()),
                )
                .unwrap();
        }
        let mut request = execution_request(false);
        request["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"web_search_20250305","name":"web_search","max_uses":1}));
        let response = compatibility_request(f.state.clone(), false, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let message: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        let wire = message.to_string();
        assert!(wire.contains("search evidence was preserved"), "{wire}");
        assert!(
            wire.contains("bash_code_execution_tool_result_error"),
            "{wire}"
        );
        assert_eq!(
            message["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
        assert_eq!(
            f.bodies
                .lock()
                .iter()
                .filter(|body| body["jsonrpc"] == "2.0")
                .count(),
            1
        );
        assert_eq!(f.calls.load(Ordering::Relaxed), 4);
    }
}

#[tokio::test]
async fn gateway_pure_search_executes_and_uses_results_for_an_answer() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::Search).await;
            let payload = json!({"model":"gpt-5.6-sol","max_tokens":256,"stream":streaming,
                "messages":[{"role":"user","content":"You must call the web_search tool before answering. Perform a web search for the query: rust release"}],
                "tools":[{"type":"web_search_20250305","name":"web_search"}]});
            let response = compatibility_request(f.state.clone(), cc, payload).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let wire = String::from_utf8_lossy(&body);
            assert!(
                wire.contains("Synthesized answer using search evidence."),
                "{wire}"
            );
            assert!(wire.contains("server_tool_use"), "{wire}");
            assert!(wire.contains("web_search_tool_result"), "{wire}");
            let usage = if streaming {
                wire.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
                    .find(|event| event["type"] == "message_delta")
                    .unwrap()["usage"]
                    .clone()
            } else {
                serde_json::from_str::<serde_json::Value>(&wire).unwrap()["usage"].clone()
            };
            assert_eq!(usage["server_tool_use"]["web_search_requests"], 1);
            assert_eq!(f.calls.load(Ordering::Relaxed), 3);
        }
    }
}

#[tokio::test]
async fn gateway_unused_search_tool_has_zero_requests_and_hides_unrequested_reasoning() {
    for cc in [false, true] {
        for streaming in [false, true] {
            let f = fixture(Mode::HiddenReasoning).await;
            let payload = json!({"model":"gpt-5.6-sol","stream":streaming,
                "messages":[{"role":"user","content":"Only return the date"}],
                "tools":[{"type":"web_search_20250305","name":"web_search"}]});
            let response = compatibility_request(f.state.clone(), cc, payload).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let wire = String::from_utf8_lossy(&bytes);
            assert!(wire.contains("2025-01"));
            assert!(!wire.contains("hidden reasoning"));
            assert!(!wire.contains("kiro_thinking"));
            let usage = if streaming {
                wire.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
                    .find(|event| event["type"] == "message_delta")
                    .unwrap()["usage"]
                    .clone()
            } else {
                serde_json::from_str::<serde_json::Value>(&wire).unwrap()["usage"].clone()
            };
            assert_eq!(usage["server_tool_use"]["web_search_requests"], 0);
            assert_eq!(f.calls.load(Ordering::Relaxed), 1);
        }
    }
}

#[tokio::test]
async fn effective_call_billing_counts_completed_requests_across_protocols() {
    use crate::admin::client_keys::{BillingMode, ClientKeyManager, KeyAuth};

    for endpoint in ["messages", "cc", "chat", "responses"] {
        for streaming in [false, true] {
            let fixture = fixture(Mode::Normal(Duration::ZERO)).await;
            let manager = Arc::new(ClientKeyManager::new());
            let entry = manager.create("billable".into(), None, None);
            manager.update_meta(entry.id, None, None, None, Some((BillingMode::Calls, Some(1))));
            let state = fixture.state.clone().with_usage(Some(manager.clone()), None, None);
            assert_eq!(manager.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));
            let key_ctx = KeyContext { key_id: entry.id, key_source: TraceKeySource::ClientKey, ..key() };
            let response = request_with_key(state, endpoint, streaming, key_ctx).await;
            assert_eq!(response.status(), StatusCode::OK, "{endpoint} streaming={streaming}");
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let snapshot = manager.list().remove(0);
            assert_eq!(snapshot.total_successful_calls, 1, "{endpoint} streaming={streaming}");
            assert_eq!(snapshot.total_input_tokens, 13);
            assert_eq!(snapshot.total_output_tokens, 17);
            assert_eq!(manager.verify_and_touch_ex(&entry.key), KeyAuth::CallsOverLimit {
                id: entry.id, used: 1, limit: 1,
            });
        }
    }
}

#[tokio::test]
async fn effective_call_billing_does_not_charge_failed_or_interrupted_requests() {
    use crate::admin::client_keys::ClientKeyManager;

    for endpoint in ["messages", "cc", "chat", "responses"] {
        for mode in [Mode::Overloaded, Mode::Interrupted, Mode::Empty, Mode::InvalidFrame] {
            for streaming in [false, true] {
                let fixture = fixture(mode).await;
                let manager = Arc::new(ClientKeyManager::new());
                let entry = manager.create("billable".into(), None, None);
                let state = fixture.state.clone().with_usage(Some(manager.clone()), None, None);
                let key_ctx = KeyContext { key_id: entry.id, key_source: TraceKeySource::ClientKey, ..key() };
                let response = request_with_key(state, endpoint, streaming, key_ctx).await;
                let status = response.status();
                let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                assert!(status.is_client_error() || status.is_server_error() || String::from_utf8_lossy(&body).contains("error"));
                assert_eq!(manager.list()[0].total_successful_calls, 0, "{endpoint} streaming={streaming}");
            }
        }
    }
}

#[tokio::test]
async fn effective_call_billing_does_not_charge_cancelled_streams() {
    use crate::admin::client_keys::ClientKeyManager;

    for endpoint in ["messages", "cc", "chat", "responses"] {
        let fixture = fixture(Mode::Stalled).await;
        let manager = Arc::new(ClientKeyManager::new());
        let entry = manager.create("billable".into(), None, None);
        let state = fixture.state.clone().with_usage(Some(manager.clone()), None, None);
        let key_ctx = KeyContext { key_id: entry.id, key_source: TraceKeySource::ClientKey, ..key() };
        let mut body = request_with_key(state, endpoint, true, key_ctx).await.into_body().into_data_stream();
        tokio::time::timeout(Duration::from_secs(2), first_content(&mut body)).await.unwrap();
        drop(body);
        assert_eq!(manager.list()[0].total_successful_calls, 0, "{endpoint}");
    }
}

#[tokio::test]
async fn effective_call_billing_counts_system_key() {
    use crate::admin::client_keys::ClientKeyManager;

    let fixture = fixture(Mode::Normal(Duration::ZERO)).await;
    let manager = Arc::new(ClientKeyManager::new());
    manager.sync_system_key("system".into(), None, "fixture-system-key".into());
    let state = fixture.state.clone().with_usage(Some(manager.clone()), None, None);
    let response = request(state, "messages", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(manager.list()[0].total_successful_calls, 1);
}

#[tokio::test]
async fn effective_call_billing_http_quota_and_non_generation_routes() {
    use crate::admin::client_keys::{BillingMode, ClientKeyManager};
    use super::middleware::auth_middleware;

    let fixture = fixture(Mode::Normal(Duration::ZERO)).await;
    let manager = Arc::new(ClientKeyManager::new());
    let entry = manager.create("billable".into(), None, None);
    manager.update_meta(entry.id, None, None, None, Some((BillingMode::Calls, Some(1))));
    let state = fixture.state.clone().with_usage(Some(manager.clone()), None, None);
    let app = Router::new()
        .route("/messages", post(post_messages))
        .route("/count_tokens", post(super::handlers::count_tokens))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let payload = json!({"model": "gpt-5.6-sol", "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]});
    let response = client.post(format!("{base}/count_tokens")).header("x-api-key", &entry.key).json(&payload).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(manager.list()[0].total_successful_calls, 0);
    let response = client.post(format!("{base}/messages")).header("x-api-key", &entry.key).json(&json!({})).send().await.unwrap();
    assert!(response.status().is_client_error());
    assert_eq!(manager.list()[0].total_successful_calls, 0);
    let response = client.post(format!("{base}/messages")).header("x-api-key", &entry.key).json(&payload).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let response = client.post(format!("{base}/messages")).header("x-api-key", &entry.key).json(&payload).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "rate_limit_error");
    assert!(error["error"]["message"].as_str().unwrap().contains("有效调用次数"));
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    assert_eq!(manager.list()[0].total_successful_calls, 1);
    task.abort();
}

#[tokio::test]
async fn effective_call_billing_compaction_counts_final_success_not_internal_rounds() {
    use crate::admin::client_keys::ClientKeyManager;

    for streaming in [false, true] {
        for (mode, expected_calls) in [(Mode::CompactionRetry, 1), (Mode::Tools, 0)] {
            let fixture = fixture(mode).await;
            let manager = Arc::new(ClientKeyManager::new());
            let entry = manager.create("billable".into(), None, None);
            let state = fixture.state.clone().with_usage(Some(manager.clone()), None, None);
            let key_ctx = KeyContext { key_id: entry.id, key_source: TraceKeySource::ClientKey, ..key() };
            let payload = json!({"model": "gpt-5.6-sol", "stream": streaming, "input": [
                {"role": "user", "content": "Summarize our work"}, {"type": "compaction_trigger"}
            ]});
            let response = post_responses(axum::extract::State(state), Extension(key_ctx), HeaderMap::new(), Json(serde_json::from_value(payload).unwrap())).await;
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(body.contains(if expected_calls == 1 { "completed" } else { "compaction_error" }), "{body}");
            assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
            assert_eq!(manager.list()[0].total_successful_calls, expected_calls);
            assert_eq!(manager.list()[0].total_input_tokens, 26);
        }
    }
}

#[tokio::test]
async fn gateway_raw_tool_arguments_arrive_before_upstream_finishes() {
    let fixture = fixture(Mode::GatedToolArguments).await;
    let payload = json!({
        "model":"gpt-5.6-sol", "max_tokens":64, "stream":true,
        "messages":[{"role":"user", "content":"Write the requested content."}],
        "tools":[{
            "name":"Write", "description":"Write exactly the supplied content.",
            "input_schema":{
                "type":"object",
                "properties":{"file_path":{"type":"string"},"content":{"type":"string"}},
                "required":["file_path","content"]
            }
        }]
    });
    let response = post_messages(
        axum::extract::State(fixture.state.clone()),
        Extension(key()),
        Json(serde_json::from_value(payload).unwrap()),
    ).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut received = String::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(chunk) = body.next().await {
            received.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if received.contains("input_json_delta") {
                return;
            }
        }
        panic!("stream ended before the first tool argument fragment");
    }).await.expect("tool arguments were buffered while upstream waited for the client");
    let early_events = parse_sse(&received);
    let start = early_events.iter().find(|(_, event)| event["content_block"]["type"] == "tool_use").unwrap();
    assert_eq!(start.1["content_block"]["name"], "Write");
    assert_eq!(start.1["content_block"]["id"], "call_1");
    let tool_index = start.1["index"].clone();
    assert!(!early_events.iter().any(|(kind, event)| kind == "content_block_stop" && event["index"] == tool_index));
    assert!(!received.contains("message_stop"));
    fixture.release_tool_arguments.notify_one();
    while let Some(chunk) = body.next().await {
        received.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    let events = parse_sse(&received);
    let fragments: Vec<_> = events.iter()
        .filter(|(_, event)| event["delta"]["type"] == "input_json_delta")
        .map(|(_, event)| event["delta"]["partial_json"].as_str().unwrap())
        .collect();
    assert_eq!(fragments.len(), 2);
    let input: serde_json::Value = serde_json::from_str(&fragments.concat()).unwrap();
    assert_eq!(input, json!({"file_path":"/not-created","content":"first second"}));
    assert_eq!(events.iter().filter(|(kind, event)| kind == "content_block_stop" && event["index"] == tool_index).count(), 1);
    assert!(events.iter().any(|(kind, event)| kind == "message_delta" && event["delta"]["stop_reason"] == "tool_use"));
    assert_eq!(events.last().unwrap().0, "message_stop");
}

#[tokio::test]
async fn gateway_cc_emits_content_before_upstream_finishes() {
    let f = fixture(Mode::Normal(Duration::from_millis(400))).await;
    let mut body = request(f.state.clone(), "cc", true)
        .await
        .into_body()
        .into_data_stream();
    tokio::time::timeout(Duration::from_millis(250), first_content(&mut body))
        .await
        .expect("CC buffered content until EOF");
    let mut rest = String::new();
    while let Some(chunk) = body.next().await {
        rest.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(rest.contains("\"input_tokens\":13"));
    assert!(rest.contains("\"output_tokens\":17"));
}

#[tokio::test]
async fn gateway_chat_emits_content_before_upstream_finishes() {
    let f = fixture(Mode::Normal(Duration::from_millis(400))).await;
    let started = Instant::now();
    let response = request(f.state.clone(), "chat", true).await;
    let mut body = response.into_body().into_data_stream();
    first_content(&mut body).await;
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "Chat buffered content until EOF"
    );
}

#[tokio::test]
async fn gateway_cc_disconnect_is_an_error_not_message_stop() {
    let f = fixture(Mode::Interrupted).await;
    let response = request(f.state.clone(), "cc", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("event: error"),
        "disconnect was hidden: {text}"
    );
    assert!(!text.contains("event: message_stop"));
}

#[tokio::test]
async fn gateway_reuses_upstream_connection() {
    let f = fixture(Mode::Normal(Duration::ZERO)).await;
    for _ in 0..3 {
        let response = request(f.state.clone(), "messages", false).await;
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    }
    assert_eq!(f.close_headers.load(Ordering::Relaxed), 0);
    assert_eq!(f.connections.lock().len(), 1);
}

#[tokio::test]
async fn gateway_model_overload_does_not_retry_three_times() {
    let f = fixture(Mode::Overloaded).await;
    let response = request(f.state.clone(), "cc", true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key("retry-after"));
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("overloaded_error"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    // 不做模型级冷却：一次过载只影响本次请求，下一个请求照常发往上游。
    let second = request(f.state.clone(), "cc", true).await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        f.calls.load(Ordering::Relaxed),
        2,
        "model overload must not block later requests without trying upstream"
    );
}

#[tokio::test]
async fn gateway_non_stream_decodes_responses_larger_than_decoder_buffer() {
    let f = fixture(Mode::Large).await;
    let response = request(f.state.clone(), "messages", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        response["content"][0]["text"].as_str().unwrap().len(),
        18000 * 1024
    );
}

#[tokio::test]
async fn gateway_damaged_frames_are_errors_not_success() {
    for mode in [Mode::Truncated, Mode::InvalidFrame, Mode::Empty] {
        let f = fixture(mode).await;
        for endpoint in ["cc", "messages"] {
            let response = request(f.state.clone(), endpoint, true).await;
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("event: error"),
                "corrupt stream was accepted: {text}"
            );
            assert!(!text.contains("event: message_stop"));
        }
        let response = request(f.state.clone(), "messages", false).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
}

#[tokio::test]
async fn gateway_content_filter_is_reported_instead_of_output_limit_or_success() {
    // 只有 reasoning + CONTENT_FILTERED（带 tokenUsage、无 refusal 详情）：
    // 必须报告为拒答，不得伪造 max_tokens / 成功结束，也不泄露上游私有字段。
    let f = fixture(Mode::Filtered).await;
    for (endpoint, streaming, expected) in [
        ("messages", false, r#""stop_reason":"refusal""#),
        ("messages", true, r#""stop_reason":"refusal""#),
        ("cc", true, r#""stop_reason":"refusal""#),
        ("chat", true, r#""finish_reason":"content_filter""#),
        ("responses", true, r#""reason":"content_filter""#),
    ] {
        let response = request(f.state.clone(), endpoint, streaming).await;
        assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains(expected), "{endpoint}: {body}");
        assert!(!body.contains("max_tokens"), "{endpoint}: {body}");
        assert!(!body.contains("model_context_window_exceeded"), "{endpoint}: {body}");
        assert!(!body.contains("private provider detail"), "{endpoint}: {body}");
    }
}

#[tokio::test]
async fn gateway_chat_disconnect_has_no_success_finish_or_done() {
    let f = fixture(Mode::Interrupted).await;
    let response = request(f.state.clone(), "chat", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\"error\""));
    assert!(!text.contains("[DONE]"));
    assert!(!text.contains("\"finish_reason\":\"stop\""));
}

#[tokio::test]
async fn gateway_chat_preserves_overload_retry_after() {
    let f = fixture(Mode::Overloaded).await;
    let response = request(f.state.clone(), "chat", true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn gateway_responses_preserves_overload_retry_after() {
    let f = fixture(Mode::Overloaded).await;
    let response = request(f.state.clone(), "responses", true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn gateway_chat_tool_arguments_and_final_usage_are_complete() {
    let f = fixture(Mode::Tools).await;
    let response = request(f.state.clone(), "chat", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let chunks: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut arguments = String::new();
    for chunk in &chunks {
        if let Some(args) = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(|v| v.as_str())
        {
            arguments.push_str(args);
        }
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments).unwrap(),
        json!({"command":"hello"})
    );
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.pointer("/choices/0/finish_reason") == Some(&json!("tool_calls")))
    );
    let usage = &chunks.last().unwrap()["usage"];
    assert_eq!(usage["prompt_tokens"], 18);
    assert_eq!(usage["completion_tokens"], 17);
    assert!(text.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn gateway_content_filter_refusal_is_not_a_successful_end_turn() {
    let f = fixture(Mode::Refused).await;
    async fn text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    let response = request(f.state.clone(), "messages", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&text(response).await).unwrap();
    assert_eq!(body["stop_reason"], "refusal", "{body}");

    for endpoint in ["cc", "messages"] {
        let sse = text(request(f.state.clone(), endpoint, true).await).await;
        assert!(sse.contains(r#""stop_reason":"refusal""#), "{endpoint}: {sse}");
        assert!(!sse.contains("model_context_window_exceeded"), "{endpoint}: {sse}");
    }

    let chat = text(request(f.state.clone(), "chat", true).await).await;
    assert!(chat.contains(r#""finish_reason":"content_filter""#), "{chat}");
    assert!(!chat.contains(r#""finish_reason":"stop""#), "{chat}");

    let responses = text(request(f.state.clone(), "responses", true).await).await;
    assert!(responses.contains("event: response.incomplete"), "{responses}");
    assert!(responses.contains(r#""reason":"content_filter""#), "{responses}");
}

#[tokio::test]
async fn gateway_refusal_nonstream_compatibility_and_bare_metadata() {
    for mode in [Mode::Refused, Mode::RefusedBare] {
        let f = fixture(mode).await;
        for (endpoint, field, expected) in [
            ("messages", "/stop_reason", "refusal"),
            ("chat", "/choices/0/finish_reason", "content_filter"),
            ("responses", "/incomplete_details/reason", "content_filter"),
        ] {
            let response = request(f.state.clone(), endpoint, false).await;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(status, StatusCode::OK, "{endpoint}: {body}");
            assert_eq!(
                body.pointer(field).and_then(|v| v.as_str()),
                Some(expected),
                "{endpoint}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn gateway_refused_round_does_not_execute_search() {
    let mut failures = Vec::new();
    for streaming in [false, true] {
        let f = fixture(Mode::RefusedWithSearch).await;
        let payload = json!({
            "model":"gpt-5.6-sol","max_tokens":64,"stream":streaming,
            "messages":[{"role":"user","content":"hello"}],
            "tools":[
                {"type":"web_search_20250305","name":"web_search"},
                {"name":"run","description":"test command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}
            ]
        });
        let response = post_messages(
            axum::extract::State(f.state.clone()),
            Extension(key()),
            Json(serde_json::from_value(payload).unwrap()),
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        let calls = f.calls.load(Ordering::Relaxed);
        if status != StatusCode::OK
            || calls != 1
            || !body.contains("\"refusal\"")
            || body.contains(r#""type":"server_tool_use""#)
            || body.contains("web_search_tool_result")
        {
            failures.push(format!(
                "stream={streaming}, calls={calls}, status={status}, body={body}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn gateway_partial_tool_before_refusal_preserves_terminal_reason() {
    let mut failures = Vec::new();
    for mode in [
        Mode::RefusedWithPartialTool,
        Mode::RefusedWithInvalidTool,
        Mode::RefusedWithEmptyTool,
    ] {
        for streaming in [false, true] {
            let f = fixture(mode).await;
            let payload = json!({
                "model":"gpt-5.6-sol","max_tokens":64,"stream":streaming,
                "messages":[{"role":"user","content":"hello"}],
                "tools":[{"name":"run","description":"test command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}]
            });
            let response = post_messages(
                axum::extract::State(f.state.clone()),
                Extension(key()),
                Json(serde_json::from_value(payload).unwrap()),
            )
            .await;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let body = String::from_utf8_lossy(&bytes);
            if streaming && matches!(mode, Mode::RefusedWithPartialTool) {
                let events = parse_sse(&body);
                assert_eq!(status, StatusCode::OK);
                assert_eq!(events.last().unwrap().1["error"]["type"], "permission_error");
                assert!(body.contains("refusal"));
                assert!(!body.contains("message_stop"));
                assert!(!body.contains("upstream_tool_json_error"));
                continue;
            }
            if status != StatusCode::OK
                || !body.contains("\"refusal\"")
                || body.contains("upstream_tool_json_error")
                || body.contains("\"type\":\"tool_use\"")
            {
                failures.push(format!("stream={streaming}, status={status}, body={body}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn gateway_stalled_body_times_out_without_normal_completion() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::Stalled, config).await;
    let response = request(f.state.clone(), "cc", true).await;
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .expect("idle stream did not time out")
    .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("event: error"));
    assert!(!text.contains("event: message_stop"));
}

#[tokio::test]
async fn gateway_header_timeout_does_not_repeat_long_waits() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::SlowHeaders, config).await;
    let response =
        tokio::time::timeout(Duration::from_secs(2), request(f.state.clone(), "cc", true))
            .await
            .expect("header deadline was ignored or retried");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn gateway_error_body_timeout_does_not_repeat_long_waits() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::SlowErrorBody, config).await;
    let response =
        tokio::time::timeout(Duration::from_secs(2), request(f.state.clone(), "cc", true))
            .await
            .expect("error response body deadline was ignored or retried");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn gateway_first_token_trace_ignores_metadata() {
    let mut f = fixture(Mode::MetadataFirst).await;
    let store = Arc::new(TraceStore::open_in_memory().unwrap());
    f.state = f.state.clone().with_trace_store(Some(store.clone()));
    let response = request(f.state.clone(), "cc", true).await;
    to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let (records, count) = store.query_paged(&TraceQuery {
        limit: 10,
        ..Default::default()
    });
    assert_eq!(count, 1);
    assert!(
        records[0].first_token_ms.unwrap() >= 90,
        "metadata was counted as the first token: {:?}",
        records[0].first_token_ms
    );
}

#[tokio::test]
async fn gateway_local_rpm_limit_is_traced_apart_from_upstream_429() {
    // 2026-10-30 线上：单账号 RPM 打满后网关自己回的 429 记成 transient / "upstream rate limited"、
    // 凭据 0，看起来像上游限流（8 小时 1101 次，同期上游真正的 429 只有 38 次）。
    let config: Config =
        serde_json::from_value(json!({"accountRpmLimitEnabled": true, "accountRpmLimit": 1}))
            .unwrap();
    let mut f = fixture_with_config(Mode::Normal(Duration::ZERO), config).await;
    let store = Arc::new(TraceStore::open_in_memory().unwrap());
    f.state = f.state.clone().with_trace_store(Some(store.clone()));

    let first = request(f.state.clone(), "messages", false).await;
    assert_eq!(first.status(), StatusCode::OK);
    to_bytes(first.into_body(), 1024 * 1024).await.unwrap();

    let second = request(f.state.clone(), "messages", false).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(second.headers().get("retry-after").is_some());
    let body = to_bytes(second.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("Gateway per-account rate limit"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 1, "被本地限流的请求不应发到上游");

    let mut rejected = Vec::new();
    for _ in 0..50 {
        let (records, _) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        rejected = records
            .into_iter()
            .filter(|r| r.final_status != "success")
            .collect();
        if !rejected.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].error_type.as_deref(), Some("local_rate_limited"));
    assert!(
        rejected[0]
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("local per-account RPM limit"),
        "{:?}",
        rejected[0].error_message
    );
}

#[tokio::test]
async fn gateway_active_stream_can_exceed_idle_deadline() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::Active, config).await;
    let started = Instant::now();
    let response = request(f.state.clone(), "cc", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(started.elapsed() > Duration::from_secs(1));
    assert!(text.contains("event: message_stop"));
    assert!(!text.contains("event: error"));
}

#[tokio::test]
#[ignore = "controlled latency measurement; run with --ignored --nocapture"]
async fn gateway_latency_measurement() {
    for endpoint in ["cc", "chat", "messages"] {
        let f = fixture(Mode::Normal(Duration::from_millis(200))).await;
        let mut first = Vec::new();
        let mut total = Vec::new();
        for _ in 0..10 {
            let start = Instant::now();
            let mut body = request(f.state.clone(), endpoint, true)
                .await
                .into_body()
                .into_data_stream();
            first_content(&mut body).await;
            first.push(start.elapsed().as_secs_f64() * 1000.0);
            while let Some(chunk) = body.next().await {
                chunk.unwrap();
            }
            total.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        first.sort_by(f64::total_cmp);
        total.sort_by(f64::total_cmp);
        println!(
            "LATENCY endpoint={endpoint} first_p50_ms={:.2} first_p95_ms={:.2} total_p50_ms={:.2} connections={} requests={}",
            first[5],
            first[9],
            total[5],
            f.connections.lock().len(),
            f.calls.load(Ordering::Relaxed)
        );
    }
}

// ---------------------------------------------------------------------------
// output_config.format = json_schema（Claude Code goal check 形态）
// ---------------------------------------------------------------------------

const OPENING_CONDITION: &[&str] = &[
    "The condition is not met yet. ",
    "{\"ok\":false,",
    "\"reason\":\"file missing\"}",
];
const OPENING_CHECKING: &[&str] = &[
    "I'm checking the repository state.\n\n",
    "{\"ok\":true,\"reason\":\"all tests pass\",\"impossible\":false}",
];
const OPENING_CORE: &[&str] = &[
    "The core con",
    "dition holds.\n```json\n{\"ok\":true,",
    "\"reason\":\"done\"}\n```\nDone.",
];
const PURE_JSON: &[&str] = &["{\"ok\":true,", "\"reason\":\"fine\"}"];
const NO_JSON: &[&str] = &["I'm checking the files, ", "nothing to report {yet}."];
const NONCONFORMING_THEN_OK: &[&str] = &[
    "First attempt: {\"ok\":\"yes\",\"reason\":1} then ",
    "{\"ok\":false,\"reason\":\"second\"} and {\"ok\":true,\"reason\":\"third\"}",
];

fn goal_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "ok": {"type": "boolean"},
            "reason": {"type": "string"},
            "impossible": {"type": "boolean"}
        },
        "required": ["ok", "reason"],
        "additionalProperties": false
    })
}

fn goal_output_config() -> serde_json::Value {
    json!({"format": {"type": "json_schema", "schema": goal_schema()}})
}

async fn goal_request(
    state: AppState,
    endpoint: &str,
    streaming: bool,
    thinking: bool,
    output_config: Option<serde_json::Value>,
) -> Response {
    goal_request_for_model(state, "claude-haiku-5-5", endpoint, streaming, thinking, output_config)
        .await
}

async fn goal_request_for_model(
    state: AppState,
    model: &str,
    endpoint: &str,
    streaming: bool,
    thinking: bool,
    output_config: Option<serde_json::Value>,
) -> Response {
    let mut req = json!({
        "model": model,
        "max_tokens": 256,
        "stream": streaming,
        "system": "You judge whether a goal condition is met.",
        "messages": [{"role": "user", "content": "Is the goal done?"}],
    });
    if let Some(output_config) = output_config {
        req["output_config"] = output_config;
    }
    if thinking {
        req["thinking"] = json!({"type": "enabled", "budget_tokens": 2000});
    }
    let payload = Json(serde_json::from_value(req).unwrap());
    if endpoint == "cc" {
        post_messages_cc(axum::extract::State(state), Extension(key()), payload).await
    } else {
        post_messages(axum::extract::State(state), Extension(key()), payload).await
    }
}

async fn body_string(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse_sse(body: &str) -> Vec<(String, serde_json::Value)> {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let event = block
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .unwrap()
                .to_owned();
            let data = block
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            (event, serde_json::from_str(data).unwrap())
        })
        .filter(|(event, _)| event != "ping")
        .collect()
}

/// 非 ping 事件的 (event, index) 序列；用于断言块结构。
fn sse_shape(events: &[(String, serde_json::Value)]) -> Vec<String> {
    events
        .iter()
        .filter(|(event, _)| event != "ping")
        .map(|(event, data)| match data.get("index") {
            Some(index) => format!("{event}#{index}"),
            None => event.clone(),
        })
        .collect()
}

fn sse_text(events: &[(String, serde_json::Value)]) -> String {
    events
        .iter()
        .filter(|(event, data)| event == "content_block_delta" && data["delta"]["type"] == "text_delta")
        .map(|(_, data)| data["delta"]["text"].as_str().unwrap())
        .collect()
}

fn assert_goal_json(text: &str, expected: serde_json::Value) {
    assert!(text.starts_with('{') && text.ends_with('}'), "not a bare JSON value: {text}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(text).unwrap(), expected);
}

fn goal_cases() -> Vec<(&'static [&'static str], serde_json::Value)> {
    vec![
        (OPENING_CONDITION, json!({"ok": false, "reason": "file missing"})),
        (OPENING_CHECKING, json!({"ok": true, "reason": "all tests pass", "impossible": false})),
        (OPENING_CORE, json!({"ok": true, "reason": "done"})),
        (PURE_JSON, json!({"ok": true, "reason": "fine"})),
        // 多个合规值时取最后一个（结论通常在理由之后）
        (NONCONFORMING_THEN_OK, json!({"ok": true, "reason": "third"})),
    ]
}

#[tokio::test]
async fn structured_output_non_stream_extracts_json_from_prose() {
    for endpoint in ["messages", "cc"] {
        for (reply, expected) in goal_cases() {
            let f = fixture(Mode::Reply(reply)).await;
            let response = goal_request(f.state.clone(), endpoint, false, false, Some(goal_output_config())).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
            let content = body["content"].as_array().unwrap();
            assert_eq!(content.len(), 1, "{endpoint}: {body}");
            assert_eq!(content[0]["type"], "text");
            assert_goal_json(content[0]["text"].as_str().unwrap(), expected);
            assert_eq!(body["type"], "message");
            assert!(body["id"].as_str().unwrap().starts_with("msg_"));
            assert_eq!(body["model"], "claude-haiku-5-5");
            assert_eq!(body["stop_reason"], "end_turn");
            assert_eq!(body["usage"]["output_tokens"], 17);
        }
    }
}

#[tokio::test]
async fn gateway_stream_respects_extract_thinking_disabled() {
    // L7：fixture 的 AppState 是 extract_thinking=false。流式与非流式必须一致：不拆出 thinking 块。
    let f = fixture(Mode::ThinkingText).await;
    let req = json!({"model": "gpt-5.6-sol", "max_tokens": 2048, "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [{"role": "user", "content": "hello"}]});
    for endpoint in ["messages", "cc"] {
        let response = if endpoint == "cc" {
            post_messages_cc(axum::extract::State(f.state.clone()), Extension(key()), Json(serde_json::from_value(req.clone()).unwrap())).await
        } else {
            post_messages(axum::extract::State(f.state.clone()), Extension(key()), Json(serde_json::from_value(req.clone()).unwrap())).await
        };
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let sse = String::from_utf8_lossy(&bytes);
        assert!(!sse.contains(r#""type":"thinking""#), "{endpoint}: {sse}");
        assert!(sse.contains("先想想</thinking>"), "{endpoint}: {sse}");
    }
}

#[tokio::test]
async fn structured_output_stream_emits_one_compliant_json_text_block() {
    for endpoint in ["messages", "cc"] {
        for (reply, expected) in goal_cases() {
            let f = fixture(Mode::Reply(reply)).await;
            let response = goal_request(f.state.clone(), endpoint, true, false, Some(goal_output_config())).await;
            assert_eq!(response.status(), StatusCode::OK);
            let events = parse_sse(&body_string(response).await);
            assert_eq!(
                sse_shape(&events),
                [
                    "message_start",
                    "content_block_start#0",
                    "content_block_delta#0",
                    "content_block_stop#0",
                    "message_delta",
                    "message_stop"
                ],
                "{endpoint}: {events:?}"
            );
            assert_eq!(events[0].1["message"]["model"], "claude-haiku-5-5");
            assert!(events[0].1["message"]["id"].as_str().unwrap().starts_with("msg_"));
            assert_eq!(events[1].1["content_block"]["type"], "text");
            assert_goal_json(&sse_text(&events), expected);
            let delta = &events.iter().find(|(event, _)| event == "message_delta").unwrap().1;
            assert_eq!(delta["delta"]["stop_reason"], "end_turn");
            assert_eq!(delta["usage"]["output_tokens"], 17);
        }
    }
}

#[tokio::test]
async fn structured_output_without_conforming_json_returns_strict_error() {
    for reply in [NO_JSON, &["{\"ok\":\"yes\"} and {\"reason\":2}"]] {
        for endpoint in ["messages", "cc"] {
            for streaming in [false, true] {
                let f = fixture(Mode::Reply(reply)).await;
                let response = goal_request(f.state.clone(), endpoint, streaming, false, Some(goal_output_config())).await;
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
                assert_eq!(body["error"]["type"], "upstream_output_schema_error");
            }
        }
    }
}

#[tokio::test]
async fn structured_output_keeps_thinking_blocks_before_json_text() {
    let f = fixture(Mode::ReplyWithReasoning(OPENING_CONDITION)).await;
    // 非流式默认不返回 thinking 块；开启 extract_thinking 后才有。
    let mut state = f.state.clone();
    state.extract_thinking = true;
    let response = goal_request(state, "messages", false, true, Some(goal_output_config())).await;
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let content = body["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "{body}");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "Let me think about the goal.");
    assert_eq!(content[1]["type"], "text");
    assert_goal_json(content[1]["text"].as_str().unwrap(), json!({"ok": false, "reason": "file missing"}));

    let response = goal_request(f.state.clone(), "messages", true, true, Some(goal_output_config())).await;
    let events = parse_sse(&body_string(response).await);
    assert_eq!(
        sse_shape(&events),
        [
            "message_start",
            "content_block_start#0",
            "content_block_delta#0",
            "content_block_delta#0",
            "content_block_stop#0",
            "content_block_start#1",
            "content_block_delta#1",
            "content_block_stop#1",
            "message_delta",
            "message_stop"
        ],
        "{events:?}"
    );
    assert_eq!(events[1].1["content_block"]["type"], "thinking");
    assert_eq!(events[5].1["content_block"]["type"], "text");
    assert_goal_json(&sse_text(&events), json!({"ok": false, "reason": "file missing"}));
}

#[tokio::test]
async fn structured_output_cc_preserves_thinking_with_format_only() {
    let fixture = fixture(Mode::ReplyWithReasoning(OPENING_CONDITION)).await;
    let response = goal_request(fixture.state.clone(), "cc", false, true, Some(goal_output_config())).await;
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["content"][0]["type"], "thinking");
    let response = goal_request(fixture.state.clone(), "cc", true, true, Some(goal_output_config())).await;
    let events = parse_sse(&body_string(response).await);
    assert!(events.iter().any(|(_, data)| data["content_block"]["type"] == "thinking"));
}

/// 去掉注入的指令、忽略空行与随机会话 id 后，便于比较“除注入外完全一致”。
fn strip_instruction(value: &mut serde_json::Value, instruction: &str) {
    match value {
        serde_json::Value::String(text) => {
            let stripped = text.replace(instruction, "");
            *text = stripped.split('\n').filter(|line| !line.is_empty()).collect::<Vec<_>>().join("\n");
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| strip_instruction(v, instruction)),
        serde_json::Value::Object(map) => {
            // 每次请求都会随机生成的会话 id。
            for key in ["conversationId", "agentContinuationId"] {
                if let Some(id) = map.get_mut(key) { *id = json!("<id>"); }
            }
            map.values_mut().for_each(|v| strip_instruction(v, instruction));
        }
        _ => {}
    }
}

#[tokio::test]
async fn structured_output_injects_system_prompt_only_when_format_is_set_and_never_forwards_it() {
    let marker = super::structured_output::system_instruction(&goal_schema());
    let marker_line = marker.lines().next().unwrap().to_owned();
    let cases = [
        ("none", None),
        ("effort_only", Some(json!({"effort": "low"}))),
        ("format_only", Some(goal_output_config())),
        ("empty_object", Some(json!({}))),
        ("effort_and_format", Some(json!({"effort": "low", "format": goal_output_config()["format"].clone()}))),
    ];
    let mut sent = HashMap::new();
    for (name, output_config) in cases {
        let f = fixture(Mode::Reply(PURE_JSON)).await;
        let response = goal_request(f.state.clone(), "messages", false, false, output_config).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let _ = body_string(response).await;
        let bodies = f.bodies.lock();
        assert_eq!(bodies.len(), 1, "{name}");
        sent.insert(name, bodies[0].clone());
    }

    for (name, body) in &sent {
        // format / schema 不能出现在 Kiro 的额外字段里。
        let extra = body.get("additionalModelRequestFields").map(|v| v.to_string()).unwrap_or_default();
        assert!(!extra.contains("format") && !extra.contains("schema") && !extra.contains("json_schema"), "{name}: {extra}");
    }
    assert!(!sent["none"].to_string().contains(&marker_line));
    assert!(!sent["effort_only"].to_string().contains(&marker_line));
    assert!(sent["format_only"].to_string().contains(&marker_line));
    assert!(sent["effort_and_format"].to_string().contains(&marker_line));

    // 只带 format 与空 output_config 均不隐式启用 effort，唯一差异是 schema 指令。
    let mut with_format = sent["format_only"].clone();
    strip_instruction(&mut with_format, &marker);
    let mut without = sent["empty_object"].clone();
    strip_instruction(&mut without, &marker);
    assert_eq!(with_format, without, "format-only must not change anything but the system prompt");
    let mut effort_and_format = sent["effort_and_format"].clone();
    strip_instruction(&mut effort_and_format, &marker);
    let mut effort_only = sent["effort_only"].clone();
    strip_instruction(&mut effort_only, &marker);
    assert_eq!(effort_and_format, effort_only, "effort must keep working next to format");
}

/// #128 x 结构化输出：claude-haiku-5.5 原生 reasoning 下，
/// - 只带 format：与 `{}` 一致，不隐式指定 effort；format 绝不下发；
/// - format + effort：显式 effort（含 xhigh / max）原样透传；
/// - 不带 format / output_config：不下发 additionalModelRequestFields，回复不被改写。
#[tokio::test]
async fn structured_output_on_haiku_5_5_keeps_native_reasoning_effort() {
    let marker = super::structured_output::system_instruction(&goal_schema());
    let schema_format = goal_output_config()["format"].clone();
    let mut sent = HashMap::new();
    for (name, output_config) in [
        ("none", None),
        ("empty_object", Some(json!({}))),
        ("format_only", Some(goal_output_config())),
        ("effort_only_max", Some(json!({"effort": "max"}))),
        ("format_low", Some(json!({"effort": "low", "format": schema_format.clone()}))),
        ("format_medium", Some(json!({"effort": "medium", "format": schema_format.clone()}))),
        ("format_xhigh", Some(json!({"effort": "xhigh", "format": schema_format.clone()}))),
        ("format_max", Some(json!({"effort": "max", "format": schema_format.clone()}))),
    ] {
        let f = fixture(Mode::Reply(PURE_JSON)).await;
        let response = goal_request_for_model(
            f.state.clone(),
            "claude-haiku-5.5",
            "messages",
            false,
            false,
            output_config,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let _ = body_string(response).await;
        let bodies = f.bodies.lock();
        assert_eq!(bodies.len(), 1, "{name}");
        sent.insert(name, bodies[0].clone());
    }
    let extra = |name: &str| {
        sent[name]
            .get("additionalModelRequestFields")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    assert_eq!(extra("none"), serde_json::Value::Null);
    assert_eq!(extra("empty_object"), serde_json::Value::Null);
    assert_eq!(extra("format_only"), serde_json::Value::Null);
    for (name, effort) in [
        ("effort_only_max", "max"),
        ("format_low", "low"),
        ("format_medium", "medium"),
        ("format_xhigh", "xhigh"),
        ("format_max", "max"),
    ] {
        assert_eq!(extra(name), json!({"output_config": {"effort": effort}}), "{name}");
    }
    // format / json_schema 绝不出现在上游请求的任何字段里（注入的 system 指令除外）。
    for (name, body) in &sent {
        let mut stripped = body.clone();
        strip_instruction(&mut stripped, &marker);
        let text = stripped.to_string().to_lowercase();
        assert!(!text.contains("json_schema") && !text.contains("\"format\""), "{name}: {text}");
    }
    // 只带 format 与空 output_config 除注入指令外完全一致。
    let mut a = sent["format_only"].clone();
    strip_instruction(&mut a, &marker);
    let mut b = sent["empty_object"].clone();
    strip_instruction(&mut b, &marker);
    assert_eq!(a, b);
    // 注入仅发生在带 json_schema format 的请求上。
    let marker_line = marker.lines().next().unwrap();
    for name in ["none", "empty_object", "effort_only_max"] {
        assert!(!sent[name].to_string().contains(marker_line), "{name}");
    }
    for name in ["format_only", "format_low", "format_medium", "format_xhigh", "format_max"] {
        assert!(sent[name].to_string().contains(marker_line), "{name}");
    }
}

/// 合并保留当前分支的约定：format 不隐式启用 effort，显式 effort 和 thinking 仍然有效。
#[tokio::test]
async fn structured_output_format_only_keeps_unspecified_effort() {
    // 使用支持原生 reasoning 的模型验证缺省与显式 effort。
    for thinking in [false, true] {
        let mut sent = HashMap::new();
        for (name, output_config) in [
            ("format_only", Some(goal_output_config())),
            ("no_output_config", None),
            ("empty_object", Some(json!({}))),
            ("explicit_high", Some(json!({"effort": "high"}))),
            ("explicit_low", Some(json!({"effort": "low"}))),
        ] {
            let f = fixture(Mode::Reply(PURE_JSON)).await;
            let response = goal_request_for_model(
                f.state.clone(),
                "claude-sonnet-4.6",
                "messages",
                false,
                thinking,
                output_config,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{name}/{thinking}");
            let _ = body_string(response).await;
            let bodies = f.bodies.lock();
            assert_eq!(bodies.len(), 1, "{name}/{thinking}");
            sent.insert(name, bodies[0].clone());
        }

        let extra = |name: &str| {
            sent[name]
                .get("additionalModelRequestFields")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
        // 缺省 effort 使用已有 thinking 配置推导；显式 effort 仍然生效。
        assert_eq!(extra("format_only"), extra("no_output_config"), "thinking={thinking}");
        assert_eq!(extra("format_only"), extra("empty_object"), "thinking={thinking}");
        assert_eq!(extra("explicit_high"), json!({"output_config": {"effort": "high"}}), "thinking={thinking}");
        // 不带 output_config 时不带 reasoning 字段（master 同；thinking 启用时由 budget 推导）。
        if !thinking {
            assert_eq!(extra("no_output_config"), serde_json::Value::Null);
        }
        // 显式 effort 仍然照旧优先。
        assert_eq!(extra("explicit_low"), json!({"output_config": {"effort": "low"}}), "thinking={thinking}");

        // format 绝不下发；注入的 system 指令是唯一差别。
        let marker = super::structured_output::system_instruction(&goal_schema());
        let mut stripped = sent["format_only"].clone();
        strip_instruction(&mut stripped, &marker);
        let format_only = stripped.to_string();
        assert!(!format_only.to_lowercase().contains("json_schema"), "{format_only}");
        let mut with_format = sent["format_only"].clone();
        strip_instruction(&mut with_format, &marker);
        let mut without = sent["empty_object"].clone();
        strip_instruction(&mut without, &marker);
        assert_eq!(with_format, without, "thinking={thinking}");
    }
}

/// 非流式 + state.extract_thinking=false + thinking enabled + 只有 format 的 output_config：
/// 已显式请求 thinking 时仍返回独立推理块；format 仅改写最终 JSON 文本。
#[tokio::test]
async fn structured_output_format_only_preserves_explicit_thinking() {
    let f = fixture(Mode::ReplyWithReasoning(OPENING_CONDITION)).await;
    let mut state = f.state.clone();
    state.extract_thinking = false;

    let mut thinking = HashMap::new();
    for (name, output_config, bare_json) in [
        ("format_only", Some(goal_output_config()), true),
        ("explicit_high", Some(json!({"effort": "high"})), false),
    ] {
        let response = goal_request(state.clone(), "messages", false, true, output_config).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        let content = body["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "{name}: {body}");
        assert_eq!(content[0]["type"], "thinking", "{name}: {body}");
        assert_eq!(content[0]["thinking"], "Let me think about the goal.", "{name}");
        assert_eq!(content[1]["type"], "text", "{name}: {body}");
        if bare_json {
            // format 存在：text 块被改写为纯 JSON。
            assert_goal_json(
                content[1]["text"].as_str().unwrap(),
                json!({"ok": false, "reason": "file missing"}),
            );
        } else {
            assert_eq!(
                content[1]["text"],
                OPENING_CONDITION.concat(),
                "没有 format 时不改写回复"
            );
        }
        thinking.insert(name, content[0].clone());
    }
    // 两种 output_config 的 thinking 块完全相同：format 不影响 reasoning 行为。
    assert_eq!(thinking["format_only"], thinking["explicit_high"]);

    // 对照组：extract_thinking=false 且无 output_config 时不拆 thinking 块（master 同）。
    let f = fixture(Mode::ReplyWithReasoning(OPENING_CONDITION)).await;
    let mut state = f.state.clone();
    state.extract_thinking = false;
    let response = goal_request(state, "messages", false, true, None).await;
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["content"], json!([{"type": "text", "text": OPENING_CONDITION.concat()}]));
}

#[tokio::test]
async fn structured_output_is_inert_without_format() {
    // 没有 output_config / 只有 effort：响应和流都不被改写，
    // 即使回复里恰好含有符合某个 schema 的 JSON。
    let original: String = OPENING_CHECKING.concat();
    for output_config in [
        None,
        Some(json!({"effort": "low"})),
    ] {
        let f = fixture(Mode::Reply(OPENING_CHECKING)).await;
        let response = goal_request(f.state.clone(), "messages", false, false, output_config.clone()).await;
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["content"], json!([{"type": "text", "text": original}]), "{output_config:?}");

        let response = goal_request(f.state.clone(), "messages", true, false, output_config.clone()).await;
        let events = parse_sse(&body_string(response).await);
        assert_eq!(sse_text(&events), original, "{output_config:?}");
        // 实时转发：文本按上游片段逐段下发，而不是合并成一个 delta。
        let deltas = events
            .iter()
            .filter(|(event, data)| event == "content_block_delta" && data["delta"]["type"] == "text_delta")
            .count();
        assert!(deltas >= 2, "{output_config:?}: text was not streamed live: {events:?}");
    }
}

#[tokio::test]
async fn structured_output_stream_failure_does_not_publish_unvalidated_content() {
    let f = fixture(Mode::Interrupted).await;
    let response = goal_request(f.state.clone(), "messages", true, false, Some(goal_output_config())).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["error"]["type"], "api_error");
    assert!(body.get("content").is_none());
}

/// 结构化流式会缓冲到流末才发出内容：trace 首字时间应记在真正发出时，
/// 而不是上游内容到达时；不带 format 的请求照旧按到达时间记。
#[tokio::test]
async fn structured_output_stream_first_token_trace_is_emit_time() {
    let mut first_token = HashMap::new();
    for (name, output_config) in [("structured", Some(goal_output_config())), ("plain", None)] {
        let mode = if name == "structured" { Mode::DelayedJson(Duration::from_millis(300)) } else { Mode::Normal(Duration::from_millis(300)) };
        let mut f = fixture(mode).await;
        let store = Arc::new(TraceStore::open_in_memory().unwrap());
        f.state = f.state.clone().with_trace_store(Some(store.clone()));
        let response = goal_request(f.state.clone(), "messages", true, false, output_config).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let _ = body_string(response).await;
        let (records, count) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(count, 1, "{name}");
        first_token.insert(name, records[0].first_token_ms.unwrap());
    }
    assert!(first_token["structured"] >= 300, "{first_token:?}");
    assert!(first_token["plain"] < 300, "{first_token:?}");
}

#[tokio::test]
async fn gateway_upstream_exception_keeps_classified_reason() {
    // M3：流中途的 ThrottlingException 必须以 rate_limit_error 告诉客户端，且不带上游原文。
    let f = fixture(Mode::Throttled).await;
    let sse = {
        let response = request(f.state.clone(), "messages", true).await;
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert!(sse.contains("rate_limit_error"), "{sse}");
    assert!(!sse.contains("message_stop"), "{sse}");
    assert!(!sse.contains("123456789012"), "{sse}");
    let response = request(f.state.clone(), "messages", false).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn gateway_search_loop_rejects_truncated_tool_json() {
    // L10：web_search 循环里半截工具参数不能以 {} 交给客户端执行。
    let mut failures = Vec::new();
    for streaming in [false, true] {
        let f = fixture(Mode::SearchInvalidToolJson).await;
        let payload = json!({
            "model":"gpt-5.6-sol","max_tokens":64,"stream":streaming,
            "messages":[{"role":"user","content":"hello"}],
            "tools":[
                {"type":"web_search_20250305","name":"web_search"},
                {"name":"run","description":"test command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}
            ]
        });
        let response = post_messages(
            axum::extract::State(f.state.clone()),
            Extension(key()),
            Json(serde_json::from_value(payload).unwrap()),
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let failed_visibly = status != StatusCode::OK || body.contains("event: error");
        if !failed_visibly || body.contains(r#""stop_reason":"tool_use""#) {
            failures.push(format!("stream={streaming}, status={status}, body={body}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// 读流式响应里最后一份 usage（message_start 先给占位，message_delta 给最终值）。
async fn streamed_cache_usage(response: Response) -> (u64, u64) {
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let (mut read, mut create) = (0, 0);
    for line in text.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let usage = event.get("usage").or_else(|| event.pointer("/message/usage"));
        if let Some(u) = usage {
            if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) { read = v; }
            if let Some(v) = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) { create = v; }
        }
    }
    (read, create)
}

#[tokio::test]
async fn gateway_dsh_master_key_reports_cache_hits_without_session_header() {
    // 回归：DSH（pi-ai）用主 Key、不带 metadata.user_id，也发不出会话请求头。
    // 以前 isolation_seed 返回 None → 每个请求都是 miss_no_seed，DSH 永远显示缓存命中 0%。
    // 请求体是 pi-ai 1.4.0 真实生成的三轮（见 fixtures/dsh-pi-ai-1.4.0/README.md），不手造。
    let turns: Vec<serde_json::Value> = [
        include_str!("fixtures/dsh-pi-ai-1.4.0/turn1.json"),
        include_str!("fixtures/dsh-pi-ai-1.4.0/turn2.json"),
        include_str!("fixtures/dsh-pi-ai-1.4.0/turn3.json"),
    ]
    .iter()
    .map(|s| serde_json::from_str(s).unwrap())
    .collect();
    let f = fixture(Mode::NoProviderUsage).await;
    let state = f
        .state
        .clone()
        .with_cache_meter(Some(Arc::new(crate::anthropic::cache_metering::CacheMeter::new(None))));
    let send = |body: serde_json::Value| {
        let state = state.clone();
        async move {
            let response = post_messages(
                axum::extract::State(state),
                Extension(key()),
                Json(serde_json::from_value(body).unwrap()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            streamed_cache_usage(response).await
        }
    };

    let (read1, create1) = send(turns[0].clone()).await;
    assert_eq!(read1, 0, "首轮没有可读的缓存");
    assert!(create1 > 0, "首轮应写入缓存，实际 create={create1}");

    let (read2, _) = send(turns[1].clone()).await;
    assert!(read2 > 0, "同一会话第二轮应读到缓存（DSH 0% 的 bug），实际 read={read2}");

    let (read3, _) = send(turns[2].clone()).await;
    assert!(read3 > read2, "第三轮应读到第二轮写入的更长前缀：read2={read2} read3={read3}");

    // 另一个会话：第一条 user 消息不同 → 不应读到上一个会话的缓存
    let mut other = turns[1].clone();
    other["messages"][0]["content"] = json!("另一个会话的第一条消息。");
    let (read_other, create_other) = send(other).await;
    assert_eq!(read_other, 0, "不同会话不应命中");
    assert!(create_other > 0);
}
