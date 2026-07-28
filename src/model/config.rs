use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 工具兼容模式。
///
/// - `ClaudeCode`（默认）：把 Claude Code 内置工具（Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch）
///   的工具名与入参双向适配为 Kiro 内置工具（fs_write/str_replace/... ），并替换为 Kiro 内置 schema。
/// - `Raw`：保留旧行为，直接透传客户端工具名/schema，用于排障。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCompatibilityMode {
    #[default]
    ClaudeCode,
    Raw,
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 工具兼容模式。默认 `claude-code`：把 Claude Code 内置工具名/入参双向适配为
    /// Kiro 内置工具；`raw` 保留旧行为、直接透传客户端工具 schema，用于排障。
    #[serde(default = "default_tool_compatibility_mode")]
    pub tool_compatibility_mode: ToolCompatibilityMode,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// Input token 膨胀倍率（应用于 input_tokens，>= 1.0）
    #[serde(default = "default_inflation_multiplier")]
    pub input_inflation_multiplier: f64,

    /// Output token 膨胀倍率（应用于 output_tokens，>= 1.0）
    #[serde(default = "default_inflation_multiplier")]
    pub output_inflation_multiplier: f64,

    /// Cache token 膨胀倍率（应用于 cache_creation + cache_read，>= 1.0）
    #[serde(default = "default_inflation_multiplier")]
    pub cache_inflation_multiplier: f64,

    /// rust 缓存模拟引擎参数（引擎 A，默认引擎）
    #[serde(default)]
    pub cache_engine_rust: CacheEngineRustConfig,

    /// go 缓存模拟引擎参数（引擎 B，移植自 kiro-go）
    #[serde(default)]
    pub cache_engine_go: CacheEngineGoConfig,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

/// rust 缓存模拟引擎（`anthropic::cache_metering`）参数。
///
/// 这些值原为源码内硬编码常量，提取为配置以便与 go 引擎独立调参。默认值与
/// 提取前的常量完全一致，故缺省配置行为不变。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEngineRustConfig {
    /// 条目上限，超出后按 `last_hit_at` 淘汰最旧
    #[serde(default = "default_rust_cache_capacity")]
    pub capacity: usize,
    /// 单条目最长 TTL（秒），与 Anthropic `ttl="1h"` 对齐
    #[serde(default = "default_rust_cache_max_ttl_secs")]
    pub max_ttl_secs: i64,
    /// 默认 TTL（秒），对应 ephemeral 缺省值
    #[serde(default = "default_rust_cache_default_ttl_secs")]
    pub default_ttl_secs: i64,
}

/// go 缓存模拟引擎（`anthropic::cache_metering_go`）参数。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEngineGoConfig {
    /// 可缓存前缀占总 input 的上限比例，保证总有未缓存尾部（0, 1]
    #[serde(default = "default_go_cache_max_ratio")]
    pub max_ratio: f64,
    /// 断点 TTL 上限（秒）。调小可让历史前缀更早过期，从而产出更多 creation
    #[serde(default = "default_go_cache_ttl_seconds")]
    pub ttl_seconds: i64,
    /// 条目上限（LRU）
    #[serde(default = "default_go_cache_max_entries")]
    pub max_entries: usize,
    /// 最小可缓存前缀 token 数，低于此值的断点不参与匹配 / 存储
    #[serde(default = "default_go_min_cacheable_tokens")]
    pub min_cacheable_tokens: i64,
    /// Opus 系列的最小可缓存前缀 token 数
    #[serde(default = "default_go_min_cacheable_tokens")]
    pub opus_min_cacheable_tokens: i64,
    /// 下发前对 `input_tokens` 的缩放倍率（go 引擎专属，不走全局膨胀倍率）
    #[serde(default = "default_go_multiplier")]
    pub input_token_multiplier: f64,
    /// 下发前对 `cache_read_input_tokens` 的缩放倍率（go 引擎专属）
    #[serde(default = "default_go_multiplier")]
    pub cache_read_multiplier: f64,
    /// 下发前对 `cache_creation_input_tokens` 的缩放倍率（go 引擎专属）。
    ///
    /// 默认 1.0（不缩放）= Go 原实现行为 —— Go 的 `buildClaudeUsageMap` 只缩放
    /// `input_tokens` 与 `cache_read_input_tokens`，creation 原样下发。
    ///
    /// 调离 1.0 会偏离 Go 原实现，且会影响两套引擎在「creation/read 划分」这个
    /// 维度上的可比性（数字差异将无法区分是引擎算法不同还是倍率不同造成的）。
    #[serde(default = "default_go_multiplier")]
    pub cache_creation_multiplier: f64,
}

