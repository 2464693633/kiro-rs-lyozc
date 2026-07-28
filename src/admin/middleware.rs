//! Admin API 中间件

use std::sync::Arc;

use parking_lot::RwLock;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use super::client_keys::SharedClientKeyManager;
use super::groups::SharedGroupManager;
use super::service::AdminService;
use super::types::AdminErrorResponse;
use super::usage_stats::SharedAggregator;
use super::trace_db::SharedTraceStore;
use crate::common::auth;

/// Admin API 共享状态
#[derive(Clone)]
pub struct AdminState {
    /// 登录API密钥（管理面板登录用，运行时可修改）
    pub admin_api_key: Arc<RwLock<String>>,
    /// Admin 服务
    pub service: Arc<AdminService>,
    /// 客户端 Key 管理器（与 anthropic 路由共享）
    pub client_keys: SharedClientKeyManager,
    /// 用量聚合器（与 anthropic 路由共享）
    pub usage_aggregator: SharedAggregator,
    /// 请求链路追踪存储（与 anthropic 路由共享）
    pub trace_store: SharedTraceStore,
    /// 账号分组注册表（持久化到 groups.json）
    pub groups: SharedGroupManager,
    /// 引擎 A 句柄（读计数器 / 热改参数）
    pub cache_meter: Option<crate::anthropic::cache_metering::SharedCacheMeter>,
    /// 引擎 B 句柄（读计数器 / 热改参数）
    pub go_cache_tracker: Option<Arc<crate::anthropic::cache_metering_go::GoCacheTracker>>,
}

impl AdminState {
    pub fn new(
        admin_api_key: impl Into<String>,
        service: AdminService,
        client_keys: SharedClientKeyManager,
        usage_aggregator: SharedAggregator,
        trace_store: SharedTraceStore,
        groups: SharedGroupManager,
    ) -> Self {
        Self {
            admin_api_key: Arc::new(RwLock::new(admin_api_key.into())),
            service: Arc::new(service),
            client_keys,
            usage_aggregator,
            trace_store,
            groups,
            cache_meter: None,
            go_cache_tracker: None,
        }
    }

    /// 注入两套缓存模拟引擎的句柄。
    ///
    /// 用 builder 而非扩 `new` 的参数表：`new` 已有 6 个位置参数，再加两个同类型
    /// Option 极易在调用点写错顺序。
    pub fn with_cache_engines(
        mut self,
        cache_meter: Option<crate::anthropic::cache_metering::SharedCacheMeter>,
        go_cache_tracker: Option<Arc<crate::anthropic::cache_metering_go::GoCacheTracker>>,
    ) -> Self {
        self.cache_meter = cache_meter;
        self.go_cache_tracker = go_cache_tracker;
        self
    }
}

/// Admin API 认证中间件 — 校验登录API密钥（adminApiKey）
pub async fn admin_auth_middleware(
    State(state): State<AdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);

    let current_key = state.admin_api_key.read().clone();
    match api_key {
        Some(key) if auth::constant_time_eq(&key, &current_key) => next.run(request).await,
        _ => {
            let error = AdminErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}
