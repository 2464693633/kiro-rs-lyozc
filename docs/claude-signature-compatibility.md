# Claude thinking 签名与结构化输出兼容性

## 签名处理

原生 `reasoningContentEvent.signature` 按不透明值原样传递。空 thinking 文本不等于没有推理块：上游只返回真实签名时，仍保留空 thinking 块，并在普通正文、工具或流结束之前发送签名与块结束事件。

本次修正覆盖流式、非流式、结构化输出的缓冲 SSE，以及未开启 thinking 时仍隐藏这些内容的行为。仅有空 thinking/签名的流不会据此伪造 `max_tokens` 或补造一个空格正文。包含实际 thinking 文字的原有停止处理保持既有行为。

没有上游签名的旧文本 `<thinking>` 兼容路径仍有 `kiro-rs-thinking-signature` 占位。该值只满足部分客户端格式检查，没有密码学验证能力。真实上游签名存在时不替换为占位，不根据缺乏证据的“分片”假设拼接或改写。

## 结构化输出

仅带 `output_config.format` 的请求不再被默认补成 `effort: high`。未显式指定 effort 时，不额外开启原生 reasoning；请求已开启 thinking 时，继续按其预算推导 effort。Schema 仍约束输出形状，不能据此改写模型身份。

## 检测分数的边界

这些修改修复协议传递和参数语义，不会将 Kiro 连接变为官方 Claude Platform 直连。上次报告中的 JSON 已符合 Schema，平台字段回答 `kiro` 是独立于 JSON 合法性的来源判断；本地把该答案写成 `claude_code` 不会改变上游来源。

签名是否能通过校验、是否具有账户绑定限制，与测试网站如何用它归因渠道也是不同问题。这里没有生成官方签名、改写渠道标记或声称上游密码学校验已通过；也没有用新版本重新运行 CCTest。分数需在部署更新后实际复测。

参考第一方说明（核对日期 2026-10-29）：

- [Anthropic thinking 文档：display 与签名](https://platform.claude.com/docs/en/build-with-claude/thinking)
- [AWS ReasoningTextBlock：signature 随原始思考一起回传](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ReasoningTextBlock.html)

第一方文档允许 omitted display 返回空文本并携带签名，要求把签名视为不透明值。跨支持平台的签名兼容不意味着可以通过重写签名把来源改成另一个渠道；具体模型的账户约束还应按实际上游说明处理。

## 验证

本地上游 HTTP 夹具覆盖 Messages/CC、普通/结构化输出、流式/非流式八种组合中的 signature-only 保留；单元测试覆盖正文/工具/EOF 边界、禁用 thinking、纯签名不伪造截断，以及 format-only 不隐式开启高 effort。

`cargo test --no-default-features --locked --offline --quiet`：923 项通过、0 失败、2 项跳过；生产编译检查通过。未对真实上游进行签名密码学校验或新版 CCTest 复测，不能将这些测试解释为渠道检测满分证明。
