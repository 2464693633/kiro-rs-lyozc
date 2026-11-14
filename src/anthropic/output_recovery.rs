//! One bounded recovery for reasoning that ended without an answer. Candidate
//! streams hold their pre-answer events, so a discarded attempt cannot leak a
//! second message_start, stale signatures, or an executable tool call.
use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{
    handlers::{self, UsageRecordHook},
    identity_guard::{Settlement, UsageCapture},
    middleware::{AppState, KeyContext},
    types::{ErrorResponse, MessagesRequest, OutputConfig, SystemMessage},
};

pub(super) const ERROR_TYPE: &str = "upstream_no_visible_output";
pub(super) const ERROR_MESSAGE: &str = "The model finished reasoning without producing an answer or tool call. Reduce reasoning effort or shorten the conversation before retrying.";
const MAX_BUFFER: usize = 4 * 1024 * 1024;
const RECOVERY_INSTRUCTION: &str = "The previous generation ended after reasoning without producing a visible answer or a tool call. Complete the current task now with a concise final answer or the required client tool call. Preserve the original output format and tool constraints. Do not repeat a long analysis or claim that a tool has already executed.";

pub(super) fn is_no_visible_output(content: &[Value], stop: &str, hidden_reasoning: bool) -> bool {
    stop == "end_turn"
        && (hidden_reasoning
            || content.iter().any(|block| {
                block["type"] == "thinking"
                    && block["thinking"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty())
            }))
        && !content.iter().any(|block| {
            block["type"] == "tool_use"
                || (block["type"] == "text"
                    && block["text"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty()))
        })
}

fn eligible(payload: &MessagesRequest) -> bool {
    payload.thinking.as_ref().is_some_and(|thinking| thinking.is_enabled())
        && payload.messages.last().is_some_and(|message| message.role == "user")
        // A server tool may already have executed during the first generation.
        && !payload.tools.as_ref().is_some_and(|tools| tools.iter().any(|tool| {
            tool.tool_type.as_deref().is_some_and(|kind| kind != "custom")
        }))
}

fn recovery_request(payload: &MessagesRequest) -> MessagesRequest {
    let mut retry = payload.clone();
    let output = retry.output_config.get_or_insert_with(|| OutputConfig {
        effort: String::new(),
        format: None,
    });
    if !matches!(output.effort.as_str(), "low" | "medium") {
        output.effort = "high".into();
    }
    if let Some(thinking) = retry.thinking.as_mut() {
        thinking.budget_tokens = thinking.budget_tokens.min(20_000);
    }
    retry
        .system
        .get_or_insert_with(Vec::new)
        .push(SystemMessage {
            text: RECOVERY_INSTRUCTION.into(),
            cache_control: None,
        });
    retry
}

async fn dispatch(
    state: AppState,
    key: KeyContext,
    payload: MessagesRequest,
    cc: bool,
    buffered_sse: bool,
) -> Response {
    if cc {
        handlers::post_messages_cc_inner_impl(state, key, payload, buffered_sse).await
    } else {
        handlers::post_messages_inner_impl(state, key, payload, buffered_sse).await
    }
}

pub(super) async fn run(
    mut state: AppState,
    key: KeyContext,
    mut payload: MessagesRequest,
    cc: bool,
    buffered_sse: bool,
) -> Response {
    handlers::override_thinking_from_model_name(&mut payload);
    if !eligible(&payload) {
        return dispatch(state, key, payload, cc, buffered_sse).await;
    }
    let hook = UsageRecordHook::from_state(&state, key.key_id, payload.model.clone());
    let capture = Arc::new(UsageCapture::with_policy(hook.cache_policy));
    let settlement = Settlement::new(hook, capture.clone());
    state.usage_capture = Some(capture.clone());
    let response = dispatch(
        state.clone(),
        key.clone(),
        payload.clone(),
        cc,
        buffered_sse,
    )
    .await;
    if payload.stream && response.status().is_success() {
        return stream_response(response, state, key, payload, cc, capture, settlement);
    }
    let mut settlement = settlement;
    let response = json_response(response, state, key, payload, cc, buffered_sse, &capture).await;
    settlement.finish(response.status().is_success());
    response
}

