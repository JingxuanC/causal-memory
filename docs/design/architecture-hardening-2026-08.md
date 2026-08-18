# 架构与 MCP 工具链路加固设计（2026-08）

> 审计日期：2026-08-18
> 范围：MCP 工具链路（server/tools.rs + server/mod.rs + output.rs）、存储层（store/）、
> 图引擎（hippocampus/）、离线子系统（consolidate/patterns/refute/chain_linker）、
> CLI 接线（commands/）、以及文档一致性。
> 依据：14 个 MCP 工具逐一代码走读 + 实库（~/.local/share/causal-memory/causal.db, schema v9）状态核对。
> 结论：共 22 项问题——MCP 工具层 14 项、架构层 8 项；其中 4 项与 roadmap 声明存在偏差
> （2 项引擎已实现但未接线/未生成数据、1 项完全缺失、1 项已实现但作用点偏离）。

---

## 第一部分：MCP 工具链路问题与优化设计

### 1.1 工具链路现状速览

14 个工具按链路特征分三类：

| 类型 | 工具 | 链路特征 |
|---|---|---|
| 纯 SQL | trace_cause_chain / search_patterns / causal_directory / invalidate_decision / trace_cause | 无网络，微秒~毫秒级，最健康 |
| 写 + 同步网络 | record_decision / record_fact / remember | 每次 1~3 次同步 HTTP + 全图重建 |
| 读 + 同步网络 | search_causal / search_facts / search_memory / intervention_query / counterfactual_query / reconstruct_lesson | embed HTTP + O(N) 全表扫描 |

典型重链路（record_decision，配置齐全时最坏约 16s）：

```
MCP → LLM judge polarity(block_on, ≤8s) → SQL 写(锁) → init_embedder(新 Client)
    → embed HTTP(≤8s) → 反查 edge id(SQL) → put_embedding → 语义矛盾全表扫描
    → reload_graph(全库重载重建图)
```

### 1.2 问题清单（MCP 层）

#### C1 每次工具调用新建 HTTP client（P1）
- 证据：tools.rs 多处调用 embed::init_embedder()，每次 Embedder::new 新建 reqwest::Client（embed.rs:241-266）。
- 影响：连接池/keep-alive 全丢，TLS 握手每请求重来；所有走语义路径的工具都受影响。
- 方案：CausalMemoryServer 挂共享 embedder（OnceLock<Arc<Mutex<UnifiedEmbedder>>>），惰性初始化，进程级复用。

#### C2 同步网络调用阻塞工具线程（P1）
- 证据：server/mod.rs:110 block_on 包住全部 embed / LLM judge / distill / reconstruct 调用；
  output.rs:7 judge_outcome_polarity 每次 record_decision 都同步调 LLM（8s 超时，llm.rs:13-19）。
- 影响：单次工具调用可挂 16s+；MCP 并发能力被同步等待锁死。
- 方案：
  1. record_decision 的 polarity：先写 NULL 立即返回，后台 job 补齐（复用 edges_without_polarity + run_polarity）。
  2. remember 的 distill：spawn 后台任务，立即返回提取中；完成回填。
  3. counterfactual_query 两侧 embed 用 tokio::join! 并行（当前串行 2×RTT）。
  4. 网络超时降级已有（8s fail-fast），保留。

#### C3 record_decision 冗余 SQL（P1）
- 证据：tools.rs:274-283 写完后 SELECT id FROM causal_edges WHERE from_id=? ORDER BY id DESC LIMIT 1 反查 edge id。
- 方案：record_decision_full 直接返回 (dec_id, edge_id)，去掉反查。

#### C4 intervention_query N+1（P1）
- 证据：tools.rs:1262 chain_stratum 对每条链单独 get_edge（每链一次锁+查询）。
- 方案：批量 SELECT ... WHERE id IN (...) 一次取全部链锚点。

#### C5 markov_blanket N+1（P1）
- 证据：retrieve/mod.rs:77-86 每个 seed 单独 query_row。
- 方案：一条 IN 查询取全部 seed 边。

