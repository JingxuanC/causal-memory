# 设计谱系：从论文 → 拆解 → 代码

> `causal-memory` 不是凭空设计的。它的每一行代码、每一个 schema 字段、每一个 API 命名，都可以追溯到 `agent-teardown` 的某篇拆解笔记或某篇论文。
>
> 本文档是一张**设计溯源图**：三层映射，从理论基础到工程实现。

---

## 三层架构

```
Layer 1: 论文（神经科学 / 认知心理学 / 形式化因果推断）
    ↓ 提供理论框架
Layer 2: insights（agent-teardown 的拆解笔记）
    ↓ 抽象为架构决策
Layer 3: causal-memory（代码实现）
```

| 层级 | 做什么 | 产物 |
|---|---|---|
| **论文** | 回答「大脑/人类如何记忆和推理因果？」 | 理论模型（CLS、因果图、反事实模拟） |
| **insights** | 回答「现有 Agent 框架在记忆上有什么缺陷？」 | 系统拆解、失效模式、设计模式 |
| **代码** | 回答「如何用工程手段填补这些缺陷？」 | SQLite schema、MCP tools、Rust 实现 |

---

## 第一层：论文 → Insights（理论输入）

### 神经科学层

| 论文 | 核心发现 | 映射到 insights | 代码体现 |
|---|---|---|---|
| **Kumaran 2016** CLS 理论 | 大脑有两套记忆系统（海马体快速 episodic + 新皮层慢速 semantic） | `insights/11-causal-state-store.md` §1.2：「Agent 记忆也需要双系统」 | `causal_edges` 表（episodic，不压缩）+ `meta_causal_edges` 表（semantic，离线提炼） |
| **Schapiro 2017** 海马体重放 | 离线重放不是回放，是重新评估因果权重 | `insights/05-agi-7x24.md` §4.2：「Agent 需要 sleep 阶段」 | 离线巩固周期（`consolidate.rs`，已实现） |
| **Davachi 2006** 时间邻近性 | 大脑默认「时间相邻 = 因果相关」（启发式，常错） | `insights/11-causal-state-store.md` §3 step three：「不是所有决策→结果都是因果关系」 | `confidence_source` 字段（temporal=0.4 vs user_feedback=0.95） |
| **Diekelmann 2010** 睡眠巩固 | 睡眠通过选择性重放 + 突触下调来转化记忆 | `insights/05-agi-7x24.md` §4.2 + §4.5 | 巩固周期四阶段：reactivation → generalization → downscaling → REM integration（`consolidate.rs`，已实现） |

### 认知心理学层

| 论文 | 核心发现 | 映射到 insights | 代码体现 |
|---|---|---|---|
| **Sloman 2005** 因果图理论 | 人类默认用 DAG 表征因果知识 | `insights/11-causal-state-store.md` §2：「Agent 因果记忆应该用图」 | `causal_edges` 的 DAG schema（from_id → to_id） |
| **Gerstenberg 2021** 反事实模拟 | 人类通过「如果没做 X，Y 还会发生吗？」来判断因果 | `insights/11-causal-state-store.md` §5：「需要支持多跳追溯」 | `trace_cause_chain()`（SQLite 递归 CTE） |
| **Schacter 2007** 重构式记忆 | 记忆不是回放，是每次检索时的重新组装 | `insights/13-reconstructive-memory.md` §1.3：「返回给 LLM 的不应该是 raw DB 记录」 | v1.1 重构式检索（路线图：因果子图 → LLM 生成叙事） |

### 计算 AI 层

| 论文 | 核心发现 | 映射到 insights | 代码体现 |
|---|---|---|---|
| **Wang 2024** Agent 记忆综述 | 没有现有系统存储因果边 | `insights/10-memory-frameworks.md` §6：「记忆公司不做因果层」 | `causal-memory` 项目的存在理由 |
| **Park 2023** 生成式智能体 | 反思机制是粗粒度文本摘要，不是决策级因果 | `insights/05-agi-7x24.md` §3.6：「slow-timescale behavioral loops」 | `meta_causal_edges` 的设计目标（结构化反思，不是文本摘要） |
| **Goyal & Bengio 2022** | System 2 需要显式因果表征 | `insights/09-stateless-function.md`：「LLM 是无状态函数，需要外部记忆层」 | 整个「外化因果图」的架构选择 |

### 形式化因果推断层

