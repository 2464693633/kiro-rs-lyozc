# kiro-rs-lyozc 扩展功能文档

> **项目定位**：在原版 [kiro.rs](../kiro.rs) 基础上二次开发，保留所有原版功能，新增三项核心扩展：
> 1. **四引擎**模拟缓存 token 口径（每个引擎独立倍率）
> 2. 上游 API 凭据直通
> 3. 逐引擎计费对比（上游真实成本 ↔ 客户端被计费额，一一配对）

---

## 目录

1. [架构概述](#架构概述)
2. [功能一：四引擎模拟缓存](#功能一四引擎模拟缓存)
   - [四引擎总览](#四引擎总览)
   - [引擎 A — rust](#引擎-a--rust)
   - [引擎 B — go 移植](#引擎-b--go-移植)
   - [引擎 C — real（上游真值）](#引擎-c--real上游真值)
   - [引擎 D — nocache（本地估算）](#引擎-d--nocache本地估算)
   - [倍率的应用规则](#倍率的应用规则)
3. [功能二：上游 API 凭据直通](#功能二上游-api-凭据直通)
4. [功能三：逐引擎计费对比](#功能三逐引擎计费对比)
   - [v1 的结构性缺陷（已修）](#v1-的结构性缺陷已修)
   - [v2 的逐引擎配对](#v2-的逐引擎配对)
   - [v1 数据的迁移](#v1-数据的迁移)
5. [Admin 界面与 API](#admin-界面与-api)
6. [配置文件形状](#配置文件形状)
7. [与原版差异清单](#与原版差异清单)
8. [已知问题与注意事项](#已知问题与注意事项)
9. [数据流总结图](#数据流总结图)

---

## 架构概述

```
客户端 (Claude Code / 其他工具)
        │  Anthropic Messages API 格式
        ▼
  kiro-rs-lyozc (本项目)
        │
        ├── 普通 Kiro 凭据 ──► Kiro 端点 (原版流程)
        │       │ 响应（无 cache 字段，只有 credit）
        │       └── 按引擎口径重建 usage ──► 返回客户端
        │
        └── 上游 API 凭据 ──► 另一个 Anthropic 兼容端点
                │               (kiro.rs 反代 / 其他中转)
                │ 响应（含真实 usage）
                └── 按引擎口径改写 usage ──► 返回客户端
```

**核心原则**：

- 发给上游的请求体**完整原样转发**，不做格式转换
- 返回给客户端的 `usage` 由**该 Key 选中的引擎**决定口径，四种口径互不相同
- 口径分歧全部收敛在 `UsageMode::resolve_tokens` 一处 —— 客户端所见与计费记录共用它，
  避免两处各写一遍而逐渐漂移
- 倍率**每个引擎各用自己一套**，不串联相乘

---

## 功能一：四引擎模拟缓存

### 设计目的

Kiro 上游不返回 `cache_creation_input_tokens` / `cache_read_input_tokens`（只返回 credit
计费量）。本项目在中转层重建这两个字段，否则 Claude Code 等客户端会认为缓存未生效而反复
重发完整上下文。

四个引擎提供四种不同的重建口径，按客户端 Key 独立选择，可在同一部署内并存对比。

### 四引擎总览

| | A · rust | B · go | C · real | D · nocache |
|---|---|---|---|---|
| **枚举值** | `"rust"` | `"go"` | `"real"` | `"nocache"` |
| **input 来源** | 模拟分摊 | 模拟分摊 | 上游真值 | **本地估算** |
| **output 来源** | 上游真值 | 上游真值 | 上游真值 | **本地估算** |
| **cache 字段** | 模拟 | 模拟 | 上游真值 | **恒为 0** |
| **缓存状态** | 有（指纹表） | 有（指纹表） | 无 | 无 |
| **隔离粒度** | session / key_id | Kiro 凭据 | — | — |
| **写入时机** | `begin()` 即写 | `commit()` 成功后写 | — | — |
| **倍率维度** | 4 | 4 | 4 | 2（无 cache） |

`UsageMode` 是这张表的代码表达（`cache_engine.rs`）：

| Mode | 用于 | token 来源 |
|---|---|---|
| `Simulated` | A / B | `CacheUsage::split_against_total` 按比例分摊 |
| `Real` | C，以及 **A/B 的降级** | 上游真实三元组原样 |
| `NoCache` | D | 本地估算，cache 恒 0 |

> **`Simulated` 自带降级**：引擎 B 的 `CacheUsage` 要等 `compute_pending` 在选定凭据后才
> 算出，故 `begin()` 返回 `Simulated` 时无法预知结果是否为零。`resolve_tokens` 内部再判一次
> `is_simulated()`，不成立则降级 `Real`。这使「mode 说模拟、数据却不支持」不可能穿到
> `split_against_total`（否则调用方的 `.max(1)` 会把 `input_tokens` 钉成 1）。

---

### 引擎 A — rust

**源文件**：`src/anthropic/cache_metering.rs`

#### 算法流程

```
请求到达
  │
  ├─ 提取会话种子
  │     优先：metadata.user_id 中的 _session_<uuid>
  │     其次：key:<key_id>
  │     若无（key_id=0 且无 session）：跳过，返回全零 CacheUsage → 降级 Real
  │
  ├─ 分段（extract_segments）
  │     按顺序遍历：tools → system → 历史 messages（最后一条除外）
  │     每个"断点边界"取 SHA-256 哈希低 64 位 + 累计 token 估算值
  │
  ├─ 查命中（cache.lookup）
  │     找最深命中段 → cache_read = 该段累计 token
  │     cache_creation = 覆盖前缀总量 - cache_read
  │
  ├─ 写回（cache.record）
  │     刷新命中段 TTL（begin 阶段即读即写，失败不回滚 —— 见 P3）
  │
  └─ 返回 CacheUsage { cache_read, cache_covered_est, prompt_total_est }
```

#### Token 分配

`CacheUsage::split_against_total(real_total)` 把模拟值按比例分配到真实 total 口径：

```
prefix_ratio = cache_covered_est / prompt_total_est
cache_total  = round(real_total × prefix_ratio)
read         = round(cache_total × cache_read / cache_covered_est)
creation     = cache_total - read
input        = real_total - cache_total
```

#### 倍率（四个，均为 `Option`）

A 的四个倍率**未设置时逐项回退全局膨胀倍率**：

| 字段 | 未设置时回退 |
|---|---|
| `inputMultiplier` | `inputInflationMultiplier` |
| `outputMultiplier` | `outputInflationMultiplier` |
| `cacheReadMultiplier` | `cacheInflationMultiplier` |
| `cacheCreationMultiplier` | `cacheInflationMultiplier` |

> **为什么是 `Option` 而非默认 1.0**：老部署只配了全局倍率。若 A 的字段直接默认 1.0，升级
> 后已配的全局倍率会**静默失效**（比如 input=2.0 突然变成 1×），且没有任何报错。
>
> 同理，`sanitized()` 遇到非法值（NaN / ≤0）回落 `None` 而非 1.0 —— 误配应退回"继承全局"，
> 不该静默改成"不缩放"。

回退在 `CacheMeter::multipliers()` 内完成，底层用 `-1` 毫倍率作哨兵表示未设置。

#### 持久化

- 内存：`HashMap<u64, CacheEntry>`，按 `last_hit_at` 淘汰，默认上限 4096 条
- 磁盘：`cache_metering.json`，后台任务定期落盘，启动时自动加载未过期条目

---

### 引擎 B — go 移植

**源文件**：`src/anthropic/cache_metering_go.rs`

移植自 `kiro-go-lyozc` 的 `proxy/cache_tracker.go`，刻意与 Go 版本行为对齐。

#### 核心差异（对比引擎 A）

| 方面 | 引擎 A | 引擎 B |
|------|----------|----------|
| 哈希输入 | 结构化文本签名 | 包装对象的规范 JSON（含结构字节） |
| Token 估算 | 仅原始文本字符 | 规范 JSON 全部字节 |
| HTML 转义 | 不转义 | 对齐 Go `json.Marshal`，`<>&` 转为 `<>&` |
| 写入时机 | `begin()` 即读即写 | `commit()` 仅在请求成功后写入 |
| 隔离粒度 | 每个 session / key_id | 每个 Kiro 凭据（`credential_id`） |
| 最深断点处理 | 不扫最后一个断点 | `scan_start = len-2`；仅 1 个断点时不扫描，整段计 creation |
| 首轮上限 | 无 | 有 `maxRatio × total` 上限（非首轮会话） |

#### 两阶段设计

```
请求开始：compute_for_account()   → 只读，标 dirty（续期命中条目）
              ↓
          返回 GoCacheUsage { cache_read, cache_creation, total_input }
              ↓
          如果请求成功：
              update_for_account()  → 写入本轮断点，标 dirty
          如果请求失败：
              不调用 update，缓存状态不受污染
```

#### 断点构建规则（PromptCacheProfile）

```
Block 0：model + tool_choice（模型变化使整条链失效）
Tool blocks：每个工具的规范 JSON 包装对象
System blocks：每个 system 消息块
Message blocks：每条 message（角色 + 内容）的包装对象
  ├── 有 cache_control.type="ephemeral" → 产生显式断点
  └── is_message_end && 消息索引 < len-1 → 产生隐式断点
```

#### 隔离粒度

指纹表按 **Kiro 凭据 ID** 分区（`HashMap<u64, TrackerInner>`）。同一凭据下的不同客户端 Key
**共享**前缀（刻意对齐 Go 原版），不同凭据之间**不共享**。

由测试 `fingerprints_are_shared_within_account_only` 钉住两侧：同账号命中、跨账号不命中。

#### 倍率（四个，全部为具体值）

```jsonc
"cacheEngineGo": {
  "inputTokenMultiplier": 1.0,
  "outputMultiplier": 1.0,          // 本项目新增；Go 原实现无此倍率
  "cacheReadMultiplier": 1.0,
  "cacheCreationMultiplier": 1.0    // 1.0 = Go 原实现（只缩放 input 与 cache_read）
}
```

`cacheCreationMultiplier` 调离 1.0 会偏离 Go 原实现，且会削弱两套引擎在「creation/read 划分」
这个维度上的可比性 —— 数值差异将无法区分是算法不同还是倍率不同造成的。

---

### 引擎 C — real（上游真值）

**源文件**：`src/anthropic/cache_engine.rs`（无状态，无独立文件）

不模拟缓存，直接采用上游真实 usage，仅套自己那组倍率。

- 无指纹表、无 TTL、无 LRU、无 `commit`
- 上游返回 cache 字段时**原样保留**（各自乘对应倍率）
- Kiro 上游本就不下发 cache 字段，故该路径上 C 的拆分为 `(total, 0, 0)`

```jsonc
"cacheEngineReal": {
  "inputMultiplier": 1.0,
  "outputMultiplier": 1.0,
  "cacheReadMultiplier": 1.0,
  "cacheCreationMultiplier": 1.0
}
```

四个倍率都是具体值，**不继承全局倍率** —— 未配置时即 1.0（不缩放）。

---

### 引擎 D — nocache（本地估算）

**源文件**：`src/anthropic/cache_engine.rs` + `src/anthropic/upstream.rs`（累积器）

**完全不读上游 usage**。这是它与引擎 C 的本质区别，也是四个引擎里唯一一个客户端 token
数与上游无关的。

| 字段 | 来源 |
|---|---|
| `input_tokens` | `token::count_all_tokens` —— 客户端请求的本地估算，请求**发出前**即确定 |
| `output_tokens` | `token::estimate_output_tokens` —— 从实际返回内容本地估算 |
| `cache_creation` / `cache_read` | 恒为 0（该引擎的定义，不是"上游没给"） |

#### 为什么不读上游

上游可能本身就是另一个 kiro-rs 反代，它报的 usage 已经被模拟 / 膨胀加工过一轮。
拿来当"真值"等于把上一跳的加工结果再加工一次。本地口径与上游无关，可独立复现。

#### 流式如何算 output

`count_tokens` 是**非线性**的（<100 token 乘 1.5，≥800 乘 1.0）。所以流式路径不能对每个
SSE delta 分别调用再求和 —— 每个小片段都会吃到 1.5 倍，严重高估。

实现是按 block index 累积 `content_block_delta`，重建成 content 数组，末尾**只调一次**
`estimate_output_tokens`。这使流式与非流式算出同一个数**由构造保证**，而非靠两处逻辑同步。

时序上恰好成立：
- `input_tokens` 在流**开头**的 `message_start` 写 —— 本地算的，请求发出前已知
- `output_tokens` 在流**末尾**的 `message_delta` 写 —— 那时 delta 已全部累积完

#### 倍率（两个）

```jsonc
"cacheEngineNocache": {
  "inputMultiplier": 1.0,
  "outputMultiplier": 1.0
}
```

**不提供 cache 倍率**：cache 恒为 0，乘任何数都是 0，留着只会让运维以为调它有用。

内部倍率四元组的 cache 两位取 `0.0` 而非 1.0。这与 token 层的归零重复表达，是刻意的冗余 ——
将来若有人新增一条不经 `resolve_tokens` 的路径并直接套用 D 的倍率，1.0 会让上游 cache 原样
漏出、违背引擎定义，0.0 则结构上不可能。

---

### 倍率的应用规则

> ⚠️ **各引擎倍率不串联相乘。** 每个引擎用**自己那一组**，`UsageMultipliers::resolve()` 里是
> 四个平行分支。唯一的跨组关系是引擎 A 未设置时逐项回退全局倍率。

```
引擎选择（客户端 Key 的 cacheEngine）
  │
  ├─ 定出 UsageMode  ──► 决定 token 从哪来（模拟分摊 / 上游真值 / 本地估算）
  │
  └─ 定出 UsageMultipliers ──► 决定乘多少
                                │
                    该引擎自己那一组四元组
                    (input, output, cache_read, cache_creation)
```

`UsageMode` 的三个分支收敛在 `UsageMode::resolve_tokens()` 一处 —— `upstream.rs` 的三个改写点
（非流式响应、SSE `message_start`、用量统计）与计费快照共用它。若各写一遍，客户端看到的数字
与用量日志会在引擎切换时悄悄分叉。

`resolve_tokens` **自带降级**：引擎 B 的 `CacheUsage` 要等 `compute_pending` 在选定凭据后才
算出，`begin()` 返回 `Simulated` 时无法预知结果是否为零。所以函数内部再判一次，
「mode 说模拟、数据却不支持模拟」时自动回落上游真值 —— 否则调用方的 `.max(1)` 会把
`input_tokens` 钉成 1（这正是 P1 的成因）。

---

## 功能二：上游 API 凭据直通

### 用途

允许在凭据池中添加"上游 API 凭据"，指向另一个 Anthropic 兼容 API 端点（如另一个 kiro.rs
反代实例）。请求以 Anthropic Messages API 格式原样转发，无需 Kiro 协议转换。

### 凭据配置

| 字段 | 说明 |
|------|------|
| `upstream_base_url` | 上游端点基础 URL，如 `https://api.example.com` |
| `upstream_token` / `token` | API Key，转发时作为 `x-api-key` 头 |

凭据记录上 `upstream_base_url` 非空即判定为上游凭据（`is_upstream_credential() == true`）。

### 工作流程

```
call_api_with_retry_for_credential()
  │
  ├── is_upstream_credential() == true
  │     │
  │     ├── 目标 URL：{upstream_base_url}/v1/messages
  │     ├── 请求体：anthropic_body_raw（未经 Kiro 格式转换的原始 JSON）
  │     │           若无 anthropic_body_raw 则由 kiro_body 降级转换
  │     ├── 请求头：
  │     │     x-api-key: {upstream_token}
  │     │     anthropic-version: 2023-06-01
  │     │     anthropic-beta: {透传客户端头，如有}
  │     │
  │     └── 响应处理（见下节）
  │
  └── is_upstream_credential() == false
        → 走原版 Kiro 协议路径（不变）
```

### 请求与响应处理

**请求**：完整原样转发 Anthropic 格式请求体，不做任何字段修改。

**非流式响应**（`upstream.rs::handle_upstream_non_stream_response`）：

```
上游返回 Anthropic JSON 响应
  │
  ├── 提取真实 token → raw_usage（供计费对比的"上游真实"列，恒为上游真值）
  │
  ├── 按引擎口径重建 usage（UsageMode::resolve_tokens）
  │     input_tokens                 ← 模拟分摊 / 上游真值 / 本地估算
  │     output_tokens                ← 上游真值，或引擎 D 的本地估算
  │     cache_creation_input_tokens  ← 同上口径
  │     cache_read_input_tokens      ← 同上口径
  │   再各乘该引擎对应倍率
  │
  └── 返回修改后的 JSON 给客户端
```

上游**未下发 `usage` 对象**时也会补齐（`.entry("usage").or_insert_with`），避免客户端拿到
缺失口径。

**流式响应**（`upstream.rs::handle_upstream_stream_response_with_inflation`）：

| 事件 | 处理 |
|---|---|
| `message_start` | 重写 `usage.input_tokens` / `cache_creation_input_tokens` / `cache_read_input_tokens` |
| `content_block_delta` | **仅引擎 D 读取**（累积输出文本用于本地估算），事件本身原样转发 |
| `message_delta` | 重写 `usage.output_tokens` |
| 其他 | 原样转发 |

引擎 D 在 `message_delta` 上即使上游**没给** `usage` 对象也会补写 —— 它的 output 口径与上游
无关，若因上游缺字段而跳过，客户端会丢掉整个输出计数。

> 非流式路径原本就用 `.entry().or_insert_with` 处理了这种情况，流式路径没有。这是加引擎 D
> 时由测试 `nocache_writes_local_output_even_when_upstream_omits_it` 抓出来的实际缺陷。

### WebSearch 路径的特殊处理

`handle_websearch_request` 在路由前会预先 acquire 一次凭据上下文以判断类型：

- **上游凭据**：调用 `call_api_dual_for_credential(ctx.id, …)` 直接透传 web_search tool，
  由上游 Anthropic 原生处理
- **Kiro 凭据**：走原有 MCP WebSearch 路径

---

## 功能三：逐引擎计费对比

### 设计目的

对比「上游真实成本」与「客户端被计费成本」，算出每个引擎的实际加价倍数。

**只统计上游凭据的请求** —— Kiro 凭据没有美元成本口径。

### v1 的结构性缺陷（已修）

旧 schema 把用量存成三个平铺槽：

```
upstream_usage   ← 一个共享槽
rust_usage       ← 引擎 A 的模拟值
go_usage         ← 引擎 B 的模拟值
```

混合流量下 `upstream_usage` 累加的是「A 请求的上游 + B 请求的上游」，而 `rust_usage` 只含
A 的客户端计费。拿 `rust_cost / upstream_cost` 算加价倍数，**分母里混着 B 的成本** —— 这个
比值没有意义。引擎 C / D 更是无处安放。

### v2 的逐引擎配对

```
EngineBillingPair {
    upstream,   // 该引擎流量的上游真实用量
    client,     // 该引擎流量的客户端计费用量（已乘倍率）
    calls,      // 该引擎的上游请求数
}
```

按 `(key_id, engine, credential_id, model)` 索引。两个口径**同一次请求同时记入**，所以在任意
聚合层级上两者始终可比 —— 这是 v1 做不到的。

**用量记录字段**（`UsageRecord`）：

| 字段 | 说明 |
|---|---|
| `engine` | 引擎标识（`rust` / `go` / `real` / `nocache`） |
| `upstreamUsage` | 上游真实用量 |
| `clientUsage` | 客户端被计费用量（已乘该引擎倍率） |
| ~~`rustUsage`~~ / ~~`goUsage`~~ | v1 兼容字段，**只读不写**，由 `normalized()` 折叠 |

### v1 数据的迁移

`UsageRecord::normalized()` / `TraceRecord::normalized()` 把 v1 的 `rust_usage` / `go_usage`
折叠进 `engine` + `client_usage`。在两处调用：

- JSONL 载入路径（`load_from_dir`）
- trace 读取路径（`admin/handlers.rs` 的 trace 查询）

> 这两个调用点是必需的。`normalized()` 曾一度只在测试里被调用，导致 v1 记录载入后
> `engine` 为 `None`、ingest 直接跳过，**全部历史计费数据从对比表里静默消失**。

`engine` 字段用 `String` 而非枚举：JSONL 载入是 `if let Ok(rec)`，**解析失败会静默丢弃整条
记录**。若将来新增引擎、用旧二进制读那批 JSONL，枚举会让整条记录连 token 数一起消失，而不
只是丢个引擎标签。

### 计费配置

```jsonc
{
  "billing": {
    "modelPrices": {
      "claude-opus-4-7": {
        "inputPerMillion": 15.0,
        "outputPerMillion": 75.0,
        "cacheCreationPerMillion": 18.75,
        "cacheReadPerMillion": 1.5
      }
    },
    "upstreamMultipliers": { "9": 1.0 },   // 按凭据 id 的成本调节系数
    "rustMultiplier": 1.0,                  // 各引擎的计费调节系数
    "goMultiplier": 1.0,
    "realMultiplier": 1.0,
    "nocacheMultiplier": 1.0
  }
}
```

> 这几个 `*Multiplier` 是**计费对比专用**的成本调节系数，与下发给客户端的 token 膨胀倍率是
> 两件不同的东西（后者已作用在 `clientUsage` 上）。

### API 响应形状

```jsonc
{
  "points": [{
    "ts": "2026-08-01T12:00:00+08:00",
    "upstreamCost": 1.23,      // 本桶所有引擎合计
    "clientCost": 2.46,
    "calls": 42,
    "engines": [{              // 逐引擎明细
      "engine": "rust",
      "upstreamCost": 0.80,
      "clientCost": 1.60,
      "upstreamTokens": 100000,
      "clientTokens": 200000,
      "calls": 20
    }]
  }],
  "upstreamCost": 1.23,        // 全窗口汇总
  "clientCost": 2.46,
  "calls": 42,
  "engines": [ /* 同上形状 */ ]
}
```

同一条目内的两个成本来自同一批请求，可直接相除得加价倍数。

---

## Admin 界面与 API

### 缓存引擎弹窗

顶栏「缓存模拟引擎参数」按钮打开，分四段：

| 段 | 内容 |
|---|---|
| **倍率矩阵** | 4 引擎 × 4 维度 = 16 个输入框，一屏可比 |
| **缓存参数** | 只有 A / B 两栏（C / D 无缓存状态，无 TTL / 容量可言） |
| **全局回退倍率** | 三个值，仅在 A 对应维度留空时生效 |
| **运行计数器** | 只有 A / B（条目数 / 命中率 / 淘汰 / 过期） |

倍率矩阵里 D 的 cache 两列显示 `—` 而非输入框 —— 它的 cache 恒为 0，给旋钮只会误导。

A 的输入框**留空 = 继承全局**，placeholder 显示实际生效的全局值。打开弹窗时不会把全局值
填进输入框 —— 否则「打开再保存」会把继承固化成显式值，静默切断后续全局调整。

保存后四套引擎全部热生效，无需重启。

### Admin API

```
GET  /api/admin/config/cache-engines     查询四套引擎配置 + 全局回退倍率
PUT  /api/admin/config/cache-engines     更新（落盘 + 热生效）

GET  /api/admin/cache-engines/stats      A / B 的运行计数器
GET  /api/admin/config/token-inflation   查询全局倍率
PUT  /api/admin/config/token-inflation   更新全局倍率
GET  /api/admin/config/billing           查询计费配置
PUT  /api/admin/config/billing           更新计费配置
GET  /api/admin/stats/billing            逐引擎计费对比数据
```

`PUT /config/cache-engines` 的 payload 里 `real` / `nocache` / `global` 均可省略。省略时的
回落顺序是**磁盘现值 → 运行时现值 → 默认**，不会直接回落默认 —— 磁盘读失败（文件被占用 /
路径未知）时那会把已配倍率静默重置成 1.0，且紧接着的 `apply_config` 会让这个重置真正生效。

该端点同时写两处（四段引擎参数 + 全局倍率）。全局倍率的范围校验（`[1.0, 100.0]`）在函数
开头就做，避免"引擎参数已写、倍率校验失败"的半应用状态。

### 客户端 Key 的引擎选择

新建 Key 时选四个引擎之一（2×2 按钮网格），默认 `rust`。Key 列表里非默认引擎会显示徽章。

```
POST /api/admin/client-keys          { "cacheEngine": "rust" | "go" | "real" | "nocache" }
PUT  /api/admin/client-keys/{id}     可修改
```

---

## 配置文件形状

```jsonc
{
  // 全局膨胀倍率 —— 只有三个（没有 cacheCreation 那一项）。
  // 引擎 A 未显式配置对应维度时逐项回退到这里。
  "inputInflationMultiplier": 1.0,
  "outputInflationMultiplier": 1.0,
  "cacheInflationMultiplier": 1.0,

  // 引擎 A：缓存参数 + 四个 Option 倍率（null / 缺省 = 继承全局）
  "cacheEngineRust": {
    "capacity": 4096,
    "maxTtlSecs": 3600,
    "defaultTtlSecs": 300,
    "inputMultiplier": null,
    "outputMultiplier": null,
    "cacheReadMultiplier": null,
    "cacheCreationMultiplier": null
  },

  // 引擎 B：缓存参数 + 四个倍率（含新增的 outputMultiplier）
  "cacheEngineGo": {
    "maxRatio": 0.85,
    "ttlSeconds": 300,
    "maxEntries": 131072,
    "minCacheableTokens": 1024,
    "opusMinCacheableTokens": 1024,
    "inputTokenMultiplier": 1.0,
    "outputMultiplier": 1.0,
    "cacheReadMultiplier": 1.0,
    "cacheCreationMultiplier": 1.0
  },

  // 引擎 C：只有四个倍率（无缓存状态）
  "cacheEngineReal": {
    "inputMultiplier": 1.0,
    "outputMultiplier": 1.0,
    "cacheReadMultiplier": 1.0,
    "cacheCreationMultiplier": 1.0
  },

  // 引擎 D：只有两个倍率（cache 恒为 0）
  // 注意键名是 cacheEngineNocache，不是 cacheEngineNoCache
  "cacheEngineNocache": {
    "inputMultiplier": 1.0,
    "outputMultiplier": 1.0
  },

  // 计费对比配置
  "billing": {
    "modelPrices": {
      "claude-sonnet-4-5": {
        "inputPerMillion": 3.0,
        "outputPerMillion": 15.0,
        "cacheCreationPerMillion": 3.75,
        "cacheReadPerMillion": 0.30
      }
    },
    "upstreamMultipliers": { "9": 1.0 },
    "rustMultiplier": 1.0,
    "goMultiplier": 1.0,
    "realMultiplier": 1.0,
    "nocacheMultiplier": 1.0
  }
}
```

老配置文件缺任何一段都会逐字段回落默认，不会报错。

---

## 与原版差异清单

### 新增文件

| 文件 | 内容 |
|---|---|
| `src/anthropic/cache_engine.rs` | 四引擎统一接口层：`CacheEngineKind` / `UsageMode` / `UsageMultipliers` / `CacheEngines` / `PendingCache` / `StatelessMultipliers` |
| `src/anthropic/cache_metering_go.rs` | 引擎 B 完整实现（`GoCacheTracker` + `PromptCacheProfile` + 规范 JSON 估算器） |
| `src/anthropic/upstream.rs` | 上游直通处理器（非流式 + 流式 + SSE 改写 + 引擎 D 的输出累积器） |
| `admin-ui/src/components/cache-engine-dialog.tsx` | 四引擎参数弹窗（4×4 倍率矩阵） |
| `admin-ui/src/components/ui/table.tsx` | 表格基础组件 |

### 修改文件

| 文件 | 主要变更 |
|---|---|
| `src/anthropic/cache_metering.rs` | `CacheMeterStats`、原子参数热重载、`spawn_background()`、引擎 A 的四个 `Option` 倍率（哨兵 `-1`） |
| `src/anthropic/handlers.rs` | 四引擎路由、`UsageMode` 贯穿、`ClientTokens` / `BillingSnapshot`、上游凭据路径 |
| `src/admin/usage_stats.rs` | `UsageRecord` 的 `engine` + `client_usage` 配对字段、`normalized()`、稀疏 `by_engine_billing` 表、`query_billing` 逐引擎重写 |
| `src/admin/trace_db.rs` | `engine` / `client_usage` 两列 + 迁移、`TraceRecord::normalized()` |
| `src/model/config.rs` | 四套引擎配置结构、`BillingConfig` 的四个引擎系数 |
| `src/admin/handlers.rs` | 四引擎配置 API、逐引擎计费查询、trace 读取归一化 |
| `admin-ui/src/components/billing-comparison.tsx` | 三卡片 → 逐引擎表格 |
| `admin-ui/src/components/client-keys-page.tsx` | 2 个引擎按钮 → 4 个（2×2） |
| `admin-ui/src/components/credential-card.tsx` | 固定三行 → 动态引擎行 |

---

## 已知问题与注意事项

### ✅ 已修复

**P1 — 主密钥 + 上游凭据时 input_tokens 恒为 1**

- **触发**：客户端用主密钥（`key_id = 0`，无 session）调用且命中上游凭据
- **原因**：`isolation_seed` 对无 session 的 `key_id=0` 返回 `None`（有意为之 —— 该 Key 被多用户共享，模拟会产生跨用户幻命中），引擎 A 因此返回 `prompt_total_est = 0`，调用方 `.max(1)` 得到 1，`split_against_total(1)` 输出 `(1, 0, 0)`，覆写真实值
- **修复**：新增 `CacheUsage::is_simulated()`（`prompt_total_est > 0 && cache_covered_est > 0`）。后续重构中该判定移入 `UsageMode::resolve_tokens` 的**自兜底**分支 —— `Simulated` 但数据不支持模拟时自动降级 `Real`。这使该 bug 类**结构上不可能再发生**，而非逐点修补（引擎 B 的 `CacheUsage` 在 `begin()` 之后才由 `compute_pending` 填入，`begin()` 无法预知，所以必须在解析处兜底）
- **回归测试**：`streaming_passthrough_keeps_real_tokens_when_simulation_disabled`、`stream_stats_record_real_tokens_when_simulation_disabled`、`simulated_mode_self_downgrades_when_usage_is_empty`

**P2 — 引擎 B 单断点边界错误**

- **问题**：`scan_start` 在 `len == 1` 时得到 0，扫到了本轮最深断点。该断点按设计必须恒计 creation，旧算法命中后报出 `cache_read` 且 `creation` 归零 —— 伪造读数
- **修复**：改用 `checked_sub(2)`，`len < 2` 时提前返回 `cache_read: 0` + 全额 creation
- **回归测试**：`single_breakpoint_never_reports_cache_read`（去掉修复后报 `cache_read: 217`）

**P6 — `normalized()` 从未被调用**

- **问题**：v1→v2 归一化方法写了，但只在测试里调过，生产的 JSONL 载入路径没调。结果 v1 记录载入后 `engine` 为 `None`，ingest 直接跳过，**全部历史计费数据从对比表静默消失**
- **修复**：在 JSONL 载入路径与 trace 读取路径补上调用

**P7 — `TraceRecord::normalized` 根本没实现**

- **问题**：在字段文档注释里引用了它，但 `impl` 块不存在。文档链接失效不会导致编译错误，所以直到调用时才暴露
- **修复**：补上实现

**P8 — 流式 `message_delta` 缺 `usage` 时引擎 D 不写输出**

- **问题**：整个分支被 `if let Some(usage) = json.get_mut("usage")` 包着。上游不下发 `usage` 对象时引擎 D 什么都不写，客户端收到 `null`。非流式路径本来就用 `.entry("usage").or_insert_with` 处理了这种情况，流式没有
- **修复**：引擎 D 在该对象缺失时创建它。其余引擎保持原行为（不凭空造字段）
- **回归测试**：`nocache_writes_local_output_even_when_upstream_omits_it`

---

### 🟡 中严重度

**P3 — 引擎 A 失败请求污染缓存（begin 即写）**

- **位置**：`cache_metering.rs`，`compute_cache_usage` 内的 `cache.record()`
- **影响**：引擎 A 在 `begin()` 阶段即写入指纹表，请求失败不回滚，后续重试可能产生幻命中
- **引擎 B 无此问题**：两阶段设计，仅 `commit()`（成功后）写入

---

### 🟢 低严重度

**P4 — 被改写的两类 SSE 事件会丢弃非标准行**

- **位置**：`upstream.rs` `inflate_sse_event`
- **准确范围**：仅 `message_start` / `message_delta` 两类事件被从头重建（`format!("event: …\ndata: …")`），其 `id:` / `retry:` / 注释行会丢失。其余事件走 `_ => event_text.to_string()` **原样透传**，不受影响
- **影响**：当前 Anthropic 格式不使用这些字段，无实际影响；属前向兼容性风险

**P5 — 逐引擎倍率无上限校验**

- **准确范围**：全局膨胀倍率**有**校验（`set_token_inflation_config` 限定 `1.0..=100.0`）。B / C / D 的倍率只经 `sanitize_multiplier`（`is_finite() && > 0.0`），**无上限**
- **影响**：可设置 1000× 等极端值。倍率只影响下发数字与计费记录，不影响上游真实成本

---

### ⚠️ 使用注意

**引擎 D 的 output 与上游报的数**不一致，这是设计意图 —— 它用本地估算器（`estimate_output_tokens`）算，不读上游 usage。两套估算器口径不同，差异属正常。

**引擎 D 的 input 完全不读上游**，取 `token::count_all_tokens` 的本地结果。若上游本身是另一个 kiro-rs 反代，它报的 usage 已被加工过一轮 —— 引擎 D 的口径与那层加工无关。

**逐引擎倍率不与全局倍率相乘**。每个引擎各用自己一套，引擎 A 的未设置项回退全局。不存在双重膨胀。

**`admin-ui/dist/` 是提交进仓库并由 `rust-embed` 嵌入二进制的**。改前端后必须跑 `npm run build`，否则运行时仍是旧界面。

---

## 数据流总结图

```
                 ┌────────────────────────────────────────────────────────────┐
                 │                  kiro-rs-lyozc 请求处理                     │
                 │                                                            │
请求 ──► handlers │ 1. 解析请求，保存 anthropic_body_raw                        │
         .rs      │ 2. token::count_all_tokens → total_input_tokens（本地口径） │
                 │ 3. cache_engines.begin(kind, global)                       │
                 │      → (CacheUsage, UsageMultipliers, PendingCache,        │
                 │         UsageMode)                                         │
                 │                                                            │
                 │  ┌──────────────────┐    ┌────────────────────────────┐   │
                 │  │  Kiro 凭据路径    │    │      上游凭据路径           │   │
                 │  │  provider.rs     │    │      upstream.rs           │   │
                 │  │  Kiro 协议请求    │    │      Anthropic 原样转发     │   │
                 │  └────────┬─────────┘    └────────────┬───────────────┘   │
                 │           │ 响应                       │ 响应（含真实 usage）│
                 │           ▼                           ▼                   │
                 │  ┌──────────────────────────────────────────────────────┐ │
                 │  │  UsageMode::resolve_tokens(cache_usage, sim_total,    │ │
                 │  │                            real, local_input)        │ │
                 │  │    Simulated → split_against_total（数据不支持时       │ │
                 │  │                自动降级 Real）                        │ │
                 │  │    Real      → 上游真值原样                           │ │
                 │  │    NoCache   → 本地估算，cache 恒 0                   │ │
                 │  │  UsageMode::resolve_output(real, local)               │ │
                 │  │    → ClientTokens（膨胀前四元组）                      │ │
                 │  │  × 该引擎的倍率四元组 → 覆写 usage 字段                 │ │
                 │  └──────────────────────────────────────────────────────┘ │
                 │                                                            │
                 │ 4. 引擎 B commit()（仅 status == "success" 时写入）          │
                 │ 5. hook.record()                                           │
                 │      主字段 = 客户端所见（膨胀前）                            │
                 │      engine + client_usage + upstream_usage（计费配对）      │
                 └────────────────────────────────────────────────────────────┘
                           │
                 修改后响应 ──► 客户端
```

**计费配对的关键性质**：`upstream_usage` 与 `client_usage` 在**同一次请求**内同时记入，故任意聚合层级上两者可比。这是 v1 三槽 schema 做不到的 —— 那里 `upstream_usage` 是所有引擎共用的一个槽。

