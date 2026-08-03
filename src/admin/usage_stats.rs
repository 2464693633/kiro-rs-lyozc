//! 请求用量记录 + 时序聚合
//!
//! 记录每次 `/v1/messages` 请求的 token 消耗与命中信息：
//! - 落盘：`usage_log.YYYY-MM-DD.jsonl`，每行一条 [`UsageRecord`]，按本地日期滚动
//! - 内存：[`UsageAggregator`] 维护近 31 天的小时桶 + 近 31 天的天桶，按需查询
//!
//! 启动时扫描历史 JSONL 文件重建聚合，保证重启后趋势图不丢数据。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// JSONL 文件保留天数
const RETENTION_DAYS: i64 = 31;
/// 小时桶数量（31 天）
const HOUR_BUCKETS: usize = 24 * 31;
/// 天桶数量（31 天）
const DAY_BUCKETS: usize = 31;

/// 单次请求的用量记录（与 JSONL 一行一一对应）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    /// 请求结束时间（RFC3339）
    pub ts: String,
    /// 客户端 Key id；0 表示用 master apiKey 调用
    pub key_id: u64,
    /// 实际命中的上游凭据 id；0 表示请求未走到上游
    pub credential_id: u64,
    /// 模型名（请求里声明的，可能含 -thinking 后缀）
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// 上游 meteringEvent.usage 上报的 credit 计费量（浮点）
    #[serde(default)]
    pub credits: f64,
    /// 端到端耗时（毫秒）
    #[serde(default)]
    pub duration_ms: u64,
    /// "success" 或 "error"
    pub status: String,
    /// True only for direct upstream Anthropic credentials.
    #[serde(default)]
    pub is_upstream: bool,
    /// 本次请求所用的缓存模拟引擎标识（`CacheEngineKind::as_str`）。
    /// `None` = v1 老记录（靠下面两个旧字段推断）。
    ///
    /// 存 `String` 而非 `CacheEngineKind`：这是**落盘**字段，会被后续版本的二进制读回。
    /// 若存枚举，将来新增引擎后、老二进制读到未知变体会 serde 失败，而摄入侧是
    /// `if let Ok(rec)` —— 整条记录被静默丢弃，连 token 数一起丢，不只是丢引擎标签。
    /// 读侧宽容、写侧仍由 `as_str()` 保证取值合法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// 上游真实用量 —— 这一次请求的真实成本口径。
    #[serde(default)]
    pub upstream_usage: Option<TokenUsageBreakdown>,
    /// 客户端被计费的用量（已乘该引擎的倍率）。
    ///
    /// 与 `upstream_usage` **同一次请求、一一对应**：两者相除即该引擎的加价倍数。
    /// v1 schema 把它按引擎分列成 `rust_usage` / `go_usage` 两个字段，导致
    /// 「上游真实」只有一个槽而模拟值有两个 —— 混合流量下 `upstream_usage` 累加的是
    /// 全部引擎之和，与单个引擎的模拟值不可比。C / D 更是无处安放。
    #[serde(default)]
    pub client_usage: Option<TokenUsageBreakdown>,
    /// v1 兼容字段：仅供读取老 JSONL，新记录一律写 `engine` + `client_usage`。
    /// 归一化由 [`UsageRecord::normalized`] 完成。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_usage: Option<TokenUsageBreakdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub go_usage: Option<TokenUsageBreakdown>,
}

impl UsageRecord {
    /// 把 v1 的 `rust_usage` / `go_usage` 折叠进 `engine` + `client_usage`。
    ///
    /// 老记录里「哪个引擎」是靠哪个字段非空隐式表达的，新 schema 改为显式。不做这层
    /// 转换的话，升级前的历史数据会在逐引擎对比表里整段消失。
    ///
    /// 已是 v2 的记录原样返回。
    pub fn normalized(mut self) -> Self {
        use crate::anthropic::cache_engine::CacheEngineKind;
        if self.client_usage.is_some() {
            return self;
        }
        if let Some(rust) = self.rust_usage.take() {
            self.engine = Some(CacheEngineKind::Rust.as_str().to_string());
            self.client_usage = Some(rust);
        } else if let Some(go) = self.go_usage.take() {
            self.engine = Some(CacheEngineKind::Go.as_str().to_string());
            self.client_usage = Some(go);
        }
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

impl TokenUsageBreakdown {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.cache_read_tokens)
    }
}

/// 按天 rotate 的 JSONL writer
pub struct UsageRecorder {
    inner: Mutex<RecorderState>,
    dir: PathBuf,
    /// 保留天数（运行时可改），cleanup_old_logs 时读取。
    retention_days: std::sync::atomic::AtomicI64,
}

struct RecorderState {
    /// 当前打开的 writer 与对应日期
    current_date: Option<NaiveDate>,
    writer: Option<BufWriter<File>>,
}

impl UsageRecorder {
    /// 指定初始保留天数构造
    pub fn with_retention(dir: PathBuf, retention_days: i64) -> Self {
        // 兜底：调用方传入空路径时归一为 "."，避免 join 出无目录前缀的路径导致写入 CWD
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!("创建 usage_log 目录失败 {}: {}", dir.display(), e);
            }
        }
        Self {
            inner: Mutex::new(RecorderState {
                current_date: None,
                writer: None,
            }),
            dir,
            retention_days: std::sync::atomic::AtomicI64::new(retention_days.max(1)),
        }
    }

    fn log_path(&self, date: NaiveDate) -> PathBuf {
        self.dir
            .join(format!("usage_log.{}.jsonl", date.format("%Y-%m-%d")))
    }

    /// 同步写入一条记录。失败仅 warn，不阻塞请求。
    pub fn record(&self, rec: &UsageRecord) {
        let line = match serde_json::to_string(rec) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("usage_log 序列化失败: {}", e);
                return;
            }
        };
        let today = Local::now().date_naive();
        let mut state = self.inner.lock();
        if state.current_date != Some(today) || state.writer.is_none() {
            // 切换到当日文件
            let path = self.log_path(today);
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    state.writer = Some(BufWriter::new(file));
                    state.current_date = Some(today);
                }
                Err(e) => {
                    tracing::warn!("打开 usage_log {} 失败: {}", path.display(), e);
                    return;
                }
            }
        }
        if let Some(w) = state.writer.as_mut() {
            if let Err(e) = writeln!(w, "{}", line) {
                tracing::warn!("写入 usage_log 失败: {}", e);
                return;
            }
            // 立即 flush，保证崩溃时不丢失最近一条
            let _ = w.flush();
        }
    }

    /// 获取保留天数
    pub fn retention_days(&self) -> i64 {
        self.retention_days
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置保留天数（>=1）
    pub fn set_retention_days(&self, days: i64) {
        self.retention_days
            .store(days.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// 清理超过保留期的旧文件
    pub fn cleanup_old_logs(&self) {
        let cutoff = Local::now().date_naive() - Duration::days(self.retention_days());
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(it) => it,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(date) = parse_usage_log_filename(&name) {
                if date < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                    tracing::info!("已清理过期 usage_log: {}", name);
                }
            }
        }
    }
}

