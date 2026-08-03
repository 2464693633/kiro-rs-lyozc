//! 双缓存模拟引擎接缝。
//!
//! 用 enum 而非 trait object：两个变体编译期已知、无第三种规划，且 trait 会迫使
//! 装箱并掩盖两引擎的**两阶段不对称** —— 引擎 A 在 `begin` 里一次完成查+写，
//! 引擎 B 必须 `begin` 只查、`commit` 才写。
//!
//! 引擎 B 的两阶段不是风格选择：Go 的 `scan_start = len-2` 依赖「本轮最深断点此刻
//! 尚未入表」。若先写后查，首轮就会因刚写入的 `len-2` 断点而报出 cache_read。
//! 且 `commit` 只在请求成功后调用，使失败请求不污染缓存。

use super::cache_metering::{CacheUsage, SharedCacheMeter};
use super::cache_metering_go::{
    GoCacheTracker, ONE_BITS, PromptCacheProfile, build_claude_profile,
};
use super::types::MessagesRequest;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// 客户端 Key 选择的缓存模拟引擎。
///
/// `Hash` 是逐引擎计费聚合需要的：[`crate::admin::usage_stats`] 用
/// `(key, credential, model, engine)` 作为计费桶的键，使每个引擎的
/// 「上游真实 / 客户端计费」成对归集、互不混淆。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CacheEngineKind {
    /// 引擎 A（现有实现，带会话隔离）。老数据缺该字段时的默认值。
    #[default]
    Rust,
    /// 引擎 B（移植自 kiro-go，按实际账号共享指纹表）。
    Go,
    /// 引擎 C：不模拟缓存，直接采用上游真实 usage，仅套自己那组倍率。
    ///
    /// 无指纹表、无 TTL、无 `commit` —— 纯无状态换算。上游返回 cache 字段时
    /// 原样保留（各自乘对应倍率）。
    Real,
    /// 引擎 D：强制无缓存，且**完全不读上游 usage**。
    ///
    /// `input_tokens` 取客户端请求的本地估算（`token::count_all_tokens`），
    /// `output_tokens` 取实际返回内容的本地估算（`token::estimate_output_tokens`），
    /// cache 两项恒为 0，最后各套自己那组倍率。
    ///
    /// 不读上游 usage 是刻意的：上游可能本就是另一个 kiro-rs 反代，它报的 usage
    /// 已被模拟 / 膨胀过一轮，拿来当"真值"等于把上一跳的加工结果再加工一次。
    ///
    /// 与引擎 C 一样无状态（无指纹表、无 TTL、无 `commit`）。
    NoCache,
}

impl CacheEngineKind {
    /// 供 `skip_serializing_if` 使用：默认值不写进 JSON，保持老文件形状不变。
    pub fn is_default(&self) -> bool {
        matches!(self, CacheEngineKind::Rust)
    }

    /// 落进用量记录 / 计费聚合的稳定标识。**改动会使历史 JSONL 无法归类**。
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheEngineKind::Rust => "rust",
            CacheEngineKind::Go => "go",
            CacheEngineKind::Real => "real",
            CacheEngineKind::NoCache => "nocache",
        }
    }
}

/// 客户端 usage 的计算口径。由 [`CacheEngines::begin`] 定出，贯穿到
/// `upstream.rs` 的三个改写点与计费快照。
///
/// 这是四引擎的核心分歧点：引擎选择决定「用什么数」，倍率只决定「乘多少」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMode {
    /// 引擎 A / B：用 [`CacheUsage::split_against_total`] 做模拟分摊。
    Simulated,
    /// 引擎 C，以及**引擎 A 在无隔离种子时的降级**（主 apiKey 无 session：
    /// `isolation_seed` 返回 `None` → `CacheUsage` 全零 → 模拟不成立）。
    ///
    /// 该降级是必需的：此时 `prompt_total_est == 0`，若仍走模拟分摊会被调用方的
    /// `.max(1)` 钉成 `input_tokens = 1`，把上游真实值彻底销毁。
    Real,
    /// 引擎 D：cache 归零、token 并入 input。
    NoCache,
}

/// 引擎 C / D 的倍率存储。
///
/// 这两个引擎**无缓存状态** —— 没有指纹表、没有 TTL、没有 LRU，唯一的运行期状态
/// 就是倍率。故不为它们建 tracker 结构，只存倍率本身。
///
/// 倍率以 `f64::to_bits` 存进 `AtomicI64`（四引擎一致）：std 无 `AtomicF64`，而
/// 曾用的千分比整数会把 <0.0005 的正倍率量化成 0 —— 下发 token 全归零，且对任何
/// 超过 3 位小数的值静默丢精度。原子量使 admin 改配置后无需重启即生效。
#[derive(Debug)]
pub struct StatelessMultipliers {
    // 引擎 C（real）：上游真实值 × 这四个。
    real_input_bits: AtomicI64,
    real_output_bits: AtomicI64,
    real_cache_read_bits: AtomicI64,
    real_cache_creation_bits: AtomicI64,
    // 引擎 D（nocache）：cache 恒为 0，故只需 input / output 两个。
    nocache_input_bits: AtomicI64,
    nocache_output_bits: AtomicI64,
}

impl Default for StatelessMultipliers {
    fn default() -> Self {
        // 默认 1.0：不缩放，与「未配置时不应凭空改动 token」一致。
        Self {
            real_input_bits: AtomicI64::new(ONE_BITS),
            real_output_bits: AtomicI64::new(ONE_BITS),
            real_cache_read_bits: AtomicI64::new(ONE_BITS),
            real_cache_creation_bits: AtomicI64::new(ONE_BITS),
            nocache_input_bits: AtomicI64::new(ONE_BITS),
            nocache_output_bits: AtomicI64::new(ONE_BITS),
        }
    }
}

impl StatelessMultipliers {
    /// 热更新引擎 C 倍率（已 sanitize 的值）。
    pub fn apply_real_config(&self, c: crate::model::config::CacheEngineRealConfig) {
        let c = c.sanitized();
        let to_bits = |v: f64| v.to_bits() as i64;
        self.real_input_bits
            .store(to_bits(c.input_multiplier), Ordering::Relaxed);
        self.real_output_bits
            .store(to_bits(c.output_multiplier), Ordering::Relaxed);
        self.real_cache_read_bits
            .store(to_bits(c.cache_read_multiplier), Ordering::Relaxed);
        self.real_cache_creation_bits
            .store(to_bits(c.cache_creation_multiplier), Ordering::Relaxed);
    }

    /// 热更新引擎 D 倍率（已 sanitize 的值）。
    pub fn apply_nocache_config(&self, c: crate::model::config::CacheEngineNoCacheConfig) {
        let c = c.sanitized();
        let to_bits = |v: f64| v.to_bits() as i64;
        self.nocache_input_bits
            .store(to_bits(c.input_multiplier), Ordering::Relaxed);
        self.nocache_output_bits
            .store(to_bits(c.output_multiplier), Ordering::Relaxed);
    }

    /// 引擎 C 倍率 `(input, output, cache_read, cache_creation)`。
    pub fn real(&self) -> (f64, f64, f64, f64) {
        let load = |a: &AtomicI64| f64::from_bits(a.load(Ordering::Relaxed) as u64);
        (
            load(&self.real_input_bits),
            load(&self.real_output_bits),
            load(&self.real_cache_read_bits),
            load(&self.real_cache_creation_bits),
        )
    }

