# One Graph 中期收敛计划

> 状态：设计稿（2026-08-20）。目标文档：docs/design/complete-memory-system.md（One Graph, One Engine, One Loop）；
> 现状基线：architecture-hardening-2026-08.md §C7（短期惰性重建）已落地。本文把中期收敛拆成可独立验证的
> 阶段，每阶段有回归护栏。

## 0. 为什么现在做

- 设计承诺的 **one graph**：所有记忆类型 = typed edge（caused/enabled/prevented/no_effect/fact/co_occurrence/meta），
  一个 typed 激活扩散引擎检索全部，一个不可变巩固循环演化整张图。
- 当前差距：`agent_facts` 是外挂（BM25/向量独立检索，**不进图**）；`search_memory` 靠 RRF 硬拼两个池；
  图只覆盖 causal/meta/cooc，且是 5 写/30s 的懒重建缓存。
- CodeGraph 拆解启示：专职图工具也是 SQLite + 应用层遍历；我们真正该抄的是**增量同步**（哈希比对补索引），
  不是换存储。

## 1. 目标形态（收敛后的运行时）

```
SQLite（真相源, 压缩免疫 P5）               内存 CausalGraph（查询表面, 全类型）
  chunks ──────────────────────────────►   节点 = 所有记忆痕迹（含 fact:{id}）
  causal_edges ─────────┐                  边   = 七类 typed edge（fact 参与传播）
  meta_causal_edges ────┤                  引擎 = 一个加权激活扩散（播种→传播→结果）
  cooccurrence_edges ───┼──► from_store ──► 生命周期 = 写路径增量修补 + 懒全量兜底
  agent_facts ──────────┘                  sleep = 一个巩固循环覆盖全类型
```

原则（不妥协）：SQLite 仍为唯一真相（图坏了随时重放重建）；图覆盖全部边类型（fact 一等公民）；
时间仍是元数据；巩固不可变（delta + clone）。

## 2. 分阶段路径

### Phase A — fact 入图（逻辑一等公民）

目标：`agent_facts` 成为图上 fact 类型的节点/边，参与激活扩散。

1. **图加载扩展**：`CausalGraph::from_store` 增加 `agent_facts` 装载——每行 fact 建一个节点
   （id=`fact:{row_id}`，text=`{key}: {value}`，task_tag 取 scope）。
2. **实体链接（确定性，无 LLM）**：fact 节点与 chunks 节点按 token 重叠建 `fact` 边——
   复用 `patterns::tokenize` + 现有共享前缀/交集逻辑，阈值控制假阳性；同一 fact 的 key 与 value 之间
   也建一条 `fact` 自链（subject → value 语义骨架）。
3. **传播系数**：`Relation::Fact = +0.8`（已存在），fact 节点进扩散后，查询经因果种子能关联到相关事实，
   反之事实种子能扩散到相关教训。
4. **写路径**：`record_fact` 后标记图脏（mark_graph_dirty）——fact 变更也触发懒重建。

验证：
- 单测：插入 fact「user prefers TypeScript」+ 因果边「用 TS 重写模块 →caused→ 编译下降」，
  查询「TypeScript」的扩散结果必须同时含事实节点与因果链节点；`search_facts` 回归不降。
- 护栏：全量 cargo test（当前 344）+ LongMemEval/AMC 不回归（fact 检索路径不变）。

### Phase B — 统一检索引擎（one engine）

目标：检索 = 播种 → 一次全类型扩散 → 结果；BM25/语义退化为播种层。

1. **播种层扩展**：`find_seeds` 把 bm25_index（已含 `fact:{id}` 命名空间）+ 语义向量对 ALL 节点类型
   统一取种子（现在只对因果侧）。
2. **search_memory 重构**：事实与因果不再双池 RRF，改为同一扩散的带类型标注结果；RRF 降级为
   fallback（回归对照用）。
3. **分层展示保留**：输出仍按 fact/causal 分组标注（前端协议不变），只是检索源统一。

验证：
- `search_memory` 对混合查询（事实+教训）结果不劣于现状（同一 query 对比新旧输出，手工抽 20 条）。
- 全量 500 题 LongMemEval 回归（多会话 57.9% 不降）。

### Phase C — 图生命周期升级（增量，CodeGraph 启示）

目标：从「5 写/30s 全量重建」升级为「写路径增量修补 + 懒全量兜底」，新教训即时出现在图侧。

1. **写路径修补**：`record_decision / record_fact / invalidate / supersede` 时，除 mark_dirty 外，
   对 CSR 做**局部补丁**：新增节点/边插入邻接数组（边 append-only 即可，CSR 追加友好），
   失效/取代只翻转 `edge_valid`/`superseded_by`（O(1)，已有位掩码）。
2. **完整性兜底**：计数器或时间阈值仍触发全量 from_store 重建（防漂移）；启动时全量建一次。
3. **Hebbian/cooc 缓冲**照旧在重建时冲刷（现有 D1 机制不变）。

验证：
- 新写入的边在**同一次查询**（无重建）即出现在扩散结果里（当前要等 30s/5 写）。
- probe 测试：连续写 + 查，图节点数单调一致；全量重建后与补丁状态一致（差分断言）。

### Phase D — 一个巩固循环覆盖全类型

目标：sleep 对 fact 边同样生效——降权（半衰期）、GC（valid_to）、取代（新 fact 顶旧 fact）。

1. **fact 半衰期**：consolidate stage 3 的 half-life 对 `fact` 边启用（discovered_by=fact 走
   user_feedback 档或独立档，待定）。
2. **fact 取代**：`record_fact` 同 key 新值 → 旧 fact 边软标注 superseded_by（对齐 C7 软取代语义，
   不隐藏，检索时标注版本）。
3. **REM/meta 挖掘**：meta 边挖掘的输入扩展为含 fact 边（跨任务模式可引用事实）。

验证：
- CausalEval 全量回归；新增 fact 衰减单测；sleep dry-run 报告含 fact 统计行。

## 3. 顺序与依赖

```
Phase A（fact 入图）→ Phase B（统一引擎）→ Phase C（增量生命周期）→ Phase D（全类型巩固）
   A 是地基：fact 不在图上，B/C/D 无从谈起
   C 可在 B 之后做（引擎稳定后再优化新鲜度）
   D 依赖 A（fact 在图上才有降权/取代的对象）
```

## 4. 风险与护栏

| 风险 | 护栏 |
|---|---|
| fact 实体链接假阳性污染扩散 | 阈值保守 + 只建高置信边；Phase A 可先只连 token 交集 ≥2 的 |
| 图变大影响懒重建延迟 | 全量重建本来就是 O(V+E)；增量修补（Phase C）进一步摊薄 |
| search_memory 行为漂移 | Phase B 保留 RRF fallback + 20 条人工对比 + LongMemEval 回归 |
| 事实取代语义被误伤 | 软取代（superseded_by 标注）不隐藏，检索可见 + 版本标注 |
| 增量修补与全量重建状态漂移 | 差分断言测试（补丁后状态 == 全量重建后状态） |

## 5. 阶段验收口径

- Phase A 完成后：`search_causal` 能经扩散关联到事实节点；`search_facts` 不变；测试 344+。
- Phase B 完成后：`search_memory` 单一扩散引擎出结果；LongMemEval 500 不降。
- Phase C 完成后：新写即查可见；差分断言过。
- Phase D 完成后：sleep 报告含 fact 统计；CausalEval 不降。

每阶段独立提交、独立验证，不阻塞其他工作。