#### C6 remember 逐 item 独立事务（P2）
- 证据：tools.rs:392-456 每个 item 单独 record_decision_at / record_fact，各自锁+事务。
- 方案：包单事务批量写；失败 item 不整体回滚，逐 item 计失败数。

#### C7 每次写后全图重建（P1）
- 证据：record_decision（tools.rs:314）、remember（tools.rs:459）都调 reload_graph = CausalGraph::from_store 全量重载
  （hippocampus/mod.rs:1063-1223：全表 chunks+edges+facts）。
- 影响：O(全库) 每写一次；几千边后是写路径主导成本。
- 方案：
  1. 惰性重建：写后置 dirty 标记，下次 hippocampus 查询前若 dirty 且超过阈值（N 次写 / T 秒）才重建；
  2. 或增量 add_edge：from_store 后单边 O(1) 插入（需要 CSR 可增量，评估后二选一，倾向惰性重建先行）。

#### C8 检索工具重复 embed / 重复全表扫描（P2）
- 证据：search_causal 与 search_facts 各自 init_embedder + 各自 O(N) 扫描；search_memory 一次 embed 但 2 次 O(N) 扫描。
- 方案：随 C1（共享 embedder）+ R1（向量索引）+ R4（embedding 缓存）自然解决。

#### C9 embedding 覆盖率极低（P0，直接影响质量）
- 证据：实库 223 条边仅 12 条 edge_embeddings；1308 条 facts 0 条 agent_facts_embeddings。
- 影响：语义路径空转（白付一次 embed HTTP 后回落 BM25）；部分覆盖导致语义结果偏向已嵌入子集。
- 方案：
  1. 立即执行 causal-memory embed 全量回填（CLI 已有 run_embed + edges_without_embedding，加 --all）。
  2. 写入路径 embedding 改后台批量补齐，不再每次同步 HTTP。
  3. 记录 vector model，防止换模型后余弦跨模型失真（加 model 一致性校验）。

#### C10 检索结果格式与工具语义问题（P3）
- 证据：search_causal 的 keyword 分支（tools.rs:570-601）忽略 detail_level/max_tokens；
  trace_cause（tools.rs:933）SQL 回退是 LIKE 全扫且无 limit 参数。
- 方案：keyword 分支统一走 layered formatting；trace_cause 回退路径加 limit + 排序。

#### C11 工具注释/文档滞后（P3）
- 证据：tools.rs:1 注释 The 13 MCP tools，实际 14 个；部分工具 description 与实现参数不完全一致。
- 方案：头注释改为 14，工具 description 与实现对齐。

---

## 第二部分：架构问题与优化设计

### 2.1 问题清单（架构层）

#### A1 SQLite 未启用 WAL，busy_timeout=0，synchronous=FULL（P0）
- 证据：实库 PRAGMA journal_mode 返回 delete；全库 grep 无 journal_mode/WAL 设置（migrate.rs 只写 user_version）。
- 影响：读写互相阻塞；HTTP 多连接下 database is locked 风险；每次提交同步 fsync。
- 方案：CausalStore::open 执行 PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;
  测试断言 migration_e2e 后 journal_mode=wal。

#### A2 单连接全局 Mutex 串行化所有 SQL（P0）
- 证据：store/mod.rs:158 pub(crate) conn: Arc<Mutex<Connection>>，读写共用一把锁。
- 影响：并发检索/写完全串行；HTTP 多 agent 模式第一个撞上。
- 方案：r2d2 连接池：读连接池（多）+ 单写连接；with_conn 语义保持；并发压力测试（32 线程混合读写）验证。

#### A3 HTTP 模式每连接重开 DB + migrate + 重建图（P0）
- 证据：misc.rs:196-203 StreamableHttpService::new(move || CausalStore::open(...))，每个请求新实例。
- 方案：进程级 Arc<CausalStore> 单例 + 共享图；每连接只建轻量 server handle。

#### A4 读路径写：每次检索 UPDATE access_count（P0）
- 证据：retrieve/mod.rs:376 record_access，semantic/bm25/trace 所有读路径调用；
  每次搜索 = 读 + 写两个事务。
