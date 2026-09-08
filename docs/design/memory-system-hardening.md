# 记忆系统完善方案：形式化验证 + 元因果层 + Harness Agent

> 2026-09-08 · 基于探索者 69 轮研究产出 + 代码审查
> 状态：提案，待评审后进入 roadmap

## 背景

探索者在 69 轮自主研究中完成了三件可用于本项目的工程产出：

1. **DoVerifier 源码级逆向**（`exploration/doverifier/`，635 行 Python）——do-calculus BFS 引擎，含 d-分离精确判定，发现其 `Eq()` bug 并定位 workaround
2. **MinimalSCM 合成因果世界**（`exploration/minimal_scm.py`，130 行）——已知 ground truth 的线性 SCM 生成器，支持观测/干预采样、ATE 理论计算
3. **CMB 评分 CLI**（`exploration/cmb_score.py`，273 行）——Layer 1 (SHD) + Layer 2 (DoVerifier 可识别性) 双维度评分

代码审查发现三个验证空白，对应本方案的三个方向。

---

## 方向一：形式化因果验证（工程层）

### 1.1 DoVerifier d-分离作为 `refute.rs` 第四 refuter

**现状**：`refute.rs` 有三种图结构启发式（neighbor Jaccard / edge-disjoint path count / random source replacement），灵感来自 DoWhy 但非形式化因果推断。

**空白**：如果 agent 记录了 `A → B`，但图中存在 `A ← C → B`（C 是混杂因子），当前 confounder test 用 Jaccard 邻近度近似，无法精确判定。

**方案**：将 d-分离检查作为第四 refuter：

```rust
// refute.rs 新增
SingleTest {
    name: "d_separation",
    result: if is_d_separated(graph, edge.from, edge.to, conditioning_set) {
        TestResult::Refuted  // d-分离成立 → 边不可能是直接因果
    } else {
        TestResult::Robust   // d-连接 → 边可能成立
    },
    ...
}
```

**实现路径**：
- Phase A：纯 Rust 实现 `is_d_separated()`（moralization + ancestral subgraph，约 100 行，参考 DoVerifier `causal_equiv.py:10-45`）
- Phase B：对 `causal_edges` 全图跑 d-分离，标记与现有 refuter 结果冲突的边，人工审查后校准阈值
- 不引入 Python 依赖，DoVerifier 仅作算法参考

### 1.2 MinimalSCM 校准 refute.rs 阈值

**现状**：refute.rs 的 grade 分布（A/B/C/D/F）阈值是手工设定的，没有用 ground truth 验证过。

**方案**：
1. 用 MinimalSCM 生成合成 DAG（已知哪些边是真因果、哪些是伪相关）
2. 将合成边灌入 causal-memory（模拟 agent 记录，包括故意注入的伪因果）
3. 跑 `refute.rs` 三+一测试
4. 计算 precision/recall，校准各测试的通过/拒绝阈值
5. 产出：`docs/evaluations/refuter-calibration.md`

**关键指标**：
- 伪因果检测率（refuted 比例）
- 真因果误杀率（被错误 refuted 的比例）
- 各 refuter 的独立贡献（ablation）

### 1.3 CMB 交叉验证 `prediction_report`

**现状**：`prediction_report` 的预测准确率基于自然解析（outcome 落盘时自动判定 correct/ambiguous），没有合成 ground truth 对照。

**方案**：
1. 用 MinimalSCM 构造已知可识别/不可识别的干预查询
2. 通过 `intervention_query` 查询，记录预测
3. 与 CMB Layer 2 的 ground truth verdict 对比
4. 产出：`intervention_query` 在合成数据上的校准度报告

**回答的问题**：当前 `intervention_query` 的"safe / warning / danger"标签，在已知 ground truth 下准确率多少？

**已完成（2026-09-09）**：`tests/intervention_calibration.rs` + `docs/evaluations/intervention-calibration.md`。结果：可解类（真危险/真安全/prevented）100% 准确；混淆类（潜变量驱动的伪 caused 边）**100% DANGER 过声称**——结构性缺陷（relation 词表无关联性类型 + 链遍历 relation 盲视），修复方向在上游（抽取器/ refuter 标注），回归守卫已钉住基线。另发现种子交叉匹配风险：查询与历史共享 token 时 summary 聚合层不保证相关性。

