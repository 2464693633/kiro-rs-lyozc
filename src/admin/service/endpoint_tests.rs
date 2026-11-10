use super::*;
use serde_json::json;

fn service(path: Option<PathBuf>) -> AdminService {
    let credential = KiroCredentials {
        id: Some(1),
        auth_method: Some("api_key".into()),
        kiro_api_key: Some("ksk_endpoint_offline_fixture".into()),
        email: Some("student@example.test".into()),
        groups: vec!["student".into()],
        proxy_url: Some("direct".into()),
        subscription_title: Some("KIRO STUDENT".into()),
        ..Default::default()
    };
    let manager = Arc::new(MultiTokenManager::new(
        Config::default(), vec![credential], None, path, true,
    ).unwrap());
    AdminService::new(manager, vec!["ide".into(), "cli".into()])
}

fn update(service: &AdminService, value: serde_json::Value) -> Result<(), AdminServiceError> {
    service.update_credential(1, serde_json::from_value(value).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_endpoint_switches_immediately_and_persists_without_changing_account() {
    let path = std::env::temp_dir().join(format!("kiro-endpoint-{}.json", Uuid::new_v4()));
    let service = service(Some(path.clone()));
    let original = service.token_manager.clone_credential(1).unwrap();
    for endpoint in ["cli", "ide", "", "cli"] {
        update(&service, json!({"endpoint": endpoint})).unwrap();
        let mut current = service.token_manager.clone_credential(1).unwrap();
        let expected = if endpoint.is_empty() { None } else { Some(endpoint) };
        assert_eq!(current.endpoint.as_deref(), expected);
        let saved: Vec<KiroCredentials> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved[0].endpoint.as_deref(), expected);
        let status = service.get_all_credentials();
        assert_eq!(status.credentials[0].endpoint, expected.unwrap_or("ide"));
        assert_eq!(status.credentials[0].endpoint_override.as_deref(), expected);
        current.endpoint = original.endpoint.clone();
        assert_eq!(serde_json::to_value(current).unwrap(), serde_json::to_value(&original).unwrap());
    }
    // Reload the saved credential to prove the override survives a restart.
    let saved = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let reloaded = MultiTokenManager::new(Config::default(), saved, None, None, true).unwrap();
    assert_eq!(reloaded.clone_credential(1).unwrap().endpoint.as_deref(), Some("cli"));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn omitted_endpoint_preserves_selection_and_invalid_endpoint_rejects_all_changes() {
    let service = service(None);
    update(&service, json!({"endpoint": " cli "})).unwrap();
    for value in [json!({"email": "updated@example.test"}), json!({"endpoint": null})] {
        update(&service, value).unwrap();
        assert_eq!(service.token_manager.clone_credential(1).unwrap().endpoint.as_deref(), Some("cli"));
    }
    let original = serde_json::to_value(service.token_manager.clone_credential(1).unwrap()).unwrap();
    assert!(matches!(update(&service, json!({
        "endpoint": "unknown", "email": "wrong@example.test", "groups": []
    })), Err(AdminServiceError::BadRequest(_))));
    assert_eq!(serde_json::to_value(service.token_manager.clone_credential(1).unwrap()).unwrap(), original);
    assert!(matches!(service.update_credential(99, serde_json::from_value(json!({"endpoint":"cli"})).unwrap()), Err(AdminServiceError::NotFound { id: 99 })));
}
