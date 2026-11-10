use super::*;
use crate::kiro::{model::events::Event, parser::decoder::EventStreamDecoder};
use futures::stream;

fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn completed() -> Value {
    json!({"type":"response.completed","response":{"status":"completed","usage":{
        "input_tokens":100,"output_tokens":12,"input_tokens_details":{"cached_tokens":60,"cache_creation_tokens":10},
        "kiro:credits":{"value":0.25,"unit":"credit"},"kiro:context_usage_percentage":1.5}}})
}

async fn decode(raw: Vec<u8>, size: usize) -> Result<Vec<Event>, io::Error> {
    let chunks = raw
        .chunks(size)
        .map(Bytes::copy_from_slice)
        .map(Ok)
        .collect::<Vec<_>>();
    let mut decoder = Decoder::new(Box::pin(stream::iter(chunks)));
    let mut frames = EventStreamDecoder::new();
    let mut events = Vec::new();
    while let Some(frame) = decoder.next_frame().await {
        frames.feed(&frame?).unwrap();
        for frame in frames.decode_iter() {
            events.push(Event::from_frame(frame.unwrap()).unwrap());
        }
    }
    Ok(events)
}

#[tokio::test]
async fn arbitrary_utf8_boundaries_preserve_text_usage_and_exact_credits_once() {
    let raw = sse(&[
        json!({"type":"response.output_text.delta","item_id":"m","delta":"中文🙂"}),
        completed(),
    ]);
    for size in [1, 2, 7, 4096] {
        let events = decode(raw.clone(), size).await.unwrap();
        assert!(matches!(&events[0],Event::AssistantResponse(v) if v.content=="中文🙂"));
        let usage = events
            .iter()
            .find_map(|e| {
                if let Event::Metadata(m) = e {
                    m.token_usage
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(usage.uncached_input_tokens, 30);
        assert_eq!(usage.cache_read_input_tokens, 60);
        assert_eq!(usage.cache_write_input_tokens, 10);
        assert_eq!(usage.output_tokens, 12);
        let credits = events
            .iter()
            .filter_map(|e| {
                if let Event::Metering(m) = e {
                    m.credits()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(credits, vec![0.25]);
    }
}

#[tokio::test]
async fn interleaved_calls_keep_their_own_arguments_and_only_finish_once() {
    let raw = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"i0","call_id":"c0","name":"Read","arguments":""}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"i1","call_id":"c1","name":"Write","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"i0","delta":"{\"path\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"i0","delta":"\"a\"}"}),
        json!({"type":"response.function_call_arguments.done","output_index":1,"arguments":"{}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"i1","call_id":"c1","name":"Write","arguments":"{}"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"i0","call_id":"c0","name":"Read","arguments":"{\"path\":\"a\"}"}}),
        completed(),
    ]);
    let events = decode(raw, 3).await.unwrap();
    let mut inputs = HashMap::<String, String>::new();
    let mut stops = HashMap::<String, usize>::new();
    for event in events {
        if let Event::ToolUse(t) = event {
            inputs
                .entry(t.tool_use_id.clone())
                .or_default()
                .push_str(&t.input);
            if t.stop {
                *stops.entry(t.tool_use_id).or_default() += 1;
            }
        }
    }
    assert_eq!(inputs["c0"], "{\"path\":\"a\"}");
    assert_eq!(inputs["c1"], "{}");
    assert_eq!(stops["c0"], 1);
    assert_eq!(stops["c1"], 1);
}

#[tokio::test]
async fn final_only_arguments_are_not_dropped_or_duplicated() {
    let raw = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"i","call_id":"c","name":"Read","arguments":""}}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"a\"}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"i","call_id":"c","name":"Read","arguments":"{\"path\":\"a\"}"}}),
        completed(),
    ]);
    let events = decode(raw, 1).await.unwrap();
    let args = events
        .iter()
        .filter_map(|e| {
            if let Event::ToolUse(t) = e {
                Some(t.input.as_str())
            } else {
                None
            }
        })
        .collect::<String>();
    assert_eq!(args, "{\"path\":\"a\"}");
}

#[tokio::test]
async fn argument_done_without_json_waits_for_final_item() {
    let raw = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"i","type":"function_call","call_id":"c","name":"Read","arguments":""}}),
        json!({"type":"response.function_call_arguments.done","output_index":0}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"i","type":"function_call","call_id":"c","name":"Read","arguments":"{}"}}),
        completed(),
    ]);
    let events = decode(raw, 3).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter_map(|e| if let Event::ToolUse(t) = e {
                Some(t.input.as_str())
            } else {
                None
            })
            .collect::<String>(),
        "{}"
    );
}

#[tokio::test]
async fn failed_response_keeps_consumed_credits_before_the_exception() {
    let raw = sse(&[
        json!({"type":"response.output_text.delta","delta":"partial"}),
        json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"slow"},
        "usage":{"kiro:credits":{"value":0.125,"unit":"credit"}}}}),
    ]);
    let events = decode(raw, 5).await.unwrap();
    assert!(matches!(events.get(1),Some(Event::Metering(m)) if m.credits()==Some(0.125)));
    assert!(matches!(events.last(), Some(Event::Exception { .. })));
}

