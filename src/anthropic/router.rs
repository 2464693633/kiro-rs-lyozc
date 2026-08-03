//! Anthropic API 路由配置

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::SharedTraceStore;
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::kiro::provider::KiroProvider;
use crate::model::config::ToolCompatibilityMode;

use super::{
    cache_metering::SharedCacheMeter,
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, capture_raw_body, cors_layer},
    openai::post_chat_completions,
    responses::post_responses,
};

/// 请求体最大大小限制 (50MB)
///
/// `pub(crate)`：`middleware::capture_raw_body` 缓冲原始 body 时要用同一个上限，
/// 否则两处不一致会出现「DefaultBodyLimit 放过、缓冲拒收」的割裂。
pub(crate) const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建带有 KiroProvider 的 Anthropic API 路由
///
/// 给嵌入到其他 Rust 项目的下游使用者预留的扩展点。
#[allow(dead_code)]
pub fn create_router_with_provider(
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Router {
    create_router(
        kiro_provider,
        extract_thinking,
        tool_compatibility_mode,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// 创建 Anthropic API 路由（供 main.rs 使用）
#[allow(clippy::too_many_arguments)]
pub fn create_router(
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    go_cache_tracker: Option<std::sync::Arc<super::cache_metering_go::GoCacheTracker>>,
    stateless_multipliers: Option<std::sync::Arc<super::cache_engine::StatelessMultipliers>>,
    trace_store: Option<SharedTraceStore>,
) -> Router {
    create_router_with_shared_provider(
        kiro_provider.map(Arc::new),
        extract_thinking,
        tool_compatibility_mode,
        client_keys,
        usage_recorder,
        usage_aggregator,
        cache_meter,
        go_cache_tracker,
        stateless_multipliers,
        trace_store,
    )
}

/// 创建共享 KiroProvider 的路由，供主程序同时挂载 API 与 Admin 控制面。
#[allow(clippy::too_many_arguments)]
pub fn create_router_with_shared_provider(
    kiro_provider: Option<Arc<KiroProvider>>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    go_cache_tracker: Option<std::sync::Arc<super::cache_metering_go::GoCacheTracker>>,
    stateless_multipliers: Option<std::sync::Arc<super::cache_engine::StatelessMultipliers>>,
    trace_store: Option<SharedTraceStore>,
) -> Router {
    let mut state = AppState::new(extract_thinking, tool_compatibility_mode);
    if let Some(provider) = kiro_provider {
        state = state.with_shared_kiro_provider(provider);
    }
    state = state.with_usage(client_keys, usage_recorder, usage_aggregator);
    state = state.with_cache_meter(cache_meter);
    state = state.with_go_cache_tracker(go_cache_tracker);
    // None 时沿用 CacheEngines::default() 里那份（倍率全 1.0，不缩放）。
    // main.rs 必须传 Some(...)，否则 Admin 改 C/D 倍率不会生效 —— 见
    // `with_stateless_multipliers` 的文档注释。
    if let Some(multipliers) = stateless_multipliers {
        state = state.with_stateless_multipliers(multipliers);
    }
    state = state.with_trace_store(trace_store);

    // 需要认证的 /v1 路由
    //
    // `/messages` 单独挂 `capture_raw_body`：只有这条路径会命中上游凭据直通，
    // 需要客户端原始字节。挂在 MethodRouter 上（而非整个 Router）使其他端点不
    // 白白多缓冲一份 body。auth 是 Router 级 layer，故在它外层 —— 未通过鉴权的
    // 请求不会被缓冲。
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route(
            "/messages",
            post(post_messages).layer(middleware::from_fn(capture_raw_body)),
        )
        .route("/messages/count_tokens", post(count_tokens))
        .route("/chat/completions", post(post_chat_completions))
        .route("/responses", post(post_responses))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route(
            "/messages",
            post(post_messages_cc).layer(middleware::from_fn(capture_raw_body)),
        )
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}
