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

/// 自定义模型定义。
///
/// 用户在 `config.json` 的 `customModels` 数组里声明客户端模型别名到 Kiro 后端
/// 模型 ID 的映射及元数据。运行期由 [`crate::model::custom_models`] 全局注册表按
/// `id`（大小写不敏感）精确匹配，优先于内置的模糊映射逻辑——既能新增模型，也能
/// 覆盖内置模型的映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModel {
    /// 客户端请求时使用的模型名（别名）。匹配大小写不敏感。
    pub id: String,

    /// 映射到的 Kiro 后端模型 ID（实际下发给上游）。
    pub backend_id: String,

    /// `/v1/models` 展示名（可选，缺省用 `id`）。
    #[serde(default)]
    pub display_name: Option<String>,

    /// 上下文窗口大小（可选，缺省 200000）。
    #[serde(default)]
    pub context_window: Option<i32>,

    /// 单次响应最大 token 数，用于 `/v1/models` 展示（可选，缺省 64000）。
    #[serde(default)]
    pub max_tokens: Option<i32>,

    /// 是否支持原生 reasoning / `output_config`（可选，缺省 false）。
    /// 命中的自定义模型置 true 时，会按 backend_id 放行 `additionalModelRequestFields`。
    #[serde(default)]
    pub supports_reasoning: Option<bool>,

    /// `/v1/models` 的 `owned_by` 字段（可选，缺省 "custom"）。
    #[serde(default)]
    pub owned_by: Option<String>,
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

    /// 是否启用单账号每分钟请求次数（RPM）主动限流（默认 false）。
    ///
    /// 开启后：每个凭据独立维护最近 60 秒的滑动窗口计数，达到 `account_rpm_limit`
    /// 上限时，该凭据在窗口内被临时排除出候选，请求自动故障转移到下一个可用凭据；
    /// 所有凭据都超限时返回 429。窗口计数不持久化，进程重启后清空。
    /// 关闭时（默认）完全不计数、不影响调度，存量用户无感知。
    #[serde(default = "default_account_rpm_limit_enabled")]
    pub account_rpm_limit_enabled: bool,

    /// 单账号每分钟请求次数上限（默认 60）。仅在 `account_rpm_limit_enabled` 为 true 时生效。
    #[serde(default = "default_account_rpm_limit")]
    pub account_rpm_limit: u32,

    /// 是否识别 403 账号封禁文案并立即禁用凭据（默认 true）。
    ///
    /// 开启后：某凭据收到 403 且响应体命中明确封禁文案（同时含 "suspended" 与
    /// "locked your account"）时，立即标记为 `Suspended` 并禁用。这类凭据**不参与
    /// 自愈**，需人工联系客服核实后手动重置，从根上打断持续 403 死循环（issue #51）。
    ///
    /// 只匹配这两个高特异短语同时出现的情形，不影响普通 403（权限/WAF/区域抖动），
    /// 后者仍按既有 `report_failure` 累计路径处理。关闭后：完全回退旧行为。
    #[serde(default = "default_suspended_detection_enabled")]
    pub suspended_detection_enabled: bool,

    /// 是否启用凭据自愈（默认 true）。
    ///
    /// 当前请求的 model/group 作用域没有可用凭据时，只恢复该作用域内因
    /// `TooManyFailures` 被自动禁用且仍满足冷却/上限的凭据。
    #[serde(default = "default_self_heal_enabled")]
    pub self_heal_enabled: bool,

    /// 同一凭据两次自愈之间的最小冷却间隔（秒，默认 300 = 5 分钟）。
    ///
    /// 冷却窗口内即使再次全灭也不触发自愈。这是打断 issue #51「全禁 → 自愈 →
    /// 403 → 再禁」死循环的关键：持续故障时自愈频率被限到每 5 分钟一次，
    /// 而非每个请求都重置刷屏并无效打上游。
    #[serde(default = "default_self_heal_min_interval_secs")]
    pub self_heal_min_interval_secs: u64,

    /// 连续自愈的最大轮数（默认 5，`0` 表示不限）。
    ///
    /// 同一凭据连续自愈达到此值且同一模型期间没有成功调用时，停止自愈并记录
    /// 错误日志提示人工介入。其它凭据、分组或模型的成功不会清零该计数。
    #[serde(default = "default_self_heal_max_consecutive_rounds")]
    pub self_heal_max_consecutive_rounds: u32,

    /// 按凭据缓存上游可用模型列表的 TTL（秒，默认 3600）。
    #[serde(default = "default_model_cache_ttl_secs")]
    pub model_cache_ttl_secs: u64,

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

    /// real 引擎参数（引擎 C：上游真实 usage + 自己一组倍率，无缓存状态）
    #[serde(default)]
    pub cache_engine_real: CacheEngineRealConfig,

    /// nocache 引擎参数（引擎 D：cache 恒为 0、token 并入 input，无缓存状态）
    #[serde(default)]
    pub cache_engine_nocache: CacheEngineNoCacheConfig,
    /// Billing comparison prices and source multipliers.
    #[serde(default)]
    pub billing: BillingConfig,
    /// 自定义模型映射表。
    ///
    /// 每条把一个客户端模型别名映射到 Kiro 后端模型 ID 并附带元数据。默认空数组
    /// （完全向后兼容）。启动时装入 [`crate::model::custom_models`] 全局注册表，
    /// 供 `map_model` / `get_context_window_size` / `/v1/models` 查询。
    #[serde(default)]
    pub custom_models: Vec<CustomModel>,

    /// API Key 订阅源（拼车链接）自动导入配置。
    #[serde(default)]
    pub key_feed: KeyFeedConfig,

    /// 上游直通凭据的故障转移检测配置（欠费/封禁文案、429 冷却切号）。
    #[serde(default)]
    pub upstream_failover: UpstreamFailoverConfig,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

