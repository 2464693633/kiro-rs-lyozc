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
use super::cache_metering_go::{GoCacheTracker, PromptCacheProfile, build_claude_profile};
use super::types::MessagesRequest;
use std::sync::Arc;

/// 客户端 Key 选择的缓存模拟引擎。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheEngineKind {
    /// 引擎 A（现有实现，带会话隔离）。老数据缺该字段时的默认值。
    #[default]
    Rust,
    /// 引擎 B（移植自 kiro-go，按实际账号共享指纹表）。
    Go,
}

impl CacheEngineKind {
    /// 供 `skip_serializing_if` 使用：默认值不写进 JSON，保持老文件形状不变。
    pub fn is_default(&self) -> bool {
        matches!(self, CacheEngineKind::Rust)
    }
}

/// 两套引擎的共享句柄。任一为 `None` 表示该引擎未启用。
#[derive(Clone, Default)]
pub struct CacheEngines {
    pub rust: Option<SharedCacheMeter>,
    pub go: Option<Arc<GoCacheTracker>>,
}

/// 本次请求下发前应使用的缩放倍率。
///
/// 两套引擎各用自己的一组，互不影响 —— 这样同一部署里可以对两套独立调参并直接
/// 对比效果。若共用全局膨胀倍率，改一处会同时动两套，对比就失去意义。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsageMultipliers {
    /// 引擎 A：沿用 provider 的全局膨胀倍率（`input` / `output` / `cache`）。
    Global,
    /// 引擎 B：go 引擎专属倍率。
    ///
    /// `cache_creation` 默认 1.0 —— 对齐 Go 侧 `buildClaudeUsageMap`，它只缩放
    /// `input_tokens` 与 `cache_read_input_tokens`。调离 1.0 会削弱两套引擎在
    /// 「creation/read 划分」这个维度上的可比性，但作为运营旋钮开放。
    ///
    /// `output` 恒不缩放（Go 侧无此倍率）。
    Go {
        input: f64,
        cache_read: f64,
        cache_creation: f64,
    },
}