#[tokio::test]
async fn oversized_sse_events_fail_before_json_parsing() {
    let mut raw = b"data: ".to_vec();
    raw.resize(MAX_EVENT_BYTES + 32, b'x');
    raw.extend_from_slice(b"\n\n");
    assert!(decode(raw, 1024).await.is_err());
}

#[test]
fn absent_or_invalid_usage_does_not_override_local_metering() {
    for usage in [
        json!({}),
        json!({"input_tokens":-1,"output_tokens":2}),
        json!({"input_tokens":20}),
        json!({"kiro:credits":{"value":0.1}}),
    ] {
        assert!(token_usage(&usage).is_none());
    }
}

#[tokio::test]
async fn reasoning_signature_and_redaction_are_preserved() {
    let raw = sse(&[
        json!({"type":"response.reasoning_text.delta","item_id":"r0","delta":"thinking"}),
        json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"r0","encrypted_content":json!({"reasoningText":{"text":"thinking","signature":"sig"}}).to_string()}}),
        json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"r1","encrypted_content":"opaque-base64"}}),
        completed(),
    ]);
    let events = decode(raw, 11).await.unwrap();
    let thinking = events
        .iter()
        .filter_map(|e| {
            if let Event::ReasoningContent(t) = e {
                t.text.as_deref()
            } else {
                None
            }
        })
        .collect::<String>();
    assert_eq!(thinking, "thinking");
    assert_eq!(
        events
            .iter()
            .filter(
                |e| matches!(e,Event::ReasoningContent(r) if r.signature.as_deref()==Some("sig"))
            )
            .count(),
        1
    );
    assert!(events.iter().any(|e|matches!(e,Event::ReasoningContent(r) if r.redacted_content.as_deref()==Some("opaque-base64"))));
}

#[tokio::test]
async fn sse_crlf_multiline_and_no_trailing_delimiter() {
    let raw=format!("\u{feff}: ping\r\ndata: {{\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"OK\"}}\r\n\r\ndata: {}",completed()).into_bytes();
    let events = decode(raw, 1).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e,Event::AssistantResponse(t) if t.content=="OK"))
    );
}

#[tokio::test]
async fn truncation_malformed_json_and_missing_completion_fail_closed() {
    for raw in [
        b"data: {bad}\n\n".to_vec(),
        b"data: [DONE]\n\n".to_vec(),
        sse(&[json!({"type":"response.output_text.delta","delta":"partial"})]),
    ] {
        assert!(decode(raw, 1).await.is_err());
    }
    let raw = sse(&[
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}),
    ]);
    let events = decode(raw, 20).await.unwrap();
    assert!(
        events.iter().any(
            |e| matches!(e,Event::Metadata(m) if m.truncation_stop_reason()==Some("max_tokens"))
        )
    );
}

#[tokio::test]
async fn error_only_200_becomes_429_but_partial_output_keeps_an_exception() {
    let failure = json!({"type":"response.failed","response":{"error":{"type":"too_many_requests","code":"rate_limit_exceeded","message":"slow down"}}});
    let raw = sse(&[failure.clone()]);
    let response: reqwest::Response = http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(raw)
        .unwrap()
        .into();
    let response = normalize_response(response).await.unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(
        response.json::<Value>().await.unwrap()["reason"],
        "USER_REQUEST_RATE_EXCEEDED"
    );
    let events = decode(
        sse(&[
            json!({"type":"response.output_text.delta","delta":"partial"}),
            failure,
        ]),
        1,
    )
    .await
    .unwrap();
    assert!(
        matches!(events.last(),Some(Event::Exception{exception_type,..}) if exception_type=="ThrottlingException")
    );
}

#[tokio::test]
async fn duplicate_terminal_usage_is_ignored_and_stream_is_lazy() {
    let chunks = vec![
        Ok(Bytes::from(sse(&[
            json!({"type":"response.output_text.delta","delta":"first"}),
        ]))),
        Ok(Bytes::from(sse(&[completed(), completed()]))),
    ];
    let mut decoder = Decoder::new(Box::pin(stream::iter(chunks)));
    assert!(decoder.next_frame().await.unwrap().is_ok());
    assert!(!decoder.terminal);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().await {
        frames.push(frame.unwrap());
    }
    let mut events = Vec::new();
    for frame in frames {
        let mut parser = EventStreamDecoder::new();
        parser.feed(&frame).unwrap();
        let frame = parser.decode_iter().next().unwrap().unwrap();
        events.push(Event::from_frame(frame).unwrap());
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Metering(_)))
            .count(),
        1
    );
}

#[test]
fn normalizes_error_codes_without_turning_model_capacity_into_account_limit() {
    for (status, code, expected) in [
        (429, "rate_limit_exceeded", "USER_REQUEST_RATE_EXCEEDED"),
        (
            400,
            "context_length_exceeded",
            "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
        ),
        (429, "kiro:monthly_request_count", "MONTHLY_REQUEST_COUNT"),
        (503, "server_is_overloaded", "MODEL_TEMPORARILY_UNAVAILABLE"),
        (403, "kiro:temporarily_suspended", "TEMPORARILY_SUSPENDED"),
    ] {
        let error: Value = serde_json::from_str(&normalize_error(
            status,
            &json!({"error":{"code":code,"message":"sample"}}).to_string(),
        ))
        .unwrap();
        assert_eq!(error["reason"], expected);
    }
}
