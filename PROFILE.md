# causal-memory · 作品简介

## 一句话

**让 AI Agent 拥有因果记忆——不重复犯错、自主进化、预测未来。**

## 是什么

causal-memory 是一个用 Rust 实现的因果记忆引擎（MCP Server），模拟人脑海马体的记忆机制，为 AI Agent 提供超越"记事本"的深度记忆能力。

它不只记住"发生了什么"，更记住**"为什么"**——决策和结果之间的因果关系。这让 Agent 能从失败中学习（不踏入同一个陷阱两次）、在行动前预测后果（世界模型能力）、并在长期运行中持续进化（睡眠巩固 + 在线学习）。

## 为什么做

我们拆解了 42 篇 Agent 框架源码（kimi-code 25 篇 + Grok Build 10 篇 + Codex/Claude Code/ADK/Pi），写了 17 篇跨学科研究笔记（覆盖信息论、神经科学、哲学、认知科学），追踪了 20+ 篇前沿论文。

核心发现：**现有 AI 记忆系统都是"记事本"——记住事实，但不理解因果。** 调研了 8 家记忆公司（Mem0 / Zep / Letta / OpenViking / Cognee / M3 / MemOS / OpenMemory），没有一家做了因果记忆。这是整个赛道的最大空白。

## 怎么做的

从理论推导到工程实现，建立了四层统一架构：

```
写入层 → 存储层 → 引擎层 → 检索层
```

- **7 种 typed edge**（caused / fact / meta / enabled / co-occurrence / prevented / no_effect）统一在一张图上
- **海马体 CSR 稀疏矩阵**做 spreading activation（含 GABA 抑制性负扩散）
- **SWR 睡眠巩固**四阶段（LTP / LTD / GC / Q-value）
- **14 个 MCP 工具**——任何 Agent 框架可即插即用（`remember` 零门槛自动提取）

## 独有能力（无竞品）

1. **🚫 Prevented 负扩散** — GABA 抑制性类比，错误记忆被自动抑制，天然防御注入攻击
2. **🔮 前向模拟** — intervention_query，行动前沿因果图预测后果（世界模型）
3. **🌙 SWR 睡眠巩固** — 产出新图不改原图，可审计可回滚，与 Anthropic Dreams API 对齐
4. **⚡ CSR + 在线进化** — 248K 节点实时检索，Q-value Bellman + Hebbian LTP 持续学习

## 数据

> benchmark 详情与口径见 `docs/benchmarks/`。

| Benchmark | 成绩 | 竞品 |
|-----------|------|------|
| LongMemEval (500题) | **75.2%** | Mem0 ~74.4% |
| LoCoMo (记忆基准, strict judge) | **79.1%** | 行业最强 91.6% |
| CausalEval 因果能力 | **81%** (v12) | mem0 65% |
| Agent 重复犯错率 | **33%** | 无记忆 67% |
| 压缩生存率提升 | **+20.8pp** | 独家指标 |
| 证据命中率 | **87.6%** | — |

## 数字

- **322** 个测试全过
- **7** 种边类型
- **14** 个 MCP 工具
- **17** 篇研究笔记
- **42** 篇源码拆解
- **20+** 篇论文追踪
- **Rust** 实现，Apache-2.0 开源

## GitHub

**github.com/JingxuanC/causal-memory**