fn default_rust_cache_capacity() -> usize {
    4096
}

fn default_rust_cache_max_ttl_secs() -> i64 {
    3600
}

fn default_rust_cache_default_ttl_secs() -> i64 {
    300
}

fn default_go_cache_max_ratio() -> f64 {
    0.85
}

fn default_go_cache_ttl_seconds() -> i64 {
    300
}

fn default_go_cache_max_entries() -> usize {
    131072
}

fn default_go_min_cacheable_tokens() -> i64 {
    1024
}

fn default_go_multiplier() -> f64 {
    1.0
}

impl Default for CacheEngineRustConfig {
    fn default() -> Self {
        Self {
            capacity: default_rust_cache_capacity(),
            max_ttl_secs: default_rust_cache_max_ttl_secs(),
            default_ttl_secs: default_rust_cache_default_ttl_secs(),
        }
    }
}

impl Default for CacheEngineGoConfig {
    fn default() -> Self {
        Self {
            max_ratio: default_go_cache_max_ratio(),
            ttl_seconds: default_go_cache_ttl_seconds(),
            max_entries: default_go_cache_max_entries(),
            min_cacheable_tokens: default_go_min_cacheable_tokens(),
            opus_min_cacheable_tokens: default_go_min_cacheable_tokens(),
            input_token_multiplier: default_go_multiplier(),
            cache_read_multiplier: default_go_multiplier(),
            cache_creation_multiplier: default_go_multiplier(),
        }
    }
}

impl CacheEngineRustConfig {
    /// 夹取到安全范围。0 / 负值一律回落默认，避免配置错误让缓存彻底失效。
    pub fn sanitized(self) -> Self {
        Self {
            capacity: if self.capacity == 0 {
                default_rust_cache_capacity()
            } else {
                self.capacity
            },
            max_ttl_secs: if self.max_ttl_secs <= 0 {
                default_rust_cache_max_ttl_secs()
            } else {
                self.max_ttl_secs
            },
            default_ttl_secs: if self.default_ttl_secs <= 0 {
                default_rust_cache_default_ttl_secs()
            } else {
                self.default_ttl_secs
            },
        }
    }
}

impl CacheEngineGoConfig {
    /// 夹取到安全范围（对齐 Go 侧 getter 的兜底逻辑）。
    pub fn sanitized(self) -> Self {
        Self {
            // 命中率调节旋钮。范围对齐 Go admin 端点的 0.5–0.99：低于 0.5 会让
            // 缓存几乎不起作用，等于 1.0 则整个 prompt 都可命中、下游永远看不到
            // 未缓存尾部（不真实）。越界一律回落默认。
            max_ratio: if self.max_ratio < 0.5 || self.max_ratio > 0.99 {
                default_go_cache_max_ratio()
            } else {
                self.max_ratio
            },
            ttl_seconds: if self.ttl_seconds <= 0 {
                default_go_cache_ttl_seconds()
            } else {
                self.ttl_seconds
            },
            // Go 侧有 256 下限，防止把缓存配到几乎不可用
            max_entries: self.max_entries.max(256),
            min_cacheable_tokens: self.min_cacheable_tokens.max(0),
            opus_min_cacheable_tokens: self.opus_min_cacheable_tokens.max(0),
            // Go 侧校验 `> 0`；NaN / inf 也一并挡掉，否则会污染整条下发口径。
            input_token_multiplier: sanitize_multiplier(self.input_token_multiplier),
            cache_read_multiplier: sanitize_multiplier(self.cache_read_multiplier),
            cache_creation_multiplier: sanitize_multiplier(self.cache_creation_multiplier),
        }
    }
}

