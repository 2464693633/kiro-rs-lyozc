//! 上游 Anthropic API 响应处理模块
//!
//! 处理上游凭据直通返回的 Anthropic JSON 格式响应（流式 + 非流式）。
//! 主要职责：透传响应并对 usage 字段应用 token 膨胀倍率。

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use std::convert::Infallible;

use super::types::ErrorResponse;

/// 对 JSON 响应体中的 usage 对象应用膨胀倍率（非流式）
pub fn inflate_usage_in_json(
    json: &mut serde_json::Value,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
) {
    if let Some(usage) = json.get_mut("usage") {
        inflate_usage_obj(usage, input_mul, output_mul, cache_mul);
    }
}

/// 对单个 usage 对象内的字段就地膨胀
fn inflate_usage_obj(
    usage: &mut serde_json::Value,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
) {
    if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
        usage["input_tokens"] = serde_json::json!((v as f64 * input_mul).round() as i64);
    }
    if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
        usage["output_tokens"] = serde_json::json!((v as f64 * output_mul).round() as i64);
    }
    if let Some(v) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()) {
        usage["cache_creation_input_tokens"] = serde_json::json!((v as f64 * cache_mul).round() as i64);
    }
    if let Some(v) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) {
        usage["cache_read_input_tokens"] = serde_json::json!((v as f64 * cache_mul).round() as i64);
    }
}

/// 处理上游非流式响应：读取完整 JSON，应用膨胀倍率，返回给客户端。
///
/// 返回 `(Response, input_tokens, output_tokens, cache_creation, cache_read)` 供调用方记录用量。
pub async fn handle_upstream_non_stream_response(
    response: reqwest::Response,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
) -> (Response, i32, i32, i32, i32) {
    let status = response.status();
    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("读取上游响应体失败: {}", e);
            let resp = (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("api_error", format!("读取上游响应失败: {}", e))),
            ).into_response();
            return (resp, 0, 0, 0, 0);
        }
    };

    let mut json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            let resp = Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap();
            return (resp, 0, 0, 0, 0);
        }
    };

    // 提取真实用量（膨胀前）
    let (input_tokens, output_tokens, cache_creation, cache_read) = extract_usage(&json);

    // 应用膨胀
    inflate_usage_in_json(&mut json, input_mul, output_mul, cache_mul);

    let resp = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_string()))
        .unwrap();

    (resp, input_tokens, output_tokens, cache_creation, cache_read)
}

/// 处理上游流式响应：透明代理 SSE 流（流式不应用膨胀）。
pub fn handle_upstream_stream_response(response: reqwest::Response) -> Response {
    let stream = response.bytes_stream().map(|chunk| -> Result<Bytes, Infallible> {
        match chunk {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                tracing::error!("上游流式响应读取失败: {}", e);
                Ok(Bytes::new())
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 从 Anthropic JSON 响应中提取真实用量
fn extract_usage(json: &serde_json::Value) -> (i32, i32, i32, i32) {
    let usage = match json.get("usage") {
        Some(u) => u,
        None => return (0, 0, 0, 0),
    };
    let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let cache_creation = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    (input, output, cache_creation, cache_read)
}