    /// 引擎 D 倍率 `(input, output)`。cache 恒为 0，无对应倍率。
    pub fn nocache(&self) -> (f64, f64) {
        let load = |a: &AtomicI64| f64::from_bits(a.load(Ordering::Relaxed) as u64);
        (
            load(&self.nocache_input_bits),
            load(&self.nocache_output_bits),
        )
    }
}

/// 四套引擎的共享句柄。
///
/// `rust` / `go` 为 `None` 表示该引擎未启用（有缓存状态，需显式构造）。
/// `stateless` 承载引擎 C / D 的倍率 —— 这两个引擎无状态，无「未启用」概念，
/// 故非 `Option`。
#[derive(Clone, Default)]
pub struct CacheEngines {
    pub rust: Option<SharedCacheMeter>,
    pub go: Option<Arc<GoCacheTracker>>,
    pub stateless: Arc<StatelessMultipliers>,
}

/// 本次请求下发前应使用的缩放倍率。
///
/// 两套引擎各用自己的一组，互不影响 —— 这样同一部署里可以对两套独立调参并直接
/// 对比效果。若共用全局膨胀倍率，改一处会同时动两套，对比就失去意义。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsageMultipliers {
    /// 引擎 A：四个独立倍率，每项**未设置时逐项回退全局膨胀倍率**。
    ///
    /// 回退在 [`super::cache_metering::CacheMeter::multipliers`] 里完成，故到这里
    /// 时四个值都已是具体数。保留回退是为了向后兼容：老部署只配了全局倍率，若 A
    /// 的新字段直接默认 1.0，升级后已配的倍率会静默失效。
    Rust {
        input: f64,
        output: f64,
        cache_read: f64,
        cache_creation: f64,
    },
    /// 引擎 B：go 引擎专属倍率。
    ///
    /// `cache_creation` 默认 1.0 —— 对齐 Go 侧 `buildClaudeUsageMap`，它只缩放
    /// `input_tokens` 与 `cache_read_input_tokens`。调离 1.0 会削弱两套引擎在
    /// 「creation/read 划分」这个维度上的可比性，但作为运营旋钮开放。
    ///
    /// `output` 默认 1.0（= Go 原实现无此倍率），但开放为可调 —— 四引擎都能独立
    /// 设置全部倍率，B 不该因为移植来源的缺失而少一个旋钮。
    Go {
        input: f64,
        output: f64,
        cache_read: f64,
        cache_creation: f64,
    },
    /// 引擎 C：四个独立倍率，含 `output`（与 Go 不同，C 允许缩放 output）。
    Real {
        input: f64,
        output: f64,
        cache_read: f64,
        cache_creation: f64,
    },
    /// 引擎 D：只有 input / output 两个倍率。cache 恒为 0，乘任何数都是 0，
    /// 故不设 cache 倍率 —— 留着只会让运维以为调它有用。
    NoCache { input: f64, output: f64 },
}

impl UsageMultipliers {
    /// 摊平成本次请求实际生效的四元组 `(input, output, cache_read, cache_creation)`。
    ///
    /// **不再接受全局倍率参数**：四个引擎各自携带完整的显式值，全局倍率的回退
    /// 已在上游完成（引擎 A 在 [`CacheMeter::multipliers`] 里解析 `Option`）。
    /// 若这里还收 `global`，就会存在「两处都能决定最终值」的歧义。
    pub fn resolve(self) -> (f64, f64, f64, f64) {
        match self {
            UsageMultipliers::Rust {
                input,
                output,
                cache_read,
                cache_creation,
            } => (input, output, cache_read, cache_creation),
            UsageMultipliers::Go {
                input,
                output,
                cache_read,
                cache_creation,
            } => (input, output, cache_read, cache_creation),
            UsageMultipliers::Real {
                input,
                output,
                cache_read,
                cache_creation,
            } => (input, output, cache_read, cache_creation),
            // cache 位给 0.0，与 UsageMode::NoCache 在 token 层的归零**重复**表达。
            //
            // 这是刻意的冗余：倍率四元组的 cache 两位只被用于缩放 cache token
            // （`scale(creation, m.3)` / `scale(read, m.2)`），不承载其他语义。给
            // 0.0 使「引擎 D 的 cache 恒为 0」成为结构性保证 —— 将来若有人新增
            // 一条不经 resolve_tokens 的路径并直接套用 D 的倍率，1.0 会让上游
            // cache 原样漏出、违背引擎定义，0.0 则不可能。
            UsageMultipliers::NoCache { input, output } => (input, output, 0.0, 0.0),
        }
    }
}

impl UsageMode {
    /// 定出客户端应看到的 `(input, cache_creation, cache_read)`（**膨胀前**）。
    ///
    /// - `real`：上游真实三元组。上游凭据路径取自响应 usage / `message_start`；
    ///   Kiro 路径取 `(final_input_tokens, 0, 0)`（Kiro 不下发 cache 字段）。
    /// - `local_input`：**本地**算出的客户端 prompt token（`token::count_all_tokens`，
    ///   在请求发出前就已确定，不含任何上游数据）。仅引擎 D 使用。
    ///
    /// 引擎 D 刻意不读 `real`：上游本身可能是另一个 kiro-rs 反代，其 usage 已被模拟
    /// 或膨胀过，拿它当"真值"是把别人的加工结果当基准。D 的口径是「客户端实际发了
    /// 多少、实际收到多少」，故 input 取本地估算、cache 恒为 0（非"并入 input"——
    /// 上游那份 cache 数字整个不参与）。
    ///
    /// 四引擎的全部口径分歧集中在这里，`upstream.rs` 的三个改写点与计费快照
    /// 共用本函数 —— 避免同一套三分支在四处各写一遍而逐渐漂移。
    pub fn resolve_tokens(
        self,
        cache_usage: CacheUsage,
        simulated_total_input: i32,
        real: (i32, i32, i32),
        local_input: i32,
    ) -> (i32, i32, i32) {
        match self {
            // 自带降级：引擎 B 的 CacheUsage 在 begin() 之后才由 compute_pending
            // 算出，故 begin() 返回 Simulated 时无法预知结果是否为零。这里再判一次，
            // 使「mode 说模拟、数据却不支持模拟」不可能穿到 split_against_total ——
            // 否则调用方的 `.max(1)` 会把 input_tokens 钉成 1。
            UsageMode::Simulated if !cache_usage.is_simulated() => real,
            UsageMode::Simulated => cache_usage.split_against_total(simulated_total_input),
            UsageMode::Real => real,
            // 引擎 D **完全不读上游 usage**：input 取客户端请求的本地估算。
            //
            // 上游本身可能就是另一个 kiro-rs 反代，它报的 usage 已被模拟/膨胀过一轮，
            // 拿来当"真值"会把上一跳的加工结果再加工一次。本地口径与上游无关，
            // 请求发出前即已确定（token::count_all_tokens）。
            //
            // cache 恒为 0：这是该引擎的定义，不是"上游没给"。
            UsageMode::NoCache => (local_input.max(0), 0, 0),
        }
    }

    /// 决定客户端应看到的 `output_tokens`（**膨胀前**）。
    ///
    /// 引擎 D 用本地估算的实际下发内容（不信上游）；其余三个引擎沿用上游真值 ——
    /// 它们的 output 从来就没被模拟过，改动会超出各自契约。
    ///
    /// `local_output` 为 `None` 时回落上游真值：流式在累积完成前取不到本地值，
    /// 此时宁可报上游数也不报 0。
    pub fn resolve_output(self, real_output: i32, local_output: Option<i32>) -> i32 {
        match self {
            UsageMode::NoCache => local_output.unwrap_or(real_output),
            _ => real_output,
        }
    }
}

