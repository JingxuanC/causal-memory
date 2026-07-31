# 检索与写入路径代码审查（2026-08-01）

> 审查范围：`benches/locomo/main.rs`、`benches/longmemeval/main.rs`、
> `crates/causal-memory/src/store/{write,retrieve}.rs`、consolidate/patterns/hippocampus。
> 本文含两部分：① 边/节点规模的成因分析；② 六个优化点及其**风险分析**（含枪毙项）。
> 凡涉及检索语义改动，一律走 `--retrieval-version` 开关 + 全量双口径重跑 + 预注册判读标准。

---

## 一、为什么这么多边和节点

以 LME distill 库实测（`longmemeval_distill.db`）：**275,676 chunks / 247,880 edges**。

| 构成 | 数量 | 性质 |
|---|---|---|
| temporal 邻接边 | 222,872（90%） | 每 turn 一条，链到上一个对方发言，confidence 0.4，`relation='caused'` |
| distill 自环边 | 25,008（10%） | `from_id = to_id`，为让条目在边空间可见 |
| chunks | ~222k raw turns + 25k distill + negation 条目 | 每 turn 一节点（ground-truth 层） |

**根因：edge-centric 读模型。** `retrieve.rs` 全部 5 条检索 SQL 都是
`causal_edges JOIN chunks`——turn 只有挂在边上才能被检索到。22 万条邻接边
本质是**索引项而非因果知识**；distill 自环边同理（`write.rs:150` 注释自述：
"the edge exists so the item is visible to the edge-based read paths"）。
边多不是 bug，是架构选择的必然产物。**注意：邻接边不能删——LME 的
hippocampus spreading（P7+，+25.6pp）走的图就是它们。**

LoCoMo 单对话库规模健康（conv0：464 chunks / 445 edges），膨胀只在 LME 单库设计
（500 题共库，靠 `task_tag` 硬隔离；索引 `idx_causal_task` 已在，无 SQL 层面问题）。

---

## 二、优化点与风险分析

### ① confidence 加权检索 —— 高危，仅限静态 tie-break 形式

**问题**：`search_causal_bm25` 候选池里 0.4 邻接边与 0.7 distill 边平等竞争
top-k，排序纯 BM25 分，不看 confidence/recency。

**陷阱一**：confidence 是写入时拍脑袋常量（0.4/0.6/0.7/0.8），非校准概率。
乘性加权会让 raw turn（0.4）永无出头之日，但 raw 层是 ground-truth 锚——
distill 是 LLM 有损压缩，检索向蒸馏层倾斜 = 蒸馏幻觉被放大且不可追溯，
动摇 "ground-truth preserving" 叙事根基。

**陷阱二（已查证零件存在）**：`record_access` 每次检索 `access_count++`，
`consolidate/stages.rs:182` 有 `new_conf = (new_conf + access_boost).min(cap)`。
bench QA 循环目前不跑 consolidate，安全；但"confidence 加权 + 周期性
consolidate"组合 = 富者愈富反馈回路，**跑题过程改变图状态、题目顺序影响
结果**，controlled delta 失效。

**安全形式**：只用写入时静态 confidence 做同分 tie-break（distill 优先），
禁乘性加权，禁使用 consolidate 动态值。

### ② 语义 + BM25 融合 —— 方向正确，列为 retrieval v2 里程碑

**现状**：`edge_embeddings` 表 0 行，`embed.rs` / `search_causal_semantic` /
`invalidate_semantic_contradictions` 在 bench 全为死代码。语义+BM25+实体
融合是 mem0 91.6% 的核心手段之一，是检索端最明确的差距。

**三个结构性代价**：
1. 叙事代价：OpenAI embedding = 破坏"纯本地"叙事；本地小模型 = candle/onnx
   重依赖 + 22 万 turns embed 时间。
2. 过拟合新通道：RRF k / 融合权重 α 是新超参。**规矩：conv0 当 dev set 调参，
   冻结后全量只跑一次报数。**