impl UsageMultipliers {
    /// 把「全局膨胀倍率三元组」换算成本次请求实际生效的四元组
    /// `(input, output, cache_read, cache_creation)`。
    ///
    /// 引擎 A 原样透传（creation 与 read 同值 = 原行为）；引擎 B 用自己那组，
    /// `output` 恒为 1.0（Go 侧无 output 倍率）。
    pub fn resolve(self, global: (f64, f64, f64)) -> (f64, f64, f64, f64) {
        match self {
            UsageMultipliers::Global => (global.0, global.1, global.2, global.2),
            UsageMultipliers::Go {
                input,
                cache_read,
                cache_creation,
            } => (input, 1.0, cache_read, cache_creation),
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
    /// 返回费用对比所需的两套客户端 token 倍率。
    ///
    /// Rust 始终使用全局倍率；Go 使用自身配置的倍率。Go 未启用时退回
    /// Rust/全局倍率，避免未启用的引擎凭空改变 token 数。
    pub fn billing_multipliers(
        &self,
        global: (f64, f64, f64),
    ) -> ((f64, f64, f64, f64), (f64, f64, f64, f64)) {
        let rust = UsageMultipliers::Global.resolve(global);
        let go = self
            .go
            .as_ref()
            .map(|tracker| {
                let (input, cache_read, cache_creation) = tracker.multipliers();
                UsageMultipliers::Go {
                    input,
                    cache_read,
                    cache_creation,
                }
                .resolve(global)
            })
            .unwrap_or(rust);
        (rust, go)
    }

    /// 阶段一：算出本次请求的缓存覆盖情况。
    ///
    /// 返回的 [`CacheUsage`] 是 estimate 口径，由调用方在拿到真实 total 后用
    /// `split_against_total` 做互斥分摊 —— 两套引擎共用这条下游链路。
    pub fn begin(
        &self,
        req: &MessagesRequest,
        key_id: u64,
        kind: CacheEngineKind,
    ) -> (CacheUsage, UsageMultipliers, PendingCache) {
        match kind {
            CacheEngineKind::Rust => {
                let usage = self
                    .rust
                    .as_ref()
                    .map(|cache| super::cache_metering::compute_cache_usage(cache, req, key_id))
                    .unwrap_or_default();
                (usage, UsageMultipliers::Global, PendingCache::None)
            }
            CacheEngineKind::Go => {
                let Some(tracker) = self.go.as_ref() else {
                    // 引擎未启用：退化为无缓存，倍率也回落全局（不该凭空缩放）。
                    return (
                        CacheUsage::default(),
                        UsageMultipliers::Global,
                        PendingCache::None,
                    );
                };
                let (input_mul, cache_read_mul, cache_creation_mul) = tracker.multipliers();
                let muls = UsageMultipliers::Go {
                    input: input_mul,
                    cache_read: cache_read_mul,
                    cache_creation: cache_creation_mul,
                };
                // 使用与 kiro-go 相同的内容型请求 token 估算作为分母；不能传 0，
                // 否则 profile 会退化为 canonical wrapper 总量，导致缓存覆盖比例偏低。
                let estimated_total = super::cache_metering_go::estimate_claude_request_input_tokens(req);
                let Some(profile) = build_claude_profile(req, estimated_total, tracker.effective_ttl_ms()) else {
                    // 无断点：无缓存，但 Key 选的仍是 go 引擎，倍率照其配置生效。
                    return (CacheUsage::default(), muls, PendingCache::None);
                };
                // Provider selects the credential inside call_api*. Compute only after
                // that selection so failover cannot reuse another account's cache.
                (CacheUsage::default(), muls, PendingCache::Go(profile))
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
        }
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

    /// `resolve` 是「两套引擎各用自己倍率」的唯一落点，也是「go 不缩放 creation」
    /// 这条规则的唯一实现处 —— 单独钉住。
    #[test]
    fn resolve_keeps_engines_on_their_own_multipliers() {
        let global = (2.0, 3.0, 4.0);

        // 引擎 A：原样透传，creation 与 read 同值（= 迁移前行为）
        let (i, o, cr, cc) = UsageMultipliers::Global.resolve(global);
        assert_eq!((i, o, cr, cc), (2.0, 3.0, 4.0, 4.0));

        // 引擎 B：改用自己那组，且完全无视传入的全局值
        // 默认 creation=1.0（Go 原实现行为）
        let go = UsageMultipliers::Go { input: 1.5, cache_read: 3.0, cache_creation: 1.0 };
        let (i, o, cr, cc) = go.resolve(global);
        assert_eq!(i, 1.5, "input 用 go 自己的");
        assert_eq!(cr, 3.0, "cache_read 用 go 自己的");
        assert_eq!(cc, 1.0, "creation 默认不缩放 = Go 原实现");
        assert_eq!(o, 1.0, "Go 侧不缩放 output");

        // 全局值变化不得影响 go 的结果
        assert_eq!(go.resolve((99.0, 99.0, 99.0)), (1.5, 1.0, 3.0, 1.0));

        // creation 可独立调离 1.0，且不影响其余三项
        let go2 = UsageMultipliers::Go { input: 1.5, cache_read: 3.0, cache_creation: 0.5 };
        assert_eq!(go2.resolve(global), (1.5, 1.0, 3.0, 0.5), "creation 应可独立缩放");
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

        let (rust, go) = eng.billing_multipliers((2.0, 3.0, 4.0));
        assert_eq!(rust, (2.0, 3.0, 4.0, 4.0));
        assert_eq!(go, (1.5, 1.0, 2.5, 0.75));
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
        let (actual, muls, pending) = eng.begin(&req, 7, CacheEngineKind::Rust);

        assert_eq!(actual.cache_read, expected.cache_read);
        assert_eq!(actual.cache_covered_est, expected.cache_covered_est);
        assert_eq!(actual.prompt_total_est, expected.prompt_total_est);
        assert!(matches!(pending, PendingCache::None), "引擎 A 无待提交状态");
        assert_eq!(
            muls,
            UsageMultipliers::Global,
            "引擎 A 必须沿用全局膨胀倍率"
        );
    }

    /// 引擎 A 的 commit 是空操作：调用它不得产生任何可观测变化。
    #[test]
    fn rust_engine_commit_is_noop() {
        let eng = engines();
        let req = convo(5);
        let (_, _, pending) = eng.begin(&req, 1, CacheEngineKind::Rust);
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

        let (_, muls, pending) = eng.begin(&req, 1, CacheEngineKind::Go);
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

        let (_, _, p1) = eng.begin(&convo(3), 1, CacheEngineKind::Go);
        eng.commit(p1, 1);

        let (_, _, p2) = eng.begin(&convo(5), 1, CacheEngineKind::Go);
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
            let (usage, muls, pending) = empty.begin(&req, 1, kind);
            assert_eq!(usage.cache_read, 0);
            assert_eq!(usage.cache_covered_est, 0);
            // 引擎未启用时不该凭空缩放，倍率回落全局
            assert_eq!(muls, UsageMultipliers::Global);
            assert_eq!(usage.split_against_total(500), (500, 0, 0));
            empty.commit(pending, 1); // 不得 panic
        }
    }

    /// 两引擎共用同一张 `CacheUsage` 出口，但数值口径不同 —— 这是并存对比的前提。
    #[test]
    fn both_engines_produce_usable_usage_for_same_request() {
        let eng = engines();
        let req = convo(5);
        let (a, _, _) = eng.begin(&req, 1, CacheEngineKind::Rust);
        let (_, _, pending) = eng.begin(&req, 1, CacheEngineKind::Go);
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