| 论文 | 核心发现 | 映射到 insights | 代码体现 |
|---|---|---|---|
| **Pearl 2009** 因果梯级 | 关联 → 干预 → 反事实，三级严格递增 | `insights/11-causal-state-store.md` §4：「v0.2 只做 Rung 1，Rung 2/3 在路线图」 | `search_causal`（Rung 1）+ `intervention_query`（Rung 2，已实现）；Rung 3 反事实明确出 scope |
| **Spirtes 2000** PC 算法 | 可以从观测数据中自动发现因果结构 | `insights/11-causal-state-store.md` §8.5：「meta_causal_edges 的挖掘需要类似 PC 的方法」 | v0.3 `meta_causal_edges` 激活（约束基模式挖掘） |

---

## 第二层：Insights → 架构决策（设计抽象）

### `insights/09-stateless-function.md` → 外化架构

**核心主张**：LLM 是无状态函数。每次推理都从头开始。

**架构决策**：
- 记忆不能是「让 LLM 自己记住」，必须是**外部注入层**
- 因果记忆不是 context 的一部分，而是** side table / MCP tool**

**代码体现**：
```rust
// main.rs: MCP stdio 传输——因果记忆是独立进程，不是 Agent 上下文的一部分
let transport = (tokio::io::stdin(), tokio::io::stdout());
server.serve(transport).await?;
```

### `insights/10-memory-frameworks.md` → 市场空白定位

**核心主张**：Mem0 存偏好，Zep 存状态，Letta 存自我管理记忆，但没有公司做因果层。

**架构决策**：
- 不做全栈记忆（不替代 Mem0/Zep）
- 只做因果切片，通过 MCP 协议接入任何 Agent

**代码体现**：
```rust
// server.rs: 8 个 MCP tools——每个对应决策回路的一个明确时刻，小面积，不竞争
#[tool_router]
impl CausalMemoryServer {
    #[tool(name = "record_decision")]
    #[tool(name = "search_causal")]
    #[tool(name = "trace_cause")]
    #[tool(name = "trace_cause_chain")]
    #[tool(name = "invalidate_decision")]
    #[tool(name = "search_patterns")]
    #[tool(name = "causal_directory")]
    #[tool(name = "intervention_query")]
}
```

### `insights/11-causal-state-store.md` → 因果状态库设计

这是**最直接的映射**。这篇 insight 几乎是 `causal-memory` 的架构规格说明书。

| Insight 章节 | 设计决策 | 代码位置 |
|---|---|---|
| §1.2 双系统记忆 | `causal_edges` + `meta_causal_edges` 双表 | `store.rs` CAUSAL_SCHEMA_SQL |
| §2 DAG 表征 | `from_id` → `to_id` 的有向边 | `store.rs` `causal_edges` schema |
| §3 置信度层级 | `confidence_source` 枚举 | `server.rs` `RecordDecisionParams.confidence_source` |
| §4 因果梯级 | v0.2 只做 Rung 1（检索），Rung 2/3 在路线图 | `docs/roadmap.md` |
| §5 因果链断裂假说 | 文本压缩会断裂中间环节，需要结构化存储 | `README.md` 中 k=5 召回率 45% 的 benchmark |
| §6 决策分叉树 | `meta_causal_edges` 的 `contradicts` relation | `store.rs` `meta_causal_edges` schema (v0.3) |
| §8.5 跨任务模式挖掘 | PC 算法启发式用于 `meta_causal_edges` | `docs/research/causal-inference/pc-algorithm.md` |

### `papers/02-compaction-degradation.md` → 压缩免疫设计

**核心发现**：因果信息在文本压缩下衰减最快（k=5 时 C 类只剩 17%）。因果表全程 100%。

**架构决策**：
- `causal_edges` **永远不被压缩**——它在 Agent 的 context window 之外
- Agent 的 compaction 只会压 context，不会碰 SQLite

**代码体现**：
```rust
// store.rs: causal_edges 不在 context 中，只在 DB 中
conn.execute(
    "INSERT INTO causal_edges (...) VALUES (...)",
    params![...]
)?;
```

### `insights/04-anti-entropy.md` → 反熵机制

**核心主张**：Agent 运行越久，信息熵越高。需要反熵机制来维持秩序。

**架构决策**：
- 实时写入（熵增）+ 离线巩固（熵减）
- `confidence` 衰减作为「选择性遗忘」机制

**代码体现**（`consolidate.rs` downscaling 阶段，已实现）：
```rust
// sleep consolidation 的 downscaling:
// 1. 时间衰减：所有边 confidence 按年龄指数衰减
// 2. 重放增强：被访问的边获得 access-based boost
// 3. 垃圾回收：回收低于阈值的边（user_feedback 边永不回收）
```

### `insights/05-agi-7x24.md` → 7×24 生存机制

