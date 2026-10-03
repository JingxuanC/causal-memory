# Cloud Context Restore — Session Commit/Archive 设计方案

> Status: proposal (未实施)
> Schema target: v16
> 参考: OpenViking `volcengine/OpenViking` 的 session lifecycle
> 关联: [counterfactual-rung3.md](counterfactual-rung3.md)（fork 的正交概念）、
> [unified-memory-design.md](unified-memory-design.md)（one graph, one engine）、
> [roadmap.md](../roadmap.md)（Backup/restore tooling、multi-tenant 两条 open 项）

## 1. 结论先行

**fork 不是上下文恢复，也不该被拿来当恢复主路径。** 云端上下文恢复的正确
参照是 OpenViking 的 **session commit/archive 闭环**：

```
session_logs → commit_session（同步归档原始轮次 + 异步生成 L0/L1 摘要、提取记忆、写 memory_diff）
             → restore_session（按 detail_level 逐层展开）
             → 云同步（对象存储 + /mcp 鉴权 + 多租户）
```

本项目已具备全部"原料"，缺的是把 `session_logs` 从 **audit-only 表** 升级为
**可 commit/archive/restore 的恢复层** 的那一段编排。

## 2. 为什么 fork 不能当恢复路径

- `decision_forks`（schema v14）是 **自然实验图**：两条 valid 因果边共享同一
  `context_fingerprint` 但决策文本不同，供 `counterfactual_query` 渲染
  「🔀 Same-context branches」做配对裁决。
- `context_text` 被明确设计为 **abduction substrate**（短世界状态描述），
  用于指纹配对与审计展示，**不是完整上下文快照**。它最多贡献"世界状态锚点"。
- 结论：fork 与上下文恢复是正交能力，互补但不替代。

## 3. OpenViking 的可借鉴点（已核对源码 + 官方文档）

| 机制 | OpenViking 做法 | 本项目对应/缺口 |
|---|---|---|
| Session lifecycle | Create → Interact → **Commit** | `session_logs` 只有 Interact 落盘，无 Commit |
| 同步归档 | commit 写 `messages.jsonl` 并清空当前消息 | 缺 archive 产物；`session_logs` 是审计表 |
| 异步摘要 | 生成 `.abstract.md` / `.overview.md` | 缺；distill 只做记忆提取不做会话摘要 |
| 记忆提取 + diff | 写 `memory_diff.json`（adds/updates/deletes，带 before/after） | distill 写 chunks/edges，但无 diff 审计产物 |
| 分层加载 | L0 256 字符 / L1 4000 字符 / L2 原始全文 | **已有** `detail_level` l0/l1/l2 + `max_tokens`，可直接复用 |
| 云存储 | `viking://` 虚拟文件系统 | 无对象存储层；`export`/`import`（JSONL）可作序列化基础 |

本项目不照搬 `viking://` 文件系统抽象，而是把 OpenViking 的 **session 闭环**
落到已有的 SQLite + JSONL 基座上，保持 "one graph, one engine" 的定位。

## 4. Schema（v16）：`session_archives`

新增一张 append-only 表，作为 `session_logs` 的归档视图。物理上**不删除**
`session_logs`（它是审计原料），而是给归档加一个状态位。

```sql
-- 会话归档：session_logs 的 commit 产物，分层摘要 + 原始轮次 + memory diff。
-- append-only；status 从 committing → committed / failed。
CREATE TABLE IF NOT EXISTS session_archives (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL,          -- 对应 session_logs.session_id
    task_tag TEXT,
    archive_uri TEXT,                      -- 云存储 URI；本地模式为 NULL
    l0_abstract TEXT,                      -- ≤256 字符一句话摘要
    l1_overview TEXT,                      -- ≤4000 字符结构化概览
    messages_json TEXT,                    -- 原始轮次 JSONL（speaker/text/turn_index/event_time）
    memory_diff_json TEXT,                 -- {adds[],updates[],deletes[]}，每条带 before/after
    status TEXT NOT NULL DEFAULT 'committing'
        CHECK(status IN ('committing','committed','failed')),
    error TEXT,
    committed_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_session_archives_session ON session_archives(session_id);
CREATE INDEX IF NOT EXISTS idx_session_archives_status ON session_archives(status);
```

