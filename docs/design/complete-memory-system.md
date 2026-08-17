# 完整记忆系统架构 —— One Graph, One Engine, One Loop

> **版本**：v1.0 架构提案 · 2026-07-31
>
> **定位**：本文是 causal-memory 的顶层架构文档，统一以下设计的全部研究成果：
> [design.md](design.md)（为什么）· [hippocampus-design.md](hippocampus-design.md)（海马体引擎）·
> [unified-memory-design.md](unified-memory-design.md)（三层存储）·
> agent-teardown 深度分析（HeLa-Mem / Dreams API / OpenViking / MemRL）。
>
> **一句话**：所有记忆类型统一为一张 typed-edge 图，用一个类型加权激活扩散引擎检索，
> 用一个不可变巩固循环演化。不是三个存储拼在一起，是一个系统。

---

## 0. 设计哲学：五条不可协商的原则

从全部研究中蒸馏出的硬约束，任何实现细节都不能违反：

| # | 原则 | 来源 | 含义 |
|---|---|---|---|
| P1 | **生物学完整性** | HeLa-Mem 分析 §6 | 兴奋侧（Hebbian LTP）和抑制侧（prevented GABA）必须共存；只做一半是残缺的系统 |
| P2 | **情景与语义共存，不是替代** | Dreams 分析 §8 | 巩固产出语义知识，但原始情景边永远保留可查 |
| P3 | **巩固不可变** | Dreams 分析 §3 | 巩固 = 计算 delta + 应用到 clone，原图永不被直接修改；出错可丢弃 |
| P4 | **检索可观察** | OpenViking 分析 §2.3 | 每次检索保留激活轨迹，能回答"为什么返回这条" |
| P5 | **压缩免疫是一等公民** | design.md | 关键结构活在 agent 上下文窗口之外，benchmark 必须持续证明 |

---

## 1. 核心命题：一切记忆都是 typed edge

### 1.1 边类型分类学（edge taxonomy）

```
                    ┌─────────────────────────────────┐
                    │        THE MEMORY GRAPH          │
                    │                                  │
                    │  节点 = 记忆痕迹 (engram)         │
                    │  边   = 类型化关系 (typed edge)   │
                    └─────────────────────────────────┘

  边类型          语义                例子                        来源
  ─────────────────────────────────────────────────────────────────────
  caused          A 导致 B            mutex →caused→ deadlock      agent 记录 / distill
  enabled         A 使 B 成为可能      index →enabled→ fast query   agent 记录 / distill
  prevented       A 阻止 B            cache →prevented→ fresh data  agent 记录 / distill
  no_effect       A 对 B 无影响        rename →no_effect→ perf      agent 记录
  fact            主语-谓语-宾语       user →prefers→ TypeScript    distill / record_fact
  co_occurrence   A 和 B 反复共现      redis ⇄ cache_config         Hebbian 运行时更新
  meta            跨情景的统计模式     "分布式系统避免 mutex"        巩固时挖掘
  ─────────────────────────────────────────────────────────────────────
  时序 = 元数据，不是边类型：所有边携带 valid_from / valid_to / event_time
```

**关键设计决策**：

1. **事实不是一张新表，是一类新边。** `agent_facts`（unified-memory-design §2.1）
   物理上仍是独立的表（查询模式不同：点查 vs 遍历），但**逻辑上**进入同一张图：
   fact 边参与激活扩散、参与巩固、参与 GC。这是和 unified-memory-design 的最大区别——
   那里事实层是外挂，这里事实层是图的一等公民。
2. **时序不是层，是所有边的有效期元数据。** schema v5 的
   `valid_from / valid_to / event_time` 已经实现了这一点，不需要新机制。
3. **Hebbian 共现边是运行时动态边。** 权重不在写入时设定，按
   `w(t+1) = (1-λ)·w(t) + η·𝕀(共激活)` 演化（HeLa-Mem 公式 1，λ=0.995，η=0.02）。
   因果边权重保持静态语义（类型决定扩散系数），共现边权重持续演化——
   **静态因果语义 + 动态统计强度，两者叠加**。

### 1.2 双系统映射（不变）

```
海马体（情景记忆）  = caused/enabled/prevented/fact/co_occurrence 边
                     具体、一次性、快速写入
新皮层（语义记忆）  = meta 边（巩固时从情景边蒸馏）
                     抽象、统计、慢速形成

巩固 = 情景边"毕业"产出 meta 边，情景边本身保留（P2）
```

---

## 2. 引擎：类型加权激活扩散（已实现，需扩展）

### 2.1 扩散系数表

