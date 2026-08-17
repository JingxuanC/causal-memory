# 三层统一记忆架构设计

> **目标**：把 causal-memory 从"因果补充层"升级为"自给自足的完整记忆系统"。
>
> Agent 只挂一个 MCP server，就能同时获得事实召回 + 时序追踪 + 因果归因。
>
> **动机**：当前所有 benchmark 差距（LoCoMo 65% vs Mem0 92.5%，LongMemEval 61.8% vs 94.4%，Memora MPA 33.9% vs A-Mem 71.8%）都来自同一个瓶颈：不会存事实。另一个会话的 Memora 文档原话——"The fix is the same single piece of work in all three: an LLM distillation step at ingest"。
>
> **2026-07-31 合入说明**：本文写于 feat/hippocampus-architecture 分支，随定位重写（README "From slice to system"）合入 main。新增 §5.1 回应当日 agent-teardown 三篇深度分析（HeLa-Mem / Dreams API / OpenViking）提出的定位张力。

---

## 0. 当前状态

causal-memory 已经有：

| 层 | 能力 | benchmark 表现 |
|---|---|---|
| **因果记忆** | causal_edges + CSR graph + spreading activation | compaction survival +20.8pp ✅, agent ablation 67%→33% ✅ |
| **时序记忆** | valid_from / valid_to / event_time | Memora FAA 80.8% ✅（遗忘准确率是结构性优势） |
| **事实记忆** | ❌ 不存在 | LoCoMo 65%, LongMemEval 61.8% — 瓶颈 |

**缺的就是事实记忆** — "用户喜欢 TypeScript"、"项目用 Redis 7.2"、"API 端点是 /api/v1/users" 这种**扁平事实**。

---

## 1. 三层架构

```
┌──────────────────────────────────────────────────────────┐
│              causal-memory 统一记忆系统                     │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │ Layer 1: 事实记忆 (新增)                          │    │
│  │                                                  │    │
│  │  存什么: "用户喜欢 TypeScript" / "项目用 Redis"   │    │
│  │  怎么存: agent_facts 表 (key + value + embedding) │    │
│  │  怎么查: 语义相似度 + BM25                        │    │
│  │  工具: record_fact / search_facts                │    │
│  │  对标: Mem0 (但更轻量 — 不做 graph 层)            │    │
│  └──────────────────────────────────────────────────┘    │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │ Layer 2: 时序记忆 (已有)                          │    │
│  │                                                  │    │
│  │  存什么: valid_from / valid_to 时间窗口           │    │
│  │  怎么查: "3 月时用户是什么状态"                    │    │
│  │  对标: Zep / Graphiti                             │    │
│  └──────────────────────────────────────────────────┘    │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │ Layer 3: 因果记忆 (已有, 海马体式)                │    │
│  │                                                  │    │
│  │  存什么: "决策 A 导致了结果 B"                     │    │
│  │  怎么查: spreading activation + trace_cause_chain │    │
│  │  对标: 无 (独家 — prevented 负扩散 + SWR 巩固)    │    │
│  └──────────────────────────────────────────────────┘    │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │ 统一检索: search_memory(query)                   │    │
│  │  → 同时查三层, RRF (Reciprocal Rank Fusion) 融合  │    │
│  │  → agent 不需要知道该查哪一层                      │    │
│  └──────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────┘
```

---

## 2. Schema 设计

### 2.1 新增 agent_facts 表