/// `begin` 的第三个返回值：请求成功后需要提交的写入意图。
///
/// 引擎 A 在 `begin` 中已完成写入，故其变体不携带数据、`commit` 为空操作 ——
/// 这使本次重构在结构上不可能改变引擎 A 的行为。
pub enum PendingCache {
    /// 无需提交（引擎未启用、无断点，或引擎 A 已写完）。
    None,
    /// 引擎 B：待写入的本轮断点集合。
    Go(PromptCacheProfile),
}

impl CacheEngines {
    /// 引擎 C 倍率 `(input, output, cache_read, cache_creation)`。
    ///
    /// C 保留上游真实的 cache 划分，所以四个维度都需要独立倍率 —— 与 A（复用全局
    /// 三元组、read/creation 同值）和 D（无 cache 维度）都不同。
    pub fn real_multipliers(&self) -> (f64, f64, f64, f64) {
        self.stateless.real()
    }

    /// 引擎 D 倍率 `(input, output)`。
    ///
    /// D 的 cache 恒为 0，cache 倍率乘上去也永远是 0，故不提供该维度的旋钮。
    pub fn nocache_multipliers(&self) -> (f64, f64) {
        self.stateless.nocache()
    }

    /// 组装引擎 A 的倍率变体。
    ///
    /// A 的四个倍率是 `Option`：未设置时逐项回退全局膨胀倍率，回退在
    /// [`super::cache_metering::CacheMeter::multipliers`] 里完成。引擎未启用时整组
    /// 回退全局 —— 未启用的引擎不该凭空缩放。
    ///
    /// 抽成函数是因为 `begin` 与 `multipliers_for` 都要用，且两处必须给出同一个
    /// 答案：前者决定下发给客户端的数字，后者决定计费快照的数字。
    fn rust_multipliers(&self, global: (f64, f64, f64)) -> UsageMultipliers {
        let (input, output, cache_read, cache_creation) = self
            .rust
            .as_ref()
            .map(|meter| meter.multipliers(global))
            .unwrap_or((global.0, global.1, global.2, global.2));
        UsageMultipliers::Rust {
            input,
            output,
            cache_read,
            cache_creation,
        }
    }

    /// 解析**指定引擎**的客户端 token 倍率四元组 `(input, output, cache_read, cache_creation)`。
    ///
    /// 一次请求只跑一个引擎，故按 kind 解析一套即可 —— 早先返回「Rust/Go 两套」的
    /// 形状在四引擎下会变成四元组，且三套是当次请求用不到的死值。
    ///
    /// 四个引擎各持一组独立倍率，互不影响：
    /// - A（rust）：四个倍率，未设置的逐项回退全局膨胀倍率（向后兼容）
    /// - B（go）/ C（real）：四个倍率，全部独立
    /// - D（nocache）：只有 input / output —— cache 恒为 0，无该维度可调
    ///
    /// A / B 的 tracker 未启用时整组回落全局倍率，避免未启用的引擎凭空改变 token 数。
    pub fn multipliers_for(
        &self,
        kind: CacheEngineKind,
        global: (f64, f64, f64),
    ) -> (f64, f64, f64, f64) {
        self.usage_multipliers(kind, global).resolve()
    }

    /// 定出该引擎本次生效的倍率组。
    ///
    /// `global` 只在引擎 A 未显式配置倍率时用作回退（见 [`CacheMeter::multipliers`]），
    /// 以及 A / B 的 tracker 未启用时的兜底 —— 未启用的引擎不该凭空缩放 token。
    fn usage_multipliers(
        &self,
        kind: CacheEngineKind,
        global: (f64, f64, f64),
    ) -> UsageMultipliers {
        // tracker 未启用时的兜底：回落引擎 A 那套（它自身又会逐项回退全局）。
        //
        // 不能只回落原始 global：`begin` 走的就是 `rust_multipliers`，若这里给出
        // 不同答案，Key 选了 B 而 B 未启用时，客户端看到 A 的显式倍率、计费记录
        // 却记原始全局倍率 —— 两者对不上账且无任何报错。见
        // `go_disabled_keeps_dispatch_and_billing_in_sync`。
        let engine_a_fallback = self.rust_multipliers(global);
        match kind {
            // 委托而非重写：`begin` 也要算 A 的倍率，两处必须给出同一答案 ——
            // 前者决定下发给客户端的数字，后者决定计费快照的数字，分叉即对不上账。
            CacheEngineKind::Rust => self.rust_multipliers(global),
            CacheEngineKind::Go => self
                .go
                .as_ref()
                .map(|tracker| {
                    let (input, output, cache_read, cache_creation) = tracker.multipliers();
                    UsageMultipliers::Go {
                        input,
                        output,
                        cache_read,
                        cache_creation,
                    }
                })
                .unwrap_or(engine_a_fallback),
            CacheEngineKind::Real => {
                let (input, output, cache_read, cache_creation) = self.real_multipliers();
                UsageMultipliers::Real {
                    input,
                    output,
                    cache_read,
                    cache_creation,
                }
            }
            CacheEngineKind::NoCache => {
                let (input, output) = self.nocache_multipliers();
                UsageMultipliers::NoCache { input, output }
            }
        }
    }

    /// 阶段一：算出本次请求的缓存覆盖情况。
    ///
    /// 返回的 [`CacheUsage`] 是 estimate 口径，由调用方在拿到真实 total 后用
    /// `split_against_total` 做互斥分摊 —— 两套引擎共用这条下游链路。
    /// `global` 是全局膨胀倍率三元组，**仅供引擎 A 未设置自己那项时逐项回退**。
    /// 其余三个引擎完全不看它。见 [`UsageMultipliers::Rust`] 的文档注释。
    pub fn begin(
        &self,
        req: &MessagesRequest,
        key_id: u64,
        kind: CacheEngineKind,
        global: (f64, f64, f64),
    ) -> (CacheUsage, UsageMultipliers, PendingCache, UsageMode) {
        // 倍率**只在这一处解析**，与 `multipliers_for`（计费快照）同源。
        //
        // 早先两处各写一遍 match，结果在「Key 选 B 但 B 未启用」时分叉：这里返回
        // 引擎 A 的显式倍率，那边返回原始全局倍率 —— 客户端看到的 token 数与计费
        // 记录不一致，且无任何报错。见 `go_disabled_keeps_dispatch_and_billing_in_sync`。
        let muls = self.usage_multipliers(kind, global);
        match kind {
            CacheEngineKind::Rust => {
                let usage = self
                    .rust
                    .as_ref()
                    .map(|cache| super::cache_metering::compute_cache_usage(cache, req, key_id))
                    .unwrap_or_default();
                // 主 apiKey 无 session 时 `isolation_seed` 返回 None → 全零 CacheUsage。
                // 此时按 Simulated 走会被 `.max(1)` 钉成 input_tokens = 1，把上游真值
                // 彻底销毁，故降级为 Real（真实值 + 全局倍率）。引擎未启用同理。
                let mode = if usage.is_simulated() {
                    UsageMode::Simulated
                } else {
                    UsageMode::Real
                };
                (usage, muls, PendingCache::None, mode)
            }
            CacheEngineKind::Real => {
                (CacheUsage::default(), muls, PendingCache::None, UsageMode::Real)
            }
            CacheEngineKind::NoCache => {
                (CacheUsage::default(), muls, PendingCache::None, UsageMode::NoCache)
            }
            CacheEngineKind::Go => {
                let Some(tracker) = self.go.as_ref() else {
                    // 引擎未启用：退化为无缓存。倍率已由 usage_multipliers 兜底
                    // （回落引擎 A 那套，它自身又会逐项回退全局）—— 不该凭空缩放。
                    // mode 取 Real —— 无模拟数据时必须用上游真值，理由同上。
                    return (
                        CacheUsage::default(),
                        muls,
                        PendingCache::None,
                        UsageMode::Real,
                    );
                };
                // 使用与 kiro-go 相同的内容型请求 token 估算作为分母；不能传 0，
                // 否则 profile 会退化为 canonical wrapper 总量，导致缓存覆盖比例偏低。
                let estimated_total = super::cache_metering_go::estimate_claude_request_input_tokens(req);
                let Some(profile) = build_claude_profile(req, estimated_total, tracker.effective_ttl_ms()) else {
                    // 无断点：无缓存，但 Key 选的仍是 go 引擎，倍率照其配置生效。
                    // mode 取 Real —— 没有可分摊的模拟量，必须用上游真值。
                    return (CacheUsage::default(), muls, PendingCache::None, UsageMode::Real);
                };
                // Provider selects the credential inside call_api*. Compute only after
                // that selection so failover cannot reuse another account's cache.
                //
                // 此处 CacheUsage 仍是全零占位，真值由 compute_pending 在选定凭据后填入，
                // 故 mode 报 Simulated 而数据尚不支持模拟 —— 由 resolve_tokens 的自兜底处理。
                (
                    CacheUsage::default(),
                    muls,
                    PendingCache::Go(profile),
                    UsageMode::Simulated,
                )
            }
        }
    }

