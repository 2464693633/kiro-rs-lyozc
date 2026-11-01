//! IP 管理与账号关联的服务层测试

use std::sync::Arc;

use super::AdminService;
use crate::admin::error::AdminServiceError;
use crate::admin::types::{AddProxyRequest, UpdateCredentialRequest, UpdateProxyRequest};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::{CredentialProxyBinding, MultiTokenManager};
use crate::model::config::Config;

fn cred(
    id: u64,
    url: Option<&str>,
    user: Option<&str>,
    pass: Option<&str>,
    proxy_id: Option<u64>,
) -> KiroCredentials {
    KiroCredentials {
        id: Some(id),
        proxy_url: url.map(str::to_string),
        proxy_username: user.map(str::to_string),
        proxy_password: pass.map(str::to_string),
        proxy_id,
        ..KiroCredentials::default()
    }
}

fn service(creds: Vec<KiroCredentials>) -> AdminService {
    let manager =
        Arc::new(MultiTokenManager::new(Config::default(), creds, None, None, false).unwrap());
    AdminService::new(manager, Vec::new())
}

fn binding(svc: &AdminService, id: u64) -> CredentialProxyBinding {
    svc.token_manager
        .proxy_bindings()
        .into_iter()
        .find(|b| b.credential_id == id)
        .unwrap()
}

fn update(json: serde_json::Value) -> UpdateCredentialRequest {
    serde_json::from_value(json).unwrap()
}

fn add_proxy(svc: &AdminService, url: &str) -> u64 {
    svc.add_proxy(AddProxyRequest {
        url: Some(url.to_string()),
        ..Default::default()
    })
    .unwrap()
    .id
}

#[tokio::test]
async fn migration_imports_custom_proxies_and_links_without_rewriting() {
    let svc = service(vec![
        cred(1, Some("socks5://1.1.1.1:1080"), Some("u"), Some("p"), None),
        cred(2, Some("socks5://u:p@1.1.1.1:1080"), None, None, None),
        cred(3, Some("direct"), None, None, Some(42)),
        cred(4, None, None, None, None),
        cred(5, Some("http://2.2.2.2:8080"), None, None, Some(99)),
    ]);
    svc.migrate_credential_proxies();

    assert_eq!(
        svc.proxy_pool.list().len(),
        2,
        "两种写法的同一代理只入池一次"
    );
    let (first, second) = (binding(&svc, 1), binding(&svc, 2));
    assert!(first.proxy_id.is_some());
    assert_eq!(first.proxy_id, second.proxy_id);
    assert_eq!(
        first.proxy_url.as_deref(),
        Some("socks5://1.1.1.1:1080"),
        "只写关联，不改原有字段"
    );
    assert_eq!(first.proxy_username.as_deref(), Some("u"));
    assert_eq!(binding(&svc, 3).proxy_id, None, "直连清除残留关联");
    assert_eq!(binding(&svc, 4).proxy_id, None);
    let fifth = binding(&svc, 5).proxy_id;
    assert!(
        fifth.is_some() && fifth != Some(99),
        "关联的代理不存在时重新入池"
    );
    assert!(
        svc.get_proxy_pool()
            .proxies
            .iter()
            .any(|p| p.credential_count == 2)
    );

    let before = svc.token_manager.proxy_bindings();
    svc.migrate_credential_proxies();
    assert_eq!(
        svc.token_manager.proxy_bindings(),
        before,
        "重复执行不应再有变化"
    );
    assert_eq!(svc.proxy_pool.list().len(), 2);
}

#[tokio::test]
async fn migration_syncs_drifted_copy_from_pool() {
    let svc = service(vec![cred(1, None, None, None, None)]);
    let id = add_proxy(&svc, "http://u:p@3.3.3.3:3128");
    svc.token_manager
        .set_credential_proxy(CredentialProxyBinding {
            credential_id: 1,
            proxy_id: Some(id),
            proxy_url: Some("http://9.9.9.9:8080".into()),
            proxy_username: None,
            proxy_password: None,
        })
        .unwrap();
    svc.migrate_credential_proxies();
    let b = binding(&svc, 1);
    assert_eq!(b.proxy_id, Some(id));
    assert_eq!(
        b.proxy_url.as_deref(),
        Some("http://u:p@3.3.3.3:3128"),
        "以代理池为准"
    );
}