/// API Key 订阅源自动导入配置。
///
/// 面向「拼车链接」这类持续发号的 HTTP 源：定期 GET 一个返回 API Key 列表的
/// JSON 接口，把其中的新 key 自动导入为 `api_key` 类型凭据。
///
/// 去重完全依赖 `MultiTokenManager::add_credential` 已有的
/// `sha256(kiroApiKey)` 指纹（在任何网络调用之前就 bail），所以轮询天然幂等，
/// 无需本模块自己记账「哪些 key 已导入过」。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyFeedConfig {
    /// 是否启用后台轮询。关闭时调度器空转，手动拉取端点仍可用。
    #[serde(default)]
    pub enabled: bool,

    /// 订阅源 URL（不含 token 查询参数）。
    ///
    /// token 单独存在 [`Self::token`]，便于 Admin API 只回 `tokenSet` 布尔而不回显明文。
    #[serde(default)]
    pub url: Option<String>,

    /// 订阅源访问令牌，作为 `?{tokenParam}=` 拼到 [`Self::url`] 上。
    #[serde(default)]
    pub token: Option<String>,

    /// 令牌的查询参数名，默认 `token`。
    ///
    /// 不同订阅源的鉴权参数名不一样（`token` / `key` / `accessKey`…）。做成可配置是为了
    /// **让脱敏对任意源都成立**：只有参数名匹配，才能把令牌拆到 [`Self::token`] 单独存，
    /// Admin API 才能只回 `tokenSet` 而不回显明文。否则令牌会留在 `url` 里被接口回显。
    #[serde(default = "default_key_feed_token_param")]
    pub token_param: String,

    /// 轮询间隔（秒）。下限 60 秒，避免把上游发号接口打成限流。
    #[serde(default = "default_key_feed_interval_secs")]
    pub interval_secs: u64,

    /// 导入时是否验活（拉一次余额）。
    ///
    /// 建议保持 `true`：`supports_opus()` 在 `subscription_title` 为 `None` 时返回
    /// `true`，未验活的 Free key 会被放进 Opus 调度然后失败；验活顺带把订阅等级填上。
    #[serde(default = "default_key_feed_verify")]
    pub verify: bool,

    /// 导入的凭据优先级（数字越小越优先）。
    #[serde(default)]
    pub priority: u32,

    /// 导入的凭据所属分组。空数组表示不绑定分组。
    #[serde(default)]
    pub groups: Vec<String>,

    /// 导入的凭据使用的端点名。`None` 表示回退 `defaultEndpoint`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// 是否清理本订阅源导入且**已被自动禁用**的凭据。
    ///
    /// 默认关闭。开启后每轮拉取结束会删除满足全部条件的凭据：
    /// - `sourceChannel` 带本模块的 `keyfeed:` 前缀（不误伤手工添加的号）；
    /// - 已禁用，且禁用原因不是 `Manual`（手动禁用视为运维显式意图，保留）。
    ///
    /// 注意这里不按 `warrantyUntil` 到点删：保修期过了 key 未必失效，硬删会浪费
    /// 还能用的号。交给现有失败计数 / 自愈机制自然淘汰，这里只做善后。
    #[serde(default)]
    pub reap_disabled: bool,
}

impl Default for KeyFeedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            token: None,
            token_param: default_key_feed_token_param(),
            interval_secs: default_key_feed_interval_secs(),
            verify: default_key_feed_verify(),
            priority: 0,
            groups: Vec::new(),
            endpoint: None,
            reap_disabled: false,
        }
    }
}

/// 轮询间隔的绝对下限（秒）。防止误配成个位数秒把上游打挂。
pub const KEY_FEED_MIN_INTERVAL_SECS: u64 = 60;

