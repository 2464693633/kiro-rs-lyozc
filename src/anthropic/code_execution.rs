//! Server-tool compatibility bridge. No commands are run by this process.
//! A selected execution tool produces an explicit unavailable error and is
//! returned to the model; ordinary client tools remain client-owned.
use std::{
    collections::HashSet,
    convert::Infallible,
    panic::AssertUnwindSafe,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{
    handlers,
    middleware::{AppState, KeyContext},
    stream::SseEvent,
    structured_output,
    types::{ErrorResponse, Message, MessagesRequest, Tool},
};
use crate::model::config::CodeExecutionMode;

const BASH: &str = "__kiro_server_bash_execution";
const EDITOR: &str = "__kiro_server_text_editor_execution";
const MAX_ROUNDS: usize = 4;
const MAX_BODY: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONTINUATION_RETRIES: usize = 2;

struct Progress {
    sender: mpsc::Sender<Bytes>,
    emitted: Arc<AtomicUsize>,
}

impl Progress {
    async fn publish(&self, content: &[Value]) -> Result<(), ()> {
        let offset = self.emitted.load(Ordering::Acquire);
        for event in structured_output::content_events(&content[offset..], offset) {
            self.sender
                .send(Bytes::from(event.to_sse_string()))
                .await
                .map_err(|_| ())?;
        }
        self.emitted.store(content.len(), Ordering::Release);
        Ok(())
    }
}

fn continuation_retry_delay(
    response: &Response,
    retry: usize,
    elapsed: Duration,
) -> Option<Duration> {
    if response.status() != StatusCode::TOO_MANY_REQUESTS || retry >= MAX_CONTINUATION_RETRIES {
        return None;
    }
    let seconds = match response.headers().get(header::RETRY_AFTER) {
        Some(value) => {
            let value = value.to_str().ok()?;
            match value.parse::<u64>() {
                Ok(seconds) => seconds,
                Err(_) => {
                    let time = chrono::DateTime::parse_from_rfc2822(value).ok()?;
                    time.timestamp()
                        .saturating_sub(chrono::Utc::now().timestamp())
                        .max(0) as u64
                }
            }
        }
        None => 5 * (1 << retry),
    };
    let delay = Duration::from_secs(seconds.max(1));
    // Never shorten an upstream Retry-After to fit the bridge deadline.
    TIMEOUT
        .checked_sub(elapsed)
        .filter(|remaining| remaining.as_secs() > delay.as_secs().saturating_add(1))?;
    Some(delay)
}

#[derive(Clone)]
pub(super) struct Plan;

pub(super) fn prepare(
    payload: &mut MessagesRequest,
    mode: CodeExecutionMode,
) -> Result<Option<Plan>, String> {
    let Some(tools) = payload.tools.as_ref() else {
        return Ok(None);
    };
    let declared: Vec<_> = tools
        .iter()
        .filter(|tool| {
            tool.tool_type
                .as_deref()
                .is_some_and(|kind| kind.starts_with("code_execution_"))
        })
        .collect();
    if declared.is_empty() {
        return Ok(None);
    }
    if mode == CodeExecutionMode::Reject {
        return Err("Server-side code_execution is unavailable: this gateway has no isolated execution backend. Enable the protocol tool bridge to receive explicit tool-unavailable results.".into());
    }
    if declared.len() != 1
        || declared[0].tool_type.as_deref() != Some("code_execution_20250825")
        || declared[0].name != "code_execution"
    {
        return Err(
            "The execution bridge supports one code_execution_20250825 tool named code_execution"
                .into(),
        );
    }
    if tools
        .iter()
        .any(|tool| matches!(tool.name.as_str(), BASH | EDITOR))
    {
        return Err("Execution bridge tool names are reserved".into());
    }
    if payload
        .tool_choice
        .as_ref()
        .is_some_and(|choice| choice["type"] == "none")
    {
        payload.tools = None;
        normalize_history(payload);
        return Ok(None);
    }
    let cache_control = declared[0].cache_control.clone();
    let tools = payload.tools.as_mut().unwrap();
    tools.retain(|tool| tool.tool_type.as_deref() != Some("code_execution_20250825"));
    for (name, description, schema) in [
        (
            BASH,
            "Request server-side shell/Python execution. The gateway returns an explicit tool error if the execution backend is unavailable. Never infer execution success without a successful result.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}),
        ),
        (
            EDITOR,
            "Request the server execution environment's text editor. Results may report that no execution backend is available.",
            json!({"type":"object","properties":{"command":{"type":"string","enum":["view","create","str_replace"]},"path":{"type":"string"},"file_text":{"type":"string"},"old_str":{"type":"string"},"new_str":{"type":"string"},"view_range":{"type":"array","items":{"type":"integer"}}},"required":["command","path"]}),
        ),
    ] {
        tools.push(Tool {
            tool_type: None,
            name: name.into(),
            description: description.into(),
            input_schema: schema
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            max_uses: None,
            cache_control: cache_control.clone(),
        });
    }
    if let Some(choice) = payload.tool_choice.as_mut() {
        if choice["type"] == "tool" && choice["name"] == "code_execution" {
            choice["name"] = json!(BASH);
        }
    }
    normalize_history(payload);
    Ok(Some(Plan))
}

fn execution_name(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        BASH => Some(("bash_code_execution", "bash_code_execution_tool_result")),
        EDITOR => Some((
            "text_editor_code_execution",
            "text_editor_code_execution_tool_result",
        )),
        _ => None,
    }
}

