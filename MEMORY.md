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

## Anthropic Prompt Cache 与输入上报

- `reportedInputTokens` 默认2（0关闭），从原输入总量预留，其余组成 creation+read 缓存池。总量不足配置值时按总量，不增加 Token。输出 Token 和实际积分不变。
- 上游精确快照优先提供原总量与自然读数，本地估算作为缺失回退；两者统一应用上报策略，无断点也可固定输入2，此时自然读取0、剩余归creation。关闭 Key 或全局缓存计量时不应用上报策略。
- Key `cacheReadTargetPercent` 的分母是 creation+read，目标覆盖上限；未设目标时 `cacheReadLimitPercent` 只压低自然read，被裁掉部分归creation。自然模式反映前缀命中，人工目标是计量分配、不是真实缓存命中。
- 多轮搜索、身份检查、输出恢复先聚合自然用量，在客户端请求边界只预留一次输入并分配比例，响应与Key统计一致，积分逐轮累计不重复。
- `inputTokenDeduction` 功能已移除，旧JSON字段忽略且后续配置保存时不再写出；不得恢复1900扣减。
- 上报策略下creation统一按默认5m分类；不调整的自然拆分保留已知TTL。CacheMeter仅按合法cache_control断点写条目（5m/1h，最多4断点、回溯20位置），人工比例不改条目。
- 自动断点优先落在当前user之前的合格块；每Key隔离，有会话标识时进一步按会话隔离。缺session的系统Key按系统提示与首条user派生种子。
- 既有边界：无按模型最小缓存阈值，条目在上游成功前可见，模拟不会降低上游积分。
- trace `adjusted` 表示应用固定输入或比例，可能来自provider或本地估算；历史分类不回写。来源面板展示分布，不能将adjusted占比解释为纯本地估算比例。

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
