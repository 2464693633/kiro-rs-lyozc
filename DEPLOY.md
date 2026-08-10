# 服务器更新操作手册

> 在服务器上把 kiro-rs-lyozc 更新到最新代码。命令按顺序**一条一条**粘，
> 等上一条跑完再粘下一条 —— 多条一起粘会被终端回显搅乱（实测踩过）。

---

## 实例参数

以 8990 为例，下面所有命令里的这几个值成套出现，换实例时一起换：

| 项 | 8990 的值 |
|---|---|
| 项目目录 | `/opt/kiro-rs-lyozc-8990` |
| compose 项目名 | `kiro8990` |
| compose 文件 | `docker-compose.fork.yml` |
| 容器名 | `kiro-rs-8990` |
| 端口 | `8990` |
| 数据目录 | `/opt/kiro-rs-lyozc-8990/data/` |

> **另一个实例（8991）的参数我没核实过**，别照抄 8990 的。查法见文末「附录 A」。

---

## 更新流程

### 1. 拉取代码

```bash
cd /opt/kiro-rs-lyozc-8990
```

```bash
git pull origin main
```

```bash
git log --oneline -1
```

看到最新的提交号即成功。

**报 `dubious ownership`**（Git 拒绝操作非当前用户属主的仓库）：

```bash
git config --global --add safe.directory /opt/kiro-rs-lyozc-8990
```

然后重跑 `git pull`。

> 如果这台机器还有别的用户会碰这个目录，root 跑完 `git pull` 可能留下 root 属主的文件，
> 让那个用户后续操作报权限错。到时用
> `chown -R 用户名:用户名 /opt/kiro-rs-lyozc-8990` 修。只用 root 操作则无影响。

**已经是最新**（输出 `Already up to date.`）：后面的重建可以跳过。

### 2. 构建

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml build
```

这步最久：先 `bun install` + 前端 build，再 Rust release 编译。开发机上约 4 分钟，
服务器通常更久，十几分钟也正常。

前端不用你手工构建 —— Dockerfile 里有 `bun run build` 阶段，容器内从源码建。
`admin-ui/dist` 是 gitignore 的，但不影响。

**出现 `Killed` 或 `signal: 9`**：内存不够被 OOM kill（rustc 峰值几个 G）。
可以加临时 swap 后重试：

```bash
fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
```

装完不想留着就 `swapoff /swapfile && rm /swapfile`。

### 3. 启动

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml up -d
```

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml ps
```

状态 `Up` 即正常。

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml logs --tail=40
```

### 4. 验证

数据不用备份 —— `./data/` 是 bind mount，重建镜像不碰它。

```bash
curl -s -o /dev/null -w "HTTP=%{http_code}\n" http://127.0.0.1:8990/admin
```

`200` 即服务起来了。

---

## 常用排查

### 取管理密钥

```bash
grep -oP '"adminApiKey"\s*:\s*"\K[^"]+' /opt/kiro-rs-lyozc-8990/data/config.json
```

没输出就直接看文件：

```bash
grep -i apikey /opt/kiro-rs-lyozc-8990/data/config.json
```

下面凡是 `<管理密钥>` 都替换成它的值。**不要用 `$(...)` 嵌套**，分步替换更不容易出错。

### 看凭据状态

```bash
curl -s -H "x-api-key: <管理密钥>" http://127.0.0.1:8990/api/admin/credentials | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(f\"总数={d.get('total')} 可用={d.get('available')} 当前={d.get('currentId')}\")
for e in d.get('credentials', []):
    print(f\"id={e.get('id')} 禁用={e.get('disabled')} 原因={e.get('disabledReason') or '-'} 冷却={e.get('throttledRemainingSecs') or '-'} 分组={e.get('groups') or []} 上游={e.get('isUpstream')} {e.get('email')}\")
"
```

### 看客户端 Key

```bash
curl -s -H "x-api-key: <管理密钥>" http://127.0.0.1:8990/api/admin/client-keys | python3 -c "
import json,sys
for k in json.load(sys.stdin).get('keys', []):
    print(f\"id={k.get('id')} 名称={k.get('name')} 分组={k.get('group') or '-'} 禁用={k.get('disabled')} 引擎={k.get('cacheEngine')} {k.get('maskedKey')}\")
"
```

### 取某个 Key 的明文

```bash
curl -s -H "x-api-key: <管理密钥>" http://127.0.0.1:8990/api/admin/client-keys/<ID>/reveal
```

随时可取、无次数限制。面板上也能点：客户端 Key 页 → 点那一列脱敏 Key → 「查看 Key 明文」。

### 查可用模型（重要）

```bash
curl -s -H "x-api-key: <客户端KEY>" http://127.0.0.1:8990/v1/models | python3 -c "
import json,sys
for m in json.load(sys.stdin).get('data',[]): print(m.get('id'))
"
```

**模型名必须在这个列表里。** 用不存在的名字（例如 `claude-sonnet-4.8`）会报
`所有凭据均已禁用（6/6）` —— 那句文案是错的，真因是模型名不存在而非凭据被禁。
这一点排查时踩过三轮，见下面「已知的误导信息」。

---

## 已知的误导信息

### `所有凭据均已禁用（N/N）`

分子分母相等还说「全部禁用」，是自相矛盾的。真实原因是**没有凭据满足本次请求的筛选条件**，
可能是：

