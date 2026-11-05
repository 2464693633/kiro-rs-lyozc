use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::admin::{AdminService, ClientKeyManager, GroupManager, TraceStore, UsageAggregator};
use crate::{kiro::token_manager::MultiTokenManager, model::config::Config};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn cache_read_limit_api_validates_before_mutation_and_distinguishes_null_from_omitted() {
    let keys = Arc::new(ClientKeyManager::new());
    let manager =
        Arc::new(MultiTokenManager::new(Config::default(), Vec::new(), None, None, false).unwrap());
    let state = AdminState::new(
        "fixture-admin",
        AdminService::new(manager, Vec::new()),
        keys.clone(),
        Arc::new(UsageAggregator::new()),
        Arc::new(TraceStore::open_in_memory().unwrap()),
        Arc::new(GroupManager::new()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/client-keys", listener.local_addr().unwrap());
    let server = Server(tokio::spawn(async move {
        axum::serve(listener, create_admin_router(state))
            .await
            .unwrap()
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    assert_eq!(
        client
            .post(&base)
            .json(&json!({"name":"unauthorized","cacheReadLimitPercent":60}))
            .send()
            .await
            .unwrap()
            .status(),
        axum::http::StatusCode::UNAUTHORIZED
    );
    for value in [json!(-1), json!(101), json!(1000), json!(60.5), json!("60")] {
        let response = client
            .post(&base)
            .header("x-api-key", "fixture-admin")
            .json(&json!({"name":"invalid","cacheReadLimitPercent":value}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert!(keys.list().is_empty());
    }
    let created: Value = client
        .post(&base)
        .header("x-api-key", "fixture-admin")
        .json(&json!({"name":"bounded","cacheReadLimitPercent":60}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_u64().unwrap();
    let url = format!("{base}/{id}");
    assert_eq!(keys.cache_read_limit_percent(id), Some(60));
    for patch in [
        json!({"name":"renamed"}),
        json!({"description":"metadata only"}),
    ] {
        assert!(
            client
                .put(&url)
                .header("x-api-key", "fixture-admin")
                .json(&patch)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert_eq!(keys.cache_read_limit_percent(id), Some(60));
    }
    let invalid = client
        .put(&url)
        .header("x-api-key", "fixture-admin")
        .json(&json!({"name":"must-not-change","cacheReadLimitPercent":101}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(keys.list()[0].name, "renamed");
    assert_eq!(keys.cache_read_limit_percent(id), Some(60));
    for (patch, expected) in [
        (json!({"cacheReadLimitPercent":0}), Some(0)),
        (json!({"cacheReadLimitPercent":100}), Some(100)),
        (json!({"cacheReadLimitPercent":null}), None),
    ] {
        assert!(
            client
                .put(&url)
                .header("x-api-key", "fixture-admin")
                .json(&patch)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert_eq!(keys.cache_read_limit_percent(id), expected);
        let listed: Value = client
            .get(&base)
            .header("x-api-key", "fixture-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["keys"][0]["cacheReadLimitPercent"], json!(expected));
    }
    let response = client.put(&url)
        .header("x-api-key", "fixture-admin")
        .json(&json!({"billingMode":"calls","maxCalls":7,"cacheMetering":false,"cacheReadLimitPercent":60}))
        .send().await.unwrap();
    assert!(response.status().is_success());
    for patch in [json!({"name":"both-settings"}), json!({"description":"preserve both"})] {
        assert!(client.put(&url).header("x-api-key", "fixture-admin")
            .json(&patch).send().await.unwrap().status().is_success());
        let row = keys.list().remove(0);
        assert_eq!(row.billing_mode, crate::admin::client_keys::BillingMode::Calls);
        assert_eq!(row.max_calls, Some(7));
        assert!(!row.cache_metering);
        assert_eq!(row.cache_read_limit_percent, Some(60));
    }
    for patch in [
        json!({"billingMode":"credits","cacheReadLimitPercent":101}),
        json!({"maxCalls":1,"cacheReadLimitPercent":20}),
    ] {
        assert_eq!(client.put(&url).header("x-api-key", "fixture-admin")
            .json(&patch).send().await.unwrap().status(), axum::http::StatusCode::BAD_REQUEST);
        let row = keys.list().remove(0);
        assert_eq!(row.billing_mode, crate::admin::client_keys::BillingMode::Calls);
        assert_eq!(row.max_calls, Some(7));
        assert_eq!(row.cache_read_limit_percent, Some(60));
    }
    drop(server);
}
