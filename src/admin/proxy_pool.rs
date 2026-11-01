//! 代理 IP 管理
//!
//! 独立于凭据管理，存储为 proxy_pool.json。代理按协议、主机、端口、认证结构化保存，
//! 账号通过 `proxyId` 关联代理：编辑代理后同步到所有关联账号，仍被使用的代理不能删除。
//!
//! 除增删改查外，还提供主动健康检查：周期性（或按需）通过每个代理探测出口 IP、
//! 地理位置与延迟；连续探测失败达阈值的代理会被自动禁用。

mod endpoint;
mod probe;
mod storage;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub use endpoint::ProxyEndpoint;
use storage::write_file;

use crate::model::config::TlsBackend;

/// 连续探测失败阈值：达到后自动禁用（与凭据的 MAX_FAILURES_PER_CREDENTIAL 对齐）
const MAX_PROXY_PROBE_FAILURES: u32 = 3;

/// 代理健康状态
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHealth {
    /// 尚未探测
    #[default]
    Unknown,
    /// 最近一次探测成功
    Healthy,
    /// 最近一次探测失败
    Unhealthy,
}

/// 最近一次探测到的出口信息
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyExitInfo {
    pub ip: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
}

/// 持久化的代理条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEntry {
    pub id: u64,
    pub name: String,
    #[serde(flatten)]
    pub endpoint: ProxyEndpoint,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 健康状态（健康检查结果）
    #[serde(default)]
    pub health: ProxyHealth,
    /// 最近一次成功探测的延迟（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 最近一次探测时间（RFC3339）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// 连续探测失败计数（成功后清零）
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 是否由健康检查自动禁用（区别于用户手动禁用）
    #[serde(default)]
    pub auto_disabled: bool,
    /// 最近一次探测失败的原因（已抹掉密码）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 最近一次探测到的出口信息
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ProxyExitInfo>,
}

fn default_true() -> bool {
    true
}

fn fresh_entry(id: u64, name: String, endpoint: ProxyEndpoint) -> ProxyEntry {
    ProxyEntry {
        id,
        name,
        endpoint,
        enabled: true,
        health: ProxyHealth::Unknown,
        latency_ms: None,
        last_checked_at: None,
        consecutive_failures: 0,
        auto_disabled: false,
        last_error: None,
        exit: None,
    }
}

/// 一次全量健康检查的摘要
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    /// 探测成功数
    pub healthy: usize,
    /// 探测失败数
    pub unhealthy: usize,
    /// 本轮新增的自动禁用数
    pub auto_disabled: usize,
}

/// 新建代理的输入；名称为空时使用 `主机:端口`
#[derive(Debug, Clone)]
pub struct ProxyInput {
    pub name: Option<String>,
    pub endpoint: ProxyEndpoint,
}

/// 编辑代理的补丁：`None` 表示不修改；用户名或密码传空串表示清除
#[derive(Debug, Clone, Default)]
pub struct ProxyPatch {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyPoolError {
    #[error("代理不存在")]
    NotFound,
    #[error("代理已存在: {0}")]
    Duplicate(String),
    #[error("{0}")]
    Invalid(String),
    #[error("保存代理池失败: {0}")]
    Persist(String),
}

pub struct ProxyPoolManager {
    entries: Mutex<Vec<ProxyEntry>>,
    // 仅需原子自增，不需要与 entries 联锁；约定独立使用，无锁顺序问题
    next_id: AtomicU64,
    path: Option<PathBuf>,
    /// TLS 后端，构建探测用 HTTP client 时需要
    tls_backend: TlsBackend,
}
impl ProxyPoolManager {
    pub fn new(path: Option<PathBuf>, tls_backend: TlsBackend) -> Self {
        let entries = path.as_deref().map(storage::load).unwrap_or_default();
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        Self {
            entries: Mutex::new(entries),
            next_id: AtomicU64::new(next_id),
            path,
            tls_backend,
        }
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        self.entries.lock().clone()
    }

    pub fn get(&self, id: u64) -> Option<ProxyEntry> {
        self.entries.lock().iter().find(|e| e.id == id).cloned()
    }

    fn persist(&self, entries: &[ProxyEntry]) -> Result<(), ProxyPoolError> {
        write_file(self.path.as_deref(), entries)
            .map_err(|e| ProxyPoolError::Persist(e.to_string()))
    }

    /// 新建代理；地址（协议 + 主机 + 端口 + 认证）与已有代理重复时拒绝。
    pub fn add(&self, input: ProxyInput) -> Result<ProxyEntry, ProxyPoolError> {
        let endpoint = input
            .endpoint
            .normalize()
            .map_err(ProxyPoolError::Invalid)?;
        let mut entries = self.entries.lock();
        if entries.iter().any(|e| e.endpoint == endpoint) {
            return Err(ProxyPoolError::Duplicate(endpoint.display_url()));
        }
        let name = input
            .name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| endpoint.default_name());
        let entry = fresh_entry(self.next_id.fetch_add(1, Ordering::Relaxed), name, endpoint);
        entries.push(entry.clone());
        if let Err(e) = self.persist(&entries) {
            entries.pop();
            return Err(e);
        }
        Ok(entry)
    }