| 可能原因 | 怎么确认 |
|---|---|
| **模型名不存在**（最常见） | 用上面「查可用模型」核对 |
| 凭据确实被禁用 | 看凭据状态里的 `禁用=True` |
| 全在冷却中 | 看 `冷却=` 有秒数 |
| 分组不匹配 | Key 绑了分组 X 而凭据都不在 X 里 |
| RPM 超限 | 单账号限流开着且已打满 |

代码里那句文案只数了「未被 disabled 的条数」就当成「可用条数」印出来，真实原因一个都没报。
**这是个待修的 bug，不是你的配置问题。**

---

## 本次更新的行为变更

### 客户端 Key 轮换后旧明文立即失效

原先有 7 天宽限期，已移除（那个宽限期没有任何应急撤销手段，泄露的 Key 轮换后还能再用 7 天）。

**升级前检查**：如果之前轮换过 Key、某些下游还在用旧明文靠宽限期撑着，重建后它们会立刻 401。

```bash
grep -c prevKey /opt/kiro-rs-lyozc-8990/data/client_api_keys.json
```

输出 `0` 或报「无此文件/无匹配」就没事。有数字则说明有 Key 处于宽限期，
升级后那些旧明文立即失效，需要把下游换成新明文。

启动日志里会看到清理提示（有残留字段时）：

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml logs --tail=50 | grep 宽限
```

### 上游凭据的 WebSearch 已修

原先上游凭据的 WebSearch 把 SSE 当 JSON 缓冲、content-type 硬写成 `application/json`，
客户端解析不出 → 表现为「不返回结果」。验证：

```bash
curl -sD- -o /dev/null -X POST http://127.0.0.1:8990/v1/messages -H "x-api-key: <客户端KEY>" -H "content-type: application/json" -d '{"model":"claude-sonnet-4.5","max_tokens":64,"stream":true,"tools":[{"type":"web_search_20250610","name":"web_search"}],"messages":[{"role":"user","content":"Perform a web search for the query: rust latest version"}]}' | grep -i content-type
```

应返回 `text/event-stream`（修复前是 `application/json`）。

> **仍未解决**：Kiro 账号路径下，**不带 `type` 字段**的 web_search 工具不会被识别为
> 原生搜索，请求当普通聊天处理 → 无搜索结果。带 `type=web_search_20250305` 则正常。
> 详见「附录 B」。

### API Key 订阅源（新功能，默认关闭）

面板顶栏齿轮 → 「凭据来源 / API Key 订阅源」。拉下来不会自动开始轮询。

---

## 附录 A：查另一个实例的参数

不确定某个容器的 compose 参数时：

```bash
docker inspect kiro-rs-8991 --format '项目名={{index .Config.Labels "com.docker.compose.project"}}
服务名={{index .Config.Labels "com.docker.compose.service"}}
compose文件={{index .Config.Labels "com.docker.compose.project.config_files"}}
工作目录={{index .Config.Labels "com.docker.compose.project.working_dir"}}'
```

列出所有实例：

```bash
docker ps --format 'table {{.Names}}\t{{.Image}}\t{{.Ports}}\t{{.Status}}'
```

**注意**：Claude Code 实际连的是哪个端口要先确认，别在一个实例上测试却看另一个的日志
（这个坑踩过）。

---

## 附录 B：WebSearch 形态差异

Kiro 账号路径判定原生 web_search 工具的条件比参照项目严：

| 请求形态 | kiro-rs-lyozc | kiro2cc-proxy |
|---|---|---|
| `{"type":"web_search_20250305","name":"web_search"}` | ✅ 走 MCP 搜索 | ✅ |
| `{"type":"web_search_20250610","name":"web_search"}` | ✅ 走 MCP 搜索 | ✅ |
| `{"name":"web_search"}`（**无 type**） | ❌ 当普通聊天 | ✅ |
| `{"type":"custom","name":"web_search"}` | ❌ 当普通聊天 | ✅ |

实测对照（`claude-sonnet-4.5`）：

- 无 `type`：只返回 `"name":"web_search","type":"tool_use"` + `stop_reason: tool_use`，
  **无任何搜索结果**，日志里连一行 WebSearch 记录都没有
- 带 `type`：返回 `server_tool_use` + `web_search_tool_result`（10 条真实结果），
  日志出现 `检测到 WebSearch 工具，路由到 WebSearch 处理`

那个额外条件是有意加的（防止客户端自定义的普通工具恰好也叫 `web_search` 被误判），
所以不能简单放宽。**要定论还差一步**：确认 Claude Code 实际发的是哪种形态。

在 Claude Code 里正常触发一次 WebSearch，然后：

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml logs --tail=100 | grep -iE "检测到 WebSearch|处理 WebSearch"
```

- **有输出** → Claude Code 带了合规 `type`，它的 WebSearch 本该正常
- **无输出** → Claude Code 不带 `type`，即确认是这个差异

或打开面板 → 请求日志 → 找那次 WebSearch → 看请求体 `tools` 数组有没有 `type` 字段（更直接）。

---

## 附录 C：回滚

```bash
git -C /opt/kiro-rs-lyozc-8990 log --oneline -10
```

挑一个提交号回退（`<提交号>` 替换）：

```bash
git -C /opt/kiro-rs-lyozc-8990 checkout <提交号>
```

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml build
```

```bash
docker compose -p kiro8990 -f /opt/kiro-rs-lyozc-8990/docker-compose.fork.yml up -d
```

回到最新：

```bash
git -C /opt/kiro-rs-lyozc-8990 checkout main
```

> 回滚只退代码，不动 `./data/`。但**跨越「移除宽限期」那个提交往回退**时，
> 已被清理的 `prevKey` 字段不会恢复 —— 那些旧明文在新版启动时就已永久失效。

