use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    pin::Pin,
};

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::kiro::parser::crc::crc32;

const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 4096;
type Source = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Reuse the established parser and all its consumers. The conversion is lazy,
/// backpressured and owned by the response body, with no detached producer task.
pub(crate) async fn normalize_response(
    response: reqwest::Response,
) -> anyhow::Result<reqwest::Response> {
    if !response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/event-stream"))
        })
    {
        anyhow::bail!("CreateResponse returned a non-SSE successful response");
    }
    let status = response.status();
    let mut headers = response.headers().clone();
    let mut decoder = Decoder::new(Box::pin(response.bytes_stream()));
    // Do not mark an error-only HTTP 200 stream as a successful provider call.
    // Read until the first semantic event, never buffer the entire answer.
    let first = decoder.next_frame().await.transpose()?;
    if let Some((status, error)) = decoder.early_failure.take() {
        let mut http = http::Response::builder().status(status);
        *http.headers_mut().expect("response headers") = headers;
        let http_headers = http.headers_mut().unwrap();
        http_headers.remove("content-length");
        http_headers.remove("content-encoding");
        http_headers.insert(
            "content-type",
            http::HeaderValue::from_static("application/json"),
        );
        return Ok(http.body(error.to_string())?.into());
    }
    let stream = futures::stream::unfold((first, decoder), |(first, mut decoder)| async move {
        let item = if let Some(bytes) = first {
            Some(Ok(bytes))
        } else {
            decoder.next_frame().await
        };
        item.map(|item| (item, (None, decoder)))
    });
    headers.remove("content-length");
    headers.remove("content-encoding");
    headers.insert(
        "content-type",
        http::HeaderValue::from_static("application/vnd.amazon.eventstream"),
    );
    let mut http = http::Response::builder().status(status);
    *http.headers_mut().expect("response headers") = headers;
    Ok(http.body(reqwest::Body::wrap_stream(stream))?.into())
}

