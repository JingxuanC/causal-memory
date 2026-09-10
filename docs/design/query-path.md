# 查询路径（Query Path）设计文档

> 状态：已落地（2026-08-26 复盘版）。本文回答「查询到底走 SQL 还是走图」：**分层混合**——
> 播种层和物化层都是 SQLite，排序层是图（spreading activation）。图是查询表面的核心引擎，
> 但不是 SQL 的替代品。配套阅读：write-path.md（写入链路）、one-graph-convergence.md（收敛计划）。

## 0. 一句话

**查询 = SQL 播种 → 图扩散（排序）→ SQL 物化。** SQLite 的持久索引（BM25/embedding）选出 ≤16 个种子，
种子在图上做一次带类型的激活扩散得到排序，再回 SQLite 拿完整行拼响应。图不可用/无激活时
退回纯 SQL 的 dual-pool RRF 老路径（兼任 A/B 回归对照）。

## 1. 入口一览

```
search_memory       统一检索：facts + causal 一图排序（默认路径 = 图引擎）
search_causal       因果检索：图优先（hippocampus_search），fallback BM25/语义
search_facts        单层事实检索：纯 SQL（语义→BM25），不进图
intervention_query  干预预测：纯 SQL（语义链→BM25→LIKE 兜底，沿链行走）
hippocampus_search  联想检索：纯图（正向/反向激活扩散，图节点文本直出）
```

## 2. 统一引擎（search_memory 主路径）

```
ranked_hits(query, task_tag, scope, limit)
  └─ unified_spread_hits           ← 图引擎（优先）
       ├─ 播种 unified_seed_ids    ← SQL（SQLite 索引）
       │    ├─ store.bm25_seed_ids(query, scope, 16)
       │    │    ← 持久 BM25 倒排索引，刻意横跨 facts + causal 双命名空间
       │    └─ 语义（有 embedder 时）
       │         ├─ search_facts_semantic → fact:{id} 种子
       │         └─ search_causal_semantic_entity_boosted → decision_id 种子
       ├─ ensure_fresh_for(seeds)  ← 种子在图里找不到节点 → 立即全量重建
       │    （保证图引擎绝不弱于它替代的 store 直查路径）
       ├─ graph.spreading_activation_seeded(query, seeds, task_tag, true)
       │    ← ★ 排序在这里：一张图上沿 typed 边做激活扩散
       ├─ split_typed(results)
       │    ├─ fact:{id} → fact_ids（按激活序）
       │    ├─ chunk → chunk_activation（按激活序）
       │    └─ scope 枢纽跳过；|activation| < 0.1 阈值丢弃
       └─ 物化                          ← SQL（SQLite 查行）
            ├─ materialize_facts   → store.facts_by_ids(fact_ids) 按激活序截断 limit
            └─ materialize_causal  → store.edges_touching_chunks(chunk_ids, task_tag)
                                     ← 取触及激活 chunk 的边，按端点最大激活重排

  └─ dual_pool_fused                ← fallback（纯 SQL，Phase B 前的老路径）

副作用：每次查询把激活节点对缓冲进 Hebbian 共现（cap 4000），重建时落库（与写入链路对称）。
```

### 2.1 为什么物化还要回 SQL

图上只有 id/text/q_value 等激活引擎需要的最小属性；返回给用户的完整行需要 confidence、
event_time、source、outcome_polarity、task_tag——这些住在 SQLite。图管「哪些记忆相关、多相关」，
SQL 管「这些记忆的完整事实」。物化是 2~3 次点查（facts_by_ids / edges_touching_chunks），
不是扫描。

## 3. dual-pool RRF fallback（纯 SQL）

```
每层 per_layer = limit × 2（下限 10）
facts 层    ：语义（search_facts_semantic）→ 空则降级 BM25（search_facts_bm25）
causal 层   ：语义（search_causal_semantic_entity_boosted）→ 空则降级 BM25
A2 hop 扩展 ：search_causal_hop(seed_edge_ids) → 1-hop 邻接 + 2-hop distilled 跳跃
RRF 融合    ：RRF_K=60，layer-prefixed key（fact:{id} / causal:{edge_id} / hop），
             秩交错而非分数加权
```

触发条件：图为空（startup 未建）/ 种子为空 / 扩散无结果 / 物化两边都空。
它同时是 A/B 回归对照：`ranked_hits` 的 mode 标签（spread / semantic / bm25）暴露在响应里，
可观测两条路径谁在服务。

