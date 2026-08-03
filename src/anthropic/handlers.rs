//! Anthropic API Handler 函数

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceKeySource, TraceRecord, TraceSink, outcome,
};
use crate::admin::usage_stats::{
    SharedAggregator, SharedRecorder, TokenUsageBreakdown, UsageRecord,
};
use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::token_manager::ModelDiscoveryError;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request_with_mode};
use super::middleware::{AppState, KeyContext, RawBody};
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub key_id: u64,
    pub model: String,
    pub started_at: Instant,
    /// 缓存引擎句柄，用于在成功路径提交引擎 B 的写入（两阶段的第二阶段）。
    cache_engines: super::cache_engine::CacheEngines,
    /// 待提交的写入意图。
    ///
    /// 挂在 hook 上而非各 handler 的 return 处：`record` 已覆盖全部成功 / 失败
    /// 路径（含流式那条已被 move 进闭包的），在此提交最不易漏。
    ///
    /// `Arc<Mutex<_>>` 而非裸 Mutex：本结构体是 `Clone` 的，各克隆必须共享同一
    /// 个槽，否则某个克隆提交后其他克隆仍持有旧 profile 会重复写入。`take()`
    /// 使提交天然幂等 —— 同一请求多次调 `record` 也只写一次。
    pending_cache: std::sync::Arc<parking_lot::Mutex<Option<super::cache_engine::PendingCache>>>,
    billing_usage: std::sync::Arc<parking_lot::Mutex<Option<BillingSnapshot>>>,
    billing_request: std::sync::Arc<
        parking_lot::Mutex<
            Option<(
                super::cache_engine::CacheEngineKind,
                super::cache_metering::CacheUsage,
            )>,
        >,
    >,
}

/// 本次请求的计费快照：**上游真实**与**客户端被计费**一一配对。
///
/// 取代此前的 `(upstream, Option<rust>, Option<go>)` 三元组。那个形状把「哪个引擎」
/// 编码在「哪个 Option 非空」里，于是引擎 C / D 无处安放（两槽都得留空），而
/// `upstream` 只有一个槽 —— 混合流量聚合后它是各引擎之和，与单个引擎的 client 值
/// 不可比。改成显式带 `engine` 的配对后，四引擎一律平等，且两个口径**必然同源**。
#[derive(Debug, Clone, Copy)]
pub(crate) struct BillingSnapshot {
    /// 本次请求所用引擎。
    pub engine: super::cache_engine::CacheEngineKind,
    /// 上游 API 真实上报的用量。
    pub upstream: TokenUsageBreakdown,
    /// 客户端实际被计费的用量（已乘该引擎倍率）。
    pub client: TokenUsageBreakdown,
}

/// 本次请求下发给客户端的**膨胀前**四元组，口径已由 [`UsageMode`] 定完。
///
/// 上游路径两条分支都已算出这四个数（非流式取响应体、流式取 `message_start` +
/// 累积输出），故计费快照直接复用，不再自行推导 —— 见 [`scaled_billing_usage`]。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientTokens {
    pub input: i32,
    pub output: i32,
    pub cache_creation: i32,
    pub cache_read: i32,
}

/// 把「客户端被计费的膨胀前口径」套上倍率，得到计费快照。
///
/// **不再自行调 `resolve_tokens`**：口径分歧（模拟分摊 / 上游真值 / 本地估算）已在
/// 下发给客户端时定完，这里只负责乘倍率。此前本函数从 `upstream` + `mode` 重新推
/// 导一遍，等于同一套规则写两处 —— 而引擎 D 的本地估算值在这里根本取不到（它不
/// 读上游 usage），重导必然得出与客户端所见不同的数。改成传入已解析值后，
/// 「计费记录 == 客户端所见 × 倍率」由构造保证，不依赖两处逻辑保持同步。
fn scaled_billing_usage(
    client: ClientTokens,
    multipliers: (f64, f64, f64, f64),
) -> TokenUsageBreakdown {
    let scale = |value: i32, multiplier: f64| (value.max(0) as f64 * multiplier).round() as u64;
    TokenUsageBreakdown {
        input_tokens: scale(client.input, multipliers.0),
        output_tokens: scale(client.output, multipliers.1),
        cache_creation_tokens: scale(client.cache_creation, multipliers.3),
        cache_read_tokens: scale(client.cache_read, multipliers.2),
    }
}