// Client replay includes server tool results in the assistant turn. Convert
// only these supported execution blocks to the Kiro tool/result history form.
fn normalize_history(payload: &mut MessagesRequest) {
    let mut messages = Vec::new();
    for message in std::mem::take(&mut payload.messages) {
        if message.role != "assistant" || !message.content.is_array() {
            messages.push(message);
            continue;
        }
        let mut pending = Vec::new();
        let mut execution_ids = HashSet::new();
        for mut block in message.content.as_array().unwrap().iter().cloned() {
            if block["type"] == "server_tool_use" {
                let internal = match block["name"].as_str() {
                    Some("bash_code_execution") => Some(BASH),
                    Some("text_editor_code_execution") => Some(EDITOR),
                    Some("web_search") => Some("web_search"),
                    _ => None,
                };
                if let Some(internal) = internal {
                    if let Some(id) = block["id"].as_str() {
                        execution_ids.insert(id.to_string());
                    }
                    block["type"] = json!("tool_use");
                    block["name"] = json!(internal);
                }
            }
            if matches!(
                block["type"].as_str(),
                Some(
                    "bash_code_execution_tool_result"
                        | "text_editor_code_execution_tool_result"
                        | "web_search_tool_result"
                )
            ) && block["tool_use_id"]
                .as_str()
                .is_some_and(|id| execution_ids.remove(id))
            {
                if !pending.is_empty() {
                    messages.push(Message {
                        role: "assistant".into(),
                        content: json!(std::mem::take(&mut pending)),
                    });
                }
                let is_error = block["content"]["type"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with("_tool_result_error"))
                    || block["content"]["return_code"]
                        .as_i64()
                        .is_some_and(|code| code != 0);
                messages.push(Message { role: "user".into(), content: json!([{"type":"tool_result","tool_use_id":block["tool_use_id"],"is_error":is_error,"content":block["content"].to_string()}]) });
            } else {
                pending.push(block);
            }
        }
        if !pending.is_empty() {
            messages.push(Message {
                role: "assistant".into(),
                content: json!(pending),
            });
        }
    }
    payload.messages = messages;
}

fn result_for(block: &Value) -> Option<(Value, Value)> {
    let (name, result_type) = execution_name(block["name"].as_str()?)?;
    let id = format!("srvtoolu_{}", Uuid::new_v4().simple());
    let mut call = block.clone();
    call["type"] = json!("server_tool_use");
    call["name"] = json!(name);
    call["id"] = json!(id);
    let error_type = if name == "bash_code_execution" {
        "bash_code_execution_tool_result_error"
    } else {
        "text_editor_code_execution_tool_result_error"
    };
    let result = json!({"type":result_type,"tool_use_id":id,"content":{"type":error_type,"error_code":"unavailable"}});
    Some((call, result))
}

fn aggregate_usage(total: &mut Value, usage: &Value) {
    if let Some(fields) = usage.as_object() {
        for (key, value) in fields {
            if let Some(number) = value.as_i64() {
                total[key] = json!(total[key].as_i64().unwrap_or(0).saturating_add(number));
            } else if key == "credit_usage" {
                total[key] =
                    json!(total[key].as_f64().unwrap_or(0.0) + value.as_f64().unwrap_or(0.0));
            } else if value.is_object() {
                if !total[key].is_object() {
                    total[key] = json!({});
                }
                aggregate_usage(&mut total[key], value);
            } else {
                total[key] = value.clone();
            }
        }
    }
}

