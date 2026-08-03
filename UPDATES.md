# 更新记录（fork 侧）

> 只记 `kiro-rs-lyozc` 相对上游 [kiro.rs](https://github.com/ZyphrZero/kiro.rs) 的增量。
> 上游自身的版本历史见 [CHANGELOG.md](CHANGELOG.md) —— 那个文件跟着上游走，本文件不动它，
> 避免 `git pull upstream` 时反复冲突。
>
> 功能层面的完整说明见 [FEATURES.md](FEATURES.md) 与 [新增功能汇总.md](新增功能汇总.md)。
> 本文件只记「哪一次改了什么、为什么」。

---

## 2026-08-04 — `426d788`

主题：**上游直通改为转发客户端原始字节**（修一个会让上游 400 的真实故障），
**四引擎倍率存储从千分比整数改为 f64 位型**（修极小倍率被量化成 0 的计费 bug），
以及一轮死代码与不生效代码的清理。

`cargo test` 702 passed / 0 failed；`cargo check --all-targets` 零 warning。

### 🔴 修复 — 上游直通注入了客户端没发的字段，导致上游 400

**现象**：客户端（Claude Code）经上游 API 凭据转发时，上游返回

```json
{"error":{"message":"thinking.budget_tokens is not supported when thinking.type is disabled","type":"invalid_request_error"}}
```

**成因**：直通路径此前转发的是**解析后结构体的再次序列化结果**，而非客户端原文。
[`MessagesRequest`](src/anthropic/types.rs) 上多个字段挂了 `#[serde(default = "…")]`，
其中 `Thinking::budget_tokens` 是**裸 `i32`**（不是 `Option<i32>`）默认 20000。裸字段的
后果是：反序列化完成后，「客户端没发这个键」与「客户端显式发了默认值」在内存里**不可区分**。
于是客户端只发了 `{"thinking":{"type":"disabled"}}`，再序列化后变成
`{"thinking":{"type":"disabled","budget_tokens":20000}}` —— 上游看到 `disabled` 却带着预算，
按其校验规则直接拒绝。同类问题还有 `system` / `tools` 等被写成显式 `null`。

**修复**：新增 [`RawBody`](src/anthropic/middleware.rs#L47) 扩展 +
[`capture_raw_body`](src/anthropic/middleware.rs#L198) 中间件，把请求体原文缓存进 request
extensions，直通路径直接转发这份字节。

- 中间件**只挂在 `/v1/messages` 与 `/cc/v1/messages`** 两条路径上
  （[router.rs:122](src/anthropic/router.rs#L122) / [router.rs:137](src/anthropic/router.rs#L137)），
  因为只有它们会命中上游凭据直通。
- 挂在 `auth_middleware` **内层**：未认证的请求不会被缓冲 body，否则任何人都能让服务端
  为一个 50MB 的垃圾请求分配内存。
- 缓冲上限复用路由的 `MAX_BODY_SIZE`（[router.rs:30](src/anthropic/router.rs#L30)，
  已提到 `pub(crate)`），两处必须同一个值，否则中间件与 `DefaultBodyLimit` 会给出两种
  不同的拒绝行为。
- 取不到原文时（中间件未挂载 / body 非 UTF-8）**降级回旧的往返序列化**并打 warn 日志，
  不让请求直接失败。

**随之而来的回归，以及它的修复**：去掉往返序列化后，`max_tokens` 也不再被自动补上 ——
而它是 Anthropic Messages API 的**必填**字段。客户端不发时上游会以
`max_tokens: field required` 拒掉。故新增
[`ensure_max_tokens`](src/anthropic/handlers.rs#L622)：

- **只补 `max_tokens` 这一个字段**，它是 API 强制要求的最小集。
- 原文已带该键时**原样返回、逐字节不变** —— 这是绝大多数请求走的路径。
- 只有确实需要注入时才重新序列化。此时键序会被 `serde_json` 的 `BTreeMap` 重排
  （本 crate 未开 `preserve_order`）；JSON 对象键序无语义，可接受。
- 非 JSON 对象的 body 原样转发，交给上游报错，不在这里加工。

**测试**：

| 测试 | 位置 | 钉住什么 |
|---|---|---|
| `forwards_client_bytes_verbatim` | [provider.rs](src/kiro/provider.rs) | 起真实 `TcpListener` 当 mock 上游，断言收到的 body 与客户端发的逐字节相同、无 `budget_tokens`、无显式 `null` |
| `injects_only_max_tokens_when_client_omits_it` | 同上 | 缺 `max_tokens` 时只多这一个键 |
| `roundtrip_materializes_serde_defaults` | [types.rs](src/anthropic/types.rs) | **反向**钉住：往返序列化确实会注入 —— 这条测试是 `RawBody` 存在的理由，删掉 `RawBody` 时它会提醒你为什么不能删 |
| `ensure_max_tokens_leaves_body_untouched_when_present` 等 3 条 | [handlers.rs](src/anthropic/handlers.rs) | 注入行为的三种情形 |

mock 上游是手写的 `TcpListener`，没引新依赖。有个坑记在这：reqwest 默认开 keep-alive，
mock 端读 socket 到 EOF 会**永久挂住** —— 必须解析 `content-length` 按长度读。

### 🔴 修复 — 极小倍率被存储格式压成 0，客户端 token 全归零

**现象**：把某项倍率配成 `0.0004`，配置校验放行、admin 回读也显示 `0.0004`，但请求路径
实际乘的是 **0** —— 客户端 usage 该项归零。无报错、无日志。

**成因**：四个引擎的倍率都以**千分比整数**存进 `AtomicI64`（`(v * 1000.0).round() as i64`）。
`sanitize_multiplier` 只要求 `is_finite() && > 0.0`，**无下限**，于是任何 `< 0.0005` 的
合法正倍率都被 `round()` 成 0。超过 3 位小数的值也会静默丢精度。

**修复**：四引擎全部改成 **f64 位型**存储（`to_bits` / `from_bits`）。

| 引擎 | 文件 | 说明 |
|---|---|---|
| A `rust` | [cache_metering.rs](src/anthropic/cache_metering.rs) | 字段改 `*_bits`；`MULTIPLIER_UNSET = -1` 保留 |
| B `go` | [cache_metering_go.rs](src/anthropic/cache_metering_go.rs) | 同上；新增 `pub(super) const ONE_BITS`（`1.0f64.to_bits()`，const 求值）作默认值 |
| C `real` / D `nocache` | [cache_engine.rs](src/anthropic/cache_engine.rs) | `StatelessMultipliers` 六个字段一并改，复用 B 的 `ONE_BITS` |

不选「毫倍率加下限」的原因：加下限只是把断点从 0.0005 推到某个更小的值，量化本身还在。
位型是原值往返，没有断点。

引擎 A 的 `-1` 哨兵在位型下仍然安全：`-1 as u64` 是**负 NaN 的位型**，而
`sanitize_opt_multiplier` 要求 `is_finite() && > 0.0`，任何合法倍率都不可能产生这个位型。
换句话说哨兵与合法值的值域天然不相交，不是靠约定回避。

引擎 B 的 `max_ratio_millis` 与 `ttl_ms` **保持整数** —— 那两个本来就是整数量纲，
不是倍率。

**测试**（两条都要求**精确相等**，不是 `assert_ne!(_, 0.0)`）：

- [`tiny_multipliers_survive_storage_roundtrip`](src/anthropic/cache_metering.rs) —— 引擎 A，
  顺带钉住「未设置 → 回落全局」与「设成极小值」两种状态可区分
- [`tiny_multipliers_survive_storage_on_all_engines`](src/anthropic/cache_engine.rs) —— B / C / D

为什么必须精确相等：只钉「非 0」的话，把千分比换成百万分比一样能通过，而百万分比只是把
断点从 0.0005 推到 0.0000005。要求原值往返才排除掉整个「定点量化」这类实现。

### 🔴 修复 — 3 处 `drop(inner)` 从未真正解锁

[cache_metering_go.rs](src/anthropic/cache_metering_go.rs) 的 `compute_for_account` 里有三处
`drop(inner)`，原意是在做原子计数与构造返回值之前提前释放 `Mutex`。但 `inner` 的类型是
`&mut TrackerInner`（从 `MutexGuard` 借出的引用），**drop 一个引用什么都不做** ——
锁一直持到函数返回。rustc 的 `dropping_references` lint 一直在报。

改成 `drop(all)`（guard 本体）。NLL 下这是合法的：那三个位置之后 `inner` 都不再被使用，
借用已经结束。原本想要的提前解锁现在真的生效了。

### 🟡 澄清 — 引擎倍率与计费系数的叠乘是有意的（未改行为）

`billing.rustMultiplier` 这类系数确实叠在 token 倍率之上：`client_usage` 在记录时已经乘过
token 倍率，成本视图再乘一次引擎系数。配 2× token 倍率 + 1.5× 计费系数，账面得 3×。

判定为有意，依据是三处独立信号：`engine_multiplier` 的 doc 明写「这**不是**下发给客户端的
token 膨胀倍率……而是计费对比视图专用的二次调节」；调用点注释同义；
`billing_query_pairs_upstream_with_client_per_engine` 的算术早就把叠乘钉住了。

所以**没改数**，只补了一条把意图写明的测试：
[`engine_multiplier_scales_cost_only_and_stacks_on_token_multiplier`](src/admin/usage_stats.rs) ——
同时钉住「只影响成本、不影响 `client_tokens`」与叠乘结果本身。

> 若本意是同一个旋钮、不该叠：改动是 `cost(pair.client, price, client_mul)` 传 `1.0`，
> 加删掉那条测试。这会直接改变历史数据的账面口径，故未擅自改。

### 🧹 清理 — 死代码与不一致的重复实现

删除：

| 符号 | 原位置 | 为什么删 |
|---|---|---|
| `resolved_multipliers` | `config.rs` | 与 `usage_multipliers` 分叉的重复实现，无调用方 |
| `UsageMode::from_kind` | `cache_engine.rs` | **与 `begin()` 的内联版本语义不一致**：内联版带「无隔离种子时 Simulated→Real 降级」，`from_kind` 无条件返回 `Simulated`。删掉死的那份就消掉了分叉 —— 留着迟早有人调它 |
| `CacheEngineKind::is_stateful` | `cache_engine.rs` | 无调用方 |
| `EngineBillingPair::merge` | `usage_stats.rs` | 无调用方 |
| `parse_ttl`（模块级函数） | `cache_metering.rs` | 薄壳，只是把缺省值写死成 `DEFAULT_TTL_SECS`；测试改调 `parse_ttl_with_default` |
| `value_has_cache_control` | `cache_metering_go.rs` | Go 移植时带过来的，生产路径没用上 |
| `set_inflation_multipliers`（非 split 版） | `stream.rs` | 四引擎全改用分列倍率后无调用方。留着会让人误以为「creation 与 read 相等」是某个引擎的语义 |
| `inflate_usage_in_json` + `inflate_usage_obj` | `upstream.rs` | 死代码。**顺带更正一处此前的错误说法**：非流式上游路径**不**经这两个函数，实际改写是 `handle_upstream_non_stream_response` 里的内联 usage 重建 |
| `handle_upstream_stream_response` | `upstream.rs` | doc 说「保留用于降级」，实际无调用方；生效的是 `_with_inflation` 版 |

`GoCacheTracker::compute` / `update` 标 `#[cfg(test)]` 而非删除 —— 它们是 Go 侧
`Compute`/`Update` 的对照壳（`*_for_account(0, …)` 的薄壳），36 处测试在用。

另修 [cache_engine.rs](src/anthropic/cache_engine.rs) 一处 `#[test]` 重复属性：
一段 doc 注释连带 `#[test]` 被落在了另一个测试头上，注释描述的其实是
`stateless_multipliers_hot_reload_reaches_begin`。已归位。

### 📄 文档

- 修正 FEATURES.md 与 新增功能汇总.md 里「用 `-1` **毫倍率**作哨兵」的表述 —— 现在是 f64 位型
- 补上直通路径「转发原始字节」的说明（此前写的是「原样转发」，但实际经过往返序列化，
  与实现不符）
- 修正 FEATURES.md 里「`admin-ui/dist/` 是提交进仓库的」—— 它在 `.gitignore` 里，
  未被跟踪；Docker 构建时由 frontend-builder 阶段现场产出

---

## 部署

服务器侧（`docker-compose.fork.yml` 只存在于服务器上，未入库）：

```bash
cd /opt/kiro-rs-lyozc-8991
git pull origin main
docker compose -p kiro8991 -f docker-compose.fork.yml build
docker compose -p kiro8991 -f docker-compose.fork.yml up -d
docker compose -p kiro8991 -f docker-compose.fork.yml ps
```

推送前跑过：

- `cargo test` —— 702 passed / 0 failed
- `cargo check --all-targets` —— 零 warning
- `cargo build --release --no-default-features` —— 与 [Dockerfile](Dockerfile#L18) 同参，通过
- 前端 `tsc -b` + `vite build` —— 通过（本机无 bun，用 `node_modules` 里的 vite 跑，
  产物形状与 Docker 内一致）

**Rust 版本**：Dockerfile 用 `rust:1.92-alpine`。本次新增的东西里最"新"的是 const 上下文的
`f64::to_bits()`（1.83 起 const 稳定），edition 2024 本来就在用（1.85 起），所以 1.92 够用。
未在 1.92 上实跑验证，是按特性稳定版本推的。
