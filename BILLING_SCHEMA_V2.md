# 计费 Schema 改造：从三槽改成逐引擎配对

## 问题陈述

v1 schema 的结构性缺陷：

1. **上游成本槽共享** — `upstream_usage` 是所有引擎共用的一个槽，混合流量下累加的是「A 请求的上游 + B 请求的上游」，而 `rust_usage` 只含 A 的客户端计费。拿 `rust_cost / upstream_cost` 算加价倍数时，分母里混着 B 的成本，这个比值没有意义。

2. **引擎 C/D 无处安放** — 三槽（upstream + rust + go）结构里，Real 和 NoCache 引擎没有位置，在对比表里整段缺失。

3. **配对关系不可验证** — 无法确认某个 `rust_usage` 记录对应的上游口径是哪次请求的，因为它们分别存储。

## v2 设计

### 核心改动

**存储层**：
- `UsageRecord`: 添加 `engine: Option<String>`, `client_usage: Option<TokenUsageBreakdown>`
- `TraceRecord`: 同上
- 旧字段 `rust_usage`/`go_usage` 保留只读，靠 `normalized()` 折叠进新字段

**聚合层**：
- `BucketStats` 移除计费字段（从 ~160B 降到 ~56B）
- 新增 `EngineBillingPair { upstream, client, calls }` 独立聚合表
- 索引：`(key_id, engine, credential, model)` 四维

**API 响应**：
```rust
BillingComparisonResponse {
  upstreamCost: f64,
  clientCost: f64,
  calls: u64,
  engines: Vec<EngineBillingPayload>,  // 逐引擎配对
  points: Vec<BillingUsagePoint>,
}

EngineBillingPayload {
  engine: String,           // "rust" | "go" | "real" | "nocache"
  upstreamCost: f64,       // 该引擎的上游真实成本
  clientCost: f64,         // 该引擎的客户端计费成本
  upstreamTokens: u64,
  clientTokens: u64,
  calls: u64,
}
```

### 配对保证

`selected_billing_snapshot()` 返回 `BillingSnapshot { engine, upstream, client }`，三者同源：
```rust
BillingSnapshot {
    engine: CacheEngineKind,           // 本次请求使用的引擎
    upstream: TokenUsageBreakdown,     // 上游真实上报的用量
    client: TokenUsageBreakdown,       // 客户端被计费的用量（已乘该引擎倍率）
}
```

写入路径保证三个字段同时记录，故任意聚合层级上 `upstream ↔ client` 都可比。

## 修复的 Bug

### Bug 1: `normalized()` 从未被调用

**症状**：v1 记录载入后 `engine` 是 `None`，ingest 跳过，历史计费数据从对比表里静默消失。

**根因**：`normalized()` 只在测试里调用，生产的 JSONL 载入路径和 trace 读取路径都没调。

**修复**：
- `usage_stats.rs:573`: 载入时调用 `self.ingest(&rec.normalized())`
- `handlers.rs:1689`: 读取时调用 `let r = r.normalized()`

### Bug 2: `TraceRecord::normalized` 根本没实现

**症状**：编译器不会因为文档链接失效而报错，调用时才暴露。

**修复**：添加 `impl TraceRecord { pub fn normalized(mut self) -> Self { ... } }`

## 测试覆盖

新增 6 个测试验证配对语义：

1. `billing_query_pairs_upstream_with_client_per_engine` — 验证基本配对
2. `each_engine_row_carries_only_its_own_upstream_cost` — **核心**：混合流量下每行只含自己的上游成本
3. `stateless_engines_appear_in_billing_comparison` — Real/NoCache 进对比表
4. `billing_filters_apply_per_engine` — 凭据/Key 过滤逐引擎生效
5. `v1_records_normalize_into_engine_rows` — v1 记录归一化
6. 反转 `stateless_engines_claim_neither_simulation_slot` — v1 断言的局限正是本次要修的

所有 689 个测试通过。

## 前端改动

### TypeScript 类型
- `BillingComparisonResponse` 从三字段改成 `{ upstreamCost, clientCost, engines: EngineBillingRow[] }`
- `CacheEngineKind` 从 `'rust' | 'go'` 改成 `'rust' | 'go' | 'real' | 'nocache'`
- 新增 `EngineBillingRow` 接口