- 方案：内存计数节流批量落库；consolidate 依赖 last_accessed_at，设计为写路径自然 flush（record 前 flush），
  保证滞后 ≤ 一次写；默认开启但可 --no-access-tracking 关闭。

#### A5 语义检索 O(N) 全表扫描（P1）
- 证据：retrieve/semantic.rs:17-48 全表 JOIN edge_embeddings 拉到 Rust 逐条余弦；
  bm25.rs:83 每次查询全表拉取 + 重建 Bm25Index。
- 方案：
  1. BM25 → SQLite FTS5 虚拟表（chunks_fts，content=chunks），查询 bm25() 打分，k1/b 与现状一致，top-k 一致性用例验证；
  2. 语义 → sqlite-vec（SQLite 原生向量索引，零服务）或 FAISS/HNSW；无扩展时回退现状（并发可接受）。

#### A6 查询无缓存（P2）
- 证据：无任何 query→vector / query→result 缓存。
- 方案：embedding LRU（HashMap+VecDeque，容量 256）；后续可加结果缓存（FTS/向量命中可短 TTL 缓存）。

#### A7 内存图与 SQL 双轨（P2，设计层面）
- 证据：检索先在内存图（hippocampus）走一遍再回 SQL；reload_graph 每次全量重建（C7）。
- 方案：短期惰性重建 + 版本号失效（C7）；中期向 roadmap one graph 方向收敛（图为主存储）或图只读缓存。

#### A8 文档滞后于代码（P3）
- 证据：docs/design/architecture.md 写 13 tools / schema v7，实际 14 tools / v9；
  README 徽章 tests-231，实际 322。
- 方案：统一更新 architecture.md、README、tools.rs 注释；增加 CHANGELOG 条目。

### 2.2 功能层：roadmap 声明 vs 代码现状（本轮重点核实结论）

| Roadmap 声明（未完成） | 代码实际状态 | 证据 | 结论 |
|---|---|---|---|
| Hebbian 共现边 | 更新逻辑完整，但边从未被创建 | mod.rs:772 hebbian_update 完整（HeLa-Mem 公式 λ=0.995/η=0.02），
  每次检索后触发（mod.rs:426-429）；但 from_store（mod.rs:1063-1223）不生成 CoOccurrence 边；
  schema v9 无表可存；测试靠 make_co_graph() 手工造边（tests.rs:652） | 死代码/半成品，缺边生成+持久化 |
| SWR 2.0 / Dreams 对齐 | immutable 引擎已实现，只有测试调用 | mod.rs:640 swr_consolidate_immutable（clone 图 + delta 审计 + instructions +
  triple-criterion GC）；生产 consolidate()（consolidate/mod.rs:53-137）仍原地改 store，文档自述 NOT idempotent | 引擎已建、未接线 |
| 查询路由分类器 | 全库无此概念 | 仅 patterns/classify.rs（模式挖掘 pair 分类，非查询路由）；search_memory 是无分类 RRF 融合 | 确实未实现 |
| Q-value 动态权重 | 完整闭环已落地，作用点是节点 seeding 而非边权重 | consolidate/mod.rs:88-113 → mod.rs:795 Bellman → persist_q_values(:834)
  → from_store 读回(:166) → seeding 0.5+0.5Q(:367-368)；边 raw_weights 仍是静态 confidence | 已实现，与 roadmap 描述有偏差 |

### 2.3 功能补齐设计

#### D1 Hebbian 共现边（补齐边生成 + 持久化）
- schema v11：新增 cooccurrence_edges(id, from_id, to_id, weight REAL DEFAULT 0.2, updated_at, valid_to)，
  加 (from_id, to_id) 唯一索引。
- 生成器：graph.build_cooccurrence(session_chunk_ids) —— 同一 session 时间窗内共现的节点对，缺失则建 weight=0.2 边；
  已有则交 hebbian_update（w=(1-λ)w+η·I(co-active)）。
