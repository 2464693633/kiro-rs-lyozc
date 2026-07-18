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
use futures::{stream, StreamExt};
use std::convert::Infallible;

use super::cache_metering::CacheUsage;
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

/// 处理上游非流式响应：读取完整 JSON，用模拟缓存替换真实 Anthropic 缓存，应用膨胀倍率，返回给客户端。
///
/// 返回 `(Response, input_tokens, output_tokens, cache_creation, cache_read)` 供调用方记录用量。
/// 返回的 token 数为膨胀前的模拟缓存值（与 Kiro 账号路径一致）。
pub async fn handle_upstream_non_stream_response(
    response: reqwest::Response,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
    cache_usage: CacheUsage,
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

    // 提取真实 token 数，计算总 input（用于模拟缓存分摊）
    let (real_input, real_output, real_cc, real_cr) = extract_usage(&json);
    let total_input = real_input + real_cc + real_cr;

    // 用模拟缓存替换上游真实 Anthropic 缓存（与 Kiro 账号路径一致）
    let (sim_input, sim_cc, sim_cr) = cache_usage.split_against_total(total_input);

    // 应用膨胀倍率并写回 usage 字段
    if let Some(usage) = json.get_mut("usage") {
        usage["input_tokens"] = serde_json::json!((sim_input as f64 * input_mul).round() as i64);
        usage["output_tokens"] = serde_json::json!((real_output as f64 * output_mul).round() as i64);
        usage["cache_creation_input_tokens"] = serde_json::json!((sim_cc as f64 * cache_mul).round() as i64);
        usage["cache_read_input_tokens"] = serde_json::json!((sim_cr as f64 * cache_mul).round() as i64);
    }

    let resp = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_string()))
        .unwrap();

    // 返回膨胀前的模拟值供 hook.record 记录
    (resp, sim_input, real_output, sim_cc, sim_cr)
}

/// 对单条 SSE 事件文本应用膨胀倍率（模拟缓存）。
///
/// 仅重写 `message_start`（usage.input_tokens / cache_* 字段）和
/// `message_delta`（usage.output_tokens 字段），其余事件原样透传。
fn inflate_sse_event(
    event_text: &str,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
    cache_usage: CacheUsage,
) -> String {
    let mut event_type: Option<&str> = None;
    let mut data_line: Option<&str> = None;

    for line in event_text.lines() {
        if let Some(t) = line.strip_prefix("event: ") {
            event_type = Some(t.trim());
        } else if let Some(d) = line.strip_prefix("data: ") {
            data_line = Some(d);
        }
    }

    match (event_type, data_line) {
        (Some("message_start"), Some(data)) => {
            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(data) {
                // 计算总 input（含真实缓存字段）用于模拟缓存分摊
                let total_input = {
                    let u = json.pointer("/message/usage");
                    let i = u.and_then(|v| v.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                    let cc = u.and_then(|v| v.get("cache_creation_input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                    let cr = u.and_then(|v| v.get("cache_read_input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                    (i + cc + cr) as i32
                };
                // 用模拟缓存分摊替换真实 Anthropic 缓存
                let (sim_input, sim_cc, sim_cr) = cache_usage.split_against_total(total_input);
                if let Some(usage) = json.pointer_mut("/message/usage") {
                    usage["input_tokens"] = serde_json::json!((sim_input as f64 * input_mul).round() as i64);
                    usage["cache_creation_input_tokens"] = serde_json::json!((sim_cc as f64 * cache_mul).round() as i64);
                    usage["cache_read_input_tokens"] = serde_json::json!((sim_cr as f64 * cache_mul).round() as i64);
                }
                return format!("event: message_start\ndata: {}\n\n", json);
            }
            event_text.to_string()
        }
        (Some("message_delta"), Some(data)) => {
            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(data) {
                if let Some(usage) = json.get_mut("usage") {
                    if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                        usage["output_tokens"] = serde_json::json!((v as f64 * output_mul).round() as i64);
                    }
                }
                return format!("event: message_delta\ndata: {}\n\n", json);
            }
            event_text.to_string()
        }
        _ => event_text.to_string(),
    }
}

/// 处理上游流式响应：解析 SSE 事件，应用膨胀倍率和模拟缓存，与 Kiro 账号路径保持一致。
pub fn handle_upstream_stream_response_with_inflation(
    response: reqwest::Response,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
    cache_usage: CacheUsage,
) -> Response {
    // 初始状态：(行缓冲区, input_mul, output_mul, cache_mul, cache_usage)
    let initial = (String::new(), input_mul, output_mul, cache_mul, cache_usage);

    // 每个 SSE 事件最大 4MB；超出视为上游异常，关闭流防止 OOM
    const MAX_BUF: usize = 4 * 1024 * 1024;

    let inflated = response
        .bytes_stream()
        .scan(initial, move |state, chunk| {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("上游流式响应读取失败: {}", e);
                    return futures::future::ready(Some(vec![]));
                }
            };
            // 缓冲区超限：上游发送了无边界的异常数据，终止流
            if state.0.len() + bytes.len() > MAX_BUF {
                tracing::error!(
                    "上游 SSE 缓冲超过 {}MB 上限，强制关闭流",
                    MAX_BUF / 1024 / 1024
                );
                return futures::future::ready(None);
            }
            // 追加到行缓冲区，按 SSE 事件边界（\n\n）切割并逐个重写
            state.0.push_str(&String::from_utf8_lossy(&bytes));
            let mut out: Vec<Bytes> = Vec::new();
            while let Some(pos) = state.0.find("\n\n") {
                let event_text = state.0[..pos + 2].to_string();
                state.0 = state.0[pos + 2..].to_string();
                let inflated_event = inflate_sse_event(&event_text, state.1, state.2, state.3, state.4);
                out.push(Bytes::from(inflated_event));
            }
            futures::future::ready(Some(out))
        })
        .flat_map(|chunks| stream::iter(chunks.into_iter().map(Ok::<Bytes, Infallible>)));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(inflated))
        .unwrap()
}

/// 处理上游流式响应：透明代理 SSE 流（保留用于降级）。
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