fn default_key_feed_interval_secs() -> u64 {
    300
}

fn default_key_feed_verify() -> bool {
    true
}

/// 令牌查询参数名的默认值。
pub fn default_key_feed_token_param() -> String {
    "token".to_string()
}

impl KeyFeedConfig {
    /// 夹取到合法区间后的轮询间隔。
    pub fn effective_interval_secs(&self) -> u64 {
        self.interval_secs.max(KEY_FEED_MIN_INTERVAL_SECS)
    }

    /// 生效的令牌参数名（空配置回退到默认 `token`）。
    pub fn effective_token_param(&self) -> &str {
        let trimmed = self.token_param.trim();
        if trimmed.is_empty() {
            "token"
        } else {
            trimmed
        }
    }

    /// 拼出完整请求 URL（`url` + `?{tokenParam}=`）。
    ///
    /// 未配置 `url` 时返回 `None`。`token` 为空时只返回 base URL —— 有些源把凭证放在
    /// 路径里，或参数已经写在 `url` 上，不强制要求单独的 token。
    pub fn request_url(&self) -> Option<String> {
        let base = self.url.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
        let token = self
            .token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let Some(token) = token else {
            return Some(base.to_string());
        };

        // base 可能自带查询串（用户直接粘了带其它参数的链接）
        let separator = if base.contains('?') { '&' } else { '?' };
        Some(format!(
            "{}{}{}={}",
            base,
            separator,
            urlencoding::encode(self.effective_token_param()),
            urlencoding::encode(token)
        ))
    }
}

/// 上游直通凭据的故障转移检测配置。
///
/// 上游多为各家中转站，欠费 / 封禁的返回文案格式互不相同且随时会变，故检测
/// 规则做成**可配置的子串列表**（大小写不敏感、按字面匹配、不是正则）。
/// 所有规则只作用于上游凭据分支；Kiro 凭据路径有自己的一套检测，互不影响。
///
/// 三类动作与 Kiro 路径的机制一一对应，复用同一套凭据池管理：
/// - 封禁命中 → `report_suspended_for_request`：立即禁用、不参与自愈、切号；
/// - 欠费命中 → `report_quota_exhausted_for_request`：立即禁用并切号；
/// - 429 → `report_account_throttled_for_request`：冷却当前凭据并立即切号
///   （旧行为是在同一凭据上原地退避重试，池里有其它 key 时纯属浪费）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamFailoverConfig {
    /// 总开关（默认 true）。关闭后上游分支完全回退旧行为：
    /// 401/403 攒失败计数、429 原地退避、400 直接终止。
    #[serde(default = "default_upstream_failover_enabled")]
    pub enabled: bool,

    /// 欠费 / 额度用尽文案（子串，大小写不敏感）。
    ///
    /// 匹配范围：400 / 401 / 402 / 403 的响应体 —— 中转站表达"没钱了"的状态码
    /// 五花八门，Anthropic 官方是 400（"credit balance is too low"），不少
    /// OpenAI 风格网关用 403（"insufficient_quota"）。命中后立即禁用该凭据
    /// （`QuotaExceeded`）并切号。429 刻意不在此列：429 语义是限流，多为临时
    /// 状态，走下面的冷却切号；把它禁死会误杀还能恢复的 key。
    #[serde(default = "default_upstream_quota_patterns")]
    pub quota_patterns: Vec<String>,

    /// 封禁 / 停用文案（子串，大小写不敏感）。
    ///
    /// 匹配范围：401 / 403 的响应体。命中后立即禁用（`Suspended`，不参与自愈，
    /// 需人工在面板重置）并切号，不必等 401/403 攒满 3 次失败计数。
    #[serde(default = "default_upstream_suspended_patterns")]
    pub suspended_patterns: Vec<String>,

    /// 429 时是否冷却当前凭据并切号（默认 true）。
    ///
    /// 冷却时长优先取上游 `Retry-After` 头；没有时用
    /// [`Self::throttle_cooldown_secs`]。关闭后 429 回退旧行为（原地退避重试）。
    #[serde(default = "default_upstream_failover_enabled")]
    pub throttle_failover: bool,

    /// 429 无 `Retry-After` 时的冷却时长（秒，默认 60）。
    ///
    /// 比 Kiro 账号级风控的 30 分钟短得多：中转站的 429 多为短时限流而非风控，
    /// 长冷却会让池子无谓萎缩。
    #[serde(default = "default_upstream_throttle_cooldown_secs")]
    pub throttle_cooldown_secs: u64,
}

