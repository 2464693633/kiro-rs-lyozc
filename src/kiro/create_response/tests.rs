use super::*;
use crate::{
    kiro::model::credentials::KiroCredentials,
    model::config::{ApiDomain, Config},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn converts_ordered_history_tools_results_images_and_reasoning() {
    let source = json!({"conversationState":{
        "conversationId":"conversation", "agentTaskType":"vibe",
        "history":[{"userInputMessage":{"content":"hello"}},
            {"assistantResponseMessage":{"content":"reading", "toolUses":[{"toolUseId":"c","name":"Read","input":{"path":"a"}}]}}],
        "currentMessage":{"userInputMessage":{"content":"continue","modelId":"claude-sonnet-5.5","origin":"KIRO_CLI",
            "images":[{"format":"PNG","source":{"bytes":"QUJD"}}],
            "userInputMessageContext":{"toolResults":[{"toolUseId":"c","content":[{"text":"result"}]}],
            "tools":[{"toolSpecification":{"name":"Read","description":"read", "inputSchema":{"json":{"type":"object"}}}}]}}}},
        "additionalModelRequestFields":{"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}}});
    let body: Value =
        serde_json::from_str(&transform_body(&source.to_string(), Some("profile")).unwrap())
            .unwrap();
    assert_eq!(
        body["input"],
        json!([
        {"role":"user","content":"hello"},{"role":"assistant","content":"reading"},
        {"type":"function_call","call_id":"c","name":"Read","arguments":"{\"path\":\"a\"}"},
        {"type":"function_call_output","call_id":"c","output":"result"},
        {"role":"user","content":[{"type":"input_text","text":"continue"},{"type":"input_image","image_url":"data:image/png;base64,QUJD"}]}])
    );
    assert_eq!(
        body["tools"][0],
        json!({"type":"function","name":"Read","description":"read","parameters":{"type":"object"}})
    );
    assert_eq!(
        body["reasoning"],
        json!({"type":"adaptive","effort":"high","summary":"summarized"})
    );
    assert_eq!(body["origin"], "AI_EDITOR");
    assert_eq!(body["profileArn"], "profile");
    assert!(body.get("conversationState").is_none());
    assert!(body.get("instructions").is_none());
}

#[test]
fn rejects_invalid_canonical_requests_instead_of_sending_an_empty_prompt() {
    for source in [
        json!({}),
        json!({"conversationState":{"currentMessage":{"userInputMessage":{"content":"","modelId":"m"}}}}),
    ] {
        assert!(transform_body(&source.to_string(), None).is_err());
    }
}

#[derive(Clone)]
struct Stub {
    calls: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
}

async fn feature_stub(State(stub): State<Stub>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    stub.calls.fetch_add(1, Ordering::SeqCst);
    assert!(headers.contains_key("x-amzn-kiro-profile-arn"));
    assert_eq!(headers["tokentype"], "SSO_OIDC");
    if stub.failures.load(Ordering::SeqCst) > 0 {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    let key = hex::encode(Sha256::digest(
        b"kiro-feature-config-key-salt-3e9b1d7acreate_response_api",
    ));
    let arm = if headers["authorization"] == "Bearer selected" {
        "create_response"
    } else {
        "generate_assistant_response"
    };
    (StatusCode::OK, Json(json!({"configuration":{key:arm}})))
}

fn context<'a>(c: &'a KiroCredentials, cfg: &'a Config, token: &'a str) -> RequestContext<'a> {
    RequestContext {
        credentials: c,
        config: cfg,
        token,
        machine_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        api_domain: ApiDomain::Kiro,
    }
}

#[tokio::test]
async fn cache_coalesces_reads_isolates_accounts_and_revalidates_new_signins() {
    let stub = Stub {
        calls: Arc::new(AtomicUsize::new(0)),
        failures: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/featureConfiguration", get(feature_stub))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cache = FeatureCache {
        test_runtime_base: Some(base),
        ..Default::default()
    };
    let cfg = Config::default();
    let c = KiroCredentials {
        id: Some(1),
        auth_method: Some("idc".into()),
        profile_arn: Some("profile".into()),
        ..Default::default()
    };
    let ctx = context(&c, &cfg, "selected");
    let client = Client::builder().no_proxy().build().unwrap();
    let values =
        futures::future::join_all((0..8).map(|_| cache.select(&client, &ctx, "cli"))).await;
    assert!(values.iter().all(|v| *v));
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
    let mut c2 = c.clone();
    c2.id = Some(2);
    assert!(
        !cache
            .select(&client, &context(&c2, &cfg, "legacy"), "ide")
            .await
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    assert!(
        !cache
            .select(&client, &context(&c, &cfg, "changed-token"), "ide")
            .await
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 3);
    // Never hit the public control plane for unrelated or special auth protocols.
    assert!(!cache.select(&client, &ctx, "loopback").await);
    c2.kiro_api_key = Some("ksk_fixture".into());
    assert!(
        !cache
            .select(&client, &context(&c2, &cfg, "selected"), "cli")
            .await
    );
    c2.kiro_api_key = None;
    c2.auth_method = Some("external_idp".into());
    assert!(
        !cache
            .select(&client, &context(&c2, &cfg, "selected"), "ide")
            .await
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn failed_refresh_retains_last_arm_and_failed_first_lookup_uses_legacy() {
    let stub = Stub {
        calls: Arc::new(AtomicUsize::new(0)),
        failures: Arc::new(AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/featureConfiguration", get(feature_stub))
        .with_state(stub.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cache = FeatureCache {
        test_runtime_base: Some(base),
        ..Default::default()
    };
    let cfg = Config::default();
    let mut c = KiroCredentials {
        id: Some(1),
        auth_method: Some("idc".into()),
        profile_arn: Some("profile".into()),
        ..Default::default()
    };
    let client = Client::builder().no_proxy().build().unwrap();
    assert!(
        cache
            .select(&client, &context(&c, &cfg, "selected"), "ide")
            .await
    );
    let entry = cache.accounts.lock()[&1].clone();
    entry.lock().await.expires = None;
    stub.failures.store(1, Ordering::SeqCst);
    assert!(
        cache
            .select(&client, &context(&c, &cfg, "selected"), "ide")
            .await
    );
    assert!(
        cache
            .select(&client, &context(&c, &cfg, "selected"), "ide")
            .await
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    c.id = Some(2);
    assert!(
        !cache
            .select(&client, &context(&c, &cfg, "selected"), "ide")
            .await
    );
    assert!(
        !cache
            .select(&client, &context(&c, &cfg, "selected"), "ide")
            .await
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 3);
    server.abort();
}
