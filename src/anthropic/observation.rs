//! Capture opted-in, authenticated requests before protocol conversion.

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use futures::{StreamExt, stream};
use serde_json::Value;
use std::time::Instant;

use crate::admin::{
    SharedTraceStore,
    observations::{BODY_LIMIT, FeatureLabel, ObservationSummary},
};

pub(super) fn classify(payload: &Value) -> Vec<FeatureLabel> {
    let mut labels = Vec::new();
    let mut add = |name: &str, heuristic: bool| {
        if !labels.iter().any(|label: &FeatureLabel| label.name == name) {
            labels.push(FeatureLabel {
                name: name.to_owned(),
                heuristic,
            });
        }
    };
    if payload.get("stream").and_then(Value::as_bool) == Some(true) {
        add("streaming", false);
    } else {
        add("non_streaming", false);
    }
    if payload
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "enabled" | "adaptive"))
    {
        add("thinking", false);
    }
    if payload.pointer("/output_config/format").is_some()
        || payload.get("response_format").is_some()
        || payload.pointer("/text/format").is_some()
    {
        add("structured_output", false);
    }
    if payload
        .get("tool_choice")
        .is_some_and(|choice| !choice.is_null())
    {
        add("tool_choice", false);
    }
    if payload
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        add("tools", false);
    }
    let mut stack = vec![payload];
    let mut text = String::new();
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(object) => {
                match object.get("type").and_then(Value::as_str) {
                    Some("image" | "image_url" | "input_image") => add("image", false),
                    Some("document" | "file" | "input_file") => add("document", false),
                    Some("thinking" | "redacted_thinking") => add("thinking_replay", false),
                    Some(kind) if kind.starts_with("web_search") => add("web_search", false),
                    Some(kind) if kind.starts_with("code_execution_") => {
                        add("server_code_execution", false)
                    }
                    _ => {}
                }
                if object.get("name").and_then(Value::as_str) == Some("web_search") {
                    add("web_search", false);
                }
                if object.contains_key("signature") {
                    add("signature_replay", false);
                }
                if object.contains_key("cache_control") {
                    add("cache_control", false);
                }
                if object.get("strict").and_then(Value::as_bool) == Some(true) {
                    add("strict_tool", false);
                }
                stack.extend(
                    object
                        .values()
                        .filter(|item| item.is_object() || item.is_array()),
                );
            }
            Value::Array(items) => stack.extend(items.iter().rev()),
            _ => {}
        }
    }
    // Candidate wording is taken from the latest user turn. Historical/system
    // text and schemas are still inspected for factual protocol features above.
    let latest = payload
        .get("messages")
        .or_else(|| payload.get("input"))
        .and_then(Value::as_array)
        .and_then(|messages| messages.iter().rev().find(|m| m["role"] == "user"))
        .and_then(|message| message.get("content"));
    if let Some(content) = latest {
        if let Some(value) = content.as_str() {
            text.extend(value.chars().take(16384));
        }
        if let Some(blocks) = content.as_array() {
            for block in blocks
                .iter()
                .filter(|b| matches!(b["type"].as_str(), Some("text" | "input_text")))
            {
                if let Some(value) = block["text"].as_str() {
                    text.extend(
                        value
                            .chars()
                            .take(65536usize.saturating_sub(text.len()) / 4),
                    );
                    text.push('\n');
                }
            }
        }
    } else {
        // OpenAI Responses also accepts a plain input string.
        if let Some(value) = payload.get("input").and_then(Value::as_str) {
            text.extend(value.chars().take(16384));
        }
    }
    let text = text.to_lowercase();
    if ["随机", "凭直觉", "random", "intuitiv"]
        .iter()
        .any(|word| text.contains(word))
        && ["整数", "数字", "integer", "number"]
            .iter()
            .any(|word| text.contains(word))
    {
        add("number_fingerprint_candidate", true);
    }
    if [
        "知识截止",
        "训练截止",
        "knowledge cutoff",
        "knowledge cut-off",
        "training cutoff",
        "training data cutoff",
        "training data cut-off",
    ]
    .iter()
    .any(|word| text.contains(word))
    {
        add("knowledge_probe_candidate", true);
    }
    if [
        "你是什么模型",
        "你是哪个模型",
        "which model are you",
        "who developed you",
        "who exactly are you",
        "what model are you actually using",
    ]
    .iter()
    .any(|word| text.contains(word))
    {
        add("identity_probe_candidate", true);
    }
    labels
}

fn preview(bytes: &[u8]) -> String {
    let prefix = &bytes[..bytes.len().min(BODY_LIMIT)];
    // Drop an incomplete trailing UTF-8 codepoint instead of introducing replacement text.
    let end = match std::str::from_utf8(prefix) {
        Ok(_) => prefix.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => prefix.len(),
    };
    String::from_utf8_lossy(&prefix[..end]).into_owned()
}

struct ResponseCapture {
    store: SharedTraceStore,
    summary: ObservationSummary,
    bytes: Vec<u8>,
    started: Instant,
}