impl Default for UpstreamFailoverConfig {
    fn default() -> Self {
        Self {
            enabled: default_upstream_failover_enabled(),
            quota_patterns: default_upstream_quota_patterns(),
            suspended_patterns: default_upstream_suspended_patterns(),
            throttle_failover: default_upstream_failover_enabled(),
            throttle_cooldown_secs: default_upstream_throttle_cooldown_secs(),
        }
    }
}

impl UpstreamFailoverConfig {
    /// 响应体是否命中欠费文案。`enabled` 关闭时恒 false。
    pub fn matches_quota(&self, body: &str) -> bool {
        self.enabled && matches_any_pattern(&self.quota_patterns, body)
    }

    /// 响应体是否命中封禁文案。`enabled` 关闭时恒 false。
    pub fn matches_suspended(&self, body: &str) -> bool {
        self.enabled && matches_any_pattern(&self.suspended_patterns, body)
    }

    /// 429 是否走冷却切号。
    pub fn throttle_failover_enabled(&self) -> bool {
        self.enabled && self.throttle_failover
    }
}

/// 大小写不敏感的子串匹配。空 pattern（含纯空白）跳过，防止空串 `contains` 恒真
/// 把所有错误都误判成命中。
fn matches_any_pattern(patterns: &[String], body: &str) -> bool {
    if body.is_empty() {
        return false;
    }
    let body_lower = body.to_lowercase();
    patterns.iter().any(|p| {
        let p = p.trim();
        !p.is_empty() && body_lower.contains(&p.to_lowercase())
    })
}

fn default_upstream_failover_enabled() -> bool {
    true
}

fn default_upstream_throttle_cooldown_secs() -> u64 {
    60
}

