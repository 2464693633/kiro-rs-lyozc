//! go 缓存模拟引擎（移植自 kiro-go-lyozc `proxy/cache_tracker.go` + `proxy/token_estimator.go`）。
//!
//! 与引擎 A（[`super::cache_metering`]）并存，由客户端 Key 上的 `cacheEngine` 字段选择。
//! 两者算法同源（前缀哈希链 + 最深命中 = cache_read），但实现刻意不同：
//!
//! - 指纹取**结构化 wrapper 的 canonical JSON** 哈希，而非签名字符串
//! - token 估算喂的是 canonical JSON，**结构噪声计入**
//! - 按实际 Kiro credential 隔离缓存；同一账号的不同客户端 Key 共享前缀
//!   （引擎 A 仍有额外的会话隔离规则）
//! - **两阶段**：`compute` 只查、`update` 才写且只在请求成功后调用
//!
//! 复刻 Go 的 HTML 转义：Go 的 `json.Marshal` 默认把 `<` `>` `&` 转成
//! `<` `>` `&`，serde_json 不转。此前未对齐时，含这三个字符的
//! 内容（HTML / 代码 / XML）在两侧算出**不同指纹**——虽然 Rust 侧仍自一致，
//! 但 canonical JSON 的字节长度不同，喂给估算器的 token 数随之偏移，
//! 且与 kiro-go 的实测数值无法对照。现由 [`push_go_escaped_string`] 对齐。

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 默认 prompt cache TTL（5min），对齐 Go `defaultPromptCacheTTL`。
pub const DEFAULT_PROMPT_CACHE_TTL_MS: i64 = 5 * 60 * 1000;
/// 最长 TTL（1h），对齐 Anthropic `ttl="1h"`。
pub const MAX_PROMPT_CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// Go `estimateApproxTokens` 的逐字移植。
///
/// 分类用**显式字符范围**而非 `unicode.IsDigit`/`IsLetter`：空格 / 制表 / 换行
/// 以及 ASCII 控制符都靠 fallthrough 落进 `regular`，不是白名单命中。
/// 短串分支是 `max(1, ceil(n/3))`——1 个字符返回 1 而非 0。
pub fn estimate_approx_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    // Go 用 []rune；Rust 对应 chars()。不可用 bytes()/len()。
    let length = text.chars().count() as i64;
    if length == 0 {
        return 0;
    }
    if length < 5 {
        return std::cmp::max(1, (length + 2) / 3); // ceil(n/3)
    }

    let (mut regular, mut digits, mut symbols, mut non_ascii) = (0i64, 0i64, 0i64, 0i64);
    for c in text.chars() {
        if c as u32 >= 0x80 {
            non_ascii += 1;
        } else if c.is_ascii_digit() {
            digits += 1;
        } else if matches!(c, '!'..='/' | ':'..='@' | '['..='`' | '{'..='~') {
            symbols += 1;
        } else {
            regular += 1;
        }
    }

    let total = regular as f64 / 4.5
        + digits as f64 / 2.0
        + symbols as f64 / 1.5
        + non_ascii as f64 / 1.5;
    let estimated = total.ceil() as i64;
    if estimated < 1 { 1 } else { estimated }
}

/// Mirrors kiro-go's `estimateClaudeRequestInputTokens`.
///
/// This is intentionally separate from the canonical-JSON block estimate used
/// for individual breakpoints. The Go implementation uses content-oriented
/// request totals as the denominator when it later rescales cache coverage to
/// the upstream's real input token count.
pub fn estimate_claude_request_input_tokens(req: &MessagesRequest) -> i64 {
    let mut total = 0;

    if let Some(system) = &req.system {
        for block in system {
            total += estimate_approx_tokens(&block.text);
        }
    }

    for message in &req.messages {
        total += estimate_claude_value_tokens(&message.content);
    }

    if let Some(tools) = &req.tools {
        for tool in tools {
            total += estimate_approx_tokens(&tool.name);
            total += estimate_approx_tokens(&tool.description);
            let schema = serde_json::to_value(&tool.input_schema).unwrap_or(Value::Null);
            total += estimate_json_tokens(&schema);
        }
    }

    total
}

fn estimate_claude_value_tokens(value: &Value) -> i64 {
    match value {
        Value::Null => 0,
        Value::String(text) => estimate_approx_tokens(text),
        Value::Array(items) => items.iter().map(estimate_claude_value_tokens).sum(),
        Value::Object(map) => {
            let block_type = map.get("type").and_then(Value::as_str).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(text) = map.get("text").and_then(Value::as_str) {
                        return estimate_approx_tokens(text);
                    }
                }
                "thinking" => {
                    if let Some(text) = map.get("thinking").and_then(Value::as_str) {
                        return estimate_approx_tokens(text);
                    }
                }
                "tool_use" => {
                    let mut total = map
                        .get("name")
                        .and_then(Value::as_str)
                        .map(estimate_approx_tokens)
                        .unwrap_or(0);
                    if let Some(input) = map.get("input") {
                        total += estimate_json_tokens(input);
                    }
                    if total > 0 {
                        return total;
                    }
                }
                "tool_result" => {
                    if let Some(content) = map.get("content") {
                        return estimate_claude_value_tokens(content);
                    }
                }
                _ => {}
            }

            let mut total = map
                .get("text")
                .and_then(Value::as_str)
                .map(estimate_approx_tokens)
                .unwrap_or(0);
            total += map
                .get("thinking")
                .and_then(Value::as_str)
                .map(estimate_approx_tokens)
                .unwrap_or(0);
            if let Some(content) = map.get("content") {
                total += estimate_claude_value_tokens(content);
            }
            if total > 0 {
                total
            } else {
                estimate_json_tokens(value)
            }
        }
        _ => estimate_json_tokens(value),
    }
}

fn estimate_json_tokens(value: &Value) -> i64 {
    estimate_approx_tokens(&canonicalize_cache_value(value))
}

/// Go `canonicalizeCacheValue` / `writeCanonicalJSON`。
///
/// 紧凑输出、map key 按字典序、**每一层都跳过名为 `cache_control` 的 key**
/// （TTL 标记不参与指纹，否则同一内容会因 ttl 标注不同而哈希成两个值）。
pub fn canonicalize_cache_value(value: &Value) -> String {
    let mut buf = String::new();
    write_canonical_json(&mut buf, value);
    buf
}

/// 按 Go `json.Marshal` 的口径写入 JSON 字符串字面量。
///
/// Go 的 `encoding/json` 默认开启 HTML 转义（`SetEscapeHTML(true)` 是默认值），
/// 把 `<` `>` `&` 编码成 `<` `>` `&`；serde_json 不做这层转换。
/// 这三个字符在 HTML / 代码 / XML 内容里极常见，不对齐会让 canonical JSON
/// 的字节长度与 Go 侧不同，进而影响指纹与 token 估算。
///
/// 其余转义（引号、反斜杠、控制字符、Unicode）沿用 serde_json，两侧一致。
fn push_go_escaped_string(buf: &mut String, s: &str) {
    let encoded = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    for ch in encoded.chars() {
        match ch {
            '<' => buf.push_str("\\u003c"),
            '>' => buf.push_str("\\u003e"),
            '&' => buf.push_str("\\u0026"),
            other => buf.push(other),
        }
    }
}

fn write_canonical_json(buf: &mut String, value: &Value) {
    match value {
        Value::Null => buf.push_str("null"),
        Value::Bool(true) => buf.push_str("true"),
        Value::Bool(false) => buf.push_str("false"),
        Value::Number(n) => buf.push_str(&n.to_string()),
        Value::String(s) => push_go_escaped_string(buf, s),
        Value::Array(items) => {
            buf.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                write_canonical_json(buf, item);
            }
            buf.push(']');
        }
        Value::Object(map) => {
            buf.push('{');
            // serde_json 未开 preserve_order → Map 底层是 BTreeMap，迭代已按 key
            // 排序，等价 Go 的 sort.Strings。canonical_keys_are_sorted 测试钉住此
            // 前提：若将来有传递依赖开启 preserve_order，指纹会静默改变。
            let mut first = true;
            for (key, val) in map {
                if key == "cache_control" {
                    continue;
                }
                if !first {
                    buf.push(',');
                }
                first = false;
                push_go_escaped_string(buf, key);
                buf.push(':');
                write_canonical_json(buf, val);
            }
            buf.push('}');
        }
    }
}

/// Go `writeHashChunk`：`ascii(len) \0 bytes \0`。
///
/// 长度前缀防止「不同分块拼接出相同字节流」的碰撞。注意 Go 用的是
/// `strconv.Itoa(len(chunk))` 即**字节**长度（而估算器用 rune），
/// Rust `str::len()` 同为字节，此不对称需保留。
pub fn write_hash_chunk(hasher: &mut Sha256, chunk: &str) {
    hasher.update(chunk.len().to_string().as_bytes());
    hasher.update([0u8]);
    hasher.update(chunk.as_bytes());
    hasher.update([0u8]);
}

/// Go `isCachePositionKey`：位置键不参与指纹，使前后位移不致前缀漂移。
pub fn is_cache_position_key(key: &str) -> bool {
    matches!(
        key,
        "tool_index" | "system_index" | "message_index" | "block_index"
    )
}

/// Go `stripCachePositionKeys`。
pub fn strip_cache_position_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !is_cache_position_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Go `isAnthropicBillingHeaderBlock`：剔除易变的计费元数据块。
///
/// Claude Code 的 `x-anthropic-billing-header` 在其余完全相同的请求间会漂移 /
/// 出现 / 消失，且不改变模型语义，故整块从指纹链剔除。
pub fn is_anthropic_billing_header_block(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    // 仅归一化 text 块（或无显式 type 但含 text 的块）。
    if let Some(t) = map.get("type").and_then(|v| v.as_str()) {
        if !t.is_empty() && t != "text" {
            return false;
        }
    }
    let Some(text) = map.get("text").and_then(|v| v.as_str()) else {
        return false;
    };
    let trimmed = text.trim_start_matches([' ', '\t', '\r', '\n']);
    trimmed
        .to_ascii_lowercase()
        .starts_with("x-anthropic-billing-header:")
}

