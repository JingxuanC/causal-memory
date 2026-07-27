# 研究背景（中文版）

> 塑造 `causal-memory` 的论文与理论基础。
> 这不是参考文献列表——而是一张地图，标注了哪些想法最终变成了哪些设计决策。

完整系统化的研究文档（BibTeX、详细摘要、方法论评析、每篇论文的设计追溯）请见 [`docs/research/`](../research/) —— 按主题组织：

- [`neuroscience/`](../research/neuroscience/) — 大脑如何处理记忆与因果
- [`cognitive-psychology/`](../research/cognitive-psychology/) — 人类如何表征和推理因果知识
- [`computational-ai/`](../research/computational-ai/) — AI 领域做了什么（以及没做什么）
- [`causal-inference/`](../research/causal-inference/) — 形式化基础（Pearl, Spirtes）

**中文版论文解析**见 [`zh/`](./) 目录，内容一一对应。

---

## 1. 核心前提：LLM 是无状态函数

**参考**：[insights/09-stateless-function](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)

每一次 LLM 推理调用都是从头开始。记忆不是附加功能——它是**强制的注入层**。因果记忆是针对「决策→结果」链接的一种特定注入策略。

---

## 2. 为什么是因果？压缩退化的实证证据

**论文**：[papers/02-compaction-degradation](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)

真实 LLM 基准测试（使用 grok-build 的生产级压缩提示词）：

| 压缩次数 (k) | 文本召回率 | 因果表召回率 |
|---|---|---|
| 1 | 100% | 100% |
| 2 | 85% | 100% |
| 3 | 55% | 100% |
| 5 | **45%** | **100%** |

核心发现：**因果信息在文本压缩下的衰减速度比预期更快**。因果表之所以能存活，是因为它位于压缩管道之外。

---

## 3. 神经科学

### Kumaran, Hassabis & McClelland (2016) — 互补学习系统理论

**核心观点**：大脑有两个记忆系统——海马体（快速、情景式）和新皮层（慢速、语义式）。

**设计关联**：我们的双表架构（`causal_edges` + `meta_causal_edges`）直接复制了这个结构。我们拒绝压缩 `causal_edges`，因为海马体在初始编码阶段不会压缩情景痕迹。

**深度阅读**：[zh/neuroscience/cls-theory.md](zh/neuroscience/cls-theory.md)

### Schapiro et al. (2017) — 海马体重放

**核心观点**：海马体通过休息期间的**压缩重放**来解决时间模糊性——不是忠实的回放，而是结构化的重新评估。

**设计关联**：离线巩固周期（"睡眠"）直接受此启发。v0.9 起重放真正落地为「重估」：重放优先级高的边在衰减阶段获得保护（半速衰减、宽松 GC 阈值），且重放写回 `last_accessed_at`，形成「重放→巩固→更易存活」的跨周期反馈回路。

**深度阅读**：[zh/neuroscience/hippocampus-temporal.md](zh/neuroscience/hippocampus-temporal.md)

### Davachi (2006) — 时间邻近性

**核心观点**：大脑默认假设「A 发生在 B 之前，因此 A 导致了 B」——这是一种启发式，不是事实。

**设计关联**：我们的置信度层级明确编码了这一点：`temporal` = 0.4（弱），`rule` = 0.7（强），`user_feedback` = 0.95（金标准）。这防止了系统对虚假时间相关性的过度加权。

**深度阅读**：[zh/neuroscience/temporal-contiguity.md](zh/neuroscience/temporal-contiguity.md)

### Diekelmann & Born (2010) — 睡眠巩固

**核心观点**：睡眠通过选择性重激活、要点提取和突触下调来主动转化记忆。

**设计关联**：v0.4 巩固周期包括：重激活（优先级队列）、泛化（meta_causal_edges）、下调（置信度衰减 + 垃圾回收）。

**深度阅读**：[zh/neuroscience/sleep-consolidation.md](zh/neuroscience/sleep-consolidation.md)

---

## 4. 认知心理学

### Sloman (2005) — 因果图理论

**核心观点**：人类使用**有向无环图（DAG）**作为因果知识的默认表征格式。

**设计关联**：`causal_edges` 是一个扁平化的 DAG 边列表。`relation` 类型（`caused`、`enabled`、`prevented`、`no_effect`）编码了因果模型必须满足的结构约束。

**深度阅读**：[zh/cognitive-psychology/causal-graph-theory.md](zh/cognitive-psychology/causal-graph-theory.md)

### Gerstenberg et al. (2021) — 反事实模拟

