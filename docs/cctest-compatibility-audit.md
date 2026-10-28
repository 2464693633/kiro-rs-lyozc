# CCTest 检测项与 kiro-rs 兼容性改造

核对日期：2026-10-28。代码基线：`7255d98`，包版本 `0.9.3`。

目标是补齐真实 API 能力、协议语义和可验证的回归测试。本文不是 CCTest 服务端算法的复刻，也不承诺其总分。

## 证据与边界

- 已读取 CCTest FAQ、API 文档、模型指纹页面，以及网页公开加载的 JavaScript。
- 结果页脚本通过 `GET /api/check/{uuid}` 轮询。该接口可以返回分项分数、步骤状态及 Token 审计明细。
- 已读取用户提供的第三方报告。这份报告不属于当前项目，不能作为当前项目的实测结果。
- 没有取得检测服务端源码、每项完整请求、准确提示词、签名解析字段、阈值和总分公式。
- 不把自己设计的回归用例描述为 CCTest 的真实考题。

公开页面入口：

```text
https://cctest.ai/zh/faq
https://cctest.ai/zh/docs
https://cctest.ai/zh/fingerprint
https://cctest.ai/api/v1/fingerprint/bank
```

结果页脚本的 Claude 项目数组有 11 个分项，每个 UI 上限为 10；网站总分展示采用 100 分制，但不能由此断言是 11 项直接相加，不能自行编造权重或换算公式。

## 已确认的检测项目

| 项目 | 公开字段 | 已确认范围 | 尚不知道的内容 |
| --- | --- | --- | --- |
| LLM 指纹验证 | `tag_check` | 有单独的模型行为/指纹检查 | 实际挑战内容、何种回复通过 |
| 流结构校验 | `stream_structure` | 检查 SSE 结构和事件序列 | 完整字段清单、容错范围 |
| 非流结构校验 | `non_stream` | 检查 JSON 响应结构 | 完整约束、各字段权重 |
| WebSearch | `websearch` | 有独立搜索能力检查 | 是否强制引用、验证最新事实、怎样验证结果回传 |
| 签名校验 | `signature_proto` | FAQ 声称解析 Protobuf 签名并识别渠道 | 字段定义、是否回放校验、怎样扣分 |
| 结构化输出 | `output_config` | 有独立结构化输出检查 | 实际 schema、是否兼容旧参数 |
| 工具调用 | `server_tool` | UI 标签为工具调用 | 具体测试普通 function、server tool 或二者；不能仅凭字段名判断 |
| 知识库检测 | `knowledge` | 有独立知识表现检查 | 题目、知识截止时间和判分方式 |
| 模型版本归因 | `model_attribution` | 三轮取数，输出数字分布与模型样本库比较 | 相似度算法、校准方法和判定阈值 |
| 文档识别 | `doc_recognition` | 有独立文档能力检查 | PDF、文本、URL 或其他输入格式 |
| 图片识别 | `image_recognition` | 有独立视觉能力检查 | 图片内容、输入方式、回答标准 |

Token 用量审计是额外流程。首页说明为 11 轮；第三方报告也返回 `10/11` 进度。明细字段包括 input/output/cache write/cache read、网站估算费用、基线费用、倍率、延迟和输出速度。网站估算费用不等于已核验的上游账单。

前端支持 `isKiro` 专用判定文案，但触发条件在服务端。不能假定替换一个字段就能影响判定。

## 当前项目已证实的缺口

### 1. 结构化输出参数没有完整传递

位置：`src/anthropic/types.rs::OutputConfig`、`src/kiro/model/requests/kiro.rs::KiroOutputConfig`。

两个结构当前都只有 `effort`，没有 `format`。`Tool` 也没有 `strict` 字段。客户端提交这些额外字段时，当前反序列化结构不会把它们保留下来。

官方 Claude 的 `output_config.format` 是 JSON schema 约束；仅在提示词里要求 JSON，无法等价实现原生约束解码。