/// 默认欠费文案：覆盖 Anthropic 官方、OpenAI 风格网关和常见中文中转站。
/// 全部取高特异性短语，避免误伤普通错误消息。
fn default_upstream_quota_patterns() -> Vec<String> {
    [
        // Anthropic 官方 / 兼容网关
        "credit balance is too low",
        // OpenAI 风格网关（one-api / new-api 系）
        "insufficient_quota",
        "insufficient quota",
        "quota exceeded",
        "exceeded your current quota",
        "余额不足",
        "额度已用尽",
        "额度不足",
        "欠费",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// 默认封禁文案。刻意不收录裸 "banned" / "disabled" 单词 —— 太宽，普通错误
/// 消息（如 "streaming is disabled"）会误命中。
fn default_upstream_suspended_patterns() -> Vec<String> {
    [
        "account has been disabled",
        "account has been suspended",
        "account is suspended",
        "user has been banned",
        "账号已被封禁",
        "账户已被禁用",
        "账号已被禁用",
        "已被封禁",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
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

    // ---- 引擎 A 专属倍率 ----
    //
    // **为什么是 `Option` 而非直接 `f64` 默认 1.0**：引擎 A 历史上一直用全局
    // 「Token 膨胀倍率」（`input_inflation_multiplier` 等三个顶层字段）。若这里直接
    // 默认 1.0，现有部署升级后 A 会突然从「全局配的 2×」变成「自己的 1×」——
    // 倍率静默失效，且配置文件里那三个全局字段看着还在生效。
    //
    // `None` = 未单独设置，回退全局；一旦从 UI 保存过就变成具体值并从此独立。
    /// input 倍率。`None` = 用全局 `input_inflation_multiplier`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_multiplier: Option<f64>,
    /// output 倍率。`None` = 用全局 `output_inflation_multiplier`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_multiplier: Option<f64>,
    /// cache_read 倍率。`None` = 用全局 `cache_inflation_multiplier`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_multiplier: Option<f64>,
    /// cache_creation 倍率。`None` = 用全局 `cache_inflation_multiplier`。
    ///
    /// 注意全局那套只有**一个** cache 倍率，creation 与 read 同值。拆成两个后
    /// A 才能像 B/C 那样独立调这两项。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_multiplier: Option<f64>,
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
    #[serde(default = "default_go_opus_min_cacheable_tokens")]
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
    /// 下发前对 `output_tokens` 的缩放倍率（go 引擎专属）。
    ///
    /// **Go 原实现没有这个倍率**（`buildClaudeUsageMap` 只缩放 input 与
    /// cache_read），故默认 1.0 = 原行为。开放它是为了让四套引擎在「可调维度」上
    /// 对称：否则 B 是唯一无法调 output 的引擎，运维要改 B 的 output 只能去动
    /// 全局倍率，而那会同时影响引擎 A。
    #[serde(default = "default_go_multiplier")]
    pub output_multiplier: f64,
}

/// 引擎 C（`real`）参数：上游真实 usage + 自己一组倍率。
///
/// 无缓存状态（无指纹表 / TTL / 容量），故只有倍率字段。四个倍率各自独立，
/// 与引擎 A 的全局膨胀倍率、引擎 B 的三倍率互不影响。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEngineRealConfig {
    /// 对上游真实 `input_tokens` 的缩放倍率
    #[serde(default = "default_go_multiplier")]
    pub input_multiplier: f64,
    /// 对上游真实 `output_tokens` 的缩放倍率
    #[serde(default = "default_go_multiplier")]
    pub output_multiplier: f64,
    /// 对上游真实 `cache_read_input_tokens` 的缩放倍率
    #[serde(default = "default_go_multiplier")]
    pub cache_read_multiplier: f64,
    /// 对上游真实 `cache_creation_input_tokens` 的缩放倍率
    #[serde(default = "default_go_multiplier")]
    pub cache_creation_multiplier: f64,
}

/// 引擎 D（`nocache`）参数：cache 字段恒为 0，token 并入 input。
///
/// 只有 input / output 两个倍率 —— cache 倍率对本引擎无意义（cache 恒为 0，
/// 乘任何倍率都是 0），故不提供，避免出现「配了却无效」的旋钮。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEngineNoCacheConfig {
    /// 对**客户端请求本地估算**的 input 的缩放倍率。
    ///
    /// 注意口径：引擎 D 不读上游 usage，input 取 `token::count_all_tokens` 对客户端
    /// 请求的本地估算值（请求发出前即已确定）。上游可能本身就是另一个 kiro-rs 反代，
    /// 它报的 usage 已被模拟 / 膨胀过一轮，拿来当"真值"会二次加工。
    #[serde(default = "default_go_multiplier")]
    pub input_multiplier: f64,
    /// 对**实际返回内容本地估算**的 output 的缩放倍率。
    ///
    /// 同样不读上游 usage：非流式从响应 `content` 数组估算，流式累积 SSE delta 后
    /// 整体估算一次（`count_tokens` 非线性，逐 delta 求和会显著高估）。
    #[serde(default = "default_go_multiplier")]
    pub output_multiplier: f64,
}

impl Default for CacheEngineRealConfig {
    fn default() -> Self {
        Self {
            input_multiplier: default_go_multiplier(),
            output_multiplier: default_go_multiplier(),
            cache_read_multiplier: default_go_multiplier(),
            cache_creation_multiplier: default_go_multiplier(),
        }
    }
}

impl Default for CacheEngineNoCacheConfig {
    fn default() -> Self {
        Self {
            input_multiplier: default_go_multiplier(),
            output_multiplier: default_go_multiplier(),
        }
    }
}

impl CacheEngineRealConfig {
    /// 非法倍率（<= 0 / NaN / inf）一律回落 1.0，与引擎 B 同规则。
    pub fn sanitized(self) -> Self {
        Self {
            input_multiplier: sanitize_multiplier(self.input_multiplier),
            output_multiplier: sanitize_multiplier(self.output_multiplier),
            cache_read_multiplier: sanitize_multiplier(self.cache_read_multiplier),
            cache_creation_multiplier: sanitize_multiplier(self.cache_creation_multiplier),
        }
    }
}

impl CacheEngineNoCacheConfig {
    /// 非法倍率（<= 0 / NaN / inf）一律回落 1.0，与引擎 B 同规则。
    pub fn sanitized(self) -> Self {
        Self {
            input_multiplier: sanitize_multiplier(self.input_multiplier),
            output_multiplier: sanitize_multiplier(self.output_multiplier),
        }
    }
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

/// Per-million-token model prices used by the billing comparison view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelPricing {
    #[serde(default)]
    pub input_per_million: f64,
    #[serde(default)]
    pub output_per_million: f64,
    #[serde(default)]
    pub cache_creation_per_million: f64,
    #[serde(default)]
    pub cache_read_per_million: f64,
}

impl Default for ModelPricing {
    fn default() -> Self {
        Self {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cache_creation_per_million: 0.0,
            cache_read_per_million: 0.0,
        }
    }
}

/// Billing comparison configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BillingConfig {
    #[serde(default)]
    pub model_prices: HashMap<String, ModelPricing>,
    #[serde(default)]
    pub upstream_multipliers: HashMap<u64, f64>,
    #[serde(default = "default_inflation_multiplier")]
    pub rust_multiplier: f64,
    #[serde(default = "default_inflation_multiplier")]
    pub go_multiplier: f64,
    /// 引擎 C（real）的计费对比调节系数。
    #[serde(default = "default_inflation_multiplier")]
    pub real_multiplier: f64,
    /// 引擎 D（nocache）的计费对比调节系数。
    #[serde(default = "default_inflation_multiplier")]
    pub nocache_multiplier: f64,
}

