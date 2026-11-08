use super::*;
use crate::admin::proxy_pool::ProxyEndpoint;
use crate::admin::types::AddProxyRequest;
use axum::{
    Router,
    http::{HeaderMap, Method, StatusCode, Uri},
};
use base64::Engine as _;

fn service(credentials: Vec<KiroCredentials>) -> AdminService {
    let manager = Arc::new(
        MultiTokenManager::new(
            Config::default(),
            credentials,
            Some(ProxyConfig::new("http://unused-global.invalid:8888")),
            None,
            false,
        )
        .unwrap(),
    );
    AdminService::new(manager, Vec::new())
}

fn add_proxy(service: &AdminService, url: &str) -> u64 {
    service
        .add_proxy(AddProxyRequest {
            url: Some(url.into()),
            ..Default::default()
        })
        .unwrap()
        .id
}

#[tokio::test]
async fn login_route_preserves_authentication_and_explicit_direct() {
    let service = service(vec![]);
    let id = add_proxy(&service, "http://user:p%40ss@127.0.0.1:3210");
    let (template, runtime) = service
        .resolve_login_proxy(Some(id), Some("direct"), None)
        .unwrap();
    assert_eq!(template.proxy_id, Some(id));
    let endpoint = ProxyEndpoint::parse(&runtime.unwrap().url).unwrap();
    assert_eq!(endpoint.username.as_deref(), Some("user"));
    assert_eq!(endpoint.password.as_deref(), Some("p@ss"));
    assert_eq!(
        template
            .effective_proxy(service.token_manager.proxy().as_ref())
            .unwrap()
            .url,
        endpoint.full_url()
    );
    let (direct, runtime) = service
        .resolve_login_proxy(None, Some("direct"), None)
        .unwrap();
    assert!(runtime.is_none());
    assert!(
        direct
            .effective_proxy(service.token_manager.proxy().as_ref())
            .is_none()
    );
    let (global, runtime) = service.resolve_login_proxy(None, Some(""), None).unwrap();
    assert!(global.proxy_id.is_none());
    assert_eq!(runtime, service.token_manager.proxy());
    service.set_proxy_enabled(id, false).unwrap();
    assert!(service.resolve_login_proxy(Some(id), None, None).is_err());
    assert!(service.resolve_login_proxy(Some(999), None, None).is_err());
}

#[tokio::test]
async fn json_import_persists_selected_ip_binding_and_ignores_old_auth() {
    let service = service(vec![]);
    let proxy_id = add_proxy(&service, "http://new-user:new-pass@127.0.0.1:9");
    let request = serde_json::from_value(serde_json::json!({
        "authMethod": "api_key", "kiroApiKey": "ksk_offline_import_fixture",
        "proxyId": proxy_id, "proxyUrl": "direct",
        "proxyUsername": "old-user", "proxyPassword": "old-pass",
        "groups": ["开发", "测试"]
    }))
    .unwrap();
    let event = service
        .import_one_credential(request, false)
        .await
        .into_event(0);
    assert_eq!(event.status, "imported", "{:?}", event.error);
    let credential = service
        .token_manager
        .clone_credential(event.credential_id.unwrap())
        .unwrap();
    assert_eq!(credential.proxy_id, Some(proxy_id));
    assert_eq!(credential.groups, vec!["开发", "测试"]);
    assert!(credential.proxy_username.is_none());
    assert!(credential.proxy_password.is_none());
    let endpoint = ProxyEndpoint::parse(&credential.effective_proxy(None).unwrap().url).unwrap();
    assert_eq!(endpoint.username.as_deref(), Some("new-user"));
    assert_eq!(endpoint.password.as_deref(), Some("new-pass"));
    assert_eq!(service.get_proxy_pool().proxies[0].credential_count, 1);
}