3. 语义稀释：专名/日期精确匹配是 BM25 优势，RRF 可缓解但非免费。

### ③ MMR 内容去重 —— 先做零成本离线分析，再决定生死

**问题**：raw turn 与 distill 条目内容重叠，top-k 槽位被同内容两种形态吃掉
（现仅按 chunk id 去重）。

**误杀风险**：多证据/时序消歧题需要"相似但不同"的证据（2022 慈善赛 vs
2023 慈善赛），MMR 一刀切会回吐 temporal 刚涨的 +11.2pp；raw 原文与 distill
归纳对答题各有用途（V2 Step 5 要日期、judge same-referent 要措辞），不纯冗余。

**安全路径**：先用已落盘 V2 结果离线统计 top-10 真·冗余占比（零 API 成本）；
占比低直接放弃；占比高只做同 layer 内去重，不跨 layer。

### ④ FTS5 / 持久化 BM25 —— 不做（写入 limitation）

bench 候选集仅几百条，无性能收益；`search_causal_bm25` 目前每查询全量重扫 +
重建索引，真实 agent 场景（单库终身增长）是 O(N)/查询，但那是产品化问题。
FTS5 内建 BM25 参数与 `bm25.rs`（k1=1.2, b=0.75, Robertson IDF）不同，
换实现 = 基线全失效；若做，FTS5 只当候选粗筛（top-1000），手写 BM25 精排。

### ⑤ 邻接边改名 followed_by —— 暂缓，先验证稀疏图覆盖率

**动机**：90% 的边标 `caused` 实为时序邻接，稀释 `trace_cause_cross_session`
信噪比，也是"因果记忆"叙事的把柄。

**牵连面**：schema CHECK constraint 需重建表（migrate.rs 已 782 行）；
spreading 若硬编码 'caused' 会被杀死（P7+ 的 +25.6pp）；纯因果图极度稀疏
（conv0 仅 45 条 distill 边），改名后跨 session 追踪可能断链退化。
**前置条件**：离线验证"仅 distill 边图"的跨 session 链路覆盖率；
全 grep relation 使用点（trace_cause / chain_linker / consolidate / patterns）。

### ⑥ record_decision 写入查重 —— ❌ 枪毙

mem0 v3（2026-04）用真金白银验证过：从 UPDATE/DELETE 退回 single-pass
ADD-only——写入侧少做聪明事是业界共识。查重失败方向不对称：误杀是记忆系统
致命伤，多存是廉价冗余。仅保留完全相同文本的幂等重放防护，相似度查重不做。

---

## 三、元风险与执行纪律

1. **基线失效级联**：任何检索语义改动使 74.2%/84.1% 成为不可比历史数字。
   存活的改动打包成一次 "retrieval v2"，`--retrieval-version` 开关（仿
   `--prompt-version`），全量双口径只重跑一次。
2. **并行冲突**：F5-F8 收尾数字冻结前，检索侧一行不动。
3. **预注册判读**：每次改动事前写死"涨多少算成、跌多少回退"，不许事后解释。
   当前叙事 "84.1% vs mem0 91.6%，同口径差 7.5pp（归因模型+预算）" 是干净
   基线，不许变成"优化后退步"的解释困境。
4. **测试集过拟合**：所有检索调参在 conv0（dev）上进行，参数冻结后全量
   单次报数。

## 四、风险调整后排序

| 优先级 | 项 | 形式 |
|---|---|---|
| 1 | ③ 的离线冗余分析 | 零成本，数据决定 ③ 生死 |
| 2 | ① 静态 tie-break 版 | 禁加权、禁动态 confidence |
| 3 | ② 语义融合 | retrieval v2 里程碑，dev/test 分离 |
| 4 | ⑤ 改名 | 前置稀疏图覆盖率验证 |
| — | ④ / ⑥ | 不做（limitation / mem0 已证伪） |
