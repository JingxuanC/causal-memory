# Optimization Plan · 2026-08-04

> 基于 2026-07-27 至 2026-08-04 调研的 20+ 篇论文/框架，对 causal-memory
> 架构的系统性 gap 分析和优化方案。每条优化都标注了调研来源、当前状态、
> 预期回报和优先级。
>
> 调研覆盖：HeLa-Mem (ACL'26)、OpenViking (VLDB'26)、Anthropic Dreams API、
> Nemori (FEP prediction gap)、RecMem (ACL'26 Findings, recurrence-based)、
> Rate-Distortion View、CMI-Mem、Oracle Agent Memory (93.8% LongMemEval)、
> LazyMem (213 token / 0.85)、AttriMem (token-level RL)、LoCoMo audit
> (6.4% answer key errors)、MemRL/MemQ/DeltaMem、LightMem/SCM、Metis
> (Memory Foundation Model)、MemSecBench (memory security)。

## 当前架构概览

```
WRITE (3 channels)              STORE (SQLite v7)           RETRIEVE
┌─────────────────────┐         ┌──────────────────┐       ┌────────────────────┐
│ raw ingest          │──chks──▶│ chunks (raw)     │       │ search_causal_bm25 │
│ distill (LLM/session)│──facts▶│ agent_facts      │──RRF─▶│ search_causal_sem  │
│ MCP direct writes   │──edges▶│ causal_edges     │       │ search_memory (RRF)│
└─────────────────────┘         └──────────────────┘       │ trace_cause(_chain)│
                                         │                  │ intervention_query │
                                         ▼                  │ counterfactual     │
                                ┌──────────────────┐       │ reconstruct_lesson │
                                │ hippocampus graph│       │ search_facts       │
                                │ (CSR, typed edge)│       └────────────────────┘
                                │ spreading activ. │
                                │ SWR consolidate  │
                                │ Q-value / Hebbian│
                                └──────────────────┘
```

13 个 MCP tools · 206 tests · 7 种 edge types · LoCoMo 67.4% · LongMemEval 71.2%

---

## P0: Answer Prompt 优化（10 分钟，+3-5pp）

### 来源
LongMemEval r3/r4 实验（2026-08-04）

### 问题
Answer prompt Step 6 "If no memory contains the requested fact, answer: I
don't have enough information" 导致 preference 类 73% 过度拒绝。

### 方案
PREFERENCE_RULE 已改为鼓励推断（"从用户的历史活动推断偏好，不要拒绝"）。
已验证 preference 从 13.3% → 56.7%（+43.4pp）。

### 待做
- [ ] 用新 prompt 跑全量 500 题 LongMemEval
- [ ] 评估对其他类别（temporal/multi-session）的副作用
- [ ] 如果其他类别不降，合入 main

---

## P1: Recurrence-Triggered Distill（1-2 天，token 省 50-87%）

### 来源
RecMem (arXiv:2605.16045, ACL 2026 Findings) — recurrence-based
consolidation 省 87% token

### 问题
当前 distill 是 eager 的——每个 session 都调 LLM 做 distill。RecMem 证明
只在语义重复出现时才 consolidate，效果更好且 token 省 87%。

### 方案
```
当前: session 结束 → distill(session_logs) → 因果/fact 提取
改为: session 结束 → 存入 session_logs (with embedding)
      → recurrence check: 话题和之前 session 是否语义重复?
         YES → 触发 distill (合并相关 sessions)
         NO  → 跳过，留在 session_logs
      定时 batch: 每天或每 N 个未 distill sessions 做一次
```

### 改动
- `store/write.rs`: `log_session_turn` 加 embedding 字段
- `distill.rs`: 新增 `distill_recurrence()` 方法
- `cli`: `causal-memory distill --mode recurrence`

### 预期
- distill LLM 调用从 N sessions → ~0.13N sessions（RecMem 报告）
- 因果边覆盖率不受影响（recurrence 检测保证重要话题不被跳过）

---

## P2: Loop Detection in Agent（半天，ablation 解决率 6→8/8）

### 来源
Agent ablation transcript（2026-08-02）—— Task 5/8 连续 10+ 次相同命令

### 问题
Agent 陷入循环重复同一命令，浪费所有步数。Condition C 的
intervention_query 返回 UNKNOWN 时，agent 完全从头开始，不复用记忆。

### 方案
```rust
// benches/agent/bench_agent.rs
struct LoopDetector {
    last_commands: Vec<String>,
    repeat_count: usize,
}

// 连续 3 次相同/相似命令 → 强制 intervention_query
// intervention 返回 UNKNOWN → fallback 到 search_past（文本检索）
```

### 改动
- `bench_agent.rs`: 加 LoopDetector + UNKNOWN fallback
- 检测到 loop 时注入 "你正在重复同样的命令，试试不同的方法或查记忆" 提示

### 预期
- Agent ablation 解决率从 6/8 → 8/8
- repeat-mistake rate 保持 0%

---

## P3: Superseded 标记 + Reversible Consolidation（1 天）

### 来源
Oracle Agent Memory (arXiv:2607.13157) — reversible consolidation
Rate-Distortion View (arXiv:2607.08032) — "可逆性 > 评分技巧"

### 问题
当前 `swr_consolidate_immutable` 产出新图但不修改原图 ✅。但
`invalidate_superseded` 直接软删除旧 edges，不支持"后来证明旧记忆是对的"
的回滚。

### 方案
```
1. Consolidate: 新图标记原图 edges 为 "superseded"（不删除）
2. 后续交互验证：
   - 新证据推翻旧记忆 → 确认 supersede (delete or archive)
   - 新证据证明旧记忆 → 回滚 (restore superseded edges)
```

### 改动
- `store/retrieve.rs`: 检索时可选包含 superseded edges
- `store/write.rs`: `restore_superseded()` 新方法
- `hippocampus/mod.rs`: SWR 标记而非删除

---

## P4: 前向模拟 Benchmark（1-2 天，论文级贡献）

### 来源
世界模型分析 (2026-08-01) — "caused 边 = 转移函数样本，向前走 = 模拟"
Physical Intelligence (arXiv:2607.06401) — 世界模型定义
Graph World Models (arXiv:2604.27895) — Graph as Reasoner

### 问题
`intervention_query` 已实现但零 benchmark、零竞品。这是 causal-memory
独有的能力（世界模型蓝海），但没有数据证明它的价值。

### 方案
```
协议：
1. 用 trap-world 因果图（已有 agent ablation 数据）
2. 对每个任务，行动前调 intervention_query 预测后果
3. 对比：预测后果 vs 实际后果 → 前向模拟准确率
4. 对比：有前向模拟的 agent vs 无前向模拟的 agent → 决策质量差异

指标：
- 前向模拟准确率（预测 vs 实际）
- 陷阱避免率（intervention 预测到危险 → agent 避免了）
- 步数节省（避免失败步骤的步数 / 总步数）
```

### 改动
- `bench_agent.rs`: 新增 Condition D (intervention + benchmark metrics)
- 记录每次 intervention_query 的预测内容和实际结果

### 预期
- 首个证明前向模拟价值的 agent 记忆 benchmark
- 论文核心贡献（无竞品）

---

## P5: 混合写入门控（2-3 天）

### 来源
Nemori (arXiv:2508.03341) — Free Energy Principle prediction gap
causal-memory novelty_entropy (Shannon entropy)

### 问题
当前 `novelty_entropy` 是词频层面的惊讶。Nemori 的 prediction gap 是语义
层面的惊讶（LLM 预测 vs 实际消息的差距），更强但更贵。

### 方案
```
当前: message → entropy check → write/discard
混合: message → entropy check (cheap, O(n))
      → if borderline → prediction gap (expensive, 1 LLM call)
      → write/discard
```

### 改动
- `hippocampus/mod.rs`: `detect_novelty` 加 prediction gap fallback
- 配置：`--novelty-mode entropy|prediction_gap|hybrid`

---

## P6: Token 效率 Benchmark + Layered Loading（1 天）

### 来源
OpenViking (VLDB'26) — token 节省 34-91%
LazyMem (arXiv:2607.22690) — 213 token / 0.85 准确率
Rate-Distortion View — "可逆性 > 评分技巧"

### 问题
causal-memory 从未测量 token 消耗。不知道我们在 token 效率上处于什么位置。

### 方案
1. 在 LongMemEval/LoCoMo 中记录每题的 token 消耗（context + answer）
2. 对比：raw检索 top-10 vs RRF 重排 top-5 vs query-time construction
3. 实现 layered loading：L0 summary / L1 overview / L2 full text

---

## P7: Memory 安全（prevented 负扩散天然防御）（半天验证）

### 来源
Persistence-Based Memory Extraction Attack (arXiv:2607.23444)
MemSecBench (arXiv:2607.27080)

### 问题
注入到 agent 记忆中的恶意内容可以跨 session 持续存在。

### 假设
causal-memory 的 prevented 负扩散可能天然抵御——恶意内容被标记为
prevented → 负权重 → 传播时被抑制。

### 方案
- 用 MemSecBench 协议测试 causal-memory 的抗注入能力
- 测量 prevented 边对恶意内容传播的抑制效果

---

## 不做的（已评估，投入产出比低）

| 方案 | 来源 | 不做的原因 |
|------|------|-----------|
| 参数化记忆（Metis） | arXiv:2607.26760 | 需要训练模型，和外部存储路线不同 |
| Token 级 RL reward | AttriMem arXiv:2607.21106 | 需要训练小模型，工程量大 |
| 多租户 scope | Oracle | 企业级需求，当前阶段过早 |
| Memory 可视化 UI | MemLens | 需要前端工作，当前不是瓶颈 |

---

## 执行优先级

| Phase | 任务 | 投入 | 预期回报 | 依赖 |
|-------|------|------|---------|------|
| **Phase 0** | Answer prompt 全量验证 | 10 min | LongMemEval +3-5pp | 无 |
| **Phase 1** | Recurrence-triggered distill | 1-2 天 | token 省 50-87% | P0 |
| **Phase 2** | Loop detection + UNKNOWN fallback | 半天 | agent 6/8→8/8 | 无 |
| **Phase 3** | Superseded + reversible | 1 天 | 可逆巩固 | 无 |
| **Phase 4** | 前向模拟 benchmark | 1-2 天 | 论文级贡献 | 无 |
| **Phase 5** | 混合写入门控 | 2-3 天 | 语义级惊讶检测 | P0 |
| **Phase 6** | Token 效率 benchmark | 1 天 | 量化效率 | P0 |
| **Phase 7** | Memory 安全测试 | 半天 | 验证 prevented 防御 | 无 |

总计：~8-10 天，可以并行执行多个 Phase。

---

## 和 insight 17 的对齐

[insight/17](../../agent-teardown/insights/17-complete-memory-system.md)
提出"One Graph, One Engine, One Loop"——所有记忆类型都是 typed edge。

本方案的优化全部在这个框架内：
- P0/P1 优化写入和检索（Loop 的"写"和"读"）
- P3 优化巩固（Loop 的"演化"）
- P4 证明前向模拟价值（Engine 的"预测"能力）
- P5 优化门控（Engine 的"惊讶检测"）

**没有任何优化需要脱离统一图架构。**

---

## 版本历史

- 2026-08-04: 初版。基于 7 天调研（07-27 ~ 08-04）的 20+ 篇论文/框架。