async fn run_rounds(
    state: AppState,
    key: KeyContext,
    mut payload: MessagesRequest,
    cc: bool,
    _plan: Plan,
    progress: Option<Progress>,
) -> Response {
    let started = tokio::time::Instant::now();
    payload.stream = false;
    let mut presentation = Vec::new();
    let mut usage = json!({});
    let mut total_bytes = 0usize;
    let original_search_limits: Vec<_> = payload
        .tools
        .iter()
        .flatten()
        .filter(|tool| {
            tool.tool_type
                .as_deref()
                .is_some_and(|kind| kind.starts_with("web_search_"))
        })
        .map(|tool| (tool.name.clone(), tool.max_uses))
        .collect();
    for round in 0..MAX_ROUNDS {
        let mut round_payload = payload.clone();
        let searches_used = usage["server_tool_use"]["web_search_requests"]
            .as_i64()
            .unwrap_or(0)
            .min(i32::MAX as i64) as i32;
        for tool in round_payload.tools.iter_mut().flatten() {
            if let Some((_, Some(limit))) = original_search_limits
                .iter()
                .find(|(name, _)| name == &tool.name)
            {
                tool.max_uses = Some(limit.saturating_sub(searches_used).max(0));
            }
        }
        let mut retry = 0;
        let response = loop {
            let response = if cc {
                handlers::post_messages_cc_inner(state.clone(), key.clone(), round_payload.clone())
                    .await
            } else {
                handlers::post_messages_inner(state.clone(), key.clone(), round_payload.clone())
                    .await
            };
            if round > 0
                && let Some(delay) = continuation_retry_delay(&response, retry, started.elapsed())
            {
                retry += 1;
                tracing::warn!(
                    round,
                    retry,
                    delay_secs = delay.as_secs(),
                    "execution bridge continuation rate limited; waiting before retry"
                );
                drop(response);
                tokio::time::sleep(delay).await;
                continue;
            }
            break response;
        };
        if !response.status().is_success() {
            tracing::warn!(round, status=%response.status(), completed_blocks=presentation.len(), "execution bridge upstream round failed");
            return response;
        }
        let bytes = match to_bytes(response.into_body(), MAX_BODY).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Execution bridge response exceeded its body limit",
                );
            }
        };
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_BODY {
            return error(
                StatusCode::BAD_GATEWAY,
                "Execution bridge aggregate exceeded its body limit",
            );
        }
        let mut message: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Execution bridge received an invalid message",
                );
            }
        };
        aggregate_usage(&mut usage, &message["usage"]);
        let Some(content) = message["content"].as_array().cloned() else {
            return error(
                StatusCode::BAD_GATEWAY,
                "Execution bridge received no content array",
            );
        };
        let terminal_override = !matches!(
            message["stop_reason"].as_str(),
            Some("tool_use" | "end_turn")
        );
        let mut feedback = Vec::new();
        let mut selected = false;
        let mut client_tools = false;
        for block in &content {
            if terminal_override
                && block["type"] == "tool_use"
                && execution_name(block["name"].as_str().unwrap_or("")).is_some()
            {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Upstream ended an execution-tool turn before it could be processed",
                );
            }
            if !terminal_override && block["type"] == "tool_use" {
                if let Some((call, result)) = result_for(block) {
                    selected = true;
                    presentation.push(call);
                    presentation.push(result);
                    feedback.push(json!({"type":"tool_result","tool_use_id":block["id"],"is_error":true,"content":"Server-side code execution is unavailable: no isolated execution backend is configured. No command was run."}));
                    continue;
                }
                client_tools = true;
            }
            presentation.push(block.clone());
        }
        if selected {
            tracing::info!(
                round,
                server_tools = feedback.len(),
                "execution bridge completed tool round"
            );
        }
        if let Some(progress) = progress.as_ref() {
            if progress.publish(&presentation).await.is_err() {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Execution bridge client disconnected",
                );
            }
        }
        if !selected || client_tools {
            message["content"] = json!(presentation);
            message["usage"] = usage;
            if selected && !terminal_override {
                message["stop_reason"] = json!(if client_tools { "tool_use" } else { "end_turn" });
            }
            let stop = message["stop_reason"].as_str().unwrap_or("").to_owned();
            structured_output::present_identity(
                structured_output::format(&payload),
                message["content"].as_array_mut().unwrap(),
                &stop,
                state
                    .protocol_compatibility_for_key(key.key_id)
                    .structured_identity_compat,
            );
            if let Err(reason) = structured_output::validate(
                structured_output::format(&payload),
                message["content"].as_array().unwrap(),
                message["stop_reason"].as_str().unwrap_or("end_turn"),
            ) {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new("upstream_output_schema_error", reason)),
                )
                    .into_response();
            }
            return Json(message).into_response();
        }
        if round + 1 == MAX_ROUNDS {
            return error(
                StatusCode::BAD_GATEWAY,
                "Execution tool loop exceeded its round limit",
            );
        }
        payload.messages.push(Message {
            role: "assistant".into(),
            content: json!(content),
        });
        payload.messages.push(Message {
            role: "user".into(),
            content: json!(feedback),
        });
        if state
            .protocol_compatibility_for_key(key.key_id)
            .search_context_compat
            && let Some(provider) = state.kiro_provider.as_ref()
        {
            let restored = super::search_context::Session::new(
                provider.token_manager().config(),
                key.key_id,
                state.protocol_compatibility.native_search_contexts.clone(),
            )
            .and_then(|context| context.restore_history(&mut payload));
            if let Err(message) = restored {
                return error(StatusCode::BAD_GATEWAY, &message);
            }
        }
        normalize_history(&mut payload);
        // A forced tool choice applies to the original request, not forever
        // after the tool already returned an error.
        payload.tool_choice = Some(json!({"type":"auto"}));
    }
    unreachable!()
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(ErrorResponse::new("api_error", message))).into_response()
}

