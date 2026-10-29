# 第三方 API 请求观察器

独立的 HTTP 转发与记录程序。管理页面填写第三方 API 的 Base URL 和 Key，CCTest 使用本程序的转发入口和独立 Relay Key。页面可查看请求、响应、SSE 事件和到达时间，便于比较不同供应商的实际接口行为。

程序单独监听 `8991`，使用 Python 3.12 标准库，不依赖 kiro-rs 进程。安装它无需重启或修改 `8990`、`8989` 上的已有实例。

## 数据与转发

- 请求体、响应体和 SSE 的签名字段按原始内容转发，不重写模型身份、工具 ID、签名、答案或 usage，也不伪造引用或成功状态。
- HTTP 转发仍需替换上游鉴权、目标 Host 等路由所需信息，并处理 HTTP 连接头。下游 Relay Key 与上游供应商 Key 不相同；这些必要变化不等于逐字节复制整条 HTTP 连接。
- 记录最多保留 200 条、24 小时。每个方向的原始内容最多记录 2 MiB；超过时仅截断观察记录，转发仍继续完整发送。比较大文件时应同时检查记录是否已截断。
- 鉴权头、Cookie 等敏感 HTTP 头在记录和导出中脱敏，URL 查询参数不持久化。原始正文不会因此被改写：如果第三方在正文中回显 Key，或请求内容本身包含秘密，它们仍可能进入捕获与导出。原始捕获受独立管理员 Key 保护，导出文件应按敏感数据保管。
- 服务配置保存在 `/data/state.json`，观察记录使用同一目录中的 SQLite；配置文件包含实际使用的密钥，应仅由服务器管理员访问。
- `admin_key` 与 `relay_key` 在首次启动时独立随机生成并保存。服务不会把它们打印到日志；管理员 Key 用于管理 API，Relay Key 只能转发请求。

观察器用于记录用户自己授权的调用。原始请求与响应可以帮助检查协议、用量字段和事件时序，但不能证明第三方内部实现、模型真实来源或实际计费；本程序不解密或验证 thinking 签名。

## 云端构建

工作流 `.github/workflows/upstream-audit.yaml` 在 `main` 上的本目录或该工作流发生变化时触发，也支持手动运行。先执行 Python 单元测试，再由 GitHub Actions 构建 `linux/amd64`、`linux/arm64` 镜像并发布到 GHCR：

```text
ghcr.io/2464693633/kiro-rs-lyozc-audit:beta-<提交 SHA 前六位>
ghcr.io/2464693633/kiro-rs-lyozc-audit:beta
```

工作流使用 `packages: write` 和 Actions 自带令牌，不需要 Docker Hub Secrets；不会发布 `latest` 或正式版本标签。镜像的 OCI `source`、`revision` 标签可用于核对仓库和提交。首次发布需确认 GHCR 包的可见性，只有公开包才能匿名拉取。

## Docker 部署

服务器只需拉取云端镜像。以下使用专用数据目录，并将端口仅绑定到服务器本机：

```sh
sudo install -d -m 700 -o 10001 -g 10001 /opt/upstream-audit/data
docker pull ghcr.io/2464693633/kiro-rs-lyozc-audit:beta
docker run -d \
  --name upstream-audit \
  --restart unless-stopped \
  -p 127.0.0.1:8991:8991 \
  -e AUDIT_DATA_DIR=/data \
  -e AUDIT_PUBLIC_BASE_URL=https://lyozc.top/relay-audit \
  -v /opt/upstream-audit/data:/data \
  ghcr.io/2464693633/kiro-rs-lyozc-audit:beta
```

容器以 `10001:10001` 运行，绑定挂载的数据目录需允许该 UID 写入。生产部署建议把 `beta` 换成验证过的固定提交标签或镜像摘要。无密钥 `GET http://127.0.0.1:8991/health` 只返回健康状态，不访问第三方接口；Docker Healthcheck 使用此端点。

## HTTPS 与管理页面

在现有 HTTPS Nginx 虚拟主机中，把 `/relay-audit/` 前缀剥离后转发给独立观察器，继续使用现有证书：

```nginx
location = /relay-audit {
    return 308 /relay-audit/;
}

location ^~ /relay-audit/ {
    proxy_pass http://127.0.0.1:8991/;
    proxy_http_version 1.1;
    proxy_set_header Connection "";
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_buffering off;
    proxy_request_buffering off;
    proxy_cache off;
    proxy_intercept_errors off;
    proxy_next_upstream off;
    proxy_read_timeout 900s;
    proxy_send_timeout 900s;
    gzip off;
}
```

`proxy_pass` 末尾的 `/` 用于去掉 `/relay-audit/` 前缀。此 location 指向 `8991`，不经过 sub2api，也不修改原有 kiro-rs 端口。现有虚拟主机的请求大小上限仍适用；图片、文档请求需要足够的 `client_max_body_size`。修改 Nginx 后先检查配置，再重新加载。

通过服务器本机读取 `/opt/upstream-audit/data/state.json` 中的 `admin_key`，在浏览器打开：

```text
https://lyozc.top/relay-audit/#admin=<管理员 Key>
```

`#admin=...` 是浏览器 fragment，不随页面 HTTP 请求发送。页面随后以 `x-audit-admin-key` 请求管理 API；不要把管理员 Key 写成查询参数。Relay Key 无法调用管理 API。

在管理页填写：

1. 第三方 API 的 Base URL，按其文档填写接口前缀。
2. 第三方 API Key。
3. 保存配置后，使用页面显示的 Relay Key 作为 CCTest 的 API Key。

## CCTest 接入

在 CCTest 填写：

| 字段 | 值 |
| --- | --- |
| Base URL | `https://lyozc.top/relay-audit` |
| API Key | 本服务生成的 Relay Key |
| 模型 | 第三方上游实际支持的模型名 |

例如，CCTest 请求 `/relay-audit/v1/messages`，Nginx 去掉前缀后，观察器按配置转发到第三方 Messages 接口。CCTest 使用的 Key 是 Relay Key，上游供应商 Key 由观察器填入上游鉴权。

先用少量自己的测试请求确认接口可用，再运行检测。记录中的 SSE 时间反映事件到达观察器的时间，会受到上游、网络及代理缓冲影响，不能直接等同模型内部生成时间。观察器不会自动调用 CCTest，也不自动更改测试结论。

## 本地检查

```sh
cd tools/upstream-audit
python -m unittest discover -s tests -v
```

后端和前端均无需额外 Python 依赖；实际第三方调用需要网络和有效 Key。
