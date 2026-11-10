//! Opt-in smoke test. Authentication arrives over stdin, stays in memory and
//! never appears in diagnostics. Normal test runs make no public network calls.
use super::*;
use crate::{
    admin::trace_db::{TraceAttempt, TraceSink},
    kiro::{
        endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint},
        model::{credentials::KiroCredentials, events::Event},
        parser::decoder::EventStreamDecoder,
        provider::KiroProvider,
        token_manager::MultiTokenManager,
    },
    model::config::Config,
};
use serde_json::json;
use std::collections::HashSet;

struct Sink(Mutex<Vec<TraceAttempt>>);
impl TraceSink for Sink {
    fn on_attempt(&self, attempt: TraceAttempt) {
        self.0.lock().push(attempt);
    }
}

#[tokio::test]
#[ignore = "Explicit live check only: one authorized credential/configuration supplied over stdin"]
async fn fixed_account_create_response_live() {
    let input = std::io::read_to_string(std::io::stdin()).expect("JSON stdin required");
    let data: Value = serde_json::from_str(&input).expect("valid diagnostic envelope");
    let credentials: KiroCredentials =
        serde_json::from_value(data["credential"].clone()).expect("credential JSON");
    assert!(
        !credentials.disabled,
        "diagnostic refuses a disabled account"
    );
    let mut cfg: Config =
        serde_json::from_value(data["config"].clone()).expect("configuration JSON");
    cfg.upstream_read_timeout_secs = 30;
    cfg.upstream_total_timeout_secs = 60;
    let proxy = cfg
        .proxy_url
        .clone()
        .map(|url| crate::http_client::ProxyConfig {
            url,
            username: cfg.proxy_username.clone(),
            password: cfg.proxy_password.clone(),
        });
    let id = credentials.id.unwrap();
    let manager = Arc::new(
        MultiTokenManager::new(cfg, vec![credentials], proxy, None, false)
            .expect("in-memory account manager"),
    );
    let endpoints = HashMap::from([
        (
            "ide".into(),
            Arc::new(IdeEndpoint::new()) as Arc<dyn KiroEndpoint>,
        ),
        (
            "cli".into(),
            Arc::new(CliEndpoint::new()) as Arc<dyn KiroEndpoint>,
        ),
    ]);
    let provider = KiroProvider::new(manager.clone(), endpoints, "ide".into());
    let model = data["model"].as_str().unwrap_or("claude-sonnet-5.5");
    let tools = data["tools"].as_bool().unwrap_or(false);
    let mut request = json!({"conversationState":{"conversationId":uuid::Uuid::new_v4().to_string(),"agentTaskType":"vibe","chatTriggerType":"MANUAL",
        "currentMessage":{"userInputMessage":{"content":if tools {"Call audit_echo with text HELLO_CHECK. Do not answer directly."}else{"Reply with exactly OK."},"modelId":model,"origin":"AI_EDITOR"}}}});
    if tools {
        request["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"] = json!({"tools":[{"toolSpecification":{
        "name":"audit_echo","description":"Return the supplied text","inputSchema":{"json":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}}}]});
    }
    let sink = Sink(Mutex::new(Vec::new()));
    let start = Instant::now();
    let call = provider
        .call_api_stream(&request.to_string(), Some(&sink), None)
        .await;
    let mut report = json!({"id":id,"model":model,"tools_requested":tools,"milliseconds":start.elapsed().as_millis(),
        "attempts":sink.0.lock().iter().map(|a|json!({"credential_id":a.credential_id,"endpoint":a.endpoint,"http":a.http_status,"outcome":a.outcome})).collect::<Vec<_>>()});
    let Ok(call) = call else {
        report["completed"] = json!(false);
        println!("LIVE_CREATE_RESPONSE_RESULT {}", report);
        return;
    };
    assert_eq!(call.credential_id, id);
    let _flight = call._in_flight;
    let mut stream = call.response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();
    let mut text = String::new();
    let mut credits = 0.0;
    let mut call_ids = HashSet::new();
    let mut stops = HashSet::new();
    let mut args = HashMap::<String, String>::new();
    let mut errors = 0;
    let mut reasoning_chars = 0;
    let mut signature = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            errors += 1;
            break;
        };
        if decoder.feed(&chunk).is_err() {
            errors += 1;
            break;
        }
        for frame in decoder.decode_iter() {
            let Ok(event) = frame.and_then(Event::from_frame) else {
                errors += 1;
                continue;
            };
            match event {
                Event::AssistantResponse(t) => text.push_str(&t.content),
                Event::Metering(m) => credits += m.credits().unwrap_or(0.0),
                Event::ReasoningContent(r) => {
                    reasoning_chars += r.text.as_deref().unwrap_or("").len();
                    signature |= r.signature.is_some();
                }
                Event::ToolUse(t) => {
                    call_ids.insert(t.tool_use_id.clone());
                    args.entry(t.tool_use_id.clone())
                        .or_default()
                        .push_str(&t.input);
                    if t.stop {
                        stops.insert(t.tool_use_id);
                    }
                }
                Event::Error { .. } | Event::Exception { .. } => errors += 1,
                _ => {}
            }
        }
    }
    report["completed"] = json!(errors == 0 && !decoder.has_pending_data());
    report["has_text"] = json!(!text.trim().is_empty());
    report["reply_is_ok"] = json!(text.trim() == "OK");
    report["credits"] = json!(credits);
    report["tool_calls"] = json!(call_ids.len());
    report["tool_stops"] = json!(stops.len());
    report["valid_tool_json"] = json!(
        args.values()
            .all(|v| serde_json::from_str::<Value>(v).is_ok())
    );
    report["reasoning_chars"] = json!(reasoning_chars);
    report["has_signature"] = json!(signature);
    println!("LIVE_CREATE_RESPONSE_RESULT {}", report);
    assert_eq!(errors, 0, "streaming adapter errors");
}