```sql
CREATE TABLE IF NOT EXISTS agent_facts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key TEXT NOT NULL,              -- 事实类别: "preference" / "tech_stack" / "config" / ...
    value TEXT NOT NULL,            -- 事实内容: "TypeScript" / "Redis 7.2" / "/api/v1/users"
    scope TEXT NOT NULL DEFAULT 'user',  -- user / session / agent
    source TEXT NOT NULL DEFAULT 'agent', -- agent / user_feedback / system
    confidence REAL NOT NULL DEFAULT 0.8,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    valid_to INTEGER,               -- NULL = 有效, 非 NULL = 已过时 (同 causal_edges)
    embedding_model TEXT,           -- embedding 模型名 (用于版本管理)
    UNIQUE(key, value, scope)       -- 去重: 同一 key+value+scope 只存一条
);
CREATE INDEX IF NOT EXISTS idx_facts_key ON agent_facts(key);
CREATE INDEX IF NOT EXISTS idx_facts_scope ON agent_facts(scope);
CREATE INDEX IF NOT EXISTS idx_facts_valid ON agent_facts(valid_to);

-- 可选: embedding 存储 (当配置了 embedding endpoint 时)
CREATE TABLE IF NOT EXISTS agent_facts_embeddings (
    fact_id INTEGER PRIMARY KEY,
    model TEXT NOT NULL,
    embedding BLOB NOT NULL,        -- 序列化的 f32 数组
    FOREIGN KEY (fact_id) REFERENCES agent_facts(id)
);
```

### 2.2 和现有 schema 的关系

```
现有表:
  chunks (id, text, created_at)
  causal_edges (from_id, to_id, relation, confidence, ...)
  meta_causal_edges (...)

新增表:
  agent_facts (key, value, scope, confidence, valid_to, ...)
  agent_facts_embeddings (fact_id, model, embedding)
```

**不修改现有表**。agent_facts 是纯叠加，和 causal_edges 完全独立。

---

## 3. MCP 工具

### 3.1 新增工具

| 工具 | 做什么 | 参数 |
|---|---|---|
| `record_fact` | 记录一个事实 | key, value, scope, confidence |
| `search_facts` | 检索事实(语义或关键词) | query, scope, limit |
| `search_memory` | 统一检索(三层 RRF 融合) | query, task_tag, limit |

### 3.2 record_fact

```typescript
{
  name: "record_fact",
  description: "Record a factual piece of information for future retrieval. " +
    "Use for user preferences, tech stack details, configuration, project facts. " +
    "Do NOT use for causal relationships (use record_decision for those).",
  inputSchema: {
    type: "object",
    properties: {
      key: { type: "string", description: "Category: 'preference', 'tech_stack', 'config', 'project', ..." },
      value: { type: "string", description: "The fact: 'TypeScript', 'Redis 7.2', '/api/v1/users'" },
      scope: { type: "string", enum: ["user", "session", "agent"], default: "user" },
      confidence: { type: "number", default: 0.8 }
    },
    required: ["key", "value"]
  }
}
```

### 3.3 search_memory (统一检索)

```typescript
{
  name: "search_memory",
  description: "Search ALL memory types: facts, causal episodes, and temporal state. " +
    "Use this when you're not sure whether the information is a fact or a causal lesson. " +
    "Results are fused by Reciprocal Rank Fusion (RRF) across all memory layers.",
  inputSchema: {
    type: "object",
    properties: {
      query: { type: "string", description: "Natural language query" },
      task_tag: { type: "string", description: "Optional task filter for causal layer" },
      limit: { type: "integer", default: 10 }
    },
    required: ["query"]
  }
}
```

**RRF 融合逻辑**：

```
search_memory("Redis"):
  Layer 1 (facts):    ["Redis 7.2", "Redis 缓存配置"] → rank 1, 2
  Layer 3 (causal):   ["用 Redis → 缓存击穿", "改用 channel → 修复"] → rank 1, 2
  Layer 2 (temporal): valid 状态查询

  RRF score(fact) = 1/(60 + rank_in_facts)    = 1/61, 1/62
  RRF score(edge) = 1/(60 + rank_in_causal)   = 1/61, 1/62

  合并后按 RRF score 排序:
    1. [fact] Redis 7.2 (RRF=0.0164)
    2. [causal] 用 Redis → 缓存击穿 (RRF=0.0164)
    3. [fact] Redis 缓存配置 (RRF=0.0161)
    4. [causal] 改用 channel → 修复 (RRF=0.0161)
```

**agent 看到的输出**：