fn parse_usage_log_filename(name: &str) -> Option<NaiveDate> {
    // 形如 usage_log.2026-05-22.jsonl
    let body = name.strip_prefix("usage_log.")?.strip_suffix(".jsonl")?;
    NaiveDate::parse_from_str(body, "%Y-%m-%d").ok()
}

/// 单个时间桶的统计
#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BucketStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub calls: u64,
    pub errors: u64,
    pub credits: f64,
}

impl BucketStats {
    fn add(&mut self, rec: &UsageRecord) {
        // saturating_add：debug 模式不 panic，release 模式不回绕（计费数据安全）
        self.input_tokens = self.input_tokens.saturating_add(rec.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(rec.output_tokens);
        self.cache_creation_tokens =
            self.cache_creation_tokens.saturating_add(rec.cache_creation_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(rec.cache_read_tokens);
        self.credits += rec.credits;
        self.calls = self.calls.saturating_add(1);
        if rec.status != "success" {
            self.errors = self.errors.saturating_add(1);
        }
    }

    /// 把另一个 stats 累加到自己上（用于 group 过滤后重新汇总）
    fn add_stats(&mut self, other: &BucketStats) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_tokens =
            self.cache_creation_tokens.saturating_add(other.cache_creation_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(other.cache_read_tokens);
        self.credits += other.credits;
        self.calls = self.calls.saturating_add(other.calls);
        self.errors = self.errors.saturating_add(other.errors);
    }
}

/// 逐引擎的「上游真实 / 客户端被计费」配对。
///
/// 两个口径**同一次请求同时记入**，故任意聚合层级上两者始终可比 —— 这是 v1 schema
/// 做不到的：那里 `upstream_usage` 是所有引擎共用的一个槽，混合流量下它累加的是全部
/// 引擎之和，而 `rust_usage` 只含 rust 流量，相除得出的"加价倍数"没有意义。
#[derive(Debug, Default, Clone, Copy)]
struct EngineBillingPair {
    /// 上游真实用量（真实成本口径）。
    upstream: TokenUsageBreakdown,
    /// 客户端被计费用量（已乘该引擎倍率）。
    client: TokenUsageBreakdown,
    /// 该引擎的上游请求数。
    calls: u64,
}

impl EngineBillingPair {
    fn add(&mut self, upstream: TokenUsageBreakdown, client: TokenUsageBreakdown) {
        add_usage(&mut self.upstream, upstream);
        add_usage(&mut self.client, client);
        self.calls = self.calls.saturating_add(1);
    }
}

/// 计费配对的聚合键。
///
/// 含 `key_id` 是为了支持按 Key 过滤（`query_billing` 的 `key_id` 参数）：不含它就得
/// 再维护一份按 Key 的副本，而带上它反而更省 —— 每个 Key 只用一个引擎，故实际条目数
/// 是 `Key 数 × 上游凭据数 × 模型数`，而非笛卡尔积。
///
/// `engine` 用 `String` 而非 `&'static str`：值来自 JSONL，可能是本二进制不认识的
/// 引擎名（降级运行）。见 [`UsageRecord::engine`]。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BillingKey {
    key_id: u64,
    credential_id: u64,
    model: String,
    engine: String,
}

/// 逐引擎费用小计（仅内部累加用；对外形状见 `types::EngineBillingPayload`）。
#[derive(Debug, Default, Clone, Copy)]
struct EngineCostRow {
    upstream_cost: f64,
    client_cost: f64,
    upstream_tokens: u64,
    client_tokens: u64,
    calls: u64,
}

impl EngineCostRow {
    fn merge(&mut self, other: &EngineCostRow) {
        self.upstream_cost += other.upstream_cost;
        self.client_cost += other.client_cost;
        self.upstream_tokens = self.upstream_tokens.saturating_add(other.upstream_tokens);
        self.client_tokens = self.client_tokens.saturating_add(other.client_tokens);
        self.calls = self.calls.saturating_add(other.calls);
    }
}

/// 转成 API payload，并按引擎名排序。
///
/// 排序不是洁癖：`HashMap` 迭代顺序每次进程都不同，不排的话前端图表的系列顺序
/// 会随刷新乱跳，颜色也跟着换。
fn engine_rows_to_payload(
    rows: &HashMap<String, EngineCostRow>,
) -> Vec<crate::admin::types::EngineBillingPayload> {
    let mut out: Vec<_> = rows
        .iter()
        .map(|(engine, row)| crate::admin::types::EngineBillingPayload {
            engine: engine.clone(),
            upstream_cost: row.upstream_cost,
            client_cost: row.client_cost,
            upstream_tokens: row.upstream_tokens,
            client_tokens: row.client_tokens,
            calls: row.calls,
        })
        .collect();
    out.sort_by(|a, b| a.engine.cmp(&b.engine));
    out
}

fn add_usage(dst: &mut TokenUsageBreakdown, src: TokenUsageBreakdown) {
    dst.input_tokens = dst.input_tokens.saturating_add(src.input_tokens);
    dst.output_tokens = dst.output_tokens.saturating_add(src.output_tokens);
    dst.cache_creation_tokens = dst.cache_creation_tokens.saturating_add(src.cache_creation_tokens);
    dst.cache_read_tokens = dst.cache_read_tokens.saturating_add(src.cache_read_tokens);
}

/// 单个时间桶含分组数据
#[derive(Debug, Default, Clone)]
struct BucketEntry {
    /// 桶起始时间戳（小时桶为整点 Unix 秒；天桶为本地 0 点 Unix 秒）
    ts: i64,
    overall: BucketStats,
    by_key: HashMap<u64, BucketStats>,
    by_model: HashMap<String, BucketStats>,
    by_credential: HashMap<u64, BucketStats>,
    by_key_model: HashMap<u64, HashMap<String, BucketStats>>,
    by_key_credential: HashMap<u64, HashMap<u64, BucketStats>>,
    by_credential_model: HashMap<u64, HashMap<String, BucketStats>>,
    by_key_credential_model: HashMap<u64, HashMap<u64, HashMap<String, BucketStats>>>,
    /// 逐引擎计费对：`(key, 引擎, 凭据, 模型) → (上游真实, 客户端被计费)`。
    ///
    /// **只收上游凭据的记录**（`is_upstream`），故实际条目数远小于 key 的笛卡尔积 ——
    /// 多数部署里上游账号只有一两个。这也是它独立于上面八张表的原因：那八张表每个
    /// 条目都会被复制一份 `BucketStats`，而计费只有 `query_billing` 读，塞进
    /// `BucketStats` 等于让另外七张表白背 100+ 字节。
    by_engine_billing: HashMap<BillingKey, EngineBillingPair>,
}

/// 时间维度聚合器
pub struct UsageAggregator {
    inner: parking_lot::RwLock<AggregatorInner>,
}

struct AggregatorInner {
    /// 小时桶（环形数组按桶起始时间索引），最近 31 天
    hour_buckets: Vec<BucketEntry>,
    /// 天桶（按本地日期），最近 31 天
    day_buckets: Vec<BucketEntry>,
}

/// 预设聚合查询时间范围
#[derive(Debug, Clone, Copy)]
pub enum Range {
    Last24h,
    Last7d,
    Last30d,
}

impl Range {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "24h" => Some(Range::Last24h),
            "7d" => Some(Range::Last7d),
            "30d" => Some(Range::Last30d),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsGranularity {
    Hour,
    Day,
}

impl StatsGranularity {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hour" => Some(StatsGranularity::Hour),
            "day" => Some(StatsGranularity::Day),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatsQueryWindow {
    pub start_ts: i64,
    pub end_ts: i64,
    pub granularity: StatsGranularity,
}

impl StatsQueryWindow {
    pub fn preset(range: Range, granularity: StatsGranularity) -> Self {
        let now = Utc::now().timestamp();
        let start_ts = match range {
            Range::Last24h => now - 24 * 3600,
            Range::Last7d => now - 7 * 24 * 3600,
            Range::Last30d => now - 30 * 24 * 3600,
        };
        Self {
            start_ts,
            end_ts: now,
            granularity,
        }
    }
}

/// 时序点（导出给前端）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSeriesPoint {
    /// 桶起始时间（RFC3339）
    pub ts: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub calls: u64,
    pub errors: u64,
    pub credits: f64,
}

/// 模型分布
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDistribution {
    pub model: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 上游凭据分布
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialDistribution {
    pub credential_id: u64,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub credits: f64,
    pub errors: u64,
}

/// 概览：今日 + 累计
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewStats {
    /// 今日（本地 0 点起）的调用次数
    pub today_calls: u64,
    pub today_input_tokens: u64,
    pub today_output_tokens: u64,
    pub today_errors: u64,
    pub today_credits: f64,
    /// 最近 7 天累计
    pub week_calls: u64,
    pub week_input_tokens: u64,
    pub week_output_tokens: u64,
    pub week_credits: f64,
}

impl UsageAggregator {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::RwLock::new(AggregatorInner {
                hour_buckets: Vec::new(),
                day_buckets: Vec::new(),
            }),
        }
    }