/// Go `parsePromptCacheTTLValue`，返回**毫秒**。
///
/// 用毫秒而非秒：Go 里 `500ms > 0` 会走到 normalize 并得到 5m；若先截断成整数秒
/// 会变成 0 → 不产生断点，与 Go 行为分叉。归一化后只会是 0/5m/1h，故毫秒精度
/// 仅在此处的 `> 0` 与 `> 5m` 比较中起作用。
pub fn parse_prompt_cache_ttl_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(s) => {
            let trimmed = s.trim().to_ascii_lowercase();
            if trimmed.is_empty() {
                return None;
            }
            if let Some(ms) = parse_go_duration_ms(&trimmed) {
                return Some(ms);
            }
            // Go 回退 strconv.Atoi → 秒
            trimmed.parse::<i64>().ok().map(|s| s * 1000)
        }
        Value::Number(n) => {
            let secs = n.as_f64()?;
            if secs > 0.0 {
                Some((secs * 1000.0) as i64)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `time.ParseDuration` 的够用子集（ns/us/ms/s/m/h），返回毫秒。
/// 无单位的纯数字返回 None，交由调用方按 Go 的 Atoi 回退当作秒处理。
fn parse_go_duration_ms(s: &str) -> Option<i64> {
    let (num, unit_ms) = if let Some(p) = s.strip_suffix("ns") {
        (p, 1e-6)
    } else if let Some(p) = s.strip_suffix("us").or_else(|| s.strip_suffix("µs")) {
        (p, 1e-3)
    } else if let Some(p) = s.strip_suffix("ms") {
        (p, 1.0)
    } else if let Some(p) = s.strip_suffix('s') {
        (p, 1000.0)
    } else if let Some(p) = s.strip_suffix('m') {
        (p, 60_000.0)
    } else if let Some(p) = s.strip_suffix('h') {
        (p, 3_600_000.0)
    } else {
        return None;
    };
    let n: f64 = num.trim().parse().ok()?;
    Some((n * unit_ms) as i64)
}

/// Go `extractPromptCacheTTL`，返回毫秒；0 表示该块不是断点。
///
/// 仅 `cache_control.type == "ephemeral"`（大小写不敏感）算有效；有 ttl 用 ttl，
/// 无 ttl 用默认 5m。
pub fn extract_prompt_cache_ttl(value: &Value) -> i64 {
    let Some(block) = value.as_object() else {
        return 0;
    };
    let Some(cc) = block.get("cache_control").and_then(|v| v.as_object()) else {
        return 0;
    };
    let cache_type = cc.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !cache_type.eq_ignore_ascii_case("ephemeral") {
        return 0;
    }
    parse_prompt_cache_ttl_value(cc.get("ttl")).unwrap_or(DEFAULT_PROMPT_CACHE_TTL_MS)
}

/// Go `normalizePromptCacheTTL`：一切非零 TTL 塌缩到 5m 或 1h。
/// 0→0；(0,5m]→5m；(5m,1h]→1h；>1h→1h。中间值有损，是刻意行为。
pub fn normalize_prompt_cache_ttl(ttl_ms: i64) -> i64 {
    if ttl_ms <= 0 {
        return 0;
    }
    if ttl_ms > MAX_PROMPT_CACHE_TTL_MS {
        return MAX_PROMPT_CACHE_TTL_MS;
    }
    if ttl_ms > DEFAULT_PROMPT_CACHE_TTL_MS {
        return MAX_PROMPT_CACHE_TTL_MS;
    }
    DEFAULT_PROMPT_CACHE_TTL_MS
}

/// Go `interfaceHasCacheControl`：递归判断值树里是否出现过 `cache_control`。
pub fn value_has_cache_control(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key("cache_control") || map.values().any(value_has_cache_control)
        }
        Value::Array(items) => items.iter().any(value_has_cache_control),
        _ => false,
    }
}

/// Go `minCacheableTokensForModel`：Opus 系列走单独阈值。
///
/// 注：Go 侧两个常量当前都是 1024，故这是个 no-op；保留为配置接缝。
pub fn min_cacheable_tokens_for_model(model: &str, default_min: i64, opus_min: i64) -> i64 {
    if model.to_ascii_lowercase().contains("opus") {
        opus_min
    } else {
        default_min
    }
}

// ============================================================================
// Profile 构建：把请求摊平成有序块，再切出断点
// ============================================================================

use super::types::MessagesRequest;

/// Go `cacheablePromptBlock`。
#[derive(Debug, Clone)]
struct CacheableBlock {
    /// 已剥离位置键的 wrapper，用于取指纹
    value: Value,
    /// 该块的估算 token（喂 canonical JSON，结构噪声计入）
    tokens: i64,
    /// 该块自身 cache_control 的 TTL（毫秒），0 表示无
    ttl_ms: i64,
    /// 是否是某条 message 的最后一块（隐式断点边界）
    is_message_end: bool,
}

/// Go `promptCacheBreakpoint`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCacheBreakpoint {
    /// 从头累积到该断点的 SHA-256 快照
    pub fingerprint: [u8; 32],
    /// 该前缀的累计估算 token
    pub cumulative_tokens: i64,
    /// 该断点的存活时长（毫秒）
    pub ttl_ms: i64,
}

/// Go `promptCacheProfile`。
#[derive(Debug, Clone)]
pub struct PromptCacheProfile {
    pub breakpoints: Vec<PromptCacheBreakpoint>,
    pub total_input_tokens: i64,
    pub model: String,
}

/// Go `detectMaxTTL`：取请求里出现过的最大 cache_control.ttl，无则默认 5m。
///
/// 该返回值给 `active_ttl` 播种，使**没有任何 cache_control 的请求也能在 message
/// 边界产生隐式断点** —— 这正是复现 Anthropic 自动前缀缓存的关键。`Tool` 的
/// cache_control 不参与（Go 侧 ClaudeTool 是无该字段的具体类型）。
pub fn detect_max_ttl(req: &MessagesRequest) -> i64 {
    let mut max = DEFAULT_PROMPT_CACHE_TTL_MS;
    if let Some(systems) = req.system.as_ref() {
        for sys in systems {
            let block = system_block_value(sys);
            max = max.max(normalize_prompt_cache_ttl(extract_prompt_cache_ttl(&block)));
        }
    }
    for msg in &req.messages {
        if let Value::Array(arr) = &msg.content {
            for block in arr {
                max = max.max(normalize_prompt_cache_ttl(extract_prompt_cache_ttl(block)));
            }
        }
    }
    max
}

/// 把 `SystemMessage` 还原成 Go 侧收到的原始客户端 JSON 形状。
///
/// `SystemMessage` 是具体类型且**没有 `type` 字段**，直接 `to_value` 会缺 `type`，
/// 与 Go 侧拿到的 `{"type":"text",...}` 不同形。这里显式补上，使 system 块的
/// 指纹口径与 Go 对齐。`cache_control` 需保留（TTL 探测要读），canonical 阶段
/// 才会把它跳过。
fn system_block_value(sys: &super::types::SystemMessage) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("type".to_string(), Value::String("text".to_string()));
    map.insert("text".to_string(), Value::String(sys.text.clone()));
    if let Some(cc) = sys.cache_control.as_ref() {
        if let Ok(v) = serde_json::to_value(cc) {
            map.insert("cache_control".to_string(), v);
        }
    }
    Value::Object(map)
}

/// Go `buildCachePreludeBlock`：model + tool_choice 进第一块。
///
/// 后果是**改 model 或 tool_choice 会让整条前缀链失效**（SHA-256 是累积的）。
fn build_prelude_block(req: &MessagesRequest) -> CacheableBlock {
    let mut map = serde_json::Map::new();
    map.insert(
        "kind".to_string(),
        Value::String("request_prelude".to_string()),
    );
    map.insert("model".to_string(), Value::String(req.model.clone()));
    map.insert(
        "tool_choice".to_string(),
        req.tool_choice.clone().unwrap_or(Value::Null),
    );
    let value = Value::Object(map);
    let tokens = estimate_approx_tokens(&canonicalize_cache_value(&value));
    CacheableBlock {
        value,
        tokens,
        ttl_ms: 0,
        is_message_end: false,
    }
}

/// Go `flattenClaudeCacheBlocks`：prelude → tools → system → messages。
fn flatten_cache_blocks(req: &MessagesRequest) -> Vec<CacheableBlock> {
    let mut blocks = vec![build_prelude_block(req)];

    if let Some(tools) = req.tools.as_ref() {
        for tool in tools {
            let mut map = serde_json::Map::new();
            map.insert("kind".to_string(), Value::String("tool".to_string()));
            map.insert("name".to_string(), Value::String(tool.name.clone()));
            map.insert(
                "description".to_string(),
                Value::String(tool.description.clone()),
            );
            map.insert(
                "input_schema".to_string(),
                serde_json::to_value(&tool.input_schema).unwrap_or(Value::Null),
            );
            let value = Value::Object(map);
            let tokens = estimate_approx_tokens(&canonicalize_cache_value(&value));
            // Go 从整个 tool 值里取 ttl；Rust 的 Tool 有具体 cache_control 字段。
            let ttl_ms = tool
                .cache_control
                .as_ref()
                .and_then(|cc| serde_json::to_value(cc).ok())
                .map(|cc| {
                    let wrapper = serde_json::json!({"cache_control": cc});
                    normalize_prompt_cache_ttl(extract_prompt_cache_ttl(&wrapper))
                })
                .unwrap_or(0);
            blocks.push(CacheableBlock {
                value,
                tokens,
                ttl_ms,
                is_message_end: false,
            });
        }
    }

    append_system_blocks(&mut blocks, req);

    for msg in &req.messages {
        append_message_blocks(&mut blocks, msg);
    }

    blocks
}

