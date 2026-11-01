//! proxy_pool.json 的读写：兼容旧格式，写出旧版本可读的冗余字段，原子落盘。

use std::path::Path;

use serde::Deserialize;

use super::{ProxyEndpoint, ProxyEntry, ProxyHealth, default_true};

/// 旧版本条目：只有完整地址与可选备注。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LegacyProxyEntry {
    pub id: u64,
    pub url: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub health: ProxyHealth,
    #[serde(default)]
    pub latency_ms: Option<u32>,
    #[serde(default)]
    pub last_checked_at: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub auto_disabled: bool,
}

impl LegacyProxyEntry {
    fn into_entry(self) -> Result<ProxyEntry, String> {
        let endpoint = ProxyEndpoint::parse(&self.url)?;
        let name = self
            .label
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| endpoint.default_name());
        Ok(ProxyEntry {
            id: self.id,
            name,
            endpoint,
            enabled: self.enabled,
            health: self.health,
            latency_ms: self.latency_ms,
            last_checked_at: self.last_checked_at,
            consecutive_failures: self.consecutive_failures,
            auto_disabled: self.auto_disabled,
            last_error: None,
            exit: None,
        })
    }
}

/// 读取代理池文件。新格式条目直接解析；只有 `url` 的旧条目转换为结构化字段。
///
/// 出现旧条目或无法识别的条目时，先把原文件复制为 `.bak-时间戳` 备份，
/// 避免下一次写盘覆盖掉无法识别的内容。
pub(super) fn load(path: &Path) -> Vec<ProxyEntry> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("读取代理池文件失败 {}: {}", path.display(), e);
            return Vec::new();
        }
    };
    let items: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(items) => items,
        Err(e) => {
            tracing::warn!("代理池文件格式无效，已忽略 {}: {}", path.display(), e);
            backup(path);
            return Vec::new();
        }
    };
    let (mut entries, mut converted, mut skipped) = (Vec::new(), 0usize, 0usize);
    for item in items {
        let parsed = match serde_json::from_value::<ProxyEntry>(item.clone()) {
            Ok(mut entry) => match entry.endpoint.clone().normalize() {
                Ok(endpoint) => {
                    entry.endpoint = endpoint;
                    Ok(entry)
                }
                Err(e) => Err(e),
            },
            Err(_) => {
                converted += 1;
                serde_json::from_value::<LegacyProxyEntry>(item)
                    .map_err(|e| e.to_string())
                    .and_then(LegacyProxyEntry::into_entry)
            }
        };
        match parsed {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                skipped += 1;
                tracing::warn!("代理池条目无法识别，已跳过: {}", e);
            }
        }
    }
    if converted > 0 || skipped > 0 {
        tracing::info!(
            "代理池文件含 {} 条旧格式条目，跳过 {} 条，转换前先备份原文件",
            converted,
            skipped
        );
        backup(path);
    }
    entries
}

fn backup(path: &Path) {
    let suffix = chrono::Local::now().format("bak-%Y%m%d%H%M%S").to_string();
    let backup_path = path.with_extension(format!("json.{suffix}"));
    match std::fs::copy(path, &backup_path) {
        Ok(_) => tracing::info!("代理池原文件已备份到 {}", backup_path.display()),
        Err(e) => tracing::warn!("备份代理池文件失败 {}: {}", backup_path.display(), e),
    }
}

/// 原子写入代理池。每个条目额外写出旧格式的 `url`（含认证）与 `label`，
/// 回退到旧版本时仍能读取。
pub(super) fn write_file(path: Option<&Path>, entries: &[ProxyEntry]) -> std::io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut value = serde_json::to_value(entry)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("url".into(), entry.endpoint.full_url().into());
            object.insert("label".into(), entry.name.clone().into());
        }
        items.push(value);
    }
    let json = serde_json::to_string_pretty(&items)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