```
[unified] Found 4 memories across 2 layers:

📊 Facts (2):
  1. tech_stack: Redis 7.2
  2. config: Redis 缓存配置 (TTL 5 分钟)

🔗 Causal (2):
  3. [caching] used Redis →(caused)→ cache stampede
  4. [concurrency] used channel →(caused)→ fixed race condition
```

---

## 4. LLM Distill at Ingest

### 4.1 问题

当前 causal-memory 存的是 **raw turns**（"[date] speaker: message"）。Memora 文档指出这是 MPA 33.9% 的根因 —— A-Mem/LangMem 在 ingest 时用 LLM 蒸馏出结构化笔记，我们存原文。

### 4.2 方案：一次 LLM 调用同时提取三种记忆

参考 Vela 的 Reflector（一个 LLM 调用同时产出 Facts + Insights + L0 摘要）：

```
输入: 一段对话历史 (10-20 turns)

LLM prompt:
  "Analyze this conversation and extract:
   1. FACTS: Stable, useful information (user preferences, tech stack, config)
   2. DECISIONS: Choices made and their outcomes (what was decided → what happened)
   3. INSIGHTS: General lessons (what to do / not do in similar situations)

   Output JSON: { facts: [...], decisions: [...], insights: [...] }"

输出:
  facts:     [{key: "tech_stack", value: "TypeScript + Express"}, ...]
  decisions: [{decision: "used mutex", outcome: "deadlock", relation: "caused"}, ...]
  insights:  [{pattern: "avoid mutex in distributed systems", confidence: 0.8}, ...]

写入:
  facts → agent_facts 表
  decisions → causal_edges 表 (通过 record_decision)
  insights → meta_causal_edges 表 (通过 search_patterns 的挖掘)
```

### 4.3 CLI 命令

```bash
# 对一个 session 做离线 distill
causal-memory distill <session-dir> [--max-messages 50]

# 或在 record_decision 时自动触发
# (当 agent 记录了一条因果边, 顺便检查是否有值得提取的事实)
```

---

## 5. 和现有系统的对比

| 维度 | Mem0 | Zep | Letta | causal-memory (补齐后) |
|---|---|---|---|---|
| 事实记忆 | ✅ 核心 | ✅ 实体关系 | ✅ core memory | ✅ agent_facts |
| 时序记忆 | ⚠️ 弱 | ✅ 核心 (valid_from/to) | ❌ | ✅ valid_to + event_time |
| 因果记忆 | ❌ | ❌ | ❌ | ✅ **causal_edges + CSR graph** |
| 海马体架构 | ❌ | ❌ | ❌ | ✅ **DG + CA3 + CA1 + SWR** |
| 统一检索 | ❌ (只做事实) | ❌ (只做时序) | ❌ (只做自管理) | ✅ **RRF 三层融合** |
| LLM distill | ✅ 自动抽取 | ✅ 图谱构建 | ✅ sleep-time | ✅ **一次调用三种产出** |
| 压缩免疫 | ❌ | ❌ | ⚠️ archival | ✅ **+20.8pp compaction survival** |
| Agent 学习 | ❌ | ❌ | ❌ | ✅ **67%→33% repeat-mistake** |
| 遗忘管理 | ⚠️ 简单 | ✅ 时序窗口 | ❌ | ✅ **valid_to + SWR LTD/GC** |

**补齐后的定位**：

> causal-memory 是**唯一同时支持事实 + 时序 + 因果的统一记忆系统**，有海马体式激活扩散架构，有压缩免疫和 agent 学习的独家 benchmark 数据。

### 5.1 与 OpenViking / HeLa-Mem 的关系（2026-07-31 补充）

agent-teardown 当日三篇深度分析提出了一个真实张力：OpenViking（VLDB'26，27.7k★，Rust）用虚拟文件系统 + L0/L1/L2 分层加载把 LoCoMo 做到 80–83%、token 节省 34–91%，分析结论是"causal-memory 不应在事实召回上正面竞争"。本文的立场是"自给自足的完整记忆系统"。两者这样调和：

