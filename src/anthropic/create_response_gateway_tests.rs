//! Full gateway coverage with a synthetic per-account feature control plane.
use super::*;
use axum::routing::get;
use sha2::{Digest, Sha256};

async fn modern_fixture(
    tools: bool,
) -> (
    AppState,
    Arc<Mutex<Vec<serde_json::Value>>>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = bodies.clone();
    let requests = calls.clone();
    let feature_key = hex::encode(Sha256::digest(
        b"kiro-feature-config-key-salt-3e9b1d7acreate_response_api",
    ));
    let app=Router::new().route("/featureConfiguration",get(move || {
        let key=feature_key.clone();async move{Json(json!({"configuration":{key:"create_response"}}))}
    })).route("/v1/responses",post(move |headers:HeaderMap,Json(body):Json<serde_json::Value>| {
        let bodies=seen.clone();let requests=requests.clone();async move {
            assert_eq!(headers["tokentype"],"SSO_OIDC");
            assert_eq!(headers["x-amzn-kiro-profile"],"fixture-profile");
            assert!(!headers.contains_key("x-amz-target"));
            requests.fetch_add(1,Ordering::SeqCst);bodies.lock().push(body);
            let mut events=vec![json!({"type":"response.reasoning_text.delta","item_id":"reason","delta":"private thought"}),
                json!({"type":"response.output_item.done","item":{"id":"reason","type":"reasoning","encrypted_content":json!({"reasoningText":{"text":"private thought","signature":"real-provider-signature"}}).to_string()}})];
            if tools {
                events.extend([
                    json!({"type":"response.output_item.added","output_index":1,"item":{"id":"item","call_id":"call","type":"function_call","name":"Read","arguments":""}}),
                    json!({"type":"response.function_call_arguments.delta","item_id":"item","output_index":1,"delta":"{\"file_path\":"}),
                    json!({"type":"response.function_call_arguments.done","item_id":"item","output_index":1,"arguments":"{\"file_path\":\"a.txt\"}"}),
                    json!({"type":"response.output_item.done","output_index":1,"item":{"id":"item","call_id":"call","type":"function_call","name":"Read","arguments":"{\"file_path\":\"a.txt\"}"}})]);
            } else { events.push(json!({"type":"response.output_text.delta","delta":"visible answer"})); }
            events.push(json!({"type":"response.completed","response":{"status":"completed","usage":{
                "input_tokens":100,"output_tokens":12,"input_tokens_details":{"cached_tokens":60},
                "kiro:credits":{"value":0.125,"unit":"credit"}}}}));
            let wire=events.iter().map(|e|format!("data: {e}\n\n")).collect::<String>();
            Response::builder().header("content-type","text/event-stream").body(Body::from_stream(
                stream::iter(wire.as_bytes().chunks(7).map(Bytes::copy_from_slice).map(Ok::<_,Infallible>).collect::<Vec<_>>())
            )).unwrap()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let credential = KiroCredentials {
        id: Some(1),
        auth_method: Some("idc".into()),
        access_token: Some("fixture-token".into()),
        refresh_token: Some("fixture-refresh".into()),
        expires_at: Some("2099-01-01T00:00:00Z".into()),
        profile_arn: Some("fixture-profile".into()),
        ..Default::default()
    };
    let manager = Arc::new(
        MultiTokenManager::new(Config::default(), vec![credential], None, None, false).unwrap(),
    );
    let endpoints = HashMap::from([(
        "ide".into(),
        Arc::new(crate::kiro::endpoint::IdeEndpoint::new()) as Arc<dyn KiroEndpoint>,
    )]);
    let mut provider = KiroProvider::new(manager, endpoints, "ide".into());
    provider.set_test_create_response_base(base);
    let state = AppState::new(true, ToolCompatibilityMode::Raw)
        .with_shared_kiro_provider(Arc::new(provider));
    (state, bodies, calls, task)
}

#[tokio::test]
async fn create_response_gateway_text_thinking_usage_and_credits_reach_both_routes() {
    for cc in [false, true] {
        for streaming in [false, true] {
            for thinking in [false, true] {
                let (state, bodies, calls, server) = modern_fixture(false).await;
                let manager = state
                    .kiro_provider
                    .as_ref()
                    .unwrap()
                    .token_manager()
                    .clone();
                let mut payload = json!({"model":"claude-sonnet-5-5","max_tokens":128,"stream":streaming,
                    "messages":[{"role":"user","content":"explain a mutex"}]});
                if thinking {
                    payload["thinking"] = json!({"type":"enabled","budget_tokens":1024});
                }
                let response = compatibility_request(state, cc, payload).await;
                assert_eq!(response.status(), StatusCode::OK);
                let message = parsed_protocol_message(response, streaming).await;
                assert_eq!(message["stop_reason"], "end_turn");
                assert!(message["content"].to_string().contains("visible answer"));
                assert_eq!(
                    message["content"].to_string().contains("private thought"),
                    thinking
                );
                if thinking {
                    assert!(
                        message["content"]
                            .to_string()
                            .contains("real-provider-signature")
                    );
                }
                assert_eq!(message["usage"]["input_tokens"], 40);
                assert_eq!(message["usage"]["cache_read_input_tokens"], 60);
                assert_eq!(message["usage"]["output_tokens"], 12);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(bodies.lock()[0]["model"], "claude-sonnet-5.5");
                assert_eq!(bodies.lock()[0]["origin"], "AI_EDITOR");
                assert!(!bodies.lock()[0]["input"].is_null());
                assert_eq!(manager.get_in_flight(1), 0);
                assert!((manager.snapshot().entries[0].total_credits - 0.125).abs() < 1e-9);
                server.abort();
            }
        }
    }
}

#[tokio::test]
async fn create_response_gateway_tools_finish_once_with_valid_arguments() {
    for streaming in [false, true] {
        let (state, bodies, calls, server) = modern_fixture(true).await;
        let payload = json!({"model":"claude-sonnet-5-5","max_tokens":128,"stream":streaming,
            "messages":[{"role":"user","content":"read a.txt"}],
            "tools":[{"name":"Read","description":"read a file","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}}]});
        let response = compatibility_request(state, false, payload).await;
        assert_eq!(response.status(), StatusCode::OK);
        let message = parsed_protocol_message(response, streaming).await;
        assert_eq!(message["stop_reason"], "tool_use");
        let content = message["content"].as_array().unwrap();
        let calls_out = content
            .iter()
            .filter(|c| c["type"] == "tool_use")
            .collect::<Vec<_>>();
        assert_eq!(calls_out.len(), 1);
        assert_eq!(calls_out[0]["name"], "Read");
        assert_eq!(calls_out[0]["input"], json!({"file_path":"a.txt"}));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(bodies.lock()[0]["tools"][0]["type"], "function");
        server.abort();
    }
}