    /// 启动时从历史 JSONL 重建聚合
    pub fn rebuild_from_logs(&self, dir: &Path) {
        // 兜底：空路径归一为 "."，否则 read_dir("") 会失败导致重建为 0
        let dir_buf;
        let dir = if dir.as_os_str().is_empty() {
            dir_buf = PathBuf::from(".");
            dir_buf.as_path()
        } else {
            dir
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(it) => it,
            Err(e) => {
                tracing::warn!("读取 usage_log 目录失败 {}: {}", dir.display(), e);
                return;
            }
        };
        let cutoff = Local::now().date_naive() - Duration::days(RETENTION_DAYS);
        let mut count = 0u64;
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Some(date) = parse_usage_log_filename(&name) else {
                continue;
            };
            if date < cutoff {
                continue;
            }
            let file = match File::open(entry.path()) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(rec) = serde_json::from_str::<UsageRecord>(&line) {
                    // 必须 normalized()：v1 记录把「哪个引擎」隐式表达在
                    // rust_usage / go_usage 哪个非空上，不折叠成 engine +
                    // client_usage 的话，ingest 会因 engine 为 None 而跳过整条 ——
                    // 升级后历史数据在逐引擎对比表里整段消失（且无任何报错）。
                    self.ingest(&rec.normalized());
                    count += 1;
                }
            }
        }
        tracing::info!(
            "UsageAggregator 重建完成：从 {} 装载 {} 条历史记录",
            dir.display(),
            count
        );
    }

    /// 接收一条记录并落入对应桶
    pub fn ingest(&self, rec: &UsageRecord) {
        let dt: DateTime<Utc> = match DateTime::parse_from_rfc3339(&rec.ts) {
            Ok(d) => d.with_timezone(&Utc),
            Err(_) => Utc::now(),
        };
        let local = dt.with_timezone(&Local);

        // 小时桶起始：当地小时整点 → 转回 UTC unix 秒
        let hour_start = Local
            .with_ymd_and_hms(local.year(), local.month(), local.day(), local.hour(), 0, 0)
            .single();
        // 天桶起始：本地 0 点 → 转回 UTC unix 秒
        let day_start = Local
            .with_ymd_and_hms(local.year(), local.month(), local.day(), 0, 0, 0)
            .single();

        let hour_ts = hour_start.map(|d| d.timestamp()).unwrap_or(0);
        let day_ts = day_start.map(|d| d.timestamp()).unwrap_or(0);

        let mut inner = self.inner.write();

        upsert_bucket(&mut inner.hour_buckets, hour_ts, rec, HOUR_BUCKETS);
        upsert_bucket(&mut inner.day_buckets, day_ts, rec, DAY_BUCKETS);
    }

    /// 时序数据查询
    pub fn query_timeseries(
        &self,
        window: StatsQueryWindow,
        key_id: Option<u64>,
        cred_filter: Option<&std::collections::HashSet<u64>>,
    ) -> Vec<TimeSeriesPoint> {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);

        let mut points: Vec<TimeSeriesPoint> = buckets
            .iter()
            .filter(|b| bucket_in_window(b, window))
            .filter(|b| bucket_matches_key(b, key_id))
            .map(|b| {
                // 不带 group 过滤 → 走老逻辑（更快，命中预聚合 by_key/overall 桶）
                let stats = match cred_filter {
                    None => stats_for_key(b, key_id),
                    Some(allow) => credential_group_for_key(b, key_id)
                        .map(|group| {
                            let mut s = BucketStats::default();
                            for (cid, cs) in group {
                                if allow.contains(cid) {
                                    s.add_stats(cs);
                                }
                            }
                            s
                        })
                        .unwrap_or_default(),
                };
                TimeSeriesPoint {
                    ts: ts_to_rfc3339(b.ts),
                    input_tokens: stats.input_tokens,
                    output_tokens: stats.output_tokens,
                    cache_creation_tokens: stats.cache_creation_tokens,
                    cache_read_tokens: stats.cache_read_tokens,
                    calls: stats.calls,
                    errors: stats.errors,
                    credits: stats.credits,
                }
            })
            .collect();
        points.sort_by_key(|p| p.ts.clone());
        points
    }

    /// 模型分布
    pub fn query_by_model(
        &self,
        window: StatsQueryWindow,
        key_id: Option<u64>,
    ) -> Vec<ModelDistribution> {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);
        let mut acc: HashMap<String, BucketStats> = HashMap::new();
        for b in buckets.iter().filter(|b| bucket_in_window(b, window)) {
            let Some(group) = model_group_for_key(b, key_id) else {
                continue;
            };
            for (model, stats) in group {
                let entry = acc.entry(model.clone()).or_default();
                entry.input_tokens += stats.input_tokens;
                entry.output_tokens += stats.output_tokens;
                entry.calls += stats.calls;
            }
        }
        let mut out: Vec<ModelDistribution> = acc
            .into_iter()
            .map(|(model, stats)| ModelDistribution {
                model,
                calls: stats.calls,
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
            })
            .collect();
        out.sort_by(|a, b| b.calls.cmp(&a.calls));
        out
    }

    /// 上游凭据分布
    pub fn query_by_credential(
        &self,
        window: StatsQueryWindow,
        key_id: Option<u64>,
        cred_filter: Option<&std::collections::HashSet<u64>>,
    ) -> Vec<CredentialDistribution> {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);
        let mut acc: HashMap<u64, BucketStats> = HashMap::new();
        for b in buckets.iter().filter(|b| bucket_in_window(b, window)) {
            let Some(group) = credential_group_for_key(b, key_id) else {
                continue;
            };
            for (id, stats) in group {
                if let Some(allow) = cred_filter {
                    if !allow.contains(id) {
                        continue;
                    }
                }
                let entry = acc.entry(*id).or_default();
                entry.input_tokens += stats.input_tokens;
                entry.output_tokens += stats.output_tokens;
                entry.cache_creation_tokens += stats.cache_creation_tokens;
                entry.cache_read_tokens += stats.cache_read_tokens;
                entry.credits += stats.credits;
                entry.calls += stats.calls;
                entry.errors += stats.errors;
            }
        }
        let mut out: Vec<CredentialDistribution> = acc
            .into_iter()
            .map(|(id, stats)| CredentialDistribution {
                credential_id: id,
                calls: stats.calls,
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
                cache_creation_tokens: stats.cache_creation_tokens,
                cache_read_tokens: stats.cache_read_tokens,
                credits: stats.credits,
                errors: stats.errors,
            })
            .collect();
        out.sort_by(|a, b| b.calls.cmp(&a.calls));
        out
    }

    /// 逐引擎计费对比，按时间桶聚合。**只统计上游凭据的记录** —— 原生 Kiro 凭据
    /// 没有上游美元成本，计入会让「上游真实」这一列失去意义。
    ///
    /// 每个引擎独立成行，行内 `upstream_cost` 与 `client_cost` 来自**同一批请求**
    /// （见 [`EngineBillingPair`]），故两者相除得到的加价倍数是有意义的。v1 schema
    /// 做不到这点：那里「上游真实」是所有引擎共用的一个槽。
    pub fn query_billing(
        &self,
        window: StatsQueryWindow,
        key_id: Option<u64>,
        cred_filter: Option<&std::collections::HashSet<u64>>,
        config: &crate::model::config::BillingConfig,
    ) -> (
        Vec<crate::admin::types::BillingUsagePoint>,
        crate::admin::types::BillingComparisonResponse,
    ) {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);
        let mut points = Vec::new();
        // 全窗口的逐引擎累计，用于汇总行。
        let mut totals: HashMap<String, EngineCostRow> = HashMap::new();
        let mut total_upstream_cost = 0.0;
        let mut total_client_cost = 0.0;
        let mut total_calls = 0u64;

        for bucket in buckets.iter().filter(|b| bucket_in_window(b, window)) {
            // 本桶内的逐引擎小计。
            let mut per_engine: HashMap<String, EngineCostRow> = HashMap::new();
            let mut bucket_upstream_cost = 0.0;
            let mut bucket_client_cost = 0.0;
            let mut bucket_calls = 0u64;

            for (bk, pair) in &bucket.by_engine_billing {
                // 按 Key 过滤：None = 全部 Key 汇总。
                if let Some(id) = key_id {
                    if bk.key_id != id {
                        continue;
                    }
                }
                if let Some(allow) = cred_filter {
                    if !allow.contains(&bk.credential_id) {
                        continue;
                    }
                }
                let upstream_mul = config
                    .upstream_multipliers
                    .get(&bk.credential_id)
                    .copied()
                    .unwrap_or(1.0);
                // 客户端侧的引擎倍率：这是**计费对比专用**的成本调节系数，与下发给
                // 客户端的 token 膨胀倍率是两件事（后者已作用在 pair.client 上）。
                let client_mul = config.engine_multiplier(&bk.engine);

                let row = per_engine.entry(bk.engine.clone()).or_default();
                row.upstream_tokens += pair.upstream.total_tokens();
                row.client_tokens += pair.client.total_tokens();
                row.calls += pair.calls;
                bucket_calls += pair.calls;
                if let Some(price) = config.model_prices.get(&bk.model) {
                    let up = cost(pair.upstream, price, upstream_mul);
                    let cl = cost(pair.client, price, client_mul);
                    row.upstream_cost += up;
                    row.client_cost += cl;
                    bucket_upstream_cost += up;
                    bucket_client_cost += cl;
                }
            }

            // 累进全窗口汇总。
            for (engine, row) in &per_engine {
                totals.entry(engine.clone()).or_default().merge(row);
            }
            total_upstream_cost += bucket_upstream_cost;
            total_client_cost += bucket_client_cost;
            total_calls += bucket_calls;

            points.push(crate::admin::types::BillingUsagePoint {
                ts: ts_to_rfc3339(bucket.ts),
                upstream_cost: bucket_upstream_cost,
                client_cost: bucket_client_cost,
                calls: bucket_calls,
                engines: engine_rows_to_payload(&per_engine),
            });
        }

        let summary = crate::admin::types::BillingComparisonResponse {
            points: points.clone(),
            upstream_cost: total_upstream_cost,
            client_cost: total_client_cost,
            calls: total_calls,
            engines: engine_rows_to_payload(&totals),
        };
        (points, summary)
    }

    /// 旧签名占位，见下方 legacy 标记。
    #[allow(dead_code)]

    /// 概览（今日 + 最近 7 天）
    pub fn overview(&self) -> OverviewStats {
        let inner = self.inner.read();
        let today_start = Local
            .with_ymd_and_hms(
                Local::now().year(),
                Local::now().month(),
                Local::now().day(),
                0,
                0,
                0,
            )
            .single()
            .map(|d| d.timestamp())
            .unwrap_or(0);

        let mut today = BucketStats::default();
        for b in inner.hour_buckets.iter().filter(|b| b.ts >= today_start) {
            today.input_tokens += b.overall.input_tokens;
            today.output_tokens += b.overall.output_tokens;
            today.calls += b.overall.calls;
            today.errors += b.overall.errors;
            today.credits += b.overall.credits;
        }

        let week_cutoff = Utc::now().timestamp() - 7 * 24 * 3600;
        let mut week = BucketStats::default();
        for b in inner.hour_buckets.iter().filter(|b| b.ts >= week_cutoff) {
            week.input_tokens += b.overall.input_tokens;
            week.output_tokens += b.overall.output_tokens;
            week.calls += b.overall.calls;
            week.credits += b.overall.credits;
        }

        OverviewStats {
            today_calls: today.calls,
            today_input_tokens: today.input_tokens,
            today_output_tokens: today.output_tokens,
            today_errors: today.errors,
            today_credits: today.credits,
            week_calls: week.calls,
            week_input_tokens: week.input_tokens,
            week_output_tokens: week.output_tokens,
            week_credits: week.credits,
        }
    }
}