## 4. 各工具查询矩阵

| 工具 | 排序 | 播种 | 物化 | fallback |
|---|---|---|---|---|
| search_memory | 图（激活扩散） | SQLite BM25/语义 | SQLite 查行 | dual-pool RRF |
| search_causal | 图优先 | 子串种子 | 图节点文本 | BM25/语义 SQL |
| search_facts | 纯 SQL（余弦/BM25） | — | SQLite | 语义空→BM25 |
| intervention_query | 纯 SQL（链式行走） | 语义→BM25→LIKE | SQLite | 逐级降级 |
| hippocampus_search | 纯图（正/反向） | 子串种子 | 图节点文本 | 无（空图返回 None） |

## 5. 设计决策：图为什么只做排序

1. **索引住在 SQLite**：`index_chunk`（BM25）和 `put_embedding` / `put_fact_embedding`（向量）
   写入时就落库——播种层复用同一份数据，不需要第二套索引。
2. **图的价值是跨类型联想**：facts 与 causal chunks 通过实体链接/scope→fact 边连成一张网，
   一次扩散同时带出两类记忆——这是两池 RRF 硬拼做不到的（RRF 只做秩融合，不做关联）。
3. **图是惰性重建缓存**：`from_store` 全量重建 O(V+E)，5 写/30s 或 seed-miss 触发；
   图坏了随时重放，SQLite 仍是唯一真相（与写入链路同一条一致性承诺）。
4. **数据完备性归 SQL**：激活只带排序，响应需要完整行——物化层避免图节点携带冗余属性。

## 6. 一致性边界（诚实）

- **图可能落后于 store**（崩溃/未重建）：`ensure_fresh_for` 在 seed-miss 时立即重建，
  保证图引擎服务的结果不弱于 store 直查；懒重建是最终兜底。
- **图 boost 消融已度量（2026-08-21，multi-session 133 题 × deepseek-chat × v2）**：
  env-gated 的 hippocampus_boost 修好三处死代码后（!with_facts 提前返回吞掉 boost 块 /
  整句播种永远空 / 去重条件 snippet.contains(e) 滤掉一切），剂量-响应曲线为：
  基线 0 行 42.9% → 20 行 44.4% → **106 行/题 46.6%（+3.8pp，峰值）** → 250 行未隔离 40.6%。
  两个决定性变量：**scope 隔离**（Some(&q.question_id) 一行让 -3 翻到 +5，与 da15204
  的链接侧隔离同一原则）和**证据覆盖 vs 稀释的平衡**（计数题需要关联尾部的跨 session 证据，
  不是越少越干净）。剩余噪音的根因在图结构而非传播算法：raw ingest 图的 120,527 条边
  全是 temporal 轮次链（对话转写顺序，0 个 fact 节点）——在其上扩散 ≈ 顺读对话，
  与 BM25 检索高度冗余。真正的语义边（fact 节点 + entity_link_facts + 蒸馏边）只在
  distill 模式存在（历史 8/4 全量：raw 63.6% vs distill 71.2%，+7.6pp 来自事实层 prompt，
  未叠加图 boost）。
- **实测基线**（2026-08-26，真实库）：图 1832 节点 / 7857 有效边 / 29 分量 / 最大 1777；
  统一引擎 seed 上限 16，扩散单次 O(种子 × 度 × 跳数)，物化 2~3 次点查。

## 7. 欠账清单（还没做 / 已知取舍）

1. **双路径维护成本**：unified 引擎与 dual-pool RRF 是两套排序逻辑，行为差异只能靠 A/B 和
   单测护栏约束；fallback 尚未退役（等 unified 在 LongMemEval 稳定压过它）。
2. **物化每查询 2~3 次 SQL 往返**：facts_by_ids + edges_touching_chunks 各一次；若走语义播种
   还多一次 embedding 请求。可批量合并，但当前延迟在单机 SQLite 上可忽略。
3. **图引擎对 bench 分数的贡献未度量**：hippocampus_boost env-gated 默认关，缺少开/关消融。
4. **无查询路径可观测性**：mode 标签只在响应串里，没有指标/日志统计 spread vs semantic vs bm25
   的服务比例、图命中率、seed 空率——无法事后判断 fallback 是否被过度使用。
5. **intervention_query / search_facts 未接入图**：单层工具仍是纯 SQL；若实体链接质量继续
   上升，intervention 的链式行走可能受益于图的因果边（待验证）。

