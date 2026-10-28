# 检测请求观察器

项目管理后台已内置「请求日志 → 内容观察」，可直接开启并查看请求/响应，使用说明见 `docs/request-observation.md`。下面的 mitmproxy 扩展是独立的外部抓包方案，不会自动把文件导入管理后台。

用 mitmproxy 的反向代理模式，观察发往 kiro-rs 的测试流量。记录实际收到的提示词、工具/schema、图片/文档及 thinking/cache 参数，并按内容特征标注。

```text
CCTest / 兼容客户端
        ↓ 你自己域名的 HTTPS 入口
反向代理 / TLS 终止
        ↓ 127.0.0.1:8991
mitmdump + observer.py
        ↓ 127.0.0.1:8990
kiro-rs
```

这个工具只观察流量，不改写请求、模型答案、签名或用量。内容标签不是来源鉴定，无法仅凭提示词可靠确定“这就是 CCTest”。通过专用测试 Key 筛选，能把操作者发起的本次检测与普通用户请求分开。

## 使用

脚本语法要求 Python 3.10+；安装 mitmproxy 时还需满足其所选版本的 Python 要求。在此目录执行：

```powershell
python -m venv .venv
.\.venv\Scripts\python.exe -m pip install mitmproxy

# 在 kiro-rs Admin 中先创建专用客户端 Key。
# 将下面占位符替换为该 Key，CCTest 中也填写同一把 Key。
$env:KIRO_OBSERVER_API_KEY = '替换为检测专用客户端Key'
$env:KIRO_OBSERVER_LABEL = 'cctest-round-1'

.\.venv\Scripts\mitmdump.exe --mode reverse:http://127.0.0.1:8990 --listen-host 127.0.0.1 --listen-port 8991 -s .\observer.py
```

把自己的 HTTPS 测试入口转发到 `127.0.0.1:8991`，然后在 CCTest 中填写该入口。若原来入口直达 8990，需要让本次测试经过 8991 才能观察到。HTTPS 证书由你已有的入口处理，反向代理模式不需要在 CCTest 安装抓包根证书。

若现有反向代理缓存 SSE，需要关闭该缓存。插件在 mitmproxy 层按块转发响应，不等待完整回答。

观察器有解析和落盘开销；经此链路测得的耗时不应直接当作 kiro-rs 的裸服务性能。

优先用单独测试域名/路由和专用 Key；插件只记录选中 Key 的 API 路径。未设置 `KIRO_OBSERVER_API_KEY` 时不记录。Admin、凭据和 OAuth 路径不在捕获范围。

Linux/macOS 可使用同样的虚拟环境安装；用 `export KIRO_OBSERVER_API_KEY='...'` 设置变量，执行 `.venv/bin/mitmdump`。

## 输出

默认位置：本目录的 `captures/<UTC时间_随机ID>/`。目录已加入本地 `.gitignore`。

| 文件 | 内容 |
| --- | --- |
| `request.meta.json` | 路径、模型、特征标签、操作者标签、长度、截断状态和正文 SHA-256 |
| `request.json` | 完整捕获且可以解析时，格式化后的原始 JSON，保留 Rust 类型可能忽略的字段 |
| `request.body` | 解码 HTTP Content-Encoding 后的请求正文；超限时为前缀 |
| `response.meta.json` | HTTP 状态、耗时、首个响应正文字节耗时、长度、截断/传输失败标记 |
| `response.body` | 转发的响应内容字节，包含 SSE 事件；可能仍为 gzip/br 等编码，按元数据解码 |

请求/响应头仅保留少量诊断字段，不记录 Authorization、x-api-key、Cookie 或 URL 查询参数。提示词、文档和图片本身会保存在正文中，因此这应是你主动运行的测试观察器。测试结束后停掉进程，并按需清理测试捕获目录。

配置在启动时读取：

| 环境变量 | 默认值 | 用途 |
| --- | --- | --- |
| `KIRO_OBSERVER_API_KEY` | 空，禁用捕获 | 完整匹配 x-api-key 或 Bearer Key；不写入日志 |
| `KIRO_OBSERVER_LABEL` | `manual-test` | 操作者自定义标记，不作为来源证明 |
| `KIRO_OBSERVER_DIR` | 脚本旁 `captures` | 输出目录 |
| `KIRO_OBSERVER_REQUEST_BYTES` | 8388608 | 单请求正文保存上限，0 表示不保存正文 |
| `KIRO_OBSERVER_RESPONSE_BYTES` | 2097152 | 单响应正文保存上限，0 表示只保存元数据 |

长度上限配置允许 0～64 MiB。超限会明确标记 `truncated`，不会截短转发到 kiro-rs/客户端的正文。正文超限时不生成 `request.json`，也不对不完整 JSON 猜测标签。保存失败只影响观察数据。

## 标签

- `observed`：实际结构中出现的能力，如 `structured_output`、`tools`、`tool_choice`、`web_search`、`image`、`document`、`thinking`、`signature_replay`、`cache_control`。
- `heuristic`：根据用词推测的探测，如数字取样、知识截止时间和模型身份提问。普通用户也可能提出相同问题，会有误报和漏报。
- 不会仅凭一个请求认定它是 Token 审计，也不会自动认定来源是 CCTest。完整轮次需要按专用 Key、时间和你发起检测的记录一起分析。

## 验证

不需要安装 mitmproxy 即可运行纯 Python 单元测试：

```powershell
python -m unittest discover -s . -p 'test_*.py' -v
```

这些测试验证分类、筛选、限长、头字段处理和流式字节保持，不等于真实 CCTest 或真实 mitmproxy 的端到端测试。

上游项目及文档：

- <https://github.com/mitmproxy/mitmproxy>
- <https://docs.mitmproxy.org/stable/concepts/modes/>
- <https://docs.mitmproxy.org/stable/addons/examples/>
