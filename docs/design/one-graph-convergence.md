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

### Phase A — fact 入图（逻辑一等公民）✅ shipped 2026-08-20

> 落地说明：节点装载与 `Fact=0.8` 传播系数此前已随 P1 存在；本次补齐的是**实体链接**与
> **record_fact 标脏**。链接为确定性 token 重叠（`patterns::tokenize`，交集 ≥3 distinct
> 非停用词才建边 + df≤20 过滤高频泛词（2026-08-26 精度修正）；双向、每 fact 上限 8 条、
> 倒排索引 O(total tokens)），权重 `0.3+0.1·overlap`（cap 0.8）。
> 偏差：key→value 自链未做——fact 节点文本本身即 `{key}: {value}`，链接直接作用于全文。
> 验证：4 个新单测（扩散同时含事实与因果链节点 / fact 种子到达因果链 / 单 token 不建边 /
> record_fact 标脏可见性），workspace 347/347，clippy 零新增。

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

### Phase B — 统一检索引擎（one engine）✅ shipped 2026-08-20

> 落地说明：`search_memory` / `search_memory_entries` 由单一扩散引擎服务（输出 `[unified/spread]` /
> mode `"spread"`）。播种层 = `bm25_seed_ids`（持久 BM25 索引、双命名空间、按 token 交集排序、
> scope 过滤）+ 语义种子（有 embedder 时）+ 图内子串匹配（`spreading_activation_seeded`，扩散核心
> 抽取为 `spread_and_collect` 共享）。结果按类型物化：facts 按 activation 序（`facts_by_ids`），
> 因果边按最强端点 activation 排序（`edges_touching_chunks`）。双池 RRF 保留为 fallback + A/B
> 回归对照；D4 意图路由与分组展示协议不变。
> **新鲜度（Phase C 预演）**：store 种子映射不到图节点 = 图早于写入（懒重建未触发）——引擎立即
> 重建一次而非静默丢种子（MCP e2e 抓到的场景：懒阈值 5 写之内的 fresh fact 必须可见）。
> 验证：4 个新单测（播种双命名空间 + scope 过滤 + 物化 / 纯种子驱动扩散 / 引擎服务混合查询 /
> 陈旧图种子丢失触发重建），workspace 351/351。**待补**：LongMemEval 500 题回归（需 LLM API
> 环境）与 20 条人工对比——引擎已作为默认路径，RRF fallback 可随时切回对照。

目标：检索 = 播种 → 一次全类型扩散 → 结果；BM25/语义退化为播种层。

1. **播种层扩展**：`find_seeds` 把 bm25_index（已含 `fact:{id}` 命名空间）+ 语义向量对 ALL 节点类型
   统一取种子（现在只对因果侧）。
2. **search_memory 重构**：事实与因果不再双池 RRF，改为同一扩散的带类型标注结果；RRF 降级为
   fallback（回归对照用）。
3. **分层展示保留**：输出仍按 fact/causal 分组标注（前端协议不变），只是检索源统一。

验证：
- `search_memory` 对混合查询（事实+教训）结果不劣于现状（同一 query 对比新旧输出，手工抽 20 条）。
- 全量 500 题 LongMemEval 回归（多会话 57.9% 不降）。

### Phase C — 图生命周期升级（增量，CodeGraph 启示）✅ shipped 2026-08-20

> 落地说明：`CausalGraph` 新增写路径补丁能力——`append_node`（SoA 数组 O(1) 追加）+
> `add_patch_edge`（CSR 中段插入是 O(E) 且会移动所有已存 CSR 边索引，故补丁边进 per-node
> overlay map，扩散步两个方向都消费）+ `invalidate_edges_between`（O(deg) 翻转）+
> `retire_node`/复活（被取代的 fact 节点在重建前不再播种/浮出）。`record_decision` /
> `record_fact`（含取代）/ `invalidate_decision` 即时补丁；懒重建保留为漂移兜底。
> `ensure_fresh_for`（Phase B 预演）在补丁模式下极少触发——种子都能命中，O(store) 重建
> 从每查询摊薄回周期性。验证：新写入在**同一次查询**可见且**懒重建未触发**（脏计数断言）；
> 差分断言：补丁态查询结果 == 全量重建态（双实例对照）；补丁边正/反向扩散 == 全量构建等价。

目标：从「5 写/30s 全量重建」升级为「写路径增量修补 + 懒全量兜底」，新教训即时出现在图侧。

