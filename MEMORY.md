# 项目实现记录

## Kiro CreateResponse 协议选择

- `KiroProvider` 在 OAuth `ide` / `cli` 聊天调用前，使用当前账号的同一个 HTTP client / 代理、token、profile 和区域查询 `runtime.<region>.kiro.dev/featureConfiguration`；不改变 MCP、API Key、external_idp 或自定义端点。
- 官方 `create_response_api` 为 `create_response` 时使用 `/v1/responses`；缓存按账号和登录身份（token/profile/区域/版本/machineId 的哈希）隔离，TTL 15 分钟。查询失败保留同身份最近有效选择，首次失败走原协议；失败后 1 分钟重查。
- 新接口 JSON 转换、SSE 解码位于 `src/kiro/create_response`，返回仍是统一内部事件，避免为每个下游协议复制工具和计量逻辑。SSE 逐事件转换、限制单事件 4 MiB，必须收到完成/失败终态；调用取消直接释放上游流。
- 原始 thinking 签名只透传；新接口 usage 缺少精确 token 快照时继续沿用原有估算/缓存模拟，不根据 credits 反推 token。credits 只从上游真实 usage 事件累计，重复终态不重复计费，部分失败保留已收到的积分。
- 账号卡片的 IDE/CLI 是原协议选择，新接口由官方实验配置决定。trace attempt 显示 `create_response` 可验证实际请求路由；现有卡片“真实测试”仍是账号池测试，不能用它证明当前学生账号成功。
- 2026-11-10/11 实测已验证新接口可返回学生账号的 Claude 正文、以及 Pro 的工具调用；间歇 429 仍存在，不能承诺切换后全部学生账号稳定恢复。

## 凭据卡片选择请求端点

- 卡片「编辑凭据 → 基本 → 请求端点」支持跟随全局、IDE、CLI，`PUT /credentials/{id}` 的 `endpoint` 省略/null 表示不修改，空串清除覆盖，非空必须是已注册端点。保存后下一次获取账号上下文即使用新值，不重启、不清冷却或改代理。
- 状态响应 `endpoint` 仍是生效端点；`endpointOverride` 为显式值或 null，供界面区分继承与固定选择，编辑其它字段不得把继承固化。
- CLI API / MCP 的 User-Agent 固定采用 `CLI_CLIENT_VERSION = 1.4.121`，独立于全局 `kiroVersion` 默认 2.3.0 与 IDE 自动版本缓存。2026-11-09 官方 stable 桌面元数据 `currentRelease` 为 1.4.121；这里按用户要求固定兼容版本，未声称它是 CLI 安装包版本。
- 回归脚本 `admin-ui/scripts/check-credential-endpoint-selection.cjs` 使用本地模拟 API；Student 只显示可选 CLI 的提示，不自动切换已有账号。

## 创建账号时选择分组

- 手动添加、Social / Builder ID / 企业 SSO 登录、JSON / KAM 导入共用 `AccountGroupField`，可多选已注册分组。
- 首次登录请求的 `groups` 默认空数组，在发起授权时存入会话的 `cred_template`，创建凭据时直接保存；不要在登录完成后另发编辑请求。重新登录保留原账号分组。
- 批量导入选择非空分组时统一覆盖本批账号分组；未选时保留文件中的分组，缺失则不分组。KAM 平铺和嵌套格式均保留文件分组。
- 前端回归：构建后运行 `admin-ui/scripts/check-account-group-selection.cjs`（需要 Playwright，可用 `PLAYWRIGHT_CHANNEL=chrome`）；所有请求在本地 mock，不访问真实 OAuth 或生产服务。

## 凭据元数据

- 凭据元数据使用可扩展对象 `metadata`，固定字段 `type` 只接受 `normal` 或 `boom`。
- 固定字段 `saleStatus` 表示账号在售状态，只接受 `not_for_sale`、`for_sale`、`sold`，
  旧凭据默认 `not_for_sale`；它和 `type` 都只做运营标记，不参与调度，批量编辑也应支持。
- 内置可选字段 `salePrice` 是非负数字，单位固定为 CNY；未设置时不展示，卡片按人民币
  格式显示，单个编辑和批量编辑都应支持设置或清除。
- 旧凭据或新增请求未携带 `metadata` 时，`metadata.type` 默认为 `normal`。
- 未识别的 metadata 扩展键必须在读取、Admin API 编辑和持久化过程中保留。
- `metadata.type` 当前仅用于运营标记，不参与优先级或负载均衡调度。
- metadata 字段定义使用标准 JSON Schema，保存于 `config.json` 的
  `credentialMetadataSchema`；设置页负责维护 key、值类型、默认值和枚举 value。
- 新增和编辑表单按 schema 动态渲染，后端按同一 schema 校验已登记字段，避免前后端规则漂移。
- 凭据卡片用两列表格、紧凑列表用单行摘要展示全部 metadata：优先使用字段 title
  和枚举 title，schema 外扩展字段回退显示原始 key，空值不占用卡片空间。
- Schema 字段可用 `x-css` 配置卡片值样式；前后端都必须拒绝外链、脚本表达式和
  会让内容脱离卡片边界的布局属性，避免自定义样式成为数据外传或界面劫持入口。