fn sanitize_multiplier(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        v
    } else {
        default_go_multiplier()
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_tool_compatibility_mode() -> ToolCompatibilityMode {
    ToolCompatibilityMode::ClaudeCode
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_usage_log_retention_days() -> u32 {
    31
}

fn default_inflation_multiplier() -> f64 {
    1.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            extract_thinking: default_extract_thinking(),
            tool_compatibility_mode: default_tool_compatibility_mode(),
            default_endpoint: default_endpoint(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            endpoints: HashMap::new(),
            input_inflation_multiplier: default_inflation_multiplier(),
            output_inflation_multiplier: default_inflation_multiplier(),
            cache_inflation_multiplier: default_inflation_multiplier(),
            cache_engine_rust: CacheEngineRustConfig::default(),
            cache_engine_go: CacheEngineGoConfig::default(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod cache_engine_config_tests {
    use super::*;

    /// 默认值必须等于引擎 A 迁移前的硬编码常量。这条与 `cache_metering.rs` 里的
    /// `default_config_matches_legacy_constants` 互为两侧断言：任一侧改了默认值，
    /// 都会被判为**行为变更**而非无害重构。
    #[test]
    fn rust_engine_defaults_match_legacy_constants() {
        let c = CacheEngineRustConfig::default();
        assert_eq!(c.capacity, 4096);
        assert_eq!(c.max_ttl_secs, 3600);
        assert_eq!(c.default_ttl_secs, 300);
    }

    #[test]
    fn go_engine_defaults_match_go_source() {
        let c = CacheEngineGoConfig::default();
        assert_eq!(c.max_ratio, 0.85);
        assert_eq!(c.ttl_seconds, 300);
        assert_eq!(c.max_entries, 131072);
        assert_eq!(c.min_cacheable_tokens, 1024);
        // Go 侧两个常量当前同值，`minCacheableTokensForModel` 实为 no-op
        assert_eq!(c.opus_min_cacheable_tokens, 1024);
    }

    /// 老配置文件完全没有这两个键时，必须反序列化成默认值而非报错。
    #[test]
    fn legacy_config_without_cache_engine_keys_deserializes() {
        let json = r#"{"host":"127.0.0.1","port":8990,"region":"us-east-1"}"#;
        let cfg: Config = serde_json::from_str(json).expect("老配置应可解析");
        assert_eq!(cfg.cache_engine_rust.capacity, 4096);
        assert_eq!(cfg.cache_engine_go.max_ratio, 0.85);
    }

    /// 嵌套结构存在但字段缺失时，逐字段回落默认。
    #[test]
    fn partial_cache_engine_config_falls_back_per_field() {
        let json = r#"{
            "host":"127.0.0.1","port":8990,"region":"us-east-1",
            "cacheEngineGo": {"ttlSeconds": 60}
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.cache_engine_go.ttl_seconds, 60, "显式给的值应生效");
        assert_eq!(cfg.cache_engine_go.max_ratio, 0.85, "缺失字段应回落默认");
        assert_eq!(cfg.cache_engine_go.max_entries, 131072);
    }

    /// 非法值一律夹取回默认，避免误配把缓存彻底关掉（0 容量 = 永不命中）。
    #[test]
    fn sanitized_clamps_illegal_values() {
        let bad_rust = CacheEngineRustConfig {
            capacity: 0,
            max_ttl_secs: 0,
            default_ttl_secs: -5,
        }
        .sanitized();
        assert_eq!(bad_rust.capacity, 4096);
        assert_eq!(bad_rust.max_ttl_secs, 3600);
        assert_eq!(bad_rust.default_ttl_secs, 300);

        let bad_go = CacheEngineGoConfig {
            max_ratio: 5.0,
            ttl_seconds: 0,
            max_entries: 1,
            min_cacheable_tokens: -100,
            opus_min_cacheable_tokens: -1,
            input_token_multiplier: -3.0,
            cache_read_multiplier: 0.0,
            cache_creation_multiplier: f64::NAN,
        }
        .sanitized();
        assert_eq!(bad_go.max_ratio, 0.85, "比例 > 1 非法");
        assert_eq!(bad_go.ttl_seconds, 300);
        assert_eq!(bad_go.max_entries, 256, "容量有 256 下限");
        assert_eq!(bad_go.min_cacheable_tokens, 0, "负阈值夹到 0");
        assert_eq!(bad_go.opus_min_cacheable_tokens, 0);
    }

    /// 命中率旋钮的合法区间是 [0.5, 0.99]，对齐 Go admin 端点的校验。
    /// 两个端点值都必须被保留；越界（含 1.0）一律回落默认。
    #[test]
    fn ratio_range_matches_go_admin_validation() {
        let at = |r: f64| {
            CacheEngineGoConfig {
                max_ratio: r,
                ..CacheEngineGoConfig::default()
            }
            .sanitized()
            .max_ratio
        };
        assert_eq!(at(0.5), 0.5, "下界应保留");
        assert_eq!(at(0.99), 0.99, "上界应保留");
        assert_eq!(at(0.7), 0.7);
        // 1.0 意味着整个 prompt 都可命中、永无未缓存尾部 —— Go 也拒绝
        assert_eq!(at(1.0), 0.85, "1.0 越界应回落默认");
        assert_eq!(at(0.49), 0.85);
        assert_eq!(at(0.0), 0.85);
        assert_eq!(at(-1.0), 0.85);
    }

    /// go 引擎专属倍率：默认 1.0（不缩放），非法值回落。
    #[test]
    fn go_engine_multipliers_default_and_sanitize() {
        let d = CacheEngineGoConfig::default();
        assert_eq!(d.input_token_multiplier, 1.0);
        assert_eq!(d.cache_read_multiplier, 1.0);

        let ok = CacheEngineGoConfig {
            input_token_multiplier: 2.5,
            cache_read_multiplier: 0.3,
            ..CacheEngineGoConfig::default()
        }
        .sanitized();
        assert_eq!(ok.input_token_multiplier, 2.5, "合法倍率原样保留");
        assert_eq!(ok.cache_read_multiplier, 0.3, "允许 < 1 缩小");

        // Go 校验 `> 0`；NaN / inf 会污染整条下发口径，一并挡掉
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let c = CacheEngineGoConfig {
                input_token_multiplier: bad,
                cache_read_multiplier: bad,
                ..CacheEngineGoConfig::default()
            }
            .sanitized();
            assert_eq!(c.input_token_multiplier, 1.0, "非法值 {bad} 应回落");
            assert_eq!(c.cache_read_multiplier, 1.0, "非法值 {bad} 应回落");
        }
    }

    /// 合法值必须原样保留（`sanitized()` 不能顺手改动正常配置）。
    #[test]
    fn sanitized_preserves_legal_values() {
        let c = CacheEngineGoConfig {
            max_ratio: 0.5,
            ttl_seconds: 1800,
            max_entries: 4096,
            min_cacheable_tokens: 2048,
            opus_min_cacheable_tokens: 4096,
            input_token_multiplier: 1.5,
            cache_read_multiplier: 2.0,
            cache_creation_multiplier: 0.5,
        };
        let s = c.sanitized();
        // 幂等：再夹一次不应变化
        assert_eq!(s.sanitized().max_ratio, s.max_ratio);
        assert_eq!(s.sanitized().max_entries, s.max_entries);
        assert_eq!(s.max_ratio, 0.5);
        assert_eq!(s.ttl_seconds, 1800);
        assert_eq!(s.max_entries, 4096);
        assert_eq!(s.min_cacheable_tokens, 2048);
        assert_eq!(s.opus_min_cacheable_tokens, 4096);
        assert_eq!(s.input_token_multiplier, 1.5);
        assert_eq!(s.cache_read_multiplier, 2.0);
    }
}