改造：先保留和校验 schema/strict 字段，再用真实上游验证 Kiro 接受哪些控制字段。若上游不支持，可明确提供“生成、验证、有限重试”的适配能力，但需要单独标注语义及额外用量；失败应返回明确错误。不能把未校验的普通文本当作已保证符合 schema。

回归：嵌套对象、枚举、必填字段、禁止额外字段、复杂转义、流式 JSON、截断、拒答；禁止在失败后补造符合格式的答案。

### 2. document 内容会被忽略

位置：`src/anthropic/converter.rs::process_message_content_dedup`。

当前只处理 text/image/tool_result/tool_use，其他类型走空分支。`ContentBlock.source` 统一使用图片结构，要求 media_type/data，不覆盖 URL、文件引用等来源。

改造：把图片和文档 source 分成可辨识的类型；支持的文档应真正转换后交给模型，不支持的输入明确报错。PDF 提取文本只能处理文本信息；需要图表和扫描件时须有页面图像处理路径。先确定部署所需的 PDF 引擎和资源预算，再加入依赖。

回归：文档内放运行时生成的唯一字符串，模型只能从实际文档获得答案；覆盖纯文本 PDF、扫描页、多页、图表、无效 base64、错误 MIME 和超限文件。

### 3. tool_choice 被接收，但未在 Kiro 请求转换中生效

位置：`src/anthropic/types.rs::MessagesRequest`、`src/anthropic/converter.rs`。

当前请求类型声明了 `tool_choice`，但请求转换主链路没有使用该字段落实选择行为。其他模块可能用于缓存指纹或协议转换，不代表上游已经遵守。

改造：区分 auto/none/any/指定工具，验证工具名，验证 thinking 与强制工具的组合。先验证上游控制能力；不能把普通模型文本重新包装成一次实际未发生的工具调用。工具名、id 和参数必须完整往返。

回归：指定某个工具、禁止工具、需要调用但缺必填参数、多工具、分片 JSON、tool_result 续轮、未知工具名、thinking 组合；跨 OpenAI/Anthropic 入口检查语义一致性。

### 4. 纯 WebSearch 路径绕过模型回答

位置：`src/anthropic/websearch.rs::extract_search_query`、`handle_websearch_request`、`generate_search_summary`、`websearch_result_content`。

- 纯搜索请求取第一条消息的首个文本作为查询，不等价于理解当前轮问题。
- MCP 搜索完成后直接拼接标题、片段和 URL，没有让模型综合结果作答。
- `encrypted_content` 当前填入明文 snippet，并不是原生 Claude API 的加密搜索上下文。
- `max_uses` 虽保存在工具定义里，但纯搜索路径未使用它实施限制。
- 当前 Tool 类型未保留 allowed_domains/blocked_domains/user_location 等搜索控制字段。

改造：评估把纯搜索与混合工具统一到真正的搜索 agent loop：当前轮问题、搜索调用、实际结果、模型总结；保留用户限制，明确错误传播。引用只能指向确实使用的结果。若使用自有续轮令牌，需要定义自己的可验证语义，不能声称它是 Anthropic 原生 encrypted_content。

回归：多轮中最后一条问题、中文问题、无需搜索的问题、零次/一次搜索限制、无结果、MCP 失败、引用与结果对应、客户端取消、下一轮携带搜索结果。

### 5. thinking 签名透传与回传不是原生 API 等价实现

位置：`src/anthropic/stream.rs::THINKING_SIGNATURE_PLACEHOLDER`、`close_open_thinking_block`、`src/kiro/model/events/reasoning.rs`、`src/anthropic/converter.rs::convert_assistant_message`。

项目已能接收原生 reasoning 事件中的 signature/redactedContent，但缺少签名时回退到固定占位。客户端 assistant thinking 回传的转换没有完整的原始签名和 redacted thinking 往返路径。

改造：先记录实际模型/端点是否下发真实签名；有真实值时逐字节保留，不改写、不拼接到别的 thinking 上。没有真实值时不能通过本地生成字符串获得原生签名语义。增加相应能力标识，保留已存在的兼容行为时也需明确它只是客户端兼容。