/// Go `appendSystemCacheBlocks` 的结构性跳过。
///
/// Claude Code 在 `system[0]` 注入一个**每轮变化**的动态块且故意不打
/// cache_control，其后才是稳定的大块。从该易变头部开始累积指纹会让整条前缀链
/// 每轮漂移、跨轮命中归零 —— 这是实测「只创建不命中」的根因。因此跳过所有
/// 「首个带 cache_control 之前」的 system 块；若无任何 cache_control 则全部纳入
/// （对齐 Go 的 `skipUntil` 保持 0）。
fn append_system_blocks(blocks: &mut Vec<CacheableBlock>, req: &MessagesRequest) {
    let Some(systems) = req.system.as_ref() else {
        return;
    };
    let skip_until = systems
        .iter()
        .position(|s| s.cache_control.is_some())
        .unwrap_or(0);
    for sys in systems.iter().skip(skip_until) {
        let block = system_block_value(sys);
        let mut map = serde_json::Map::new();
        map.insert("kind".to_string(), Value::String("system".to_string()));
        map.insert("block".to_string(), block.clone());
        push_block(blocks, Value::Object(map), &block, false);
    }
}

fn append_message_blocks(blocks: &mut Vec<CacheableBlock>, msg: &super::types::Message) {
    let make_wrapper = |block: &Value| {
        let mut map = serde_json::Map::new();
        map.insert("kind".to_string(), Value::String("message".to_string()));
        map.insert("role".to_string(), Value::String(msg.role.clone()));
        map.insert("block".to_string(), block.clone());
        Value::Object(map)
    };

    match &msg.content {
        Value::String(s) => {
            let block = serde_json::json!({"type": "text", "text": s});
            push_block(blocks, make_wrapper(&block), &block, true);
        }
        Value::Array(arr) => {
            let last = arr.len().saturating_sub(1);
            for (i, block) in arr.iter().enumerate() {
                push_block(blocks, make_wrapper(block), block, i == last);
            }
        }
        Value::Null => {}
        other => {
            push_block(blocks, make_wrapper(other), other, true);
        }
    }
}

/// Go `appendPromptBlock`：取块自身 TTL、剔除 billing header、剥位置键、算 token。
fn push_block(
    blocks: &mut Vec<CacheableBlock>,
    wrapper: Value,
    inner_block: &Value,
    is_message_end: bool,
) {
    // 易变的计费元数据不进指纹链（否则同一对话会因该块漂移而全部 miss）。
    if is_anthropic_billing_header_block(inner_block) {
        return;
    }
    let ttl_ms = normalize_prompt_cache_ttl(extract_prompt_cache_ttl(inner_block));
    let value = strip_cache_position_keys(&wrapper);
    let tokens = estimate_approx_tokens(&canonicalize_cache_value(&value));
    blocks.push(CacheableBlock {
        value,
        tokens,
        ttl_ms,
        is_message_end,
    });
}

/// Go `BuildClaudeProfile`。返回 `None` 表示本请求无断点、不参与缓存模拟。
///
/// `effective_ttl_ms` 是运营配置的 TTL 上限（`cacheEngineGo.ttlSeconds`），所有
/// 断点 TTL 向它收敛：调小即让历史前缀更早过期，从而产出更多 cache_creation。
pub fn build_claude_profile(
    req: &MessagesRequest,
    total_input_tokens: i64,
    effective_ttl_ms: i64,
) -> Option<PromptCacheProfile> {
    let blocks = flatten_cache_blocks(req);
    if blocks.is_empty() {
        return None;
    }

    let mut hasher = Sha256::new();
    let mut breakpoints: Vec<PromptCacheBreakpoint> = Vec::new();
    let mut cumulative: i64 = 0;
    // 自动前缀：用 detect_max_ttl 播种，使无显式 cache_control 时 message 边界
    // 仍成为隐式断点。
    let mut active_ttl = detect_max_ttl(req);

    for block in &blocks {
        write_hash_chunk(&mut hasher, &canonicalize_cache_value(&block.value));
        cumulative += block.tokens;

        // 断点判定：1) 本块有显式 cache_control；2) 已见过断点后，每个 message
        // 结束边界都成为隐式断点，使多轮对话能命中更早的前缀。
        let mut ttl_ms = 0;
        if block.ttl_ms > 0 {
            ttl_ms = block.ttl_ms;
            active_ttl = block.ttl_ms;
        } else if block.is_message_end && active_ttl > 0 {
            ttl_ms = active_ttl;
        }
        if ttl_ms <= 0 {
            continue;
        }

        // 向运营配置的 TTL 上限收敛。
        ttl_ms = ttl_ms.min(effective_ttl_ms);

        let digest = hasher.clone().finalize();
        let mut fingerprint = [0u8; 32];
        fingerprint.copy_from_slice(&digest);
        breakpoints.push(PromptCacheBreakpoint {
            fingerprint,
            cumulative_tokens: cumulative,
            ttl_ms,
        });
    }

    if breakpoints.is_empty() {
        return None;
    }

    Some(PromptCacheProfile {
        breakpoints,
        total_input_tokens: total_input_tokens.max(cumulative),
        model: req.model.clone(),
    })
}

// ============================================================================
// Tracker：两阶段 compute（只查）/ update（才写）
// ============================================================================

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

/// Go `promptCacheEntry`。
#[derive(Debug, Clone, Copy)]
struct PromptCacheEntry {
    expires_at_ms: i64,
    ttl_ms: i64,
    /// LRU 序号：插入 / 刷新时 bump，等价 Go `container/list` 的 MoveToFront。
    /// 淘汰时弹出最小序号，语义与侵入式链表一致但无需 Rc/unsafe。
    seq: u64,
}

#[derive(Default)]
struct TrackerInner {
    entries: HashMap<[u8; 32], PromptCacheEntry>,
    next_seq: u64,
    dirty: bool,
}

/// go 引擎的运行计数器快照。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoCacheStats {
    pub entries: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub expirations: u64,
}

/// `compute` 的产出（Go `promptCacheUsage` 去掉无消费方的 5m/1h 明细）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoCacheUsage {
    pub cache_creation: i64,
    pub cache_read: i64,
}

/// go 缓存模拟引擎的进程内状态。
///
/// **无会话隔离**：`entries` 是全局表（忠实移植 Go 的 `C1: cross-account sharing`），
/// 不同客户端 Key 的相同前缀会互相命中。这是与引擎 A 唯一的可观测行为差异。
pub struct GoCacheTracker {
    /// Cache state is partitioned by the actual Kiro credential.
    inner: Mutex<HashMap<u64, TrackerInner>>,
    persist_path: Option<std::path::PathBuf>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    expirations: AtomicU64,
    max_entries: AtomicUsize,
    max_ratio_millis: AtomicI64,
    ttl_ms: AtomicI64,
    min_cacheable_tokens: AtomicI64,
    opus_min_cacheable_tokens: AtomicI64,
    /// go 引擎专属的下发缩放倍率（不走全局膨胀倍率）。同样用千分比整数存进原子量。
    input_token_multiplier_millis: AtomicI64,
    cache_read_multiplier_millis: AtomicI64,
    cache_creation_multiplier_millis: AtomicI64,
    /// 可注入时钟（毫秒）。`None` = 用系统时间。仅测试使用：否则 TTL 与 LRU
    /// 相关行为只能靠 sleep 验证。
    clock_ms: Mutex<Option<i64>>,
}

impl GoCacheTracker {
    pub fn new(
        persist_path: Option<std::path::PathBuf>,
        config: crate::model::config::CacheEngineGoConfig,
    ) -> Self {
        let tracker = Self {
            inner: Mutex::new(HashMap::new()),
            persist_path,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            expirations: AtomicU64::new(0),
            max_entries: AtomicUsize::new(0),
            max_ratio_millis: AtomicI64::new(0),
            ttl_ms: AtomicI64::new(0),
            min_cacheable_tokens: AtomicI64::new(0),
            opus_min_cacheable_tokens: AtomicI64::new(0),
            input_token_multiplier_millis: AtomicI64::new(1000),
            cache_read_multiplier_millis: AtomicI64::new(1000),
            cache_creation_multiplier_millis: AtomicI64::new(1000),
            clock_ms: Mutex::new(None),
        };
        tracker.apply_config(config);
        tracker
    }

    /// 热更新参数（admin 改配置后调用，无需重启）。
    pub fn apply_config(&self, config: crate::model::config::CacheEngineGoConfig) {
        let c = config.sanitized();
        self.max_entries.store(c.max_entries, Ordering::Relaxed);
        // f64 存进原子量：用千分比整数，避免 AtomicF64（std 无）与 bit 转换的晦涩。
        self.max_ratio_millis
            .store((c.max_ratio * 1000.0).round() as i64, Ordering::Relaxed);
        self.ttl_ms.store(c.ttl_seconds * 1000, Ordering::Relaxed);
        self.min_cacheable_tokens
            .store(c.min_cacheable_tokens, Ordering::Relaxed);
        self.opus_min_cacheable_tokens
            .store(c.opus_min_cacheable_tokens, Ordering::Relaxed);
        self.input_token_multiplier_millis.store(
            (c.input_token_multiplier * 1000.0).round() as i64,
            Ordering::Relaxed,
        );
        self.cache_read_multiplier_millis.store(
            (c.cache_read_multiplier * 1000.0).round() as i64,
            Ordering::Relaxed,
        );
        self.cache_creation_multiplier_millis.store(
            (c.cache_creation_multiplier * 1000.0).round() as i64,
            Ordering::Relaxed,
        );
    }

/// go 引擎专属倍率 `(input, cache_read, cache_creation)`。
    ///
    /// 这三个值**取代**全局膨胀倍率作用在 go 路径上：两套引擎各用自己的一组，
    /// 使各自下发口径可独立调参。`cache_creation` 默认 1.0（= Go 原实现只缩放
    /// input 与 cache_read），可按需调离。
    pub fn multipliers(&self) -> (f64, f64, f64) {
        (
            self.input_token_multiplier_millis.load(Ordering::Relaxed) as f64 / 1000.0,
            self.cache_read_multiplier_millis.load(Ordering::Relaxed) as f64 / 1000.0,
            self.cache_creation_multiplier_millis.load(Ordering::Relaxed) as f64 / 1000.0,
        )
    }

