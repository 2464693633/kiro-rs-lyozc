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
    Normal(Duration),
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
}

struct Fixture {
    state: AppState,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    connections: Arc<Mutex<HashSet<SocketAddr>>>,
    close_headers: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
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
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen_bodies = bodies.clone();
    let connections = Arc::new(Mutex::new(HashSet::new()));
    let close_headers = Arc::new(AtomicUsize::new(0));
    let (seen_calls, seen_connections, seen_close) =
        (calls.clone(), connections.clone(), close_headers.clone());
    let app = Router::new().route("/", post(move |ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes| {
        let (calls, connections, close) = (seen_calls.clone(), seen_connections.clone(), seen_close.clone());
        let bodies = seen_bodies.clone();
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
                bytes.extend_from_slice(&frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})));
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
            if matches!(mode, Mode::SlowHeaders) { tokio::time::sleep(Duration::from_secs(4)).await; }
            if matches!(mode, Mode::SlowErrorBody) {
                return Response::builder().status(500).body(Body::from_stream(stream::pending::<Result<Bytes, Infallible>>())).unwrap();
            }
            if matches!(mode, Mode::Empty) { return Response::new(Body::empty()); }
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
            if matches!(mode, Mode::Tools) {
                let mut bytes = Vec::new();
                for part in [
                    frame("toolUseEvent", json!({"name":"run", "toolUseId":"call_1", "input":"{\"command\":", "stop":false})),
                    frame("toolUseEvent", json!({"name":"run", "toolUseId":"call_1", "input":"\"hello\"}", "stop":true})),
                    frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})),
                ] { bytes.extend_from_slice(&part); }
                return Response::new(Body::from(bytes));
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
            let first = frame("assistantResponseEvent", json!({"content": "early content ".repeat(64)}));
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
                        1 => { if let Mode::Normal(gap) = mode { tokio::time::sleep(gap).await; } Some((Ok(metadata), 2)) }
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
        bodies,
        connections,
        close_headers,
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
async fn request(state: AppState, endpoint: &str, streaming: bool) -> Response {
    let req = json!({"model": "gpt-5.6-sol", "max_tokens": 64, "stream": streaming, "messages": [{"role": "user", "content": "hello"}], "input": [{"role":"user", "content":"hello"}]});
    if endpoint == "chat" {
        post_chat_completions(
            axum::extract::State(state),
            Extension(key()),
            HeaderMap::new(),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else if endpoint == "responses" {
        post_responses(
            axum::extract::State(state),
            Extension(key()),
            HeaderMap::new(),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else if endpoint == "cc" {
        post_messages_cc(
            axum::extract::State(state),
            Extension(key()),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else {
        post_messages(
            axum::extract::State(state),
            Extension(key()),
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
            let f=fixture(Mode::Execution { client_tool:false }).await;
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
    let f=fixture(Mode::ExecutionSchema).await;
    enable_execution_bridge(&f.state);
    let mut request=execution_request(false);
    request["output_config"]=json!({"format":{"type":"json_schema","schema":{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}},"additionalProperties":false}}});
    let response=compatibility_request(f.state.clone(),false,request).await;
    assert_eq!(response.status(),StatusCode::BAD_GATEWAY);
    let bytes=to_bytes(response.into_body(),1024*1024).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("upstream_output_schema_error"));
    assert_eq!(f.calls.load(Ordering::Relaxed),2);
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
