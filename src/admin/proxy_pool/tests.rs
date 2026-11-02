use super::probe::{ProbeResult, apply_probe_result, parse_geo, parse_ipify, redact};
use super::*;

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "kiro-proxy-pool-{}-{}.json",
        std::process::id(),
        fastrand::u64(..)
    ))
}

fn backups(path: &std::path::Path) -> Vec<PathBuf> {
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let prefix = format!("{stem}.json.bak-");
    std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .collect()
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    for backup in backups(path) {
        let _ = std::fs::remove_file(backup);
    }
}

fn endpoint(raw: &str) -> ProxyEndpoint {
    ProxyEndpoint::parse(raw).unwrap()
}

fn input(raw: &str) -> ProxyInput {
    ProxyInput {
        name: None,
        endpoint: endpoint(raw),
    }
}

#[test]
fn detected_proxy_requires_probe_and_recovers_without_reenabling_manual_entries() {
    let pool = ProxyPoolManager::new(None, crate::model::config::TlsBackend::Rustls);
    let proxy = crate::http_client::ProxyConfig::new("http://user:p%40ss@localhost:1234");
    assert_eq!(pool.register_detected(std::slice::from_ref(&proxy)), 1);
    assert_eq!(pool.register_detected(std::slice::from_ref(&proxy)), 0);
    assert_eq!(
        pool.availability_for_proxy(&proxy),
        ProxyAvailability::Untracked
    );
    assert!(pool.assignable_urls().is_empty());
    let id = pool.list()[0].id;
    {
        let mut entries = pool.entries.lock();
        let entry = &mut entries[0];
        for _ in 0..MAX_PROXY_PROBE_FAILURES {
            apply_probe_result(
                entry,
                &ProbeResult::Err {
                    error: "unavailable".into(),
                },
            );
        }
        assert!(entry.auto_disabled);
        apply_probe_result(
            entry,
            &ProbeResult::Ok {
                latency_ms: 1,
                exit: None,
            },
        );
        assert!(entry.enabled);
        assert!(!entry.auto_disabled);
    }
    assert_eq!(
        pool.availability_for_proxy(&proxy),
        ProxyAvailability::Available
    );
    assert_eq!(pool.assignable_urls(), vec![proxy.url.clone()]);
    pool.set_enabled(id, false).unwrap();
    {
        let mut entries = pool.entries.lock();
        apply_probe_result(
            &mut entries[0],
            &ProbeResult::Ok {
                latency_ms: 1,
                exit: None,
            },
        );
        assert!(!entries[0].enabled);
    }
    assert_eq!(
        pool.availability_for_proxy(&proxy),
        ProxyAvailability::Unavailable
    );
    assert!(pool.assignable_urls().is_empty());
}

#[test]
fn parse_decodes_auth_and_normalizes() {
    let ep = endpoint("SOCKS5://us%40r:p%3Ass@Proxy.Example.com:1080");
    assert_eq!(ep.protocol, "socks5");
    assert_eq!(ep.host, "proxy.example.com");
    assert_eq!(ep.port, 1080);
    assert_eq!(ep.username.as_deref(), Some("us@r"));
    assert_eq!(ep.password.as_deref(), Some("p:ss"));
    assert_eq!(ep.display_url(), "socks5://proxy.example.com:1080");
    assert_eq!(ProxyEndpoint::parse(&ep.full_url()).unwrap(), ep);
}

#[test]
fn parse_supports_ipv6_and_default_http_port() {
    let ep = endpoint("socks5h://[::1]:1080");
    assert_eq!(ep.host, "::1");
    assert_eq!(ep.display_url(), "socks5h://[::1]:1080");
    assert_eq!(ProxyEndpoint::parse(&ep.full_url()).unwrap(), ep);
    assert_eq!(endpoint("http://127.0.0.1").port, 80);
}

#[test]
fn parse_rejects_bad_input_without_echoing_secret() {
    for raw in [
        "ftp://u:secret@1.2.3.4:21",
        "socks5://u:secret@1.2.3.4",
        "http://:secret@1.2.3.4:8080",
        "not a url secret",
    ] {
        let err = ProxyEndpoint::parse(raw).unwrap_err();
        assert!(!err.contains("secret"), "{raw} -> {err}");
    }
}