/// 算出本次请求的「上游真实 ↔ 客户端被计费」配对快照。
///
/// 每个请求只跑一个引擎，故只解析该引擎的倍率。**不再做槽位分配** —— v1 要在
/// `rust_usage` / `go_usage` 里挑一个槽填，这既让 C / D 无处安放，又使「上游真实」
/// 成了所有引擎共用的一个槽（混合流量下它累加的是全部引擎之和，与单引擎的客户端
/// 计费不可比）。改成带 `engine` 标签的配对后，四引擎一视同仁，且任意聚合层级上
/// 两个口径都来自同一批请求。
fn selected_billing_snapshot(
    kind: super::cache_engine::CacheEngineKind,
    client_tokens: ClientTokens,
    upstream: TokenUsageBreakdown,
    multipliers: (f64, f64, f64, f64),
) -> BillingSnapshot {
    BillingSnapshot {
        engine: kind,
        upstream,
        client: scaled_billing_usage(client_tokens, multipliers),
    }
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            key_id,
            model,
            started_at: Instant::now(),
            cache_engines: state.cache_engines.clone(),
            pending_cache: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            billing_usage: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            billing_request: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// 登记本次请求待提交的缓存写入。仅引擎 B 会产生非 `None` 的意图。
    fn set_pending_cache(&self, pending: super::cache_engine::PendingCache) {
        *self.pending_cache.lock() = Some(pending);
    }

    /// 提交缓存写入。只在 `status == "success"` 时调用，使失败请求不污染缓存
    /// （对齐 Go：`Update` 只在成功分支执行）。
    fn resolve_pending_cache(&self, credential_id: u64) -> super::cache_metering::CacheUsage {
        let selected = self.billing_request.lock().as_ref().copied();
        let Some((kind, begin_usage)) = selected else {
            return super::cache_metering::CacheUsage::default();
        };
        if kind == super::cache_engine::CacheEngineKind::Rust {
            return begin_usage;
        }

        let pending = self.pending_cache.lock();
        match pending.as_ref() {
            Some(pending) => self.cache_engines.compute_pending(pending, credential_id),
            None => super::cache_metering::CacheUsage::default(),
        }
    }

    fn set_billing_usage(&self, snapshot: BillingSnapshot) {
        *self.billing_usage.lock() = Some(snapshot);
    }

    fn set_billing_request(
        &self,
        kind: super::cache_engine::CacheEngineKind,
        usage: super::cache_metering::CacheUsage,
    ) {
        *self.billing_request.lock() = Some((kind, usage));
    }

    fn set_selected_billing_usage(&self, usage: super::cache_metering::CacheUsage) {
        if let Some((_, selected)) = self.billing_request.lock().as_mut() {
            *selected = usage;
        }
    }

    /// 记录本次请求的「上游真实 / 客户端被计费」快照。
    ///
    /// `client` 是**已解析、未膨胀**的客户端口径四元组，由上游响应处理函数算出
    /// （`UpstreamStreamUsage` 或 `handle_upstream_non_stream_response` 的返回值）。
    /// 此处不再自行解析 —— 见 [`scaled_billing_usage`] 的文档注释。
    fn set_upstream_billing_usage(
        &self,
        upstream: TokenUsageBreakdown,
        client: ClientTokens,
        global_multipliers: (f64, f64, f64),
    ) -> Option<BillingSnapshot> {
        let Some((kind, _)) = self.billing_request.lock().as_ref().copied() else {
            return None;
        };
        let multipliers = self.cache_engines.multipliers_for(kind, global_multipliers);
        // 只解析本次请求所用引擎的倍率。另外三套不参与本次计费，也不该因为「对比」
        // 而被凭空填数 —— 逐引擎配对存储后，每条记录只属于一个引擎。
        let snapshot = selected_billing_snapshot(kind, client, upstream, multipliers);
        self.set_billing_usage(snapshot);
        Some(snapshot)
    }

    fn commit_pending_cache(&self, credential_id: u64) {
        if let Some(pending) = self.pending_cache.lock().take() {
            self.cache_engines.commit(pending, credential_id);
        }
    }

    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        // 有快照即为上游请求：快照只在上游路径产生（见 set_upstream_billing_usage）。
        let snapshot = self.billing_usage.lock().take();
        let (is_upstream, engine, upstream_usage, client_usage) = match snapshot {
            Some(s) => (
                true,
                Some(s.engine.as_str().to_string()),
                Some(s.upstream),
                Some(s.client),
            ),
            None => (false, None, None, None),
        };
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
            is_upstream: credential_id != 0 && is_upstream,
            engine,
            upstream_usage,
            client_usage,
            // v2 一律写 engine + client_usage；这两个字段只为读老 JSONL 保留。
            rust_usage: None,
            go_usage: None,
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        // 引擎 B 的第二阶段：仅成功时落写。放在 key_id 判断之外，因为系统 Key
        // （id=0）的请求同样需要提交缓存。
        if status == "success" {
            self.commit_pending_cache(credential_id);
        }

        if status == "success" && self.key_id != 0 {
            if let Some(m) = &self.client_keys {
                m.record_usage(
                    self.key_id,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_creation_tokens,
                    rec.cache_read_tokens,
                    rec.credits,
                );
            }
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    model: String,
    is_stream: bool,
    started_at: Instant,
    /// 首个上游 chunk 到达时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
    billing_usage: parking_lot::Mutex<Option<BillingSnapshot>>,
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）
#[derive(Clone, Copy, Default)]
pub(crate) struct TraceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub credits: f64,
}

impl TraceUsage {
    /// 错误早退等无用量场景
    pub fn zero() -> Self {
        Self::default()
    }
}

struct RequestTraceOptions {
    key_ctx: KeyContext,
    model: String,
    is_stream: bool,
}

impl RequestTracer {
    fn new(state: &AppState, options: RequestTraceOptions) -> Self {
        Self {
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            model: options.model,
            is_stream: options.is_stream,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            billing_usage: parking_lot::Mutex::new(None),
        }
    }

    fn set_billing_usage(&self, usage: BillingSnapshot) {
        *self.billing_usage.lock() = Some(usage);
    }

    /// 标记首个上游 chunk 到达（幂等，仅记录第一次）
    pub fn mark_first_token(&self) {
        let mut slot = self.first_token_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
        usage: TraceUsage,
    ) {
        let Some(store) = &self.store else { return };
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let first_token_ms = self
            .first_token_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let billing_usage = self.billing_usage.lock().take();
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(|s| s.to_string()),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            credits: usage.credits,
            first_token_ms,
            // 逐引擎配对：engine + (upstream, client) 三者同源同写，使 trace 行里
            // 「上游真实 ↔ 客户端被计费」始终属于同一次请求、同一个引擎。
            engine: billing_usage
                .as_ref()
                .map(|snapshot| snapshot.engine.as_str().to_string()),
            upstream_usage: billing_usage.as_ref().map(|snapshot| snapshot.upstream),
            client_usage: billing_usage.as_ref().map(|snapshot| snapshot.client),
            // v1 兼容列不再写入：新行一律用上面那对。老行由 normalized() 折叠。
            rust_usage: None,
            go_usage: None,
            attempts,
        };
        store.insert(&rec);
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, attempt: TraceAttempt) {
        self.attempts.lock().push(attempt);
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(match last.as_str() {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    })
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if item.get("type").and_then(|v| v.as_str()) != Some("image") {
                    continue;
                }
                let Some(src) = item.get("source") else {
                    continue;
                };
                if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
                    continue;
                }
                let n = src
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                count += 1;
                total += n;
                if n > largest {
                    largest = n;
                }
            }
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    if let Some(rate_limit) = err.downcast_ref::<crate::kiro::error::UpstreamRateLimitError>() {
        tracing::warn!(error = %err, "上游限流（映射为 429）");
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                "Upstream rate limit exceeded. Retry later.",
            )),
        )
            .into_response();
        if let Some(value) = rate_limit
            .retry_after()
            .and_then(|value| value.parse::<header::HeaderValue>().ok())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }

    // Bedrock client-side validation errors (tool_use <-> tool_result mismatch, invalid message sequence, etc.)
    // The root cause is the client's own messages array, not an upstream failure, so it must not map to 5xx
    // otherwise it triggers an upstream cooldown that amplifies one client error into a 30+ burst of 503s.
    // Detection is centralized in the endpoint layer (single source of truth for the markers); the provider
    // already bails out without retry on these, and this mapping is the client-facing safety net.
    if crate::kiro::endpoint::default_is_client_validation_error(&err_str) {
        tracing::warn!(
            error = %err,
            "client messages array violates the protocol (Bedrock validation; mapped to 400 to avoid a false cooldown)"
        );
        // Return a stable, client-facing message and avoid echoing the raw upstream
        // error string (which can carry request IDs or internal validation details).
        // The full error is already logged above for diagnostics.
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.".to_string(),
            )),
        )
            .into_response();
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            "Upstream API request failed.",
        )),
    )
        .into_response()
}

/// 计算 Anthropic usage 口径的 input_tokens
fn resolve_usage_input_tokens(
    fallback_total_input_tokens: i32,
    context_total_input_tokens: Option<i32>,
) -> i32 {
    context_total_input_tokens.unwrap_or(fallback_total_input_tokens)
}

fn validate_max_tokens(max_tokens: i32) -> Result<(), ErrorResponse> {
    if max_tokens <= 0 {
        Err(ErrorResponse::new(
            "invalid_request_error",
            "max_tokens must be greater than 0",
        ))
    } else {
        Ok(())
    }
}