    /// 当前配置的断点 TTL 上限（毫秒），供 `build_claude_profile` 使用。
    pub fn effective_ttl_ms(&self) -> i64 {
        self.ttl_ms.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> GoCacheStats {
        let inner = self.inner.lock();
        GoCacheStats {
            entries: inner.values().map(|state| state.entries.len()).sum(),
            capacity: self.max_entries.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            expirations: self.expirations.load(Ordering::Relaxed),
        }
    }

    fn now_ms(&self) -> i64 {
        if let Some(fixed) = *self.clock_ms.lock() {
            return fixed;
        }
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn set_clock_ms(&self, ms: i64) {
        *self.clock_ms.lock() = Some(ms);
    }

    #[cfg(test)]
    fn advance_clock_ms(&self, delta: i64) {
        let mut guard = self.clock_ms.lock();
        let base = guard.unwrap_or(0);
        *guard = Some(base + delta);
    }

    fn min_tokens_for(&self, model: &str) -> i64 {
        min_cacheable_tokens_for_model(
            model,
            self.min_cacheable_tokens.load(Ordering::Relaxed),
            self.opus_min_cacheable_tokens.load(Ordering::Relaxed),
        )
    }

    /// Go `Compute`：**只查不写**。写入由 [`GoCacheTracker::update`] 在请求成功后完成。
    ///
    /// 两阶段是 `scan_start = len-2` 能成立的前提：本轮最深断点此刻尚未入表，故
    /// 它覆盖的「本轮新内容」必然落进 cache_creation。若改成一次性先写后查，首轮
    /// 就会因刚写入的 `len-2` 断点而报出 cache_read —— 那是伪造读数。
    pub fn compute(&self, profile: &PromptCacheProfile) -> GoCacheUsage {
        self.compute_for_account(0, profile)
    }

    /// Compute against the namespace of the actual Kiro credential.
    pub fn compute_for_account(
        &self,
        account_id: u64,
        profile: &PromptCacheProfile,
    ) -> GoCacheUsage {
        if profile.breakpoints.is_empty() {
            return GoCacheUsage::default();
        }
        let min_tokens = self.min_tokens_for(&profile.model);
        let now = self.now_ms();

        let last = profile.breakpoints.last().unwrap();
        let mut last_tokens = last.cumulative_tokens.min(profile.total_input_tokens);

        let mut all = self.inner.lock();
        let inner = all.entry(account_id).or_default();
        self.prune_expired_locked(inner, now);

        // 首个请求（表空）：只可能是 creation，且**不套 0.85 封顶**（对齐 Go）。
        if inner.entries.is_empty() {
            drop(inner);
            self.misses.fetch_add(1, Ordering::Relaxed);
            let creation = if last_tokens < min_tokens { 0 } else { last_tokens };
            return GoCacheUsage {
                cache_creation: creation,
                cache_read: 0,
            };
        }

        // 可缓存量封顶到总 input 的一定比例，保证总有未缓存尾部：本轮最新内容
        // 不可能全部由缓存供给。
        let ratio = self.max_ratio_millis.load(Ordering::Relaxed) as f64 / 1000.0;
        let max_cacheable = (profile.total_input_tokens as f64 * ratio) as i64;
        if last_tokens > max_cacheable {
            last_tokens = max_cacheable;
        }

        // 最深断点覆盖本轮新内容，Anthropic 恒计 creation，故从 len-2 起扫。
        let scan_start = if profile.breakpoints.len() >= 2 {
            profile.breakpoints.len() - 2
        } else {
            profile.breakpoints.len() - 1
        };

        let mut matched = 0i64;
        for i in (0..=scan_start).rev() {
            let bp = &profile.breakpoints[i];
            if bp.cumulative_tokens < min_tokens {
                continue;
            }
            let Some(entry) = inner.entries.get_mut(&bp.fingerprint) else {
                continue;
            };
            if entry.expires_at_ms < now {
                continue;
            }
            // 命中即续期并刷新 LRU 序号。
            entry.expires_at_ms = now + entry.ttl_ms;
            inner.next_seq += 1;
            let seq = inner.next_seq;
            if let Some(e) = inner.entries.get_mut(&bp.fingerprint) {
                e.seq = seq;
            }
            inner.dirty = true; // 命中延长了 TTL，需落盘
            matched = bp.cumulative_tokens.min(profile.total_input_tokens).min(last_tokens);
            break;
        }
        drop(inner);

        if matched > 0 {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }

        GoCacheUsage {
            cache_creation: (last_tokens - matched).max(0),
            cache_read: matched,
        }
    }

    /// Go `Update`：把本轮断点写入表。**只应在请求成功后调用**，使失败请求不污染缓存。
    pub fn update(&self, profile: &PromptCacheProfile) {
        self.update_for_account(0, profile)
    }

    /// Update the namespace of the actual Kiro credential after success.
    pub fn update_for_account(&self, account_id: u64, profile: &PromptCacheProfile) {
        if profile.breakpoints.is_empty() {
            return;
        }
        let min_tokens = self.min_tokens_for(&profile.model);
        let now = self.now_ms();

        let mut all = self.inner.lock();
        let inner = all.entry(account_id).or_default();
        self.prune_expired_locked(inner, now);

        for bp in &profile.breakpoints {
            if bp.cumulative_tokens < min_tokens {
                continue;
            }
            self.put_locked(inner, bp.fingerprint, now + bp.ttl_ms, bp.ttl_ms);
        }
        inner.dirty = true;
        self.evict_overflow_locked(inner);
    }

    fn put_locked(
        &self,
        inner: &mut TrackerInner,
        fp: [u8; 32],
        expires_at_ms: i64,
        ttl_ms: i64,
    ) {
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner
            .entries
            .entry(fp)
            .and_modify(|e| {
                e.expires_at_ms = expires_at_ms;
                e.ttl_ms = ttl_ms;
                e.seq = seq;
            })
            .or_insert(PromptCacheEntry {
                expires_at_ms,
                ttl_ms,
                seq,
            });
    }

    fn prune_expired_locked(&self, inner: &mut TrackerInner, now: i64) {
        let before = inner.entries.len();
        inner.entries.retain(|_, e| e.expires_at_ms > now);
        let removed = before - inner.entries.len();
        if removed > 0 {
            inner.dirty = true;
            self.expirations
                .fetch_add(removed as u64, Ordering::Relaxed);
        }
    }

    /// 按 LRU 序号淘汰到容量以内。
    fn evict_overflow_locked(&self, inner: &mut TrackerInner) {
        let cap = self.max_entries.load(Ordering::Relaxed);
        if inner.entries.len() <= cap {
            return;
        }
        let drop_n = inner.entries.len() - cap;
        let mut victims: Vec<([u8; 32], u64)> =
            inner.entries.iter().map(|(k, v)| (*k, v.seq)).collect();
        victims.sort_by_key(|x| x.1);
        for (fp, _) in victims.into_iter().take(drop_n) {
            inner.entries.remove(&fp);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ---- 持久化 ----

    /// 从 `persist_path` 载入未过期条目。缺文件 / 损坏均视为空表（首次启动的常态）。
    ///
    /// 载入后必须做一次容量裁剪：`put_locked` 不管容量，而 `compute` 只按 TTL 清理，
    /// 若不裁剪，一个超容量状态文件会一直超标到下次 `update`。
    pub fn load(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(disk) = serde_json::from_slice::<GoCacheDisk>(&bytes) else {
            tracing::warn!("GoCacheTracker 状态文件损坏，按空表启动: {}", path.display());
            return;
        };
        // Version 1 had one global namespace and cannot be safely attributed to
        // credentials. Drop it rather than allowing a legacy cross-account hit.
        if disk.version < 2 {
            tracing::info!(
                "忽略旧版 GoCacheTracker 状态文件（缺少账号隔离）: {}",
                path.display()
            );
            return;
        }

        let now = self.now_ms();
        let mut all = self.inner.lock();
        let mut loaded = 0usize;
        for e in disk.entries {
            if e.expires_at_ms <= now {
                continue;
            }
            let Some(fp) = decode_fingerprint(&e.fingerprint) else {
                continue;
            };
            let inner = all.entry(e.account_id).or_default();
            self.put_locked(inner, fp, e.expires_at_ms, e.ttl_ms);
            loaded += 1;
        }
        for inner in all.values_mut() {
            self.evict_overflow_locked(inner);
            inner.dirty = false;
        }
        if loaded > 0 {
            tracing::info!(
                "GoCacheTracker 重建：从 {} 加载 {} 条有效记录",
                path.display(),
                all.values().map(|state| state.entries.len()).sum::<usize>()
            );
        }
    }

    /// 落盘（仅 dirty 时实际写）。原子写，避免崩溃留下半截文件导致缓存被静默重置。
    pub fn flush_to_disk(&self) {
        let Some(path) = self.persist_path.clone() else {
            return;
        };
        let now = self.now_ms();
        let snapshot = {
            let mut all = self.inner.lock();
            if !all.values().any(|state| state.dirty) {
                return;
            }
            let mut snapshot = Vec::new();
            for (account_id, inner) in all.iter_mut() {
                inner.dirty = false;
                snapshot.extend(inner.entries.iter()
                    .filter(|(_, e)| e.expires_at_ms > now)
                    .map(|(fp, e)| GoCacheDiskEntry {
                        account_id: *account_id,
                        fingerprint: encode_fingerprint(fp),
                        expires_at_ms: e.expires_at_ms,
                        ttl_ms: e.ttl_ms,
                    }));
            }
            snapshot
        };

        let disk = GoCacheDisk {
            version: 2,
            entries: snapshot,
        };
        let json = match serde_json::to_vec(&disk) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("GoCacheTracker 序列化失败: {}", e);
                return;
            }
        };
        if let Err(e) = crate::common::fs::write_file_atomic(&path, &json) {
            tracing::warn!("GoCacheTracker 落盘失败 {}: {}", path.display(), e);
        }
    }

    /// 清理过期条目（对外，供后台任务调用）。
    pub fn prune_expired(&self) {
        let now = self.now_ms();
        let mut all = self.inner.lock();
        for inner in all.values_mut() {
            self.prune_expired_locked(inner, now);
        }
    }

    /// 启动后台周期任务：每分钟清过期 + 落盘。与引擎 A 各自独立一个任务，
    /// 避免改动引擎 A 已有的 `spawn_background`。
    pub fn spawn_background(self: std::sync::Arc<Self>) {
        let weak = std::sync::Arc::downgrade(&self);
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(60);
            loop {
                tokio::time::sleep(interval).await;
                let Some(tracker) = weak.upgrade() else { return };
                tracker.prune_expired();
                tracker.flush_to_disk();
            }
        });
    }
}

