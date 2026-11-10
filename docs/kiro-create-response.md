# Kiro 上游 CreateResponse

## 自动选择

聊天调用选定 OAuth 凭据后，kiro-rs 通过该凭据现有的代理、区域、token 和 profile 查询官方 `featureConfiguration`。如果 `create_response_api` 被分配为 `create_response`，聊天请求使用 `https://runtime.<region>.kiro.dev/v1/responses`。

这个地址是 **Kiro 上游**的 API；客户端继续使用 kiro-rs 原有的 `/v1/messages`、`/cc/v1/messages`、`/v1/chat/completions` 或 `/v1/responses`。

不需要在管理界面新增设置。卡片的 IDE / CLI 设置继续决定未入选账号的原协议。新协议始终使用官方 runtime 域名，与旧协议的全局 `apiDomain` 配置分开。API Key、企业 external IdP、自定义端点和 MCP 调用保留既有处理。

账号选择结果缓存 15 分钟，并对登录 token、profile、区域、客户端版本和机器标识做哈希隔离。同账号并发查询合并；登录身份变化重新查询。控制面失败时保留同身份最近一次有效选择，第一次查询失败则回退原协议，1 分钟后重查。不会因为一次聊天 429 就静默更换协议或把所有学生号强制切到新接口。

## 支持内容

- 历史和当前文本、图片、函数工具声明、工具调用及结果、多轮会话 ID。
- 原有 converter 已提取的文档文本和已转换的工具结果图片。
- reasoning effort，以及请求体已有的 thinking type/display 控制。
- 增量正文、拒答、thinking 原文/签名/加密内容、工具参数分片与完成事件。
- 完成、输出上限、内容过滤、上下文切换要求，以及 HTTP/流内错误。
- 上游提供的精确 token/cache 快照、上下文比例和实际 credits。

SSE 在 Provider 边界逐事件转换为统一内部事件，现有下游处理继续使用同一套工具解析、缓存计量和积分累计逻辑。没有精确 token 快照时使用原有回退计量，不根据 credits 编造 token。重复终态不重复结算；上游报告部分工作积分后失败，保留已报告用量。

单个 SSE 事件与工具参数限制为 4 MiB；UTF-8 分片、CR/LF/CRLF、多行 data 和无尾部空行的完整 JSON 均可读取。流在完成事件前结束会报错，不能作为正常空答案结束。客户端取消请求会释放上游响应，不启动后台脱离请求的生成任务。

## 观察和复测

“请求日志”的尝试链路中，`endpoint = create_response` 表示实际使用了新协议。没有新增凭据类型，也不会改写账号的代理、分组或已保存的 IDE/CLI 选择。

现有“可用模型 → 真实测试”仍是账号池请求，可能切换到其他凭据；判断学生号是否可用时必须核对返回的凭据 ID 和链路。隔离账号进行实测时不要把账号池成功当作该账号成功。

本适配只解决协议兼容。学生账号仍可能收到上游 `rate_limit_exceeded` / `USER_REQUEST_RATE_EXCEEDED`，即使月度积分尚未用完。新接口不保证恢复所有账号，也不会把请求的 Claude 模型改成 Auto 或其他模型。

本地验证：

```powershell
cargo test --no-default-features create_response
cargo test --no-default-features
cargo check
```

测试中的真实网络检查默认忽略；固定账号的人工实测只能显式运行，并通过 stdin 提供凭据，不把认证放入参数、日志或仓库文件。