impl Default for UsageAggregator {
    fn default() -> Self {
        Self::new()
    }
}

/// 把记录写入对应桶；不存在则二分查找插入位置（保持升序），超过容量时移除最旧的。
/// 原线性扫描+全量排序改为 binary_search_by_key，O(log n) 查找 + O(n) 插入，
/// 去掉了每次插入都重新排序的 O(n log n) 开销。
fn upsert_bucket(buckets: &mut Vec<BucketEntry>, ts: i64, rec: &UsageRecord, max: usize) {
    match buckets.binary_search_by_key(&ts, |b| b.ts) {
        Ok(idx) => {
            add_record_to_bucket(&mut buckets[idx], rec);
        }
        Err(idx) => {
            let mut entry = BucketEntry { ts, ..Default::default() };
            add_record_to_bucket(&mut entry, rec);
            buckets.insert(idx, entry);
            while buckets.len() > max {
                buckets.remove(0);
            }
        }
    }
}

fn add_record_to_bucket(bucket: &mut BucketEntry, rec: &UsageRecord) {
    bucket.overall.add(rec);
    bucket.by_key.entry(rec.key_id).or_default().add(rec);
    bucket
        .by_model
        .entry(rec.model.clone())
        .or_default()
        .add(rec);
    bucket
        .by_key_model
        .entry(rec.key_id)
        .or_default()
        .entry(rec.model.clone())
        .or_default()
        .add(rec);
    if rec.credential_id == 0 {
        return;
    }
    bucket
        .by_credential
        .entry(rec.credential_id)
        .or_default()
        .add(rec);
    bucket
        .by_key_credential
        .entry(rec.key_id)
        .or_default()
        .entry(rec.credential_id)
        .or_default()
        .add(rec);
    bucket
        .by_credential_model
        .entry(rec.credential_id)
        .or_default()
        .entry(rec.model.clone())
        .or_default()
        .add(rec);
    bucket
        .by_key_credential_model
        .entry(rec.key_id)
        .or_default()
        .entry(rec.credential_id)
        .or_default()
        .entry(rec.model.clone())
        .or_default()
        .add(rec);

    // 逐引擎计费：只有上游凭据有真实美元成本，Kiro 凭据不参与（与 query_billing
    // 既有语义一致）。无 engine 标记的记录也跳过 —— 那是 v1 里 A/B 之外的路径，
    // 归不到任何引擎，硬塞进某一栏会污染对比。
    if rec.is_upstream {
        if let Some(engine) = rec.engine.as_deref() {
            bucket
                .by_engine_billing
                .entry(BillingKey {
                    key_id: rec.key_id,
                    engine: engine.to_string(),
                    credential_id: rec.credential_id,
                    model: rec.model.clone(),
                })
                .or_default()
                .add(
                    rec.upstream_usage.unwrap_or_default(),
                    rec.client_usage.unwrap_or_default(),
                );
        }
    }
}

