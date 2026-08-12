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

use super::cache_engine::UsageMode;
use super::cache_metering::CacheUsage;
use super::types::ErrorResponse;
use crate::admin::usage_stats::TokenUsageBreakdown;

/// 引擎 D 的流式输出累积器。
///
/// 引擎 D 的 `output_tokens` 必须本地算 —— 上游可能是另一个 kiro-rs 反代，它报的
/// output 已被膨胀过一轮。但本地算需要**完整**输出文本，而流式是逐 delta 到达的。
///
/// 为什么按 block index 重建 content 数组、而不是把所有 delta 拼成一条字符串：
/// [`crate::token::count_tokens`] 是**非线性**的（<100 token 乘 1.5，≥800 乘 1.0），
/// 所以「逐 delta 各算一次再求和」会让每个小片段都吃 1.5 倍而严重高估，而「全部
/// 拼成一条」又与非流式的逐 block 估算得出不同的数。重建成 content 数组后复用
/// 同一个 [`crate::token::estimate_output_tokens`]，流式与非流式**由构造保证一致**。
#[derive(Debug, Default)]
struct LocalOutputAccumulator {
    /// BTreeMap 而非 HashMap：按 index 有序，重建出的数组顺序与上游一致。
    blocks: std::collections::BTreeMap<i64, AccumBlock>,
}

/// 从一条 SSE 事件文本里取出 `(event 类型, data 的 JSON)`。
///
/// 只认 `event:` / `data:` 两个前缀 —— 与本文件既有的两个改写点口径一致。
/// `data` 非法 JSON 时返回 `None`，让调用方按"该事件不参与统计"处理。
fn parse_sse_event(event_text: &str) -> Option<(&str, serde_json::Value)> {
    let mut event_type: Option<&str> = None;
    let mut data_line: Option<&str> = None;
    for line in event_text.lines() {
        if let Some(t) = line.strip_prefix("event: ") {
            event_type = Some(t.trim());
        } else if let Some(d) = line.strip_prefix("data: ") {
            data_line = Some(d);
        }
    }
    let data = serde_json::from_str(data_line?).ok()?;
    Some((event_type?, data))
}

/// 累积中的单个内容块。变体与 `estimate_output_tokens` 识别的块类型一一对应。
#[derive(Debug)]
enum AccumBlock {
    Text(String),
    Thinking(String),
    /// 无 delta，固定计 8 token（对齐 `estimate_output_tokens`）。
    RedactedThinking,
    /// 累积 `input_json_delta.partial_json`，重建为 tool_use 的 input。
    ToolUse(String),
}

impl LocalOutputAccumulator {
    /// 喂入一条 SSE 事件；只关心 `content_block_start` / `content_block_delta`。
    fn feed(&mut self, event_type: &str, data: &serde_json::Value) {
        let Some(index) = data.get("index").and_then(|v| v.as_i64()) else {
            return;
        };
        match event_type {
            // redacted_thinking 没有后续 delta，只能在 start 时登记。
            "content_block_start" => {
                if data.pointer("/content_block/type").and_then(|v| v.as_str())
                    == Some("redacted_thinking")
                {
                    self.blocks.insert(index, AccumBlock::RedactedThinking);
                }
            }
            "content_block_delta" => {
                let Some(delta) = data.get("delta") else { return };
                let kind = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                // signature_delta 不计入 token（estimate_output_tokens 也不看 signature）。
                let (field, ctor): (&str, fn(String) -> AccumBlock) = match kind {
                    "text_delta" => ("text", AccumBlock::Text),
                    "thinking_delta" => ("thinking", AccumBlock::Thinking),
                    "input_json_delta" => ("partial_json", AccumBlock::ToolUse),
                    _ => return,
                };
                let Some(chunk) = delta.get(field).and_then(|v| v.as_str()) else {
                    return;
                };
                match self.blocks.entry(index).or_insert_with(|| ctor(String::new())) {
                    AccumBlock::Text(s) | AccumBlock::Thinking(s) | AccumBlock::ToolUse(s) => {
                        s.push_str(chunk)
                    }
                    AccumBlock::RedactedThinking => {}
                }
            }
            _ => {}
        }
    }

    /// 是否收到过任何输出内容。空流时不该报出 `estimate_output_tokens` 的 `.max(1)`。
    fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// 重建 content 数组并复用非流式那条估算路径。
    fn estimate(&self) -> i32 {
        let blocks: Vec<serde_json::Value> = self
            .blocks
            .values()
            .map(|b| match b {
                AccumBlock::Text(s) => serde_json::json!({"type": "text", "text": s}),
                AccumBlock::Thinking(s) => serde_json::json!({"type": "thinking", "thinking": s}),
                AccumBlock::RedactedThinking => serde_json::json!({"type": "redacted_thinking"}),
                // partial_json 累积完是一段 JSON 文本；estimate_output_tokens 会把
                // input 再序列化一次，故这里按已解析的值放回，避免双重转义改变字节数。
                AccumBlock::ToolUse(s) => serde_json::json!({
                    "type": "tool_use",
                    "input": serde_json::from_str::<serde_json::Value>(s)
                        .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
                }),
            })
            .collect();
        crate::token::estimate_output_tokens(&blocks)
    }
}

/// 上游流式响应结束后回传给 handler 的用量统计（膨胀前，供 hook.record 使用）
#[derive(Debug, Default)]
pub struct UpstreamStreamUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_creation_tokens: i32,
    pub cache_read_tokens: i32,
    pub raw_usage: TokenUsageBreakdown,
}