| 边类型 | 扩散系数 | 生物学对应 | 状态 |
|---|---|---|---|
| `caused` | +1.0 × decay | 谷氨酸强兴奋 | ✅ 已实现 |
| `fact` | +0.8 × decay | 语义关联 | 🔲 新增 |
| `meta` | +0.6 × decay | 皮层自上而下 | 🔲 新增 |
| `enabled` | +0.5 × decay | 弱兴奋 | ✅ 已实现 |
| `co_occurrence` | +0.2 × w(t) × decay | Hebbian LTP（权重动态） | 🔲 新增 |
| `prevented` | **−0.3** × decay | **GABA 抑制** | ✅ 已实现（独家） |
| `no_effect` | 0.0 | 无连接 | ✅ 已实现 |

公共参数：decay = 0.7/跳，threshold = 0.1，max_hops = 5（hippocampus-design §3.1）。

### 2.2 检索流水线（读路径）

```
query
  │
  ├─ [1] 查询分类（轻量 rule → LLM fallback）
  │       fact 型（"用户喜欢什么"）→ 直查 fact 边为主
  │       causal 型（"为什么失败"）→ 扩散为主
  │       temporal 型（"三月时的状态"）→ 有效期过滤为主
  │       不确定 → 三路并行
  │
  ├─ [2] 种子选取：DG SimHash + BM25/embedding 双通道
  │
  ├─ [3] 激活扩散（§2.1 系数表，CSR SpMV）
  │       · 记录激活轨迹（P4）：seed → 经过的边 → 浮出的节点
  │       · 翻转路径标记：区分「直接命中」和「扩散浮出」（HeLa-Mem Top-k ∪ Top-m）
  │
  ├─ [4] RRF 融合：直查结果 ∪ 扩散结果，score = Σ 1/(60 + rank)
  │
  └─ [5] 分层返回（OpenViking L0/L1/L2）
          L0: 一句话边摘要（~50 tok）—— 默认
          L1: 边 + 邻接上下文（~500 tok）—— agent 要求展开
          L2: 全文 + 激活轨迹 —— 调试/审计
          严格 token budget（max_tokens 参数，roadmap 已有候选）
```

**和现有实现的差距**：现有 `spreading_activation` 只做因果边 + 无轨迹记录。
改动集中在 `hippocampus.rs`：边类型泛化 + 轨迹收集 + 翻转标记。

### 2.3 写入流水线（写路径）

```
session turns / agent 显式调用
  │
  ├─ [1] LLM distill（一次调用三种产出，unified-memory-design §4）
  │       facts:     [{key, value, scope}]
  │       decisions: [{decision, outcome, relation}]
  │       insights:  [{pattern, confidence}]      → meta 边候选
  │
  ├─ [2] DG pattern separation：SimHash 去重，相似决策归组
  │
  ├─ [3] CA1 新异性检测：扩散预测 vs 实际结果
  │       surprise = 1 − sim(predicted, actual)
  │       surprise > 0.5 → 值得记（自动提升优先级）
  │
  ├─ [4] 写入 + 矛盾短路：新边自动失效被推翻了同决策旧边（已实现）
  │
  └─ [5] Hebbian 更新：本次激活集合内两两共现边 w += η
```

---

## 3. 循环：不可变巩固（SWR 2.0）

### 3.1 触发

不再按日历触发（roadmap 已有 noveltyEntropy 候选）：

```
trigger = novelty_entropy(recent_decisions) > θ   OR   手动 causal-memory sleep
```

### 3.2 执行（不可变，P3）

```
old_graph（只读）
  │
  ├─ SWR 回放：随机采样因果链，forward + reverse replay
  │     LTP: 链上边 w ×= 1.05；replay_count += 1
  │     LTD: 全局 w ×= 0.99；replay_count > 3 的减半衰减
  │
  ├─ Hub 检测：D(v) = Σw > δ_hub → LLM 蒸馏成 meta 边（HeLa-Mem 公式 2）
  │     ※ 情景边保留，meta 边是增量（P2）
  │
  ├─ GC（三重复合判据，HeLa-Mem）：
  │     删除 ⟺ w < δ_prune AND dormant > δ_age AND 近期零访问
  │
  ├─ Q-value 更新：被成功使用的边 Q 值上调（MemRL 式，替代静态 confidence）
  │
  └─ 产出 ConsolidationResult {
       new_graph:   clone + delta（原图不动）
       delta_log:   每一步 LTP/LTD/GC/distill 的审计日志
       instructions: 本次引导参数
     }

上层审查 → 接受：原子切换 graph = new_graph
        → 丢弃：原图完好
```