#[test]
fn legacy_file_is_converted_backed_up_and_keeps_ids() {
    let path = temp_path();
    std::fs::write(
        &path,
        r#"[
            {"id":3,"url":"socks5://u:p@1.2.3.4:1080","label":"东京","enabled":false},
            {"id":5,"url":"http://5.6.7.8:3128"},
            {"id":9,"url":"not a proxy"}
        ]"#,
    )
    .unwrap();
    let pool = ProxyPoolManager::new(Some(path.clone()), TlsBackend::Rustls);
    let list = pool.list();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, 3);
    assert_eq!(list[0].name, "东京");
    assert!(!list[0].enabled);
    assert_eq!(list[0].endpoint.password.as_deref(), Some("p"));
    assert_eq!(list[1].name, "5.6.7.8:3128");
    assert_eq!(backups(&path).len(), 1, "含旧格式条目时先备份原文件");
    assert_eq!(pool.add(input("http://9.9.9.9:8080")).unwrap().id, 6);
    cleanup(&path);
}

#[test]
fn new_format_round_trips_and_stays_readable_by_old_version() {
    let path = temp_path();
    {
        let pool = ProxyPoolManager::new(Some(path.clone()), TlsBackend::Rustls);
        pool.add(ProxyInput {
            name: Some("香港".into()),
            endpoint: endpoint("socks5h://user:p%40ss@10.0.0.1:1080"),
        })
        .unwrap();
    }
    let raw: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let legacy: super::storage::LegacyProxyEntry = serde_json::from_value(raw[0].clone()).unwrap();
    assert_eq!(legacy.url, "socks5h://user:p%40ss@10.0.0.1:1080");
    assert_eq!(legacy.label.as_deref(), Some("香港"));

    let reloaded = ProxyPoolManager::new(Some(path.clone()), TlsBackend::Rustls);
    let entry = &reloaded.list()[0];
    assert_eq!(entry.name, "香港");
    assert_eq!(entry.endpoint.password.as_deref(), Some("p@ss"));
    assert!(backups(&path).is_empty(), "新格式不应触发备份");
    cleanup(&path);
}

#[test]
fn add_and_find_or_add_deduplicate_by_endpoint() {
    let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
    let first = pool.add(input("socks5://u:p@1.1.1.1:1080")).unwrap();
    let err = pool.add(input("SOCKS5://u:p@1.1.1.1:1080")).unwrap_err();
    assert!(matches!(err, ProxyPoolError::Duplicate(_)));
    let (found, created) = pool
        .find_or_add(endpoint("socks5://u:p@1.1.1.1:1080"))
        .unwrap();
    assert_eq!((found.id, created), (first.id, false));
    let (other, created) = pool
        .find_or_add(endpoint("http://u:p@1.1.1.1:1080"))
        .unwrap();
    assert!(created, "协议不同视为不同代理");
    assert_ne!(other.id, first.id);
}
#[test]
fn batch_add_reports_line_numbers_without_passwords() {
    let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
    let (added, errors) = pool.batch_add(vec![
        "# 注释".into(),
        "socks5://u:secret@1.1.1.1:1080".into(),
        "".into(),
        "socks5://u:secret@1.1.1.1:1080".into(),
        "ftp://u:secret@2.2.2.2:21".into(),
        "http://3.3.3.3:8080".into(),
    ]);
    assert_eq!(added.len(), 2);
    assert_eq!(errors.len(), 2);
    assert!(errors[0].starts_with("第 4 行"), "{}", errors[0]);
    assert!(errors[1].starts_with("第 5 行"), "{}", errors[1]);
    assert!(errors.iter().all(|e| !e.contains("secret")));
}

#[test]
fn rename_keeps_health_but_endpoint_change_resets_it() {
    let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
    let entry = pool.add(input("socks5://u:p@1.1.1.1:1080")).unwrap();
    {
        let mut entries = pool.entries.lock();
        let e = entries.iter_mut().find(|e| e.id == entry.id).unwrap();
        e.health = ProxyHealth::Unhealthy;
        e.consecutive_failures = MAX_PROXY_PROBE_FAILURES;
        e.enabled = false;
        e.auto_disabled = true;
    }

    let rename = ProxyPatch {
        name: Some("东京".into()),
        ..Default::default()
    };
    let (renamed, changed) = pool.update(entry.id, rename).unwrap();
    assert!(!changed);
    assert_eq!(renamed.name, "东京");
    assert_eq!(renamed.health, ProxyHealth::Unhealthy);

    let move_port = ProxyPatch {
        port: Some(1081),
        ..Default::default()
    };
    let (moved, changed) = pool.update(entry.id, move_port).unwrap();
    assert!(changed);
    assert_eq!(moved.health, ProxyHealth::Unknown);
    assert_eq!(moved.consecutive_failures, 0);
    assert!(moved.enabled && !moved.auto_disabled, "改地址解除自动禁用");
    assert_eq!(
        moved.endpoint.password.as_deref(),
        Some("p"),
        "密码留空保持不变"
    );

    let clear_auth = ProxyPatch {
        username: Some(String::new()),
        password: Some(String::new()),
        ..Default::default()
    };
    let (cleared, _) = pool.update(entry.id, clear_auth).unwrap();
    assert!(cleared.endpoint.username.is_none() && cleared.endpoint.password.is_none());
}