官方文档要求把 thinking/redacted_thinking 原样回传，并将 signature 视为不透明数据。签名结构识别本身不等于签名完整性验证，也不能独立证明底层模型真假。

### 6. Token 与缓存计量存在真实边界

位置：`src/token.rs`、`src/anthropic/cache_metering.rs`、`src/anthropic/handlers.rs::resolve_non_stream_usage`。

本地 count_tokens 是字符启发式估算，可选调用远程计数服务。缓存模块明确是模拟计量；上游精确 tokenUsage 和本地估算必须区分。会话粘性有利于维持同一上游账号，但不能保证上游实际缓存命中。

改造：追踪每次 usage 的来源，优先采用上游真实值；保留多轮搜索的累计使用量；流式最终量与非流式口径一致。不能为了倍率好看修改缓存比例、隐去额外调用或修改输出 tokens。

回归：无缓存、冷启动、重复前缀、前缀修改、断点移动、模型切换、TTL 到期、并发、工具续轮、搜索多轮、流式失败。针对上游真实 usage 与 fallback 分别断言，不能要求估算路径等于原生账单。

### 7. 其他会影响协议行为的缺口

- `MessagesRequest` 没有 temperature/top_p/stop_sequences 等字段，不能假定客户端提交后会生效。这些字段是否被 CCTest 使用尚未确认。
- `max_tokens` 在主 handler 校验为正数，但当前 Kiro 请求转换没有传递它，也没有完整按客户端上限截断的实现。不要把上游主动结束原因映射为 max_tokens，误认为已实现客户端限额。
- 图片结构主要覆盖 base64；URL 来源不能按同等能力对外承诺。
- 工具 schema 的兼容规范化会修改部分语义，需验证真实用户工具是否仍满足原始约束。
- SSE 的 message/block 配对、完整工具参数、异常退出、最终 usage 与 stop_reason 已有网关测试，应保留并扩展。

## 不应与接口修复混淆的项目

- 知识题与取数指纹主要受实际模型、版本、系统上下文和采样影响。正确做法是验证实际选择的模型，避免错误别名和降级；不要硬编码测试题答案或数字序列。
- Kiro 提供的系统上下文和上游能力无法单靠代理完全控制。
- 增加一个像签名的字符串、伪造工具/搜索结果或修改 Token 计量，不能提供对应真实能力。
- 即便接口兼容性提高，检测器仍可能识别 Kiro 渠道，不能承诺其 official 判定或 100 分。

## 建议实施顺序与验收

1. 建立隔离测试配置：固定真实模型、固定凭据、记录测试开始时的代码版本；先关闭会话故障转移等会干扰归因的变量，仅用于该测试配置。
2. 为自建通用用例保存原始请求、响应状态、逐条 SSE 和最终 JSON。脱敏密钥，记录选中模型、实际凭据/端点、usage 来源；原始资料保存在测试输出目录，不提交含真实数据的日志。
3. 优先补文档处理、工具选择和结构化输出；每项先验证上游可行性，再实现适配与回归。
4. 统一纯搜索与混合搜索语义，完成真实结果、模型回答和多轮续传。
5. 补 thinking 原始签名往返、Token 来源和流式/非流式一致性；无上游支持的部分明确标识。
6. 使用同一实例在 CCTest 复测，按自有请求日志定位它实际发了什么；只将真实观察到的请求纳入对应证据。首次测试通过不能代表对所有请求正确。

离线 fixture 测试负责验证协议转换；真实上游测试负责验证模型能力；CCTest 是额外的外部黑盒观察。三类结果分别记录，不互相代替。

本次只完成静态分析和方案，不修改运行时代码，不调用当前项目的真实上游或提交密钥进行收费检测。

## 官方协议参考

```text
https://platform.claude.com/docs/en/build-with-claude/structured-outputs
https://platform.claude.com/docs/en/build-with-claude/streaming
https://platform.claude.com/docs/en/build-with-claude/extended-thinking
https://platform.claude.com/docs/en/build-with-claude/pdf-support
https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool
https://platform.claude.com/cookbook/tool-use-tool-choice
```
