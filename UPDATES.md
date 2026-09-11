# 更新记录（fork 侧）

> 只记 `kiro-rs-lyozc` 相对上游 [kiro.rs](https://github.com/ZyphrZero/kiro.rs) 的增量。
> 上游自身的版本历史见 [CHANGELOG.md](CHANGELOG.md) —— 那个文件跟着上游走，本文件不动它，
> 避免 `git pull upstream` 时反复冲突。
>
> 功能层面的完整说明见 [FEATURES.md](FEATURES.md) 与 [新增功能汇总.md](新增功能汇总.md)。
> 本文件只记「哪一次改了什么、为什么」。

---

## 2026-08-13 — 上游凭据补齐自动切号（欠费/封禁文案检测 + 429 冷却）

**问题**：Kiro 凭据有完整的故障分类（402 额度禁用、403 封禁文案立即禁用、429 风控冷却切号），
上游直通凭据只有 401/403 攒失败计数这一条。两个实际后果：

1. **欠费 key 打死请求且永不出局**。中转站欠费多以 400 返回（Anthropic 官方是
   `credit balance is too low`），而上游分支对 400 直接 `bail!` —— 不切号、不计失败、
   不禁用。没余额的 key 会让路由到它的每个请求都失败，还一直留在池子里。
2. **429 不切号**。上游 429 只在原凭据上退避重试（priority 模式下 `current_id` 不变，
   下一轮还是它），直到耗尽 `MAX_TOTAL_RETRIES` 把 429 抛给客户端 —— 池里明明还有
   别的 key。

**方案**：`config.json` 新增 `upstreamFailover` 段（`model/config.rs::UpstreamFailoverConfig`）。
上游多为各家中转站、文案随时会变，故检测规则做成**可配置子串列表**（大小写不敏感，
非正则），三类动作全部复用凭据池现成机制：

- 封禁文案（401/403 命中 `suspendedPatterns`）→ `report_suspended_for_request`：
  立即禁用、不参与自愈。**判定先于欠费** —— 封禁文案里常同时出现额度字样，后判会把
  封号降级成欠费，禁用原因就错了。
- 欠费文案（400/401/402/403 命中 `quotaPatterns`）→ `report_quota_exhausted_for_request`。
  429 刻意不在匹配范围：其语义是限流、多为临时态，禁死会误杀能恢复的 key。
- 429 → `report_account_throttled_for_request` 冷却当前凭据并立即换号；冷却时长优先取
  `Retry-After`（delta 秒与 HTTP 日期都支持，过期日期回退配置值），缺省 60s ——
  比 Kiro 风控的 30 分钟短得多，中转站 429 多为短时限流，长冷却会让池子无谓萎缩。
  无其它可用凭据时把带 Retry-After 的 429 交还客户端遵守。

未命中文案的普通 401/403（攒 3 次禁用）与普通 400（直接终止）语义不变；
`enabled: false` 整段关闭回退旧行为；字段缺省走 `#[serde(default)]`，老配置零迁移。

**Admin API 热更新**：`GET/PUT /api/admin/config/upstream-failover`（与
`/config/account-throttle` 同模式）。运行时配置放 `MultiTokenManager` 的
`Mutex<UpstreamFailoverConfig>`（含 `Vec<String>` 文案列表，拆不成原子量；读取只在
上游请求**失败**的错误路径发生，锁竞争可忽略），provider 每次错误处理时取运行时
副本而非启动 config 快照。PUT 是 patch 语义：未提供字段不动，列表传 `[]` = 清空
该类检测；写入前校验（冷却 1..=86400 秒；文案 trim 后过滤空白项、拒绝 <4 字节的
过短子串 —— "ban" 会命中 "banner" 这类无辜消息）；持久化失败回滚内存值。

**Admin UI 编辑界面**：设置齿轮菜单「调度与限流」下新增「上游凭据故障转移」入口
（`admin-ui/src/components/upstream-failover-dialog.tsx`）。总开关 + 两个按行编辑的
文案列表（欠费 / 封禁）+ 429 冷却切号开关与冷却秒数；前端预校验与后端同规则
（文案 ≥4 UTF-8 字节、冷却 1..=86400 秒），保存整份提交、热生效。配置仅在弹窗
打开时拉取，不产生常驻轮询。

测试：`config.rs` 5 条（默认值 / 大小写与中英文命中 / 空 pattern 防恒真 / 总开关 /
自定义规则覆盖默认），`provider.rs` 4 条 mock 上游集成（欠费 400 禁用切号、封禁 403
立即禁用、429 冷却切号取 Retry-After、普通 400 不切号）+ 2 条 `retry_after_secs` 单测，
`token_manager.rs` 2 条（热更新 patch 语义 / 非法输入拒绝且不部分生效）。
mock 踩了一个坑：错误 mock 若不按 content-length 读完请求体就关 socket，会触发 RST
把已写的响应冲掉（走系统代理时表现为 502），故与成功 mock 一样先读完再响应。
`cargo test` **793 passed / 0 failed**。

---

## 2026-08-09 — 合并上游：opus-5 纳入 1M 上下文窗口

上游 [PR #61](https://github.com/ZyphrZero/kiro.rs/pull/61)（`eb309f7`），改一个文件、
一个函数。**自动合并零冲突**。

**修的是什么**：`get_context_window_size` 的 1M 模型名单里漏了 `claude-opus-5`。名单里
已有 opus-4.6/4.7/4.8、sonnet-4.6/4.8/5、fable-5，opus-5 掉了。

**后果不在请求侧，在 usage 侧**：该返回值用于 `Event::ContextUsage` 把上游只回报的
**百分比**换算成 token 数（`pct × window / 100`）。漏配 → 按 200k 而非 1M 换算 →
**usage 上报缩小 5 倍** → 客户端的上下文进度条与自动压缩阈值全部失准（实际用了 50 万
token，显示成 10 万，压缩迟迟不触发）。发往上游的请求不受影响。

**与本 fork 的关系**：我们在用 `claude-opus-5`，所以这个修复直接相关。之前那次
`Input is too long` 里，压缩不及时可能是其中一环 —— 但不是直接成因（那个报错的根因是
上游转换后载荷超 Kiro 单次请求体上限，见 2026-08-04 那节）。

**为什么没冲突**：上游改的是该函数的 match 分支，我方在同一函数的改动
（`custom_models::lookup` 优先返回，位于第 286 / 305 行）在其**上方**，两处不重叠。
合并后已核实我方那段仍在。

上游同时补了两个回归测试 —— `test_context_window_opus_5`（含别名 / 后缀变体，以及
「`opus-4-5` 不得被误匹配为 `opus-5`」）与 `test_context_window_1m_family`（整个 1M 名单
的正反校验）。后者是防再次漏配的：新增 1M 模型时若忘了同步名单，它会失败。

验证：`cargo test` **767 passed / 0 failed**（合并前 744 + 上游新增），
`cargo check --all-targets` 零 warning。

---

## 2026-08-09 — 上游凭据的 WebSearch 无返回（SSE 被当 JSON 缓冲）

**现象**：上游 API 项目本身支持 Claude Code 的 WebSearch，但接到 kiro-rs 作为上游后
WebSearch 不返回结果。

**根因**：上游 WebSearch 直通分支把响应当**完整 JSON** 处理 —— `bytes().await` 整体
缓冲，再把 `content-type` 硬写成 `application/json`。而 Claude Code 的 WebSearch 请求
带 `stream: true`，上游回的是 SSE。客户端按 `application/json` 去解析一段 SSE 文本，
什么都取不出来。

反向验证（把修复条件临时改成 `false` 重跑同一套断言）给出了精确判据：
`content-type=application/json`、`chunks=1`（一次收完而非逐事件），**而 SSE 文本仍在
body 里**。所以不是内容丢失，是包装错了 —— 这也解释了为什么表现为「静默无结果」
而不是报错。

对比：普通聊天的上游直通走
[`handle_upstream_stream_response_with_inflation`](src/anthropic/upstream.rs#L397)，
是正确的流式转发。WebSearch 分支绕过了它、自己手写了一个只处理非流式的版本。
kiro2cc-proxy 没有这个「按凭据类型分叉」的逻辑，所以不受影响。

**修复**：按 `payload.stream` 分流，复用普通上游直通已在用的那两个函数，不再自己拼响应。

**两处并行路径**：`/v1/messages` 与其 beta 变体各写了一份等价的 WebSearch 派发逻辑。
若只改一处，另一处会继续沿用旧行为 —— 已抽成 `dispatch_websearch` 共用，避免再次分叉。

**顺带修好计费**：这条路径原先既不应用膨胀倍率、也不回报上游真实 usage
（`begin()` 返回的 `multipliers` / `mode` 被 `_` 丢弃了）。现在与普通上游直通同口径，
`credential_id` 也记真实值而非 0。

**实测**（mock 上游 + 15 条断言，脚本已删）：流式回 `text/event-stream`、6 个 chunk
逐事件到达、`message_start` / `content_block_delta` / `message_stop` 齐全、搜索内容
送达客户端；非流式回 `application/json` 且 `usage` 存在；普通聊天回归未受影响。

---

## 2026-08-09 — 客户端 Key 支持回看明文

**问题**：Key 明文只在创建/轮换的那一次弹窗里出现，关掉就再也看不到。列表里只有
`sk-5dasy...fgpB` 这样的脱敏值，而下游要配的是完整明文 —— 忘记保存就只能重新生成，
逼着下游跟着换一遍，纯属自找的麻烦。

明文本来就以**原值**存在 `client_api_keys.json`（鉴权要按原值做常量时间比较，不做 hash，
见 `client_keys.rs` 模块注释）。所以"看不到"不是安全设计，只是界面没给入口。

**做法**：明文不塞进列表接口，另开一个按需端点。

- 后端：`ClientKeyManager::get(id)` 取整条快照 + `GET /client-keys/{id}/reveal`。
  落一条 `tracing::info!` 审计（只记 `key_id` / `name`，**不记明文**），
  日后查明文外泄时能知道何时被读过。
- 前端：点列表 Key 列 → 下拉多一项「查看 Key 明文」，复用原来的明文弹窗
  （带显示/隐藏 + 复制）。弹窗按 `created` / `rotated` / `revealed` 三态切文案。

**为什么不直接把明文加进列表接口**：列表每 30s 轮询一次，那等于把所有 Key 的明文
反复送上网络、并常驻前端的 query cache。按需单取则只在用户点击时传一条，也才有
审计点可埋。

**实测**（临时实例 + 27 条断言，脚本已删）：reveal 回的明文与 create 给的一致；
轮换后 reveal 只给新值、不给旧值（这条最关键 —— 若读到陈旧快照，用户拿去配下游会
得到 401 而界面看起来完全正常）；列表响应不含明文、条目无 `key` 字段；
系统 Key（id=0）可回看且等于 `config.json` 的 `apiKey`；未知 id 回 404；
无鉴权/错误管理密钥回 401 且响应不夹带明文；回看到的明文能过入口鉴权
（返回 502 上游错误而非 401）。日志确认无明文泄漏，且有 reveal 审计记录。

### 移除轮换宽限期：旧 Key 现在立即失效

先是发现文案与行为不符（下一小节），改文案时进一步查明：**那 7 天宽限期没有任何逃生门。**
`PREV_KEY_GRACE_DAYS = 7` 的注释里写着「必须配合 `revoke_prev_key` 一起用：宽限是默认
路径，撤销是逃生门」，但 `revoke_prev_key` **根本不存在** —— 全仓库只有三处注释在引用它。

也就是说：明文一旦泄露，轮换之后那把还能再用 7 天，而代码里没有任何提前掐断的手段。
这不是可以权衡的偏好，是个洞。已按「轮换即失效」改掉：

- `rotate` 不再写 `prev_key`，并把旧明文从 `by_key` 判重表摘除（与宽限版的关键差别）
- `verify_and_touch` 只比当前 `key` 一把，删掉整段宽限分支与过期清理逻辑
- `load` 时清零老数据残留的三个宽限字段，并打一条 info 日志。**这是升级路径上唯一
  有风险的地方** —— 若加载后仍保留，一把本应失效的旧明文会继续能进门，而用户以为
  轮换过就安全了
- `PREV_KEY_GRACE_DAYS` 常量删除；三个 `prev_key*` 字段保留但标注为历史遗留、
  不参与鉴权（老 `client_api_keys.json` 里带着这些键，直接删字段不影响反序列化，
  但留着值会随下次落盘写回，让人误以为宽限逻辑还在）

代价是下游必须在轮换后立刻换新明文，否则马上 401。需要平滑迁移就新建一把 Key、
逐个下游切过去、再删旧的那条 —— 全程可控，不依赖一个所有人都看不见的隐式时间窗。

`sync_system_key` 未受影响：它直接覆盖 `entry.key` 且从不设 `prev_key`，本来就是
立即失效。

**测试**：删掉 4 个钉住宽限行为的单测，换成 3 个钉住新行为的 ——
`legacy_grace_fields_are_rejected_and_cleared_on_load`（造一份到期时刻在**未来**的
老 JSON，按旧逻辑那把完全有效，现在必须被拒且字段被清零）、
`every_rotation_invalidates_all_previous_keys`、
`rotation_removes_old_key_from_dedup_index`。

**端到端实测**（真实实例 20 条断言，脚本已删）：预置一份宽限期时代的老文件 →
启动日志出现清理提示 `count=1` → `sk-legacy-old` 被拒 401 而 `sk-legacy-new` 通过；
轮换后旧明文立即 401、连续轮换后每一把旧的都 401；落盘文件不再含 `prevKey` 三字段、
也不含任何已失效明文；reveal 连点三次仍可用（确认没碰坏回看功能）。

### 顺带修正「旧 Key 立即失效」的错误文案

写这轮断言时多加了一条「轮换后旧明文应被拒 401」，结果失败：实际返回 502，
即**鉴权已放行**。查下来不是 bug —— `rotate` 按设计把旧明文转为宽限 Key
（`PREV_KEY_GRACE_DAYS = 7` 天），让下游有时间迁移，应急撤销走 `revoke_prev_key`。
是我的断言写错了。

但顺着查出真问题：UI 和文档共 5 处仍写着「旧 Key 立即失效」。
`client_keys.rs` 的测试注释里明确记着「行为已变更：旧明文进入 7 天宽限期」，
说明当初改行为时漏改了这些文案。

这个错误的方向很危险：**用户以为泄露的 Key 轮换一下就没了，实际它还能用 7 天。**

最初改的是文案（把界面改成如实描述宽限期）。但查证 `revoke_prev_key` 时发现它不存在，
说明宽限期从一开始就缺了配套的撤销手段 —— 于是改为删掉宽限期本身，让行为回到界面
原本描述的样子（见上一小节）。所以这几处文案最终又改回了「立即失效」，只是现在它是真的。

`sync_system_key` 那句「使旧值立即失效」全程未动 —— 它直接覆盖 `entry.key` 且不设
`prev_key`，措辞一直是对的。

---

## 2026-08-09 — 顶栏按钮重叠盖住「请求日志」tab（点不动）

**问题**：宽屏（≥2xl）下工具区把 tab 行盖住，最右的「请求日志」tab 看不见。

**先量后改**。用 CDP 驱动 headless Edge 量了 5 档视口，拿到确切数字：

| 视口 | tab 行右边界 | 工具区左边界 | 重叠 | 日志 tab 可点 |
|---|---|---|---|---|
| 1536 | 656 | 536 | **+120** | ❌ |
| 1600 | 688 | 568 | **+120** | ❌ |
| 1700 | 738 | 618 | **+120** | ❌ |
| 1920 | 848 | 728 | **+120** | ❌ |
| 2560 | 1168 | 1048 | **+120** | ❌ |

两个关键事实：

1. **重叠量恒定 120px，与视口宽度无关** —— 把窗口拉到 2560 也不自愈。
   因为 `HeaderActions` 是 `shrink-0` 且宽 900px，`max-w-[1400px]` 容器里
   `HeaderBrand`（`flex-1 min-w-0`）拿到的剩余空间不够，而 `DesktopTabs` 自身
   **没有 `min-w-0`** —— flex item 默认 `min-width:auto` 让它按内容撑开、溢出父级。
2. `elementFromPoint(日志 tab 中心)` 命中的不是它自己 —— 即**点不动**，不只是看不见。

这个重叠**早于本次订阅源改动**（在 committed 基线上量的），我加的按钮只是让它从
120px 恶化到约 210px。

**修复**（两处，主次分明）：

- **主**：宽屏工具栏从「平铺 6 个带文字的配置按钮」改为「3 个高频图标 + 1 个齿轮菜单」。
  工具区 900px → **289px**。配置项复用既有的 `*CompactItems`（它们本来就是为下拉设计的），
  宽屏齿轮菜单与窄屏 ⋯ 菜单共用同一份 `SettingsMenuItems`，避免两套入口的项目集合漂移。
- **次（护栏）**：`DesktopTabs` 补 `min-w-0 overflow-x-auto`。将来再塞东西也只会变成
  可横向滚动，而不是静默盖住 tab。

**状态可见性怎么保住的**：平铺按钮原本能一眼看到「膨胀中 / 故障转移·30m / 自愈开」。
收进菜单后，① 菜单项自带当前值（实测文案：`关闭故障转移 · 30m`、`凭据自愈 | 已启用`）；
② 齿轮上加一个琥珀色小点，仅在存在非默认配置（膨胀 >1x 或故障转移被关）时显示 ——
不展开也知道有东西被调过。

**修复后复量**：

| 指标 | 修复前 | 修复后 |
|---|---|---|
| `overlapPx`（5 档均值） | +120 | **−412**（余量） |
| 日志 tab `topmostIsSelf` | false | **true** |
| 工具区宽度 | 900px | **289px** |

**功能验证**（真实鼠标事件，非 `.click()`——Radix 监听 `pointerdown`）：
点击「请求日志」→ hash 从 `#/credentials` 切到 `#/traces`，页面渲染出日志面板；
齿轮可点；窄屏（1280/1440）⋯ 菜单 8 组配置齐全、`actionsOverflowsRow: false`、
移动版「日志」tab 可点。截图目视确认 5 个 tab 全部可见。

净删 165 行（移除 8 个不再引用的平铺按钮组件及其专用 helper / 图标 import）。
`cargo test` 761 passed；`tsc -b --force` 零错误（仓库开了 `noUnusedLocals`，
死代码会直接编译失败，正好当清理清单用）。

### 顺带去掉重复的「Token 膨胀倍率」入口

顶栏原先有两个入口改同一份数据：独立的「Token 膨胀倍率」按钮，和「缓存引擎」弹窗里的
`global` 段。后端两条路径都落到 `token_manager.set_token_inflation_config`，校验区间
同为 `[1.0, 100.0]` —— 是同一份存储的两个门，不是两份配置。

已删掉独立入口，四套引擎倍率 + 全局倍率统一在缓存引擎弹窗里调。菜单项改名为
「缓存引擎与下发倍率」，非默认时带「（倍率生效中）」后缀。

`PUT /api/admin/config/token-inflation` 端点**保留**（供直接调 API 的使用者），
只是管理面板不再提供第二个按钮。前端 `useSetTokenInflationConfig` 也留着并加了注释
说明「当前无调用方、保留做接口对等」，免得后来者当死代码删掉又在别处重写。

修的一个连带 bug：`useSetCacheEnginesConfig` 原先只失效 `cacheEnginesConfig` /
`cacheEnginesStats`。全局倍率的编辑入口收敛到这里后，齿轮上的「非默认」状态点
（读 `tokenInflationConfig`）改完不会刷新 —— 已补上该 key 的失效。

实测（CDP + API 双向）：顶栏与齿轮菜单均无独立膨胀入口；从菜单能打开缓存引擎弹窗
（四套引擎 + 全局段齐全）；`PUT /config/cache-engines` 带 `global: {input: 2.5}` 后
`GET /config/token-inflation` 读到 `2.5`，证明确为同一份存储；齿轮状态点从
false → true。测完已复原为 1.0。

---

## 2026-08-09 — API Key 订阅源（拼车链接）自动导入

**需求**：手里有个持续发号的拼车链接（GET 返回带 `ksk_` key 与 `aws_region` 的 JSON），
希望定期拉取、把新 key 自动导入成凭据，不用手工一个个粘。

**没有新增凭据类型**。复用了两套既有机制：
[`import_one_credential`](src/admin/service.rs)（去重 + 验活 + 失败回滚都在里面）
和 `start_*_scheduler` 那套后台轮询 pattern。

### 为什么不需要自己记账「哪些 key 已导入过」

`credential_dedup_fingerprint`（[token_manager.rs:131](src/kiro/token_manager.rs#L131)）
对 api_key 类凭据取 `sha256(kiroApiKey)`，且这道检查在
[`add_credential`](src/kiro/token_manager.rs#L3974) 里发生在**任何网络调用之前**就 bail。

所以轮询天然幂等：20 个存量号位每 5 分钟拉一次，上游开销是 **0 次请求**，
只有真正的新 key 才花一次验活。这是整套方案不需要额外状态文件的原因。

### 字段映射的一个坑

`aws_region` 落到 `apiRegion` 而**不是** `region`。endpoint 层用
`effective_api_region` 拼 `q.{region}.amazonaws.com`
（[ide.rs:26](src/kiro/endpoint/ide.rs#L26)），而 `region` 走的是 OIDC 刷新链路 ——
api_key 凭据根本不刷新 token，填 `region` 只是噪音。

**验活默认开启**。不是为了保险：`supports_opus()` 在 `subscription_title` 为 `None`
时返回 `true`（[credentials.rs:509](src/kiro/model/credentials.rs#L509)），
未验活的 Free key 会被放进 Opus 调度然后失败；验活顺带把订阅等级填上。

### key 是短命的：清理策略

观测到的两条 `warrantyUntil - claimTime` 都正好 2 小时。不处理的话
`credentials.json` 会堆满死 key，调度器持续撞失败、触发自愈 churn。

**没有做「到点即删」** —— 保修期过了 key 未必失效，硬删会浪费还能用的号。
改为交给现有失败计数 / 自愈自然淘汰，再加一道精准善后（`reapDisabled`，默认关闭）：
只删同时满足三条的凭据 —— `sourceChannel` 带 `keyfeed:` 前缀、已禁用、
且禁用原因**不是** `Manual`（手动禁用是运维显式意图，例如留着排查）。

`warrantyUntil` 仍然解析并回传前端展示（含「最早到保」与剩余时长），只是不触发删除。

### 地址完全可配置（零硬编码）

源地址、令牌、令牌参数名全在 `keyFeed` 配置里，代码中不含任何具体域名。换源只改 URL，
只要返回结构一致就能直接用。三个入口都行：Admin UI 弹窗、`PUT /api/admin/config/key-feed`、
直接编辑 `config.json`。

UI 入口在顶栏「设置」齿轮菜单 →「凭据来源 / API Key 订阅源」（窄屏在 ⋯ 菜单同一位置）。
起初这个按钮是平铺在顶栏的，结果撞上顶栏本就存在的布局缺陷 —— 见上一节
「顶栏按钮重叠盖住『请求日志』tab」。

**`tokenParam` 可配（默认 `token`）**。不同源的鉴权参数名不一样
（`token` / `key` / `accessKey`…），做成可配置不是为了"灵活"，而是**让脱敏对任意源都成立**：
只有参数名匹配，才能把令牌拆到独立字段、让 Admin API 只回 `tokenSet`。否则令牌会留在
`url` 里被接口回显（这一点在实测中确认过）。参数名大小写不敏感。

### 安全边界

- 令牌与 URL **分开存**。粘贴完整链接时 `split_url_and_token` 在入口按 `tokenParam`
  拆开；Admin API 只回 `tokenSet` 布尔，从不回显明文（与 `github_token` 同思路）
- reqwest 的错误信息会把请求 URL 原样带上（含令牌），进日志/响应前先过
  `redact_token` 换成 `<REDACTED>`
- URL 强制 http/https 且必须可解析；端点名沿用 `add_credential` 的注册表校验
- `intervalSecs` 有 60 秒硬下限，**落盘前就夹取** —— 若存原始值，将来下限调低会让
  那个值静默变成生效值

### 一个只在"换源"时才会暴露的坑

`split_url_and_token` 原先无条件用 `Url::query_pairs_mut` 重建查询串。实测发现它会按
`application/x-www-form-urlencoded` 重新编码，把 `%20` 改写成 `+`：

```
in : https://other.example.com/fetch?t=xyz&note=hello%20world
out: https://other.example.com/fetch?t=xyz&note=hello+world   ← 变了
```

多数服务器两者都当空格，所以对当前那个源无影响。但只要换成的源在链接里带 HMAC /
签名类参数，任何一个字节的改写都会让签名校验失败 —— 而且表现为"换了个地址就一直 401"，
极难定位到是中转层改写了 URL。

修法：**先只检测、不改 URL**，没有目标参数就原样返回输入字符串，一个字节都不碰。
`test_split_url_preserves_bytes_when_no_token_param` 钉住这个行为（含签名参数、`%20`、
大小写、路径里带 `token` 字样等 5 个用例）。做过反向验证：去掉快速路径该测试即失败。

### 端到端验证

临时目录起服务实测（均已清理）。真源部分：

- 首轮拉取 → `imported: 2`，`apiRegion` 分别 `eu-central-1` / `us-east-1`，
  `sourceChannel` 带 `keyfeed:` 前缀，无 `region` 噪音
- 第二轮 → `imported: 0, duplicate: 2`，验证幂等
- reaper 三方鉴别：造 `#1` Manual(keyfeed) / `#2` TooManyFailures(keyfeed) /
  `#3` TooManyFailures(手工添加) → 只删 `#2`
- 坏 token → 回上游原因「无效的领取令牌」，响应内 grep 不到 token
- 非 http scheme / 不可解析 URL / 未注册端点 → 均 400
- 后台调度器自行触发并**持续循环**（`pulledAt` 03:20:27 → 03:21:29），
  且全去重那轮降到 debug 不刷屏

**换源验证**（写了个 mock 源，故意换掉全部三要素：不同 host+port、路径 `/v2/dispense`
而非 `/get`、鉴权参数 `accessKey` 而非 `token`）：

- 指定 `tokenParam: "accessKey"` → `url` 里的密钥被拆走，响应 `tokenSet: true`，
  grep 不到密钥；请求被正确拼回 `?accessKey=`，mock 源返回 200
- `status: "expired"` 的号位被过滤（`total: 3` → `candidates: 2`）
- 新区域 `ap-northeast-1` / `eu-west-1` 正常入库

**向后兼容**：旧 `config.json` 缺 `tokenParam` → 回退 `token`，其余字段（间隔 600 /
验活 true / 优先级 2）原样保留；完全没有 `keyFeed` 段 → 全默认（间隔 300、验活开），
未配置 URL 时拉取干净报「未配置订阅源 URL」而非 panic。

`cargo test` 761 passed / 0 failed（新增 17 个 key_feed 单测）；
`key_feed.rs` clippy 零 warning；前端 `tsc -b` + build 通过。

> **注意**：那个 GET 返回的 `msg` 是「已进拼车队列…正在补货」，带 `claimId`/`seatNo`。
> 无法确定重复调用是纯读列表还是每次占座，故默认间隔 300 秒。若服务方语义是后者，
> 应把间隔调大。

---

## 2026-08-06 — 合并上游 v0.7.5

上游 `ae99f5a..a2f4d7f`，25 个提交，0.7.3 → 0.7.5。**28 处冲突全部人工解决**。

`cargo test` 743 passed / 0 failed（我方 708 + 上游新增 35）；零 warning；
release 构建与前端构建均通过。

### 上游带来的能力

有两条正好打在此前碰到的问题上：

- **403 账号封禁精准识别 + 自愈节流**（0.7.4）—— Kiro 账号被封时，旧逻辑会陷入
  「全禁 → 自愈 → 403 → 再禁」死循环。新增 `suspendedDetectionEnabled` /
  `selfHealEnabled` / 冷却间隔 / 连续轮数上限，并把自愈状态持久化
- **IdC/Enterprise 重新登录**后整体替换与 OIDC 客户端绑定的凭据，修 Token 刷不动

其余：

- 单账号 RPM 主动限流（默认关闭，每凭据独立滑动窗口，耗尽时返回类型化 429 并按
  最早释放的窗口算 `Retry-After`）
- WebSearch MCP 查询参数兼容（`query` / `search_query` / `q` / `queries` 及嵌套
  `text` / `value`），区分「空结果」与「真实失败」
- Admin UI：凭据多字段排序与按状态隐藏、批量删除进度与失败重试、凭据创建时间、
  trace 详情的计费与缓存效率面板、移动端菜单与操作列修复

### 冲突解决

绝大多数是「双方各加了不同东西」的相邻冲突 —— 例如凭据结构上我方加
`is_upstream` / `upstream_base_url`，上游加 `created_at`；构造函数里我方加三个膨胀
倍率，上游加 RPM 与自愈配置。这类一律**两边都留**。

git 在若干处把两组各自完整的函数按公共尾部（`return Err(err); } tracing::info!(`）
切开，需要逐个还原成完整定义，不能简单取一侧。

三处需要判断：

| 位置 | 处理 |
|---|---|
| `credential-card` 操作区 | 上游把拖拽手柄包进 `!dragDisabled`（字段排序时隐藏，避免视觉顺序与服务端优先级混淆），我方在紧随的按钮上加 `!credential.isUpstream`（上游凭据没有 Kiro Token 可刷新）。两个条件互不干扰，都保留 |
| trace 详情两个计费面板 | **刻意并存**：上游 `CacheBillingPanel` 以 credit 为核心，看缓存省了多少钱；我方「单次计费对比」按模型单价 × 引擎倍率算美元，看上游真实成本与客户端被计费额的差距。口径不同，注释里写明分工 |
| `ExpandedDetail` 签名 | 取我方版本（带 `billingConfig`）。函数体已自动合并为同时渲染两个面板 |

### 一处真实语义修复

上游把 `report_success` / `report_failure` 标记 `#[cfg(test)]`，生产路径改用
`*_for_request(id, model, group)` —— 为的是支持**按模型/分组作用域**判断"是否还有
可用凭据"。

我方上游直通路径有 4 处调用旧签名（[provider.rs](src/kiro/provider.rs)），编译期
暴露后改写。这不只是修编译错误：直通路径现在也纳入了该作用域判断，此前用的是全局
口径 —— 意味着"某模型/分组下已无可用凭据"这个判断以前在直通路径上是不准的。

### 版本与文档

`Cargo.toml` 0.7.3 → 0.7.5。`CHANGELOG.md` 取上游版本 —— 那个文件归上游管，
往里加 fork 条目会让每次 `git pull upstream` 在同一位置反复冲突。

---

## 2026-08-06 — 新凭据模型缓存预热（承接下一节，那次修复不充分）

**问题**：上一节的修复上线后，用户报告**仍然不切换** —— 推入高优先级 Kiro 账号后，
请求继续走优先级更低的上游 API 凭据。

**为什么上次没修好**：那次只解决了「指针不动」，但选择逻辑里还有两道关卡把新凭据踢回。

候选排序键是 `(discovery_rank, priority)`
（[token_manager.rs:2063](src/kiro/token_manager.rs#L2063)），而
`discovery_rank = (support != Confirmed)` 排在 **priority 前面**。
`cached_model_support` 对没有缓存条目的凭据返回 `Unknown`
（[token_manager.rs:1683](src/kiro/token_manager.rs#L1683)），于是：

| | 模型缓存 | discovery_rank | priority | 排序键 |
|---|---|---|---|---|
| 上游凭据（用过） | `Confirmed` | 0 | 5 | `(0, 5)` ✅ 胜 |
| 新 Kiro 账号 | 空 → `Unknown` | 1 | 0 | `(1, 0)` |

`(0,5) < (1,0)` —— **优先级根本没参与比较**，第一个键就分出胜负了。

同一个 `Unknown` 还让 priority 短路失效：存在 `Confirmed` 凭据时
（`confirmed_available`），短路要求 `current_id` 指向的凭据也必须 `Confirmed`
（[token_manager.rs:2169](src/kiro/token_manager.rs#L2169)）。

自锁的形状：**不被选中 → 缓存不填 → 一直 `Unknown` → 不被选中**。

**修复**：新增
[`warm_model_cache_for`](src/kiro/token_manager.rs#L1829)，在凭据入库后异步预热模型缓存。
接在三条入口后面 —— Admin 添加（批量导入也走它）、Social 登录、IdC 登录。

两个设计点：

- **后台 spawn 而非 await**：凭据此刻已落盘成功，若 await 且上游不可达，会把「添加成功
  但预热失败」报成「添加失败」，误导调用方。预热失败只降级为按 `Unknown` 走原逻辑
- **预热成功后必须再 `select_highest_priority()`** —— 这一步是写测试时才发现的：
  `acquire` 选中凭据后会**回写** `current_id`。若预热返回前来了一个请求，短路因
  `Unknown` 失败 → 排序选中旧凭据 → **指针被拖回旧凭据**。此后预热虽成功，但短路对
  健康的旧凭据恒命中，优先级永不重估。缺这一步，修复照样无效

**测试**：
[`empty_model_cache_defeats_priority_until_warmed`](src/kiro/token_manager.rs) ——
**请求带具体模型**，这是与上一节测试的关键区别：那三条传 `model = None`，此时所有凭据
一律 `Unknown`、`discovery_rank` 互相抵消，优先级才决定胜负 —— 所以它们当时全绿却没
覆盖真实故障。测试用 `seed_model_cache` 模拟预热效果（真实预热要打上游 HTTP，单测无法
覆盖），并显式断言中途 `current_id` 被拖回旧凭据，钉住「填缓存 + 重选指针」两步都必需。

做过反向验证：注释掉 `select_highest_priority()` 后第二段断言失败（`left: 1, right: 2`）。

> **运维提示**：若预热失败（上游临时不可达），在 Admin 界面对该凭据点一次
> 「查询模型」即可手动填充缓存，优先级随即生效。

---

## 2026-08-06 — 凭据卡片补「本实例消耗的积分」

**问题**：凭据卡片只显示 `balance.remaining`，那是向 Kiro 查的**账号总余额**。
账号多人共用时这个数含别人的消耗，反映不出"我用了多少"。

**现状**：后端早就在算了 —— `credits` 来自上游 `meteringEvent`，逐请求落盘
（[usage_stats.rs:263](src/admin/usage_stats.rs#L263)），按凭据聚合
（[usage_stats.rs:714](src/admin/usage_stats.rs#L714)），已挂在凭据响应上
（[handlers.rs:63](src/admin/handlers.rs#L63)），前端类型里也有
（[api.ts:60](admin-ui/src/types/api.ts#L60)）。唯一缺的是
[credential-card.tsx](admin-ui/src/components/credential-card.tsx#L1202) 那个网格
只渲染了 calls / 输入 / 输出 / 缓存写 / 缓存读五项，把 `credits` 漏了。

**改动**：网格 5 列 → 6 列，「积分」放第一列（它比 token 数更能回答"我花了多少"）。
格式化复用现成的 [`formatCredits`](admin-ui/src/lib/utils.ts#L144) —— 总览页与图表都在用它，
不另写一个。标题从「Token 用量」改成「用量」，因为积分不是 token。

两点口径限制，用这个数时需要知道：

- **窗口是近 7 天**，写死在 [handlers.rs:46](src/admin/handlers.rs#L46) 的 `Range::Last7d`。
  不是账号总消耗。`Range` 枚举只有 `Last24h` / `Last7d` / `Last30d`
  （[usage_stats.rs:407](src/admin/usage_stats.rs#L407)），没有"全部时间"
- **只统计经过本实例的请求**。这正是与账号余额的区别所在；反过来，
  `余额减少量 − 本列积分` 可以估出其他人用掉的部分

真正的"总消耗"需要一个逐凭据累计计数器（仿
[`client_keys.total_credits`](src/admin/client_keys.rs#L49) 那个模式，只增不减、
不受日志保留期裁剪影响）。加 `Range::All` 扫全部日桶不等价 —— 超期桶已被删除，
数字会随时间**变小**，看起来像消耗减少了。该项未做。

---

## 2026-08-06 — priority 模式新增凭据不生效

主题：**修一条「新增的高优先级凭据被无限期跳过」的行为** —— 不是切换慢，是不会切。

`cargo test` 708 passed / 0 failed；`cargo check --all-targets` 零 warning。

### 🔴 修复 — priority 模式下 `add_credential` 不重新评估当前凭据

**现象**：Kiro 账号被封 → 自动降级到上游 API 凭据 → 自动化推入新的 Kiro 账号
（优先级更高）→ **请求仍然全部走上游**，新账号一直不被选中。

**成因**：两处各自合理的逻辑相加后留了个缺口。

- [token_manager.rs:1811-1837](src/kiro/token_manager.rs#L1811-L1837) `acquire` 在 priority
  模式下有一条**短路**：`current_id` 指向的凭据只要还健康（未禁用、未限流、匹配模型与
  分组），就直接用它，**不再与其它凭据比较优先级**
- [token_manager.rs:3345](src/kiro/token_manager.rs#L3345) `add_credential` 只往 `entries`
  里 push，然后持久化返回，**不动 `current_id`**

`select_highest_priority()` 原本只有三个调用点 —— 改优先级
（[2855](src/kiro/token_manager.rs#L2855)）、删凭据（[3663](src/kiro/token_manager.rs#L3663)）、
切负载模式（[3844](src/kiro/token_manager.rs#L3844)）。新增凭据不在其中。

于是那条路径正好卡住：上游凭据**始终健康**，短路每次都命中，指针永远没有重新评估的
机会。只有上游自己出故障才会换 —— 而它不出故障。

**修复**：`add_credential` 成功后，在 priority 模式下调一次 `select_highest_priority()`。

- 与 `set_priority` 的既有行为对齐：改优先级会立刻重选，那么新增一个优先级更高的凭据
  同样应该立刻生效
- **只在 priority 模式下做**：balanced 每次请求都重新均衡选择，本就不读 `current_id`，
  无条件调用会干扰均衡状态
- `select_highest_priority` 自身按 `priority` 取最小值，故新凭据优先级更低时不会抢占

**测试**（三条，覆盖三个不同维度）：

| 测试 | 钉住什么 |
|---|---|
| `added_higher_priority_credential_takes_effect_immediately` | 主回归。刻意让上游凭据**全程健康**并在末尾断言 `!disabled` —— 若测试里把它禁用，短路会因"当前凭据不可用"自然失效，这条修复就测不到了 |
| `added_lower_priority_credential_does_not_preempt_current` | 重新评估仍按优先级取最小值。少了这条，把修复误写成 `*current_id = new_id` 也能让上面那条通过 |
| `adding_credential_in_balanced_mode_leaves_current_id_alone` | balanced 模式不受影响 |

三条都做过**反向验证**：临时把修复短路掉（`if false && ...`）后重跑 —— 主回归如期失败
（`left: 1, right: 2`），另两条仍通过，确认它们钉的是不同维度而非重复覆盖。

---

## 2026-08-04 — Kiro 路径历史 `tool_result` 去重

主题：**修一条会让上游拒掉整个请求、且不可重试的失败** ——
`each tool_use must have a single result`（`TOOL_USE_RESULT_MISMATCH`）。

`cargo test` 705 passed / 0 failed；`cargo check --all-targets` 零 warning。

### 🔴 修复 — 历史里的重复 `tool_result` 直达上游

**现象**：长会话（`messages.24.content.1`）经 **Kiro 凭据**转换后被上游拒：

```json
{"message":"messages.24.content.1: each tool_use must have a single result","reason":"TOOL_USE_RESULT_MISMATCH"}
```

这条 reason 在 [`CLIENT_VALIDATION_REASONS`](src/kiro/endpoint/mod.rs#L174) 里，命中即
**立即终止、不重试** —— 判定"根因在请求体，重试无意义"。所以它不是偶发抖动，同一个会话
每一轮都会失败。

**成因**：`convert_request` 的步骤顺序留了个缺口。

- [converter.rs:717](src/anthropic/converter.rs#L717) `build_history` 先把历史组装完。
  历史里的 `tool_result` 来自 [`merge_user_messages`](src/anthropic/converter.rs#L1704) 的
  `all_tool_results.extend(...)` —— **无条件累加**
- [converter.rs:736](src/anthropic/converter.rs#L736) `validate_tool_pairing` 只校验
  **当前消息**那一份（传进去的是 `last_message` 解析出来的 `&tool_results`）
- 历史里的 result 从组装到发出，**中间没有任何一处去重**

于是重复落在最后一条消息 → 被兜住；落在**历史**里 → 原样发给上游。`messages.24` 是
25 条消息的会话，那个下标必然在历史里。

两种重复来源都会命中：

1. **单条消息内** —— 客户端在一个 content 数组里放两个同 id 的 `tool_result` 块。
   [`process_message_content_dedup`](src/anthropic/converter.rs#L848) 的 `tool_result`
   分支是无条件 `push` 的（那个函数的 `dedup` 参数只管**图片 SHA256**，与 tool_result 无关）
2. **跨消息** —— 两条连续 user 消息各带一个同 id 的 result，被 `merge_user_messages`
   合并进同一条历史消息

常见触发路径是上一轮响应在 `tool_use` 中途被截断（超时 / 断线 / 客户端重连），
客户端重发时带上了重复的结果块。

**修复**：新增
[`dedup_history_tool_results`](src/anthropic/converter.rs#L1071)，在 `build_history`
之后、`validate_tool_pairing` 之前跨**整条历史**按 `tool_use_id` 去重。

- **跨整条历史**而非逐条消息内：同 id 可能分散在不同 user 消息里，只在消息内去重挡不住
- **保留首次出现**：后来的重复来自重发，第一个才是与当时那次 `tool_use` 真正配对的结果
- **收敛在一处**而不是在 `process_message_content_dedup` 和 `merge_user_messages` 各修一遍：
  「一个 `tool_use_id` 在发给上游的报文里只能出现一次」是个报文级不变量，分散去重的话，
  将来任何新增的历史组装路径都得记得再去重一次
- 日志用 `debug` 而非 `warn`：一次重发会让**后续每一轮**都命中同一批重复，warn 会持续
  刷屏；而这里是成功兜住了，不是需要运维介入的异常

**测试**：

| 测试 | 钉住什么 |
|---|---|
| `history_duplicate_tool_result_is_deduped_before_upstream` | 端到端走 `convert_request`，统计 **current + history 合计**每个 id 出现几次（上游校验的就是这个口径），并断言留下的是 `"first"` |
| `cross_message_duplicate_tool_result_is_deduped_before_upstream` | 同 id 分散在两条历史 user 消息的形状 |

两条测试都刻意**多追加两轮对话**把带重复的 user 消息推进 history —— 删掉这两轮，
`validate_tool_pairing` 就会兜住，测试随之失效。这一点写进测试注释了。

> 排查过程中一个作废的猜测记在这，省下次的时间：`process_message_content` 旁边有个
> `process_message_content_dedup`，看着像"有个去重版没被主路径调用"。实际
> [converter.rs:813](src/anthropic/converter.rs#L813) 显示前者就是
> `process_message_content_dedup(content, None)`，`dedup` 参数只做图片去重，与本 bug 无关。

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