async fn json_response(
    mut response: Response,
    state: AppState,
    key: KeyContext,
    payload: MessagesRequest,
    cc: bool,
    buffered_sse: bool,
    capture: &UsageCapture,
) -> Response {
    for attempt in 0..2 {
        let (mut parts, body) = response.into_parts();
        let Ok(bytes) = to_bytes(body, MAX_BUFFER).await else {
            return failure(
                "api_error",
                "Response exceeded the output recovery buffer limit",
            );
        };
        let Ok(mut message) = serde_json::from_slice::<Value>(&bytes) else {
            return Response::from_parts(parts, Body::from(bytes));
        };
        if attempt == 0 && message["error"]["type"] == ERROR_TYPE {
            tracing::warn!(original_trace = ?parts.headers.get("x-request-id"),
                "Retrying a no-visible-output response once with reduced reasoning effort");
            response = dispatch(
                state.clone(),
                key.clone(),
                recovery_request(&payload),
                cc,
                buffered_sse,
            )
            .await;
            continue;
        }
        if parts.status.is_success() && message["content"].is_array() {
            capture.merge_usage(&mut message["usage"]);
        }
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::from(message.to_string()));
    }
    unreachable!("one recovery attempt only")
}

fn failure(kind: &str, message: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(kind, message)),
    )
        .into_response()
}

fn sse(event: &Value) -> Bytes {
    Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        event["type"].as_str().unwrap_or("error"),
        event
    ))
}

async fn send(sender: &mpsc::Sender<Bytes>, event: Value) -> Result<(), ()> {
    sender.send(sse(&event)).await.map_err(|_| ())
}

async fn send_error(sender: &mpsc::Sender<Bytes>, kind: &str, message: &str) {
    let _ = send(
        sender,
        json!({"type":"error","error":{"type":kind,"message":message}}),
    )
    .await;
}

fn visible(event: &Value) -> bool {
    (event["type"] == "content_block_start"
        && (event["content_block"]["type"] == "tool_use"
            || (event["content_block"]["type"] == "text"
                && event["content_block"]["text"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()))))
        || (event["type"] == "content_block_delta"
            && event["delta"]["type"] == "text_delta"
            && event["delta"]["text"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()))
}