pub(crate) fn normalize_error(status: u16, body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    let error = value.get("error").unwrap_or(&value);
    let code = error
        .get("code")
        .or_else(|| error.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let reason = if status == 413 {
        "CONTENT_LENGTH_EXCEEDS_THRESHOLD".to_string()
    } else {
        reason(code)
    };
    let message = error
        .get("message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("CreateResponse request failed");
    let mut result = json!({"message":message});
    if !reason.is_empty() {
        result["reason"] = json!(reason);
    }
    if let Some(delay) = value.get("retryAfterMilliseconds") {
        result["retryAfterMilliseconds"] = delay.clone();
    }
    result.to_string()
}

fn reason(code: &str) -> String {
    match code {
        "context_length_exceeded" => "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
        "model_not_found" => "INVALID_MODEL_ID",
        "invalid_function_parameters" => "TOOL_SCHEMA_INVALID",
        "invalid_json" => "REQUEST_BODY_INVALID_JSON",
        "rate_limit_exceeded" => "USER_REQUEST_RATE_EXCEEDED",
        "server_is_overloaded" => "MODEL_TEMPORARILY_UNAVAILABLE",
        other => {
            return other
                .strip_prefix("kiro:")
                .map(str::to_ascii_uppercase)
                .unwrap_or_else(|| other.to_owned());
        }
    }
    .to_owned()
}

/// Emit a valid AWS event frame, including both CRCs, so every consumer sees
/// exactly the same typed events as on the legacy protocol.
fn encode_event(kind: &str, value: Value) -> Bytes {
    encode_frame("event", ":event-type", kind, value)
}

fn encode_frame(message_type: &str, kind_header: &str, kind: &str, value: Value) -> Bytes {
    let mut headers = Vec::new();
    for (key, value) in [
        (":message-type", message_type),
        (kind_header, kind),
        (":content-type", "application/json"),
    ] {
        headers.push(key.len() as u8);
        headers.extend_from_slice(key.as_bytes());
        headers.push(7); // AWS UTF-8 string header
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = value.to_string();
    let total = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(payload.as_bytes());
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    Bytes::from(frame)
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
    closed: bool,
}

struct Decoder {
    source: Source,
    bytes: BytesMut,
    scan_offset: usize,
    data: Vec<String>,
    event_bytes: usize,
    pending: VecDeque<Bytes>,
    calls: HashMap<String, Call>,
    aliases: HashMap<String, String>,
    text_seen: HashSet<String>,
    reasoning_seen: HashSet<String>,
    refusal_seen: HashSet<String>,
    terminal: bool,
    stopped: bool,
    semantic: bool,
    early_failure: Option<(u16, Value)>,
}

impl Decoder {
    fn new(source: Source) -> Self {
        Self {
            source,
            bytes: BytesMut::new(),
            scan_offset: 0,
            data: Vec::new(),
            event_bytes: 0,
            pending: VecDeque::new(),
            calls: HashMap::new(),
            aliases: HashMap::new(),
            text_seen: HashSet::new(),
            reasoning_seen: HashSet::new(),
            refusal_seen: HashSet::new(),
            terminal: false,
            stopped: false,
            semantic: false,
            early_failure: None,
        }
    }

    async fn next_frame(&mut self) -> Option<Result<Bytes, io::Error>> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                self.semantic = true;
                return Some(Ok(frame));
            }
            if self.stopped || self.terminal {
                return None;
            }
            // CR, LF and CRLF are all legal SSE delimiters; leave a trailing CR
            // pending until the next read to avoid treating CRLF as two lines.
            if let Some(index) = self.bytes[self.scan_offset..]
                .iter()
                .position(|b| *b == b'\r' || *b == b'\n')
                .map(|i| i + self.scan_offset)
            {
                if self.bytes[index] == b'\r' && index + 1 == self.bytes.len() {
                    // Need the next byte before choosing a delimiter width.
                    self.scan_offset = index;
                } else {
                    let width = if self.bytes[index] == b'\r'
                        && self.bytes.get(index + 1) == Some(&b'\n')
                    {
                        2
                    } else {
                        1
                    };
                    let line = self.bytes.split_to(index + width);
                    self.scan_offset = 0;
                    if let Err(e) = self.line(&line[..index]) {
                        self.stopped = true;
                        return Some(Err(e));
                    }
                    continue;
                }
            } else {
                self.scan_offset = self.bytes.len();
            }
            if self.bytes.len().saturating_add(self.event_bytes) > MAX_EVENT_BYTES {
                self.stopped = true;
                return Some(Err(invalid("CreateResponse SSE event exceeds 4 MiB")));
            }
            match self.source.next().await {
                Some(Ok(chunk)) => self.bytes.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    self.stopped = true;
                    return Some(Err(io::Error::other(error)));
                }
                None => {
                    self.stopped = true;
                    // A completion without a trailing blank line is still a
                    // complete JSON event. A partial JSON or absent terminal is not.
                    if !self.bytes.is_empty() {
                        let line = self.bytes.split().freeze();
                        let line = line.strip_suffix(b"\r").unwrap_or(&line);
                        if let Err(e) = self.line(line) {
                            return Some(Err(e));
                        }
                    }
                    if let Err(e) = self.dispatch() {
                        return Some(Err(e));
                    }
                    if !self.terminal {
                        return Some(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "CreateResponse ended before a terminal event",
                        )));
                    }
                    return self.pending.pop_front().map(Ok);
                }
            }
        }
    }

    fn line(&mut self, line: &[u8]) -> io::Result<()> {
        if line.len().saturating_add(self.event_bytes) > MAX_EVENT_BYTES {
            return Err(invalid("CreateResponse SSE event exceeds 4 MiB"));
        }
        if line.is_empty() {
            return self.dispatch();
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| invalid("Invalid UTF-8 in CreateResponse SSE"))?;
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            self.event_bytes += value.len() + 1;
            self.data.push(value.to_owned());
        }
        Ok(())
    }

    fn dispatch(&mut self) -> io::Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data).join("\n");
        self.event_bytes = 0;
        if data.trim() == "[DONE]" {
            if !self.terminal {
                return Err(invalid("CreateResponse sent DONE before completion"));
            }
            return Ok(());
        }
        let event: Value = serde_json::from_str(&data)
            .map_err(|_| invalid("Malformed CreateResponse SSE JSON"))?;
        self.event(event)
    }

    fn emit(&mut self, kind: &str, payload: Value) {
        self.pending.push_back(encode_event(kind, payload));
    }

    fn emit_usage(&mut self, usage: &Value, stop: Option<&str>) {
        let mut metadata = json!({});
        if let Some(stop) = stop {
            metadata["stopReason"] = json!(stop);
        }
        if let Some(tokens) = token_usage(usage) {
            metadata["tokenUsage"] = tokens;
        }
        if metadata.as_object().is_some_and(|m| !m.is_empty()) {
            self.emit("metadataEvent", metadata);
        }
        if let Some(pct) = usage
            .get("kiro:context_usage_percentage")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
        {
            self.emit("contextUsageEvent", json!({"contextUsagePercentage":pct}));
        }
        if let Some(value) = usage
            .pointer("/kiro:credits/value")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
        {
            self.emit("meteringEvent", json!({"usage":value,"unit":usage.pointer("/kiro:credits/unit").and_then(Value::as_str).unwrap_or("credit"),"unitPlural":"credits"}));
        }
    }

    fn item_key(&self, event: &Value) -> String {
        if let Some(id) = event
            .get("item_id")
            .and_then(Value::as_str)
            .or_else(|| event.pointer("/item/id").and_then(Value::as_str))
        {
            if let Some(key) = self.aliases.get(id) {
                return key.clone();
            }
            return id.to_owned();
        }
        event
            .get("output_index")
            .map(Value::to_string)
            .unwrap_or_else(|| "0".into())
    }

    fn call_start(&mut self, event: &Value) -> io::Result<String> {
        let item = &event["item"];
        let index = event.get("output_index").map(Value::to_string);
        let key = index.clone().unwrap_or_else(|| self.item_key(event));
        if !self.calls.contains_key(&key) {
            if self.calls.len() >= MAX_OUTPUT_ITEMS {
                return Err(invalid("Too many CreateResponse tool calls"));
            }
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("Missing CreateResponse tool call ID"))?
                .to_owned();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("Missing CreateResponse tool name"))?
                .to_owned();
            self.emit(
                "toolUseEvent",
                json!({"toolUseId":id,"name":name,"input":"","stop":false}),
            );
            self.calls.insert(
                key.clone(),
                Call {
                    id,
                    name,
                    ..Default::default()
                },
            );
        }
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            self.aliases.insert(id.to_owned(), key.clone());
        }
        if let Some(id) = item.get("call_id").and_then(Value::as_str) {
            self.aliases.insert(id.to_owned(), key.clone());
        }
        Ok(key)
    }

    fn call_delta(&mut self, key: &str, delta: &str) -> io::Result<()> {
        let call = self
            .calls
            .get_mut(key)
            .ok_or_else(|| invalid("Arguments for unknown CreateResponse tool call"))?;
        if call.closed {
            return Err(invalid(
                "Arguments after completed CreateResponse tool call",
            ));
        }
        if call.arguments.len().saturating_add(delta.len()) > MAX_EVENT_BYTES {
            return Err(invalid("CreateResponse tool arguments exceed 4 MiB"));
        }
        call.arguments.push_str(delta);
        let payload = json!({"toolUseId":call.id,"name":call.name,"input":delta,"stop":false});
        self.emit("toolUseEvent", payload);
        Ok(())
    }

    fn call_done(&mut self, key: &str, final_args: Option<&str>) -> io::Result<()> {
        let call = self
            .calls
            .get(key)
            .ok_or_else(|| invalid("Completion for unknown CreateResponse tool call"))?;
        if call.closed {
            return Ok(());
        }
        if let Some(args) = final_args {
            let suffix = args
                .strip_prefix(&call.arguments)
                .ok_or_else(|| invalid("CreateResponse final tool arguments disagree with deltas"))?
                .to_owned();
            if !suffix.is_empty() {
                self.call_delta(key, &suffix)?;
            }
        }
        let call = self.calls.get_mut(key).unwrap();
        call.closed = true;
        let payload = json!({"toolUseId":call.id,"name":call.name,"input":"","stop":true});
        self.emit("toolUseEvent", payload);
        Ok(())
    }

    fn event(&mut self, event: Value) -> io::Result<()> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Missing CreateResponse event type"))?;
        let key = self.item_key(&event);
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Invalid CreateResponse text delta"))?;
                if kind == "response.refusal.delta" {
                    self.refusal_seen.insert(key);
                } else {
                    self.text_seen.insert(key);
                }
                self.emit("assistantResponseEvent", json!({"content":delta}));
            }
            "response.output_text.done" | "response.refusal.done" => {
                let seen = if kind == "response.refusal.done" {
                    self.refusal_seen.contains(&key)
                } else {
                    self.text_seen.contains(&key)
                };
                if !seen {
                    if let Some(text) = event
                        .get(if kind == "response.refusal.done" {
                            "refusal"
                        } else {
                            "text"
                        })
                        .and_then(Value::as_str)
                    {
                        self.emit("assistantResponseEvent", json!({"content":text}));
                    }
                }
            }
            "response.reasoning_text.delta"
            | "response.reasoning.delta"
            | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.reasoning_seen.insert(key);
                    self.emit("reasoningContentEvent", json!({"text":delta}));
                }
            }
            "response.output_item.added"
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
            {
                let key = self.call_start(&event)?;
                if let Some(args) = event
                    .pointer("/item/arguments")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                {
                    self.call_delta(&key, args)?;
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Missing CreateResponse arguments delta"))?;
                self.call_delta(&key, delta)?;
            }
            "response.function_call_arguments.done" => {
                // Some runtimes omit arguments on this event and provide the
                // complete JSON only on output_item.done. Closing now would
                // discard that JSON and turn a valid call into an empty one.
                if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                    self.call_done(&key, Some(arguments))?;
                }
            }
            "response.output_item.done" => {
                match event.pointer("/item/type").and_then(Value::as_str) {
                    Some("function_call") => {
                        let key = self.call_start(&event)?;
                        self.call_done(
                            &key,
                            event.pointer("/item/arguments").and_then(Value::as_str),
                        )?;
                    }
                    Some("reasoning") => {
                        if let Some(encrypted) = event
                            .pointer("/item/encrypted_content")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            let envelope = serde_json::from_str::<Value>(encrypted).ok();
                            if let Some(reasoning) =
                                envelope.as_ref().and_then(|v| v.get("reasoningText"))
                            {
                                if !self.reasoning_seen.contains(&key) {
                                    if let Some(text) = reasoning
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.is_empty())
                                    {
                                        self.emit("reasoningContentEvent", json!({"text":text}));
                                    }
                                }
                                if let Some(sig) = reasoning
                                    .get("signature")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                {
                                    self.emit("reasoningContentEvent", json!({"signature":sig}));
                                }
                            } else {
                                self.emit(
                                    "reasoningContentEvent",
                                    json!({"redactedContent":encrypted}),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                if self.calls.values().any(|c| !c.closed) {
                    return Err(invalid(
                        "CreateResponse completed with unfinished tool calls",
                    ));
                }
                let response = &event["response"];
                let stop = if kind == "response.incomplete"
                    || response.get("status").and_then(Value::as_str) == Some("incomplete")
                {
                    match response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                    {
                        Some("max_output_tokens") => "MAX_TOKENS",
                        Some("content_filter") => "CONTENT_FILTERED",
                        Some("compaction_required_for_model_handoff") => {
                            "MODEL_CONTEXT_WINDOW_EXCEEDED"
                        }
                        _ => {
                            return Err(invalid(
                                "CreateResponse stopped with an unknown incomplete reason",
                            ));
                        }
                    }
                } else {
                    "END_TURN"
                };
                self.emit_usage(&response["usage"], Some(stop));
                self.terminal = true;
            }
            "error" | "response.failed" => {
                let error = if kind == "error" {
                    event.get("error").unwrap_or(&event)
                } else {
                    &event["response"]["error"]
                };
                let code = error.get("code").and_then(Value::as_str).unwrap_or("");
                let err_type = error.get("type").and_then(Value::as_str).unwrap_or("");
                let status = match (code, err_type) {
                    ("rate_limit_exceeded", _) | (_, "too_many_requests") => 429,
                    (_, "insufficient_quota") => 402,
                    ("context_length_exceeded", _) | (_, "invalid_request") => 400,
                    _ => 502,
                };
                let error = normalize_error(status, &json!({"error":error}).to_string());
                let normalized: Value = serde_json::from_str(&error).expect("normalized JSON");
                // Failed responses can still contain billable partial work.
                // Preserve reported usage before the exception; do not retry
                // that physical response as though it had consumed nothing.
                self.emit_usage(&event["response"]["usage"], None);
                if !self.semantic && self.pending.is_empty() {
                    self.early_failure = Some((status, normalized.clone()));
                }
                self.pending.push_back(encode_frame(
                    "exception",
                    ":exception-type",
                    if status == 429 {
                        "ThrottlingException"
                    } else {
                        "CreateResponseException"
                    },
                    normalized,
                ));
                self.terminal = true;
            }
            _ => {} // created/progress/content-part and future optional events
        }
        if self.text_seen.len() + self.reasoning_seen.len() + self.refusal_seen.len()
            > MAX_OUTPUT_ITEMS
        {
            return Err(invalid("Too many CreateResponse output items"));
        }
        Ok(())
    }
}

fn token_usage(usage: &Value) -> Option<Value> {
    // A full provider snapshot is authoritative; do not invent missing counts.
    if let Some(native) = usage
        .get("kiro:token_usage")
        .or_else(|| usage.get("tokenUsage"))
    {
        if native.get("uncachedInputTokens").is_some() && native.get("outputTokens").is_some() {
            return Some(native.clone());
        }
    }
    let input = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    let read = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input);
    let write = usage
        .pointer("/input_tokens_details/cache_creation_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input - read);
    let count = |v: u64| v.min(i32::MAX as u64);
    Some(
        json!({"uncachedInputTokens":count(input-read-write),"outputTokens":count(output),
        "cacheReadInputTokens":count(read),"cacheWriteInputTokens":count(write)}),
    )
}

#[cfg(test)]
mod tests;