**核心观点**：人类通过运行反事实世界的**心理模拟**来确定因果责任——「如果我当时没做 X，Y 还会发生吗？」

**设计关联**：`trace_cause_chain` 是回溯式部分实现。v0.9 新增 `counterfactual_query`——对比式经验反事实（对比已记录的替代决策在相似情境下的结果），输出中明确标注它不是 Pearl Rung-3 的 SCM 反事实。

**深度阅读**：[zh/cognitive-psychology/counterfactual-simulation.md](zh/cognitive-psychology/counterfactual-simulation.md)

### Schacter & Addis (2007) — 重构式记忆

**核心观点**：记忆不是回放——而是**重构**。海马体存储的是「构造蓝图」，不是原始录像。每次提取都会重新组装。

**设计关联**：这是**重构式检索**的理论基础，v0.9 已实现为 `reconstruct_lesson`：系统检索因果子图（Markov blanket：父+子+共父），由 LLM 生成连贯的「经验教训」叙事；`calibrate≥2` 时生成多段独立叙事并测量一致性，低一致性标记底层记忆可能不可靠。

**深度阅读**：[zh/cognitive-psychology/reconstructive-memory.md](zh/cognitive-psychology/reconstructive-memory.md)

---

## 5. 计算 AI

### Wang et al. (2024) — Agent 记忆综述

**核心观点**：当前 LLM Agent 记忆系统几乎全是 RAG 范式。**没有一个系统将因果关系作为主要数据结构存储。**

**设计关联**：这是我们因果记忆作为**真实市场空白**的核心证据。

**深度阅读**：[zh/computational-ai/agent-memory-survey.md](zh/computational-ai/agent-memory-survey.md)

### Park et al. (2023) — 生成式智能体

**核心观点**：持久记忆 + 周期性反思能产生涌现的社会行为。但反思是粗粒度的（文本摘要），不是决策级别的因果链接。

**设计关联**：生成式智能体是最接近的先例。我们将其扩展为结构化（`meta_causal_edges`）和因果化（`causal_edges`）的反思。

**深度阅读**：[zh/computational-ai/generative-agents.md](zh/computational-ai/generative-agents.md)

### Goyal & Bengio (2022) — System 2 归纳偏置

**核心观点**：System 2 认知（规划、因果推理）需要**显式的对象-关系-规则表征**，而不是端到端的隐式编码。

**设计关联**：`causal-memory` 是这一原则的实现。我们不指望 LLM「学会」因果性，而是将因果结构外化为显式图。

**深度阅读**：[zh/computational-ai/system2-explicit-representation.md](zh/computational-ai/system2-explicit-representation.md)

---

## 6. 因果推断（形式化基础）

### Pearl (2009) — 因果性

**核心观点**：**因果梯级**——三个层级：关联（观察）、干预（行动）、反事实（想象）。每一级严格强于前一级。

**设计关联**：第一级 `search_causal`、第二级 `intervention_query`（含 task_tag 分层调整与 Simpson 悖论警告）均已实现；第三级以对比式工程版（`counterfactual_query`）落地，SCM 意义上的反事实明确不做。

**深度阅读**：[zh/causal-inference/pearl-causality.md](zh/causal-inference/pearl-causality.md)

### Spirtes, Glymour & Scheines (2000) — PC 算法

**核心观点**：通过条件独立性检验，从观测数据中自动发现因果结构。

**设计关联**：`meta_causal_edges` 的模式挖掘受此启发。v0.9（schema v5）起升级为真实的分层复现检验：模式须至少在 2 个 task_tag 分层中独立成立才能晋升，单分层模式标记 `confounded`，分层间极性相反触发 Simpson 悖论警告——仍是 PC 的工程替代而非完整 PC，但有了真正的条件化检验。

**深度阅读**：[zh/causal-inference/pc-algorithm.md](zh/causal-inference/pc-algorithm.md)

---

## BibTeX

所有论文：[`docs/research/references.bib`](../research/references.bib)

```bash
# 导入 Zotero
zotero docs/research/references.bib
```

---

## 阅读顺序

1. 从 [insights/09](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md) 开始 —— "LLM 是无状态的"这一前提
2. 读 [papers/02](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md) —— 实证证据
3. 读 [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md) —— 本仓库实现的设计
4. 然后按主题探索 [`zh/`](./) 目录 —— 每篇论文都连接到具体的设计决策

---

*本文档是活的。随着我们实现 v0.3+ 功能，会持续更新研究地图。*