## Anthropic Prompt Cache 本地计量

- 固定上报输入 Token（`reportedInputTokens`，默认 2，0 关闭，上限 1000）把对外的
  `input_tokens` 钉在配置值：命中的 `cache_read` 原样保留（Key 级读取上限先生效），
  未缓存尾部并入 `cache_creation`，三项之和仍等于扣减后的真实输入总量。
  上游精确快照同样套用，避免新旧协议两种形状；并入的尾部按 5m TTL 计。
  与 `inputTokenDeduction` 叠加时先扣减再固定。关掉缓存计量的 Key 和总开关关闭时不生效。
  只改上报口径，不动缓存条目、输出 Token 和上游真实积分；trace 标记为「已扣减」。

- 全局 `inputTokenDeduction` 默认 0，可在「设置 → 模型」热更新。对每次上游调用，
  先扣输入总量再拆模拟缓存；精确 provider 拆分按累计比例缩减。输出 Token、缓存条目
  与实际积分不变；内部搜索和恢复逐轮处理，聚合处不得再扣，历史记录不回写。

- 本项目拿不到 Kiro 上游的真实 KV 张量缓存，只能在 provider 未返回精确 usage 时，
  按请求前缀模拟 Anthropic 的 `cache_creation_input_tokens` 与
  `cache_read_input_tokens`；该模拟不会降低上游推理成本。
- 只有顶层自动缓存或显式 block `cache_control` 才写入条目；自动断点落在最后一个
  合格 block，显式断点最多 4 个，读取从各断点回溯最多 20 个位置。
- 连续 `tool_use` 和连续 `tool_result` 块分别只占一个回溯位置；断点只匹配此前真实
  写入的前缀，不为未声明的中间 block 建条目。
- 每个条目独立保存 5 分钟或 1 小时 TTL，命中后按自身 TTL 续期；混用时 1 小时
  断点必须位于 5 分钟断点之前。
- 缓存键覆盖完整结构化 block 与模型配置，并遵守 tools -> system -> messages 的
  失效层级；`tool_choice` 和图片存在性只使 message 前缀失效。
- 本地条目按客户端 Key 与 session 组合隔离；缺少可识别 session 时，按系统提示和
  第一条 user 消息派生稳定隔离种子。系统 Key 缺少 session 本身不代表模拟关闭。
- 客户端 Key 的 `cacheMetering`（默认 true，仅为 false 时写入文件）可单独关闭模拟：
  该 Key 不读写 CacheMeter、不计入 hit/miss 统计，输入全部计入 `input_tokens`，
  上游真实用量仍优先。请求路径必须经 `AppState::cache_meter_for_key` 取计量器，
  `cache_meter` 字段因此是私有的。
- 代表性三轮夹具的热缓存读取率约 88.21%，仅用于验证目标工作负载；不得随机或
  硬编码制造 75%～90% 命中率。
- 已知边界：尚未按模型执行 512～4096 token 的最小缓存阈值；并发请求中条目在
  上游响应开始前即可见；JSON 解析会规范化对象键序，无法复现原始线序变化导致的失效。

## 模拟缓存读取目标与上限

- Key 级 `cacheReadTargetPercent`（0–100，留空关闭）一律按比例上报模拟缓存读取，
  自然命中率不参与；`cacheReadLimitPercent` 只封顶不垫高。两者同时设置时目标优先，
  上限不再二次裁剪，否则「一律按 N%」在上限更低时不成立。
- 两者都向下取整，三项守恒到扣减后的真实输入总量；`total > 0` 时 input 至少留 1 个 token，
  所以目标 100% 会让出 1 个。上游精确快照同样套用；未声明 cache_control 时目标也生效。
- 只在模拟缓存计量开启时生效（Key 级开关与全局开关都算），和固定上报输入同一道门控。
- 目标垫高的读取量本地没有对应缓存条目：它改的是上报口径，不是真实命中，也不减少上游积分。
  要真正提高命中率只能动容量、TTL 间隔、稳定前缀、Redis 共享或会话粘性。

## IP 管理（代理池）

- 代理池是账号代理配置的唯一来源：账号保存 `proxyId` 与代理配置副本（认证以百分号编码
  并入 `proxyUrl`，单独的用户名 / 密码字段清空）。运行时只读账号副本，所以编辑代理后
  必须同步所有关联账号；删除仍被关联的代理返回 409。
- 代理身份是规范化后的 协议 + 主机 + 端口 + 用户名 + 密码，用于去重与启动对齐；
  主机转小写并去掉 IPv6 方括号，空用户名 / 密码视为未设置。
- 启动对齐只给自定义代理补关联、不改写账号原有代理字段；关联代理配置不一致时以代理池为准；
  全局代理或直连的账号清除残留的 `proxyId`。
- 禁用代理只影响新的选择与轮询分配，已关联的账号继续使用；全局代理是独立配置，
  「设为全局」只复制一次，之后编辑代理不会同步到全局代理。
- 列表与错误信息不回显代理密码；`proxy_pool.json` 保留 `url` / `label` 字段供旧版本读取。