/// 上游直通的请求体：客户端原文，仅在缺 `max_tokens` 时补上该字段。
///
/// Anthropic Messages API 的 `max_tokens` 是必填项，而 [`MessagesRequest`] 给它挂了
/// `default = 32000` —— 客户端不发时解析照样能过，但**原文里确实没有这个键**，直接
/// 转发会被上游以 `max_tokens: field required` 拒掉。
///
/// **只补这一个字段**：它是 API 强制要求的最小集。往返序列化会把所有缺省字段一并
/// 实体化（`thinking.budget_tokens` 被补成 20000、`system`/`tools` 写成显式 `null`），
/// 那才是上游报错的成因 —— 见 [`super::middleware::RawBody`]。
///
/// 原文已带 `max_tokens` 时**原样返回，逐字节不变**，这是绝大多数请求走的路径。只有
/// 需要注入时才重新序列化，此时键序会被 serde_json 的 `BTreeMap` 重排（本 crate 未开
/// `preserve_order`）；JSON 对象的键序无语义，可接受。
fn ensure_max_tokens(raw: String, fallback: i32) -> String {
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(&raw)
    else {
        // 不是 JSON 对象：原样转发。这种请求本就不合法，交给上游报错，不在这里加工。
        return raw;
    };
    if map.contains_key("max_tokens") {
        return raw;
    }
    map.insert("max_tokens".to_string(), serde_json::json!(fallback));
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or(raw)
}

fn merge_token_limits(target: &mut Option<TokenLimits>, incoming: Option<TokenLimits>) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        Some(target) => {
            target.max_input_tokens = target.max_input_tokens.max(incoming.max_input_tokens);
            target.max_output_tokens = target.max_output_tokens.max(incoming.max_output_tokens);
        }
        None => *target = Some(incoming),
    }
}

fn infer_model_owner(model_id: &str) -> &'static str {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("claude-") {
        "anthropic"
    } else if id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        "openai"
    } else {
        "kiro"
    }
}

fn model_from_upstream(upstream: UpstreamModel) -> Model {
    let max_tokens = upstream
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_output_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(64_000);
    Model {
        display_name: upstream
            .model_name
            .clone()
            .unwrap_or_else(|| upstream.model_id.clone()),
        owned_by: infer_model_owner(&upstream.model_id).to_string(),
        id: upstream.model_id,
        object: "model".to_string(),
        created: 0,
        model_type: "chat".to_string(),
        max_tokens,
    }
}

fn aggregate_available_models_with_custom(
    upstream_models: Vec<UpstreamModel>,
    custom_models: &[crate::model::config::CustomModel],
) -> Vec<Model> {
    let mut merged_upstream: BTreeMap<String, UpstreamModel> = BTreeMap::new();
    for incoming in upstream_models {
        match merged_upstream.get_mut(&incoming.model_id) {
            Some(existing) => {
                if existing.model_name.is_none() {
                    existing.model_name = incoming.model_name;
                }
                if existing.description.is_none() {
                    existing.description = incoming.description;
                }
                merge_token_limits(&mut existing.token_limits, incoming.token_limits);
            }
            None => {
                merged_upstream.insert(incoming.model_id.clone(), incoming);
            }
        }
    }

    let mut models: BTreeMap<String, Model> = BTreeMap::new();
    for upstream in merged_upstream.into_values() {
        let model = model_from_upstream(upstream);
        models.insert(model.id.clone(), model);
    }

    // 自定义别名最后写入，同名时其展示元数据优先于动态条目。
    for custom in custom_models {
        let model = Model {
            id: custom.id.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: custom
                .owned_by
                .clone()
                .unwrap_or_else(|| "custom".to_string()),
            display_name: custom
                .display_name
                .clone()
                .unwrap_or_else(|| custom.id.clone()),
            model_type: "chat".to_string(),
            max_tokens: custom.max_tokens.unwrap_or(64_000),
        };
        models.insert(model.id.clone(), model);
    }

    models.into_values().collect()
}

