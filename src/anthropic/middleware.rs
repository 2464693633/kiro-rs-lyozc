//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{SharedTraceStore, TraceKeySource};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::cache_metering::SharedCacheMeter;
use super::types::ErrorResponse;

/// 命中的鉴权上下文（注入到请求扩展，供 handler 记录用量）
#[derive(Clone, Debug)]
pub struct KeyContext {
    /// 命中的客户端 Key id
    pub key_id: u64,
    /// 该 Key 绑定的账号分组；None 表示未绑定，可使用全部账号
    pub group: Option<String>,
    /// 命中的入口 Key 类型。
    pub key_source: TraceKeySource,
    /// 该 Key 选择的缓存模拟引擎。老 Key 缺该字段时为 `Rust`（保持原行为）。
    pub cache_engine: super::cache_engine::CacheEngineKind,
}

/// 客户端请求体原文（未经「反序列化 → 重新序列化」往返）。
///
/// 上游凭据直通路径必须转发这份字节。用 `serde_json::to_string(&payload)` 会把
/// serde 的默认值实体化进请求体，最典型的是 [`super::types::Thinking`]：
/// `budget_tokens` 是带 `default = 20000` 的裸 `i32`，客户端发
/// `{"type":"disabled"}` 时会被补成 `{"type":"disabled","budget_tokens":20000}`，
/// 而 Anthropic 对 `disabled` 不接受该字段 —— 上游直接 400
/// `thinking.budget_tokens is not supported when thinking.type is disabled`。
///
/// 同类注入还有：`max_tokens` 缺省被补成 32000（客户端本想用上游自己的默认值），
/// `system` / `tools` / `tool_choice` / `output_config` 被写成显式 `null`。
#[derive(Clone, Debug)]
pub struct RawBody(pub bytes::Bytes);

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 工具兼容模式（ClaudeCode 内置工具名/入参双向适配 / Raw 透传）
    pub tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    /// 客户端 Key 管理器（可选，未启用 Admin 时为 None）
    pub client_keys: Option<SharedClientKeyManager>,
    /// 用量日志记录器
    pub usage_recorder: Option<SharedRecorder>,
    /// 用量聚合器
    pub usage_aggregator: Option<SharedAggregator>,
    /// 中转层缓存计量（基于 cache_control 断点的内存缓存）
    pub cache_meter: Option<SharedCacheMeter>,
    /// 双缓存模拟引擎句柄。由客户端 Key 上的 `cacheEngine` 字段选择走哪一套。
    pub cache_engines: super::cache_engine::CacheEngines,
    /// 请求链路追踪存储（SQLite，可选）
    pub trace_store: Option<SharedTraceStore>,
}

impl AppState {
    /// 创建新的应用状态（不含 client_keys 的基础构造，供嵌入 / 测试使用）
    #[allow(dead_code)]
    pub fn new(
        extract_thinking: bool,
        tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    ) -> Self {
        Self {
            kiro_provider: None,
            extract_thinking,
            tool_compatibility_mode,
            client_keys: None,
            usage_recorder: None,
            usage_aggregator: None,
            cache_meter: None,
            cache_engines: super::cache_engine::CacheEngines::default(),
            trace_store: None,
        }
    }

    /// 注入可与 Admin 控制面共享的 KiroProvider。
    pub fn with_shared_kiro_provider(mut self, provider: Arc<KiroProvider>) -> Self {
        self.kiro_provider = Some(provider);
        self
    }

    /// 注入用量记录组件
    pub fn with_usage(
        mut self,
        client_keys: Option<SharedClientKeyManager>,
        recorder: Option<SharedRecorder>,
        aggregator: Option<SharedAggregator>,
    ) -> Self {
        self.client_keys = client_keys;
        self.usage_recorder = recorder;
        self.usage_aggregator = aggregator;
        self
    }