### billing-comparison.tsx
- 从三卡片改成逐引擎表格，显示：引擎 / 上游真实 / 客户端计费 / 差额 / 倍数 / 调用数 / Tokens
- 图表改成每引擎一条线（客户端计费曲线）
- 配置面板增加 Real/NoCache 引擎倍率输入

### cache-engine-dialog.tsx
- 从两面板改成四面板布局
- 新增 Real 引擎参数（minCacheableTokens）
- NoCache 无需参数，仅占位说明

### client-keys-page.tsx
- 引擎选择从 2 个按钮改成 4 个（2×2 网格）
- Key 列表中增加 Real/NoCache 的 Badge 显示

### credential-card.tsx
- 从固定三行改成动态行：上游真实 + 各引擎的客户端计费
- 使用 `data.engines` 数组动态构建

## 内存收益

`BucketStats` 被复制到 8 张聚合表里（按 Key、凭据、模型等维度），但计费字段只有 `query_billing` 读，且只读其中 2 张。移出后：
- `BucketStats`: ~160B → ~56B
- 计费独立表只在上游凭据上建条目（Kiro 凭据本就不参与计费对比），稀疏存储

## 设计判断

### `engine` 用 `String` 而非枚举
JSONL 载入是 `if let Ok(rec)`，解析失败会静默丢弃整条记录。如果将来加了引擎、用旧二进制读那批 JSONL，枚举会让整条记录连 token 数一起消失。持久化数据的读取侧应当宽容。

### 旧字段保留可读
`rust_usage`/`go_usage` 在 struct 和 SQLite 里都留着，只读不写，靠 `normalized()` 折叠。SQLite 删列不方便，且保留可让旧二进制读新库时不出错。

## 升级路径

1. 部署新二进制
2. 已有 JSONL 自动归一化：`normalized()` 在载入时透明折叠
3. 新写入的记录一律用 `engine` + `client_usage` 字段
4. 前端立即显示四引擎对比表，历史数据无缝迁移

## 文件清单

### Rust
- `src/admin/usage_stats.rs` — 核心：移除 `BucketStats` 计费字段，新增 `EngineBillingPair` 聚合表
- `src/admin/types.rs` — API 响应类型：`EngineBillingPayload`, `BillingComparisonResponse`
- `src/admin/trace_db.rs` — `TraceRecord` 增加 `engine`/`client_usage` 字段 + `normalized()` 实现
- `src/admin/handlers.rs` — trace 读取路径调用 `normalized()`
- `src/anthropic/handlers.rs` — `BillingSnapshot` 结构改造 + `selected_billing_snapshot()` 返回配对
- `src/model/config.rs` — `BillingConfig` 增加 `realMultiplier`/`nocacheMultiplier`

### 前端
- `admin-ui/src/types/api.ts` — TypeScript 类型更新
- `admin-ui/src/api/credentials.ts` — `CacheEnginesConfig` 增加 Real/NoCache
- `admin-ui/src/components/billing-comparison.tsx` — 表格 + 图表重写
- `admin-ui/src/components/cache-engine-dialog.tsx` — 四引擎面板
- `admin-ui/src/components/client-keys-page.tsx` — 四引擎按钮
- `admin-ui/src/components/credential-card.tsx` — 动态引擎行
- `admin-ui/src/components/ui/table.tsx` — 新增 Table 组件

## 验证清单

- [x] 689 个 Rust 测试全部通过
- [x] 前端 TypeScript 编译通过
- [x] `normalized()` 在 JSONL 载入路径调用
- [x] `normalized()` 在 trace 读取路径调用
- [x] v1 记录在测试中验证归一化正确
- [x] 混合流量测试验证配对隔离
- [x] 四引擎在计费对比表中正确显示

## 后续工作

无。本次改造已完成所有目标：
1. ✅ 修复 v1 的配对错配问题
2. ✅ 引擎 C/D 进计费对比表
3. ✅ 历史数据无缝迁移
4. ✅ 前端展示逐引擎对比
5. ✅ 内存占用优化
