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

**已落地（2026-09-09，PR #30）**：
- **Stage 5 BiasAudit**（consolidate 管线末尾，post-consolidation 种群上运行）：三个检测器在 `store/bias_audit.rs`——
  1. `audit_polarity_skew(min_edges=5, skew_ratio=0.9)`：同一 task_tag 下已知 polarity 的 outcome 一边倒 ≥90% 即标记该 tag 全部边（`bias_flag = polarity_skew:<tag>`）。
  2. `audit_low_variance_decisions(min_count=3)`：同一决策（from chunk）重复 ≥3 次且已知 polarity 零方差 → 自我强化嫌疑（`bias_flag = low_variance:<snippet>`）。
  3. `audit_confidence_drift(window=20, threshold=0.15)`：最近 N 条边 confidence 均值 vs 历史均值漂移超阈值 → 仅进报告，不标边（无单一边有错）。
- **纯标注原则**：bias_flag 只供人工审查（`bias_flagged_edges()` 是审查队列），检索/衰减/GC 一律无视；dry run 只报告不落标记。检测器失败 best-effort，永不中断固化周期。
- **schema v17**：`causal_edges` 加可空 `bias_flag` 列（一次 ALTER，列存在性守卫幂等）；迁移测试覆盖 v16→v17 数据保留 + 重开幂等。
- **配置项**：`ConsolidateConfig` 新增 `bias_audit_enabled` + 五个阈值旋钮。
- **CLI**：sleep 报告新增 `⑤.1 Bias audit` 段（skew/low-variance/drift 三类发现 + 标记计数）。
- **测试**：consolidate 层 4 个（skew 命中/平衡对照、零方差命中/有方差对照、drift 单测含无基线 None、dry run 不落标记）+ v17 迁移 e2e。326 lib + 48 cli 全绿。

### 2.3 记忆溯源增强

**现状**：`discovered_by` 记录来源（rule/llm_inferred/user_feedback），`recall_audit` 记录检索历史。但没有记录"这条记忆影响了哪些后续决策"。

**方案**：`record_decision` 时可选记录 `influenced_by`（哪条记忆影响了这个决策），形成"记忆影响链"。长期可用于检测：错误记忆是否导致了更多错误决策（错误传播路径）。

**已落地（2026-10-02）**：
- **schema v18**：`causal_edges` 加可空 `influenced_by TEXT`（JSON 数组 `[12, 45]`）。`migrate_to_v18()` 一次 ALTER + 列存在守卫幂等；`CAUSAL_SCHEMA_SQL` 同步。反向查询用 JSON1 `json_each`（rusqlite bundled SQLite ≥ 3.38 自带）。
- **store 层**：`record_decision_full` 新增 `influenced_by: Option<&[i64]>`——落盘前 SELECT 校验过滤不存在的 id（保序去重），校验失败 best-effort 不阻塞记录；`CausalEntry` 加 `influenced_by: Option<Vec<i64>>`（JSON 解析失败降级为 None）；新增反向查询 `influenced_decisions(edge_id)`（只返回 valid 边，按 id 排序）。
- **ops 层**：`record_decision` 响应追加 `🔗 Influenced by: #12, #45`（被过滤的无效 id 以 `skipped unknown id(s)` 注明）；`invalidate_decision` 作废一条边时追加 `⚠️ This edge influenced N later decision(s): …` 警示（最多列 5 条）——错误传播路径的落地检测点。
- **可用性前置修复**：search_causal/search_memory/trace 的命中渲染统一带 `(#edge_id)` 后缀（`format_entry_layered`/`format_lesson_layered`/tag-only/multi-pass/trace 各路径），否则 agent 拿不到 id 无法填 influenced_by。explain=false 相对 explain=true 的字节不变量保持（PR #29 测试绿）。
- **MCP/Python**：`RecordDecisionParams` 加 `influenced_by: Option<Vec<i64>>`（工具总数仍 17）；py 绑定同名可选参数透传；SKILL.md 与 `scripts/causal_memory_client.py` 同步。
- **测试**：store 层 2 个（roundtrip+过滤 / 反向查询+invalidated 排除）+ ops 层 2 个（🔗 响应段 / ⚠️ 传播警示）+ `tests/migration_v18.rs`（v17 数据保留 + 重开幂等）。330 lib + 48 cli 全绿。

---

## 方向三：Harness Agent

在记忆系统完善之后构建。三层设计：

### 3.1 合成验证 harness（P0）

复用方向一的全部产出。回答："提取的因果关系准确吗？"

