# 写入链路（Write Path）设计文档

> 状态：已落地（2026-08-26 复盘版，Phase C 增量补丁 + 精度护栏后）。本文把「一次写入」从入口到查询表面
> 的完整路径拆开：① SQLite 事务（真相源）→ ② 图 overlay 补丁（立即可见）→ ③ 懒重建（漂移兜底）+
> 静默副作用。配套阅读：one-graph-convergence.md（收敛计划）、long-term-vision.md（终态）。

## 0. 一句话

**写入不重建图。** 每条新记忆先以事务落到 SQLite（唯一真相），然后用 O(1) overlay 补丁把同一内容
增量接进内存 CausalGraph（查询表面），新记忆对**下一条查询**即可见；懒全量重建只作为漂移兜底，
并顺带把 Hebbian 共现缓冲落库。

## 1. 入口（谁在写）

```
MCP server (causal-memory-cli::server) ─┐
Python bindings (causal-memory-py) ─────┼──► Memory::record_decision
CLI / Cooper / sleep 巩固 ──────────────┘    Memory::record_fact
                                             Memory::invalidate_decision
                                             Memory::remember (→ distill 抽取 → 多次 record_*)
```

- `remember`（mem0 式）把原始对话文本交给 LLM 抽取器（`distill`），产出 fact / lesson / causal edge，
  再走同一条 record_* 路径——入口多样，落库路径唯一。
- 所有入口都返回人类可读的确认串（含 id），供上层拼接响应。

## 2. 存储写入（SQLite，单一事务）

### 2.1 record_decision / record_decision_full（crates/causal-memory/src/store/write.rs）

```
1. reuse_or_create_chunk ×2
   精确文本复用（v9）：同文本 chunk 只保留一个节点 id，不再每次新建。
   → 真实库 431 chunks 却有 223 条有效边：同一决策节点的多次记录/多条边。
2. invalidate_contradicted_edges（规则级，无 LLM）
   保守短路：只有「旧边明确负面 AND 新边明确正面」才自动软失效旧边；
   v4 起存储的 outcome_polarity 优先于文本启发式，mixed/neutral 永不触发。
3. INSERT causal_edges
   relation / confidence / task_tag / outcome_polarity 全部落库，返回 edge_id。
```

### 2.2 record_fact / record_fact_replacing（crates/causal-memory/src/store/facts.rs）

```
1. INSERT INTO agent_facts ON CONFLICT(key, value, scope) DO UPDATE
   同 (key,value,scope) 再次写入 = 复活 + 更新 confidence（valid_to/superseded_by 清空）。
2. index_chunk → BM25 持久倒排索引（播种层，不是图）。
3. record_fact_replacing 额外一步：UPDATE 旧值 valid_to=now, superseded_by=新 id
   ——「换 pnpm」这类取代在同一批里完成：新值插入 + 旧值退休 + 谱系记录，无窗口期。
```

### 2.3 invalidate_edge / invalidate_fact

- 一律**软失效**（valid_to 标注）：退出检索但保留审计；`superseded_by` 记录取代谱系（C7）。
- `invalidate_other_facts_for_key` 是 store 级 API：先记新 fact，再退休同 key 的其他值。

### 2.4 设计不变式

- SQLite 是唯一真相：图坏了随时从 store 重放重建（`from_store`）。
- 删除都是软删除；物理清理只发生在重建过滤（`valid_to IS NULL`）。

## 3. 图补丁（overlay，写路径核心）

> 为什么不直接插 CSR？中间插入一条边是 O(E) 且会让所有已存 CSR 边索引移位，所以补丁挂在
> per-node 覆盖图（`patch_fwd` / `patch_rev`），扩散两个方向都先查覆盖图；下次全量重建
> overlay 丢弃，store 重新成为图的唯一来源。

### 3.1 patch_graph_new_edge（memory/mod.rs）

```
append_node(decision, task_tag) ── 幂等：id 已存在则返回旧 idx（chunk 复用只加边）
append_node(outcome)
add_patch_edge(from → to, relation, weight) ── 同 (from,to,relation) 是 upsert，
                                               重复写入更新权重不叠副本
```

### 3.2 patch_graph_new_fact（memory/mod.rs）

```
append_node(scope 枢纽 "scope:{scope}")
append_node(fact 节点 "fact:{id}"，text = "{key}: {value}"，q = confidence)
add_patch_edge(scope → fact)
link_fact_node(fact_idx)   ← 增量链接器，与重建路径共用同一策略
```

