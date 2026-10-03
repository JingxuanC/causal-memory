# Spread 洪泛与物化瓶颈：ablation 发现记录 · 2026-08-24

> 来源：formal ablation harness（`benches/ablation/`）在两种库上的四臂实验。
> 本文记录两个 roadmap 之外的发现：**物化阶段的 confidence 预过滤瓶颈**
> （已修复，`f6405dd`）和 **小而密图上的 spread 激活洪泛**（已治理，`400cfcd`）。
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
  seed-only 与 spread 的反差。曾预期"释义查询下反差收窄"——2026-08-24 用
  机械释义（去 30% 高 IDF token）实测**未能构造出该反差**：删除式变换的
  查询 token 仍是 outcome chunk 的子集，BM25 覆盖率依旧 100%，种子层照样
  稳命中（两轮四臂逐位一致）。真正的释义需要**同义替换**（查询 token 不在
  原文中），只能由 LLM 或同义词典生成——后续方向：LLM 离线生成一版
  paraphrase 查询集、冻结后复跑本 harness（gold 仍是 edge id，无需 judge）。

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

## 发现 2：spread 激活洪泛（已治理，`400cfcd`）

修复预过滤后真实库 baseline 仅从 12.8% 回到 14.2%，未翻正。深挖结论：

- 真实库 433 chunks，但图实际 **1834 节点**（Hebbian 共现边 + meta 边贡献了
  大量连接，形成 hub；其中 1400 个是 fact 节点）。
- 每次查询 spread 激活 **~1000–1100 节点**，且大量**饱和在 1.0**。
- 金边端点激活 0.7975 时，有 ~500–600 个节点比它强；金边两端都到 1.0 时，
  仍有几百个节点并列——**平局落回 confidence 决胜，金边再次输掉**。
- 即：在小而密的图上，扩散的关联覆盖本身把精度稀释到 top-20 之外，
  这不是预过滤能修的。no-spread 95% 的对比度是真实机制，不是测试 bug。

LME 大库不受影响的原因：大图激活稀疏、不饱和，spread 维持正贡献（+1pt）。

### 治理：通道级 fan-out 约束（2026-08-24，`400cfcd`）

机制：**关联通道**（`Fact` 双向实体链接 + `CoOccurrence` Hebbian 共现边——
学习/派生、稠密、hub 形成）向外传播时按出度分摊 `a / assoc_out_degree`；
**因果家族边**（Caused/Enabled/Prevented/Meta、patch overlay）不分摊。

关键决策——为什么不按经典 Collins & Loftus 全出度分摊：全度归一化会让
2–4 度的普通因果节点上 prevented 的弱负传播（coeff −0.3）被稀释到阈值下，
抑制性信号这个核心特性被误伤（3 个既有测试当场变红）。洪泛的根源是关联
通道的 hub，因果边稀疏（220 条 / 433 chunks）且是主信号；通道级分摊两全，
因果/抑制语义逐位保持。hop 距离决胜未采用：单机制已消除洪泛，最小侵入。

**治理效果（真实库 self-queries n=219，治理前 → 治理后）：**

| arm | 治理前 | 治理后 |
|---|---|---|
| baseline | 14.2% / 15.5 / 1850 | **95.0% / 12.2 / 1273** |
| no-spread | 95.0% / 10.4 / 892 | 95.0% / 10.4 / 892（锚点不变） |
| no-inhibition | 14.2% / 15.5 / 1851 | 95.0% / 12.2 / 1274 |
| no-swr | 11.9% / 15.3 / 1848 | **96.8%** / 11.8 / 1262 |

baseline 回升 **+80.8pt**，与 seed-only 完全持平，且 pool 覆盖更广
（1273 vs 892 tokens）——spread 不再有害；pool tokens −31%（洪泛噪声被挤出）。
**no-swr 首次浮现真信号**：Q flatten 反而 +1.8pt，说明该库上 Q 加权播种
略微过拟合高 Q hub；no-inhibition 仍无可见差异（25 条 prevented 在检索级
指标上无量级影响）。

**LME 大库回归检查（n=100）**：baseline 84.0% 逐位保持，pool tokens
1221→1191（−2.5%，阻尼的预期效果），延迟 p50 154ms / p90 183ms 同区间。
零回归。

结果文件：`benches/ablation/results/ablation_20260824_{154840,155018}_summary.json`。

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
2. **spread 洪泛是小而密图的真实精度杀手**——已由通道级 fan-out 约束治理
   （关联通道按出度分摊，因果家族不分摊，保护抑制性弱信号）；hop 距离决胜
   和 top-K 剪枝留作后备，当前不需要。
3. **no-swr 信号稳定复现**：三轮独立运行（含干净副本重建）逐位一致——
   Q flatten 反而 +1.8pt（96.8% vs 95.0%），即该库上 Q 加权播种轻微过拟合
   高 Q 节点，抹平后多捞 4 条金边。Q 加权是否也该做通道级限制是开放问题。
   no-inhibition 依旧无可见差异（25 条 prevented 低于检索级指标分辨率，
   需更敏感的指标或 LLM 冻结查询集才能检验）。
4. 实验注意：真实库副本会被检索副作用演化（record_access / Hebbian flush，
   q_value distinct 24→115、有效边 221→220），跨轮对比有轻微底噪；
   严格对比应每次从原库重新 .backup + sleep。