1. **事实层自建，但保持轻量。** `agent_facts` 是一张表 + embedding，不是重造 OpenViking 的数据库工程。目标是"挂一个 MCP server 就够用"，不是"在检索工程上打赢 27.7k★ 的项目"。LoCoMo 75–80% 的目标区间对标 Letta（74%），不预设追平 OpenViking 的 80–83%。
2. **存储底层可插拔。** 统一检索（RRF 融合）和因果层不绑定 SQLite 细节；如果用户已有 OpenViking / LanceDB 部署，事实层可以退化为对其存储的适配器，因果层作为上层语义增强运行。"完整系统"是默认形态，"因果增强层"是部署形态之一——两个叙事都成立，不冲突。
3. **直接吸收对手机制。** 分层加载（L0/L1/L2）、目录递归检索的可观察性（激活轨迹记录）、token budget 控制列入 roadmap；HeLa-Mem 的 Hebbian 共现权重作为兴奋侧补充（我们的 `prevented` 负扩散是抑制侧，完整系统需要两者）。
4. **巩固安全模型对齐 Dreams API。** SWR consolidate 从直接改图改为产出 delta + clone（输入不可变），并加 `instructions` 引导参数。

**差异化不受影响**：`prevented` 负扩散（GABA 抑制性类比，无任何系统实现）、compaction survival（+20.8pp）、SWR 回放式巩固（非文本蒸馏）、类型化因果语义（caused/enabled/prevented ≠ 共现频率）。

**新增行动项**（合并三篇分析后去重，已同步进 [roadmap](../roadmap.md)）：GC 三重复合判据（结构弱 AND 时间休眠 AND 零访问）、翻转路径标记（区分 seed 直接命中 vs spreading 浮出）、正式消融实验（SWR / spreading / prevented 各砍一次）、token 效率测量。

---

## 6. 实施计划

### Phase 1：事实层 (2 天) — ✅ 已完成 2026-07-31

- [x] `agent_facts` + `agent_facts_embeddings` schema (store.rs / migrate.rs v6)
- [x] `record_fact()` / `search_facts_bm25()` / `search_facts_semantic()` / `invalidate_fact()` / `invalidate_other_facts_for_key()` / `list_facts()` store 方法
- [x] `record_fact` MCP 工具 (server.rs, 含 `replace_same_key` 退休旧值)
- [x] `search_facts` MCP 工具 (server.rs, 语义 → BM25 → 列表 三档降级)
- [x] 单元测试 ×5 + migration v5→v6 测试 + mcp_e2e 全链路覆盖（212 tests 全绿）

### Phase 2：统一检索 (1 天) — ✅ 已完成 2026-07-31

- [x] `rrf_fuse()` 实现 (RRF k=60, 跨层共识加分, server.rs)
- [x] `search_memory` MCP 工具 (server.rs, 语义/BM25 双模式共享一次 query embedding)
- [x] 测试：RRF 单元测试 ×3（排序/跨层共识/并集）+ mcp_e2e 统一检索覆盖

### Phase 3：LLM Distill (1 天) — ✅ 已完成 2026-07-31

- [x] `distill` CLI 命令 (session JSON/目录 → facts + lessons/events 分流写入)
- [x] facts/preferences → agent_facts (supersedes 退休旧值), lessons/events → record_distilled
- [x] 测试：load_session 解析 + retire_superseded_facts（key/scope 隔离 + 阈值）

### Phase 4：Benchmark 重跑 (半天) — ✅ 已完成 2026-07-31