**核心主张**：7×24 Agent 面临五个系统性失效模式，其中「累积压缩退化」和「身份漂移」是记忆层的直接责任。

**架构决策**：
- **身份持久性**：`causal_edges` 作为跨越 compaction 的「因果身份基底」
- **离线巩固**：16h 活跃 + 4h 巩固 + 4h 维护的周期

**代码体现**（已实现为 `causal-memory sleep` 四阶段离线周期）：
```
sleep cycle（每天一次，非幂等）:
  reactivation:    为边打分排出重放优先级（失败与用户反馈优先）
  generalization:  合并重复边 + 运行 pattern miner 提炼 meta 边
  downscaling:     置信度时间衰减 + access boost + 垃圾回收
  REM integration: 跨 task_tag 链接相似模式（跨域迁移）
```

### `insights/13-reconstructive-memory.md` → 重构式检索

**核心主张**：Agent 不应该把 raw DB 记录塞进 context。应该让 LLM 基于因果子图「重构」一段叙事。

**架构决策**：
- `search_causal` 返回结构化数据（v0.2）
- 未来版本增加一个轻量 LLM 层，将子图转化为自然语言叙事（v1.1）

**代码体现**（路线图）：
```rust
// v1.1 reconstructive retrieval:
// 1. search_causal 返回 CausalEntry Vec
// 2. 喂给轻量 LLM（或 Agent 自己的 LLM）
// 3. 生成："上次你用 mutex 导致死锁，后来用 channel 修复了"
```

---

## 第三层：架构决策 → 代码实现（工程落地）

### Schema 设计溯源

```sql
-- store.rs: CAUSAL_SCHEMA_SQL

-- causal_edges: 直接来自 insights/11 §1.2 + §2 (DAG)
CREATE TABLE causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,        -- ← DAG 的源节点
    to_id TEXT NOT NULL,          -- ← DAG 的目标节点
    relation TEXT NOT NULL        -- ← Sloman 的因果模型理论
        CHECK(relation IN ('caused','enabled','prevented','no_effect')),
    confidence REAL NOT NULL,     -- ← Davachi 的时间邻近性启发式（不是 1.0）
    discovered_by TEXT NOT NULL,  -- ← 区分 temporal(0.4) vs user_feedback(0.95)
    discovered_at INTEGER,        -- ← Diekelmann 的时间分割（theta 周期等价物）
    task_tag TEXT                 -- ← Wang 2024 的「任务分类」缺失
);

-- meta_causal_edges: 来自 insights/11 §6 (决策分叉树)
-- + insights/05 §4.2 (语义层)
-- + Spirtes 2000 (PC 算法自动挖掘)
CREATE TABLE meta_causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL        -- ← 'similar_to','repeated','contradicts','refines'
        CHECK(relation IN ('similar_to','repeated','contradicts','refines')),
    pattern TEXT,                 -- ← 抽象出来的跨任务模式
    confidence REAL
);
```

### API 设计溯源

| MCP Tool | 理论来源 | Insight 来源 | 解决的问题 |
|---|---|---|---|
| `record_decision` | Kumaran 2016（海马体快速编码） | `insights/11` §1.2 | Agent 做完决策后必须立即记录，否则会丢失 |
| `search_causal` | Sloman 2005（因果图检索） | `insights/11` §2 | Agent 做新决策前需要检索过去的因果经验 |
| `trace_cause` | Gerstenberg 2021（单跳反事实） | `insights/11` §5 | 失败后找出直接原因 |
| `trace_cause_chain` | Gerstenberg 2021（多跳反事实）+ Pearl Rung 1 | `insights/11` §5 + §4 | 失败后找出根因链 |
| `invalidate_decision` | `insights/08`（认识论谦逊） | `insights/11` §3 | 记录的经验被证明是错的时需要纠正路径 |
| `search_patterns` | Kumaran 2016（新皮层慢速语义系统） | `insights/11` §6, §8.5 | 检索离线挖掘出的跨任务模式，而非原始片段 |
| `causal_directory` | Schacter 2007（重构需要蓝图索引） | `insights/13` §1.3 | 零检索成本回答「我拥有什么经验」 |
| `intervention_query` | Pearl 2009 Rung 2（干预） | `insights/11` §4 | 行动前预测相似历史动作造成的结果 |

Rung 3 反事实查询（「如果当初选了 B 而不是 A？」）明确不做——见 `docs/roadmap.md` 的 out-of-scope 说明。

### 工程权衡溯源