pub(super) async fn run(
    state: AppState,
    key: KeyContext,
    payload: MessagesRequest,
    cc: bool,
    plan: Plan,
) -> Response {
    if !payload.stream {
        return match tokio::time::timeout(TIMEOUT, run_rounds(state, key, payload, cc, plan, None))
            .await
        {
            Ok(response) => response,
            Err(_) => error(
                StatusCode::GATEWAY_TIMEOUT,
                "Execution tool bridge timed out",
            ),
        };
    }
    let model = payload.model.clone();
    let initial = json!({"type":"message_start","message":{"id":format!("msg_{}",Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}});
    let (sender, receiver) = mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        let emitted = Arc::new(AtomicUsize::new(0));
        // A structured answer may still need rewriting/validation at the end;
        // preserve its buffered path so no unvalidated JSON is emitted.
        let progress = structured_output::format(&payload)
            .is_none()
            .then(|| Progress {
                sender: sender.clone(),
                emitted: emitted.clone(),
            });
        let work = AssertUnwindSafe(tokio::time::timeout(
            TIMEOUT,
            run_rounds(state, key, payload, cc, plan, progress),
        ))
        .catch_unwind();
        tokio::pin!(work);
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
        );
        let response = loop {
            tokio::select! {
                _ = sender.closed() => return,
                result = &mut work => break match result { Ok(Ok(response)) => response, Ok(Err(_)) => error(StatusCode::GATEWAY_TIMEOUT, "Execution tool bridge timed out"), Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Execution tool bridge failed") },
                _ = interval.tick() => if sender.send(Bytes::from(SseEvent::new("ping",json!({"type":"ping"})).to_sse_string())).await.is_err() { return; },
            }
        };
        let status = response.status();
        let body = to_bytes(response.into_body(), MAX_BODY)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let events = if status.is_success() {
            match body {
                Some(message) => structured_output::sse_events(&message)
                    .into_iter()
                    .skip(1)
                    .filter(|event| {
                        !event.event.starts_with("content_block_")
                            || event.data["index"].as_u64().is_some_and(|index| {
                                index as usize >= emitted.load(Ordering::Acquire)
                            })
                    })
                    .collect(),
                None => vec![SseEvent::new(
                    "error",
                    json!({"type":"error","error":{"type":"api_error","message":"Invalid execution bridge response"}}),
                )],
            }
        } else {
            vec![SseEvent::new(
                "error",
                json!({"type":"error","error":body.and_then(|b|b.get("error").cloned()).unwrap_or_else(||json!({"type":"api_error","message":"Execution bridge failed"}))}),
            )]
        };
        for event in events {
            if sender
                .send(Bytes::from(event.to_sse_string()))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let initial = stream::once(async move {
        Ok::<Bytes, Infallible>(Bytes::from(
            SseEvent::new("message_start", initial).to_sse_string(),
        ))
    });
    let rest = stream::unfold(receiver, |mut receiver| async {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), receiver))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(initial.chain(rest)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_backoff_honors_headers_retry_count_and_total_deadline() {
        let limited = Response::builder()
            .status(429)
            .header(header::RETRY_AFTER, "3")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            continuation_retry_delay(&limited, 0, Duration::ZERO),
            Some(Duration::from_secs(3))
        );
        assert!(continuation_retry_delay(&limited, 2, Duration::ZERO).is_none());
        assert!(continuation_retry_delay(&limited, 0, Duration::from_secs(117)).is_none());
        let missing = Response::builder().status(429).body(Body::empty()).unwrap();
        assert_eq!(
            continuation_retry_delay(&missing, 0, Duration::ZERO),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            continuation_retry_delay(&missing, 1, Duration::ZERO),
            Some(Duration::from_secs(10))
        );
        let future = (chrono::Utc::now() + chrono::Duration::seconds(10))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let dated = Response::builder()
            .status(429)
            .header(header::RETRY_AFTER, future)
            .body(Body::empty())
            .unwrap();
        assert!(
            (8..=10).contains(
                &continuation_retry_delay(&dated, 0, Duration::ZERO)
                    .unwrap()
                    .as_secs()
            )
        );
        for status in [400, 401, 403, 500, 503] {
            assert!(
                continuation_retry_delay(
                    &Response::builder()
                        .status(status)
                        .body(Body::empty())
                        .unwrap(),
                    0,
                    Duration::ZERO
                )
                .is_none()
            );
        }
        for header in ["invalid", "120", "18446744073709551615"] {
            let response = Response::builder()
                .status(429)
                .header(header::RETRY_AFTER, header)
                .body(Body::empty())
                .unwrap();
            assert!(continuation_retry_delay(&response, 0, Duration::ZERO).is_none());
        }
    }
    fn request() -> MessagesRequest {
        serde_json::from_value(json!({"model":"claude-opus-5-5","messages":[{"role":"user","content":"Run a calculation"}],"tools":[{"type":"code_execution_20250825","name":"code_execution"}]})).unwrap()
    }
    #[test]
    fn preparation_is_opt_in_and_validates_versions() {
        assert!(prepare(&mut request(), CodeExecutionMode::Reject).is_err());
        let mut req = request();
        req.tools.as_mut().unwrap()[0].tool_type = Some("code_execution_20990101".into());
        assert!(prepare(&mut req, CodeExecutionMode::Bridge).is_err());
        let mut req = request();
        assert!(
            prepare(&mut req, CodeExecutionMode::Bridge)
                .unwrap()
                .is_some()
        );
        assert_eq!(req.tools.unwrap().len(), 2);
    }
    #[test]
    fn disabled_tool_choice_does_not_expose_internal_execution_tools() {
        let mut req = request();
        req.tool_choice = Some(json!({"type":"none"}));
        assert!(
            prepare(&mut req, CodeExecutionMode::Bridge)
                .unwrap()
                .is_none()
        );
        assert!(req.tools.is_none());
    }
    #[test]
    fn unavailable_is_not_a_fabricated_process_result() {
        let (call, result) = result_for(
            &json!({"type":"tool_use","id":"internal","name":BASH,"input":{"command":"example"}}),
        )
        .unwrap();
        assert_eq!(call["id"], result["tool_use_id"]);
        assert_eq!(result["content"]["error_code"], "unavailable");
        assert!(result["content"].get("return_code").is_none());
        assert!(result_for(&json!({"name":"Bash"})).is_none());
    }
    #[test]
    fn replay_preserves_success_and_error_semantics_and_pairing() {
        for (content, is_error) in [
            (
                json!({"type":"bash_code_execution_result","return_code":0,"stdout":"35","stderr":""}),
                false,
            ),
            (
                json!({"type":"bash_code_execution_result","return_code":2,"stdout":"","stderr":"failed"}),
                true,
            ),
            (
                json!({"type":"bash_code_execution_tool_result_error","error_code":"unavailable"}),
                true,
            ),
        ] {
            let mut req = request();
            req.messages = vec![
                Message {
                    role: "assistant".into(),
                    content: json!([
                {"type":"server_tool_use","id":"srvtoolu_previous","name":"bash_code_execution","input":{"command":"example"}},
                {"type":"bash_code_execution_tool_result","tool_use_id":"srvtoolu_previous","content":content},
                {"type":"text","text":"Previous result"}]),
                },
                Message {
                    role: "user".into(),
                    content: json!("Continue"),
                },
            ];
            prepare(&mut req, CodeExecutionMode::Bridge).unwrap();
            assert_eq!(req.messages[0].content[0]["type"], "tool_use");
            assert_eq!(
                req.messages[1].content[0]["tool_use_id"],
                "srvtoolu_previous"
            );
            assert_eq!(req.messages[1].content[0]["is_error"], is_error);
            assert!(
                super::super::converter::convert_request_with_mode(
                    &req,
                    crate::model::config::ToolCompatibilityMode::Raw
                )
                .is_ok()
            );
        }
    }
}
