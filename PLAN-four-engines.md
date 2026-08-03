# 四引擎 + 上游/客户端计费一一对应

## 目标

1. 缓存引擎从 2 套扩到 4 套，按 API Key 选择
2. **每个引擎的上游真实计费与客户端模拟计费一一对应**，可分别查看
3. 所有引擎的倍率等参数集中在一个设置窗口

---

## 一、引擎分类

| | 引擎 | 客户端 cache 来源 | 倍率 | 指纹表 |
|---|---|---|---|---|
| A | `rust` | 模拟（session/Key 隔离） | 全局 3 个 | 有 |
| B | `go` | 模拟（account 共享） | 自己 3 个 | 有 |
| **C** | `real` | 上游真实值 | **自己 4 个** | 无 |
| **D** | `nocache` | 恒 0，token 并入 input | **自己 2 个** | 无 |

C/D 无状态：`begin()` 返回 `PendingCache::None`，无 TTL、无 commit、无淘汰。

### D 的合并语义（已确认）

上游返回 `input=800, cc=120, cr=400` → 客户端见 `input=1320, cc=0, cr=0`（总量守恒）。

### C/D 在 Kiro 路径上等价

Kiro 上游不下发 cache 字段，故 C 的"真实值"与 D 的"合并值"都等于 `(total, 0, 0)`。
**两者的差异只在上游凭据路径体现。** 这是刻意的，不是缺陷。

---

## 二、`UsageMode`：替换 `is_simulated()` 二分支

当前 `upstream.rs` 用 `cache_usage.is_simulated()` 做二选一。四引擎需要三分支：

```rust
pub enum UsageMode {
    Simulated,  // A/B → split_against_total()
    Real,       // C   → 上游真实值原样
    NoCache,    // D   → input = input+cc+cr, cache 归零
}
```

由 `begin()` 与 `CacheUsage`、`UsageMultipliers` 一并返回。

**P1 修复并入此模型**：引擎 A 在 `key_id=0` 且无 session 时 `prompt_total_est == 0`
→ 降级为 `UsageMode::Real`。上次的修复成为 `Simulated → Real` 的降级规则，不是被替换。

---

## 三、计费 schema：修正已存在的错配

### 当前的错误

```
upstream_usage  ← 所有引擎的上游真实值混在一起累加
rust_usage      ← 只有 A 请求的客户端计费
go_usage        ← 只有 B 请求的客户端计费
```

混合流量下 `rust_cost` vs `upstream_cost` 是「A 的客户端计费」对比「A+B 的上游计费」——
分母被污染。这正是要修的问题。

### 新设计：按引擎配对，独立稀疏表

`BucketStats` **移除** `upstream_usage` / `rust_usage` / `go_usage` / `upstream_calls`，
改为 `BucketEntry` 上的独立稀疏映射：

```rust
#[derive(Clone, Copy, Default)]
struct EngineBillingPair {
    upstream: TokenUsageBreakdown,  // 该引擎请求的上游真实消耗
    client:   TokenUsageBreakdown,  // 该引擎请求的客户端计费
    calls:    u64,
}

struct BucketEntry {
    // ...既有字段不变...
    /// 仅上游请求填充。键 = (engine, credential_id, model)
    billing: HashMap<(CacheEngineKind, u64, String), EngineBillingPair>,
    /// 同上，多一层 client key 以支持按 Key 过滤
    billing_by_key: HashMap<u64, HashMap<(CacheEngineKind, u64, String), EngineBillingPair>>,
}
```

**副作用是收益**：`BucketStats` 从 ~160 B 降到 ~56 B。它被复制到 8 张表
（含 `by_key_credential_model` 的 K×C×M 项），而计费字段只有 2 张表用得到。
按 K=10/C=20/M=8/744 小时桶估算，聚合器常驻内存从 ~248 MB 降到 ~87 MB。

新增的 `billing` 表是稀疏的——只有上游凭据请求才写入，多数部署上游账号数很少。

### `UsageRecord`（JSONL）

```rust
pub engine: CacheEngineKind,                    // 新增
pub upstream_usage: Option<TokenUsageBreakdown>, // 保留（上游真实）
pub client_usage: Option<TokenUsageBreakdown>,   // 新增（客户端计费）
// rust_usage / go_usage：保留字段以读回历史 JSONL，新记录不再写
```

读回历史行时：`client_usage` 缺失则回退 `rust_usage ?? go_usage`，并据非空者推断 engine。

### `trace_db`（SQLite）

沿用既有 `ALTER TABLE ADD COLUMN IF NOT EXISTS` 机制，加两列：
`engine TEXT`、`client_usage TEXT`。旧列 `rust_usage`/`go_usage` 保留（SQLite 不便 DROP），
读取时同上回退。老库自动迁移，无需手工操作。

### API 响应

```rust
pub struct EngineBillingRow {
    pub engine: String,          // "rust" | "go" | "real" | "nocache"
    pub upstream_cost: f64,      // 上游真实 $
    pub client_cost: f64,        // 客户端计费 $
    pub upstream_tokens: u64,
    pub client_tokens: u64,
    pub calls: u64,
}

pub struct BillingComparisonResponse {
    pub points: Vec<BillingUsagePoint>,  // 趋势：上游总额 vs 客户端总额
    pub engines: Vec<EngineBillingRow>,  // 每引擎配对（新）
    pub upstream_cost: f64,              // 总计，保留
    pub client_cost: f64,
    pub calls: u64,
}
```