impl BillingConfig {
    /// 按引擎标识取计费对比的成本调节系数。
    ///
    /// 这**不是**下发给客户端的 token 膨胀倍率（那个已经作用在记录下来的 client
    /// usage 上了），而是计费对比视图专用的二次调节 —— 用于把不同引擎的账面成本
    /// 拉到同一口径下比较。未知引擎（历史 JSONL 里将来新增的名字）取 1.0，
    /// 使其成本仍按原样计入而不是被静默归零。
    pub fn engine_multiplier(&self, engine: &str) -> f64 {
        match engine {
            "rust" => self.rust_multiplier,
            "go" => self.go_multiplier,
            "real" => self.real_multiplier,
            "nocache" => self.nocache_multiplier,
            _ => 1.0,
        }
    }
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            model_prices: HashMap::new(),
            upstream_multipliers: HashMap::new(),
            rust_multiplier: 1.0,
            go_multiplier: 1.0,
            real_multiplier: 1.0,
            nocache_multiplier: 1.0,
        }
    }
}

fn default_go_opus_min_cacheable_tokens() -> i64 {
    // kiro-go's opusMinCacheableTokens is 1024.
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
            // None = 回退全局膨胀倍率。默认不填，使升级后行为与升级前完全一致。
            input_multiplier: None,
            output_multiplier: None,
            cache_read_multiplier: None,
            cache_creation_multiplier: None,
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
            opus_min_cacheable_tokens: default_go_opus_min_cacheable_tokens(),
            input_token_multiplier: default_go_multiplier(),
            output_multiplier: default_go_multiplier(),
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
            // 非法值回落 `None`（= 回退全局）而非 1.0：`None` 是本字段既有的
            // 「未设置」语义，回落到它可保持升级前的行为；回落 1.0 则会把已配的
            // 全局倍率静默抹成不缩放。
            input_multiplier: sanitize_opt_multiplier(self.input_multiplier),
            output_multiplier: sanitize_opt_multiplier(self.output_multiplier),
            cache_read_multiplier: sanitize_opt_multiplier(self.cache_read_multiplier),
            cache_creation_multiplier: sanitize_opt_multiplier(self.cache_creation_multiplier),
        }
    }
}

/// `Option<f64>` 倍率的夹取：`Some(非法)` → `None`，其余原样。
fn sanitize_opt_multiplier(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x > 0.0)
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
            output_multiplier: sanitize_multiplier(self.output_multiplier),
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

fn default_account_rpm_limit_enabled() -> bool {
    false
}

fn default_account_rpm_limit() -> u32 {
    60
}

fn default_suspended_detection_enabled() -> bool {
    true
}

fn default_self_heal_enabled() -> bool {
    true
}

fn default_self_heal_min_interval_secs() -> u64 {
    5 * 60
}

fn default_self_heal_max_consecutive_rounds() -> u32 {
    5
}