配套：`session_logs` 增加 `archived_at INTEGER`（NULL = 未归档），通过
`migrate_to_v16` 的 `ALTER TABLE ADD COLUMN` 追加。该列让"哪些 turn 已归档、
哪些还是活跃会话"可查询，也支持恢复时只回放未归档部分。

## 5. 工具接口（新增 2 个 MCP 工具，第 18/19 个）

### 5.1 `commit_session`

参数：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | i64 | ✅ | 要归档的会话 |
| `task_tag` | string | | 会话主题标签，写入归档行 |
| `extract` | bool | | 默认 true：异步走 LLM 摘要 + 记忆提取；false = 仅归档原始轮次 |

行为（两阶段，与 OpenViking 对齐）：

1. **Phase 1 同步**：`session_turns(session_id)` 读出全部轮次 → 写
   `session_archives`（`status='committing'`，`messages_json` 落盘）→
   `UPDATE session_logs SET archived_at=now WHERE session_id=?`。立即返回
   `{status, archive_id, session_id, archive_uri, task_id}`，不阻塞 agent。
2. **Phase 2 异步**（`extract=true` 时）：
   - 复用 `distill` 管线做记忆提取（长期记忆进 chunks/edges/facts）；
   - 复用 LLM 生成 `l0_abstract` / `l1_overview`（新提示词，见 §8）；
   - 记录本次提取的增量到 `memory_diff_json`（复用 distill 的 item 流，
     每条带 before/after；无 before 视为 add）；
   - 成功 → `status='committed'`；失败 → `status='failed'` + `error`。

### 5.2 `restore_session`

参数：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | i64 | ✅ | 或按 `archive_id` 二选一 |
| `archive_id` | i64 | | 指定某次归档（默认取最近一次 committed） |
| `detail_level` | string | | `l0`/`l1`/`l2`，默认 `l1`（复用已有语义） |
| `max_tokens` | int | | 与 `search_causal`/`search_memory` 一致 |

行为（只读）：

- `l0` → 返回 `l0_abstract`；
- `l1` → 返回 `l1_overview`（含 Primary Request / Key Concepts / Pending Tasks，
  见 §8）；
- `l2` → 返回 `messages_json` 全量原始轮次（受 `max_tokens` 截断）。

恢复**不做**自动重灌（不隐式调用 `remember`），只把上下文交还给 agent
决定如何使用——保持"恢复是只读展开"的边界，避免与写入路径的 gatekeeping 语义纠缠。

## 6. 分层加载

复用已有 `detail_level`（l0/l1/l2）与 `max_tokens` 预算机制，不新造概念：

- **L0**：归档时由 LLM 生成的 ≤256 字符摘要，用于快速过滤/向量检索；
- **L1**：≤4000 字符结构化概览，用于 rerank 与内容导航；
- **L2**：`messages_json` 原始全文，按需加载。

这与 roadmap 已交付的 "Layered loading + token budget" 是同一条路：检索池
（chunks）和归档池（session_archives）共享一套分层词汇，降低用户心智成本。

## 7. 云同步与安全

分两半，可独立推进：

1. **对象存储适配层**（可插拔）：
   - trait `ArchiveSink { fn put(&self, archive: &SessionArchive) -> Result<String /*uri*/> }`；
   - 三个实现：`LocalDir`（写 `session_archives/<id>/` 下的 jsonl + md）、
     `S3Compat`（AWS SDK 或手写 S3 签名，最小依赖）、`Http`（POST 到既有
     `export` 端点）；
   - `export_jsonl` 的 JSONL 格式是序列化基础，archive 在该格式上追加
     `l0/l1/memory_diff` 三个 header 字段即可向后兼容。