fn stream_response(
    response: Response,
    state: AppState,
    key: KeyContext,
    payload: MessagesRequest,
    cc: bool,
    capture: Arc<UsageCapture>,
    mut settlement: Settlement,
) -> Response {
    let (mut parts, body) = response.into_parts();
    let original_trace = parts.headers.get("x-request-id").cloned();
    parts.headers.remove(header::CONTENT_LENGTH);
    let (sender, receiver) = mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        let mut body = body;
        let mut ping = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
        );
        for attempt in 0..2 {
            let mut source = body.into_data_stream();
            let mut pending = Vec::<u8>::new();
            let mut held = Vec::<Bytes>::new();
            let mut held_bytes = 0usize;
            let mut committed = false;
            let mut complete = false;
            let mut recover = false;
            let mut failed = false;
            let mut error_sent = false;
            'read: loop {
                let chunk = tokio::select! {
                    biased;
                    _ = sender.closed() => { drop(source); return; },
                    chunk = source.next() => chunk,
                    _ = ping.tick() => {
                        if send(&sender, json!({"type":"ping"})).await.is_err() { drop(source); return; }
                        continue;
                    }
                };
                match chunk {
                    Some(Ok(bytes)) => pending.extend_from_slice(&bytes),
                    Some(Err(_)) => {
                        failed = true;
                        break;
                    }
                    None => break,
                }
                while let Some((position, delimiter)) = frame_boundary(&pending) {
                    let frame: Vec<_> = pending.drain(..position + delimiter).collect();
                    let Some(mut event) = parse_frame(&frame) else {
                        failed = true;
                        break 'read;
                    };
                    if event["type"] == "ping" {
                        if sender.send(sse(&event)).await.is_err() {
                            drop(source);
                            return;
                        }
                        continue;
                    }
                    if event["type"] == "error"
                        && event["error"]["type"] == ERROR_TYPE
                        && !committed
                    {
                        recover = attempt == 0;
                        if !recover {
                            send_error(&sender, ERROR_TYPE, ERROR_MESSAGE).await;
                            error_sent = true;
                        }
                        failed = true;
                        break 'read;
                    }
                    failed |= event["type"] == "error";
                    error_sent |= event["type"] == "error";
                    complete |= event["type"] == "message_stop";
                    if event["type"] == "message_delta" {
                        capture.merge_usage(&mut event["usage"]);
                    }
                    let release = visible(&event) || failed || complete;
                    let bytes = sse(&event);
                    if !committed {
                        held_bytes = held_bytes.saturating_add(bytes.len());
                        if held_bytes > MAX_BUFFER {
                            send_error(&sender, "api_error", "Reasoning exceeded the output recovery buffer limit. Reduce reasoning effort.").await;
                            drop(source);
                            settlement.finish(false);
                            return;
                        }
                        held.push(bytes);
                        if release {
                            for bytes in held.drain(..) {
                                if sender.send(bytes).await.is_err() {
                                    drop(source);
                                    return;
                                }
                            }
                            committed = true;
                        }
                    } else if sender.send(bytes).await.is_err() {
                        drop(source);
                        return;
                    }
                    // message_stop is authoritative. Do not poll the channel
                    // again and mistake a client closing a completed message
                    // for a cancelled generation.
                    if failed || complete {
                        break 'read;
                    }
                }
                if pending.len() > MAX_BUFFER {
                    failed = true;
                    break;
                }
            }
            drop(source); // Inner guards settle before aggregate usage is read.
            if !recover {
                if (!complete || failed || !pending.is_empty()) && !error_sent {
                    send_error(
                        &sender,
                        "api_error",
                        "Upstream response ended before a complete message was received",
                    )
                    .await;
                }
                settlement.finish(complete && !failed && pending.is_empty());
                return;
            }
            tracing::warn!(
                ?original_trace,
                "Retrying a no-visible-output stream once with reduced reasoning effort"
            );
            let request = dispatch(
                state.clone(),
                key.clone(),
                recovery_request(&payload),
                cc,
                false,
            );
            tokio::pin!(request);
            let response = loop {
                tokio::select! {
                    biased;
                    _ = sender.closed() => return,
                    response = &mut request => break response,
                    _ = ping.tick() => {
                        if send(&sender, json!({"type":"ping"})).await.is_err() { return; }
                    }
                }
            };
            tracing::info!(?original_trace, recovery_trace = ?response.headers().get("x-request-id"),
                "No-visible-output recovery request received upstream headers");
            if !response.status().is_success() {
                let bytes = to_bytes(response.into_body(), MAX_BUFFER).await;
                let error = bytes
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|message| message.get("error").cloned())
                    .unwrap_or_else(
                        || json!({"type":"api_error","message":"Output recovery failed"}),
                    );
                let _ = send(&sender, json!({"type":"error","error":error})).await;
                settlement.finish(false);
                return;
            }
            body = response.into_body();
        }
    });
    let output = stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), receiver))
    });
    Response::from_parts(parts, Body::from_stream(output))
}

fn frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .map(|pos| (pos, 2))
        .or_else(|| {
            bytes
                .windows(4)
                .position(|pair| pair == b"\r\n\r\n")
                .map(|pos| (pos, 4))
        })
}