`link_fact_node` 的过滤链（2026-08-26 精度修正后与 entity_link_facts 完全一致）：

```
tokenize → 去 LINK_STOPWORDS → 去 df>20 的高频泛词 → 对每个 token 查增量倒排索引
         → scope_matches（冒号命名空间 scope 只连 task_tag 匹配后缀的 chunk）
         → 共享 distinct token ≥ 3 → 双向建边，权重 0.3+0.1·overlap（cap 0.8）
         → 每 fact 上限 8 条，按 (overlap desc, chunk 索引 asc) 截断
```

增量倒排索引（`token_index`）只收录 chunk 节点（fact/scope 非链接目标），由 append_node 维护，
所以写路径链接成本是 O(fact tokens × hits)，不重扫全图。

### 3.3 patch_graph_retire_facts / invalidate_edges_between

- `record_fact_replacing` 之后：retire_node("fact:{id}")——被取代的旧值**立即**停止播种/出现，
  不必等懒重建（重建时才物理消失）。
- `invalidate_decision` 之后：invalidate_edges_between(dec_id, out_id)——O(deg) 有效性翻转，
  被证伪的教训立即停止扩散。

## 4. 新鲜度账本与懒重建（漂移兜底）

```
mark_graph_dirty()          graph_writes.fetch_add(1)          // 每次写 +1
maybe_rebuild_graph()       下次查询时：writes ≥ 5 或距上次重建 ≥ 30s → rebuild
ensure_fresh_for(seeds)     统一引擎：store 解析出的 seed 在图上找不到节点 → 立即重建
                            （保证统一引擎绝不弱于它替代的 store 直查路径）
rebuild_graph_now()         CausalGraph::from_store 全量重建 + flush_cooccurrences()
```

- 常数：`GRAPH_REBUILD_WRITES = 5`、`GRAPH_REBUILD_SECS = 30`（memory/mod.rs）。
- Hebbian 共现（D1）：每次检索把激活节点对缓冲（cap 4000 对，防病态 store 无界增长），
  **只在重建时** flush 到 cooccurrence_edges（0.2 起步，重复共现强化）——真实库该表当前为空。

## 5. 副作用（全部静默失败，绝不阻塞写入）

```
embed_shared(文本) → put_embedding / put_fact_embedding   // 语义播种层
invalidate_semantic_contradictions(0.85)                  // 改写文本的语义矛盾扫描
```

失败只意味着语义检索暂时找不到这条新记忆；`causal-memory embed` CLI 可事后回填。

## 6. 一致性边界（诚实）

- **store 与图不在同一事务**：先提交 SQLite，再打图补丁。进程在中间崩溃 → 图短暂过期，
  由 ensure_fresh_for / 懒重建修复。一致性承诺是"最终一致到最近一次重建"，不是强一致。
- **补丁是幂等的**：append_node 按 id 去重、add_patch_edge 同键 upsert——重放/重试安全。
- **图的覆盖范围**：chunks + facts + scope 枢纽 + 七类 typed 边；BM25/embedding 索引是播种层，
  不是平行图（见 one-graph-convergence.md）。
- **实测基线**（2026-08-26，真实库）：431 chunks / 223 有效 causal 边 / 1400 facts / 1 scope 枢纽 /
  3117 双向 fact↔chunk 链接 / 图上 7857 有效边 / 29 分量 / 最大 1777。

## 7. 欠账清单（还没做 / 已知取舍）

1. **图上退休是补丁层操作，非事务**：replace 的原子性只覆盖 store 侧（同一批 UPDATE）；
   图的 retire_node 若先于 patch 成功而进程崩溃，旧 fact 节点会多活到下次重建——行为无害
   （不播种、不出现由 retired_nodes 决定，重建时物理清除），但没有测试覆盖这个崩溃窗口。
2. **cooccurrence 只在重建时 flush**：查询高频而重建稀疏时，共现学习延迟累积；cap 4000 是
   粗暴截断，超限直接丢。真实库该表为空，机制尚未被真实流量验证。
3. **ensure_fresh_for 是"每次 stale 都全量重建"**：seed miss 即触发，连续 stale 查询会重复重建；
   理论上可只补缺失节点（增量 from_store），当前是正确性优先。
4. **无写放大监控**：graph_writes 计数在进程内，没有暴露成指标/日志，无法事后观测重建频率
   与补丁的覆盖比例。