fn aggregate_available_models(upstream_models: Vec<UpstreamModel>) -> Vec<Model> {
    aggregate_available_models_with_custom(upstream_models, crate::model::custom_models::all())
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!("Received GET /v1/models request");

    let Some(provider) = &state.kiro_provider else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "Kiro API provider not configured",
            )),
        )
            .into_response();
    };

    let upstream = match provider
        .token_manager()
        .discover_models_for_group(key_ctx.group.as_deref())
        .await
    {
        Ok(models) => models,
        Err(ModelDiscoveryError::NoAvailableCredentials) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "No available credentials for this API key",
                )),
            )
                .into_response();
        }
        Err(error @ ModelDiscoveryError::ColdStartFailed { .. }) => {
            tracing::warn!("动态模型列表加载失败: {}", error);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "Unable to load available models from upstream",
                )),
            )
                .into_response();
        }
    };

    let models = aggregate_available_models(upstream);
    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
    .into_response()
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: axum::http::HeaderMap,
    raw_body: Option<Extension<RawBody>>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 上游凭据直通转发的是客户端原文，不能用 `to_string(&payload)`：往返序列化会把
    // serde 默认值实体化进 body（最典型的是 `thinking.budget_tokens` 被补成 20000，
    // 客户端发 `disabled` 时上游直接 400）。见 [`RawBody`]。
    let anthropic_body_raw = raw_body
        .and_then(|Extension(RawBody(bytes))| String::from_utf8(bytes.to_vec()).ok())
        .map(|raw| ensure_max_tokens(raw, payload.max_tokens))
        .unwrap_or_else(|| {
            // 取不到原文说明 capture_raw_body 未挂载，或 body 非 UTF-8。退回往返序列化
            // 保可用性，但上游直通仍可能因默认值注入被拒 —— 故留一条 warn 便于定位。
            tracing::warn!("未取到请求体原文，退回往返序列化（上游直通可能被拒）");
            serde_json::to_string(&payload).unwrap_or_default()
        });
    // 提取客户端 anthropic-beta 头，透传给上游（kiro.rs 需要它启用扩展思考等功能）
    let upstream_beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置（仅 Kiro 路径使用）
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;
        // 缓存模拟：WebSearch 也要走 begin()，否则这条路径的请求既拿不到模拟
        // 缓存（cache_* 恒为 0），也不进缓存表 —— 会话中间夹一次 WebSearch 就
        // 断掉后续请求的前缀链。引擎 B 的写入在下面 record("success") 时提交。
        let (ws_cache_usage, _ws_multipliers, ws_pending, _ws_mode) =
            state
                .cache_engines
                .begin(
                    &payload,
                    key_ctx.key_id,
                    key_ctx.cache_engine,
                    provider.get_inflation_multipliers(),
                );
        hook.set_billing_request(key_ctx.cache_engine, ws_cache_usage);
        hook.set_pending_cache(ws_pending);

        let resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
            ws_cache_usage,
        )
        .await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        // 记账口径与下发给客户端的 usage 保持一致（同一 split 结果）
        let (rec_input, rec_cc, rec_cr) = ws_cache_usage.split_against_total(input_tokens);
        hook.record(0, rec_input, 0, rec_cc, rec_cr, 0.0, status);
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools: web_search coexists with other tools, use internal agentic loop
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
        )
        .await;
    }
    // 转换请求
    let conversion_result = match convert_request_with_mode(&payload, state.tool_compatibility_mode)
    {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // 缓存模拟：按客户端 Key 选中的引擎查缓存覆盖情况（estimate 口径）。
    // 真实 input/cache 互斥分摊在拿到 total 真值时进行。
    //
    // 引擎 B 是两阶段的：此处只查，写入意图存进 hook，待 `record("success")` 时提交。
    // 引擎 A 在 begin 内已完成查+写，其 pending 为 None、commit 是空操作。
    let (cache_usage, cache_multipliers, pending_cache, usage_mode) =
        state
            .cache_engines
            .begin(
                    &payload,
                    key_ctx.key_id,
                    key_ctx.cache_engine,
                    provider.get_inflation_multipliers(),
                );
    hook.set_billing_request(key_ctx.cache_engine, cache_usage);
    hook.set_pending_cache(pending_cache);

    // 序列化 Anthropic 格式请求体：使用 override_thinking 之前捕获的原始版本（上游直通时使用）
    // anthropic_body_raw 已在 override_thinking 前捕获，此处直接使用

    if payload.stream {
        // 流式响应
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
            },
        ));
        handle_stream_request(
            provider,
            &request_body,
            &anthropic_body_raw,
            &payload.model,
            total_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            cache_multipliers,
            usage_mode,
            tracer,
            upstream_beta,
            key_ctx.group.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &anthropic_body_raw,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            cache_multipliers,
            usage_mode,
            tracer,
            upstream_beta,
            key_ctx.group.clone(),
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    anthropic_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    _cache_usage: super::cache_metering::CacheUsage,
    cache_multipliers: super::cache_engine::UsageMultipliers,
    usage_mode: super::cache_engine::UsageMode,
    tracer: std::sync::Arc<RequestTracer>,
    upstream_beta: Option<String>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移 + 上游直通）
    let call_result = match provider
        .call_api_stream_dual(
            request_body,
            Some(anthropic_body),
            upstream_beta.as_deref(),
            Some(tracer.as_ref()),
            group.as_deref(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            // 重试链路全部失败、未开始返回内容：error_type 取最后一跳分类
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let cache_usage = hook.resolve_pending_cache(call_result.credential_id);
    hook.set_selected_billing_usage(cache_usage);
    let simulated_total_input = cache_usage.prompt_total_est.max(1);

    // 上游凭据直通：应用膨胀倍率 + 模拟缓存，流结束后回调 hook.record
    if call_result.is_upstream {
        let credential_id = call_result.credential_id;
        let (input_mul, output_mul, cache_mul, cache_creation_mul) =
            cache_multipliers.resolve();
        let (resp, usage_rx) = super::upstream::handle_upstream_stream_response_with_inflation(
            call_result.response,
            input_mul,
            output_mul,
            cache_mul,
            cache_creation_mul,
            simulated_total_input,
            cache_usage,
            usage_mode,
            // 引擎 D 的 input 口径：客户端请求的本地估算（token::count_all_tokens），
            // 与上游报的 usage 无关 —— 上游可能是另一个反代，其 usage 已被加工过。
            input_tokens,
        );
        // 流结束后在后台任务里记录真实用量（不阻塞客户端响应）
        tokio::spawn(async move {
            let usage = usage_rx.await.unwrap_or_default();
            if let Some(billing_usage) = hook
                .set_upstream_billing_usage(
                    usage.raw_usage,
                    ClientTokens {
                        input: usage.input_tokens,
                        output: usage.output_tokens,
                        cache_creation: usage.cache_creation_tokens,
                        cache_read: usage.cache_read_tokens,
                    },
                    provider.get_inflation_multipliers(),
                )
            {
                tracer.set_billing_usage(billing_usage);
            }
            hook.record(
                credential_id,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_tokens,
                usage.cache_read_tokens,
                0.0,
                "success",
            );
            tracer.finalize(
                "success",
                None,
                None,
                None,
                TraceUsage {
                    input_tokens: usage.input_tokens.max(0) as u64,
                    output_tokens: usage.output_tokens.max(0) as u64,
                    cache_creation_tokens: usage.cache_creation_tokens.max(0) as u64,
                    cache_read_tokens: usage.cache_read_tokens.max(0) as u64,
                    credits: 0.0,
                },
            );
        });
        return resp;
    }

    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    ctx.cache_usage = cache_usage;
    // 设置膨胀倍率
    let (input_mul, output_mul, cache_mul, cache_creation_mul) =
        cache_multipliers.resolve();
    ctx.input_inflation_multiplier = input_mul;
    ctx.output_inflation_multiplier = output_mul;
    ctx.cache_inflation_multiplier = cache_mul;
    ctx.cache_creation_inflation_multiplier = cache_creation_mul;

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(response, ctx, initial_events, hook, credential_id, tracer);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), hook, credential_id, tracer, 0u64),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, hook, credential_id, tracer, mut sent_bytes)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            tracer.mark_first_token();
                            sent_bytes += chunk.len() as u64;
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束（记为 error）
                            let final_events = ctx.generate_final_events();
                            record_stream_usage(&hook, &ctx, credential_id, "error");
                            // 已开始返回内容后上游断流：标记为 interrupted，带已发送字节数
                            tracer.finalize(
                                "interrupted",
                                Some(outcome::STREAM_INTERRUPTED),
                                Some(&e.to_string()),
                                Some(sent_bytes),
                                stream_trace_usage(&ctx),
                            );
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes)))
                        }
                        None => {
                            // 流结束，发送最终事件（generate_final_events 内部会 finish()
                            // 累积器，据此判定是否有半截 / 非法工具调用 JSON）。
                            let final_events = ctx.generate_final_events();
                            if let Some(message) = ctx.tool_json_error_message() {
                                // 工具调用 JSON 半截 / 非法：实时流已回 200，无法改状态码，
                                // 只能记 error 并让 generate_final_events 补发的 `error` 事件透传给客户端。
                                record_stream_usage(&hook, &ctx, credential_id, "error");
                                tracer.finalize(
                                    "error",
                                    Some(outcome::BAD_REQUEST),
                                    Some(&message),
                                    None,
                                    stream_trace_usage(&ctx),
                                );
                            } else {
                                record_stream_usage(&hook, &ctx, credential_id, "success");
                                tracer.finalize(
                                    "success",
                                    None,
                                    None,
                                    None,
                                    stream_trace_usage(&ctx),
                                );
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

/// 从 StreamContext 提取最终用量并写入 hook
fn record_stream_usage(
    hook: &UsageRecordHook,
    ctx: &StreamContext,
    credential_id: u64,
    status: &str,
) {
    // 互斥分摊后的 (input, cache_creation, cache_read)，与 trace 上报口径一致。
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    hook.record(
        credential_id,
        input,
        ctx.output_tokens,
        cache_creation,
        cache_read,
        ctx.credits,
        status,
    );
}

/// 从 StreamContext 提取用量，转成 trace 行用量（与 record_stream_usage 同源）
fn stream_trace_usage(ctx: &StreamContext) -> TraceUsage {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    TraceUsage {
        input_tokens: input.max(0) as u64,
        output_tokens: ctx.output_tokens.max(0) as u64,
        cache_creation_tokens: cache_creation.max(0) as u64,
        cache_read_tokens: cache_read.max(0) as u64,
        credits: if ctx.credits.is_finite() && ctx.credits > 0.0 {
            ctx.credits
        } else {
            0.0
        },
    }
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    anthropic_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    _known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    _cache_usage: super::cache_metering::CacheUsage,
    cache_multipliers: super::cache_engine::UsageMultipliers,
    usage_mode: super::cache_engine::UsageMode,
    tracer: std::sync::Arc<RequestTracer>,
    upstream_beta: Option<String>,
    group: Option<String>,
) -> Response {
    // 获取膨胀倍率（用于上游非流式和正常路径）
    let (input_mul, output_mul, cache_mul, cache_creation_mul) =
        cache_multipliers.resolve();

    // 调用 Kiro API（支持多凭据故障转移 + 上游直通）
    let call_result = match provider
        .call_api_dual(
            request_body,
            Some(anthropic_body),
            upstream_beta.as_deref(),
            Some(tracer.as_ref()),
            group.as_deref(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let cache_usage = hook.resolve_pending_cache(call_result.credential_id);
    hook.set_selected_billing_usage(cache_usage);
    let simulated_total_input = cache_usage.prompt_total_est.max(1);

    // 上游凭据直通：解析 JSON，用模拟缓存替换真实 Anthropic 缓存，应用膨胀倍率
    if call_result.is_upstream {
        let credential_id = call_result.credential_id;
        let (resp, u_input, u_output, u_cache_creation, u_cache_read, raw_usage) =
            super::upstream::handle_upstream_non_stream_response(
                call_result.response,
                input_mul,
                output_mul,
                cache_mul,
                cache_creation_mul,
                simulated_total_input,
                cache_usage,
                usage_mode,
                input_tokens,
            )
            .await;
        if let Some(billing_usage) =
            hook.set_upstream_billing_usage(
                raw_usage,
                ClientTokens {
                    input: u_input,
                    output: u_output,
                    cache_creation: u_cache_creation,
                    cache_read: u_cache_read,
                },
                provider.get_inflation_multipliers(),
            )
        {
            tracer.set_billing_usage(billing_usage);
        }
        hook.record(
            credential_id,
            u_input,
            u_output,
            u_cache_creation,
            u_cache_read,
            0.0,
            "success",
        );
        tracer.finalize(
            "success",
            None,
            None,
            None,
            TraceUsage {
                input_tokens: u_input.max(0) as u64,
                output_tokens: u_output.max(0) as u64,
                cache_creation_tokens: u_cache_creation.max(0) as u64,
                cache_read_tokens: u_cache_read.max(0) as u64,
                credits: 0.0,
            },
        );
        return resp;
    }

    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut native_thinking = String::new();
    let mut native_thinking_signature: Option<String> = None;
    let mut native_redacted_thinking: Vec<String> = Vec::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // meteringEvent 上报的 credit 计费量（上游真实下发）；
    // input/cache_* 的互斥分摊在拿到 total 真值后由 cache_usage 完成。
    let mut credits: f64 = 0.0;
    // 最近一次 meteringEvent 的完整 payload，用于在响应体 usage 中透传
    // credit_usage / credit_unit / credit_unit_plural 字段，与 /v1/messages
    // 流式（message_delta）行为一致；如果上游多次下发则取最后一次。
    let mut metering: Option<crate::kiro::model::events::MeteringEvent> = None;

    // 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，stop 时整体解析。
    // 半截 / 非法 JSON 显式暴露为错误（返回 502），不再静默回退 {} 或丢弃。
    let mut tool_accumulator = super::stream::ToolJsonAccumulator::new();
    let mut tool_json_error: Option<super::stream::ToolJsonAccumulatorError> = None;

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ReasoningContent(reasoning) => {
                            if let Some(text) = reasoning.text
                                && !text.is_empty()
                            {
                                native_thinking.push_str(&text);
                            }
                            if let Some(signature) = reasoning.signature
                                && !signature.is_empty()
                            {
                                native_thinking_signature = Some(signature);
                            }
                            if let Some(redacted) = reasoning.redacted_content
                                && !redacted.is_empty()
                            {
                                native_redacted_thinking.push(redacted);
                            }
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;
                            match tool_accumulator.push(&tool_use, &tool_name_map) {
                                Ok(Some(completed)) => {
                                    tool_uses.push(completed.to_anthropic_block());
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!("{}", e);
                                    tool_json_error = Some(e);
                                }
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(event_metering) => {
                            // 上游只下发 credit；token / cache 字段不存在
                            credits += event_metering.usage;
                            tracing::debug!(
                                usage = event_metering.usage,
                                unit = %event_metering.unit,
                                unit_plural = %event_metering.unit_plural,
                                "metering credits +{:.6}", event_metering.usage
                            );
                            metering = Some(event_metering);
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 收尾：若仍有未收到 stop=true 的工具调用缓冲（上游在参数写到一半时截断），
    // finish() 返回 IncompleteJson。已有错误则保持不变。
    if tool_json_error.is_none()
        && let Err(e) = tool_accumulator.finish()
    {
        tracing::error!("{}", e);
        tool_json_error = Some(e);
    }

    // 工具调用 JSON 半截 / 非法：非流式路径尚未发送任何字节，直接回 502，
    // 明确暴露上游问题，而不是把无法解析的参数当成完整调用返回。
    if let Some(err) = tool_json_error {
        let message = err.message();
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(&message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_tool_json_error", message)),
        )
            .into_response();
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（非流式：整段文本已就绪，一次性剥离）。
    let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);

    // 构建响应内容
    let mut content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    content.extend(tool_uses);

    // 估算输出 tokens（上游不下发 token，全部走估算）
    let output_tokens = token::estimate_output_tokens(&content);

    // 输入 tokens：contextUsage 真实值优先，否则用客户端估算
    let total_input_tokens = resolve_usage_input_tokens(input_tokens, context_input_tokens);
    // 互斥分摊：input + cache_creation + cache_read == total
    let (final_input_tokens, cache_creation_tokens, cache_read_tokens) =
        cache_usage.split_against_total(total_input_tokens);

    // 应用膨胀倍率（返回给客户端的值）
    let inflated_input = (final_input_tokens as f64 * input_mul).round() as i32;
    let inflated_output = (output_tokens as f64 * output_mul).round() as i32;
    // creation 用独立倍率：go 引擎不缩放它（对齐 Go 只缩放 input / cache_read）。
    let inflated_cache_creation =
        (cache_creation_tokens as f64 * cache_creation_mul).round() as i32;
    let inflated_cache_read = (cache_read_tokens as f64 * cache_mul).round() as i32;

    // 构建 Anthropic 响应
    let mut usage_json = json!({
        "input_tokens": inflated_input,
        "output_tokens": inflated_output,
        "cache_creation_input_tokens": inflated_cache_creation,
        "cache_read_input_tokens": inflated_cache_read
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro
    // 后端口径一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = &metering {
        usage_json["credit_usage"] = json!(m.usage);
        usage_json["credit_unit"] = json!(m.unit);
        usage_json["credit_unit_plural"] = json!(m.unit_plural);
    }
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_json
    });

    hook.record(
        credential_id,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        credits,
        "success",
    );
    tracer.finalize(
        "success",
        None,
        None,
        None,
        TraceUsage {
            input_tokens: final_input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
        },
    );
    (StatusCode::OK, Json(response_body)).into_response()
}

fn build_non_stream_content(
    thinking_enabled: bool,
    text_content: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    let has_native_thinking = !native_thinking.is_empty();

    if thinking_enabled {
        if has_native_thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": native_thinking.clone(),
                "signature": native_thinking_signature
                    .unwrap_or_else(|| super::stream::THINKING_SIGNATURE_PLACEHOLDER.to_string()),
            }));
        } else {
            // 从完整文本中提取 thinking 块，兼容旧的 <thinking> 文本路径。
            let (thinking, remaining_text) =
                super::stream::extract_thinking_from_complete_text(&text_content);

            if let Some(thinking_text) = thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking_text,
                    "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
                }));
            }

            if !remaining_text.is_empty() {
                content.push(json!({
                    "type": "text",
                    "text": remaining_text
                }));
            }
        }

        for redacted in native_redacted_thinking {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted
            }));
        }

        if has_native_thinking && !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    } else if has_native_thinking {
        content.push(json!({
            "type": "text",
            "text": native_thinking
        }));
    }
    content
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: axum::http::HeaderMap,
    raw_body: Option<Extension<RawBody>>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 上游凭据直通要转发的请求体：优先客户端原文，绝不用 `to_string(&payload)`。
    // 后者会把 serde 默认值实体化进去（`thinking.budget_tokens`、`max_tokens`、
    // 一串显式 `null`），上游会 400。见 middleware::RawBody。
    let anthropic_body_raw = raw_body
        .and_then(|Extension(RawBody(bytes))| String::from_utf8(bytes.to_vec()).ok())
        .map(|raw| ensure_max_tokens(raw, payload.max_tokens))
        .unwrap_or_else(|| {
            // 兜底：路由未挂 capture_raw_body，或 body 非 UTF-8（JSON 必须是 UTF-8，
            // 故后者意味着请求本就不合法）。回退往返序列化 —— 会带上默认值注入，
            // 但比发空 body 好。
            tracing::warn!("无客户端原始 body，回退往返序列化（上游直通可能因默认值注入被拒）");
            serde_json::to_string(&payload).unwrap_or_default()
        });
    // 提取客户端 anthropic-beta 头，透传给上游
    let upstream_beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置（仅 Kiro 路径使用）
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;
        // 与 /v1/messages 同处理：WebSearch 也要走 begin()，见该处注释。
        let (ws_cache_usage, _ws_multipliers, ws_pending, _ws_mode) =
            state
                .cache_engines
                .begin(
                    &payload,
                    key_ctx.key_id,
                    key_ctx.cache_engine,
                    provider.get_inflation_multipliers(),
                );
        hook.set_billing_request(key_ctx.cache_engine, ws_cache_usage);
        hook.set_pending_cache(ws_pending);

        let resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
            ws_cache_usage,
        )
        .await;
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        let (rec_input, rec_cc, rec_cr) = ws_cache_usage.split_against_total(input_tokens);
        hook.record(0, rec_input, 0, rec_cc, rec_cr, 0.0, status);
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools: web_search coexists with other tools, use internal agentic loop
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
        )
        .await;
    }
    // 转换请求
    let conversion_result = match convert_request_with_mode(&payload, state.tool_compatibility_mode)
    {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 计算总 input tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // 缓存模拟：按客户端 Key 选中的引擎查缓存覆盖情况（estimate 口径）。
    // 引擎 B 两阶段，写入意图存进 hook，待 `record("success")` 时提交。
    let (cache_usage, cache_multipliers, pending_cache, usage_mode) =
        state
            .cache_engines
            .begin(
                    &payload,
                    key_ctx.key_id,
                    key_ctx.cache_engine,
                    provider.get_inflation_multipliers(),
                );
    hook.set_billing_request(key_ctx.cache_engine, cache_usage);
    hook.set_pending_cache(pending_cache);

    // 序列化 Anthropic 格式请求体：使用 override_thinking 之前捕获的原始版本（上游直通时使用）
    // anthropic_body_raw 已在 override_thinking 前捕获，此处直接使用

    if payload.stream {
        // 流式响应（缓冲模式）
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
            },
        ));
        handle_stream_request_buffered(
            provider,
            &request_body,
            &anthropic_body_raw,
            &payload.model,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            hook,
            total_input_tokens,
            cache_usage,
            cache_multipliers,
            usage_mode,
            tracer,
            upstream_beta,
            key_ctx.group.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &anthropic_body_raw,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            hook,
            cache_usage,
            cache_multipliers,
            usage_mode,
            tracer,
            upstream_beta,
            key_ctx.group.clone(),
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    anthropic_body: &str,
    model: &str,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    _cache_usage: super::cache_metering::CacheUsage,
    cache_multipliers: super::cache_engine::UsageMultipliers,
    usage_mode: super::cache_engine::UsageMode,
    tracer: std::sync::Arc<RequestTracer>,
    upstream_beta: Option<String>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移 + 上游直通）
    let call_result = match provider
        .call_api_stream_dual(
            request_body,
            Some(anthropic_body),
            upstream_beta.as_deref(),
            Some(tracer.as_ref()),
            group.as_deref(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let cache_usage = hook.resolve_pending_cache(call_result.credential_id);
    hook.set_selected_billing_usage(cache_usage);
    let simulated_total_input = cache_usage.prompt_total_est.max(1);

    // 上游凭据直通：应用膨胀倍率 + 模拟缓存，流结束后回调 hook.record
    if call_result.is_upstream {
        let credential_id = call_result.credential_id;
        let (input_mul, output_mul, cache_mul, cache_creation_mul) =
            cache_multipliers.resolve();
        let (resp, usage_rx) = super::upstream::handle_upstream_stream_response_with_inflation(
            call_result.response,
            input_mul,
            output_mul,
            cache_mul,
            cache_creation_mul,
            simulated_total_input,
            cache_usage,
            usage_mode,
            // 引擎 D 的 input 口径：客户端请求的本地估算（token::count_all_tokens），
            // 与上游报的 usage 无关 —— 上游可能是另一个反代，其 usage 已被加工过。
            fallback_input_tokens,
        );
        tokio::spawn(async move {
            let usage = usage_rx.await.unwrap_or_default();
            if let Some(billing_usage) = hook
                .set_upstream_billing_usage(
                    usage.raw_usage,
                    ClientTokens {
                        input: usage.input_tokens,
                        output: usage.output_tokens,
                        cache_creation: usage.cache_creation_tokens,
                        cache_read: usage.cache_read_tokens,
                    },
                    provider.get_inflation_multipliers(),
                )
            {
                tracer.set_billing_usage(billing_usage);
            }
            hook.record(
                credential_id,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_tokens,
                usage.cache_read_tokens,
                0.0,
                "success",
            );
            tracer.finalize(
                "success",
                None,
                None,
                None,
                TraceUsage {
                    input_tokens: usage.input_tokens.max(0) as u64,
                    output_tokens: usage.output_tokens.max(0) as u64,
                    cache_creation_tokens: usage.cache_creation_tokens.max(0) as u64,
                    cache_read_tokens: usage.cache_read_tokens.max(0) as u64,
                    credits: 0.0,
                },
            );
        });
        return resp;
    }

    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(
        model,
        fallback_input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    ctx.set_cache_usage(cache_usage);
    let (input_mul, output_mul, cache_mul, cache_creation_mul) =
        cache_multipliers.resolve();
    ctx.set_inflation_multipliers_split(input_mul, output_mul, cache_mul, cache_creation_mul);

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(response, ctx, hook, credential_id, tracer);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            hook,
            credential_id,
            tracer,
            0u64,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, hook, credential_id, tracer, mut sent_bytes)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                tracer.mark_first_token();
                                sent_bytes += chunk.len() as u64;
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                hook.record(credential_id, i, o, cc, cr, credits, "error");
                                // 缓冲模式 chunk 读取失败：上游中途断流
                                tracer.finalize(
                                    "interrupted",
                                    Some(outcome::STREAM_INTERRUPTED),
                                    Some(&e.to_string()),
                                    Some(sent_bytes),
                                    TraceUsage {
                                        input_tokens: i.max(0) as u64,
                                        output_tokens: o.max(0) as u64,
                                        cache_creation_tokens: cc.max(0) as u64,
                                        cache_read_tokens: cr.max(0) as u64,
                                        credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                                    },
                                );
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）。
                                // finish_and_get_all_events 内部会 finish() 累积器；若有半截 /
                                // 非法工具调用 JSON，error 事件已随缓冲发出，这里据此记 error。
                                let all_events = ctx.finish_and_get_all_events();
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                let trace_usage = TraceUsage {
                                    input_tokens: i.max(0) as u64,
                                    output_tokens: o.max(0) as u64,
                                    cache_creation_tokens: cc.max(0) as u64,
                                    cache_read_tokens: cr.max(0) as u64,
                                    credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                                };
                                if let Some(message) = ctx.tool_json_error_message() {
                                    hook.record(credential_id, i, o, cc, cr, credits, "error");
                                    tracer.finalize(
                                        "error",
                                        Some(outcome::BAD_REQUEST),
                                        Some(&message),
                                        None,
                                        trace_usage,
                                    );
                                } else {
                                    hook.record(credential_id, i, o, cc, cr, credits, "success");
                                    tracer.finalize("success", None, None, None, trace_usage);
                                }
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_billing_usage_applies_each_engine_multiplier_independently() {
        let upstream = TokenUsageBreakdown {
            input_tokens: 100,
            output_tokens: 10,
            cache_creation_tokens: 20,
            cache_read_tokens: 30,
        };
        let simulated = super::super::cache_metering::CacheUsage {
            cache_read: 30,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };

        // 口径已由 UsageMode 定完（此处等价于 Simulated 对 total=150 的分摊结果），
        // scaled_billing_usage 只负责乘倍率。
        let client = super::ClientTokens {
            input: 75,
            output: 10,
            cache_creation: 30,
            cache_read: 45,
        };
        let usage = super::scaled_billing_usage(client, (2.0, 3.0, 4.0, 5.0));

        // 逐项独立乘：input×2、output×3、read×4、creation×5。
        assert_eq!(usage.input_tokens, 150);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.cache_creation_tokens, 150);
        assert_eq!(usage.cache_read_tokens, 180);
        assert_eq!(upstream.input_tokens, 100, "上游原始 usage 不得被修改");
        let _ = simulated;
    }

    #[test]
    fn billing_snapshot_only_keeps_the_selected_engine() {
        let upstream = TokenUsageBreakdown {
            input_tokens: 100,
            output_tokens: 10,
            cache_creation_tokens: 20,
            cache_read_tokens: 30,
        };
        let client = super::ClientTokens {
            input: 75,
            output: 10,
            cache_creation: 30,
            cache_read: 45,
        };
        let multipliers = (2.0, 3.0, 4.0, 5.0);

        // v1 里"选中哪个引擎"是靠 rust 槽 / go 槽哪个非空隐式表达的；v2 改为显式
        // `engine` 标签 + 单个 client 值。该不变量本身没变：一次请求只记一个引擎。
        for kind in [
            super::super::cache_engine::CacheEngineKind::Rust,
            super::super::cache_engine::CacheEngineKind::Go,
        ] {
            let snap = super::selected_billing_snapshot(kind, client, upstream, multipliers);
            assert_eq!(
                snap.engine,
                kind,
                "快照必须标记本次请求实际使用的引擎"
            );
            assert_eq!(
                snap.upstream, upstream,
                "上游真值必须原样保留（它是该引擎的真实成本口径）"
            );
        }
    }

    /// 引擎 C / D 必须**各自**带上游真值进计费快照。
    ///
    /// 这条断言的前提被本次 schema 改造反转了：v1 三槽（upstream + rust + go）里
    /// C / D 无处安放，只能两槽皆空、在对比表里整段缺失。v2 改为逐引擎配对后，
    /// 每个引擎自带一份上游口径，四套引擎一视同仁。
    #[test]
    fn stateless_engines_get_their_own_paired_slot() {
        let upstream = TokenUsageBreakdown {
            input_tokens: 100,
            output_tokens: 10,
            cache_creation_tokens: 20,
            cache_read_tokens: 30,
        };
        for kind in [
            super::super::cache_engine::CacheEngineKind::Real,
            super::super::cache_engine::CacheEngineKind::NoCache,
        ] {
            let snap = super::selected_billing_snapshot(
                kind,
                super::ClientTokens {
                    input: 100,
                    output: 10,
                    cache_creation: 0,
                    cache_read: 0,
                },
                upstream,
                (1.0, 1.0, 1.0, 1.0),
            );
            assert_eq!(snap.engine, kind, "{kind:?} 必须标记自己的引擎名");
            assert_eq!(
                snap.upstream, upstream,
                "{kind:?} 必须带自己那份上游真值 —— 否则对比表里没有分母"
            );
            assert_eq!(snap.client.input_tokens, 100, "{kind:?} 客户端计费必须记入");
        }
    }

    /// 引擎 D 的计费快照必须等于「客户端所见 × 倍率」，且与上游 usage 无关。
    ///
    /// 上游报 input=100 / output=10 / cc=20 / cr=30，但引擎 D 下发的是本地估算
    /// （input=777 / output=42）。计费快照若从上游值重新推导，就会记成 100/10 ——
    /// 与客户端实际被计费的数字不符。故此处钉住：快照只认传入的 ClientTokens。
    #[test]
    fn nocache_engine_bills_local_tokens_not_upstream() {
        let upstream = TokenUsageBreakdown {
            input_tokens: 100,
            output_tokens: 10,
            cache_creation_tokens: 20,
            cache_read_tokens: 30,
        };
        // 客户端实际收到的口径：本地估算，cache 恒为 0。
        let client_tokens = ClientTokens {
            input: 777,
            output: 42,
            cache_creation: 0,
            cache_read: 0,
        };
        let snap = super::selected_billing_snapshot(
            super::super::cache_engine::CacheEngineKind::NoCache,
            client_tokens,
            upstream,
            (2.0, 3.0, 1.0, 1.0),
        );
        assert_eq!(
            snap.engine,
            super::super::cache_engine::CacheEngineKind::NoCache
        );
        assert_eq!(
            snap.upstream.input_tokens, 100,
            "上游真值列必须原样保留（你选的口径）"
        );

        // 直接验证倍率作用在本地值上，而非上游值。
        let billed = super::scaled_billing_usage(client_tokens, (2.0, 3.0, 1.0, 1.0));
        assert_eq!(billed.input_tokens, 1554, "777×2，不是 100×2");
        assert_eq!(billed.output_tokens, 126, "42×3，不是 10×3");
        assert_eq!(billed.cache_creation_tokens, 0, "D 的 cache 恒为 0");
        assert_eq!(billed.cache_read_tokens, 0);
    }

    #[test]
    fn rust_billing_keeps_the_usage_computed_at_begin() {
        let state = AppState::new(
            false,
            crate::model::config::ToolCompatibilityMode::default(),
        );
        let hook = UsageRecordHook::from_state(&state, 7, "test-model".to_string());
        let expected = super::super::cache_metering::CacheUsage {
            cache_read: 30,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };

        hook.set_billing_request(super::super::cache_engine::CacheEngineKind::Rust, expected);
        // Rust begin 返回 PendingCache::None，但该值仍会被包进外层 Option。
        hook.set_pending_cache(super::super::cache_engine::PendingCache::None);

        let actual = hook.resolve_pending_cache(42);
        assert_eq!(actual.cache_read, expected.cache_read);
        assert_eq!(actual.cache_covered_est, expected.cache_covered_est);
        assert_eq!(actual.prompt_total_est, expected.prompt_total_est);
    }

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。识别逻辑集中在 endpoint 层。
        for needle in [
            // 精确 reason（provider 错误串里嵌着上游 body）
            "非流式 API 请求失败: 500 {\"reason\":\"TOOL_USE_RESULT_MISMATCH\"}",
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(anyhow::anyhow!(needle.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "错误串 `{needle}` 应映射为 400"
            );
        }
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn upstream_rate_limit_maps_to_429_with_retry_after() {
        let err = crate::kiro::error::UpstreamRateLimitError::new(Some("1800".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1800");
    }

    #[test]
    fn upstream_rate_limit_drops_invalid_retry_after() {
        let err =
            crate::kiro::error::UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn generic_upstream_error_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let resp = map_provider_error(anyhow::anyhow!(secret));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("Upstream API request failed"));
    }

    #[test]
    fn non_stream_native_thinking_precedes_redacted_and_text() {
        let content = build_non_stream_content(
            true,
            "final answer".to_string(),
            "native thinking".to_string(),
            Some("real-signature".to_string()),
            vec!["encrypted-thinking".to_string()],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "native thinking");
        assert_eq!(content[0]["signature"], "real-signature");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "final answer");
    }

    #[test]
    fn non_stream_legacy_thinking_extraction_still_works_without_native_reasoning() {
        let content = build_non_stream_content(
            true,
            "<thinking>legacy thinking</thinking>\n\nfinal answer".to_string(),
            String::new(),
            None,
            Vec::new(),
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "legacy thinking");
        assert_eq!(
            content[0]["signature"],
            crate::anthropic::stream::THINKING_SIGNATURE_PLACEHOLDER
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_native_thinking_downgrades_to_text_when_thinking_disabled() {
        let content = build_non_stream_content(
            false,
            String::new(),
            "native thinking fallback".to_string(),
            Some("ignored-signature".to_string()),
            vec!["ignored-redacted".to_string()],
        );

        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "native thinking fallback");
    }

    #[test]
    fn dynamic_models_do_not_synthesize_claude_thinking_alias() {
        let models = aggregate_available_models(vec![UpstreamModel {
            model_id: "claude-opus-5".to_string(),
            model_name: Some("Claude Opus 5".to_string()),
            description: None,
            token_limits: None,
        }]);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-5"));
        assert!(!ids.contains(&"claude-opus-5-thinking"));
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn dynamic_models_merge_metadata_and_do_not_use_input_limit_as_output_limit() {
        let models = aggregate_available_models(vec![
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: None,
                description: Some("first".to_string()),
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(200_000),
                    max_output_tokens: None,
                }),
            },
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: Some("GLM 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(32_000),
                }),
            },
        ]);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "GLM 5");
        assert_eq!(models[0].owned_by, "kiro");
        assert_eq!(models[0].max_tokens, 32_000);
    }

    #[test]
    fn custom_model_metadata_overrides_dynamic_collision() {
        let custom = crate::model::config::CustomModel {
            id: "gpt-next".to_string(),
            backend_id: "gpt-next".to_string(),
            display_name: Some("Configured GPT".to_string()),
            context_window: Some(500_000),
            max_tokens: Some(12_345),
            supports_reasoning: Some(true),
            owned_by: Some("configured-owner".to_string()),
        };
        let models = aggregate_available_models_with_custom(
            vec![UpstreamModel {
                model_id: "gpt-next".to_string(),
                model_name: Some("Upstream GPT".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(300_000),
                    max_output_tokens: Some(64_000),
                }),
            }],
            &[custom],
        );

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "Configured GPT");
        assert_eq!(models[0].owned_by, "configured-owner");
        assert_eq!(models[0].max_tokens, 12_345);
    }

    #[test]
    fn max_tokens_must_be_positive() {
        assert!(validate_max_tokens(1).is_ok());
        assert!(validate_max_tokens(0).is_err());
        assert!(validate_max_tokens(-1).is_err());
    }

    /// 原文已带 `max_tokens` 时必须逐字节原样返回 —— 直通路径的保真度全靠这条。
    #[test]
    fn ensure_max_tokens_leaves_body_untouched_when_present() {
        // 刻意用非字母序的键序 + 紧凑无空格，验证没有被重新序列化过。
        let raw = r#"{"model":"m","max_tokens":100,"messages":[],"thinking":{"type":"disabled"}}"#;
        assert_eq!(ensure_max_tokens(raw.to_string(), 32_000), raw);
    }

    /// 缺 `max_tokens` 时补上该字段（Anthropic 必填），且**只补这一个**。
    #[test]
    fn ensure_max_tokens_injects_only_that_field_when_missing() {
        let raw = r#"{"model":"m","messages":[],"thinking":{"type":"disabled"}}"#;
        let got = ensure_max_tokens(raw.to_string(), 32_000);
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();

        assert_eq!(v["max_tokens"], 32_000);
        // thinking 必须还是客户端发的样子：不能被补出 budget_tokens。
        assert_eq!(v["thinking"], serde_json::json!({"type": "disabled"}));
        // 不该凭空多出往返序列化才会有的那些键。
        for key in ["system", "tools", "tool_choice", "output_config", "metadata", "stream"] {
            assert!(
                v.get(key).is_none(),
                "不应注入 `{key}`，实际 body: {got}"
            );
        }
        // 原有键一个不少。
        assert_eq!(v.as_object().unwrap().len(), 4, "应为 model/messages/thinking/max_tokens");
    }

    /// 非 JSON 对象（数组 / 裸值 / 语法错误）原样转发，不在这里加工。
    #[test]
    fn ensure_max_tokens_passes_through_non_object_bodies() {
        for raw in ["[1,2,3]", "\"str\"", "not json at all", ""] {
            assert_eq!(
                ensure_max_tokens(raw.to_string(), 32_000),
                raw,
                "非对象 body 应原样返回"
            );
        }
    }
}