fn parse_frame(frame: &[u8]) -> Option<Value> {
    let frame = std::str::from_utf8(frame).ok()?;
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_recovery_settles_message_stop_without_waiting_for_transport_eof() {
        use crate::admin::{
            client_keys::ClientKeyManager, trace_db::TraceKeySource, usage_stats::UsageRecord,
        };
        let keys = Arc::new(ClientKeyManager::new());
        let client = keys.create("completed stream".into(), None, None);
        let state = AppState::new(true, crate::model::config::ToolCompatibilityMode::Raw)
            .with_usage(Some(keys.clone()), None, None);
        let key = KeyContext {
            key_id: client.id,
            group: None,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
        };
        let capture = Arc::new(UsageCapture::default());
        capture.record(UsageRecord {
            ts: chrono::Utc::now().to_rfc3339(),
            key_id: client.id,
            credential_id: 1,
            model: "claude-opus-5-5".into(),
            input_tokens: 13,
            output_tokens: 17,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.125,
            duration_ms: 1,
            status: "success".into(),
        }, None);
        let settlement = Settlement::new(
            UsageRecordHook::from_state(&state, client.id, "claude-opus-5-5".into()),
            capture.clone(),
        );
        let mut wire = Vec::new();
        for event in [
            json!({"type":"message_start","message":{"id":"completed","usage":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}),
            json!({"type":"message_stop"}),
        ] {
            wire.extend_from_slice(&sse(&event));
        }
        let source = stream::once(async move { Ok::<_, Infallible>(Bytes::from(wire)) })
            .chain(stream::pending::<Result<Bytes, Infallible>>());
        let request: MessagesRequest = serde_json::from_value(json!({
            "model":"claude-opus-5-5","max_tokens":64,"stream":true,
            "messages":[{"role":"user","content":"task"}],"thinking":{"type":"adaptive"}
        }))
        .unwrap();
        let response = stream_response(
            Response::new(Body::from_stream(source)),
            state,
            key,
            request,
            false,
            capture,
            settlement,
        );
        let bytes = tokio::time::timeout(
            Duration::from_millis(500),
            to_bytes(response.into_body(), 1024 * 1024),
        )
        .await
        .expect("completed SSE must not wait for an idle transport")
        .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("message_stop"));
        let billed = keys.list()[0].clone();
        assert_eq!(billed.total_successful_calls, 1);
        assert_eq!(billed.total_output_tokens, 17);
        assert_eq!(billed.total_credits, 0.125);
    }

    #[test]
    fn output_recovery_only_classifies_substantive_silent_end_turns() {
        let reasoning = vec![json!({"type":"thinking","thinking":"unfinished work"})];
        assert!(is_no_visible_output(&reasoning, "end_turn", false));
        assert!(is_no_visible_output(&[], "end_turn", true));
        for stop in [
            "max_tokens",
            "refusal",
            "model_context_window_exceeded",
            "stop_sequence",
        ] {
            assert!(!is_no_visible_output(&reasoning, stop, false));
        }
        assert!(!is_no_visible_output(
            &[json!({"type":"thinking","thinking":"","signature":"original"})],
            "end_turn",
            false
        ));
        assert!(!is_no_visible_output(
            &[json!({"type":"redacted_thinking","data":"private"})],
            "end_turn",
            false
        ));
        let mut tool = reasoning;
        tool.push(json!({"type":"tool_use","id":"original","name":"Grep","input":{}}));
        assert!(!is_no_visible_output(&tool, "end_turn", false));
    }

    #[test]
    fn output_recovery_does_not_enable_disabled_thinking_or_retry_server_tools() {
        let mut payload: MessagesRequest = serde_json::from_value(json!({
            "model":"claude-opus-5-5", "max_tokens":64,
            "messages":[{"role":"user","content":"task"}], "thinking":{"type":"disabled"}
        }))
        .unwrap();
        assert!(!eligible(&payload));
        payload.thinking.as_mut().unwrap().thinking_type = "adaptive".into();
        for kind in ["web_search_20250305", "code_execution_20250825"] {
            payload.tools =
                Some(serde_json::from_value(json!([{"type":kind,"name":"server_tool"}])).unwrap());
            assert!(!eligible(&payload));
        }
        payload.tools = None;
        payload.output_config = Some(OutputConfig {
            effort: "low".into(),
            format: Some(json!({"type":"json_schema"})),
        });
        let retry = recovery_request(&payload);
        assert_eq!(retry.output_config.as_ref().unwrap().effort, "low");
        assert_eq!(
            retry.output_config.unwrap().format,
            payload.output_config.unwrap().format
        );
    }
}