#[test]
fn update_rejects_duplicate_of_another_entry() {
    let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
    pool.add(input("http://1.1.1.1:8080")).unwrap();
    let second = pool.add(input("http://2.2.2.2:8080")).unwrap();
    let patch = ProxyPatch {
        host: Some("1.1.1.1".into()),
        ..Default::default()
    };
    assert!(matches!(
        pool.update(second.id, patch),
        Err(ProxyPoolError::Duplicate(_))
    ));
}
#[test]
fn parses_exit_lookups() {
    let exit = parse_geo(
        r#"{"status":"success","query":"1.2.3.4","country":"日本","countryCode":"JP","regionName":"东京都","city":"东京"}"#,
    )
    .unwrap();
    assert_eq!(exit.ip, "1.2.3.4");
    assert_eq!(exit.country_code.as_deref(), Some("JP"));
    assert_eq!(exit.city.as_deref(), Some("东京"));
    assert!(parse_geo(r#"{"status":"fail","message":"reserved range"}"#).is_err());
    assert_eq!(parse_ipify(r#"{"ip":"5.6.7.8"}"#).unwrap().ip, "5.6.7.8");
    assert!(parse_ipify(r#"{"ip":""}"#).is_err());
}

#[test]
fn redact_hides_plain_and_encoded_password() {
    let ep = endpoint("socks5://u:p%40ss@1.1.1.1:1080");
    let message = "proxy failed: p@ss / p%40ss";
    let redacted = redact(message, &ep);
    assert!(
        !redacted.contains("p@ss") && !redacted.contains("p%40ss"),
        "{redacted}"
    );
}

#[test]
fn probe_failure_increments_and_auto_disables_at_threshold() {
    let mut entry = fresh_entry(1, "t".into(), endpoint("socks5://127.0.0.1:1080"));
    let err = ProbeResult::Err {
        error: "connection refused".to_string(),
    };
    // 前两次失败：计数累加，仍启用
    for n in 1..MAX_PROXY_PROBE_FAILURES {
        let (unhealthy, disabled) = apply_probe_result(&mut entry, &err);
        assert!(unhealthy && !disabled);
        assert_eq!(entry.consecutive_failures, n);
        assert!(entry.enabled && !entry.auto_disabled);
    }
    // 第 N 次失败：自动禁用
    let (_, disabled) = apply_probe_result(&mut entry, &err);
    assert!(disabled);
    assert!(!entry.enabled && entry.auto_disabled);
    assert_eq!(entry.last_error.as_deref(), Some("connection refused"));
}

#[test]
fn probe_success_clears_failures_and_keeps_known_geo() {
    let mut entry = fresh_entry(1, "t".into(), endpoint("socks5://127.0.0.1:1080"));
    entry.consecutive_failures = 2;
    entry.health = ProxyHealth::Unhealthy;
    let geo = parse_geo(r#"{"status":"success","query":"1.2.3.4","country":"日本"}"#).unwrap();
    let ok = ProbeResult::Ok {
        latency_ms: 123,
        exit: Some(geo),
    };
    let (unhealthy, disabled) = apply_probe_result(&mut entry, &ok);
    assert!(!unhealthy && !disabled);
    assert_eq!(entry.consecutive_failures, 0);
    assert_eq!(entry.health, ProxyHealth::Healthy);
    assert_eq!(entry.latency_ms, Some(123));

    // 只拿到相同 IP 时保留已知的国家
    let ip_only = ProbeResult::Ok {
        latency_ms: 50,
        exit: Some(parse_ipify(r#"{"ip":"1.2.3.4"}"#).unwrap()),
    };
    apply_probe_result(&mut entry, &ip_only);
    assert_eq!(
        entry.exit.as_ref().unwrap().country.as_deref(),
        Some("日本")
    );
}

#[test]
fn set_enabled_true_clears_auto_disable_state() {
    let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
    let entry = pool.add(input("socks5://127.0.0.1:1080")).unwrap();
    {
        let mut entries = pool.entries.lock();
        let e = entries.iter_mut().find(|e| e.id == entry.id).unwrap();
        e.enabled = false;
        e.auto_disabled = true;
        e.consecutive_failures = MAX_PROXY_PROBE_FAILURES;
    }
    pool.set_enabled(entry.id, true).unwrap();
    let e = pool.get(entry.id).unwrap();
    assert!(e.enabled && !e.auto_disabled);
    assert_eq!(e.consecutive_failures, 0);
}