fn cost(
    usage: TokenUsageBreakdown,
    price: &crate::model::config::ModelPricing,
    multiplier: f64,
) -> f64 {
    let raw = usage.input_tokens as f64 * price.input_per_million
        + usage.output_tokens as f64 * price.output_per_million
        + usage.cache_creation_tokens as f64 * price.cache_creation_per_million
        + usage.cache_read_tokens as f64 * price.cache_read_per_million;
    raw / 1_000_000.0 * multiplier
}

fn bucket_matches_key(bucket: &BucketEntry, key_id: Option<u64>) -> bool {
    key_id
        .map(|id| bucket.by_key.contains_key(&id))
        .unwrap_or(true)
}

fn credential_group_for_key(
    bucket: &BucketEntry,
    key_id: Option<u64>,
) -> Option<&HashMap<u64, BucketStats>> {
    match key_id {
        Some(id) => bucket.by_key_credential.get(&id),
        None => Some(&bucket.by_credential),
    }
}

fn model_group_for_key(
    bucket: &BucketEntry,
    key_id: Option<u64>,
) -> Option<&HashMap<String, BucketStats>> {
    match key_id {
        Some(id) => bucket.by_key_model.get(&id),
        None => Some(&bucket.by_model),
    }
}

fn bucket_in_window(bucket: &BucketEntry, window: StatsQueryWindow) -> bool {
    bucket.ts >= window.start_ts && bucket.ts < window.end_ts
}