    /// 注入缓存计量器
    pub fn with_cache_meter(mut self, cache: Option<SharedCacheMeter>) -> Self {
        // 引擎 A 的句柄同时进 cache_engines，使接缝能路由到它。
        self.cache_engines.rust = cache.clone();
        self.cache_meter = cache;
        self
    }

    /// 注入引擎 B（go 缓存模拟引擎）。
    pub fn with_go_cache_tracker(
        mut self,
        tracker: Option<std::sync::Arc<super::cache_metering_go::GoCacheTracker>>,
    ) -> Self {
        self.cache_engines.go = tracker;
        self
    }

    /// 注入引擎 C / D 的倍率存储。
    ///
    /// **必须由外部传入同一个 `Arc`，不可依赖 `CacheEngines::default()`**：Admin
    /// 侧热更新是对 `Arc` 内的原子量做 store，只有请求路径与 Admin 路径共享同一份
    /// 分配，改配置才会立刻生效。若两边各自 `Default::default()`，Admin 改完看着
    /// 成功、请求路径却永远读到 1.0 —— 静默失效，且没有任何报错提示。
    ///
    /// A / B 不需要这个方法，因为它们的句柄本身就是外部构造的 `Arc` tracker。
    pub fn with_stateless_multipliers(
        mut self,
        multipliers: std::sync::Arc<super::cache_engine::StatelessMultipliers>,
    ) -> Self {
        self.cache_engines.stateless = multipliers;
        self
    }

    /// 注入链路追踪存储
    pub fn with_trace_store(mut self, store: Option<SharedTraceStore>) -> Self {
        self.trace_store = store;
        self
    }
}

/// API Key 认证中间件
///
/// 所有入口 Key 统一按已存储的完整值精确匹配，不限制前缀。命中后向请求扩展注入
/// [`KeyContext`]，供 handler 记录用量时使用。
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let presented = match auth::extract_api_key(&request) {
        Some(k) => k,
        None => {
            let error = ErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        }
    };

    if let Some(mgr) = &state.client_keys {
        if let Some(id) = mgr.verify_and_touch(&presented) {
            let group = mgr.group_of(id);
            let cache_engine = mgr.cache_engine_of(id);
            request.extensions_mut().insert(KeyContext {
                key_id: id,
                group,
                key_source: TraceKeySource::ClientKey,
                cache_engine,
            });
            return next.run(request).await;
        }
    }

    let error = ErrorResponse::authentication_error();
    (StatusCode::UNAUTHORIZED, Json(error)).into_response()
}

/// 把请求体原文缓存进扩展，供上游凭据直通路径原样转发。见 [`RawBody`]。
///
/// 只挂在 `/v1/messages` 与 `/cc/v1/messages` 上 —— 其余端点不走上游直通，
/// 没有转发原文的需求，不必为它们多留一份 body 副本。
///
/// **必须排在 [`auth_middleware`] 之内层**：未鉴权的请求应在缓冲 body 之前就被
/// 拒掉，否则任意来源都能让服务为一个 50MB 的匿名请求分配内存。
///
/// body 在这里被完整读出，再原样塞回给下游的 `Json` 提取器。故内存里会同时存在
/// 原文字节与反序列化后的结构体 —— 对 messages 端点可接受（本就要缓冲整个 body
/// 才能解析 JSON），但这是有意的取舍而非疏漏。
pub async fn capture_raw_body(request: Request<Body>, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    // 上限与 router 的 DefaultBodyLimit 同源，避免两处各写一个数导致行为分叉。
    let bytes = match axum::body::to_bytes(body, super::router::MAX_BODY_SIZE).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("读取请求体失败: {}", e);
            let error = ErrorResponse::new("invalid_request_error", "failed to read request body");
            return (StatusCode::BAD_REQUEST, Json(error)).into_response();
        }
    };

    let mut request = Request::from_parts(parts, Body::from(bytes.clone()));
    request.extensions_mut().insert(RawBody(bytes));
    next.run(request).await
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}