- 持久化：persist_cooccurrence(store) 写回 weight；from_store 读回为 Relation::CoOccurrence 边（spread_coeff 已定义）。
- 触发：spreading_activation 检索后（run_hebbian=true）节流批量持久化（如每 10 次检索或写路径 flush）。
- 测试：端到端（检索→生成边→权重上升→进程重启后仍在）+ 单元测试（权重公式）。
- 风险：共现边数量失控 → 全图边数暴涨；需要阈值（最低共现次数/激活强度）与容量上限（如不超过总边数 20%）。

#### D2 SWR 2.0：immutable 合并产出新 store
- 引擎已就绪（swr_consolidate_immutable），只缺接线：
  1. causal-memory sleep 新增 --immutable 模式：跑 immutable 合并 → 将 delta（LTP/LTD/GC + Q persist）
     应用到新 DB 文件 causal.db.consolidated-<ts>（先复制骨架，再应用 delta，不删行只改 weight/valid_to/q_value）；
  2. 报告 new_store_path + delta_log 摘要 + forgotten/ltp/ltd 计数；
  3. --restore <path>：用合并 store 替换当前 store（原文件先备份 causal.db.bak.<ts>）；
  4. 默认保留原地模式（--legacy），验收通过后再切换默认。
- 说明：merge_redundant_edges / rem_integrate 暂留原地模式（immutable 先不做合并，避免跨文件引用复杂度）。
- 验收：sleep --immutable 后原文件哈希不变；--restore 后可查询且可回滚。

#### D3 Q-value：修正作用点（可选）
- 现状：Q 影响节点 seeding（0.5+0.5Q），边权重仍为静态 confidence。
- 方案 A（roadmap 原意）：effective_weight = confidence × (0.5 + 0.5 × Q_outcome_node)，from_store 建边时计算；
  需先跑 CausalEval A/B 验证不下降。
- 方案 B（推荐先行）：保持节点侧 seeding，更新 roadmap 与文档表述为节点效用加权。

#### D4 查询路由分类器（新功能，可选）
- query_router.rs：规则分类器（无 LLM）：时间表达式/疑问词/实体密度/长度 → fact|causal|chain|directory|unified。
- search_memory 内嵌路由：高置信分类走单层检索，低置信走 RRF 融合（现状兜底）。
- 验收：50 条人工 query 分类准确率 ≥85%（对照：全 fusion）。
- 后续：iterative retrieval（entity/time anchors）作为 Phase 6 候选。

---

## 第三部分：落地计划

### 3.1 阶段划分

| 阶段 | 内容 | 涉及问题 | 工作量 |
|---|---|---|---|
| Phase 0 | 文档同步 | A8 C11 | 0.5d |
| Phase 1 | 存储基建：WAL/连接池/HTTP 单例/读路径写 | A1 A2 A3 A4 | 2d |
| Phase 2 | 检索基建：FTS5/向量索引/embedding 回填/缓存 | A5 A6 C9 | 3d |
| Phase 3 | 服务器层：共享 embedder/去冗余/异步化 | C1-C8 | 3d |
| Phase 4 | 图引擎接线：Hebbian 生成/SWR 2.0 store/Q 修正 | D1 D2 D3 | 4~5d |
| Phase 5 | 查询路由（可选） | D4 | 2d |

总计约 14.5~15.5 人日。建议推进顺序 Phase 0→1→2→3（前四阶段兑现主要收益），
Phase 4 单独排期（涉及 schema v11 与 sleep 语义变更，需先过 CausalEval 回归），Phase 5 视需要。

### 3.2 验收标准

1. cargo test 全量通过（现有 322 + 新增 ≥40）。
2. 并发基准：32 线程混合读写，Phase 1 后吞吐提升 ≥3x，无 database is locked。
3. 检索基准：CausalEval/bench_tokens 回归不下降；FTS5 top-k 与旧 BM25 一致率 ≥95%。
4. Hebbian：检索 3 次共现后权重上升且重启后仍在（端到端测试）。
5. SWR 2.0：sleep --immutable 产出新文件且原文件哈希不变；--restore 可回滚。
6. 文档与代码一致：无 13 tools、schema v7 残留。

### 3.3 关键证据索引