fn select_buckets(inner: &AggregatorInner, granularity: StatsGranularity) -> &[BucketEntry] {
    match granularity {
        StatsGranularity::Hour => &inner.hour_buckets,
        StatsGranularity::Day => &inner.day_buckets,
    }
}

fn stats_for_key(bucket: &BucketEntry, key_id: Option<u64>) -> BucketStats {
    match key_id {
        Some(id) => bucket.by_key.get(&id).copied().unwrap_or_default(),
        None => bucket.overall,
    }
}

fn ts_to_rfc3339(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

pub type SharedRecorder = Arc<UsageRecorder>;
pub type SharedAggregator = Arc<UsageAggregator>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_log_filename() {
        assert!(parse_usage_log_filename("usage_log.2026-05-22.jsonl").is_some());
        assert!(parse_usage_log_filename("foo.bar").is_none());
    }

    #[test]
    fn aggregator_basic_ingest_and_overview() {
        let agg = UsageAggregator::new();
        let now = Utc::now();
        let rec = UsageRecord {
            ts: now.to_rfc3339(),
            key_id: 1,
            credential_id: 5,
            model: "claude-opus-4-7".to_string(),
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.05,
            duration_ms: 1500,
            status: "success".to_string(),
            is_upstream: false,
            upstream_usage: None,
            engine: None,
            client_usage: None,
            rust_usage: None,
            go_usage: None,
        };
        agg.ingest(&rec);
        agg.ingest(&rec);

        let ov = agg.overview();
        assert_eq!(ov.today_calls, 2);
        assert_eq!(ov.today_input_tokens, 2000);

        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);
        let series = agg.query_timeseries(window, None, None);
        assert!(!series.is_empty());

        let by_model = agg.query_by_model(window, None);
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].model, "claude-opus-4-7");
        assert_eq!(by_model[0].calls, 2);

        let by_cred = agg.query_by_credential(window, None, None);
        assert_eq!(by_cred.len(), 1);
        assert_eq!(by_cred[0].credential_id, 5);
    }

    #[test]
    fn aggregator_filters_by_client_key() {
        let agg = UsageAggregator::new();
        let now = Utc::now().to_rfc3339();
        let rec_a = UsageRecord {
            ts: now.clone(),
            key_id: 1,
            credential_id: 5,
            model: "m-a".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.01,
            duration_ms: 100,
            status: "success".to_string(),
            is_upstream: false,
            upstream_usage: None,
            engine: None,
            client_usage: None,
            rust_usage: None,
            go_usage: None,
        };
        let rec_b = UsageRecord {
            ts: now,
            key_id: 2,
            credential_id: 6,
            model: "m-b".to_string(),
            input_tokens: 300,
            output_tokens: 40,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.02,
            duration_ms: 200,
            status: "error".to_string(),
            is_upstream: false,
            upstream_usage: None,
            engine: None,
            client_usage: None,
            rust_usage: None,
            go_usage: None,
        };
        agg.ingest(&rec_a);
        agg.ingest(&rec_b);

        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);
        let series = agg.query_timeseries(window, Some(1), None);
        assert_eq!(series.iter().map(|p| p.calls).sum::<u64>(), 1);
        assert_eq!(series.iter().map(|p| p.input_tokens).sum::<u64>(), 100);

        let by_model = agg.query_by_model(window, Some(1));
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].model, "m-a");

        let by_cred = agg.query_by_credential(window, Some(1), None);
        assert_eq!(by_cred.len(), 1);
        assert_eq!(by_cred[0].credential_id, 5);
    }

    #[test]
    fn aggregator_filters_by_custom_window_and_granularity() {
        let agg = UsageAggregator::new();
        let today = Local::now().date_naive();
        let yesterday = today - Duration::days(1);
        let yesterday_noon = Local
            .with_ymd_and_hms(
                yesterday.year(),
                yesterday.month(),
                yesterday.day(),
                12,
                0,
                0,
            )
            .single()
            .unwrap()
            .with_timezone(&Utc)
            .to_rfc3339();
        let today_noon = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 12, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
            .to_rfc3339();
        let rec_yesterday = UsageRecord {
            ts: yesterday_noon,
            key_id: 0,
            credential_id: 5,
            model: "m-yesterday".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.01,
            duration_ms: 100,
            status: "success".to_string(),
            is_upstream: false,
            upstream_usage: None,
            engine: None,
            client_usage: None,
            rust_usage: None,
            go_usage: None,
        };
        let rec_today = UsageRecord {
            ts: today_noon,
            key_id: 0,
            credential_id: 5,
            model: "m-today".to_string(),
            input_tokens: 300,
            output_tokens: 40,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.02,
            duration_ms: 100,
            status: "success".to_string(),
            is_upstream: false,
            upstream_usage: None,
            engine: None,
            client_usage: None,
            rust_usage: None,
            go_usage: None,
        };
        agg.ingest(&rec_yesterday);
        agg.ingest(&rec_today);

        let start_ts = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 0, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let end_ts = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 23, 59, 59)
            .single()
            .unwrap()
            .timestamp();
        let hour_window = StatsQueryWindow {
            start_ts,
            end_ts,
            granularity: StatsGranularity::Hour,
        };
        let day_window = StatsQueryWindow {
            start_ts,
            end_ts,
            granularity: StatsGranularity::Day,
        };

        let hourly = agg.query_timeseries(hour_window, None, None);
        assert_eq!(hourly.iter().map(|p| p.calls).sum::<u64>(), 1);
        assert_eq!(hourly.iter().map(|p| p.input_tokens).sum::<u64>(), 300);

        let daily = agg.query_timeseries(day_window, None, None);
        assert_eq!(daily.iter().map(|p| p.calls).sum::<u64>(), 1);
        assert_eq!(daily.iter().map(|p| p.output_tokens).sum::<u64>(), 40);
    }

    #[test]
    fn error_record_increments_errors() {
        let agg = UsageAggregator::new();
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            credential_id: 0,
            model: "claude-opus-4-7".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.0,
            duration_ms: 100,
            status: "error".to_string(),
            is_upstream: false,
            upstream_usage: None,
            engine: None,
            client_usage: None,
            rust_usage: None,
            go_usage: None,
        };
        agg.ingest(&rec);
        let ov = agg.overview();
        assert_eq!(ov.today_errors, 1);
    }

    /// 计费测试用的基础记录：1M 上游 input、2M 客户端 input，走 rust 引擎。
    fn billing_rec(engine: &str, client_input: u64) -> UsageRecord {
        UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: 1,
            credential_id: 9,
            model: "m".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            credits: 0.0,
            duration_ms: 0,
            status: "success".to_string(),
            is_upstream: true,
            engine: Some(engine.to_string()),
            upstream_usage: Some(TokenUsageBreakdown {
                input_tokens: 1_000_000,
                ..Default::default()
            }),
            client_usage: Some(TokenUsageBreakdown {
                input_tokens: client_input,
                ..Default::default()
            }),
            rust_usage: None,
            go_usage: None,
        }
    }

    fn billing_config() -> crate::model::config::BillingConfig {
        let mut config = crate::model::config::BillingConfig::default();
        config.model_prices.insert(
            "m".to_string(),
            crate::model::config::ModelPricing {
                input_per_million: 5.0,
                ..Default::default()
            },
        );
        config.upstream_multipliers.insert(9, 2.0);
        config.rust_multiplier = 1.5;
        config.go_multiplier = 0.5;
        config
    }

    fn engine_row<'a>(
        resp: &'a crate::admin::types::BillingComparisonResponse,
        engine: &str,
    ) -> &'a crate::admin::types::EngineBillingPayload {
        resp.engines
            .iter()
            .find(|e| e.engine == engine)
            .unwrap_or_else(|| panic!("应有 {engine} 行"))
    }

    #[test]
    fn billing_query_pairs_upstream_with_client_per_engine() {
        let agg = UsageAggregator::new();
        agg.ingest(&billing_rec("rust", 2_000_000));
        let config = billing_config();
        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);

        let (_, result) = agg.query_billing(window, None, None, &config);
        // 上游 1M × $5/M × 凭据倍率 2.0 = $10；客户端 2M × $5/M × rust 倍率 1.5 = $15
        assert_eq!(result.upstream_cost, 10.0);
        assert_eq!(result.client_cost, 15.0);
        assert_eq!(result.calls, 1);

        let rust = engine_row(&result, "rust");
        assert_eq!(rust.upstream_cost, 10.0, "引擎行必须自带上游成本");
        assert_eq!(rust.client_cost, 15.0);
        assert_eq!(rust.calls, 1);
        assert_eq!(result.engines.len(), 1, "未使用的引擎不该出现");
    }

    /// `engine_multiplier` 与 token 膨胀倍率**刻意叠乘**，不是重复相乘的 bug。
    ///
    /// 两者是两个不同的旋钮，作用在两个不同的层：
    /// - token 膨胀倍率（`cacheEngine*.{input,output,...}Multiplier`）在**请求路径**
    ///   上作用，改的是下发给客户端的 usage 数字，已经烙进记录下来的 `client_usage`。
    /// - `billing.<engine>Multiplier` 只在**计费对比视图**上作用，改的是账面单价，
    ///   不回头改 token。
    ///
    /// 故一条 1M 上游 / 2M 客户端（2× 已在请求路径乘过）的记录，配 `rust_multiplier
    /// = 1.5` 时客户端成本相对上游是 3×。这看着像"乘了两次"，但两次乘的不是同一个
    /// 量：第一次乘 token，第二次乘钱。
    ///
    /// 本测试同时钉住 `client_tokens` 在两种配置下都是 2M —— 若哪天有人把
    /// `engine_multiplier` 改成也缩放 token（即"合并成一个旋钮"），token 断言先炸，
    /// 使这次口径变更不可能静默发生。
    #[test]
    fn engine_multiplier_scales_cost_only_and_stacks_on_token_multiplier() {
        let agg = UsageAggregator::new();
        // client_input = 2M 表示请求路径已按 2× 膨胀过（上游只有 1M）。
        agg.ingest(&billing_rec("rust", 2_000_000));
        let window = || StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);

        // 计费系数取 1.0：客户端成本只反映 token 膨胀，2M × $5/M = $10。
        let mut config = billing_config();
        config.rust_multiplier = 1.0;
        let (_, neutral) = agg.query_billing(window(), None, None, &config);
        assert_eq!(neutral.client_cost, 10.0, "系数 1.0 时只剩 token 膨胀那一层");

        // 计费系数取 1.5：在已膨胀的 token 之上再调账面单价，$10 × 1.5 = $15。
        config.rust_multiplier = 1.5;
        let (_, scaled) = agg.query_billing(window(), None, None, &config);
        assert_eq!(scaled.client_cost, 15.0, "计费系数叠在 token 膨胀之上");

        // 两种配置下 token 数完全相同：计费系数不碰 token。
        assert_eq!(engine_row(&neutral, "rust").client_tokens, 2_000_000);
        assert_eq!(
            engine_row(&scaled, "rust").client_tokens,
            2_000_000,
            "engine_multiplier 只作用于成本，不得改动 token 口径"
        );
    }

    /// **v1 schema 的核心缺陷**：`upstream_usage` 是所有引擎共用的一个槽，混合流量下
    /// 它累加的是「A 请求的上游 + B 请求的上游」，而 `rust_usage` 只含 A 的客户端计费。
    /// 拿 `rust_cost / upstream_cost` 得到的"加价倍数"分母里混着 B 的成本，无意义。
    ///
    /// 新 schema 逐引擎配对存储，故每个引擎行的上游成本**只含该引擎自己的请求**。
    #[test]
    fn each_engine_row_carries_only_its_own_upstream_cost() {
        let agg = UsageAggregator::new();
        agg.ingest(&billing_rec("rust", 2_000_000));
        agg.ingest(&billing_rec("go", 3_000_000));
        let config = billing_config();
        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);
        let (_, result) = agg.query_billing(window, None, None, &config);

        // 两条请求各自 1M 上游 → 汇总 $20。这个数 v1 也对。
        assert_eq!(result.upstream_cost, 20.0);
        assert_eq!(result.calls, 2);

        // 但逐引擎行必须各自只有 $10 —— v1 无法表达这一点。
        let rust = engine_row(&result, "rust");
        let go = engine_row(&result, "go");
        assert_eq!(
            rust.upstream_cost, 10.0,
            "rust 行的上游成本不得含 go 请求的成本（v1 此处为 20.0）"
        );
        assert_eq!(
            go.upstream_cost, 10.0,
            "go 行的上游成本不得含 rust 请求的成本"
        );
        assert_eq!(rust.client_cost, 15.0, "2M × $5/M × 1.5");
        assert_eq!(go.client_cost, 7.5, "3M × $5/M × 0.5");

        // 一一对应的意义：比值只在配对存储下才成立。
        assert_eq!(rust.client_cost / rust.upstream_cost, 1.5);
        assert_eq!(go.client_cost / go.upstream_cost, 0.75);
    }

    /// 引擎 C / D 必须与 A / B 一样进对比表 —— 这是四引擎改造的目的。
    #[test]
    fn stateless_engines_appear_in_billing_comparison() {
        let agg = UsageAggregator::new();
        agg.ingest(&billing_rec("real", 1_000_000));
        agg.ingest(&billing_rec("nocache", 4_000_000));
        let mut config = billing_config();
        config.real_multiplier = 2.0;
        config.nocache_multiplier = 3.0;
        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);
        let (_, result) = agg.query_billing(window, None, None, &config);

        let real = engine_row(&result, "real");
        let nocache = engine_row(&result, "nocache");
        assert_eq!(real.upstream_cost, 10.0);
        assert_eq!(real.client_cost, 10.0, "1M × $5/M × real 倍率 2.0");
        assert_eq!(nocache.upstream_cost, 10.0);
        assert_eq!(nocache.client_cost, 60.0, "4M × $5/M × nocache 倍率 3.0");
    }

    /// 凭据过滤与按 Key 过滤都必须逐引擎生效。
    #[test]
    fn billing_filters_apply_per_engine() {
        let agg = UsageAggregator::new();
        agg.ingest(&billing_rec("rust", 2_000_000));
        let mut other_cred = billing_rec("rust", 2_000_000);
        other_cred.credential_id = 10;
        agg.ingest(&other_cred);
        let mut other_key = billing_rec("go", 3_000_000);
        other_key.key_id = 2;
        agg.ingest(&other_key);

        let config = billing_config();
        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);

        // 凭据过滤：只留 9，另一条 rust 请求（凭据 10）应被排除。
        let only_nine = std::collections::HashSet::from([9]);
        let (_, filtered) = agg.query_billing(window, None, Some(&only_nine), &config);
        assert_eq!(engine_row(&filtered, "rust").calls, 1);
        assert_eq!(engine_row(&filtered, "rust").upstream_cost, 10.0);

        // 按 Key 过滤：Key 1 只用 rust，故 go 行整个消失。
        let (_, by_key) = agg.query_billing(window, Some(1), None, &config);
        assert!(
            by_key.engines.iter().all(|e| e.engine != "go"),
            "Key 1 没有 go 流量，不该出现 go 行"
        );
        assert_eq!(engine_row(&by_key, "rust").calls, 2, "Key 1 的两个凭据都算");
    }

    /// v1 老 JSONL（只有 `rust_usage` / `go_usage`，无 `engine`）必须仍能归类，
    /// 否则升级瞬间历史数据会从对比表里整段消失。
    #[test]
    fn v1_records_normalize_into_engine_rows() {
        let agg = UsageAggregator::new();
        let mut v1 = billing_rec("rust", 2_000_000);
        // 造一条真正的 v1 记录：engine / client_usage 为空，靠 rust_usage 表达。
        v1.engine = None;
        v1.client_usage = None;
        v1.rust_usage = Some(TokenUsageBreakdown {
            input_tokens: 2_000_000,
            ..Default::default()
        });
        agg.ingest(&v1.clone().normalized());

        let config = billing_config();
        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);
        let (_, result) = agg.query_billing(window, None, None, &config);
        let rust = engine_row(&result, "rust");
        assert_eq!(rust.client_cost, 15.0, "v1 的 rust_usage 应归入 rust 行");
        assert_eq!(rust.upstream_cost, 10.0);
    }
}