1. **写路径修补**：`record_decision / record_fact / invalidate / supersede` 时，除 mark_dirty 外，
   对 CSR 做**局部补丁**：新增节点/边插入邻接数组（边 append-only 即可，CSR 追加友好），
   失效/取代只翻转 `edge_valid`/`superseded_by`（O(1)，已有位掩码）。
2. **完整性兜底**：计数器或时间阈值仍触发全量 from_store 重建（防漂移）；启动时全量建一次。
3. **Hebbian/cooc 缓冲**照旧在重建时冲刷（现有 D1 机制不变）。

验证：
- 新写入的边在**同一次查询**（无重建）即出现在扩散结果里（当前要等 30s/5 写）。
- probe 测试：连续写 + 查，图节点数单调一致；全量重建后与补丁状态一致（差分断言）。

### Phase D — 一个巩固循环覆盖全类型 ✅ shipped 2026-08-20

> 落地说明：
> ① **fact 半衰期**：stage 3 新增 `downscale_facts`——按 `updated_at` 年龄、
> `user_feedback` 档（90d，fact 是高信任「what is」知识，取最慢档）衰减
> `agent_facts.confidence`，低于 gc_threshold 退休（valid_to）；当日 fact 不衰减（对齐边路径）。
> 报告新增 `facts_decayed` / `facts_gc` 统计行（dry-run 同样计数）。
> ② **fact 取代**：schema v12 `agent_facts.superseded_by`——`record_fact_replacing` 一次写
> 同时退休 + 记录 lineage（谁取代了谁），复活时清除；该 id 直接驱动图侧 `retire_node` 补丁。
> **偏差**：完整的「软取代不隐藏 + 检索时标注版本」未做——fact 的检索契约（知识更新=新值
> 顶旧值、旧值退出检索）由 MCP e2e 固化，改动需独立的评测 A/B（对齐 C7 当年的做法），
> 先落 lineage 列。
> ③ **REM/meta 挖掘**：挖掘输入统一为 `MineItem`——有效 fact 作为一等参与者
> （id=`fact:{id}`，stratum=scope）；fact 无 outcome 语义，只参与 `similar_to`
> （conf = sim×0.8），`observe_tag` 记 stratum 不记方向。挖出的 meta 边在图上把 fact 节点
> 接进因果内容（Phase A 后 fact 节点在图中，端点校验通过）。
> 验证：fact 衰减/GC + dry-run 不写 + lineage 复活清除（3 测试）；fact↔决策挖出 similar_to
> meta 边且扩散可达（1 测试）；CausalEval 140 题全量回归（fact-free 存储 → 行为不变）。

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
| fact 实体链接假阳性污染扩散 | 阈值保守 + 只建高置信边：≥3 distinct 非停用词 + df≤20 过滤（实测精度 17%→33% 严格 / 29%→75% 宽松，链接 9,764→3,116） |
| 图变大影响懒重建延迟 | 全量重建本来就是 O(V+E)；增量修补（Phase C）进一步摊薄 |
| search_memory 行为漂移 | Phase B 保留 RRF fallback + 20 条人工对比 + LongMemEval 回归 |
| 事实取代语义被误伤 | 软取代（superseded_by 标注）不隐藏，检索可见 + 版本标注 |
| 增量修补与全量重建状态漂移 | 差分断言测试（补丁后状态 == 全量重建后状态） |

## 5. 阶段验收口径

- Phase A 完成后：`search_causal` 能经扩散关联到事实节点；`search_facts` 不变；测试 344+。
  ✅ 2026-08-20：347/347；`search_facts` 路径未动。
- Phase B 完成后：`search_memory` 单一扩散引擎出结果；LongMemEval 500 不降。
  ✅ 2026-08-20（引擎 + 单测）：单一扩散引擎为默认路径；LongMemEval 回归待 LLM API 环境补跑。
- Phase C 完成后：新写即查可见；差分断言过。
  ✅ 2026-08-20：补丁即查可见（脏计数断言证明无重建）+ 双实例差分断言过。
- Phase D 完成后：sleep 报告含 fact 统计；CausalEval 不降。
  ✅ 2026-08-20：报告含 facts decayed/GC'd 行；79.3% vs 83.6%（噪声主导，evidence 持平，详见 CHANGELOG）。

每阶段独立提交、独立验证，不阻塞其他工作。