/// 磁盘格式。指纹存 hex：Go 侧 `[32]byte` 会 marshal 成数字数组，但两个项目永不
/// 共享该文件，hex 更紧凑可读。带 `version` 便于将来演进。
#[derive(serde::Serialize, serde::Deserialize)]
struct GoCacheDisk {
    version: u32,
    entries: Vec<GoCacheDiskEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoCacheDiskEntry {
    #[serde(default)]
    account_id: u64,
    fingerprint: String,
    expires_at_ms: i64,
    ttl_ms: i64,
}

fn encode_fingerprint(fp: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in fp {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn decode_fingerprint(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn estimate_empty_and_short_strings() {
        assert_eq!(estimate_approx_tokens(""), 0);
        // 短串分支 max(1, ceil(n/3))
        assert_eq!(estimate_approx_tokens("a"), 1);
        assert_eq!(estimate_approx_tokens("ab"), 1);
        assert_eq!(estimate_approx_tokens("abc"), 1);
        assert_eq!(estimate_approx_tokens("abcd"), 2);
    }

    #[test]
    fn estimate_buckets_in_isolation() {
        assert_eq!(estimate_approx_tokens("abcdefgh"), 2, "8 字母 → ceil(8/4.5)");
        assert_eq!(estimate_approx_tokens("12345678"), 4, "8 数字 → ceil(8/2)");
        assert_eq!(estimate_approx_tokens("!!!!!!!!"), 6, "8 符号 → ceil(8/1.5)");
        assert_eq!(
            estimate_approx_tokens("中文测试中文测试"),
            6,
            "8 CJK → ceil(8/1.5)"
        );
    }

    #[test]
    fn estimate_whitespace_counts_as_regular() {
        // 空白靠 fallthrough 进 regular，不是 symbols：
        // 8 个空白 → ceil(8/4.5) = 2；若误判成 symbols 会是 6。
        assert_eq!(estimate_approx_tokens(" \t\n \t\n \t"), 2);
    }

    #[test]
    fn estimate_mixed_matches_formula() {
        // "ab12!!中中" → regular=2, digits=2, symbols=2, nonASCII=2
        // 2/4.5 + 2/2 + 2/1.5 + 2/1.5 = 0.4444+1+1.3333+1.3333 = 4.111 → 5
        assert_eq!(estimate_approx_tokens("ab12!!中中"), 5);
    }

    #[test]
    fn request_input_estimate_uses_content_not_canonical_wrappers() {
        let request = MessagesRequest {
            model: "claude-sonnet-4-5".to_string(),
            max_tokens: 128,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([{"type": "text", "text": "hello world"}]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        assert_eq!(
            estimate_claude_request_input_tokens(&request),
            estimate_approx_tokens("hello world")
        );
        let canonical_total = build_claude_profile(&request, 0, DEFAULT_PROMPT_CACHE_TTL_MS)
            .unwrap()
            .total_input_tokens;
        assert!(
            canonical_total > estimate_claude_request_input_tokens(&request),
            "profile total should include wrapper tokens only when no Go-compatible denominator is supplied"
        );
    }

    /// 钉住「serde_json 未开 preserve_order」这一前提。若被开启，本测试失败，
    /// 提醒必须改为显式排序，否则指纹会静默变化。
    #[test]
    fn canonical_keys_are_sorted() {
        let v = json!({"zeta": 1, "alpha": 2, "mid": 3});
        assert_eq!(
            canonicalize_cache_value(&v),
            r#"{"alpha":2,"mid":3,"zeta":1}"#
        );
    }

    /// 对齐 Go `json.Marshal` 的 HTML 转义（`<` `>` `&` → `<` `>` `&`）。
    ///
    /// 期望值取自 kiro-go 侧 `json.Marshal` 的**实测输出**，非推测。含这三个字符的
    /// 内容（HTML / 代码 / XML）此前会算出与 Go 不同的 canonical JSON，导致
    /// 字节长度和 token 估算偏移。非 ASCII 两侧都不转义。
    #[test]
    fn canonical_matches_go_html_escaping() {
        let cases = [
            ("a<b>c&d", "\"a\\u003cb\\u003ec\\u0026d\""),
            (
                "<div class=\"x\">Hello & bye</div>",
                "\"\\u003cdiv class=\\\"x\\\"\\u003eHello \\u0026 bye\\u003c/div\\u003e\"",
            ),
            (
                "if a < b && c > d { }",
                "\"if a \\u003c b \\u0026\\u0026 c \\u003e d { }\"",
            ),
            ("plain ascii", "\"plain ascii\""),
            // Go 不转义非 ASCII，与 serde_json 一致
            ("中文 <tag>", "\"中文 \\u003ctag\\u003e\""),
            // 其余转义沿用 serde_json，两侧本就一致
            (
                "quote \" and backslash \\",
                "\"quote \\\" and backslash \\\\\"",
            ),
            ("newline\nhere", "\"newline\\nhere\""),
        ];
        for (input, expected) in cases {
            assert_eq!(
                canonicalize_cache_value(&Value::String(input.to_string())),
                expected,
                "input={input:?}"
            );
        }
    }

    /// 对象 key 同样要转义——Go 对 key 也走 `json.Marshal`。
    #[test]
    fn canonical_escapes_object_keys_too() {
        let v = json!({"a<b": 1});
        assert_eq!(canonicalize_cache_value(&v), "{\"a\\u003cb\":1}");
    }

    /// 转义改变了指纹：含 `<>&` 的内容与不含的必须哈希成不同值，且转义后的
    /// 字节长度进入 `write_hash_chunk` 的长度前缀，故两者不可能碰撞。
    #[test]
    fn html_escaping_affects_fingerprint_length_prefix() {
        let with = canonicalize_cache_value(&Value::String("a<b".to_string()));
        let without = canonicalize_cache_value(&Value::String("aXb".to_string()));
        assert_ne!(with, without);
        assert!(with.len() > without.len(), "转义后应更长: {with}");
    }

    #[test]
    fn canonical_skips_cache_control_at_every_level() {
        let v = json!({
            "type": "text",
            "cache_control": {"type": "ephemeral"},
            "nested": {"cache_control": {"ttl": "1h"}, "keep": true},
            "arr": [{"cache_control": {}, "x": 1}]
        });
        let s = canonicalize_cache_value(&v);
        assert!(!s.contains("cache_control"), "got {s}");
        assert!(s.contains("\"keep\":true"));
        assert!(s.contains("\"x\":1"));
    }

    #[test]
    fn canonical_is_compact_and_handles_scalars() {
        let v = json!({"a": null, "b": false, "c": [1, 2, [3]], "d": "x"});
        assert_eq!(
            canonicalize_cache_value(&v),
            r#"{"a":null,"b":false,"c":[1,2,[3]],"d":"x"}"#
        );
    }

    #[test]
    fn hash_chunk_framing_prevents_concatenation_collision() {
        // ("ab","c") 与 ("a","bc") 拼接后字节流相同，长度前缀必须让摘要不同。
        let mut h1 = Sha256::new();
        write_hash_chunk(&mut h1, "ab");
        write_hash_chunk(&mut h1, "c");
        let mut h2 = Sha256::new();
        write_hash_chunk(&mut h2, "a");
        write_hash_chunk(&mut h2, "bc");
        assert_ne!(h1.finalize(), h2.finalize());
    }

    #[test]
    fn hash_chunk_uses_byte_length_not_char_count() {
        // "中" 是 3 字节 / 1 字符。前缀必须是 "3"，与估算器的 rune 口径不同。
        let mut actual = Sha256::new();
        write_hash_chunk(&mut actual, "中");
        let mut expected = Sha256::new();
        expected.update(b"3");
        expected.update([0u8]);
        expected.update("中".as_bytes());
        expected.update([0u8]);
        assert_eq!(actual.finalize(), expected.finalize());
    }

    #[test]
    fn strips_exactly_the_four_position_keys() {
        let v = json!({
            "kind": "message", "role": "user",
            "tool_index": 0, "system_index": 1,
            "message_index": 2, "block_index": 3,
            "other_index": 4
        });
        let stripped = strip_cache_position_keys(&v);
        let obj = stripped.as_object().unwrap();
        for k in ["tool_index", "system_index", "message_index", "block_index"] {
            assert!(!obj.contains_key(k), "{k} 应被剔除");
        }
        assert!(obj.contains_key("other_index"), "只剔除那四个键");
        assert!(obj.contains_key("kind") && obj.contains_key("role"));
    }

    #[test]
    fn billing_header_detection() {
        assert!(is_anthropic_billing_header_block(
            &json!({"type": "text", "text": "x-anthropic-billing-header: abc"})
        ));
        assert!(
            is_anthropic_billing_header_block(
                &json!({"type": "text", "text": "  \n X-Anthropic-Billing-Header: v"})
            ),
            "去前导空白 + 大小写不敏感"
        );
        assert!(
            is_anthropic_billing_header_block(&json!({"text": "x-anthropic-billing-header: v"})),
            "无显式 type 但含 text"
        );
        assert!(
            !is_anthropic_billing_header_block(
                &json!({"type": "image", "text": "x-anthropic-billing-header: v"})
            ),
            "非 text 类型不算"
        );
        assert!(!is_anthropic_billing_header_block(
            &json!({"type": "text", "text": "hello"})
        ));
        assert!(!is_anthropic_billing_header_block(&json!("bare string")));
    }

    #[test]
    fn normalize_ttl_collapses_to_5m_or_1h() {
        assert_eq!(normalize_prompt_cache_ttl(0), 0);
        assert_eq!(normalize_prompt_cache_ttl(-1), 0);
        assert_eq!(normalize_prompt_cache_ttl(1), DEFAULT_PROMPT_CACHE_TTL_MS);
        assert_eq!(
            normalize_prompt_cache_ttl(DEFAULT_PROMPT_CACHE_TTL_MS),
            DEFAULT_PROMPT_CACHE_TTL_MS
        );
        assert_eq!(
            normalize_prompt_cache_ttl(DEFAULT_PROMPT_CACHE_TTL_MS + 1),
            MAX_PROMPT_CACHE_TTL_MS
        );
        assert_eq!(
            normalize_prompt_cache_ttl(MAX_PROMPT_CACHE_TTL_MS),
            MAX_PROMPT_CACHE_TTL_MS
        );
        assert_eq!(
            normalize_prompt_cache_ttl(2 * MAX_PROMPT_CACHE_TTL_MS),
            MAX_PROMPT_CACHE_TTL_MS
        );
    }

    #[test]
    fn parse_ttl_values() {
        assert_eq!(
            parse_prompt_cache_ttl_value(Some(&json!("5m"))),
            Some(300_000)
        );
        assert_eq!(
            parse_prompt_cache_ttl_value(Some(&json!("1h"))),
            Some(3_600_000)
        );
        assert_eq!(
            parse_prompt_cache_ttl_value(Some(&json!("300"))),
            Some(300_000),
            "无单位纯数字走 Atoi 回退，按秒解释"
        );
        assert_eq!(
            parse_prompt_cache_ttl_value(Some(&json!(300))),
            Some(300_000)
        );
        // 亚秒必须保留：截断成整数秒会变 0 → 不产生断点，与 Go 分叉。
        assert_eq!(parse_prompt_cache_ttl_value(Some(&json!("500ms"))), Some(500));
        assert_eq!(
            normalize_prompt_cache_ttl(
                parse_prompt_cache_ttl_value(Some(&json!("500ms"))).unwrap()
            ),
            DEFAULT_PROMPT_CACHE_TTL_MS
        );
        assert_eq!(parse_prompt_cache_ttl_value(Some(&json!("garbage"))), None);
        assert_eq!(parse_prompt_cache_ttl_value(Some(&json!(""))), None);
        assert_eq!(parse_prompt_cache_ttl_value(None), None);
    }

    #[test]
    fn extract_ttl_requires_ephemeral() {
        assert_eq!(
            extract_prompt_cache_ttl(&json!({"cache_control": {"type": "ephemeral"}})),
            DEFAULT_PROMPT_CACHE_TTL_MS,
            "无 ttl 时用默认 5m"
        );
        assert_eq!(
            extract_prompt_cache_ttl(
                &json!({"cache_control": {"type": "Ephemeral", "ttl": "1h"}})
            ),
            MAX_PROMPT_CACHE_TTL_MS,
            "type 大小写不敏感"
        );
        assert_eq!(
            extract_prompt_cache_ttl(&json!({"cache_control": {"type": "persistent"}})),
            0
        );
        assert_eq!(extract_prompt_cache_ttl(&json!({"type": "text"})), 0);
        assert_eq!(extract_prompt_cache_ttl(&json!("bare")), 0);
    }

    #[test]
    fn value_has_cache_control_is_recursive() {
        assert!(value_has_cache_control(&json!({"cache_control": {}})));
        assert!(value_has_cache_control(
            &json!({"a": {"b": [{"cache_control": {}}]}})
        ));
        assert!(!value_has_cache_control(&json!({"a": {"b": [1, 2, "x"]}})));
    }

    // ---- profile 构建 ----

    use super::super::types::{CacheControl, Message, SystemMessage};

    fn req(messages: Vec<Message>) -> MessagesRequest {
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

    fn text_msg(role: &str, text: &str) -> Message {
        Message {
            role: role.to_string(),
            content: json!([{"type": "text", "text": text}]),
        }
    }

    fn ephemeral() -> CacheControl {
        CacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: None,
        }
    }

    const TTL_CAP: i64 = DEFAULT_PROMPT_CACHE_TTL_MS;

    #[test]
    fn message_end_breakpoints_fire_without_any_cache_control() {
        // detect_max_ttl 给 active_ttl 播种 5m，故无 cache_control 也有隐式断点。
        // 这是复现 Anthropic 自动前缀缓存的关键。
        let body = "lorem ipsum dolor sit amet ".repeat(20);
        let r = req(vec![
            text_msg("user", &body),
            text_msg("assistant", &body),
            text_msg("user", &body),
        ]);
        let p = build_claude_profile(&r, 0, TTL_CAP).expect("应产生断点");
        assert_eq!(p.breakpoints.len(), 3, "每条 message 末块一个断点");
        // 累计单调递增
        assert!(p.breakpoints[0].cumulative_tokens < p.breakpoints[1].cumulative_tokens);
        assert!(p.breakpoints[1].cumulative_tokens < p.breakpoints[2].cumulative_tokens);
    }

    #[test]
    fn total_input_tokens_floors_at_cumulative() {
        let r = req(vec![text_msg("user", &"x ".repeat(200))]);
        let p = build_claude_profile(&r, 1, TTL_CAP).unwrap();
        let deepest = p.breakpoints.last().unwrap().cumulative_tokens;
        assert!(
            p.total_input_tokens >= deepest,
            "传入 total 小于累计时应被抬到累计值"
        );
    }

    #[test]
    fn prelude_model_change_invalidates_whole_chain() {
        // model 在 prelude 块里，SHA-256 累积 → 改 model 后所有断点指纹都变。
        let body = "stable conversation body ".repeat(20);
        let msgs = || vec![text_msg("user", &body), text_msg("assistant", &body)];

        let a = build_claude_profile(&req(msgs()), 0, TTL_CAP).unwrap();
        let mut r2 = req(msgs());
        r2.model = "claude-opus-4-8".to_string();
        let b = build_claude_profile(&r2, 0, TTL_CAP).unwrap();

        assert_eq!(a.breakpoints.len(), b.breakpoints.len());
        for (x, y) in a.breakpoints.iter().zip(b.breakpoints.iter()) {
            assert_ne!(x.fingerprint, y.fingerprint, "改 model 应让每个断点都失效");
        }
    }

    #[test]
    fn tool_choice_participates_in_prelude() {
        let body = "body ".repeat(20);
        let a = build_claude_profile(&req(vec![text_msg("user", &body)]), 0, TTL_CAP).unwrap();
        let mut r2 = req(vec![text_msg("user", &body)]);
        r2.tool_choice = Some(json!({"type": "any"}));
        let b = build_claude_profile(&r2, 0, TTL_CAP).unwrap();
        assert_ne!(a.breakpoints[0].fingerprint, b.breakpoints[0].fingerprint);
    }

    #[test]
    fn identical_requests_produce_identical_fingerprints() {
        // 自一致性：同一前缀必须恒哈希成同一值，否则跨轮永不命中。
        let body = "deterministic body ".repeat(20);
        let msgs = || vec![text_msg("user", &body), text_msg("assistant", &body)];
        let a = build_claude_profile(&req(msgs()), 0, TTL_CAP).unwrap();
        let b = build_claude_profile(&req(msgs()), 0, TTL_CAP).unwrap();
        assert_eq!(
            a.breakpoints.iter().map(|b| b.fingerprint).collect::<Vec<_>>(),
            b.breakpoints.iter().map(|b| b.fingerprint).collect::<Vec<_>>()
        );
    }

    #[test]
    fn growing_conversation_preserves_earlier_fingerprints() {
        // 跨轮命中的前提：历史前缀逐字节不变 → 前几个断点指纹必须与上一轮相同。
        let body = "turn body ".repeat(20);
        let turn1 = build_claude_profile(
            &req(vec![text_msg("user", &body), text_msg("assistant", &body)]),
            0,
            TTL_CAP,
        )
        .unwrap();
        let turn2 = build_claude_profile(
            &req(vec![
                text_msg("user", &body),
                text_msg("assistant", &body),
                text_msg("user", &body),
            ]),
            0,
            TTL_CAP,
        )
        .unwrap();

        assert_eq!(turn1.breakpoints.len(), 2);
        assert_eq!(turn2.breakpoints.len(), 3);
        for i in 0..2 {
            assert_eq!(
                turn1.breakpoints[i].fingerprint, turn2.breakpoints[i].fingerprint,
                "第 {i} 个历史断点应跨轮稳定"
            );
        }
    }

    #[test]
    fn dynamic_system_head_is_skipped() {
        // system[0] 每轮变化且无 cache_control → 必须跳过，否则整链漂移。
        let stable = "You are a coding assistant. ".repeat(50);
        let body = "conversation ".repeat(20);
        let make = |dyn_head: &str| {
            let mut r = req(vec![text_msg("user", &body), text_msg("assistant", &body)]);
            r.system = Some(vec![
                SystemMessage {
                    text: dyn_head.to_string(),
                    cache_control: None,
                },
                SystemMessage {
                    text: stable.clone(),
                    cache_control: Some(ephemeral()),
                },
            ]);
            r
        };
        let a = build_claude_profile(&make("now=1001"), 0, TTL_CAP).unwrap();
        let b = build_claude_profile(&make("now=2002"), 0, TTL_CAP).unwrap();
        assert_eq!(
            a.breakpoints.iter().map(|x| x.fingerprint).collect::<Vec<_>>(),
            b.breakpoints.iter().map(|x| x.fingerprint).collect::<Vec<_>>(),
            "动态 system 头变化不应改变任何断点指纹"
        );
    }

    #[test]
    fn system_without_cache_control_is_fully_included() {
        // 无任何 cache_control 时 skipUntil 保持 0 → 全部纳入。
        let body = "body ".repeat(20);
        let mut with_sys = req(vec![text_msg("user", &body)]);
        with_sys.system = Some(vec![SystemMessage {
            text: "some system text that is reasonably long ".repeat(10),
            cache_control: None,
        }]);
        let without = build_claude_profile(&req(vec![text_msg("user", &body)]), 0, TTL_CAP).unwrap();
        let with = build_claude_profile(&with_sys, 0, TTL_CAP).unwrap();
        assert!(
            with.breakpoints[0].cumulative_tokens > without.breakpoints[0].cumulative_tokens,
            "无 cache_control 的 system 仍应计入累计 token"
        );
    }

    #[test]
    fn billing_header_block_is_dropped_from_chain() {
        let body = "body ".repeat(20);
        let clean = req(vec![text_msg("user", &body)]);
        let mut dirty = req(vec![Message {
            role: "user".to_string(),
            content: json!([
                {"type": "text", "text": "x-anthropic-billing-header: drifting-value"},
                {"type": "text", "text": body}
            ]),
        }]);
        // 同一对话、billing header 值不同 → 指纹必须一致（该块被整体剔除）。
        let a = build_claude_profile(&dirty, 0, TTL_CAP).unwrap();
        dirty.messages[0].content = json!([
            {"type": "text", "text": "x-anthropic-billing-header: other-value"},
            {"type": "text", "text": body}
        ]);
        let b = build_claude_profile(&dirty, 0, TTL_CAP).unwrap();
        assert_eq!(a.breakpoints.last().unwrap().fingerprint, b.breakpoints.last().unwrap().fingerprint);
        // 且与完全没有该块的请求同指纹。
        let c = build_claude_profile(&clean, 0, TTL_CAP).unwrap();
        assert_eq!(
            a.breakpoints.last().unwrap().fingerprint,
            c.breakpoints.last().unwrap().fingerprint,
            "剔除 billing header 后应与无该块的请求等价"
        );
    }

    #[test]
    fn explicit_cache_control_ttl_wins_and_converges_to_cap() {
        let body = "body ".repeat(20);
        let mut r = req(vec![Message {
            role: "user".to_string(),
            content: json!([{
                "type": "text", "text": body,
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]),
        }]);
        r.system = None;
        // TTL 上限设为 1h 时，断点取 1h。
        let p = build_claude_profile(&r, 0, MAX_PROMPT_CACHE_TTL_MS).unwrap();
        assert_eq!(p.breakpoints.last().unwrap().ttl_ms, MAX_PROMPT_CACHE_TTL_MS);
        // 上限压到 5m 时，1h 被收敛下来。
        let p2 = build_claude_profile(&r, 0, DEFAULT_PROMPT_CACHE_TTL_MS).unwrap();
        assert_eq!(
            p2.breakpoints.last().unwrap().ttl_ms,
            DEFAULT_PROMPT_CACHE_TTL_MS,
            "断点 TTL 必须向运营上限收敛"
        );
    }

    #[test]
    fn tools_participate_in_chain() {
        use super::super::types::Tool;
        let body = "body ".repeat(20);
        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), json!("object"));
        let mut r = req(vec![text_msg("user", &body)]);
        r.tools = Some(vec![Tool {
            tool_type: None,
            name: "bash".to_string(),
            description: "run a command".to_string(),
            input_schema: schema,
            max_uses: None,
            cache_control: None,
        }]);
        let with = build_claude_profile(&r, 0, TTL_CAP).unwrap();
        let without = build_claude_profile(&req(vec![text_msg("user", &body)]), 0, TTL_CAP).unwrap();
        assert_ne!(
            with.breakpoints[0].fingerprint, without.breakpoints[0].fingerprint,
            "tools 应改变后续断点指纹"
        );
        assert!(with.breakpoints[0].cumulative_tokens > without.breakpoints[0].cumulative_tokens);
    }

    #[test]
    fn string_content_is_treated_as_text_block() {
        let body = "plain string body ".repeat(20);
        let as_string = req(vec![Message {
            role: "user".to_string(),
            content: Value::String(body.clone()),
        }]);
        let as_array = req(vec![text_msg("user", &body)]);
        assert_eq!(
            build_claude_profile(&as_string, 0, TTL_CAP)
                .unwrap()
                .breakpoints
                .last()
                .unwrap()
                .fingerprint,
            build_claude_profile(&as_array, 0, TTL_CAP)
                .unwrap()
                .breakpoints
                .last()
                .unwrap()
                .fingerprint,
            "字符串内容应合成等价的 text 块"
        );
    }

    #[test]
    fn detect_max_ttl_defaults_to_5m_and_picks_1h() {
        let body = "body ".repeat(20);
        assert_eq!(
            detect_max_ttl(&req(vec![text_msg("user", &body)])),
            DEFAULT_PROMPT_CACHE_TTL_MS,
            "无 cache_control 时默认 5m（这才让 message 边界成为隐式断点）"
        );
        let r = req(vec![Message {
            role: "user".to_string(),
            content: json!([{
                "type": "text", "text": body,
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]),
        }]);
        assert_eq!(detect_max_ttl(&r), MAX_PROMPT_CACHE_TTL_MS);
    }

    // ---- tracker（两阶段 compute/update）----

    fn go_config() -> crate::model::config::CacheEngineGoConfig {
        crate::model::config::CacheEngineGoConfig {
            max_ratio: 0.85,
            ttl_seconds: 300,
            max_entries: 131072,
            // 测试用小阈值：默认 1024 需要极长 fixture 才能跨过。
            min_cacheable_tokens: 0,
            opus_min_cacheable_tokens: 0,
            ..Default::default()
        }
    }

    fn tracker() -> GoCacheTracker {
        let t = GoCacheTracker::new(None, go_config());
        t.set_clock_ms(1_000_000);
        t
    }

    fn profile_of(r: &MessagesRequest, tracker: &GoCacheTracker) -> PromptCacheProfile {
        build_claude_profile(r, 0, tracker.effective_ttl_ms()).expect("应有断点")
    }

    fn convo(turns: usize) -> MessagesRequest {
        let body = "the quick brown fox jumps over the lazy dog ".repeat(20);
        let mut msgs = Vec::new();
        for i in 0..turns {
            msgs.push(text_msg(
                if i % 2 == 0 { "user" } else { "assistant" },
                &body,
            ));
        }
        req(msgs)
    }

    /// **首轮不得伪造 read**。这条是一阶段/两阶段搞错时最先炸的测试。
    #[test]
    fn first_turn_reports_creation_only_never_read() {
        let t = tracker();
        let p = profile_of(&convo(3), &t);
        let u = t.compute(&p);
        assert_eq!(u.cache_read, 0, "首轮无任何已缓存前缀，read 必须为 0");
        assert!(u.cache_creation > 0, "首轮应全部计 creation");
        assert_eq!(t.stats().misses, 1);
        assert_eq!(t.stats().hits, 0);
    }

    /// 多轮：compute→update 逐轮推进，第二轮起应命中上一轮写入的前缀。
    #[test]
    fn multi_turn_hits_previous_turn_prefix() {
        let t = tracker();

        let p1 = profile_of(&convo(3), &t);
        let u1 = t.compute(&p1);
        assert_eq!(u1.cache_read, 0);
        t.update(&p1);

        let p2 = profile_of(&convo(5), &t);
        let u2 = t.compute(&p2);
        assert!(u2.cache_read > 0, "第二轮应命中历史前缀");
        assert!(u2.cache_creation > 0, "本轮新增内容仍计 creation");
        t.update(&p2);

        let p3 = profile_of(&convo(7), &t);
        let u3 = t.compute(&p3);
        assert!(u3.cache_read > u2.cache_read, "命中前缀应随对话增长而变深");
    }

    /// `scan_start = len-2`：即便**所有**断点都已预先入表，最深断点仍计 creation。
    #[test]
    fn deepest_breakpoint_always_counts_as_creation() {
        let t = tracker();
        let p = profile_of(&convo(5), &t);
        assert!(p.breakpoints.len() >= 2);

        // 先把全部断点写入（含最深那个），再 compute。
        t.update(&p);
        let u = t.compute(&p);

        assert!(u.cache_read > 0, "较浅断点应命中");
        assert!(
            u.cache_creation > 0,
            "最深断点覆盖的本轮新内容必须计 creation，得到 {}",
            u.cache_creation
        );
        // 命中的是 len-2 断点，而非最深那个。
        let second_deepest = p.breakpoints[p.breakpoints.len() - 2].cumulative_tokens;
        assert_eq!(u.cache_read, second_deepest.min(u.cache_read.max(second_deepest)));
    }

    /// update 只在成功路径调用，故失败请求（不调 update）不得污染缓存。
    #[test]
    fn compute_alone_does_not_write() {
        let t = tracker();
        let p = profile_of(&convo(3), &t);
        t.compute(&p);
        assert_eq!(t.stats().entries, 0, "compute 不应写入任何条目");
        t.compute(&p);
        assert_eq!(t.compute(&p).cache_read, 0, "反复 compute 仍不该命中");
    }

    /// 最小可缓存阈值：所有断点都低于阈值时，既不命中也不写入。
    #[test]
    fn min_cacheable_threshold_blocks_matching_and_storage() {
        let cfg = crate::model::config::CacheEngineGoConfig {
            min_cacheable_tokens: 1_000_000,
            opus_min_cacheable_tokens: 1_000_000,
            ..go_config()
        };
        let t = GoCacheTracker::new(None, cfg);
        t.set_clock_ms(1_000_000);

        let p = profile_of(&convo(5), &t);
        let u1 = t.compute(&p);
        assert_eq!(u1.cache_creation, 0, "低于阈值的前缀不产生 creation");
        assert_eq!(u1.cache_read, 0);

        t.update(&p);
        assert_eq!(t.stats().entries, 0, "低于阈值的断点不应入表");

        let u2 = t.compute(&p);
        assert_eq!(u2.cache_read, 0);
    }

    /// 0.85 封顶只作用于非首轮；首轮分支刻意跳过它。
    #[test]
    fn ratio_cap_applies_only_after_first_turn() {
        let t = tracker();
        let p = profile_of(&convo(5), &t);
        let deepest = p.breakpoints.last().unwrap().cumulative_tokens;

        // 首轮：creation == 完整最深累计（未被 0.85 夹）。
        let u1 = t.compute(&p);
        assert_eq!(
            u1.cache_creation, deepest.min(p.total_input_tokens),
            "首轮不套封顶"
        );
        t.update(&p);

        // 非首轮：creation + read 之和受 0.85×total 约束。
        let u2 = t.compute(&p);
        let cap = (p.total_input_tokens as f64 * 0.85) as i64;
        assert!(
            u2.cache_creation + u2.cache_read <= cap,
            "非首轮缓存量 {} 应 ≤ 封顶 {}",
            u2.cache_creation + u2.cache_read,
            cap
        );
    }

    /// TTL 过期 → miss，且 expirations 计数累加。
    #[test]
    fn ttl_expiry_causes_miss_and_counts_expiration() {
        let t = tracker();
        let p = profile_of(&convo(5), &t);
        t.compute(&p);
        t.update(&p);
        assert!(t.stats().entries > 0);

        // 越过 5m TTL。
        t.advance_clock_ms(300_001);
        let u = t.compute(&p);
        assert_eq!(u.cache_read, 0, "过期后不得命中");
        assert!(t.stats().expirations > 0, "应记录过期清理");
        assert_eq!(t.stats().entries, 0, "过期条目应被清空");
    }

    /// 命中会续期，使该前缀在原 TTL 之后仍存活。
    #[test]
    fn hit_extends_ttl() {
        let t = tracker();
        let p1 = profile_of(&convo(3), &t);
        t.compute(&p1);
        t.update(&p1);

        // 在 TTL 内命中一次 → 续期。
        t.advance_clock_ms(200_000);
        let p2 = profile_of(&convo(5), &t);
        assert!(t.compute(&p2).cache_read > 0);

        // 再走 200s：距原始写入已 400s > 300s TTL，但因续期仍应存活。
        t.advance_clock_ms(200_000);
        assert!(
            t.compute(&p2).cache_read > 0,
            "命中续期后条目不应在原 TTL 点过期"
        );
    }

    /// LRU：容量 2、写入 3 个断点 → 最旧序号被淘汰，evictions 累加。
    #[test]
    fn lru_evicts_lowest_seq_first() {
        let t = tracker();
        // sanitized() 有 256 条下限（防误配把缓存关掉），故直接压原子量来验证 LRU。
        t.max_entries.store(2, Ordering::Relaxed);

        let p = profile_of(&convo(3), &t);
        assert_eq!(p.breakpoints.len(), 3, "本 fixture 应有 3 个断点");
        t.update(&p);

        let s = t.stats();
        assert_eq!(s.entries, 2, "应被夹到容量 2");
        assert_eq!(s.evictions, 1);
        // 保留的是后写入（序号更大）的两个。
        let all = t.inner.lock();
        let inner = all.get(&0).unwrap();
        assert!(!inner.entries.contains_key(&p.breakpoints[0].fingerprint));
        assert!(inner.entries.contains_key(&p.breakpoints[1].fingerprint));
        assert!(inner.entries.contains_key(&p.breakpoints[2].fingerprint));
    }

    /// 同一账号的不同客户端共享，相邻账号不共享。
    #[test]
    fn fingerprints_are_shared_within_account_only() {
        let t = tracker();
        // profile 不含任何 key 维度信息 —— 同内容必然同指纹。
        let a = profile_of(&convo(5), &t);
        let b = profile_of(&convo(5), &t);
        assert_eq!(a.breakpoints[0].fingerprint, b.breakpoints[0].fingerprint);

        t.compute_for_account(11, &a);
        t.update_for_account(11, &a);
        assert!(
            t.compute_for_account(11, &b).cache_read > 0,
            "同一账号的不同客户端应共享前缀"
        );
        assert_eq!(
            t.compute_for_account(12, &b).cache_read,
            0,
            "不同 Kiro 账号不得共享前缀"
        );
    }

    #[test]
    fn empty_profile_is_inert() {
        let t = tracker();
        let empty = PromptCacheProfile {
            breakpoints: Vec::new(),
            total_input_tokens: 100,
            model: "m".to_string(),
        };
        assert_eq!(t.compute(&empty), GoCacheUsage::default());
        t.update(&empty);
        let s = t.stats();
        assert_eq!(s.entries, 0);
        assert_eq!((s.hits, s.misses), (0, 0), "空 profile 不应污染命中率");
    }

    #[test]
    fn apply_config_is_hot_and_sanitizes() {
        let t = tracker();
        assert_eq!(t.effective_ttl_ms(), 300_000);
        t.apply_config(crate::model::config::CacheEngineGoConfig {
            ttl_seconds: 3600,
            max_entries: 10,
            ..go_config()
        });
        assert_eq!(t.effective_ttl_ms(), 3_600_000);
        assert_eq!(t.stats().capacity, 256, "max_entries 有 256 下限");

        // 非法值回落默认
        t.apply_config(crate::model::config::CacheEngineGoConfig {
            max_ratio: 5.0,
            ttl_seconds: 0,
            ..go_config()
        });
        assert_eq!(t.effective_ttl_ms(), 300_000);
        assert_eq!(t.max_ratio_millis.load(Ordering::Relaxed), 850);
    }

    // ---- 持久化 ----

    fn tmp_state_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kiro-gocache-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cache_metering_go.json")
    }

    #[test]
    fn fingerprint_hex_round_trips() {
        let mut fp = [0u8; 32];
        for (i, b) in fp.iter_mut().enumerate() {
            *b = i as u8;
        }
        let hex = encode_fingerprint(&fp);
        assert_eq!(hex.len(), 64);
        assert_eq!(decode_fingerprint(&hex), Some(fp));
        assert_eq!(decode_fingerprint("short"), None);
        assert_eq!(decode_fingerprint(&"z".repeat(64)), None);
    }

    #[test]
    fn persistence_round_trip_restores_hits() {
        let path = tmp_state_path("roundtrip");
        let cfg = go_config();

        let p = {
            let t = GoCacheTracker::new(Some(path.clone()), cfg);
            t.set_clock_ms(1_000_000);
            let p = profile_of(&convo(5), &t);
            t.compute(&p);
            t.update(&p);
            t.flush_to_disk();
            p
        };

        // 新 tracker 从磁盘恢复后，同一前缀应命中。
        let t2 = GoCacheTracker::new(Some(path.clone()), cfg);
        t2.set_clock_ms(1_000_000);
        t2.load();
        assert!(t2.stats().entries > 0, "应载入条目");
        assert!(t2.compute(&p).cache_read > 0, "恢复后应命中原前缀");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_drops_expired_entries() {
        let path = tmp_state_path("expired");
        let cfg = go_config();

        {
            let t = GoCacheTracker::new(Some(path.clone()), cfg);
            t.set_clock_ms(1_000_000);
            let p = profile_of(&convo(5), &t);
            t.compute(&p);
            t.update(&p);
            t.flush_to_disk();
        }

        // 载入时时钟已越过 5m TTL → 全部丢弃。
        let t2 = GoCacheTracker::new(Some(path.clone()), cfg);
        t2.set_clock_ms(1_000_000 + 300_001);
        t2.load();
        assert_eq!(t2.stats().entries, 0, "过期条目不应载入");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_trims_oversized_state_file_to_capacity() {
        let path = tmp_state_path("oversized");
        let cfg = go_config();

        {
            let t = GoCacheTracker::new(Some(path.clone()), cfg);
            t.set_clock_ms(1_000_000);
            let p = profile_of(&convo(3), &t);
            t.compute(&p);
            t.update(&p);
            assert_eq!(t.stats().entries, 3);
            t.flush_to_disk();
        }

        // 容量被压到 2 后载入 3 条 → 必须在 load 内裁剪，否则会一直超标。
        let t2 = GoCacheTracker::new(Some(path.clone()), cfg);
        t2.set_clock_ms(1_000_000);
        t2.max_entries.store(2, Ordering::Relaxed);
        t2.load();
        assert_eq!(t2.stats().entries, 2, "load 应裁剪到容量以内");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_and_corrupt_state_files_start_empty() {
        let cfg = go_config();

        // 缺文件（首次启动常态）
        let missing = tmp_state_path("missing");
        let t1 = GoCacheTracker::new(Some(missing.clone()), cfg);
        t1.load();
        assert_eq!(t1.stats().entries, 0);

        // 损坏文件
        let corrupt = tmp_state_path("corrupt");
        std::fs::write(&corrupt, b"{not valid json").unwrap();
        let t2 = GoCacheTracker::new(Some(corrupt.clone()), cfg);
        t2.load();
        assert_eq!(t2.stats().entries, 0);

        let _ = std::fs::remove_dir_all(missing.parent().unwrap());
        let _ = std::fs::remove_dir_all(corrupt.parent().unwrap());
    }

    #[test]
    fn flush_is_noop_when_not_dirty() {
        let path = tmp_state_path("notdirty");
        let t = GoCacheTracker::new(Some(path.clone()), go_config());
        t.set_clock_ms(1_000_000);
        t.flush_to_disk();
        assert!(!path.exists(), "无变更时不应写文件");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn min_cacheable_tokens_switches_on_opus() {
        assert_eq!(min_cacheable_tokens_for_model("claude-opus-4-8", 1024, 4096), 4096);
        assert_eq!(
            min_cacheable_tokens_for_model("claude-sonnet-4-5", 1024, 4096),
            1024
        );
        assert_eq!(
            min_cacheable_tokens_for_model("Claude-OPUS-5", 1024, 4096),
            4096,
            "大小写不敏感"
        );
    }
}