fn default_model_cache_ttl_secs() -> u64 {
    60 * 60
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
            account_rpm_limit_enabled: default_account_rpm_limit_enabled(),
            account_rpm_limit: default_account_rpm_limit(),
            suspended_detection_enabled: default_suspended_detection_enabled(),
            self_heal_enabled: default_self_heal_enabled(),
            self_heal_min_interval_secs: default_self_heal_min_interval_secs(),
            self_heal_max_consecutive_rounds: default_self_heal_max_consecutive_rounds(),
            model_cache_ttl_secs: default_model_cache_ttl_secs(),
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
            cache_engine_real: CacheEngineRealConfig::default(),
            cache_engine_nocache: CacheEngineNoCacheConfig::default(),
            billing: BillingConfig::default(),
            custom_models: Vec::new(),
            key_feed: KeyFeedConfig::default(),
            upstream_failover: UpstreamFailoverConfig::default(),
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
        assert_eq!(c.opus_min_cacheable_tokens, 1024);
    }

    /// 老配置文件完全没有这两个键时，必须反序列化成默认值而非报错。
    #[test]
    fn legacy_config_without_cache_engine_keys_deserializes() {
        let json = r#"{"host":"127.0.0.1","port":8990,"region":"us-east-1"}"#;
        let cfg: Config = serde_json::from_str(json).expect("老配置应可解析");
        assert_eq!(cfg.cache_engine_rust.capacity, 4096);
        assert_eq!(cfg.cache_engine_go.max_ratio, 0.85);
        // C / D 是后加的两段：老文件必然没有，且默认必须是 1.0（不缩放）。
        // 若默认不是 1.0，升级即静默改变所有既有 Key 的下发口径。
        assert_eq!(cfg.cache_engine_real.input_multiplier, 1.0);
        assert_eq!(cfg.cache_engine_real.output_multiplier, 1.0);
        assert_eq!(cfg.cache_engine_real.cache_read_multiplier, 1.0);
        assert_eq!(cfg.cache_engine_real.cache_creation_multiplier, 1.0);
        assert_eq!(cfg.cache_engine_nocache.input_multiplier, 1.0);
        assert_eq!(cfg.cache_engine_nocache.output_multiplier, 1.0);
    }

    /// C / D 的默认值：全 1.0。与「配置段缺失时行为不变」是同一条约束的两面 ——
    /// 上一条测反序列化，这条测结构体本身的 `Default`。
    #[test]
    fn stateless_engine_defaults_are_all_one() {
        let real = CacheEngineRealConfig::default();
        assert_eq!(
            (
                real.input_multiplier,
                real.output_multiplier,
                real.cache_read_multiplier,
                real.cache_creation_multiplier
            ),
            (1.0, 1.0, 1.0, 1.0)
        );
        let nocache = CacheEngineNoCacheConfig::default();
        assert_eq!(
            (nocache.input_multiplier, nocache.output_multiplier),
            (1.0, 1.0)
        );
    }

    /// C / D 的非法倍率必须夹回 1.0。
    ///
    /// 0 / 负数 / NaN / inf 都要挡：这四个值乘上去会分别得到"永远 0 token"、
    /// 负 token、NaN（序列化成 null）、inf（序列化成 null），全都会让客户端
    /// 拿到不可用的 usage。复用 `sanitize_multiplier`，与 go 引擎同一套判据。
    #[test]
    fn stateless_engine_sanitize_clamps_illegal_multipliers() {
        let real = CacheEngineRealConfig {
            input_multiplier: 0.0,
            output_multiplier: -2.0,
            cache_read_multiplier: f64::NAN,
            cache_creation_multiplier: f64::INFINITY,
        }
        .sanitized();
        assert_eq!(
            (
                real.input_multiplier,
                real.output_multiplier,
                real.cache_read_multiplier,
                real.cache_creation_multiplier
            ),
            (1.0, 1.0, 1.0, 1.0),
            "0 / 负 / NaN / inf 一律夹回 1.0"
        );

        let nocache = CacheEngineNoCacheConfig {
            input_multiplier: f64::NEG_INFINITY,
            output_multiplier: 0.0,
        }
        .sanitized();
        assert_eq!((nocache.input_multiplier, nocache.output_multiplier), (1.0, 1.0));

        // 合法值必须原样保留（含小于 1 的缩小倍率 —— 那是合法运营选择）。
        let ok = CacheEngineRealConfig {
            input_multiplier: 2.5,
            output_multiplier: 0.3,
            cache_read_multiplier: 10.0,
            cache_creation_multiplier: 0.75,
        }
        .sanitized();
        assert_eq!(
            (
                ok.input_multiplier,
                ok.output_multiplier,
                ok.cache_read_multiplier,
                ok.cache_creation_multiplier
            ),
            (2.5, 0.3, 10.0, 0.75),
            "合法值不得被改动"
        );
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
            ..Default::default()
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
            output_multiplier: f64::INFINITY,
            cache_read_multiplier: 0.0,
            cache_creation_multiplier: f64::NAN,
        }
        .sanitized();
        assert_eq!(bad_go.max_ratio, 0.85, "比例 > 1 非法");
        assert_eq!(bad_go.ttl_seconds, 300);
        assert_eq!(bad_go.max_entries, 256, "容量有 256 下限");
        assert_eq!(bad_go.min_cacheable_tokens, 0, "负阈值夹到 0");
        assert_eq!(bad_go.opus_min_cacheable_tokens, 0);
        // B 的倍率非法时回落 1.0（不是 None —— B 的字段不是 Option，
        // 它没有「回退全局」的语义，全局倍率从来不作用在 go 路径上）。
        assert_eq!(bad_go.input_token_multiplier, 1.0, "负倍率非法");
        assert_eq!(bad_go.output_multiplier, 1.0, "inf 非法");
        assert_eq!(bad_go.cache_read_multiplier, 1.0, "0 倍率非法");
        assert_eq!(bad_go.cache_creation_multiplier, 1.0, "NaN 非法");
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
            output_multiplier: 2.5,
            cache_read_multiplier: 2.0,
            cache_creation_multiplier: 0.5,
        };
        let s = c.sanitized();
        assert_eq!(s.output_multiplier, 2.5, "合法 output 倍率不得被改动");
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
        assert_eq!(s.cache_creation_multiplier, 0.5);
    }
}

#[cfg(test)]
mod upstream_failover_config_tests {
    use super::{Config, UpstreamFailoverConfig};

