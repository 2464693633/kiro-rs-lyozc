# 云端构建和镜像发布

GitHub Actions 负责编译和发布，服务器只拉取镜像。默认使用 GHCR（GitHub Container Registry），无需配置 Docker Hub 账号或个人令牌。

## 默认发布到 GHCR

工作流使用 `packages: write` 和 Actions 自带的 `github.token` 登录 `ghcr.io`。镜像地址由仓库名自动生成并转为小写：

```text
ghcr.io/2464693633/kiro-rs-lyozc
```

- 推送 `main` 或 `master`：发布 `beta-<提交 SHA 前六位>`、`beta` 和对应的架构标签。不会更新 `latest`。
- 手动运行 `docker-build.yaml` 并填写版本：发布该版本、`latest` 和架构标签。
- 推送 `v*` 标签或手动运行 `release.yaml`：使用 `Dockerfile.release` 打包云端已编译的静态程序，发布版本镜像、`latest` 和 GitHub Release。版本必须与 `Cargo.toml` 一致。
- 支持 `linux/amd64` 和 `linux/arm64`，由各自原生 runner 构建，再合并为多架构镜像。

GHCR 包的可见性独立于仓库可见性。首次发布的新包默认私有；若希望服务器免登录拉取，在 GitHub 的个人主页进入 **Packages → kiro-rs-lyozc → Package settings → Change visibility → Public**。公开仓库本身不代表镜像包已公开。私有包则需在服务器使用具有 `read:packages` 权限的凭据登录。

镜像带有源码地址和提交 SHA 的 OCI 标签，可检查 `org.opencontainers.image.revision`，确认拉取的镜像包含目标修改。

## 可选 Docker Hub

在仓库 **Settings → Secrets and variables → Actions** 配置：

| 类型 | 名称 | 值 |
| --- | --- | --- |
| Variable | `IMAGE_REGISTRY` | `docker.io` |
| Secret | `DOCKERHUB_USERNAME` | Docker Hub 登录用户名 |
| Secret | `DOCKERHUB_TOKEN` | 具有镜像推送权限的 Docker Hub 访问令牌 |
| Variable（可选） | `DOCKERHUB_NAMESPACE` | 镜像命名空间；默认使用小写 GitHub 仓库所有者名 |

此时镜像地址为 `<namespace>/kiro-rs`。这是选择发布目标，不是同时向两个仓库发布。删除 `IMAGE_REGISTRY` 变量或改为 `ghcr.io` 即恢复默认。不要把密码或令牌提交到代码，也不要把 Docker Hub 令牌用于 GHCR。

## 服务器拉取

在使用本仓库 `docker-compose.yml` 的目录中，将实际发布的地址写入 `.env`：

```dotenv
KIRO_RS_IMAGE=ghcr.io/2464693633/kiro-rs-lyozc:beta
```

生产环境建议改用已验证的固定提交标签或镜像摘要，再运行：

```sh
docker compose pull kiro-rs
docker compose up -d kiro-rs
```

Compose 默认镜像仍是上游的 `zyphrzero/kiro-rs:latest`，因此需要设置 `KIRO_RS_IMAGE` 才会使用本仓库构建。实际服务名、端口、卷挂载以服务器现有 Compose 为准；不要覆盖已有配置或另一个实例。
