# Messages 兼容适配

以下行为适用于 `/v1/messages` 和 `/cc/v1/messages`，默认生效，无需在管理界面开启。内容观察仍从转换前捕获请求，因此能看到调用方原始的文档、Schema 和工具声明。

## 文档

支持内联 `document.source`：

- `base64` + `application/pdf`：通过 Rust PDF 引擎解析并提取文本，再随消息发送给上游模型。
- `base64` + `text/plain`：解码 UTF-8 文本。
- `text` + `data`，或 `content` + 字符串/纯文本块数组。

同时处理当前轮、历史轮和 `tool_result.content` 内的文档。保留块上的缓存控制。单个二进制文档最多 10 MiB，一次请求提取后的文档文本合计最多 2 MiB，最多两个 PDF 转换任务同时运行。

这是文本提取适配。扫描页 OCR、图表理解、原生 PDF 引用、URL 和文件 ID 来源尚不支持；无可提取文字、无效 PDF、无效编码及不支持的来源返回 400，不会静默丢弃。不得把文本 PDF 支持解释成完整原生视觉文档能力。

## JSON Schema 输出

保留并处理 `output_config.format = {"type":"json_schema","schema":{...}}`。先校验 Schema，再将格式要求发送给上游；收到 `end_turn` 的最终文本后，解析 JSON 并验证 Schema。必填项、枚举、字段类型、额外字段和本地 `$ref` 均参与验证。

只指定 `format` 而未指定 `effort` 时，不会自动开启高强度推理；显式 thinking 的预算和显式 effort 仍按原规则传递。

- 非法/不支持的 Schema：400。
- 上游最终答案不是合法 JSON 或不符合 Schema：502，`upstream_output_schema_error`；请求链路记为失败，并保留实际消耗用量。
- `tool_use`、拒答、`max_tokens` 等停止原因保持原语义，不能按正常完成的结构化答案使用。
- 流式请求先缓冲并校验，正文会等待模型完成；等待超过 1 秒时提前发送 `message_start`，每 10 秒发送 `ping`。流内失败发送 `error` 并结束，不发送成功结束事件。参见 [协议兼容](protocol-compatibility.md) 的等待与保活说明。
- 不自动重试、不伪造字段、不改写模型身份；Schema 适配仍消耗正常模型用量。
- 仅支持本地片段引用，不下载 HTTP/file Schema。

这属于提示约束与返回校验适配，**没有声明 Kiro 提供原生语法约束解码**。

## 搜索和代码执行

纯 `web_search` 和混合工具请求都使用模型续轮：模型给出查询、网关调用 Kiro MCP、结果反馈给模型、模型生成回答。搜索失败会传播错误，不再把固定拼接的搜索片段当作模型答案。该过程可能消耗多轮模型用量。搜索上下文仍是网关适配格式，不能当作 Anthropic 原生加密上下文。

最终 JSON/SSE 用量包含 `server_tool_use.web_search_requests`，记录真正进入 MCP 的逻辑搜索次数；空结果计一次，内部重试不重复计数，只声明工具但未调用时为 0。上游没有原生 citation 时不会根据 Markdown 链接补造引用；已有 text citations 会在 SSE 中保留。搜索每轮模型正文目前仍先完整收集再发送，工具执行进度实时发送，因此正文首字节可能等待较久。

未请求 `thinking.type: enabled/adaptive` 的响应不会把上游原生推理混入正文，也不会通过搜索扩展字段暴露；模型别名中的 thinking 后缀仍按原规则启用。隐藏内容仍参与缺少上游精确用量时的本地输出估算。此规则只识别原生推理事件，不按文字内容猜测或删改普通回答。

原生 thinking 文本为空而签名非空时，仍保留空 thinking 块及原始 signature；结构化输出缓冲为 SSE 时同样保留。签名作为不透明值传递，不通过重写内容、工具 ID 或渠道字段来改变来源。没有上游签名的旧文本标签路径仍使用已有兼容占位，它不具备密码学签名语义。

带 `type: code_execution_*` 的服务端工具请求返回 400，并说明未配置隔离执行后端。不会把生成的普通 `tool_use` 宣称为执行完成。自定义客户端工具照常返回客户端执行。

## 冷却和观察标签

实时流在尚未获知上游用量时，`message_start.usage` 的 token 字段为 0。结束事件 `message_delta.usage` 是本次请求的最终绝对快照，应覆盖此前同名字段，不能与起始值相加；其中 `input_tokens: 0` 是合法的未缓存输入值，缓存创建/读取分别计入各自字段。上游精确用量优先，只有缺少精确数据才使用既有估算回退。已缓冲并完成校验的结构化输出在发送 SSE 前已知最终输入/缓存值，因此允许在 `message_start` 中报告这些已知值。OpenAI Chat Completions 与 Responses 按最终快照转换输入总量，缓存输入只计算一次。

匹配账号处于冷却或 RPM 已满时返回 429 和 `Retry-After`。每个账号分别等待冷却与 RPM 同时释放，再取候选账号中最早的时间；不把冷却误报为账号禁用。上游网络、权限等独立故障仍可返回其他错误。

管理界面“请求日志 → 内容观察”新增“服务端代码执行”标签，补充身份和知识截止用语。推测标签仅基于最后一个用户回合，避免系统提示词、历史消息触发误报；标签不能证明来源或检测通过。

## 部署和复测

推荐使用本仓库 GitHub Actions 在云端构建，服务器只拉取镜像并重启，无需安装 Rust/Bun 或在服务器编译：

- 推送 `main`/`master`：`build.yaml` 构建可下载程序，`docker-build.yaml` 构建并推送 `beta-<提交 SHA 前六位>` 和 `beta` 镜像，不更新 `latest`。
- 推送 `v*` 版本标签，或手动运行 `release.yaml`：构建前端和各平台程序，使用 `Dockerfile.release` 打包已编译的 Linux 静态程序，发布版本镜像和 `latest`。版本必须与 `Cargo.toml` 一致；手动发布还要求标签不存在。
- 默认发布到 GitHub Container Registry：`ghcr.io/<小写所有者>/<小写仓库名>`，使用 Actions 自带令牌，不需要 Docker Hub Secrets。本仓库地址为 `ghcr.io/2464693633/kiro-rs-lyozc`。首次发布后需确认包可见性，公开包才能匿名拉取。
- 如需 Docker Hub，在仓库变量中设置 `IMAGE_REGISTRY=docker.io`，并配置 Secrets `DOCKERHUB_USERNAME`、`DOCKERHUB_TOKEN`；命名空间使用仓库变量 `DOCKERHUB_NAMESPACE`，未配置时使用小写 GitHub 仓库所有者名。详见 [镜像发布配置](container-publishing.md)，无需把凭据发到对话中。

服务器通过 `KIRO_RS_IMAGE` 指定上述实际发布的镜像，然后执行 `docker compose pull`、`docker compose up -d`。仓库 Compose 默认仍指向公共上游 `zyphrzero/kiro-rs:latest`，必须覆盖成自己的镜像，才会包含本仓库的修复。

使用新生成的 PDF 内容、多个不同 Schema 和实际搜索问题复测；观察日志只能验证请求和响应，外部网站的判分仍需重新运行。429/502 的具体线上原因需结合链路中的上游尝试记录确认。