| 问题 | 证据位置 |
|---|---|
| A1/A2 | store/mod.rs:158, 172-179；migrate.rs（无 PRAGMA） |
| A3 | misc.rs:196-203 |
| A4 | retrieve/mod.rs:376-395；semantic.rs:48,109；bm25.rs:98 |
| A5 | retrieve/semantic.rs:17-48；retrieve/bm25.rs:83-89 |
| C1 | embed.rs:241-266；tools.rs:269,496,640,769 |
| C2 | server/mod.rs:110-117；output.rs:7-16；llm.rs:13-19 |
| C3 | tools.rs:274-283 |
| C4 | tools.rs:1262-1267 |
| C5 | retrieve/mod.rs:77-86 |
| C6 | tools.rs:392-456 |
| C7 | tools.rs:314,459；hippocampus/mod.rs:1063-1223 |
| C9 | 实库：223 边/12 embedding；1308 facts/0 embedding |
| D1 | hippocampus/mod.rs:772-789,426-429；types.rs:14,24；tests.rs:652-674；store/mod.rs:67,93 |
| D2 | hippocampus/mod.rs:640-764；consolidate/mod.rs:53-137 |
| D3 | hippocampus/mod.rs:795-845,166,367-368；consolidate/mod.rs:88-113；store/mod.rs:30 |
| D4 | 全库 grep 无 classifier/router/query_type |
| A8 | docs/design/architecture.md:1；tools.rs:1；migrate.rs:34（SCHEMA_VERSION=9） |

---

## 附：实施状态（2026-08-18 更新）

Phase 0-4 已按本设计落地并全部通过 `cargo test`（328 个，含新增），提交见 git log：

| 阶段 | 状态 | 关键实现 |
|---|---|---|
| Phase 0 文档同步 | ✅ d9c2aff | 14 tools / schema v9 表述修正 |
| Phase 1 存储基建 | ✅ 46bfd00 | WAL/busy_timeout/NORMAL；手写连接池（crates.io 不可达，r2d2 无法引入）；HTTP 共享 store；读路径写节流 |
| Phase 2 检索基建 | ✅ 538f872 + 487541d | **B2 调整为自建持久化倒排索引**（FTS5 的 unicode61 对中文是整段 token，破坏 bigram 语义）；B1 评估：无 sqlite-vec 依赖，保持 O(N)+B4 缓存缓解；B3 回填 CLI 增强；B4 LRU 缓存 |
| Phase 3 服务器层 | ✅ 104870f + ca39da2 + 67d981d + 54a4371 | C1 共享 embedder 单例；C3/C4/C5 去冗余 SQL+N+1；C7 图惰性重建；C2 部分（counterfactual 并行 + polarity 开关）；**C6/2c 评估后跳过**（A2 连接池已消除锁瓶颈；stdio 模式后台任务不可靠） |
| Phase 4 图引擎 | ✅ 692b2d2 + d0ef129 + 文档 | D1 Hebbian 共现边（schema v11：cooccurrence_edges 表 + 检索共现缓冲 + 图加载）；D2 SWR 2.0（sleep --immutable 经 VACUUM INTO 产出新 store + --restore 带备份）；D3 Q-value 采用方案 B（保持节点 seeding，roadmap 表述修正；边权重变体留待 CausalEval A/B） |
| Phase 5 查询路由 | ✅ ecd1c7c 后 | D4 规则分类器（query_router.rs，50 条标注 query 准确率 86%，search_memory 展示层路由，检索仍融合兜底） |

**实施中与设计的偏差**（均为环境约束或工程判断）：
1. B2：FTS5 → 自建 bm25_index 倒排表（中文分词语义不变，零新依赖）。
2. A2：r2d2 → 手写连接池（crates.io 403 不可达）。
3. D1：权重公式从 HeLa-Mem 稳态式改为加法强化（稳态 η/λ≈0.02 低于初始 0.2，原式会使共现强化反而衰减）。
4. C6（remember 事务）与 C2 的 remember 后台 distill：跳过（收益已被 A2 覆盖 / stdio 生命周期限制），在 commit 信息与本文档记录。
5. D3：方案 B（文档修正），不做边权重变体（无 CausalEval 环境做 A/B 回归）。