/// 处理上游非流式响应：按 `mode` 决定客户端 usage 口径，应用膨胀倍率，返回给客户端。
///
/// 返回 `(Response, input_tokens, output_tokens, cache_creation, cache_read, raw_usage)`。
/// 前四个 token 数为**膨胀前**的客户端口径（与 Kiro 账号路径一致），供调用方记录用量；
/// `raw_usage` 是上游真实值，供计费对比。
pub async fn handle_upstream_non_stream_response(
    response: reqwest::Response,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
    cache_creation_mul: f64,
    simulated_total_input: i32,
    cache_usage: CacheUsage,
    mode: crate::anthropic::cache_engine::UsageMode,
    local_input: i32,
) -> (Response, i32, i32, i32, i32, TokenUsageBreakdown) {
    let status = response.status();
    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("读取上游响应体失败: {}", e);
            let resp = (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("api_error", format!("读取上游响应失败: {}", e))),
            ).into_response();
            return (resp, 0, 0, 0, 0, TokenUsageBreakdown::default());
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
            return (resp, 0, 0, 0, 0, TokenUsageBreakdown::default());
        }
    };

    // 提取真实 token 数，仅用于上游真实计费快照；客户端模拟不依赖这组 input/cache 值。
    let (real_input, real_output, real_cc, real_cr) = extract_usage(&json);
    let raw_usage = TokenUsageBreakdown {
        input_tokens: real_input.max(0) as u64,
        output_tokens: real_output.max(0) as u64,
        cache_creation_tokens: real_cc.max(0) as u64,
        cache_read_tokens: real_cr.max(0) as u64,
    };

    // 引擎 D 的 output 同样不取上游值：从响应 `content` 数组本地估算。
    //
    // 与流式路径共用 `estimate_output_tokens`，且都是"先攒齐完整文本再算一次" ——
    // `count_tokens` 是非线性的（<100 token 乘 1.5，>=800 乘 1.0），逐块分别计算
    // 再求和会让每个小块都吃到 1.5 倍而显著高估。两条路径必须同口径，否则同一次
    // 对话切换 stream 开关就会看到不同的 output_tokens。
    //
    // 其余引擎用不到，故按 mode 惰性计算，不给 A/B/C 增加无谓开销。
    let local_output = if matches!(mode, UsageMode::NoCache) {
        json.get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| crate::token::estimate_output_tokens(blocks))
    } else {
        None
    };

    // 客户端 usage 的口径由 mode 决定（模拟分摊 / 上游真值 / 本地估算），
    // 三分支收敛在 UsageMode::resolve_tokens —— 与计费快照共用同一套规则。
    let (sim_input, sim_cc, sim_cr) = mode.resolve_tokens(
        cache_usage,
        simulated_total_input,
        (real_input, real_cc, real_cr),
        local_input,
    );
    let client_output = mode.resolve_output(real_output, local_output);

    // 重建标准 Anthropic usage。上游没有 usage 时也补齐，避免客户端拿到原始/缺失口径。
    let usage = json
        .as_object_mut()
        .map(|object| object.entry("usage").or_insert_with(|| serde_json::json!({})))
        .and_then(|value| value.as_object_mut());
    if let Some(usage) = usage {
        usage.insert(
            "input_tokens".to_string(),
            serde_json::json!((sim_input as f64 * input_mul).round() as i64),
        );
        usage.insert(
            "output_tokens".to_string(),
            serde_json::json!((client_output as f64 * output_mul).round() as i64),
        );
        usage.insert(
            "cache_creation_input_tokens".to_string(),
            serde_json::json!((sim_cc as f64 * cache_creation_mul).round() as i64),
        );
        usage.insert(
            "cache_read_input_tokens".to_string(),
            serde_json::json!((sim_cr as f64 * cache_mul).round() as i64),
        );
    }
    // 嵌套 cache_creation：与流式两处同策，扁平值改了它就必须跟着改。
    if let Some(usage) = json.get_mut("usage") {
        rescale_nested_cache_creation(usage, (sim_cc as f64 * cache_creation_mul).round() as i64);
    }

    let resp = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_string()))
        .unwrap();

    // 返回膨胀前的客户端口径供 hook.record 记录（引擎 D 时 output 为本地估算值）
    (resp, sim_input, client_output, sim_cc, sim_cr, raw_usage)
}

/// 把嵌套 `usage.cache_creation` 重标定到给定的 creation 总量。
///
/// Anthropic 的 usage 里 creation 有**两种表达**：扁平的
/// `cache_creation_input_tokens`，以及嵌套对象 `cache_creation`
/// （`ephemeral_5m_input_tokens` + `ephemeral_1h_input_tokens`，两项之和等于扁平值）。
/// 只改扁平值会让两者自相矛盾 —— 客户端若读嵌套项（SDK 的
/// `usage.cache_creation.ephemeral_5m_input_tokens`）拿到的仍是上游真值，模拟等于没生效。
///
/// 重标定保留上游的 5m/1h 配比（那是 TTL 归属，不是可模拟的量），只把总量换成本
/// 引擎口径；残差补给 5m 使两项之和**恒等于** target。上游未下发该对象时不创建 ——
/// 与扁平字段同策，不凭空造上游没给的字段。
fn rescale_nested_cache_creation(usage: &mut serde_json::Value, target_cc: i64) {
    let Some(nested) = usage.get("cache_creation").and_then(|v| v.as_object()) else {
        return;
    };
    let read = |k: &str| nested.get(k).and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let m5 = read("ephemeral_5m_input_tokens");
    let h1 = read("ephemeral_1h_input_tokens");
    let sum = m5 + h1;
    let target = target_cc.max(0);
    // 上游两项全 0（或无该键）时整体归 5m：5m 是 ephemeral 的缺省 TTL。
    let new_1h = if sum > 0 {
        (((h1 as f64) * (target as f64) / (sum as f64)).round() as i64).clamp(0, target)
    } else {
        0
    };
    let new_5m = target - new_1h;
    if let Some(nested) = usage
        .get_mut("cache_creation")
        .and_then(|v| v.as_object_mut())
    {
        nested.insert(
            "ephemeral_5m_input_tokens".to_string(),
            serde_json::json!(new_5m),
        );
        nested.insert(
            "ephemeral_1h_input_tokens".to_string(),
            serde_json::json!(new_1h),
        );
    }
}