趋势图保留「上游总额 vs 客户端总额」两条线；每引擎明细走表格（8 条线不可读）。
将来若要每引擎趋势线，是纯增量改动。

`BillingConfig` 的 source multiplier 从 2 个（rust/go）扩到 4 个。

---

## 四、配置

```rust
pub struct CacheEngineRealConfig {      // C：4 倍率
    input_multiplier, output_multiplier,
    cache_read_multiplier, cache_creation_multiplier,
}
pub struct CacheEngineNoCacheConfig {   // D：2 倍率（cache 恒 0，无需倍率）
    input_multiplier, output_multiplier,
}
```

均带 `sanitized()`，与既有两套同构。`#[serde(default)]` → 老 `config.json` 不需迁移。

**全局倍率保留双入口**：`/config/token-inflation` 端点与顶栏入口不动（不破坏现有用法）；
新对话框把三个全局倍率纳入自己的 payload，一次性落盘。同一份存储，两个入口。

---

## 五、UI

**`cache-engine-dialog.tsx`** — 你要的"一个设置窗口"：
- 2×2 四面板（A/B/C/D），每板含各自参数与倍率
- 顶部全局倍率区（供 A 使用）
- 底部运行计数器（仅 A/B 有，C/D 无状态故不显示）

**`billing-comparison.tsx`** — 每引擎配对表：

| 引擎 | 上游真实 | 客户端计费 | 差额 | 倍数 | 调用 |
|---|---|---|---|---|---|
| rust | $1.2340 | $3.7020 | +$2.4680 | 3.00× | 152 |
| go | $0.8800 | $2.6400 | +$1.7600 | 3.00× | 97 |
| real | $2.1000 | $2.1000 | $0.0000 | 1.00× | 40 |
| nocache | $0.5000 | $1.5000 | +$1.0000 | 3.00× | 12 |

**`client-keys-page.tsx`** — 四引擎选择按钮 + 列表徽章。

---

## 六、改动清单

**Rust（10）**
1. `cache_engine.rs` — 2 个枚举变体、`UsageMode`、`UsageMultipliers` 2 变体、`begin()` 分支
2. `model/config.rs` — 2 个 config struct + `sanitized()` + `Config` 字段 + BillingConfig 扩 4 倍率
3. `upstream.rs` — 3 处改按 `UsageMode` 三分支（非流式 / `inflate_sse_event` / `update_stream_stats`）
4. `anthropic/handlers.rs` — 4 处 `begin()` 传递 mode；`scaled_billing_usage` 三分支；record 带 engine
5. `admin/usage_stats.rs` — `EngineBillingPair`、稀疏表、`BucketStats` 瘦身、`query_billing` 重写
6. `admin/trace_db.rs` — 加 2 列 + 回退读取
7. `admin/types.rs` — `EngineBillingRow`、响应重塑、config payload 扩展
8. `admin/handlers.rs` — get/set 新字段、trace 响应带 engine
9. `token_manager.rs` — `persist_cache_engines_config` 签名扩展
10. `client_keys.rs` — 无需改（`is_default()` 仍成立，默认仍 `Rust`）

**TypeScript（4）**
11. `api/credentials.ts` + `types/api.ts` — 类型
12. `cache-engine-dialog.tsx` — 四面板 + 全局倍率
13. `billing-comparison.tsx` — 每引擎配对表
14. `client-keys-page.tsx` — 四引擎按钮 + 徽章

---

## 七、执行顺序

1. **配置层** — config struct + sanitized + 测试
2. **引擎层** — 枚举 + UsageMode + begin 分支 + 测试
3. **计费管道** — upstream.rs 三分支 + handlers + 测试（含 D 的合并语义、C 的真实值）
4. **统计 schema** — EngineBillingPair + 稀疏表 + query_billing 重写 + 测试（含混合流量配对正确性）
5. **持久化** — trace_db 迁移 + JSONL 回退读取 + 测试（老数据读回）
6. **Admin API** — types + handlers
7. **前端** — TS 类型 → 三个组件 → `npm run build`
8. **全量验证** — `cargo test` + 手工核对四引擎各自的配对数字

每步跑 `cargo test` 后再进下一步。

---

## 八、需要你知道的两点

**① 前端需 npm 构建。** `admin-ui/dist/` 提交进仓库并由 `rust-embed` 嵌入，改 TSX 后
必须构建否则运行时仍是旧界面。若环境无依赖需先 `npm install`。

**② 计费对比仅统计上游凭据请求。** 沿用现状（Kiro 凭据无上游美元成本，无对比意义）。
故走 Kiro 凭据的 Key 在每引擎配对表里不出现——与当前行为一致。

---

## 九、向后兼容

- 默认引擎仍 `Rust`，`skip_serializing_if` 不变 → 老 `client_api_keys.json` 读回不变
- 新 config 段缺失走 `#[serde(default)]` → 老 `config.json` 不需迁移
- 老 JSONL 的 `rust_usage`/`go_usage` 可读回并推断 engine
- 老 traces.db 自动 ALTER TABLE 加列
- A/B 的算法、TTL、指纹表一律不动