**已落地（2026-10-02）**：`benches/extraction_eval/`（cli crate 的 `[[bin]]` `causal-memory-extraction-eval`，复用 benches/common 的 LLM 约定）。
- **增量定位**：refuter/intervention 两个 calibration 都绕过抽取直接灌边；本 harness 测的是「自然语言对话 → remember/Distiller 抽取 → 因果边」这一段。
- **生成**：seeded SplitMix64，五类 episode（caused 30 / prevented 15 / enabled 15 / no_effect 20 / confounded 20），模板+槽位池（5 领域 × 双语），2-6 轮含 filler 噪声；决策文本跨 episode 唯一（否则 v9 chunk 复用 + 矛盾短路会毁掉 ground truth）。可选 `--narrate-llm` 改写 + 回验。
- **评分**：归一化包含 / char-bigram Dice 模糊匹配 + 全局贪心指派（防兄弟 episode 偷边）；产出抽取召回率、relation 混淆矩阵、confounded→caused 过声称率（§1.3 基线 100% 的端到端回归表）、no_effect 伪边率、polarity 准确率。
- **selftest（零 LLM）**：mock 抽取器注入已知错误率（miss 15% / rel_err 10% / overclaim 40% / spurious 20%）走真实 store 写入，评分器逐 episode 精确恢复注入判定（300/300），聚合率落在采样噪声内——无 key 环境全链路绿。
- **live run**：待 LLM key（当前环境无 DEEPSEEK_API_KEY）。方法论与验证细节见 `docs/evaluations/extraction-calibration.md`。

### 3.2 对抗注入 harness（P1）

故意注入 10% 伪因果边 → 跑 `refute.rs` + 新 d-separation refuter → 测量检测率/误杀率/纠正延迟。回答："系统能自我纠错吗？"

**已落地（2026-10-02）**：`benches/adversarial_eval/`（cli `[[bin]]` `causal-memory-adversarial-eval`，零 LLM 全链路）。planted-community DAG（移植 refuter_calibration）写入真实 store，10 轮演进中每轮跑 refuter 全量扫 + consolidate + 反证到达（驱动 write-path 矛盾短路）；伪边状态机 hidden→flagged→invalidated 全程跟踪。实测（seed 42，120 真边 + 12 伪边）：
- **检测**：refuter 首轮标 92% 伪边（easy 100% / hard 87.5%），但 71% 真边也被标 D/F（backdoor 在稠密图上过敏）——且 refute 从不写库，advisory only。
- **纠正**：唯一零 LLM 失效机制是矛盾短路，完全由反证驱动——30%/轮反证 10 轮纠正 92%（中位 3 轮），50% 档中位 2 轮 < 10% 档 5 轮，**零反证时纠正率 0**（结构性伪边永不纠正）。
- **误伤**：consolidate 零误杀；矛盾 collateral 3.3%（同 decision 文本的负向真边连带，C7 灰色地带）；BiasAudit 误标 19–36 条真边（合成重复结构触发 low_variance），0 伪边。
- **结论**：自我纠错 = 矛盾短路 × 反证；refuter/BiasAudit 只是审查队列生产者，缺口在「F 级/flag → 自动处置」的路径。详见 `docs/evaluations/adversarial-injection.md`。selftest 4 场景断言电池全绿。

### 3.3 长期漂移 harness（P2）

真实 agent 使用场景 + 方向二的偏差审计 → 定期产出漂移报告。回答："系统会自我强化偏差吗？"

**已落地（2026-10-02）——报表机器就绪，真实漂移数据待 dogfooding 积累**：
- **`causal-memory drift` 子命令**（`--db` / `--json` / `--days N` + 四个阈值旋钮），纯读、best-effort（分节失败进 `section_errors`，永不 panic）。分析逻辑在库层 `causal_memory::drift`（`DriftReport` 可 serde），CLI 只渲染。四节：
  1. **偏差审计快照**：§2.2 三检测器只读跑（不落 bias_flag，写路径仍归 consolidate 阶段）；
  2. **趋势漂移**：per-task_tag 周桶（默认 7 天 × 4 周，锚定最新 event_time 而非 wall clock——合成库可复现）的正向占比 / 均置信度 / 新增边速率，环比超阈值告警（极性 0.15、置信度 0.10、量增速 2×，默认与 §2.2 旋钮同量级）；
  3. **错误传播路径**：invalidated 边中仍有 valid influenced_by follower 的清单（§2.3 影响链的长期用途落地），按 follower 数 top 10；
  4. **自我强化信号**：零方差重复决策 + user_feedback 占比 ≥50% 的 tag（自证循环嫌疑）。
- **sleep 报告接线**：`print_consolidation_report` 末尾追加 ⑥ drift tail（②③④ 节；① 与 ⑤.1 重复故略）。成本低（一次只读分析），无明显理由不接。
- **合成验证**（`tests/drift_report.rs`，零 LLM，3 测试）：健康库四节全静默；漂移库四节各自命中注入（极性渐变 50%→100% 被趋势节捕获且均衡对照 tag 不告警 / 失效边 + 3 valid follower / 5× 零方差 / user_feedback 主导 tag）；空库不 panic + JSON 可序列化。

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