### 3.3 `instructions` 引导参数（Dreams 对齐）

```bash
causal-memory sleep --instructions "focus on causal lessons; ignore routine ops"
```

是高层综合引导（聚焦方向 / 保留什么 / 输出组织），不是逐条编辑命令。

---

## 4. 这个系统独家拥有什么

| 能力 | 本系统 | HeLa-Mem | OpenViking | Mem0 | Zep | Letta | Dreams |
|---|---|---|---|---|---|---|---|
| 类型化因果语义（caused/enabled/prevented） | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **prevented 负扩散（抑制侧）** | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Hebbian 动态共现边（兴奋侧） | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| 巩固不可变（delta+clone） | ✅ | ❌ | ❌ | n/a | n/a | ❌ | ✅ |
| 情景/语义共存 | ✅ | ⚠️ 蒸馏后模糊 | ❌ | ❌ | ⚠️ | ❌ | ✅ |
| 检索激活轨迹 | ✅ | ❌ | ✅ 目录轨迹 | ❌ | ❌ | ❌ | ❌ |
| 分层加载 L0/L1/L2 | ✅ | ❌ | ✅ | ❌ | ❌ | ⚠️ | ❌ |
| Compaction survival 实证 | ✅ +20.8pp | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Q-value 动态效用 | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| 一张图统一所有记忆类型 | ✅ | ⚠️ 仅情景 | ⚠️ 文件系统 | ❌ | ⚠️ 仅时序 | ❌ | ❌ |

**没有任何一行是别家有的；最后两行是组合独有的。**

---

## 5. 实施路线（和 roadmap 对齐）

| Phase | 内容 | 依赖 | 预估 |
|---|---|---|---|
| 1 | 事实边：`agent_facts` 表 + `record_fact`/`search_facts` + 矛盾失效复用 | 无 | 2 天 |
| 2 | 统一检索：`search_memory` RRF 三层融合 | P1 | 1 天 |
| 3 | LLM distill ingest：一次调用三种产出 + CLI | P1 | 1 天 |
| 4 | 边类型泛化：扩散引擎支持 fact/meta/co_occurrence 边 + 激活轨迹 + 翻转标记 | P2 | 3 天 |
| 5 | Hebbian 运行时权重：共现边建边 + 更新规则 | P4 | 2 天 |
| 6 | SWR 2.0：不可变 delta+clone + instructions + 巩固日志 + 三重 GC | 无（可提前） | 3 天 |
| 7 | Q-value 动力学：替代静态 confidence | P6 | 2 天 |
| 8 | Benchmark 战役：LoCoMo/LongMemEval/Memora distill 重跑 + 正式消融 + token 效率 | P1–P7 | 3 天 |

**总计约 17 天。** Phase 1–3 是 unified-memory-design 的原计划（benchmark 收益最快）；
Phase 6 可以并行提前（不依赖事实层，且是安全底线）。

## 6. 风险（诚实清单）

1. **工具数量膨胀**：13+ 个 MCP 工具 vs insights/14 "complete-looking is the enemy
   of depth"。对策：`search_memory` 作为默认入口，其他工具退化为专家模式；
   CLAUDE.md 引导只教三个（search_memory / record_decision / trace_cause）。
2. **LLM distill 的 ingest 成本**：每 session 一次 LLM 调用。对策：distill 可选，
   未配置时退化为规则提取（现状），零侵入默认。
3. **图规模**：10 万+ 节点时 CSR 重建成本。对策：增量 CSR（已有 rev_to_fwd_idx
   的教训），巩固时才全量重建。
4. **Benchmark 风险**：事实层可能达不到 75–80%。对策：Phase 8 先跑小规模
   （200 题）验证 distill 收益，再决定全量投入。

---

## 参考资料

- [design.md](design.md) — 问题与压缩免疫实证
- [hippocampus-design.md](hippocampus-design.md) — DG/CA3/CA1/SWR 工程映射 + 扩散算法
- [unified-memory-design.md](unified-memory-design.md) — 三层存储 schema + MCP 工具 + distill 方案
- agent-teardown `papers/daily/2026-07-30-helamem-analysis.md` — Hebbian 更新规则 + 三重 GC + 消融数据
- agent-teardown `papers/daily/2026-07-30-dreams-api-analysis.md` — 不可变巩固 + instructions + 伪代码
- agent-teardown `papers/daily/2026-07-30-openviking-analysis.md` — 分层加载 + 可观察检索
- agent-teardown `papers/daily/2026-07-29-memrl-analysis.md` — Q-value 记忆动力学
- [roadmap.md](../roadmap.md) — 与本架构对齐的执行清单