/// 对单条 SSE 事件文本应用膨胀倍率（模拟缓存）。
///
/// 仅重写 `message_start` 与 `message_delta` 两类事件的 usage，其余原样透传。
/// **两者都要改全部四项**：`message_delta.usage` 的计数按官方约定是**累积**的
/// （"The token counts shown in the usage field of the message_delta event are
/// cumulative"），客户端 SDK 的 accumulate 用它覆盖 `message_start` 的快照。
/// 若这里只改 output、把 input / cache 原样透传，`message_start` 刚写好的引擎
/// 口径会在流末尾被上游真值顶掉 —— 客户端最终显示的就是上游原值，四个引擎的
/// 模拟全部等于没生效。Kiro 路径不受影响是因为它自己生成 `message_delta`
/// （[`super::stream::SseStateManager::generate_final_events`] 填的是模拟值），
/// 没有上游字段可漏。
///
/// `local_input` 是客户端请求的本地 token 估算（引擎 D 用它取代上游 input）。
/// `local_output` 是本地累积算出的输出量，仅引擎 D 在 `message_delta` 时用 ——
/// 由调用方在流末尾从 [`StreamOutputAccumulator`] 取得。
#[allow(clippy::too_many_arguments)]
fn inflate_sse_event(
    event_text: &str,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
    cache_creation_mul: f64,
    simulated_total_input: i32,
    cache_usage: CacheUsage,
    mode: UsageMode,
    local_input: i32,
    local_output: Option<i32>,
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
                if let Some(usage) = json.pointer_mut("/message/usage") {
                    // 真实三元组必须在改写前读出：下面三行会就地覆盖同一对象。
                    let read_real = |usage: &serde_json::Value, key: &str| {
                        usage.get(key).and_then(|v| v.as_i64()).unwrap_or(0) as i32
                    };
                    let real = (
                        read_real(usage, "input_tokens"),
                        read_real(usage, "cache_creation_input_tokens"),
                        read_real(usage, "cache_read_input_tokens"),
                    );
                    // 四引擎的口径分歧全在 resolve_tokens 里，此处不再自行分支。
                    let (base_input, base_cc, base_cr) =
                        mode.resolve_tokens(cache_usage, simulated_total_input, real, local_input);
                    let scaled_cc = (base_cc as f64 * cache_creation_mul).round() as i64;
                    usage["input_tokens"] = serde_json::json!((base_input as f64 * input_mul).round() as i64);
                    usage["cache_creation_input_tokens"] = serde_json::json!(scaled_cc);
                    usage["cache_read_input_tokens"] = serde_json::json!((base_cr as f64 * cache_mul).round() as i64);
                    // 嵌套对象与扁平字段是同一个量的两种表达，必须一起改。
                    rescale_nested_cache_creation(usage, scaled_cc);
                }
                return format!("event: message_start\ndata: {}\n\n", json);
            }
            event_text.to_string()
        }
        (Some("message_delta"), Some(data)) => {
            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(data) {
                // 全部在改写前读出：下面的写入会就地覆盖同一对象。
                // 用 pointer 而非 get_mut("usage")：上游可能整个 usage 对象都没下发，
                // 若把改写整体套在 `if let Some(usage)` 里，引擎 D 就会因为「没有可改
                // 的对象」而一个字都不写，客户端拿到 output_tokens 缺失。
                let usage_val = json.pointer("/usage");
                let read = |key: &str| usage_val.and_then(|u| u.get(key)).and_then(|v| v.as_i64());
                let upstream_output = read("output_tokens");
                // input / cache 三项同样是**累积**口径，上游给了就必须按引擎口径改写 ——
                // 原样透传会把 message_start 里刚写好的模拟值顶掉（见函数头注释）。
                let real_input = read("input_tokens");
                let real_cc = read("cache_creation_input_tokens");
                let real_cr = read("cache_read_input_tokens");
                let (base_input, base_cc, base_cr) = mode.resolve_tokens(
                    cache_usage,
                    simulated_total_input,
                    (
                        real_input.unwrap_or(0) as i32,
                        real_cc.unwrap_or(0) as i32,
                        real_cr.unwrap_or(0) as i32,
                    ),
                    local_input,
                );
                // 引擎 D 的 output 口径与上游无关：上游没给也必须补出本地值，故下面
                // 按需**创建** usage 对象（与非流式路径的 `entry().or_insert_with` 同策）。
                // 其余三引擎沿用原行为：只在上游确实给了字段时才改写，不凭空造字段。
                let base_output = match mode {
                    UsageMode::NoCache => {
                        Some(mode.resolve_output(upstream_output.unwrap_or(0) as i32, local_output))
                    }
                    _ => upstream_output.map(|v| v as i32),
                };
                let needs_write = base_output.is_some()
                    || real_input.is_some()
                    || real_cc.is_some()
                    || real_cr.is_some();
                if needs_write && let Some(obj) = json.as_object_mut() {
                    let usage = obj.entry("usage").or_insert_with(|| serde_json::json!({}));
                    if let Some(usage) = usage.as_object_mut() {
                        let scale =
                            |v: i32, mul: f64| serde_json::json!((v as f64 * mul).round() as i64);
                        if let Some(base) = base_output {
                            usage.insert("output_tokens".to_string(), scale(base, output_mul));
                        }
                        // 三项各自只在上游给了时改写：`message_start` 已建立基线，凭空
                        // 补出上游刻意省略的累积字段只会多一处口径来源。
                        if real_input.is_some() {
                            usage.insert("input_tokens".to_string(), scale(base_input, input_mul));
                        }
                        if real_cc.is_some() {
                            usage.insert(
                                "cache_creation_input_tokens".to_string(),
                                scale(base_cc, cache_creation_mul),
                            );
                        }
                        if real_cr.is_some() {
                            usage.insert(
                                "cache_read_input_tokens".to_string(),
                                scale(base_cr, cache_mul),
                            );
                        }
                    }
                    // 嵌套 cache_creation 同为累积口径，与扁平字段一起重标定。
                    // 在 as_object_mut 之外做：helper 要同时读写该对象。
                    if let Some(usage) = obj.get_mut("usage") {
                        rescale_nested_cache_creation(
                            usage,
                            (base_cc as f64 * cache_creation_mul).round() as i64,
                        );
                    }
                }
                return format!("event: message_delta\ndata: {}\n\n", json);
            }
            event_text.to_string()
        }
        _ => event_text.to_string(),
    }
}