    /// 老 config.json（无该段）反序列化后必须整段取默认值，且默认启用。
    #[test]
    fn defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.upstream_failover.enabled);
        assert!(config.upstream_failover.throttle_failover);
        assert_eq!(config.upstream_failover.throttle_cooldown_secs, 60);
        assert!(!config.upstream_failover.quota_patterns.is_empty());
        assert!(!config.upstream_failover.suspended_patterns.is_empty());
    }

    /// 匹配是大小写不敏感的子串匹配，中英文都要能命中。
    #[test]
    fn pattern_matching_is_case_insensitive_substring() {
        let cfg = UpstreamFailoverConfig::default();
        assert!(cfg.matches_quota(
            r#"{"error":{"message":"Your Credit Balance Is Too Low to access the API."}}"#
        ));
        assert!(cfg.matches_quota(r#"{"message":"当前分组余额不足，请联系管理员"}"#));
        assert!(cfg.matches_suspended(
            r#"{"error":{"message":"Your account has been disabled."}}"#
        ));
        // 普通错误不得误命中
        assert!(!cfg.matches_quota(r#"{"message":"model not found"}"#));
        assert!(!cfg.matches_suspended(r#"{"message":"streaming is disabled for this model"}"#));
    }

    /// 空 pattern（含纯空白）必须被跳过：空串 `contains` 恒真，会把一切错误
    /// 都判成命中，等于把所有 4xx 的凭据全部禁用。
    #[test]
    fn blank_patterns_never_match() {
        let cfg = UpstreamFailoverConfig {
            quota_patterns: vec!["".to_string(), "   ".to_string()],
            ..UpstreamFailoverConfig::default()
        };
        assert!(!cfg.matches_quota(r#"{"message":"anything"}"#));
    }

    /// 总开关关闭后所有检测恒 false（完全回退旧行为）。
    #[test]
    fn disabled_flag_turns_everything_off() {
        let cfg = UpstreamFailoverConfig {
            enabled: false,
            ..UpstreamFailoverConfig::default()
        };
        assert!(!cfg.matches_quota("credit balance is too low"));
        assert!(!cfg.matches_suspended("account has been disabled"));
        assert!(!cfg.throttle_failover_enabled());
    }

    /// 用户自定义规则覆盖默认列表后按自定义生效。
    #[test]
    fn custom_patterns_replace_defaults() {
        let config: Config = serde_json::from_str(
            r#"{"upstreamFailover":{"quotaPatterns":["my custom broke message"],"throttleCooldownSecs":120}}"#,
        )
        .unwrap();
        let cfg = &config.upstream_failover;
        assert!(cfg.matches_quota("MY CUSTOM BROKE MESSAGE with details"));
        // 默认规则已被替换，不再命中
        assert!(!cfg.matches_quota("credit balance is too low"));
        assert_eq!(cfg.throttle_cooldown_secs, 120);
        // 未提供的字段仍取默认
        assert!(cfg.enabled);
        assert!(!cfg.suspended_patterns.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn model_cache_ttl_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.model_cache_ttl_secs, 3600);
        assert_eq!(Config::default().model_cache_ttl_secs, 3600);
    }

    #[test]
    fn model_cache_ttl_accepts_explicit_value() {
        let config: Config = serde_json::from_str(r#"{"modelCacheTtlSecs":120}"#).unwrap();
        assert_eq!(config.model_cache_ttl_secs, 120);
    }

    #[test]
    fn self_heal_config_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.suspended_detection_enabled);
        assert!(config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 300);
        assert_eq!(config.self_heal_max_consecutive_rounds, 5);

        let default = Config::default();
        assert!(default.suspended_detection_enabled);
        assert!(default.self_heal_enabled);
        assert_eq!(default.self_heal_min_interval_secs, 300);
        assert_eq!(default.self_heal_max_consecutive_rounds, 5);
    }

    #[test]
    fn self_heal_config_accepts_explicit_values() {
        let config: Config = serde_json::from_str(
            r#"{
                "suspendedDetectionEnabled": false,
                "selfHealEnabled": false,
                "selfHealMinIntervalSecs": 60,
                "selfHealMaxConsecutiveRounds": 0
            }"#,
        )
        .unwrap();
        assert!(!config.suspended_detection_enabled);
        assert!(!config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 60);
        assert_eq!(config.self_heal_max_consecutive_rounds, 0);
    }

    #[test]
    fn account_rpm_limit_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.account_rpm_limit_enabled);
        assert_eq!(config.account_rpm_limit, 60);

        let default = Config::default();
        assert!(!default.account_rpm_limit_enabled);
        assert_eq!(default.account_rpm_limit, 60);
    }

    #[test]
    fn account_rpm_limit_accepts_explicit_values() {
        let config: Config = serde_json::from_str(
            r#"{
                "accountRpmLimitEnabled": true,
                "accountRpmLimit": 120
            }"#,
        )
        .unwrap();
        assert!(config.account_rpm_limit_enabled);
        assert_eq!(config.account_rpm_limit, 120);
    }
}