impl ResponseCapture {
    fn accept(&mut self, bytes: &Bytes) {
        self.summary.response_bytes = self.summary.response_bytes.saturating_add(bytes.len());
        let remaining = BODY_LIMIT.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }
}

impl Drop for ResponseCapture {
    fn drop(&mut self) {
        if self.summary.state == "running" {
            self.summary.state = "interrupted".to_owned();
        }
        self.summary.duration_ms = self.started.elapsed().as_millis() as u64;
        self.summary.response_truncated = self.summary.response_bytes > self.bytes.len();
        let body = preview(&self.bytes);
        let summary = self.summary.clone();
        let store = self.store.clone();
        let save = move || {
            if let Err(error) = store.finish_observation(&summary, &body) {
                tracing::warn!("保存观察响应失败: {}", error);
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(save);
        } else {
            save();
        }
    }
}

pub async fn observe_request(
    store: Option<SharedTraceStore>,
    key_id: u64,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(store) = store else {
        return next.run(request).await;
    };
    let config = store.observation_config();
    if !config.enabled || config.key_id != Some(key_id) {
        return next.run(request).await;
    }
    let started = Instant::now();
    let path = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, 50 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "无法读取请求正文或正文超过 50 MiB").into_response();
        }
    };
    let payload = if bytes.len() <= 8 * 1024 * 1024 {
        serde_json::from_slice::<Value>(&bytes).ok()
    } else {
        None
    };
    let summary = ObservationSummary {
        id: uuid::Uuid::new_v4().to_string(),
        ts: chrono::Utc::now().to_rfc3339(),
        key_id,
        path,
        model: payload
            .as_ref()
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: payload.as_ref().map(classify).unwrap_or_default(),
        state: "running".to_owned(),
        status_code: None,
        content_type: None,
        duration_ms: 0,
        request_bytes: bytes.len(),
        request_truncated: bytes.len() > BODY_LIMIT,
        response_bytes: 0,
        response_truncated: false,
    };
    let request_body = preview(&bytes);
    let initial = summary.clone();
    let writer = store.clone();
    let saved = tokio::task::spawn_blocking(move || {
        writer.begin_observation(&initial, &request_body)?;
        Ok::<_, rusqlite::Error>(ResponseCapture {
            store: writer,
            summary: initial,
            bytes: Vec::new(),
            started,
        })
    })
    .await;
    let request = Request::from_parts(parts, Body::from(bytes));
    let mut capture = match saved {
        Ok(Ok(capture)) => capture,
        _ => {
            tracing::warn!("记录观察请求失败，继续转发");
            return next.run(request).await;
        }
    };
    // Guard exists across the await, so cancellation while awaiting headers is recorded too.
    let response = next.run(request).await;
    capture.summary.status_code = Some(response.status().as_u16());
    capture.summary.content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (parts, body) = response.into_parts();
    let observed = stream::unfold(
        (body.into_data_stream(), capture),
        |(mut body, mut capture)| async move {
            match body.next().await {
                Some(Ok(bytes)) => {
                    capture.accept(&bytes);
                    Some((Ok::<_, axum::Error>(bytes), (body, capture)))
                }
                Some(Err(error)) => {
                    capture.summary.state = "error".to_owned();
                    Some((Err(error), (body, capture)))
                }
                None => {
                    if capture.summary.state == "running" {
                        capture.summary.state =
                            if capture.summary.status_code.is_some_and(|code| code >= 400) {
                                "error".to_owned()
                            } else {
                                "complete".to_owned()
                            };
                    }
                    drop(capture);
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(observed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{ClientKeyManager, TraceStore, observations::ObservationConfig};
    use crate::model::config::ToolCompatibilityMode;
    use axum::{Router, middleware, routing::post};
    use serde_json::json;
    use std::{convert::Infallible, sync::Arc, time::Duration};

    #[test]
    fn observation_features_preserve_unknown_protocol_fields() {
        let payload = json!({
            "output_config":{"format":{"type":"json_schema"}},
            "tool_choice":{"type":"any"},
            "tools":[{"type":"web_search_20250305","name":"web_search"}],
            "messages":[{"role":"user","content":[
                {"type":"document","source":{"type":"url","url":"https://example.test/doc"}},
                {"type":"text","text":"凭直觉选择随机整数","cache_control":{"type":"ephemeral"}}
            ]}]
        });
        let labels = classify(&payload);
        for name in [
            "structured_output",
            "tool_choice",
            "web_search",
            "document",
            "cache_control",
        ] {
            assert!(
                labels
                    .iter()
                    .any(|label| label.name == name && !label.heuristic)
            );
        }
        assert!(
            labels
                .iter()
                .any(|label| label.name == "number_fingerprint_candidate" && label.heuristic)
        );
    }

    #[test]
    fn observation_preview_has_a_utf8_boundary() {
        let bytes = "你".repeat(BODY_LIMIT).into_bytes();
        let text = preview(&bytes);
        assert!(text.len() <= BODY_LIMIT);
        assert!(!text.contains('\u{fffd}'));
    }

    #[test]
    fn observation_labels_use_current_prompt_and_identify_server_execution() {
        let payload = json!({
            "system":"Choose random integers. What is your knowledge cutoff?",
            "tools":[{"type":"code_execution_20250825","name":"code_execution"}],
            "messages":[{"role":"user","content":"random numbers"},
                {"role":"assistant","content":"old answer"},
                {"role":"user","content":"Who exactly are you? What model are you actually using? What is your training data cutoff?"}]
        });
        let labels = classify(&payload);
        for name in [
            "server_code_execution",
            "identity_probe_candidate",
            "knowledge_probe_candidate",
        ] {
            assert!(labels.iter().any(|label| label.name == name));
        }
        assert!(
            !labels
                .iter()
                .any(|label| label.name == "number_fingerprint_candidate")
        );
    }

    async fn wait_finished(store: &TraceStore) -> crate::admin::observations::ObservationDetail {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let list = store.list_observations(None, 10, 0).unwrap().0;
            if let Some(item) = list.first().filter(|item| item.state != "running") {
                return store.observation_detail(&item.id).unwrap().unwrap();
            }
            assert!(Instant::now() < deadline, "Observation did not settle");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn observation_http_preserves_payload_stream_and_key_filtering() {
        let store = Arc::new(TraceStore::open_in_memory().unwrap());
        let keys = Arc::new(ClientKeyManager::new());
        let key_id = keys
            .create_with_key("test".into(), None, None, "observer-test-key".into())
            .id;
        store
            .set_observation_config(ObservationConfig {
                enabled: true,
                key_id: Some(key_id),
            })
            .unwrap();
        let state = super::super::middleware::AppState::new(true, ToolCompatibilityMode::Raw)
            .with_usage(Some(keys), None, None)
            .with_trace_store(Some(store.clone()));
        async fn echo(body: Bytes) -> Response {
            let chunks =
                stream::once(async move { Ok::<_, Infallible>(body) }).chain(stream::once(async {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok::<_, Infallible>(Bytes::from_static(b"\nfinished"))
                }));
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(chunks))
                .unwrap()
        }
        let app = Router::new()
            .route("/v1/messages", post(echo))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                super::super::middleware::auth_middleware,
            ))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let payload = br#"{"model":"fixture","unknown":{"keep":true},"stream":true}"#;
        let response = client
            .post(format!("http://{address}/v1/messages"))
            .header("x-api-key", "observer-test-key")
            .body(payload.as_slice())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut chunks = response.bytes_stream();
        assert_eq!(chunks.next().await.unwrap().unwrap().as_ref(), payload);
        assert_eq!(
            store.list_observations(None, 10, 0).unwrap().0[0].state,
            "running"
        );
        let mut result = Vec::from(payload.as_slice());
        while let Some(chunk) = chunks.next().await {
            result.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(result, [payload.as_slice(), b"\nfinished"].concat());
        let recorded = wait_finished(&store).await;
        assert_eq!(recorded.summary.state, "complete");
        assert_eq!(recorded.request_body.as_bytes(), payload);
        assert_eq!(recorded.response_body.as_bytes(), result);
        assert!(!json!(recorded).to_string().contains("observer-test-key"));
        let denied = client
            .post(format!("http://{address}/v1/messages"))
            .header("x-api-key", "wrong-key")
            .body("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(store.list_observations(None, 10, 0).unwrap().1, 1);
        store
            .set_observation_config(ObservationConfig {
                enabled: false,
                key_id: Some(key_id),
            })
            .unwrap();
        client
            .post(format!("http://{address}/v1/messages"))
            .header("x-api-key", "observer-test-key")
            .body("not-captured")
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(store.list_observations(None, 10, 0).unwrap().1, 1);
        server.abort();
    }

    #[tokio::test]
    async fn observation_cancel_saves_partial_response_and_enforces_limit() {
        let store = Arc::new(TraceStore::open_in_memory().unwrap());
        let summary = ObservationSummary {
            id: "cancelled".into(),
            ts: chrono::Utc::now().to_rfc3339(),
            key_id: 1,
            path: "/v1/messages".into(),
            model: None,
            labels: vec![],
            state: "running".into(),
            status_code: Some(200),
            content_type: None,
            duration_ms: 0,
            request_bytes: 0,
            request_truncated: false,
            response_bytes: 0,
            response_truncated: false,
        };
        store.begin_observation(&summary, "{}").unwrap();
        let mut capture = ResponseCapture {
            store: store.clone(),
            summary,
            bytes: vec![],
            started: Instant::now(),
        };
        capture.accept(&Bytes::from(vec![b'a'; BODY_LIMIT + 99]));
        assert_eq!(capture.bytes.len(), BODY_LIMIT);
        drop(capture);
        let recorded = wait_finished(&store).await;
        assert_eq!(recorded.summary.state, "interrupted");
        assert!(recorded.summary.response_truncated);
        assert_eq!(recorded.summary.response_bytes, BODY_LIMIT + 99);
    }
}