- [x] LoCoMo 用 distill 模式重跑 — **69.6% vs raw 64.2% (+5.4pp)**，1,986 题 0 错误（run_distill_full_20260730）
- [x] LongMemEval 用 distill 模式重跑 — **69.6% vs raw 61.8% (+7.8pp)**，500 题 0 错误，knowledge-update 76.9%→85.9%（supersedes 机制主场兑现）
- [x] Memora weekly 用 distill 模式重跑 — 10/10 persona 事实层全量：MPA 33.9%→**46.8% (+12.9pp)**，平均 FAA 72.1%
- [x] 对比 raw vs distill 的 MPA 差异 — 三个 benchmark 一致正向，详见 docs/benchmarks/*.md

**总计约 4.5 天。**

---

## 7. 对现有 12 个工具的影响

```
当前 10 个工具 + 海马体集成:
  record_decision / search_causal / trace_cause / trace_cause_chain
  invalidate_decision / search_patterns / causal_directory
  intervention_query / counterfactual_query / reconstruct_lesson

新增 3 个工具:
  record_fact       — 记录事实 ("用户用 TypeScript")
  search_facts      — 检索事实 (语义或关键词)
  search_memory     — 统一检索 (三层 RRF 融合)

总共 13 个工具
```

### 工具使用指南 (给 agent 的 CLAUDE.md)

```markdown
## Memory Integration

You have THREE types of memory. Use the right one:

### For FACTS (stable information):
- "用户喜欢 TypeScript" → record_fact(key="preference", value="TypeScript")
- "项目用 Redis 7.2" → record_fact(key="tech_stack", value="Redis 7.2")
- 查事实 → search_facts("Redis") 或 search_memory("Redis")

### For DECISIONS and their OUTCOMES (causal lessons):
- "用 mutex 导致了死锁" → record_decision(decision="mutex", outcome="deadlock", relation="caused")
- 查因果教训 → search_causal("concurrency") 或 search_memory("mutex")
- 追溯失败原因 → trace_cause("deadlock crash")

### When UNSURE which type:
- search_memory(query) — searches ALL layers simultaneously

### Before any non-trivial decision:
1. Call search_memory to check all past experience
2. If relevant, call trace_cause_chain for deep causal analysis
```

---

## 8. 为什么这会让 causal-memory "完善"

### 8.1 Benchmark 提升

当前瓶颈是"不会存事实"。补齐后：
- **LoCoMo**: 65% → ~75-80%（distill 出的事实 + BM25 召回）
- **LongMemEval**: 61.8% → ~70-75%（多 session 事实合成）
- **Memora MPA**: 33.9% → ~50-60%（distill 替代 raw turns）
- **Memora FAA**: 80.8% → 保持或提升（valid_to 时序过滤仍然有效）
- **Compaction survival**: 不变（因果层不受影响）
- **Agent ablation**: 不变（因果学习不受影响）

### 8.2 用户体验

用户只需要：
1. 挂一个 MCP server（不需要 Mem0 + causal-memory 两个）
2. 调 search_memory（不需要区分该查事实还是查因果）
3. distill 自动提取所有类型（不需要手动 record 每条事实）

### 8.3 差异化更强

当前差异化："唯一存因果边的记忆层"（但需要和 Mem0 配合使用）。

补齐后差异化："唯一同时支持事实 + 时序 + 因果的统一记忆系统，有海马体式激活扩散"（不需要配合，自给自足）。

**这个定位比"因果补充层"强得多** —— 从"工具的一个配件"变成"完整解决方案"。

---

## 参考资料

- Memora benchmark 文档: `docs/benchmarks/memora.md` (另一个会话写)
- Vela Reflector 设计: `frameworks/vela-shopify/03-memory-reflect-decay.md`
- Mem0 论文: arXiv:2504.19413
- Hippocampus 设计: `docs/hippocampus-design.md`
- insights/10: 记忆公司赛道分析
- insights/11: 因果状态库设计
- HeLa-Mem 深度分析: agent-teardown `papers/daily/2026-07-30-helamem-analysis.md`（消融数据 + 五个行动项）
- Dreams API 深度分析: agent-teardown `papers/daily/2026-07-30-dreams-api-analysis.md`（输入不可变原则 + 伪代码）
- OpenViking 深度分析: agent-teardown `papers/daily/2026-07-30-openviking-analysis.md`（分层加载 + 定位张力）