2. **`/mcp` 鉴权 + 多租户**（roadmap v1.0 两条 open 项，与本方案合并推进）：
   - 复用 `http_auth::protected` 的 bearer 中间件思路，给 `/mcp` 加
     `CAUSAL_MEMORY_HTTP_AUTH_TOKEN` 门禁（rmcp Streamable HTTP 需要在握手
     前拦截，需评估 rmcp 2.2.0 的 middleware 钩子）；
   - 多租户沿用 `agent_facts.scope` 的 colon 命名空间先例
     （`tenant:acme`），`session_archives.session_id` 全局唯一即可，租户隔离
     交给调用方自己的 store 分库（AMC server 已是 one store per user_id 的先例）。

## 8. L0/L1 提示词契约（新）

新增一个 `SessionSummarizer`（与 `distill` 的 LLM 路径复用同一 HTTP 客户端），
输出固定结构，保证 `restore_session` 的 L1 可机器解析：

```
**One-line overview**: [Topic]: [Intent] | [Result] | [Status]
## Analysis
## Primary Request and Intent
## Key Concepts
## Pending Tasks
```

L0 = One-line overview；L1 = 完整结构体（截断到 4000 字符预算）。

## 9. 分阶段实施

| 阶段 | 内容 | 验收 | 规模 |
|---|---|---|---|
| **P1 最小闭环** | schema v16 + `commit_session`(extract=false) + `restore_session`(l2) | 本地 commit→restore roundtrip 轮次保真 | 2–3 天 |
| **P2 摘要 + diff** | LLM 摘要 + memory_diff + l0/l1/l2 分层 | 分层输出符合长度预算；diff 可回放 | 2–3 天 |
| **P3 云 + 安全** | ArchiveSink + `/mcp` 鉴权 + 多租户命名 | archive_uri 可上传/下载；未授权 401 | 3–5 天 |

每阶段独立可交付，P1 不依赖 LLM 配置即可跑通（与 `remember` 的 no-LLM 降级一致）。

## 10. 与现有代码的衔接点

| 现有代码 | 复用方式 |
|---|---|
| `store/write.rs::log_session_turn` | 已有写入，不改 |
| `store/write.rs::session_turns` / `session_date` | Phase 1 读出原始轮次 |
| `store/write.rs::mark_session_distilled` | 思路同款，新增 `mark_session_archived` |
| `memory/ops.rs::remember` + `distill` | Phase 2 记忆提取管线复用 |
| `memory/ops.rs::search_memory` 的 `detail_level`/`max_tokens` | 分层加载语义复用 |
| `cli/src/commands/io.rs::export_jsonl`/`import_jsonl` | 云同步序列化基础 |
| `migrate.rs`（当前 `SCHEMA_VERSION=15`） | 新增 `migrate_to_v16` |
| `cli/src/server/tools.rs` | 新增 2 个 tool handler |
| `cli/src/http_auth.rs::protected` | `/mcp` 鉴权中间件复用思路 |

## 11. 验收标准（合并进 CI）

1. **保真**：commit → restore(l2) roundtrip 后，`(speaker, text, turn_index)` 三元组完全一致。
2. **分层**：l0 ≤ 256 字符、l1 ≤ 4000 字符；l2 不受摘要预算影响。
3. **幂等**：同一 session 重复 commit 不产生重复 `messages_json`（`archived_at` 已置位则跳过或覆盖同 archive）。
4. **回归**：现有 368 tests + `fork_eval` 无回归（fork 逻辑与 archive 完全隔离）。
5. **安全**：P3 后 `/mcp` 未带 token 返回 401，健康探针保持开放。

## 12. 与 fork 的最终关系

一句话：**fork 负责"相同世界状态下不同决策哪个更好"，archive 负责"上次会话
说了什么、做了什么"**。前者是 counterfactual 证据，后者是 context 快照。
`context_text` 可作为 archive 摘要中的一个"世界状态锚点"字段（可选），
把同一 task_tag 的多个会话串起来，但它永远不是恢复的主路径。

## 13. 明确不做

- 不引入 `viking://` 式通用虚拟文件系统（超出本项目 one-graph 定位）；
- 不做自动"重灌"式恢复（restore 只读，重灌交给 agent 显式调用 `remember`）；
- 不在 P1/P2 引入对象存储依赖（本地 JSONL 先行，云同步是 P3 的可插拔扩展）。