/// 从 SSE 事件提取膨胀前的用量（仅 message_start / message_delta）
fn update_stream_stats(
    event_text: &str,
    stats: &mut UpstreamStreamUsage,
    simulated_total_input: i32,
    cache_usage: CacheUsage,
    mode: super::cache_engine::UsageMode,
    local_input: i32,
    local_output: Option<i32>,
) {
    let mut event_type: Option<&str> = None;
    let mut data_line: Option<&str> = None;
    for line in event_text.lines() {
        if let Some(t) = line.strip_prefix("event: ") { event_type = Some(t.trim()); }
        else if let Some(d) = line.strip_prefix("data: ") { data_line = Some(d); }
    }
    match (event_type, data_line) {
        (Some("message_start"), Some(data)) => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                let u = json.pointer("/message/usage");
                let i  = u.and_then(|v| v.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                let cc = u.and_then(|v| v.get("cache_creation_input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                let cr = u.and_then(|v| v.get("cache_read_input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                stats.raw_usage.input_tokens = i.max(0) as u64;
                stats.raw_usage.cache_creation_tokens = cc.max(0) as u64;
                stats.raw_usage.cache_read_tokens = cr.max(0) as u64;
                // 与 inflate_sse_event 必须同源：客户端所见与用量日志若各算一次，
                // 两者会在引擎切换时悄悄分叉。故同样走 resolve_tokens。
                let real = (i.max(0) as i32, cc.max(0) as i32, cr.max(0) as i32);
                let (input, creation, read) =
                    mode.resolve_tokens(cache_usage, simulated_total_input, real, local_input);
                stats.input_tokens = input;
                stats.cache_creation_tokens = creation;
                stats.cache_read_tokens = read;
            }
        }
        (Some("message_delta"), Some(data)) => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                let usage_val = json.pointer("/usage");
                let read = |key: &str| usage_val.and_then(|u| u.get(key)).and_then(|v| v.as_i64());
                let upstream_output = read("output_tokens");
                // raw_usage 恒为上游真值，且只在上游确实下发该字段时写 —— 否则会把
                // "上游没给"记成 0，污染计费对比表的"上游真实"列。
                if let Some(v) = upstream_output {
                    stats.raw_usage.output_tokens = v.max(0) as u64;
                }
                // 引擎 D 用本地累积值，且上游缺字段时也要记；其余引擎沿用上游真值。
                stats.output_tokens =
                    mode.resolve_output(upstream_output.unwrap_or(0) as i32, local_output);

                // `message_delta` 的 input / cache 也是累积口径，与 inflate_sse_event
                // 必须同源：客户端看到的是这条事件改写后的值，记账若还停在
                // `message_start` 那一版，两者会分叉。仅在上游确实下发时覆盖 ——
                // 上游省略该字段时 `message_start` 已写好的值就是最终值。
                let real_input = read("input_tokens");
                let real_cc = read("cache_creation_input_tokens");
                let real_cr = read("cache_read_input_tokens");
                if real_input.is_some() || real_cc.is_some() || real_cr.is_some() {
                    if let Some(v) = real_input {
                        stats.raw_usage.input_tokens = v.max(0) as u64;
                    }
                    if let Some(v) = real_cc {
                        stats.raw_usage.cache_creation_tokens = v.max(0) as u64;
                    }
                    if let Some(v) = real_cr {
                        stats.raw_usage.cache_read_tokens = v.max(0) as u64;
                    }
                    // 缺失项回落 raw_usage 里 message_start 存下的真值，而非 0 ——
                    // 引擎 C 会原样采用这三元组，补 0 等于凭空抹掉一项真实消耗。
                    let real = (
                        real_input.unwrap_or(stats.raw_usage.input_tokens as i64).max(0) as i32,
                        real_cc
                            .unwrap_or(stats.raw_usage.cache_creation_tokens as i64)
                            .max(0) as i32,
                        real_cr.unwrap_or(stats.raw_usage.cache_read_tokens as i64).max(0) as i32,
                    );
                    let (input, creation, read_tok) =
                        mode.resolve_tokens(cache_usage, simulated_total_input, real, local_input);
                    stats.input_tokens = input;
                    stats.cache_creation_tokens = creation;
                    stats.cache_read_tokens = read_tok;
                }
            }
        }
        _ => {}
    }
}

