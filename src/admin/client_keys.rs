//! 客户端 API Key 管理
//!
//! 管理中转站下发的客户端 Key。生成值以 `sk-` 开头；鉴权不校验前缀，只按完整值匹配。
//!
//! 与上游 Kiro 凭据（`KiroCredentials`，`ksk_*`）相互独立：
//! - 上游凭据池：服务对接 Kiro 的"出口"
//! - 客户端 Key：中转站对外的"入口"
//!
//! 持久化为 `client_api_keys.json`（与 `credentials.json` 同目录）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::anthropic::cache_engine::CacheEngineKind;

/// 单条客户端 Key
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientKey {
    pub id: u64,
    /// 明文 Key（中转站场景，校验需原值，不做 hash）
    pub key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub total_calls: u64,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// 累计 credit 计费量（meteringEvent.usage 累加）
    #[serde(default)]
    pub total_credits: f64,
    /// 绑定的账号分组名（可选）
    ///
    /// 设置后，用该 Key 发起的请求只会调度到 groups 包含此分组名的上游账号（严格隔离）。
    /// None 表示不绑定分组，可使用全部账号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 系统 Key（由 config.json apiKey 同步，不可删除、可轮换）。
    /// 老数据无此字段，默认 false。
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_system: bool,
    /// 该 Key 使用哪套缓存模拟引擎。
    /// 老数据无此字段，默认 `rust`（引擎 A），使既有 Key 行为完全不变。
    #[serde(default, skip_serializing_if = "CacheEngineKind::is_default")]
    pub cache_engine: CacheEngineKind,
}

/// `by_key` 仅用于判重；鉴权扫描 `entries` 并做常量时间比较。
pub struct ClientKeyManager {
    inner: RwLock<Inner>,
    path: Option<PathBuf>,
}

struct Inner {
    entries: HashMap<u64, ClientKey>,
    by_key: HashMap<String, u64>,
    next_id: u64,
}