#[tokio::test]
async fn proxy_edits_sync_to_linked_accounts_and_delete_is_blocked() {
    let svc = service(vec![
        cred(1, None, None, None, None),
        cred(2, None, None, None, None),
    ]);
    let id = add_proxy(&svc, "socks5://u:p@1.1.1.1:1080");
    svc.update_credential(1, update(serde_json::json!({ "proxyId": id })))
        .unwrap();
    assert_eq!(
        binding(&svc, 1).proxy_url.as_deref(),
        Some("socks5://u:p@1.1.1.1:1080")
    );
    let item = svc
        .get_all_credentials()
        .credentials
        .into_iter()
        .find(|c| c.id == 1)
        .unwrap();
    assert_eq!(item.proxy_name.as_deref(), Some("1.1.1.1:1080"));

    // 只改邮箱不影响代理
    svc.update_credential(1, update(serde_json::json!({ "email": "a@example.com" })))
        .unwrap();
    assert_eq!(binding(&svc, 1).proxy_id, Some(id));

    let patch = UpdateProxyRequest {
        password: Some("new".into()),
        ..Default::default()
    };
    let (entry, changed) = svc.update_proxy(id, patch).unwrap();
    assert!(changed && entry.has_password);
    assert_eq!(
        binding(&svc, 1).proxy_url.as_deref(),
        Some("socks5://u:new@1.1.1.1:1080")
    );
    assert_eq!(binding(&svc, 2).proxy_id, None, "未关联的账号不受影响");

    assert!(matches!(
        svc.delete_proxy(id),
        Err(AdminServiceError::Conflict(_))
    ));
    svc.update_credential(1, update(serde_json::json!({ "proxyUrl": "" })))
        .unwrap();
    let cleared = binding(&svc, 1);
    assert_eq!((cleared.proxy_id, cleared.proxy_url), (None, None));
    svc.delete_proxy(id).unwrap();
}
#[tokio::test]
async fn custom_proxy_on_account_is_adopted_and_disabled_proxy_is_rejected() {
    let svc = service(vec![cred(1, None, None, None, None)]);
    svc.update_credential(
        1,
        update(serde_json::json!({ "proxyUrl": "http://u:p%40ss@4.4.4.4:8080" })),
    )
    .unwrap();
    let pool = svc.proxy_pool.list();
    assert_eq!(pool.len(), 1, "自定义代理收进 IP 管理");
    assert_eq!(pool[0].endpoint.password.as_deref(), Some("p@ss"));
    let b = binding(&svc, 1);
    assert_eq!(b.proxy_id, Some(pool[0].id));
    assert_eq!(b.proxy_url.as_deref(), Some("http://u:p%40ss@4.4.4.4:8080"));

    svc.update_credential(1, update(serde_json::json!({ "proxyUrl": "direct" })))
        .unwrap();
    let direct = binding(&svc, 1);
    assert_eq!(
        (direct.proxy_id, direct.proxy_url.as_deref()),
        (None, Some("direct"))
    );

    let disabled = add_proxy(&svc, "http://5.5.5.5:8080");
    svc.set_proxy_enabled(disabled, false).unwrap();
    let err = svc
        .update_credential(1, update(serde_json::json!({ "proxyId": disabled })))
        .unwrap_err();
    assert!(matches!(err, AdminServiceError::BadRequest(_)));
    assert!(matches!(
        svc.update_credential(1, update(serde_json::json!({ "proxyId": 999 }))),
        Err(AdminServiceError::ProxyNotFound { id: 999 })
    ));
}

#[tokio::test]
async fn round_robin_links_accounts_to_pool_entries() {
    let svc = service((1..=3).map(|id| cred(id, None, None, None, None)).collect());
    let a = add_proxy(&svc, "http://1.1.1.1:8080");
    let b = add_proxy(&svc, "http://2.2.2.2:8080");
    let resp = svc.assign_proxies_round_robin(None).unwrap();
    assert_eq!((resp.assigned, resp.proxy_count), (3, 2));
    let ids: Vec<Option<u64>> = (1..=3).map(|id| binding(&svc, id).proxy_id).collect();
    assert_eq!(ids.iter().filter(|p| **p == Some(a)).count(), 2);
    assert_eq!(ids.iter().filter(|p| **p == Some(b)).count(), 1);
    assert!(matches!(
        svc.delete_proxy(a),
        Err(AdminServiceError::Conflict(_))
    ));
}