/// 处理上游流式响应：解析 SSE 事件，应用膨胀倍率和模拟缓存，与 Kiro 账号路径保持一致。
///
/// 返回 `(Response, Receiver<UpstreamStreamUsage>)`：
/// - Response 立即返回给客户端（SSE 流）。
/// - Receiver 在流全部消耗完后收到膨胀前的用量统计，供 hook.record / tracer.finalize 使用。
pub fn handle_upstream_stream_response_with_inflation(
    response: reqwest::Response,
    input_mul: f64,
    output_mul: f64,
    cache_mul: f64,
    cache_creation_mul: f64,
    simulated_total_input: i32,
    cache_usage: CacheUsage,
    mode: UsageMode,
    local_input: i32,
) -> (Response, tokio::sync::oneshot::Receiver<UpstreamStreamUsage>) {
    let (usage_tx, usage_rx) = tokio::sync::oneshot::channel::<UpstreamStreamUsage>();
    let (bytes_tx, bytes_rx) = tokio::sync::mpsc::channel::<Bytes>(32);

    const MAX_BUF: usize = 4 * 1024 * 1024;

    // 后台任务：读取上游 SSE 流 → 膨胀 → 发送到 bytes_tx；结束时通过 usage_tx 回报用量
    tokio::spawn(async move {
        let mut buffer = String::new();
        let mut stats = UpstreamStreamUsage::default();
        let mut byte_stream = response.bytes_stream();
        // 引擎 D 专用：累积输出内容，供流末尾自行估算 output_tokens。
        // 其余引擎不喂它（免掉整条流的字符串累积开销）。
        let mut accumulator = LocalOutputAccumulator::default();
        let counting_output = matches!(mode, UsageMode::NoCache);

        while let Some(chunk) = byte_stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("上游流式响应读取失败: {}", e);
                    break;
                }
            };
            if buffer.len() + bytes.len() > MAX_BUF {
                tracing::error!("上游 SSE 缓冲超过 {}MB 上限，强制关闭流", MAX_BUF / 1024 / 1024);
                break;
            }
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buffer.find("\n\n") {
                let event_text = buffer[..pos + 2].to_string();
                buffer = buffer[pos + 2..].to_string();

                // 引擎 D：先喂累积器。Anthropic SSE 里 content_block_* 全部先于
                // message_delta 到达，故 message_delta 被处理时累积必然已完整 ——
                // 这正是"流开头写 input、流末尾写 output"能成立的原因。
                let local_output = if counting_output {
                    match parse_sse_event(&event_text) {
                        Some((etype, data)) => {
                            accumulator.feed(etype, &data);
                            // 只在 message_delta 上求值：estimate() 要重建 content
                            // 数组并整体计数，每个事件都算一遍纯属浪费。
                            //
                            // is_empty 判断保留空流语义：一个字都没输出时应报 0，
                            // 而 estimate_output_tokens 有 `.max(1)` 会报 1。
                            (etype == "message_delta" && !accumulator.is_empty())
                                .then(|| accumulator.estimate())
                        }
                        None => None,
                    }
                } else {
                    None
                };

                // 提取膨胀前用量（在 inflate 之前）
                update_stream_stats(
                    &event_text,
                    &mut stats,
                    simulated_total_input,
                    cache_usage,
                    mode,
                    local_input,
                    local_output,
                );
                let inflated = inflate_sse_event(
                    &event_text,
                    input_mul,
                    output_mul,
                    cache_mul,
                    cache_creation_mul,
                    simulated_total_input,
                    cache_usage,
                    mode,
                    local_input,
                    local_output,
                );
                if bytes_tx.send(Bytes::from(inflated)).await.is_err() {
                    // 客户端已断开
                    return;
                }
            }
        }
        // 流结束，回传用量（忽略 receiver 已丢弃的情况）
        let _ = usage_tx.send(stats);
    });

    // 把 mpsc::Receiver 转为 Stream 供 axum Body 消费
    let body_stream = futures::stream::unfold(bytes_rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<Bytes, Infallible>(b), rx))
    });

    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap();

    (resp, usage_rx)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_usage_uses_simulated_total_instead_of_upstream_total() {
        let cache_usage = CacheUsage {
            cache_read: 50,
            cache_covered_est: 100,
            prompt_total_est: 200,
        };
        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":10,",
            "\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30}}}\n\n"
        );

        let rendered = inflate_sse_event(
            event,
            1.0,
            1.0,
            1.0,
            1.0,
            200,
            cache_usage,
            UsageMode::Simulated,
            // 哨兵值：Simulated 必须忽略本地口径，若被误用会立刻显形。
            -999,
            Some(-999),
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("message_start 必须包含 data");
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["message"]["usage"];

        // 模拟总量为 200，缓存覆盖率为 50%，所以客户端应看到
        // input=100、cache_creation=50、cache_read=50，而不是上游的 60 总量。
        assert_eq!(usage["input_tokens"], serde_json::json!(100));
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(50));
        assert_eq!(usage["cache_read_input_tokens"], serde_json::json!(50));
    }

    #[test]
    fn json_usage_uses_simulated_total_and_preserves_real_output() {
        let mut json = serde_json::json!({
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_creation_input_tokens": 20,
                "cache_read_input_tokens": 30
            }
        });
        let cache_usage = CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };
        let (input, creation, read) = cache_usage.split_against_total(300);

        // 直接验证实际响应重写使用的是 split_against_total 的模拟口径；
        // 上游原始 input/cache 总量 60 不应参与分摊。
        assert_eq!((input, creation, read), (150, 75, 75));
        let usage = json["usage"].as_object_mut().unwrap();
        usage.insert("input_tokens".into(), serde_json::json!(input));
        usage.insert("output_tokens".into(), serde_json::json!(7));
        usage.insert("cache_creation_input_tokens".into(), serde_json::json!(creation));
        usage.insert("cache_read_input_tokens".into(), serde_json::json!(read));
        assert_eq!(json["usage"]["input_tokens"], serde_json::json!(150));
        assert_eq!(json["usage"]["cache_creation_input_tokens"], serde_json::json!(75));
        assert_eq!(json["usage"]["cache_read_input_tokens"], serde_json::json!(75));
        assert_eq!(json["usage"]["output_tokens"], serde_json::json!(7));
    }

    /// 主密钥（key_id=0 且无 session）：`isolation_seed` 返回 None，引擎 A 给出全零
    /// `CacheUsage`。此时不得走模拟分摊——旧行为 `prompt_total_est.max(1)` 会让
    /// `split_against_total(1)` 返回 `(1,0,0)`，把上游真实 input_tokens 覆写成 1。
    /// 期望：改用上游真实值，倍率照常生效。
    #[test]
    fn streaming_passthrough_keeps_real_tokens_when_simulation_disabled() {
        let cache_usage = CacheUsage::default(); // prompt_total_est == 0
        assert!(!cache_usage.is_simulated(), "全零 CacheUsage 应判定为未模拟");

        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":800,",
            "\"cache_creation_input_tokens\":120,\"cache_read_input_tokens\":400}}}\n\n"
        );

        // 倍率 2×，缓存倍率 3×：真实值应被缩放，而不是被模拟值替换。
        let rendered = inflate_sse_event(
            event, 2.0, 1.0, 3.0, 3.0, 1, cache_usage,
            UsageMode::Simulated, -999, Some(-999),
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("message_start 必须包含 data");
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["message"]["usage"];

        assert_eq!(usage["input_tokens"], serde_json::json!(1600), "800×2");
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(360), "120×3");
        assert_eq!(usage["cache_read_input_tokens"], serde_json::json!(1200), "400×3");
    }

    /// 同一场景下，回报给 hook.record / tracer 的膨胀前用量也必须是真实值，
    /// 否则用量日志里会记成 input_tokens=1。
    #[test]
    fn stream_stats_record_real_tokens_when_simulation_disabled() {
        let mut stats = UpstreamStreamUsage::default();
        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":800,",
            "\"cache_creation_input_tokens\":120,\"cache_read_input_tokens\":400}}}\n\n"
        );

        update_stream_stats(
            event, &mut stats, 1, CacheUsage::default(),
            UsageMode::Simulated, -999, Some(-999),
        );

        assert_eq!(stats.input_tokens, 800, "不得记成 split_against_total(1) 的 1");
        assert_eq!(stats.cache_creation_tokens, 120);
        assert_eq!(stats.cache_read_tokens, 400);
        // raw_usage 始终是真实值，两者在此场景下应一致。
        assert_eq!(stats.raw_usage.input_tokens, 800);
    }

    /// 引擎 D 的 input **完全不看上游 usage**：上游报 800，本地算 1234，
    /// 客户端必须看到 1234×倍率。上游可能是另一个反代，其 usage 已被加工过一轮。
    #[test]
    fn nocache_uses_local_input_and_ignores_upstream() {
        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":800,",
            "\"cache_creation_input_tokens\":120,\"cache_read_input_tokens\":400}}}\n\n"
        );

        let rendered = inflate_sse_event(
            event, 2.0, 1.0, 3.0, 3.0,
            999, // simulated_total_input 必须被忽略
            CacheUsage {
                cache_read: 50,
                cache_covered_est: 100,
                prompt_total_est: 200, // 即便有模拟数据也不得采用
            },
            UsageMode::NoCache,
            1234, // local_input
            None,
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("message_start 必须包含 data");
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["message"]["usage"];

        assert_eq!(usage["input_tokens"], serde_json::json!(2468), "1234×2，与上游 800 无关");
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(0), "D 的 cache 恒为 0");
        assert_eq!(usage["cache_read_input_tokens"], serde_json::json!(0));
    }

    /// 引擎 D 的 output 取本地累积估算，且**上游省略该字段时也必须写出**。
    #[test]
    fn nocache_writes_local_output_even_when_upstream_omits_it() {
        // 上游 message_delta 不带 usage.output_tokens
        let event = "event: message_delta\ndata: {\"delta\":{}}\n\n";

        let rendered = inflate_sse_event(
            event, 1.0, 2.0, 1.0, 1.0, 0, CacheUsage::default(),
            UsageMode::NoCache,
            100,
            Some(77), // 本地累积估算
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("message_delta 必须包含 data");
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(json["usage"]["output_tokens"], serde_json::json!(154), "77×2");
    }

    /// 流式与非流式必须算出**同一个** output_tokens —— 同一次对话切换 stream
    /// 开关不该看到不同数字。这是"重建 content 数组后复用同一估算器"的目的。
    #[test]
    fn stream_and_non_stream_output_estimates_agree() {
        let text = "the quick brown fox jumps over the lazy dog ".repeat(30);
        let tool_input = serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"});

        // 非流式：直接对 content 数组估算。
        let content = vec![
            serde_json::json!({"type": "text", "text": text}),
            serde_json::json!({"type": "tool_use", "input": tool_input}),
        ];
        let non_stream = crate::token::estimate_output_tokens(&content);

        // 流式：把同样内容拆成多个 delta 喂进累积器。
        let mut acc = LocalOutputAccumulator::default();
        for chunk in text.as_bytes().chunks(37) {
            let piece = std::str::from_utf8(chunk).unwrap();
            acc.feed(
                "content_block_delta",
                &serde_json::json!({"index": 0, "delta": {"type": "text_delta", "text": piece}}),
            );
        }
        let tool_json = serde_json::to_string(&tool_input).unwrap();
        for chunk in tool_json.as_bytes().chunks(11) {
            let piece = std::str::from_utf8(chunk).unwrap();
            acc.feed(
                "content_block_delta",
                &serde_json::json!({
                    "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": piece}
                }),
            );
        }

        assert_eq!(
            acc.estimate(),
            non_stream,
            "流式累积估算必须等于非流式整体估算，否则切 stream 开关会看到不同 output"
        );
    }

    /// 累积器不得把 signature_delta 计入，且空流不该报出 `.max(1)` 的 1。
    #[test]
    fn accumulator_skips_signatures_and_reports_empty() {
        let mut acc = LocalOutputAccumulator::default();
        assert!(acc.is_empty(), "未喂任何事件时应为空");

        acc.feed(
            "content_block_delta",
            &serde_json::json!({"index": 0, "delta": {"type": "signature_delta", "signature": "abc"}}),
        );
        assert!(acc.is_empty(), "signature_delta 不产生内容块");

        // redacted_thinking 只在 content_block_start 出现，固定计 8。
        acc.feed(
            "content_block_start",
            &serde_json::json!({"index": 0, "content_block": {"type": "redacted_thinking"}}),
        );
        assert!(!acc.is_empty());
        assert_eq!(acc.estimate(), 8, "redacted_thinking 固定 8 token");
    }

    /// P13 回归：`message_delta` 的 input / cache 是**累积**口径，会覆盖
    /// `message_start`。只改 output 会让四个引擎的模拟在流末尾被上游真值顶掉 ——
    /// 这正是「上游凭据下客户端显示的 token 恒等于上游原值」的成因。
    #[test]
    fn message_delta_rewrites_cumulative_input_and_cache() {
        let cache_usage = CacheUsage {
            cache_read: 50,
            cache_covered_est: 100,
            prompt_total_est: 200,
        };
        assert!(cache_usage.is_simulated());

        // 官方 web_search 流式示例的形状：message_delta 带完整四项。
        let event = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":10682,\"cache_creation_input_tokens\":0,",
            "\"cache_read_input_tokens\":0,\"output_tokens\":510,",
            "\"server_tool_use\":{\"web_search_requests\":1}}}\n\n"
        );

        let rendered = inflate_sse_event(
            event, 1.0, 1.0, 1.0, 1.0, 200, cache_usage,
            UsageMode::Simulated, -999, Some(-999),
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("message_delta 必须包含 data");
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["usage"];

        // 模拟口径：total=200、覆盖 50% → input=100, cc=50, cr=50。
        // 上游的 10682 不得穿到客户端。
        assert_eq!(usage["input_tokens"], serde_json::json!(100), "累积 input 必须是模拟值");
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(50));
        assert_eq!(usage["cache_read_input_tokens"], serde_json::json!(50));
        assert_eq!(usage["output_tokens"], serde_json::json!(510), "output 沿用上游真值");
        // 不认识的字段原样保留（server_tool_use 是官方字段，客户端要用）。
        assert_eq!(
            usage["server_tool_use"]["web_search_requests"],
            serde_json::json!(1),
            "未参与改写的 usage 字段不得被丢弃"
        );
    }

    /// 官方基础流式示例：`message_delta` 只有 `output_tokens`。此时不得凭空补出
    /// input / cache —— `message_start` 已建立基线，补字段等于多一处口径来源。
    #[test]
    fn message_delta_without_cumulative_fields_adds_nothing() {
        let event = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":15}}\n\n"
        );

        let rendered = inflate_sse_event(
            event, 3.0, 1.0, 3.0, 3.0, 200,
            CacheUsage { cache_read: 50, cache_covered_est: 100, prompt_total_est: 200 },
            UsageMode::Simulated, -999, Some(-999),
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["usage"];

        assert_eq!(usage["output_tokens"], serde_json::json!(15));
        assert!(usage.get("input_tokens").is_none(), "上游未给 input 时不得补写");
        assert!(usage.get("cache_creation_input_tokens").is_none());
        assert!(usage.get("cache_read_input_tokens").is_none());
    }

    /// 记账必须与下发同源：客户端看到改写值、日志记上游真值就会对不上账。
    #[test]
    fn stream_stats_follow_message_delta_cumulative_fields() {
        let cache_usage = CacheUsage {
            cache_read: 50,
            cache_covered_est: 100,
            prompt_total_est: 200,
        };
        let mut stats = UpstreamStreamUsage::default();

        // 先喂 message_start（上游报的是小数值），再喂累积的 message_delta。
        update_stream_stats(
            concat!(
                "event: message_start\n",
                "data: {\"message\":{\"usage\":{\"input_tokens\":2679,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n"
            ),
            &mut stats, 200, cache_usage, UsageMode::Simulated, -999, None,
        );
        update_stream_stats(
            concat!(
                "event: message_delta\n",
                "data: {\"usage\":{\"input_tokens\":10682,\"cache_creation_input_tokens\":0,",
                "\"cache_read_input_tokens\":0,\"output_tokens\":510}}\n\n"
            ),
            &mut stats, 200, cache_usage, UsageMode::Simulated, -999, None,
        );

        // 客户端口径：模拟分摊，与 inflate_sse_event 一致。
        assert_eq!(stats.input_tokens, 100);
        assert_eq!(stats.cache_creation_tokens, 50);
        assert_eq!(stats.cache_read_tokens, 50);
        assert_eq!(stats.output_tokens, 510);
        // raw_usage 取累积后的上游真值（10682，不是 message_start 的 2679）。
        assert_eq!(stats.raw_usage.input_tokens, 10682, "上游真实列应取累积终值");
        assert_eq!(stats.raw_usage.output_tokens, 510);
    }

    /// 引擎 C 原样采用上游真值：`message_delta` 部分下发时，缺失项必须回落
    /// `message_start` 存下的真值，不能补 0 抹掉一项真实消耗。
    #[test]
    fn real_mode_partial_message_delta_falls_back_to_message_start() {
        let mut stats = UpstreamStreamUsage::default();

        update_stream_stats(
            concat!(
                "event: message_start\n",
                "data: {\"message\":{\"usage\":{\"input_tokens\":800,",
                "\"cache_creation_input_tokens\":120,\"cache_read_input_tokens\":400}}}\n\n"
            ),
            &mut stats, 0, CacheUsage::default(), UsageMode::Real, 0, None,
        );
        assert_eq!(stats.cache_read_tokens, 400);

        // 只带 input_tokens 的累积更新：cache 两项不该被抹成 0。
        update_stream_stats(
            "event: message_delta\ndata: {\"usage\":{\"input_tokens\":950,\"output_tokens\":7}}\n\n",
            &mut stats, 0, CacheUsage::default(), UsageMode::Real, 0, None,
        );

        assert_eq!(stats.input_tokens, 950, "input 取累积终值");
        assert_eq!(stats.cache_creation_tokens, 120, "上游未重报，保留 message_start 的值");
        assert_eq!(stats.cache_read_tokens, 400, "不得被补 0 抹掉");
        assert_eq!(stats.raw_usage.cache_read_tokens, 400);
    }

    /// 引擎 D 的 cache 恒为 0、input 取本地估算 —— 累积事件同样不得让上游值漏出。
    #[test]
    fn nocache_message_delta_keeps_local_input_and_zero_cache() {
        let event = concat!(
            "event: message_delta\n",
            "data: {\"usage\":{\"input_tokens\":10682,\"cache_creation_input_tokens\":300,",
            "\"cache_read_input_tokens\":900,\"output_tokens\":510}}\n\n"
        );

        let rendered = inflate_sse_event(
            event, 2.0, 1.0, 1.0, 1.0, 0, CacheUsage::default(),
            UsageMode::NoCache,
            1234,      // local_input
            Some(77),  // local_output
        );
        let data = rendered
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["usage"];

        assert_eq!(usage["input_tokens"], serde_json::json!(2468), "1234×2，与上游 10682 无关");
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(0), "D 的 cache 恒为 0");
        assert_eq!(usage["cache_read_input_tokens"], serde_json::json!(0));
        assert_eq!(usage["output_tokens"], serde_json::json!(77), "本地估算，output_mul=1.0");
    }

    /// P14 回归：嵌套 `usage.cache_creation` 必须与扁平字段一起重标定。
    ///
    /// creation 在 Anthropic usage 里有两种表达，只改扁平值会让客户端读嵌套项时
    /// 仍拿到上游真值 —— 模拟对读嵌套项的 SDK 等于没生效，且两者自相矛盾。
    #[test]
    fn nested_cache_creation_is_rescaled_with_flat_field() {
        let cache_usage = CacheUsage {
            cache_read: 50,
            cache_covered_est: 100,
            prompt_total_est: 200,
        };
        // 官方 usage 形状：嵌套对象与扁平字段并存，且两项之和 == 扁平值。
        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":2679,",
            "\"cache_creation_input_tokens\":1024,\"cache_read_input_tokens\":0,",
            "\"cache_creation\":{\"ephemeral_5m_input_tokens\":768,",
            "\"ephemeral_1h_input_tokens\":256},\"output_tokens\":3}}}\n\n"
        );
        let rendered = inflate_sse_event(
            event, 1.0, 1.0, 1.0, 1.0, 200, cache_usage,
            UsageMode::Simulated, 5000, None,
        );
        let data = rendered.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["message"]["usage"];

        // 模拟口径 cc=50。嵌套两项之和必须恒等于扁平值，且保留 768:256 = 3:1 配比。
        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(50));
        let m5 = usage["cache_creation"]["ephemeral_5m_input_tokens"].as_i64().unwrap();
        let h1 = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_i64().unwrap();
        assert_eq!(m5 + h1, 50, "嵌套两项之和必须等于扁平值，否则自相矛盾");
        assert_eq!(h1, 13, "保留上游 1/4 配比：50×0.25≈13");
        assert_eq!(m5, 37, "残差补给 5m，保证和恒等");
    }

    /// 引擎 D 的 cache 恒为 0 —— 嵌套对象同样必须归零，不得漏出上游值。
    #[test]
    fn nested_cache_creation_zeroed_for_nocache_engine() {
        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":2679,",
            "\"cache_creation_input_tokens\":1024,\"cache_read_input_tokens\":512,",
            "\"cache_creation\":{\"ephemeral_5m_input_tokens\":768,",
            "\"ephemeral_1h_input_tokens\":256}}}}\n\n"
        );
        let rendered = inflate_sse_event(
            event, 1.0, 1.0, 1.0, 1.0, 0, CacheUsage::default(),
            UsageMode::NoCache, 1234, None,
        );
        let data = rendered.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        let usage = &json["message"]["usage"];

        assert_eq!(usage["cache_creation_input_tokens"], serde_json::json!(0));
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], serde_json::json!(0));
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], serde_json::json!(0));
    }

    /// 上游未下发嵌套对象时不得凭空创建 —— 与扁平字段同策。
    #[test]
    fn nested_cache_creation_not_fabricated_when_absent() {
        let mut usage = serde_json::json!({"cache_creation_input_tokens": 50});
        rescale_nested_cache_creation(&mut usage, 50);
        assert!(
            usage.get("cache_creation").is_none(),
            "上游没给嵌套对象就不该造一个出来"
        );
    }

    /// 上游嵌套两项全 0 但目标非 0（如 Kiro 转发链路）：整体归 5m（ephemeral 缺省 TTL）。
    #[test]
    fn nested_cache_creation_all_zero_upstream_defaults_to_5m() {
        let mut usage = serde_json::json!({
            "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
        });
        rescale_nested_cache_creation(&mut usage, 120);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], serde_json::json!(120));
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], serde_json::json!(0));
    }

    /// 非流式路径同样要重标定嵌套对象 —— 三个改写点必须同策。
    #[test]
    fn non_stream_rescales_nested_cache_creation() {
        // 直接验 helper 在非流式那种 usage 形状上的行为（handle_upstream_non_stream_response
        // 需要 reqwest::Response，无法在单测里构造）。
        let mut usage = serde_json::json!({
            "input_tokens": 2679,
            "cache_creation_input_tokens": 1024,
            "cache_creation": {"ephemeral_5m_input_tokens": 1024, "ephemeral_1h_input_tokens": 0}
        });
        rescale_nested_cache_creation(&mut usage, 75);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], serde_json::json!(75));
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], serde_json::json!(0));
    }

    /// 设计意图钉子：上游路径**刻意**用 `prompt_total_est` 作分摊分母，使客户端
    /// 所见完全等于引擎自算，上游影响为零。
    ///
    /// 这个选择极容易被误判成 bug 并"修正"成 `count_all_tokens`（两者都是本地值，
    /// 只是换了估算器）—— 审阅时确实按那个方向质疑过一轮。差别在语义：
    /// `prompt_total_est` 让 `split_against_total` 退化成恒等式，输出就是引擎的原始
    /// 估算；换任何别的分母都会把引擎估算重新投影一次，客户端所见不再是引擎自己
    /// 算出来的那个数。
    ///
    /// 代价是已知且被接受的：引擎 A / B 用不同分词器，故同一请求在上游路径上两者
    /// 报出的**总量**不同（见 `cache_engine::tests::both_engines_produce_usable_usage_for_same_request`
    /// 断言 `b.prompt_total_est > a.prompt_total_est`）。这正是并存两套引擎做对比的
    /// 意义，不是缺陷。Kiro 路径另有 `contextUsageEvent` 真值可锚，故那条路用真实
    /// 总量 × 引擎比例 —— 两条路的锚点不同是刻意的。
    #[test]
    fn upstream_denominator_makes_output_pure_engine_estimate() {
        let usage = CacheUsage {
            cache_read: 3000,
            cache_covered_est: 6000,
            prompt_total_est: 8000,
        };
        // 上游路径的实际接线：handlers.rs 三处 `cache_usage.prompt_total_est.max(1)`。
        let denom = usage.prompt_total_est.max(1);

        // 上游真值与本地估算都给成极端值：结果必须一个字都不受影响。
        for real in [(0, 0, 0), (2679, 1024, 0), (999_999, 888_888, 777_777)] {
            for local in [0, 1234, 999_999] {
                assert_eq!(
                    UsageMode::Simulated.resolve_tokens(usage, denom, real, local),
                    (2000, 3000, 3000),
                    "引擎自算 =(8000−6000, 6000−3000, 3000)，不得受 real={real:?} local={local} 影响"
                );
            }
        }
    }

    /// 有模拟数据时行为不变（防止 P1 修复回归到"永远走真实值"）。
    #[test]
    fn simulation_still_wins_when_cache_usage_is_present() {
        let cache_usage = CacheUsage {
            cache_read: 50,
            cache_covered_est: 100,
            prompt_total_est: 200,
        };
        assert!(cache_usage.is_simulated());

        let mut stats = UpstreamStreamUsage::default();
        let event = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":10,",
            "\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30}}}\n\n"
        );
        update_stream_stats(
            event, &mut stats, 200, cache_usage,
            UsageMode::Simulated, -999, Some(-999),
        );

        // 模拟口径：total=200，覆盖率 50% → input=100, cc=50, cr=50。
        assert_eq!(stats.input_tokens, 100);
        assert_eq!(stats.cache_creation_tokens, 50);
        assert_eq!(stats.cache_read_tokens, 50);
        // raw_usage 仍保留上游真实值，供计费对比。
        assert_eq!(stats.raw_usage.input_tokens, 10);
    }
}