| 权衡 | 选择 | 来源 |
|---|---|---|
| SQLite vs. 向量数据库 | SQLite | `insights/10`：不需要替换现有记忆系统，只需要一个因果 side table。SQLite 零依赖、单文件、可移植。 |
| MCP stdio vs. HTTP | stdio | `insights/09`：记忆层是注入层，不是服务层。stdio 是最简单的进程间通信。 |
| 8 个 tools vs. 更多 | 8 个 | `insights/14` §2.2：「complete-looking is the enemy of depth」——每个工具必须覆盖决策回路中一个不重叠的时刻，再多就会互相踩踏。 |
| 手动 record vs. 自动提取 | 两者都有 | `insights/02`：自动提取（extractor.rs）是 v0.2 功能，但 manual `record_decision` 保留作为 fallback。 |

---

## 哲学基础

### Parfit 的心理连续性 → 身份持久性

`papers/01-from-sessions-to-lifetimes.md` §3.5 引用了 Derek Parfit 的《Reasons and Persons》(1984)：

> "After hundreds of compactions, is the agent still 'the same agent'?"

**工程回答**：如果 Agent 在每次 compaction 后都丢失了「为什么做了这个决策」的因果链，那么它**不是**同一个 Agent——它只是一个继承了名字的新实例。

`causal-memory` 的 `causal_edges` 表是这个问题的工程解：**一个跨越 compaction 的因果身份基底**。只要因果链还在，Agent 的「决策历史自我」就保持连续。

### Searle 的中文屋 → 外化因果结构的认识论辩护

`insights/07-philosophy-deep-dive.md` 可能讨论了 Searle 的中文屋论证（如果仓库里有这篇）。核心观点：符号操作不等于理解。

**工程回答**：我们不指望 LLM「理解」因果性。我们给它一个显式的因果图去查询。LLM 不需要「知道」mutex 导致死锁——它只需要知道去 `trace_cause("deadlock")` 获取答案。理解发生在**系统层面**（LLM + causal-memory），而不是在 LLM 内部。

### 怀疑主义 → 置信度系统

`insights/08-self-rebuttal.md` 的主题是自我反驳和认识论谦逊。

**工程回答**：Agent 不应该对自己记录的因果链接有 100% 的信心。`confidence` 字段（最高 0.95 的 user_feedback） encode 了这种谦逊——即使是「确定」的知识，也留有余地。

---

## 路线图：Insights 如何指导各版本（含落地状态）

| 版本 | 功能 | Insight 来源 | 论文支撑 | 状态 |
|---|---|---|---|---|
| **v0.3** | `meta_causal_edges` 激活 | `insights/11` §6, §8.5 | Spirtes 2000 PC 算法 | ✅ 已实现（patterns.rs miner） |
| **v0.7** | 语义/向量搜索 | `insights/13` §1.3 | Reimers & Gurevych (Sentence-BERT) | ✅ 已实现（embed.rs，OpenAI 兼容端点） |
| **v0.4** | 离线巩固周期 | `insights/05` §4.2 | Diekelmann 2010, Schapiro 2017 | ✅ 已实现（consolidate.rs） |
| **v0.4** | 置信度时间衰减 | `insights/04` §2 | Tononi SHY (突触稳态) | ✅ 已实现（downscaling 阶段） |
| **v0.7** | 干预查询 (Rung 2) | `insights/11` §4 | Pearl 2009 do-calculus | ✅ 已实现（intervention_query） |
| ~~v0.5~~ | 反事实查询 (Rung 3) | `insights/11` §4 | Pearl 2009 counterfactuals | ❌ 明确出 scope（`insights/11`：对 agent 不切实际） |
| **v1.x** | 跨 Agent 共享 | `insights/05` §4.5 + `insights/10` | Hutchins 1995 (分布式认知) | 路线图（v1.1 research direction） |
| **v1.1** | 重构式检索 | `insights/13` §1.3 | Schacter 2007 | 路线图 |

---

## 如何阅读这张谱系图

如果你是：

- **读论文的人** → 从「第一层」开始，理解每个设计决策的理论根基
- **读拆解的人** → 从「第二层」开始，看 insights 如何转化为架构
- **读代码的人** → 从「第三层」开始，反向追溯「这个字段为什么这么设计」

---

## 参考文献

- `agent-teardown` insights: https://github.com/JingxuanC/agent-teardown/tree/main/insights
- `agent-teardown` papers: https://github.com/JingxuanC/agent-teardown/tree/main/papers
- `causal-memory` research docs: [`../research/`](../research/)

---

*本文档是活的。随着 `agent-teardown` 新增 insights 和 `causal-memory` 新增功能，会持续更新映射关系。*
