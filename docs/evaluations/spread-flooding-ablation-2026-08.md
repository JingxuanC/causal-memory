# Spread 洪泛与物化瓶颈：ablation 发现记录 · 2026-08-24

> 来源：formal ablation harness（`benches/ablation/`）在两种库上的四臂实验。
> 本文记录两个 roadmap 之外的发现：**物化阶段的 confidence 预过滤瓶颈**
> （已修复，`f6405dd`）和 **小而密图上的 spread 激活洪泛**（治理中）。
> 所有数字可复跑：harness 无 LLM、无 judge，指标为检索级（evidence hit /
> mean rank / pool tokens）。

## 实验设置

- **四臂**：baseline（生产管线）/ no-spread（`Memory::disable_spread()`，零跳纯种子）/
  no-inhibition（DB 副本上 prevented→caused 翻转）/ no-swr（DB 副本上 q_value 抹平到 0.5）。
- **LME 大库**：`benches/longmemeval/db/longmemeval_distill.db`，n=100 真实问题，
  evidence 来自数据集 has_answer turns。
- **真实库**：`~/.local/share/causal-memory/causal.db` 的 sleep-consolidated 副本
  （2 轮 sleep 后 q_value 从全 0.5 变为 24 个 distinct 值，0.30–1.00），
  `--self-queries` 模式：每条因果边派生 query=outcome 文本、gold=decision 所在边
  （按 `causal:{edge_id}` 精确匹配），n=219。
  **口径注意**：query 是 outcome 原文，对种子检索是"简单模式"，最大化了
  seed-only 与 spread 的反差；释义查询下反差预期收窄。

## 发现 1：物化瓶颈（已修复）

`materialize_causal` 的设计是"activation 主导排序，confidence 只做平局加赛"，
但实现里 SQL 预过滤先执行：`edges_touching_chunks` 按
`ORDER BY confidence DESC LIMIT 20` 截断候选，`rank_edges_by_activation`
只能在幸存者里重排。高激活、低 confidence 的金边根本活不到重排。

回归测试（`store/tests.rs` `test_edges_touching_chunks_activation_prefilter_surfaces_gold`）：
金边 confidence 0.3 / 端点激活 0.9，对 25 条 confidence 0.9 / 激活 0.05 的干扰边——
旧排序下金边排第 26 必被截断，修复后排第一。

**修复**（`f6405dd`）：激活分数经 `WITH act(id,a) AS (VALUES ...)` CTE 传入 SQL，
排序改为 `MAX(ABS(端点激活)) DESC, confidence DESC, id`，confidence 回到平局加赛位置。
副作用是正的：`record_access` 的 access boost 现在打在激活正确的边上。

## 发现 2：spread 激活洪泛（治理中）

修复预过滤后真实库 baseline 仅从 12.8% 回到 14.2%，未翻正。深挖结论：

- 真实库 433 chunks，但图实际 **1834 节点**（Hebbian 共现边 + meta 边贡献了
  大量连接，形成 hub）。
- 每次查询 spread 激活 **~1000–1100 节点**，且大量**饱和在 1.0**。
- 金边端点激活 0.7975 时，有 ~500–600 个节点比它强；金边两端都到 1.0 时，
  仍有几百个节点并列——**平局落回 confidence 决胜，金边再次输掉**。
- 即：在小而密的图上，扩散的关联覆盖本身把精度稀释到 top-20 之外，
  这不是预过滤能修的。no-spread 95% 的对比度是真实机制，不是测试 bug。

LME 大库不受影响的原因：大图激活稀疏、不饱和，spread 维持正贡献（+1pt）。

## 数据（四臂，hit rate / mean rank / pool tokens）

**LME 大库 n=100（回归检查，修复前后逐位相同）：**

| arm | 值 |
|---|---|
| baseline | 84.0% / 2.6 / 1221 |
| no-spread | 83.0% / 2.5 / 1035 |
| no-inhibition / no-swr | 84.0% 同 baseline（该库 1 条 prevented、q_value 全 0.5，两臂 vacuous） |

**真实库 self-queries n=219（修复前 → 修复后）：**

| arm | 修复前 | 修复后 |
|---|---|---|
| baseline | 12.8% / 14.8 / 1837 | 14.2% / 15.5 / 1850 |
| no-spread | 95.0% / 10.4 / 892 | 95.0% / 10.4 / 892（不变） |
| no-inhibition | 12.8% / 14.8 / 1839 | 14.2% / 15.5 / 1851 |
| no-swr | 12.8% / 15.2 / 1833 | 11.9% / 15.3 / 1848 |

结果文件：`benches/ablation/results/ablation_20260824_{130735,145421,152315,152844}_summary.json`
（130735 = LME 首轮，145421 = 真实库修复前，152315/152844 = 修复后真实库/LME）。

## 结论

1. **物化预过滤必须感知激活值**——已修复并钉死回归测试；LME 零回归（逐位相同），
   每查询延迟 70–105ms 不变（CTE 上限 900 chunks，远低于 SQLite 绑定限制）。
2. **spread 洪泛是小而密图的真实精度杀手**，治理方向（按优先级）：
   度归一化 / fan-out 约束（hub 摊薄，Hebbian 共现边是主要 hub 来源）、
   hop 距离决胜（同激活近者优先，confidence 再后移）、激活 top-K 剪枝。
3. **no-swr 首次获得非 vacuous 信号**（mean rank +0.4 变差），Q 播种有用但
   影响被下游压缩；no-inhibition 信号被洪泛掩盖，待治理后复测。
4. 实验注意：真实库副本会被检索副作用演化（record_access / Hebbian flush，
   q_value distinct 24→115、有效边 221→220），跨轮对比有轻微底噪；
   严格对比应每次从原库重新 .backup + sleep。