    /// 按地址查找代理，不存在时新建。返回 `(条目, 是否新建)`。
    /// 账号带自定义代理地址时用它把地址收进代理池。
    pub fn find_or_add(
        &self,
        endpoint: ProxyEndpoint,
    ) -> Result<(ProxyEntry, bool), ProxyPoolError> {
        let endpoint = endpoint.normalize().map_err(ProxyPoolError::Invalid)?;
        let mut entries = self.entries.lock();
        if let Some(existing) = entries.iter().find(|e| e.endpoint == endpoint) {
            return Ok((existing.clone(), false));
        }
        let name = endpoint.default_name();
        let entry = fresh_entry(self.next_id.fetch_add(1, Ordering::Relaxed), name, endpoint);
        entries.push(entry.clone());
        if let Err(e) = self.persist(&entries) {
            entries.pop();
            return Err(e);
        }
        Ok((entry, true))
    }
    /// 批量添加：每行一个代理地址，空行与 `#` 开头的行跳过。
    /// 错误信息带行号与不含密码的地址，返回 `(新增条目, 跳过原因)`。
    pub fn batch_add(&self, urls: Vec<String>) -> (Vec<ProxyEntry>, Vec<String>) {
        let mut added = Vec::new();
        let mut errors = Vec::new();
        let mut entries = self.entries.lock();
        let original_len = entries.len();
        for (index, raw) in urls.iter().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let endpoint = match ProxyEndpoint::parse(line) {
                Ok(endpoint) => endpoint,
                Err(e) => {
                    errors.push(format!("第 {} 行：{}", index + 1, e));
                    continue;
                }
            };
            if entries.iter().any(|e| e.endpoint == endpoint) {
                errors.push(format!(
                    "第 {} 行：代理已存在 {}",
                    index + 1,
                    endpoint.display_url()
                ));
                continue;
            }
            let name = endpoint.default_name();
            let entry = fresh_entry(self.next_id.fetch_add(1, Ordering::Relaxed), name, endpoint);
            entries.push(entry.clone());
            added.push(entry);
        }
        if !added.is_empty() {
            if let Err(e) = self.persist(&entries) {
                entries.truncate(original_len);
                added.clear();
                errors.push(e.to_string());
            }
        }
        (added, errors)
    }
    /// 编辑代理。返回 `(更新后的条目, 地址是否变化)`；地址变化时清空健康状态与出口信息，
    /// 并解除健康检查造成的自动禁用（手动禁用保持不变）。
    pub fn update(&self, id: u64, patch: ProxyPatch) -> Result<(ProxyEntry, bool), ProxyPoolError> {
        let mut entries = self.entries.lock();
        let index = entries
            .iter()
            .position(|e| e.id == id)
            .ok_or(ProxyPoolError::NotFound)?;
        let mut entry = entries[index].clone();
        let mut endpoint = entry.endpoint.clone();
        if let Some(v) = patch.protocol {
            endpoint.protocol = v;
        }
        if let Some(v) = patch.host {
            endpoint.host = v;
        }
        if let Some(v) = patch.port {
            endpoint.port = v;
        }
        if let Some(v) = patch.username {
            endpoint.username = Some(v);
        }
        if let Some(v) = patch.password {
            endpoint.password = Some(v);
        }
        let endpoint = endpoint.normalize().map_err(ProxyPoolError::Invalid)?;
        if entries.iter().any(|e| e.id != id && e.endpoint == endpoint) {
            return Err(ProxyPoolError::Duplicate(endpoint.display_url()));
        }
        let endpoint_changed = endpoint != entry.endpoint;
        if let Some(name) = patch.name {
            let name = name.trim();
            entry.name = if name.is_empty() {
                endpoint.default_name()
            } else {
                name.to_string()
            };
        }
        if endpoint_changed {
            entry.health = ProxyHealth::Unknown;
            entry.latency_ms = None;
            entry.last_checked_at = None;
            entry.consecutive_failures = 0;
            entry.last_error = None;
            entry.exit = None;
            if entry.auto_disabled {
                entry.auto_disabled = false;
                entry.enabled = true;
            }
        }
        entry.endpoint = endpoint;
        let previous = std::mem::replace(&mut entries[index], entry.clone());
        if let Err(e) = self.persist(&entries) {
            entries[index] = previous;
            return Err(e);
        }
        Ok((entry, endpoint_changed))
    }

    pub fn delete(&self, id: u64) -> Result<(), ProxyPoolError> {
        let mut entries = self.entries.lock();
        let index = entries
            .iter()
            .position(|e| e.id == id)
            .ok_or(ProxyPoolError::NotFound)?;
        let removed = entries.remove(index);
        if let Err(e) = self.persist(&entries) {
            entries.insert(index, removed);
            return Err(e);
        }
        Ok(())
    }

    /// 设置代理启用/禁用状态
    ///
    /// 用户手动启用时清除「健康检查自动禁用」标记与连续失败计数，
    /// 让该代理重新参与健康检查与分配。
    pub fn set_enabled(&self, id: u64, enabled: bool) -> Result<(), ProxyPoolError> {
        let mut entries = self.entries.lock();
        let index = entries
            .iter()
            .position(|e| e.id == id)
            .ok_or(ProxyPoolError::NotFound)?;
        let previous = entries[index].clone();
        let entry = &mut entries[index];
        entry.enabled = enabled;
        if enabled {
            entry.auto_disabled = false;
            entry.consecutive_failures = 0;
        }
        if let Err(e) = self.persist(&entries) {
            entries[index] = previous;
            return Err(e);
        }
        Ok(())
    }

    /// 可用于轮询分配的代理 ID：已启用且最近一次探测未失败
    pub fn assignable_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .entries
            .lock()
            .iter()
            .filter(|e| e.enabled && e.health != ProxyHealth::Unhealthy)
            .map(|e| e.id)
            .collect();
        ids.sort_unstable();
        ids
    }
}