impl ClientKeyManager {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                entries: HashMap::new(),
                by_key: HashMap::new(),
                next_id: 1,
            }),
            path: None,
        }
    }

    /// 从文件加载（不存在时返回空管理器）
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let entries: Vec<ClientKey> = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)?
            }
        } else {
            Vec::new()
        };

        let mut by_key = HashMap::with_capacity(entries.len());
        let mut by_id = HashMap::with_capacity(entries.len());
        let mut max_id = 0u64;
        for ck in entries {
            max_id = max_id.max(ck.id);
            by_key.insert(ck.key.clone(), ck.id);
            by_id.insert(ck.id, ck);
        }

        Ok(Self {
            inner: RwLock::new(Inner {
                entries: by_id,
                by_key,
                next_id: max_id + 1,
            }),
            path: Some(path),
        })
    }

    fn save_locked(&self, inner: &Inner) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        let mut list: Vec<&ClientKey> = inner.entries.values().collect();
        list.sort_by_key(|k| k.id);
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("写入客户端 Key 文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化客户端 Key 失败: {}", e),
        }
    }

    /// 列表（按 id 升序）
    pub fn list(&self) -> Vec<ClientKey> {
        let inner = self.inner.read();
        let mut list: Vec<ClientKey> = inner.entries.values().cloned().collect();
        list.sort_by_key(|k| k.id);
        list
    }

    /// 生成并保存新 Key。
    pub fn create(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        cache_engine: CacheEngineKind,
    ) -> ClientKey {
        let entry = self.create_with_key(name, description, group, generate_client_key());
        // 引擎选择在创建后单独落一次，避免改动 create_with_key 的既有签名
        // （sync_system_key 也走它，那条路径必须保持默认引擎）。
        if cache_engine != CacheEngineKind::default() {
            self.update_meta(entry.id, None, None, None, Some(cache_engine));
            return self
                .inner
                .read()
                .entries
                .get(&entry.id)
                .cloned()
                .unwrap_or(entry);
        }
        entry
    }

    /// 使用指定明文创建 Key；明文已存在时返回原条目。
    pub fn create_with_key(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        plaintext: String,
    ) -> ClientKey {
        let mut inner = self.inner.write();
        if let Some(&id) = inner.by_key.get(&plaintext) {
            return inner.entries.get(&id).cloned().expect("by_key 与 entries 应一致");
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let entry = ClientKey {
            id,
            key: plaintext.clone(),
            name,
            description,
            disabled: false,
            created_at: Utc::now().to_rfc3339(),
            last_used_at: None,
            total_calls: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            total_credits: 0.0,
            group: group.filter(|g| !g.trim().is_empty()),
            is_system: false,
            cache_engine: CacheEngineKind::default(),
        };
        inner.by_key.insert(plaintext, id);
        inner.entries.insert(id, entry.clone());
        self.save_locked(&inner);
        entry
    }

    /// 将 `config.apiKey` 同步为唯一的 `id=0` 系统 Key。配置值变化时保留元数据与统计、
    /// 重新启用新值，并删除与新旧明文冲突的非系统条目，使旧值立即失效。
    pub fn sync_system_key(&self, name: String, description: Option<String>, plaintext: String) {
        let mut inner = self.inner.write();
        let previous_key = inner.entries.get(&0).map(|entry| entry.key.clone());
        let mut changed = false;

        if let Some(entry) = inner.entries.get_mut(&0) {
            if entry.key != plaintext {
                entry.key = plaintext.clone();
                entry.disabled = false;
                changed = true;
            }
            if !entry.is_system {
                entry.is_system = true;
                changed = true;
            }
        } else {
            inner.entries.insert(
                0,
                ClientKey {
                    id: 0,
                    key: plaintext.clone(),
                    name,
                    description,
                    disabled: false,
                    created_at: Utc::now().to_rfc3339(),
                    last_used_at: None,
                    total_calls: 0,
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_credits: 0.0,
                    group: None,
                    is_system: true,
                    cache_engine: CacheEngineKind::default(),
                },
            );
            changed = true;
        }

        let entries_before = inner.entries.len();
        inner.entries.retain(|id, entry| {
            *id == 0
                || (entry.key != plaintext
                    && previous_key
                        .as_deref()
                        .map(|old_key| entry.key != old_key)
                        .unwrap_or(true))
        });
        changed |= inner.entries.len() != entries_before;

        for (id, entry) in inner.entries.iter_mut() {
            if *id != 0 && entry.is_system {
                entry.is_system = false;
                changed = true;
            }
        }

        let by_key: HashMap<String, u64> = inner
            .entries
            .iter()
            .map(|(id, entry)| (entry.key.clone(), *id))
            .collect();
        changed |= inner.by_key != by_key;
        inner.by_key = by_key;

        if changed {
            self.save_locked(&inner);
        }
    }

    pub fn delete(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
        // 系统 Key 拒绝删除
        if inner.entries.get(&id).map(|e| e.is_system).unwrap_or(false) {
            return false;
        }
        let removed = match inner.entries.remove(&id) {
            Some(e) => {
                inner.by_key.remove(&e.key);
                true
            }
            None => false,
        };
        if removed {
            self.save_locked(&inner);
        }
        removed
    }

    pub fn set_disabled(&self, id: u64, disabled: bool) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.disabled = disabled;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    pub fn update_meta(
        &self,
        id: u64,
        name: Option<String>,
        description: Option<Option<String>>,
        group: Option<Option<String>>,
        cache_engine: Option<CacheEngineKind>,
    ) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                if let Some(n) = name {
                    e.name = n;
                }
                if let Some(d) = description {
                    e.description = d;
                }
                if let Some(g) = group {
                    e.group = g.filter(|s| !s.trim().is_empty());
                }
                // None = 不变，与其余字段的可选语义一致
                if let Some(engine) = cache_engine {
                    e.cache_engine = engine;
                }
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 返回指定 Key 绑定的分组名（None 表示未绑定或 Key 不存在）
    pub fn group_of(&self, id: u64) -> Option<String> {
        self.inner.read().entries.get(&id).and_then(|e| e.group.clone())
    }

    /// 返回指定 Key 选择的缓存模拟引擎；Key 不存在时回落默认（引擎 A）。
    pub fn cache_engine_of(&self, id: u64) -> CacheEngineKind {
        self.inner
            .read()
            .entries
            .get(&id)
            .map(|e| e.cache_engine)
            .unwrap_or_default()
    }

    /// 列出所有当前被引用的分组名（仅去重，不带计数）。
    pub fn used_group_names(&self) -> Vec<String> {
        let inner = self.inner.read();
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in inner.entries.values() {
            if let Some(g) = &e.group {
                set.insert(g.clone());
            }
        }
        let mut list: Vec<String> = set.into_iter().collect();
        list.sort();
        list
    }

    /// 统计指定分组被多少把 Key 绑定（用于分组管理页 / 删除前提示）。
    pub fn count_with_group(&self, group: &str) -> usize {
        self.inner
            .read()
            .entries
            .values()
            .filter(|e| e.group.as_deref() == Some(group))
            .count()
    }

    /// 指定 id 的 Key 是否为系统 Key（不存在也返回 false）。
    pub fn is_system(&self, id: u64) -> bool {
        self.inner
            .read()
            .entries
            .get(&id)
            .map(|e| e.is_system)
            .unwrap_or(false)
    }

    /// 把所有引用 `old` 的 Key 的 group 字段改为 `new`（分组改名级联用）。
    /// 返回受影响的 Key 数。
    pub fn rename_group(&self, old: &str, new: &str) -> usize {
        let mut inner = self.inner.write();
        let mut affected = 0usize;
        for entry in inner.entries.values_mut() {
            if entry.group.as_deref() == Some(old) {
                entry.group = Some(new.to_string());
                affected += 1;
            }
        }
        if affected > 0 {
            self.save_locked(&inner);
        }
        affected
    }

    /// 把所有引用 `name` 的 Key 的 group 字段清空（强删分组级联用）。
    /// 返回受影响的 Key 数。
    pub fn clear_group(&self, name: &str) -> usize {
        let mut inner = self.inner.write();
        let mut affected = 0usize;
        for entry in inner.entries.values_mut() {
            if entry.group.as_deref() == Some(name) {
                entry.group = None;
                affected += 1;
            }
        }
        if affected > 0 {
            self.save_locked(&inner);
        }
        affected
    }

    /// 生成新明文并保留 id、元数据、分组、统计及状态；旧明文立即失效。
    /// 系统 Key 的调用方必须同步 `config.apiKey`，否则重启时配置值会覆盖轮换结果。
    pub fn rotate(&self, id: u64) -> Option<ClientKey> {
        let new_key = generate_client_key();
        let mut inner = self.inner.write();
        let old_key = inner.entries.get(&id).map(|e| e.key.clone())?;
        inner.by_key.remove(&old_key);
        let entry = inner.entries.get_mut(&id)?;
        entry.key = new_key.clone();
        let snapshot = entry.clone();
        inner.by_key.insert(new_key, id);
        self.save_locked(&inner);
        Some(snapshot)
    }

    /// 重置计数（保留 Key 与名称）
    pub fn reset_stats(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.total_calls = 0;
                e.total_input_tokens = 0;
                e.total_output_tokens = 0;
                e.total_cache_creation_tokens = 0;
                e.total_cache_read_tokens = 0;
                e.total_credits = 0.0;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 不校验前缀，常量时间匹配所有启用 Key；命中后更新使用记录。
    pub fn verify_and_touch(&self, presented: &str) -> Option<u64> {
        let mut inner = self.inner.write();
        let mut hit_id: Option<u64> = None;
        for (id, ck) in inner.entries.iter() {
            if ck.disabled {
                continue;
            }
            if ck.key.as_bytes().ct_eq(presented.as_bytes()).into() {
                hit_id = Some(*id);
                // 不 break，继续完整扫描以保持常量时间
            }
        }
        let id = hit_id?;
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.total_calls += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        // 不在每次请求都落盘（高频写入），由 record_usage / 定期 flush 持久化
        Some(id)
    }

    /// 在请求结束时累计 Token 用量并落盘
    pub fn record_usage(
        &self,
        id: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        credits: f64,
    ) {
        let mut inner = self.inner.write();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.total_input_tokens += input_tokens;
            entry.total_output_tokens += output_tokens;
            entry.total_cache_creation_tokens += cache_creation_tokens;
            entry.total_cache_read_tokens += cache_read_tokens;
            if credits.is_finite() && credits > 0.0 {
                entry.total_credits += credits;
            }
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        self.save_locked(&inner);
    }

    /// 获取统计后的 active Key 数（未禁用）
    pub fn active_count(&self) -> usize {
        self.inner.read().entries.values().filter(|e| !e.disabled).count()
    }
}

impl Default for ClientKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

/// 生成 `sk-` 前缀 + 32 位 base62 随机字符串
pub fn generate_client_key() -> String {
    const CHARSET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let body: String = (0..32)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    format!("sk-{}", body)
}

/// 脱敏展示：保留前 8 个字符（含前缀）和后 4 个字符
pub fn mask_client_key(key: &str) -> String {
    let char_count = key.chars().count();
    if char_count <= 12 {
        return key.to_string();
    }
    let start: String = key.chars().take(8).collect();
    let end: String = key.chars().skip(char_count - 4).collect();
    format!("{start}...{end}")
}

pub fn default_path_in(dir: &Path) -> PathBuf {
    dir.join("client_api_keys.json")
}

pub type SharedClientKeyManager = Arc<ClientKeyManager>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_verify() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, CacheEngineKind::default());
        assert!(entry.key.starts_with("sk-"));
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
        assert_eq!(mgr.verify_and_touch("nope"), None);
    }

    /// 按 Key 选引擎：创建时可指定，`cache_engine_of` 能读回，`update_meta` 可改，
    /// 传 None 表示不变。未知 id 回落默认（引擎 A）。
    #[test]
    fn per_key_cache_engine_selection() {
        let mgr = ClientKeyManager::new();

        // 默认创建 → 引擎 A
        let a = mgr.create("a".into(), None, None, CacheEngineKind::default());
        assert_eq!(mgr.cache_engine_of(a.id), CacheEngineKind::Rust);

        // 创建时指定引擎 B
        let b = mgr.create("b".into(), None, None, CacheEngineKind::Go);
        assert_eq!(b.cache_engine, CacheEngineKind::Go, "返回的条目应带所选引擎");
        assert_eq!(mgr.cache_engine_of(b.id), CacheEngineKind::Go);

        // update_meta 传 Some 改引擎、传 None 不动
        assert!(mgr.update_meta(a.id, None, None, None, Some(CacheEngineKind::Go)));
        assert_eq!(mgr.cache_engine_of(a.id), CacheEngineKind::Go);
        assert!(mgr.update_meta(a.id, Some("改名".into()), None, None, None));
        assert_eq!(
            mgr.cache_engine_of(a.id),
            CacheEngineKind::Go,
            "cache_engine 传 None 时不应被重置"
        );

        // 未知 id → 默认
        assert_eq!(mgr.cache_engine_of(9999), CacheEngineKind::Rust);
    }

    /// 老数据缺 `cacheEngine` 字段时必须反序列化成引擎 A，且默认值不写回 JSON
    /// （保持既有文件形状不变）。
    #[test]
    fn legacy_keys_default_to_rust_engine_and_field_is_omitted() {
        let legacy = r#"[{
            "id": 1, "key": "sk-legacy", "name": "老 Key",
            "createdAt": "2026-01-01T00:00:00Z"
        }]"#;
        let keys: Vec<ClientKey> = serde_json::from_str(legacy).unwrap();
        assert_eq!(keys[0].cache_engine, CacheEngineKind::Rust);

        // 默认值不序列化
        let json = serde_json::to_string(&keys[0]).unwrap();
        assert!(
            !json.contains("cacheEngine"),
            "默认引擎不应写进 JSON，实际: {json}"
        );

        // 非默认值必须序列化
        let mut go_key = keys[0].clone();
        go_key.cache_engine = CacheEngineKind::Go;
        let json = serde_json::to_string(&go_key).unwrap();
        assert!(json.contains("\"cacheEngine\":\"go\""), "实际: {json}");
    }

    /// 经**磁盘**往返后每个 Key 的引擎选择必须原样存活，且互不串味。
    ///
    /// 与 `legacy_keys_default_to_rust_engine_and_field_is_omitted` 的区别：那条只测
    /// serde 层的单条序列化，本条驱动真实 `save_locked` → 新 manager `load`。
    /// `cache_engine` 带 `skip_serializing_if`，默认值不落盘、靠 `serde(default)` 读回 ——
    /// 若谁去掉 `default`，非默认 Key 仍能存活，只有本条会失败。
    #[test]
    fn cache_engine_survives_disk_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "kiro-ck-{}-{:?}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        // load 于不存在的路径 → 空 manager 但带 path，后续 create 会真实写盘
        let mgr = ClientKeyManager::load(&path).expect("空路径应可加载");
        let rust_key = mgr.create("走引擎A".into(), None, None, CacheEngineKind::Rust);
        let go_key = mgr.create("走引擎B".into(), None, None, CacheEngineKind::Go);
        // 第三条：建成默认再改成 Go，覆盖 update_meta 的写盘路径
        let flipped = mgr.create("后改B".into(), None, None, CacheEngineKind::default());
        assert!(mgr.update_meta(flipped.id, None, None, None, Some(CacheEngineKind::Go)));

        // 磁盘形状：只有非默认引擎的 Key 才写出 cacheEngine（此处 2 条）
        let on_disk = std::fs::read_to_string(&path).expect("应已写盘");
        assert_eq!(
            on_disk.matches("\"cacheEngine\"").count(),
            2,
            "只应有 2 条非默认引擎落盘，实际文件: {on_disk}"
        );

        // 换一个 manager 重新加载，模拟重启
        let reloaded = ClientKeyManager::load(&path).expect("应可重新加载");
        assert_eq!(
            reloaded.cache_engine_of(rust_key.id),
            CacheEngineKind::Rust,
            "引擎 A 的 Key 重启后应仍为 A"
        );
        assert_eq!(
            reloaded.cache_engine_of(go_key.id),
            CacheEngineKind::Go,
            "引擎 B 的 Key 重启后应仍为 B（字段真的落盘了）"
        );
        assert_eq!(
            reloaded.cache_engine_of(flipped.id),
            CacheEngineKind::Go,
            "经 update_meta 改过的 Key 重启后应保持 B"
        );
        // Key 本身也要能继续鉴权，确认加载的是同一批条目而非空表
        assert_eq!(reloaded.verify_and_touch(&go_key.key), Some(go_key.id));
        assert_eq!(reloaded.list().len(), 3);

        let _ = std::fs::remove_file(&path);
    }

    /// 系统 Key（id=0）经 `sync_system_key` 反复同步后，引擎选择必须存活。
    #[test]
    fn system_key_retains_cache_engine_across_sync() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("系统".into(), None, "key-v1".into());
        assert_eq!(mgr.cache_engine_of(0), CacheEngineKind::Rust, "初始为默认");

        assert!(mgr.update_meta(0, None, None, None, Some(CacheEngineKind::Go)));
        assert_eq!(mgr.cache_engine_of(0), CacheEngineKind::Go);

        // 轮换 apiKey：既有条目路径只改 key/disabled/is_system，引擎选择应保留。
        mgr.sync_system_key("系统".into(), None, "key-v2".into());
        assert_eq!(
            mgr.cache_engine_of(0),
            CacheEngineKind::Go,
            "轮换系统 Key 不应重置引擎选择"
        );
        assert_eq!(mgr.verify_and_touch("key-v2"), Some(0));
    }

    #[test]
    fn disabled_key_rejected() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, CacheEngineKind::default());
        mgr.set_disabled(entry.id, true);
        assert_eq!(mgr.verify_and_touch(&entry.key), None);
        mgr.set_disabled(entry.id, false);
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
    }

    #[test]
    fn record_usage_accumulates() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, CacheEngineKind::default());
        mgr.record_usage(entry.id, 100, 50, 0, 0, 0.0);
        mgr.record_usage(entry.id, 200, 30, 5, 10, 1.5);
        let list = mgr.list();
        let e = list.iter().find(|x| x.id == entry.id).unwrap();
        assert_eq!(e.total_input_tokens, 300);
        assert_eq!(e.total_output_tokens, 80);
        assert_eq!(e.total_cache_creation_tokens, 5);
        assert_eq!(e.total_cache_read_tokens, 10);
    }

    #[test]
    fn mask_format() {
        assert_eq!(mask_client_key("sk-abcdefghijklmnop"), "sk-abcde...mnop");
        assert_eq!(mask_client_key("short"), "short");
        assert_eq!(mask_client_key("密钥🔐测试abcdefgh"), "密钥🔐测试abc...efgh");
    }

    #[test]
    fn rotate_replaces_key_but_keeps_metadata_and_stats() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("kb".to_string(), Some("desc".into()), Some("groupA".into()), CacheEngineKind::default());
        mgr.record_usage(entry.id, 100, 50, 5, 10, 1.5);
        let old_key = entry.key.clone();
        let rotated = mgr.rotate(entry.id).expect("rotate should succeed");
        assert_ne!(rotated.key, old_key);
        assert!(rotated.key.starts_with("sk-"));
        assert_eq!(rotated.id, entry.id);
        assert_eq!(rotated.name, "kb");
        assert_eq!(rotated.description.as_deref(), Some("desc"));
        assert_eq!(rotated.group.as_deref(), Some("groupA"));
        assert_eq!(rotated.total_input_tokens, 100);
        assert_eq!(rotated.total_output_tokens, 50);
        assert_eq!(mgr.verify_and_touch(&old_key), None);
        assert_eq!(mgr.verify_and_touch(&rotated.key), Some(entry.id));
    }

    #[test]
    fn rotate_unknown_id_returns_none() {
        let mgr = ClientKeyManager::new();
        assert!(mgr.rotate(999).is_none());
    }

    #[test]
    fn sync_system_key_uses_id_zero() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert!(mgr.is_system(0));
        assert_eq!(mgr.list().first().map(|k| k.id), Some(0));
        assert_eq!(mgr.verify_and_touch("custom-api-key"), Some(0));
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert_eq!(mgr.list().iter().filter(|k| k.is_system).count(), 1);
    }

    #[test]
    fn sync_system_key_replaces_config_value_and_revokes_old_key() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), Some("初始描述".into()), "custom-a".into());
        mgr.update_meta(
            0,
            Some("保留名称".into()),
            Some(Some("保留描述".into())),
            Some(Some("group-a".into())),
            None,
        );
        mgr.record_usage(0, 100, 50, 5, 10, 1.5);
        assert_eq!(mgr.verify_and_touch("custom-a"), Some(0));
        mgr.set_disabled(0, true);

        let conflicting = mgr.create_with_key(
            "冲突密钥".into(),
            None,
            None,
            "custom-b".into(),
        );
        assert_ne!(conflicting.id, 0);

        mgr.sync_system_key("默认密钥".into(), None, "custom-b".into());

        assert_eq!(mgr.verify_and_touch("custom-a"), None);
        assert_eq!(mgr.verify_and_touch("custom-b"), Some(0));
        let entries = mgr.list();
        assert_eq!(entries.len(), 1);
        let system = &entries[0];
        assert_eq!(system.id, 0);
        assert!(system.is_system);
        assert_eq!(system.name, "保留名称");
        assert_eq!(system.description.as_deref(), Some("保留描述"));
        assert_eq!(system.group.as_deref(), Some("group-a"));
        assert!(!system.disabled);
        assert_eq!(system.total_input_tokens, 100);
        assert_eq!(system.total_output_tokens, 50);
    }

    #[test]
    fn system_key_cannot_be_deleted() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert!(!mgr.delete(0), "系统密钥 id=0 不可删除");
        assert!(mgr.is_system(0));
    }
}