    /// Compute a Go profile in the namespace of the selected credential.
    pub fn compute_pending(&self, pending: &PendingCache, account_id: u64) -> CacheUsage {
        let PendingCache::Go(profile) = pending else {
            return CacheUsage::default();
        };
        let Some(tracker) = self.go.as_ref() else {
            return CacheUsage::default();
        };
        let usage = tracker.compute_for_account(account_id, profile);
        CacheUsage {
            cache_read: usage.cache_read as i32,
            cache_covered_est: (usage.cache_creation + usage.cache_read) as i32,
            prompt_total_est: profile.total_input_tokens as i32,
        }
    }

    /// 阶段二：**仅在请求成功后调用**。引擎 A 走空分支。
    pub fn commit(&self, pending: PendingCache, account_id: u64) {
        match pending {
            PendingCache::None => {}
            PendingCache::Go(profile) => {
                if let Some(tracker) = self.go.as_ref() {
                    tracker.update_for_account(account_id, &profile);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{CacheEngineGoConfig, CacheEngineRustConfig};

    fn convo(turns: usize) -> MessagesRequest {
        let body = "the quick brown fox jumps over the lazy dog ".repeat(20);
        let messages = (0..turns)
            .map(|i| super::super::types::Message {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                content: serde_json::json!([{"type": "text", "text": body}]),
            })
            .collect();
        MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 64,
            messages,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    fn go_cfg() -> CacheEngineGoConfig {
        CacheEngineGoConfig {
            min_cacheable_tokens: 0,
            opus_min_cacheable_tokens: 0,
            ..CacheEngineGoConfig::default()
        }
    }

    fn engines() -> CacheEngines {
        CacheEngines {
            rust: Some(Arc::new(
                super::super::cache_metering::CacheMeter::new_with_config(
                    None,
                    CacheEngineRustConfig::default(),
                ),
            )),
            go: Some(Arc::new(GoCacheTracker::new(None, go_cfg()))),
            stateless: Arc::new(StatelessMultipliers::default()),
        }
    }

    /// 回归：go tracker 未启用时，`begin`（下发给客户端）与 `multipliers_for`
    /// （写进计费记录）必须给出同一组倍率。
    ///
    /// 这两处曾各自独立构造倍率变体：给引擎 A 加显式倍率时只改了 `begin`，
    /// 于是 Key 选 B 而 B 未启用时，客户端按 A 的显式倍率收数、计费记录按原始
    /// 全局倍率记账 —— 两边对不上账，且无任何报错。
    #[test]
    fn go_disabled_keeps_dispatch_and_billing_in_sync() {
        let global = (2.0, 3.0, 4.0);
        // 只启用 A，且给 A 配一组与 global 明显不同的显式倍率。
        let eng = CacheEngines {
            rust: Some(std::sync::Arc::new(
                super::super::cache_metering::CacheMeter::new_with_config(
                    None,
                    CacheEngineRustConfig {
                        input_multiplier: Some(7.0),
                        output_multiplier: Some(8.0),
                        cache_read_multiplier: Some(9.0),
                        cache_creation_multiplier: Some(10.0),
                        ..CacheEngineRustConfig::default()
                    },
                ),
            )),
            go: None,
            stateless: std::sync::Arc::new(StatelessMultipliers::default()),
        };

        let req = convo(2);
        let (_, muls, _, _) = eng.begin(&req, 1, CacheEngineKind::Go, global);
        assert_eq!(
            muls.resolve(),
            eng.multipliers_for(CacheEngineKind::Go, global),
            "下发口径与计费口径必须一致，否则客户端所见与账单分叉"
        );
    }

    /// 接线哨兵：`with_cache_meter` / `with_go_cache_tracker` 必须真的把句柄塞进
    /// `cache_engines`。此前 `cache_engines.go` 恒为 None，选 go 的 Key 会安静
    /// 退化成「无缓存」—— 无任何报错，只是数字全 0。这条测试让那种回归编译期
    /// 之后立刻可见。
    #[test]
    fn app_state_builders_populate_both_engines() {
        use super::super::middleware::AppState;
        use crate::model::config::ToolCompatibilityMode;

        let state = AppState::new(false, ToolCompatibilityMode::default());
        assert!(state.cache_engines.rust.is_none());
        assert!(state.cache_engines.go.is_none());

        let meter = Arc::new(super::super::cache_metering::CacheMeter::new_with_config(
            None,
            CacheEngineRustConfig::default(),
        ));
        let tracker = Arc::new(GoCacheTracker::new(None, go_cfg()));
        let state = state
            .with_cache_meter(Some(meter))
            .with_go_cache_tracker(Some(tracker));

        assert!(
            state.cache_engines.rust.is_some(),
            "引擎 A 句柄未进 cache_engines"
        );
        assert!(
            state.cache_engines.go.is_some(),
            "引擎 B 句柄未进 cache_engines —— 选 go 的 Key 会安静退化为无缓存"
        );
        // 旧字段仍需保持，admin 侧统计仍在读它
        assert!(state.cache_meter.is_some());
    }

    /// `resolve` 是四套引擎倍率的唯一摊平落点 —— 每个变体的字段必须原样落到
    /// 对应位置，不得串位。串位不会引发编译错误，只会让某个维度悄悄用错倍率。
    #[test]
    fn resolve_maps_each_variant_to_its_own_slots() {
        // 四个变体各给互不相同的值，任何串位都会被下面的断言抓到。
        assert_eq!(
            UsageMultipliers::Rust {
                input: 2.0,
                output: 3.0,
                cache_read: 4.0,
                cache_creation: 5.0,
            }
            .resolve(),
            (2.0, 3.0, 4.0, 5.0),
            "A 的四个倍率现已完全独立，creation 不再被迫等于 read"
        );

        assert_eq!(
            UsageMultipliers::Go {
                input: 1.5,
                output: 2.5,
                cache_read: 3.0,
                cache_creation: 0.5,
            }
            .resolve(),
            (1.5, 2.5, 3.0, 0.5),
            "B 的 output 已可调 —— 不再硬编码 1.0"
        );

        assert_eq!(
            UsageMultipliers::Real {
                input: 6.0,
                output: 7.0,
                cache_read: 8.0,
                cache_creation: 9.0,
            }
            .resolve(),
            (6.0, 7.0, 8.0, 9.0),
        );

        // D 的 cache 两位恒为 0：cache 结构性不存在，见变体文档注释。
        assert_eq!(
            UsageMultipliers::NoCache {
                input: 1.5,
                output: 2.5
            }
            .resolve(),
            (1.5, 2.5, 0.0, 0.0),
            "D 的 cache 倍率恒 0，不受传入值影响"
        );
    }

    #[test]
    fn billing_multipliers_keep_rust_and_go_independent() {
        let eng = engines();
        eng.go.as_ref().unwrap().apply_config(CacheEngineGoConfig {
            input_token_multiplier: 1.5,
            cache_read_multiplier: 2.5,
            cache_creation_multiplier: 0.75,
            ..go_cfg()
        });

        // 每个 kind 各自解析，互不影响。
        //
        // A 此时四个倍率均未设置（engines() 用 default 构造），故逐项回退全局 ——
        // 这是老部署升级后的行为：只配过全局倍率的部署不该因为「A 有了独立字段」
        // 而突然变成 1×。
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Rust, (2.0, 3.0, 4.0)),
            (2.0, 3.0, 4.0, 4.0),
            "引擎 A 未设置时逐项回退全局倍率，creation 与 read 同取 cache 位"
        );
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Go, (2.0, 3.0, 4.0)),
            (1.5, 1.0, 2.5, 0.75),
            "引擎 B 用自己那组；output 此处是配置值 1.0 而非结构性恒 1"
        );

        // 引擎 C / D 默认 1.0，且**不受全局倍率影响** —— 各自独立配置。
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Real, (2.0, 3.0, 4.0)),
            (1.0, 1.0, 1.0, 1.0),
            "引擎 C 默认不缩放，不继承全局倍率"
        );
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::NoCache, (2.0, 3.0, 4.0)),
            (1.0, 1.0, 0.0, 0.0),
            "引擎 D 的 cache 倍率恒为 0（cache 本身恒为 0）"
        );

        // C / D 热更新后按各自配置生效。
        eng.stateless.apply_real_config(crate::model::config::CacheEngineRealConfig {
            input_multiplier: 2.5,
            output_multiplier: 1.5,
            cache_read_multiplier: 3.5,
            cache_creation_multiplier: 0.5,
        });
        eng.stateless.apply_nocache_config(crate::model::config::CacheEngineNoCacheConfig {
            input_multiplier: 4.0,
            output_multiplier: 2.0,
        });
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Real, (9.0, 9.0, 9.0)),
            (2.5, 1.5, 3.5, 0.5)
        );
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::NoCache, (9.0, 9.0, 9.0)),
            (4.0, 2.0, 0.0, 0.0)
        );
    }

    /// 引擎 A 显式配置倍率后必须**压过**全局膨胀倍率。
    ///
    /// 这是本次「四引擎各自独立倍率」的核心：改动前 A 只能用全局值，无法与 B/C/D
    /// 一样独立调参。若 `CacheMeter::multipliers` 的 `Option` 解析写反（未设置时用
    /// 1.0、已设置时用 global），本测试是唯一能抓住的地方 —— 默认路径的测试会照常
    /// 通过，因为未设置时两种写法都回落全局。
    #[test]
    fn rust_engine_explicit_multipliers_override_global() {
        let eng = engines();
        let global = (2.0, 3.0, 4.0);

        // 未设置：逐项回退全局（向后兼容 —— 老部署只配了全局倍率）
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Rust, global),
            (2.0, 3.0, 4.0, 4.0),
            "未设置时必须回退全局，否则升级会让已配的全局倍率静默失效"
        );

        // 部分设置：只覆盖 input，其余三项仍回退全局
        eng.rust.as_ref().unwrap().apply_config(CacheEngineRustConfig {
            input_multiplier: Some(7.0),
            ..CacheEngineRustConfig::default()
        });
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Rust, global),
            (7.0, 3.0, 4.0, 4.0),
            "逐项回退：设了 input 不该把 output/cache 也一起顶成默认"
        );

        // 全部设置：完全不看全局值
        eng.rust.as_ref().unwrap().apply_config(CacheEngineRustConfig {
            input_multiplier: Some(1.1),
            output_multiplier: Some(2.2),
            cache_read_multiplier: Some(3.3),
            cache_creation_multiplier: Some(4.4),
            ..CacheEngineRustConfig::default()
        });
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Rust, global),
            (1.1, 2.2, 3.3, 4.4),
            "全部显式设置后应完全无视全局倍率"
        );
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Rust, (99.0, 99.0, 99.0)),
            (1.1, 2.2, 3.3, 4.4),
            "全局倍率变化不得影响已显式配置的 A"
        );
    }

    /// 引擎 B 的 `output` 倍率可独立设置。
    ///
    /// 改动前它在 `resolve` 里被硬编码成 1.0（Go 原实现无此倍率），任何配置都无效。
    #[test]
    fn go_engine_output_multiplier_is_settable() {
        let eng = engines();
        eng.go.as_ref().unwrap().apply_config(CacheEngineGoConfig {
            input_token_multiplier: 1.5,
            output_multiplier: 2.75,
            cache_read_multiplier: 2.5,
            cache_creation_multiplier: 0.75,
            ..go_cfg()
        });
        assert_eq!(
            eng.multipliers_for(CacheEngineKind::Go, (9.0, 9.0, 9.0)),
            (1.5, 2.75, 2.5, 0.75),
            "output 必须取配置值，而非被硬编码成 1.0"
        );
    }

    /// 引擎 A 的倍率热更新必须立刻被 `begin` 读到，且「清空 → 回退全局」可逆。
    ///
    /// 可逆性是 `Option` 语义的核心：运维把某项调回"继承"后，后续改全局倍率必须
    /// 重新对 A 生效。若 `apply_config` 把 `None` 存成 1.0 而非哨兵，这条会断 ——
    /// 表现为"清空后 A 永远是 1×"，且不报任何错。
    #[test]
    fn rust_multipliers_hot_reload_and_revert_to_global() {
        let eng = engines();
        let req = convo(3);
        let global = (2.0, 3.0, 4.0);

        // 初始未设置 → 全项回退全局
        let (_, muls, _, _) = eng.begin(&req, 7, CacheEngineKind::Rust, global);
        assert_eq!(muls.resolve(), (2.0, 3.0, 4.0, 4.0), "未设置时回退全局");

        // 只显式设 input / cache_creation，另两项留 None
        eng.rust
            .as_ref()
            .unwrap()
            .apply_config(CacheEngineRustConfig {
                input_multiplier: Some(1.25),
                cache_creation_multiplier: Some(0.5),
                ..CacheEngineRustConfig::default()
            });
        let (_, muls, _, _) = eng.begin(&req, 7, CacheEngineKind::Rust, global);
        assert_eq!(
            muls.resolve(),
            (1.25, 3.0, 4.0, 0.5),
            "逐项独立：设了的用自己的，没设的仍回退全局"
        );

        // 清空全部 → 必须重新回退全局（哨兵语义可逆）
        eng.rust
            .as_ref()
            .unwrap()
            .apply_config(CacheEngineRustConfig::default());
        let (_, muls, _, _) = eng.begin(&req, 7, CacheEngineKind::Rust, global);
        assert_eq!(
            muls.resolve(),
            (2.0, 3.0, 4.0, 4.0),
            "清空后必须重新回退全局，而非固化成 1.0"
        );

        // 全局改动对"继承中"的 A 立即生效
        let (_, muls, _, _) = eng.begin(&req, 7, CacheEngineKind::Rust, (9.0, 8.0, 7.0));
        assert_eq!(muls.resolve(), (9.0, 8.0, 7.0, 7.0), "继承项跟随全局变化");
    }

    /// `multipliers_for`（计费快照）与 `begin`（下发客户端）必须给出同一答案。
    ///
    /// 两者若分叉，客户端看到的 token 数与计费记录会不一致 —— 这类 bug 在日志里
    /// 看不出来，只能靠对账发现。故直接钉住四引擎的一致性。
    #[test]
    fn billing_and_client_multipliers_agree_for_all_engines() {
        let eng = engines();
        let req = convo(3);
        let global = (2.0, 3.0, 4.0);
        eng.rust
            .as_ref()
            .unwrap()
            .apply_config(CacheEngineRustConfig {
                input_multiplier: Some(1.5),
                ..CacheEngineRustConfig::default()
            });
        eng.go.as_ref().unwrap().apply_config(CacheEngineGoConfig {
            output_multiplier: 2.5,
            ..go_cfg()
        });
        eng.stateless
            .apply_real_config(crate::model::config::CacheEngineRealConfig {
                input_multiplier: 3.5,
                output_multiplier: 1.5,
                cache_read_multiplier: 2.0,
                cache_creation_multiplier: 0.5,
            });
        eng.stateless
            .apply_nocache_config(crate::model::config::CacheEngineNoCacheConfig {
                input_multiplier: 4.5,
                output_multiplier: 2.0,
            });

        for kind in [
            CacheEngineKind::Rust,
            CacheEngineKind::Go,
            CacheEngineKind::Real,
            CacheEngineKind::NoCache,
        ] {
            let (_, muls, _, _) = eng.begin(&req, 7, kind, global);
            assert_eq!(
                muls.resolve(),
                eng.multipliers_for(kind, global),
                "{kind:?}：计费快照与下发口径必须同源"
            );
        }
    }

    #[test]
    fn engine_kind_defaults_to_rust() {
        assert_eq!(CacheEngineKind::default(), CacheEngineKind::Rust);
        assert!(CacheEngineKind::Rust.is_default());
        assert!(!CacheEngineKind::Go.is_default());
        // 序列化形状：写进 client_api_keys.json 的字面量
        assert_eq!(
            serde_json::to_string(&CacheEngineKind::Go).unwrap(),
            "\"go\""
        );
        assert_eq!(
            serde_json::from_str::<CacheEngineKind>("\"rust\"").unwrap(),
            CacheEngineKind::Rust
        );
    }

    /// 引擎 A 经接缝走出的结果必须与直接调用 `compute_cache_usage` 完全一致 ——
    /// 这是「重构不得改变引擎 A 行为」的机器化保证。
    #[test]
    fn rust_engine_begin_matches_direct_call() {
        let req = convo(5);

        let direct = super::super::cache_metering::CacheMeter::new_with_config(
            None,
            CacheEngineRustConfig::default(),
        );
        let expected = super::super::cache_metering::compute_cache_usage(&direct, &req, 7);

        let eng = engines();
        // 传入非 1.0 的全局倍率：A 未显式配置倍率，故应逐项回退到它。
        let (actual, muls, pending, _mode) =
            eng.begin(&req, 7, CacheEngineKind::Rust, (2.0, 3.0, 4.0));

        assert_eq!(actual.cache_read, expected.cache_read);
        assert_eq!(actual.cache_covered_est, expected.cache_covered_est);
        assert_eq!(actual.prompt_total_est, expected.prompt_total_est);
        assert!(matches!(pending, PendingCache::None), "引擎 A 无待提交状态");
        assert_eq!(
            muls,
            UsageMultipliers::Rust {
                input: 2.0,
                output: 3.0,
                cache_read: 4.0,
                cache_creation: 4.0,
            },
            "引擎 A 未配倍率时必须回退全局（creation 与 read 同取全局 cache 倍率）"
        );
    }

    /// 引擎 A 的 commit 是空操作：调用它不得产生任何可观测变化。
    #[test]
    fn rust_engine_commit_is_noop() {
        let eng = engines();
        let req = convo(5);
        let (_, _, pending, _mode) = eng.begin(&req, 1, CacheEngineKind::Rust, (1.0, 1.0, 1.0));
        let before = eng.rust.as_ref().unwrap().stats();
        eng.commit(pending, 1);
        let after = eng.rust.as_ref().unwrap().stats();
        assert_eq!(before.entries, after.entries);
        assert_eq!(before.hits, after.hits);
        assert_eq!(before.misses, after.misses);
    }

    /// 引擎 B 两阶段：begin 只查（不写），commit 才写。
    #[test]
    fn go_engine_defers_write_to_commit() {
        let eng = engines();
        let req = convo(5);

        let (_, muls, pending, _mode) = eng.begin(&req, 1, CacheEngineKind::Go, (1.0, 1.0, 1.0));
        let usage = eng.compute_pending(&pending, 1);
        assert_eq!(usage.cache_read, 0, "首轮不得有 read");
        assert!(
            matches!(muls, UsageMultipliers::Go { .. }),
            "go 引擎必须用自己的倍率，不得回落全局"
        );
        assert!(usage.cache_covered_est > 0);
        assert_eq!(
            eng.go.as_ref().unwrap().stats().entries,
            0,
            "begin 不应写入任何条目"
        );

        eng.commit(pending, 1);
        assert!(
            eng.go.as_ref().unwrap().stats().entries > 0,
            "commit 后才应有条目"
        );
    }

    /// 提交后的第二轮应命中，且互斥口径自洽。
    #[test]
    fn go_engine_hits_after_commit_and_split_is_consistent() {
        let eng = engines();

        let (_, _, p1, _mode) = eng.begin(&convo(3), 1, CacheEngineKind::Go, (1.0, 1.0, 1.0));
        eng.commit(p1, 1);

        let (_, _, p2, _mode) = eng.begin(&convo(5), 1, CacheEngineKind::Go, (1.0, 1.0, 1.0));
        let usage = eng.compute_pending(&p2, 1);
        assert!(usage.cache_read > 0, "第二轮应命中已提交前缀");
        eng.commit(p2, 1);

        // 下游共用的分摊链路：三者互斥相加必等于 total。
        let real_total = 12_345;
        let (input, creation, read) = usage.split_against_total(real_total);
        assert_eq!(input + creation + read, real_total);
        assert!(read > 0 && creation > 0);
    }

    /// 未启用的引擎不得 panic，安静退化为「无缓存」。
    #[test]
    fn missing_engines_degrade_to_empty_usage() {
        let empty = CacheEngines::default();
        let req = convo(3);
        for kind in [CacheEngineKind::Rust, CacheEngineKind::Go] {
            let (usage, muls, pending, _mode) = empty.begin(&req, 1, kind, (2.0, 3.0, 4.0));
            assert_eq!(usage.cache_read, 0);
            assert_eq!(usage.cache_covered_est, 0);
            // 引擎未启用时不该凭空缩放，倍率整组回落全局。
            // 传入非 1.0 的值才能区分「真的回落了」与「恰好都是 1.0」。
            assert_eq!(
                muls,
                UsageMultipliers::Rust {
                    input: 2.0,
                    output: 3.0,
                    cache_read: 4.0,
                    cache_creation: 4.0,
                },
                "{kind:?} 未启用时必须整组回落全局倍率"
            );
            assert_eq!(usage.split_against_total(500), (500, 0, 0));
            empty.commit(pending, 1); // 不得 panic
        }
    }

    /// 引擎 C / D 无缓存状态：`begin` 必须返回空 `CacheUsage` + `PendingCache::None`
    /// + 各自的 mode，且**不依赖 rust/go 句柄是否存在**。
    #[test]
    fn stateless_engines_need_no_trackers() {
        // 故意用 default()：rust / go 均为 None，C / D 仍须正常工作。
        let empty = CacheEngines::default();
        let req = convo(5);

        let (usage, muls, pending, mode) = empty.begin(&req, 1, CacheEngineKind::Real, (1.0, 1.0, 1.0));
        assert!(!usage.is_simulated(), "C 不产生模拟量");
        assert_eq!(usage.prompt_total_est, 0);
        assert_eq!(mode, UsageMode::Real);
        assert!(matches!(pending, PendingCache::None), "C 无写入意图");
        assert!(
            matches!(muls, UsageMultipliers::Real { .. }),
            "C 必须用自己的倍率，不回落全局"
        );

        let (usage, muls, pending, mode) = empty.begin(&req, 1, CacheEngineKind::NoCache, (1.0, 1.0, 1.0));
        assert!(!usage.is_simulated(), "D 不产生模拟量");
        assert_eq!(usage.prompt_total_est, 0);
        assert_eq!(mode, UsageMode::NoCache);
        assert!(matches!(pending, PendingCache::None), "D 无写入意图");
        assert!(matches!(muls, UsageMultipliers::NoCache { .. }));

        // commit 必须是安全空操作（不 panic）。
        empty.commit(PendingCache::None, 1);
    }

    /// Admin 热更新必须穿到请求路径。
    ///
    /// 复现 main.rs 的接线：外部建一个 `Arc` → 经 builder 注入 `AppState`（请求
    /// 路径）→ Admin 侧持同一个 `Arc` 改配置 → 请求路径的 `begin()` 必须立刻读到。
    ///
    /// 这一条专门盯 `with_stateless_multipliers` 是否真的存下了传入的 `Arc`。若它
    /// 忽略参数、退回 `CacheEngines::default()` 自建一份，Admin 改完返回成功、
    /// 请求路径却永远读 1.0 —— 静默失效，无报错、无日志。
    ///
    /// 注意不能改用 `eng.stateless.apply_*` 直接调：那样测的是原子量本身，
    /// builder 断了也照样通过（本测试早期版本正是如此，破坏 builder 后仍绿）。
    #[test]
    fn admin_hot_reload_reaches_request_path_through_builder() {
        use super::super::middleware::AppState;
        use crate::model::config::ToolCompatibilityMode;

        // main.rs 里这一份同时交给 AppState 和 AdminState。
        let shared = Arc::new(StatelessMultipliers::default());
        let state = AppState::new(false, ToolCompatibilityMode::default())
            .with_stateless_multipliers(shared.clone());
        let req = convo(3);

        // Admin 侧改配置：只动外部那个 Arc，不碰 state。
        shared.apply_real_config(crate::model::config::CacheEngineRealConfig {
            input_multiplier: 6.0,
            output_multiplier: 7.0,
            cache_read_multiplier: 8.0,
            cache_creation_multiplier: 9.0,
        });

        let (_, muls, _, _) = state
            .cache_engines
            .begin(&req, 1, CacheEngineKind::Real, (1.0, 1.0, 1.0));
        assert_eq!(
            muls.resolve(),
            (6.0, 7.0, 8.0, 9.0),
            "builder 必须存下传入的 Arc —— 否则 Admin 改倍率永远到不了请求路径"
        );
    }

    /// 引擎 C / D 的倍率热更新必须被 `begin` 立刻读到。
    ///
    /// 回归的是一个**静默失效**故障：若 Admin 侧与请求路径各持一份
    /// `Arc<StatelessMultipliers>`，改配置会返回成功而请求路径永远读到 1.0。
    #[test]
    fn stateless_multipliers_hot_reload_reaches_begin() {
        let eng = CacheEngines::default();
        let req = convo(3);

        // 初始为 1.0（不缩放）。
        let (_, muls, _, _) = eng.begin(&req, 1, CacheEngineKind::Real, (1.0, 1.0, 1.0));
        assert_eq!(
            muls.resolve(),
            (1.0, 1.0, 1.0, 1.0),
            "C 默认不缩放，且不受全局倍率影响"
        );

        eng.stateless
            .apply_real_config(crate::model::config::CacheEngineRealConfig {
                input_multiplier: 2.0,
                output_multiplier: 3.0,
                cache_read_multiplier: 4.0,
                cache_creation_multiplier: 5.0,
            });
        let (_, muls, _, _) = eng.begin(&req, 1, CacheEngineKind::Real, (1.0, 1.0, 1.0));
        assert_eq!(
            muls.resolve(),
            (2.0, 3.0, 4.0, 5.0),
            "改配置后 begin 必须立刻读到新倍率"
        );

        eng.stateless
            .apply_nocache_config(crate::model::config::CacheEngineNoCacheConfig {
                input_multiplier: 1.5,
                output_multiplier: 2.5,
            });
        let (_, muls, _, _) = eng.begin(&req, 1, CacheEngineKind::NoCache, (1.0, 1.0, 1.0));
        assert_eq!(
            muls.resolve(),
            (1.5, 2.5, 0.0, 0.0),
            "D 的 cache 倍率恒为 0：cache 结构性不存在"
        );
    }

    /// 克隆 `CacheEngines` 后仍共享同一份倍率存储 —— `AppState` 会被 axum 克隆到
    /// 每个请求，若克隆断开共享则热更新只对某一份生效。
    #[test]
    fn cloned_engines_share_stateless_multipliers() {
        let eng = CacheEngines::default();
        let cloned = eng.clone();

        cloned
            .stateless
            .apply_real_config(crate::model::config::CacheEngineRealConfig {
                input_multiplier: 7.0,
                output_multiplier: 1.0,
                cache_read_multiplier: 1.0,
                cache_creation_multiplier: 1.0,
            });

        // 对克隆体的改动必须对原体可见（同一个 Arc）。
        assert_eq!(
            eng.real_multipliers().0,
            7.0,
            "克隆必须共享倍率存储，否则热更新只影响一份"
        );
    }

    /// 极小正倍率在**四个引擎**上都必须原值往返。
    ///
    /// 四套倍率是四份独立存储（A 在 `CacheMeter`、B 在 `GoCacheTracker`、C/D 在
    /// `StatelessMultipliers`），曾经四份都用千分比整数 `(v * 1000.0).round()`，
    /// 于是 <0.0005 的正倍率一律被 round 成 0：配置校验放行（`sanitize_multiplier`
    /// 只要求 `is_finite() && > 0.0`，无下限），admin 回读也显示原值，但请求路径
    /// 乘的是 0 —— 客户端 usage 全归零，无报错无日志。
    ///
    /// 引擎 A 侧另有 [`super::cache_metering::tests::tiny_multipliers_survive_storage_roundtrip`]
    /// 覆盖「未设置回落全局兜底」那一维；这一条的作用是横向钉死四份存储，防止
    /// 只修其中一两份。断言用精确相等而非 `assert_ne!(_, 0.0)`：只钉非 0 的话，
    /// 把千分比换成百万分比也能过，而那只是把断点从 0.0005 推到 0.0000005。
    #[test]
    fn tiny_multipliers_survive_storage_on_all_engines() {
        // 引擎 A。
        let a = super::super::cache_metering::CacheMeter::new_with_config(
            None,
            CacheEngineRustConfig {
                input_multiplier: Some(0.0004),
                output_multiplier: Some(0.0003),
                cache_read_multiplier: Some(0.0006),
                cache_creation_multiplier: Some(0.0007),
                ..CacheEngineRustConfig::default()
            },
        );
        assert_eq!(
            a.multipliers((9.0, 9.0, 9.0)),
            (0.0004, 0.0003, 0.0006, 0.0007),
            "引擎 A 倍率被存储格式量化"
        );

        // 引擎 B。
        let b = GoCacheTracker::new(
            None,
            CacheEngineGoConfig {
                input_token_multiplier: 0.0004,
                output_multiplier: 0.0003,
                cache_read_multiplier: 0.0006,
                cache_creation_multiplier: 0.0007,
                ..go_cfg()
            },
        );
        assert_eq!(
            b.multipliers(),
            (0.0004, 0.0003, 0.0006, 0.0007),
            "引擎 B 倍率被存储格式量化"
        );

        // 引擎 C / D 共享一份存储，但走两套 setter，故分别验。
        let cd = StatelessMultipliers::default();
        cd.apply_real_config(crate::model::config::CacheEngineRealConfig {
            input_multiplier: 0.0004,
            output_multiplier: 0.0003,
            cache_read_multiplier: 0.0006,
            cache_creation_multiplier: 0.0007,
        });
        assert_eq!(
            cd.real(),
            (0.0004, 0.0003, 0.0006, 0.0007),
            "引擎 C 倍率被存储格式量化"
        );

        cd.apply_nocache_config(crate::model::config::CacheEngineNoCacheConfig {
            input_multiplier: 0.0004,
            output_multiplier: 0.0003,
        });
        assert_eq!(cd.nocache(), (0.0004, 0.0003), "引擎 D 倍率被存储格式量化");
    }

    /// 四引擎的口径分歧全部收敛在 `resolve_tokens`，故这里逐模式钉住其语义。
    #[test]
    fn resolve_tokens_covers_all_three_modes() {
        let simulated = CacheUsage {
            cache_read: 30,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };
        // 上游真实三元组：input=800, creation=120, read=400。
        let real = (800, 120, 400);

        // 本地估算的客户端 input（token::count_all_tokens 口径）。刻意取一个与
        // real 三元组无任何算术关系的值，使"引擎 D 用的是它、而非上游任何组合"
        // 可被唯一确定 —— 若取 1320 之类就无法区分是本地值还是上游合并值。
        let local_input = 777;

        // Simulated：按 estimate 比例分摊到 simulated_total_input，不看 real，也不看 local。
        let (i, cc, cr) = UsageMode::Simulated.resolve_tokens(simulated, 200, real, local_input);
        assert_eq!((i, cc, cr), (100, 40, 60), "覆盖率 50% → 缓存 100，其中 read 占 60%");

        // Real：原样返回上游真值，simulated_total_input 与 local_input 均被忽略。
        assert_eq!(
            UsageMode::Real.resolve_tokens(simulated, 200, real, local_input),
            real,
            "Real 模式不得改动上游真值"
        );

        // NoCache：完全不读上游 usage，input 取本地估算，cache 恒为 0。
        let (i, cc, cr) = UsageMode::NoCache.resolve_tokens(simulated, 200, real, local_input);
        assert_eq!((i, cc, cr), (777, 0, 0), "input 必须是本地估算值");
        assert_ne!(
            i,
            real.0 + real.1 + real.2,
            "引擎 D 不再合并上游三元组：上游可能是另一个反代，其 usage 已被加工过"
        );
        assert_ne!(i, real.0, "也不是上游 input 原值");
    }

    /// `Simulated` 但数据不支持模拟时必须自降级为 `Real`。
    ///
    /// 这是引擎 B 的结构性需要：`begin()` 返回 Simulated 时其 `CacheUsage` 仍是
    /// 全零占位，真值要等 `compute_pending` 在选定凭据后才填入。若不自兜底，
    /// 调用方的 `.max(1)` 会把 `input_tokens` 钉成 1，销毁上游真实值。
    #[test]
    fn simulated_mode_self_downgrades_when_usage_is_empty() {
        let empty = CacheUsage::default();
        assert!(!empty.is_simulated(), "全零 CacheUsage 不成立为模拟");
        let real = (800, 120, 400);

        assert_eq!(
            UsageMode::Simulated.resolve_tokens(empty, 1, real, 777),
            real,
            "mode 说模拟、数据却为空时必须回落上游真值"
        );
    }

    /// output 的口径分歧：只有引擎 D 用本地估算，其余三个用上游真值。
    #[test]
    fn resolve_output_only_localizes_for_nocache() {
        // 引擎 D：有本地值就用本地值。
        assert_eq!(
            UsageMode::NoCache.resolve_output(9999, Some(42)),
            42,
            "引擎 D 的 output 必须是本地估算，不是上游报的数"
        );

        // 其余三个引擎：即便本地值在场也不采用。
        for mode in [UsageMode::Simulated, UsageMode::Real] {
            assert_eq!(
                mode.resolve_output(9999, Some(42)),
                9999,
                "{mode:?} 的 output 应取上游真值"
            );
        }

        // 引擎 D 但本地值缺失（如上游未下发 content / 空流）：回落上游值，
        // 不得凭空报 0 —— 那会让客户端以为没产生输出。
        assert_eq!(
            UsageMode::NoCache.resolve_output(9999, None),
            9999,
            "本地估算不可得时必须回落上游值"
        );
    }

    /// 两引擎共用同一张 `CacheUsage` 出口，但数值口径不同 —— 这是并存对比的前提。
    #[test]
    fn both_engines_produce_usable_usage_for_same_request() {
        let eng = engines();
        let req = convo(5);
        let (a, _, _, _mode) = eng.begin(&req, 1, CacheEngineKind::Rust, (1.0, 1.0, 1.0));
        let (_, _, pending, _mode) = eng.begin(&req, 1, CacheEngineKind::Go, (1.0, 1.0, 1.0));
        let b = eng.compute_pending(&pending, 1);
        assert!(a.prompt_total_est > 0 && b.prompt_total_est > 0);
        // 结构噪声计入 → go 引擎的 estimate 总量应更大。
        assert!(
            b.prompt_total_est > a.prompt_total_est,
            "go 引擎喂 canonical JSON，token 口径应大于引擎 A 的纯文本口径 (a={}, b={})",
            a.prompt_total_est,
            b.prompt_total_est
        );
    }
}