**修复已落地（2026-09-09，PR #28）**：
1. **co_occurrence relation 全链路**：抽取器新增第 4 种 causal_relation（机制不清/疑似共同原因时用，替代 caused）；schema v16 迁移拓宽 CHECK；trace.rs 链遍历排除非因果 relation。校准新增 confounded_tagged 类：**0% 过声称**。残留局限：抽取器错标为 caused 时仍 100% 过声称（信号不在图中，靠抽取器判别力改进）。
2. **BM25 种子门控**：`search_causal_bm25_gated`（0.3 × top 相对分数线）仅用于 intervention_query 种子回退，消除共享 token 的交叉匹配。

### 1.4 多证据融合 refuter（1.2 校准的衍生任务，已完成）

**动机**：1.2 校准发现纯结构 refuter 存在 keep-flag 前沿（二者之和 ~110%，密度只改变证据可裁判性，不改变真/伪边结构签名的可分离性）。突破前沿必须引入 NodeData/EdgeData 中未用的非结构证据。

**已完成（2026-09-08）**：第 5 个 refuter **temporal**——event_time(cause) > event_time(effect) 判 Refuted，无时间戳/同时刻弃权。校准器种植时序证据（真边时间沿拓扑递增，伪边保持随机方向约一半颠倒）后：flag 35.7% → **64.3%**，keep 72.9% → 71.3%（真边零误伤），前沿被打破（keep+flag ≈ 135%）。

**后续可做**：同模式扩展激活统计 refuter（q_value/replay_count 相关性）、边权 refuter（weight vs 社区基线）。详见 `docs/evaluations/refuter-calibration.md`。

---

## 方向二：元因果层（认知层）

### 2.1 矛盾主动检索

**现状**：`search_causal` 返回最相似的历史——确认 agent 当前意图的过去经验。但没有反向检索：**与当前意图矛盾的历史**。

**方案**：新增 `search_contradicting` 内部路径（不暴露为新 MCP 工具，挂在 `search_causal` 的 `explain=true` 分支）：

```
agent 意图: "用 Redis 做缓存"
search_causal 返回: "Redis 缓存成功了" (positive)
search_contradicting 返回: "Redis 缓存雪崩了" (negative, same task_tag)
```

实现：在现有 retrieval 上 polarity 反转过滤 + 相同 task_tag 约束。

**已落地（2026-09-09，PR #29）**：
- store 层 `search_causal_bm25_contradicting(task_tag, query, pool_limit, out_limit)`：复用 BM25 候选池，belief 取排名最高且有效 polarity 已知的条目（即普通 search_causal 会确认的意图）；返回有效 polarity 与 belief 相反的条目。pool 比输出 limit 大（4×，下限 20）——矛盾项按定义不在 top 排名里，同尺寸池几乎必然零命中。polarity 未知的条目永远不会矛盾。
- ops 层：`search_causal` 主体下沉为私有 `search_causal_body`；explain=true 且有 query 时，包装器追加 `⚠️ contradicting history` 段（每条带 `[contradiction: opposes the success-belief of your top hit]` 标签）。explain=false 输出字节不变。检索失败静默吞掉（普通搜索结果必须独立成立）。
- 测试：store 层 3 个（反向 polarity 命中 / task_tag 作用域 / 无 belief 为空）+ ops 层 explain 集成 1 个。

### 2.2 自动偏差检测（记忆漂移监控）

**现状**：有半衰期衰减（`halflife_hours`）和手动 `invalidate_decision`，但没有**自动检测记忆系统性偏差**的机制。

**方案**：在 `sleep --auto` 固化周期中添加偏差审计阶段：

```rust
// consolidate/stages.rs 新增 Stage: BiasAudit
// 检测模式：
// 1. 同一 task_tag 下 polarity 分布偏移（全是 positive → 可疑）
// 2. 同一 decision pattern 的 outcome 方差异常低（自我强化信号）
// 3. 最近 N 条记忆的 confidence 均值漂移（系统性高估/低估）
```

