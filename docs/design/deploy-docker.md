# Docker 部署 —— 记忆同步服务器（git-sync https remote）

> 配套设计：[memory-git-sync.md](memory-git-sync.md) §7 P1「https remote +
> cloud register」。本文用一个镜像把 **MCP HTTP 服务 + 对象仓库
> （/agents/<id>/objects|refs）+ agent 注册端点** 一起跑起来。
> 仓库现有 `Dockerfile` 是 AMC 评测镜像（`causal-memory-amc`），与本文无关；
> 同步服务器跑的是 CLI 二进制 `causal-memory http`。

## 1. 镜像（Dockerfile.sync）

```dockerfile
# ── Builder: 编译 CLI（含 http / git-sync 子命令） ───────────────────
FROM rust:1.92-trixie AS builder
WORKDIR /build
COPY . .
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS
RUN cargo build --release --bin causal-memory

# ── Runtime: 只要二进制 + CA 证书（reqwest native-tls-vendored 静态
#    OpenSSL，无需系统 libssl） ─────────────────────────────────────────
FROM debian:trixie-slim
COPY --from=builder /build/target/release/causal-memory /usr/local/bin/causal-memory

# 同步仓库根（agents/<id> 都在这下面）+ 可选的本地因果库
ENV CAUSAL_MEMORY_SYNC_ROOT=/data/sync \
    CAUSAL_MEMORY_DB=/data/causal.db \
    CAUSAL_MEMORY_ALLOWED_HOSTS=localhost,127.0.0.1,::1,host.docker.internal
VOLUME ["/data"]
EXPOSE 9938
ENTRYPOINT ["causal-memory"]
CMD ["http", "--port", "9938", "--host", "0.0.0.0"]
```

```bash
docker build -f Dockerfile.sync -t causal-memory-sync .
```

## 2. 运行（三个 token 各司其职）

| 环境变量 | 作用 | 建议 |
|---|---|---|
| `CAUSAL_MEMORY_ADMIN_TOKEN` | 管理面：`cloud register/list/revoke` | 必设（≥16 字符），用 secret 注入 |
| `CAUSAL_MEMORY_HTTP_AUTH_TOKEN` | 共享读 token（无 per-agent token 的 agent 回落到它）+ admin 回落 | 设了 admin 后可不设；`/metrics` 也靠它 |
| `CAUSAL_MEMORY_SYNC_ROOT` | 对象仓库根（默认 `~/.local/share/causal-memory/sync`） | 容器内挂 /data |

```bash
docker run -d --name cm-sync --restart unless-stopped \
  -p 9938:9938 \
  -v cm-data:/data \
  -e CAUSAL_MEMORY_ADMIN_TOKEN="$(openssl rand -hex 24)" \
  -e CAUSAL_MEMORY_HTTP_AUTH_TOKEN="$(openssl rand -hex 24)" \
  causal-memory-sync

curl -fsS http://localhost:9938/healthz          # → ok
docker logs cm-sync | grep "Git sync"            # → 端点与仓库根
```

### docker-compose.yml（完整形态）

```yaml
services:
  cm-sync:
    build:
      context: .
      dockerfile: Dockerfile.sync
    restart: unless-stopped
    ports: ["9938:9938"]
    volumes: ["cm-data:/data"]
    environment:
      CAUSAL_MEMORY_SYNC_ROOT: /data/sync
      CAUSAL_MEMORY_DB: /data/causal.db
      # 生产：从 .env / secret 管理器注入，不要写死在仓库
      CAUSAL_MEMORY_ADMIN_TOKEN: ${CM_ADMIN_TOKEN:?set CM_ADMIN_TOKEN in .env}
      CAUSAL_MEMORY_HTTP_AUTH_TOKEN: ${CM_AUTH_TOKEN:?set CM_AUTH_TOKEN in .env}
volumes:
  cm-data:
```

## 3. 客户端接入（宿主机 / 别的机器）

```bash
# 1) 在服务器上开一个 agent 账号，拿到 per-agent token 并存进本地配置
causal-memory cloud register athena http://<host>:9938 --db ~/mem/a.db

# 2) 正常打快照并推上去
causal-memory commit -m "学会：灰度优于直推" --db ~/mem/a.db
causal-memory push athena --db ~/mem/a.db          # → pushed 1 commit(s)

# 3) 换台机器（Docker 里也行），注册后按 agent_id 直接 clone 全量上下文
causal-memory cloud register athena http://<host>:9938 --db ~/mem/b.db
causal-memory clone athena --db ~/mem/b.db         # → bootstrap 摘要 + 最近 lessons

# 4) 吊销：agent 离职/泄露 → token 立即失效（回落共享 token，若设了）
causal-memory cloud revoke athena http://<host>:9938
```

宿主机容器内联测（MCP Host 白名单已含 host.docker.internal）：
`docker exec -it cm-sync causal-memory http` 外的工具直接对
`http://host.docker.internal:9938` 操作即可。

## 4. 安全默认值

- **agent 隔离 = 仓库隔离**：`/agents/<id>/` 是独立目录，id 白名单
  `[A-Za-z0-9._-]` ≤64 字符，路径穿越在路由层被拒。
- **token 即权限**：per-agent token（register 下发，48 hex CSPRNG）优先于
  共享 token；revoke 删 token 文件即失效。token 明文存 `<sync root>/
  agents/<id>/token` —— 挂载卷注意权限（建议容器内非 root，`USER` 指令
  自行加）。
- **无 token = 开放**：dev/内网形态；公网部署必须设 admin token（并建议
  admin ≠ 共享 token）。TLS 由前面的反代（nginx/caddy/TLS 网关）终结，
  该镜像不内置 TLS。
- 备份 = 整个 `/data` 卷（对象仓库全量快照，与文件 remote 同构，直接
  rsync/tar 即可）。

## 5. 验证清单

```bash
curl -fsS http://localhost:9938/healthz                                # ok
curl -s http://localhost:9938/agents -H "Authorization: Bearer $CM_ADMIN_TOKEN"  # agents 列表
causal-memory cloud register bot http://localhost:9938 --db /tmp/ci.db # mint token
causal-memory commit -m "ci" --db /tmp/ci.db && causal-memory push bot --db /tmp/ci.db
causal-memory clone bot --db /tmp/ci2.db                               # 命中同一教训
```
