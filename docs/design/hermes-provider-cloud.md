# Hermes Provider 云模式 — 设计（P2 剩余项）

> Status: proposal（待评审）
> 关联: [memory-git-sync.md](memory-git-sync.md)（commit/checkout/https remote/cloud）、
> [deploy-docker.md](deploy-docker.md)（服务器部署）、[commercialization.md](../commercialization.md)
> 目标：**Hermes 跑在任意机器上，session 结束自动把记忆推到自己 agent 命名空间
> 的云仓库；换机器 clone 即恢复全部因果上下文。**

## 1. 结论先行

**两段式，中间是 CLI（已是事实上的集成面）：**

```
Hermes（agent 跑的地方）                    causal-memory 服务器（云端）
┌──────────────────────────────┐          ┌──────────────────────────┐
│ gateway 会话（record 教训 →   │  MCP     │ :9938  http（/mcp +      │
│ causal.db，本地 causal-memory │─────────▶│  /agents/<id>/objects    │
│ 服务 / CLI）                 │          │  + register/list/revoke) │
│        │                    │          └──────────────────────────┘
│  cadence 触发（定时 / 钩子）   │
│        ▼                    │
│  causal-memory session-commit │
│  [-m <L0>|--l0-llm] --push    │── https + bearer ──▶ 云端 agents/<id>
│  <agent_id>                   │   （token 由 cloud register 下发）
└──────────────────────────────┘
```

Hermes 侧**不需要新二进制**：因果记忆已通过本地 causal-memory 服务
（MCP）写入同一个 `causal.db`；自动 commit 只需要一个「会话结束/定时」
触发器去调 CLI。本设计只解决**触发器从哪来** + **agent_id/token 怎么配**。

## 2. 两条触发路线（选 A 做 MVP，B 是上游诉求）

### A. Hermes cron 定时 commit（推荐 MVP，零跨仓改动）
Hermes 的 `cronjob`（或系统 launchd）每小时/每日跑：

```
causal-memory session-commit <最近会话导出> --l0-llm --push <agent_id> --db <hermes 用的 causal.db>
```

- 优点：不动 Hermes 核心；`session-commit` 自带 `nothing to commit` 幂等，
  空转零成本；`--l0-llm` 在配了 LLM keys 时自动生成摘要消息。
- 缺点：不是「严格 on_session_end」，而是有界滞后（≤1 周期）。
  记忆是快照提交，滞后一个周期可接受（与 session_archives 的异步
  L0/L1 生成同哲学）。
- 会话素材来源：Hermes session DB（`session_search` 已有导出路径）或
  `distill` 的 session 目录约定；MVP 可直接让 cron 把「本小时有记录的
  session」文件名传给 session-commit（无文件也能 commit 存量教训，
  见 session-commit 的 parse-advisory 设计）。

### B. Hermes memory-provider ABC 加 on_session_end 钩子（上游改动，后续）
Hermes `plugins/memory/<name>/` 的 `MemoryProvider` ABC 现有
`sync_turn / prefetch / shutdown`，无会话结束信号。要「严格端上提交」需
给 ABC 加可选 `on_session_end(summary: Option<&str>)`，causal-memory
provider 实现 = 组装 L0 + 调 CLI session-commit。这是对 Hermes 仓库的
通用扩展（其他 provider 也受益），单独提 PR，不与本仓库耦合。

**推荐：先 A 落地跑通，B 作为对 Hermes 的上游提案。**

## 3. agent_id / token 配置流（一次，之后全自动）

```
1) 服务器已部署（Docker 或裸跑），CAUSAL_MEMORY_ADMIN_TOKEN 已设。
2) Hermes 机器：
   causal-memory cloud register athena https://cm.example.com --db <hermes causal.db>
   → 拿到 per-agent token，写入 <db>.cm/config.json remotes.athena {url, token}
3) 之后所有 push/pull/clone <agent_id> 自动带 bearer，无需再输 token。
4) 换机器：同一命令 register（token 轮换）→ clone athena 全量恢复。
```
Hermes 多实例/多用户：每个 (profile, user) 一个 agent_id（如
`hermes-<profile>-<user_hash>`），租户隔离沿用 server 的
`/agents/<id>/` 目录隔离 + token 隔离 —— Hermes 侧零新逻辑。

## 4. 安全

- token 明文在 `<db>.cm/config.json` —— 与 db 同级敏感，沿用本地文件
  权限；服务器端 TLS 由反代终结（deploy-docker.md §4）。
- 快照明文含记忆原文（redact: false 是内部同步语义，设计已定）——
  云端仓库权限 = token 权限；revoke 即刻生效（删 token 文件）。
- cadence 触发用 Hermes cron 的 no_agent + deliver=local 形态，脚本只
  push 已记录的教训，不引入新的 LLM 调用面（除非 --l0-llm）。

## 5. 验收（A 路线 MVP）

```bash
# Hermes 机器：register 一次
causal-memory cloud register athena https://cm.example.com
# 模拟 session 结束
causal-memory record "X" "Y" --db ~/.hermes/.../causal.db   # Hermes 正常路径
causal-memory session-commit --l0-llm --push athena          # → pushed
# 另一台机器
causal-memory cloud register athena https://cm.example.com
causal-memory clone athena                                   # → 命中同一教训
```

## 6. 不做（明确边界）

- 不做 Hermes 仓库改动（MVP）；B 路线以独立提案形式给上游。
- 不做实时双向 sync/websocket——快照 + cadence 已覆盖「agent 上下文
  跨位置可用」的诉求。
- 计量/计费（bootstrap、检索量）不在本设计内，见 commercialization。
