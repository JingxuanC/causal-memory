# 研究文档（中文版）

> 系统化记录塑造 `causal-memory` 的神经科学、认知心理学、计算 AI 和因果推断论文。

这不是参考文献列表。它是一张**设计追溯地图**：每篇论文都被映射到代码库中的具体设计决策。

---

## 目录结构

```
docs/research/
├── README.md                 ← 英文入口
├── references.bib            ← BibTeX（13 篇论文）
├── neuroscience/             ← 大脑如何处理记忆与因果
│   ├── README.md
│   ├── cls-theory.md         ← Kumaran 2016：双系统记忆
│   ├── hippocampus-temporal.md ← Schapiro 2017：重放与模式完成
│   ├── temporal-contiguity.md  ← Davachi 2006：时间作为因果启发式
│   └── sleep-consolidation.md  ← Diekelmann & Born 2010：离线巩固
├── cognitive-psychology/     ← 人类如何表征因果知识
│   ├── README.md
│   ├── causal-graph-theory.md  ← Sloman 2005：心理因果模型
│   ├── counterfactual-simulation.md ← Gerstenberg 2021：模拟判断
│   └── reconstructive-memory.md ← Schacter & Addis 2007：记忆重构
├── computational-ai/         ← AI 领域做了什么（以及没做什么）
│   ├── README.md
│   ├── agent-memory-survey.md  ← Wang 2024：因果记忆空白
│   ├── generative-agents.md    ← Park 2023：记忆流 + 反思
│   └── system2-explicit-representation.md ← Goyal & Bengio 2022：显式结构
└── causal-inference/         ← 形式化基础
    ├── README.md
    ├── pearl-causality.md      ← Pearl 2009：因果梯级
    └── pc-algorithm.md         ← Spirtes 2000：自动化因果发现

zh/                          ← 中文版（本文档所在目录）
```

---

## 如何阅读

### 如果你想理解生物学基础

从 [`neuroscience/`](neuroscience/) 开始：
1. `cls-theory.md` —— 为什么有两张记忆表？
2. `sleep-consolidation.md` —— 为什么需要离线巩固？
3. `hippocampus-temporal.md` —— 多跳追溯在生物学上如何工作？

### 如果你想理解认知基础

从 [`cognitive-psychology/`](cognitive-psychology/) 开始：
1. `causal-graph-theory.md` —— 为什么是图结构？
2. `counterfactual-simulation.md` —— 为什么是多跳追溯？
3. `reconstructive-memory.md` —— 为什么是重构式检索（路线图）？

### 如果你想理解市场空白

从 [`computational-ai/`](computational-ai/) 开始：
1. `agent-memory-survey.md` —— 现有系统和缺失部分
2. `generative-agents.md` —— 最接近的先例
3. `system2-explicit-representation.md` —— 为什么外化因果结构？

### 如果你想要形式化数学

从 [`causal-inference/`](causal-inference/) 开始：
1. `pearl-causality.md` —— 因果梯级（我们在哪，要去哪）
2. `pc-algorithm.md` —— 如何自动发现模式

---

## BibTeX

所有论文都在 [`../references.bib`](../references.bib)。可导入 Zotero、Mendeley、JabRef。

```bash
# 导入 Zotero
zotero docs/research/references.bib
```

---

## 活文档

这个目录是**活的**。随着我们实现 v0.3+ 功能，会持续添加论文并更新设计追溯：

| 功能 | 待添加论文 |
|---|---|
| 语义/向量搜索 | Mikolov 等 (word2vec)、Reimers & Gurevych (Sentence-BERT) |
| 离线巩固 | Rasch & Born (2013) 睡眠期间重激活；Nadel 等 (2012) 系统巩固 |
| 重构式检索 | Bartlett (1932) 记忆；Conway & Pleydell-Pearce (2000) 自我记忆系统 |
| 跨智能体共享 | Hutchins (1995) 分布式认知；Ostrom (1990) 公共事务治理 |

---

*最后更新：2026-07-27*