产出：审计报告写入 `consolidation_report`，标记可疑边供人工审查。

### 2.3 记忆溯源增强

**现状**：`discovered_by` 记录来源（rule/llm_inferred/user_feedback），`recall_audit` 记录检索历史。但没有记录"这条记忆影响了哪些后续决策"。

**方案**：`record_decision` 时可选记录 `influenced_by`（哪条记忆影响了这个决策），形成"记忆影响链"。长期可用于检测：错误记忆是否导致了更多错误决策（错误传播路径）。

---

## 方向三：Harness Agent

在记忆系统完善之后构建。三层设计：

### 3.1 合成验证 harness（P0）

复用方向一的全部产出。回答："提取的因果关系准确吗？"

### 3.2 对抗注入 harness（P1）

故意注入 10% 伪因果边 → 跑 `refute.rs` + 新 d-separation refuter → 测量检测率/误杀率/纠正延迟。回答："系统能自我纠错吗？"

### 3.3 长期漂移 harness（P2）

真实 agent 使用场景 + 方向二的偏差审计 → 定期产出漂移报告。回答："系统会自我强化偏差吗？"

---

## 与现有 roadmap 的关系

| 本方案 | 对应 roadmap 项 | 关系 |
|--------|----------------|------|
| 1.1 d-separation refuter | refute.rs 已有三测试 | 新增第四测试，非替换 |
| 1.2 refuter 校准 | 无对应项 | 填补验证空白 |
| 1.3 prediction 校准 | `prediction_report` 已有 | 增加合成对照组 |
| 2.1 矛盾检索 | 无对应项 | 新增能力 |
| 2.2 偏差审计 | `sleep --auto` 已有 diversity gate | 在 diversity gate 之上增加 bias audit |
| 2.3 记忆影响链 | `record_decision` 已有 context 参数 | 新增 influenced_by 可选字段 |
| 3.x harness | benches/ 已有基准测试 | 从"功能基准"扩展到"自我纠错验证" |

**不重复**：半衰期衰减、provenance、prediction_report、diversity gate 均已 shipped，本方案在其上叠加。

**对齐 Rung-3 路线**：roadmap 明确"Rung-3 SCM ground truth 在开放世界中 out of scope"，本方案不挑战这一判断——d-separation 用于**图结构验证**（不是 SCM 反事实），MinimalSCM 仅用于**合成基准测试**（不是开放世界推理）。

---

## 实施优先级

| 优先级 | 项 | 工作量估计 | 依赖 |
|-------|---|-----------|------|
| **P0** | 1.2 MinimalSCM 校准 refute.rs | 1 天 | 无（复用现有产出） |
| **P0** | 1.1 Phase A: Rust is_d_separated() | 1 天 | 无 |
| **P1** | 1.1 Phase B: d-separation refuter 集成 | 0.5 天 | 1.1 Phase A |
| **P1** | 1.3 prediction_report 合成校准 | 0.5 天 | 1.2 |
| **P1** | 2.1 矛盾主动检索 | 1 天 | 无 |
| **P2** | 2.2 自动偏差审计 | 1-2 天 | 2.1 |
| **P2** | 2.3 记忆影响链 | 1 天 | 无 |
| **P3** | 3.1 合成验证 harness | 1 天 | P0+P1 全部 |
| **P3** | 3.2 对抗注入 harness | 1 天 | 3.1 |
| **P4** | 3.3 长期漂移 harness | 持续 | 真实使用数据 |

---

## 探索者产出索引

| 文件 | 位置 | 在本方案中的用途 |
|------|------|----------------|
| DoVerifier 源码 | `exploration/doverifier/` | 1.1 算法参考（d-separation 实现） |
| DoVerifier Eq bug 分析 | `exploration/journal.md` 第68轮 | 类型安全设计教训 |
| MinimalSCM | `exploration/minimal_scm.py` | 1.2/1.3/3.1 合成 ground truth |
| CMB 评分 CLI | `exploration/cmb_score.py` | 1.3 评分框架 |
| DoVerifier 集成测试 | `exploration/doverifier_integration_test_v2.py` | 1.1 正确性验证参考 |