#[tokio::test]
async fn social_session_freezes_selection_and_relogin_inherits_account_ip() {
    let credential = KiroCredentials {
        id: Some(1),
        proxy_url: Some("http://account.invalid:1234".into()),
        proxy_username: Some("account-user".into()),
        proxy_password: Some("account-pass".into()),
        groups: vec!["原有分组".into()],
        ..Default::default()
    };
    let service = service(vec![credential.clone()]);
    let id = add_proxy(&service, "http://login-user:login-pass@127.0.0.1:3211");
    let response = service
        .start_social_login(serde_json::from_value(serde_json::json!({
            "proxyId": id, "groups": ["开发", "测试"]
        })).unwrap())
        .await
        .unwrap();
    {
        let sessions = service.social_sessions.lock();
        let session = &sessions[&response.session_id];
        assert_eq!(session.cred_template.proxy_id, Some(id));
        assert_eq!(session.cred_template.groups, vec!["开发", "测试"]);
        assert_eq!(
            session.proxy,
            session
                .cred_template
                .effective_proxy(service.token_manager.proxy().as_ref())
        );
    }
    let response = service
        .start_social_relogin(1, serde_json::from_value(serde_json::json!({})).unwrap())
        .await
        .unwrap();
    {
        let sessions = service.social_sessions.lock();
        let session = &sessions[&response.session_id];
        assert_eq!(
            session.proxy,
            credential.effective_proxy(service.token_manager.proxy().as_ref())
        );
        assert!(!session.relogin_proxy_override);
        assert_eq!(session.cred_template.groups, credential.groups);
    }
    let response = service
        .start_social_relogin(
            1,
            serde_json::from_value(serde_json::json!({"proxyId": id})).unwrap(),
        )
        .await
        .unwrap();
    let template = {
        let sessions = service.social_sessions.lock();
        let session = &sessions[&response.session_id];
        assert!(session.relogin_proxy_override);
        session.cred_template.clone()
    };
    service.save_login_proxy(1, &template).unwrap();
    let saved = service.token_manager.clone_credential(1).unwrap();
    assert_eq!(saved.proxy_id, Some(id));
    assert_eq!(saved.groups, credential.groups);
    assert_eq!(saved.effective_proxy(None), template.effective_proxy(None));
    assert_eq!(
        saved.proxy_username, None,
        "old split authentication must not follow a new proxy"
    );
}

#[test]
fn login_groups_are_optional_and_reject_invalid_shapes() {
    let social: StartSocialLoginRequest = serde_json::from_value(serde_json::json!({})).unwrap();
    let idc: StartIdcLoginRequest = serde_json::from_value(serde_json::json!({
        "region": "us-east-1"
    })).unwrap();
    assert!(social.groups.is_empty());
    assert!(idc.groups.is_empty());
    let idc: StartIdcLoginRequest = serde_json::from_value(serde_json::json!({
        "region": "us-east-1", "groups": ["开发", "测试"]
    })).unwrap();
    assert_eq!(idc.groups, vec!["开发", "测试"]);
    for groups in [serde_json::json!("开发"), serde_json::json!([42])] {
        assert!(serde_json::from_value::<StartSocialLoginRequest>(serde_json::json!({
            "groups": groups
        })).is_err());
        assert!(serde_json::from_value::<StartIdcLoginRequest>(serde_json::json!({
            "region": "us-east-1", "groups": groups
        })).is_err());
    }
}

#[tokio::test]
async fn first_social_and_oidc_requests_use_selected_proxy_without_direct_fallback() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
        seen.lock().push((
            method,
            uri.to_string(),
            headers.get("proxy-authorization").cloned(),
        ));
        async { StatusCode::BAD_GATEWAY }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://user:password@{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let service = service(vec![]);
    let proxy_id = add_proxy(&service, &url);
    let response = service
        .start_social_login(
            serde_json::from_value(serde_json::json!({
                "proxyId": proxy_id, "authEndpoint": "http://auth.example.test"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let state = service.social_sessions.lock()[&response.session_id]
        .state
        .clone();
    let social = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        service.complete_social_login(
            &response.session_id,
            "fixture-code".into(),
            state,
            "".into(),
            "/oauth/callback".into(),
        ),
    )
    .await
    .unwrap();
    assert!(social.is_err());
    let idc = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        service.start_idc_login(
            serde_json::from_value(serde_json::json!({"region": "us-east-1", "proxyId": proxy_id}))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert!(idc.is_err());
    let seen = requests.lock();
    assert_eq!(
        seen.len(),
        2,
        "both requests must stop at the rejecting local proxy"
    );
    assert_eq!(seen[0].0, Method::POST);
    assert!(seen[0].1.contains("auth.example.test/oauth/token"));
    assert_eq!(seen[1].0, Method::CONNECT);
    assert!(seen[1].1.contains("oidc.us-east-1.amazonaws.com:443"));
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("user:password")
    );
    assert!(
        seen.iter()
            .all(|request| request.2.as_ref().unwrap().to_str().unwrap() == expected)
    );
    assert!(service.token_manager.clone_all_credentials().is_empty());
    server.abort();
}

#[tokio::test]
async fn batch_json_import_binds_proxy_before_first_token_refresh() {
    let requests = Arc::new(Mutex::new(0));
    let seen = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(move || {
        *seen.lock() += 1;
        async { StatusCode::BAD_GATEWAY }
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let service = service(vec![]);
    let id = add_proxy(&service, &url);
    for verify in [false, true] {
        let request = serde_json::from_value(serde_json::json!({
            "refreshToken": "offline-refresh-token".repeat(8), "proxyId": id
        }))
        .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            service.import_one_credential(request, verify),
        )
        .await
        .unwrap();
        assert_eq!(result.into_event(0).status, "failed");
    }
    assert_eq!(*requests.lock(), 2);
    assert!(service.token_manager.clone_all_credentials().is_empty());
    server.abort();
}
