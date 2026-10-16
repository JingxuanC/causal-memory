# 查询链路重构设计（v0.2，经 Claude 两轮评审修订）

> v0.1 初稿 → Claude Code 对照实码评审两轮 → 本版吸收全部采纳意见。
> 被推翻的 v0.1 假设在文末「评审变更记录」留档。

## 问题清单（评审确认）

- P1 每请求全量建图（tenant.rs:372-381 → memory/mod.rs:91），N 工具调用 = N 次 O(N+E)
- P2 D1 Hebbian 共激活学习在 HTTP 模式**现已完全失效**（cooc_buffer 只在 rebuild flush，无 Drop impl；评审独立确认）
- P3 语义腿全表扫 embeddings——但**在多租户模式不痛**（租户库小），真正痛的是大共享库（stdio/harness）与**写路径**的 `invalidate_semantic_contradictions` 每 record_decision 全表扫
- P4 access buffer flush = N 个隐式事务（**不是** N 次 fsync——synchronous=NORMAL 下 WAL 提交不 fsync）
- P5 rebuild 换图竞态：丢边（patch 打旧图）+ 无条件清零 dirty + **无 single-flight 可致图倒退**（更旧图覆盖更新图）
- P6（评审新增）`from_store` 非一致快照：chunks/edges 两条独立 SELECT，可装进"边在端点不在"的撕裂图——目前靠 id_to_idx 缺失静默丢边兜底，是运气不是设计
- P7（评审新增）git-sync 是**独立进程**旁路写入（git.rs:936 align 模式会软失效边），新鲜度兜底只有 unified 的 `ensure_fresh_for`（只查种子缺节点，查不到失效/退休/端点复用）；`hippocampus_search`/`trace_cause` 无任何新鲜度检查——池化后会把已被推翻的教训当有效证据返回

## 实施方案（按合入顺序）

### 第 1 批：P5+P6 正确性套装（memory/ 内部，对外零行为变化）✅ 已合入 0650b78

1. **代际双检（P5 真修法）**：`graph_version: AtomicU64`，换图 fetch_add；`patch_graph_new_edge/Fact` 乐观重放——记 v → 锁内 append → 复查 version，不等则对新图重放（有界循环）。防丢边，并覆盖 F2 的 graph=None 分支
2. **single-flight**：`try_lock` 抢重建权，抢不到用当前图继续服务（宁陈旧不排队）。防 F1 后的重建惊群。**注意：single-flight 是性能机制不是正确性机制，与 1 正交，缺一不可**
3. **dirty 计数 CAS saturating sub**（裸 fetch_sub 会 underflow）；`graph_last_rebuild`/`snapshot_ts` 改取**读事务内时间**（现实现写换图后 now()，当 delta 边界会漏 build 窗口写入）
4. **一致快照（P6）**：`from_store` 包 BEGIN DEFERRED（WAL 下稳定快照）
5. 回归测试：build 窗口注入写（丢边）+ 8 并发请求只重建一次（惊群）

### 第 2 批：F4 access buffer 批量 flush（最小最安全）✅ 已合入 8d256dd

- 单事务包住全部 UPDATE。理由修正：治的是 N 个隐式事务，不是 fsync

### 第 3 批：F2 图懒加载（按评审修正）✅ 已合入本提交

- **三态图槽**：`Unbuilt / Ready / Failed`——v0.1 的 Option 重载会静默失效（`ensure_fresh_for` 对 None 不重建、unified `guard.as_mut()?` 直接退双池，图永远建不起来且不报错）
- 或每个图入口显式 `ensure_graph_built()`：`hippocampus_search`、`unified_spread_hits`、`trace_cause`、`disable_spread`，配断言测试
- stdio 模式需后台预热（否则首查询从热变 30s 请求内建图，UX 回退）

### 第 4 批：F1 租户级 Memory 池化（收益最大风险最大，放最后）✅ 已合入本提交

前置：第 1 批全部合入。要点：
- `TenantStores` 改存 `Arc<Memory>`（amc.rs:65 有先例，但无淘汰——必须加）
- **淘汰策略**：LRU 按实例数上限起步（默认 64，env 可调）；get 时驱逐（不引入后台线程）；**驱逐前强制 flush_cooccurrences**（否则 P2 数据在驱逐点丢失，白救）；access_buffer 挂 CausalStore 随之释放
- **旁路写入新鲜度（P7，必须同批）**：`PRAGMA data_version` 探针（**不是** db mtime——WAL 下 checkpoint 前 mtime 不变会漏）用专用长驻探测连接（池借的连接同连接自写不变更，语义混淆）；变化时跑 **delta 追赶**：`WHERE discovered_at > snapshot_ts` 增量 patch + `WHERE valid_to > snapshot_ts` retire，毫秒级；全量 rebuild 回归摊销维护定位（GC），移出请求线程
- F1 救活 P2（长驻后 rebuild 周期 flush）+ 消除 P1

### 本批不做：F3 语义腿预过滤

评审否决理由：① 大库上 BM25 候选池 MAX_INDEX_CANDIDATES=900 必超限退全扫 → no-op；② 多租户库小（<8k 向量全扫仅 ms），P3 在多租户模式本就不痛；③ 唯一改变检索结果的变更，混在重构里无法归因；④ 真正该治的是写路径 `invalidate_semantic_contradictions` 全表扫（可按 task_tag/时间窗安全收窄，幂等）。
后续路线：绝对阈值腿（contradiction/similar_decision_edges）需要真近邻时，优先**与 embedding 同事务的量化签名列**（SimHash/随机投影——无漂移、可重建、无新依赖），而非 sqlite-vec 类 sidecar ANN（引入第四漂移源）。sqlite-vec 引入判据：单向量库 ≥1e5-1e6 + 绝对阈值腿 p95 超预算 + vec0 虚拟表可同事务维护，三者同时满足。

## 验证

- 每批独立回归 + cargo test --workspace 全绿（基线 494）
- 第 1 批：竞态/惊群断言；第 3 批：图入口懒建断言；第 4 批：同租户二次 search 零建图、data_version 注入旁路写后 trace_cause 可见、benches/bench_retrieval.rs 前后对比
- F3 若将来做：extraction_eval/adversarial_eval 跑开/关预过滤召回对比，阈值测量值非猜测

## 评审变更记录（v0.1 → v0.2）

| v0.1 假设 | 评审结论 |
|---|---|
| F4 治"N 次 fsync" | 错：synchronous=NORMAL 不 fsync；治的是 N 个隐式事务 |
| F3 与矛盾检索同模式（有候选池） | 错：矛盾检索全表扫；有候选池的是 BM25 路径，且 900 上限在大库必退全扫 |
| P5 用 fetch_sub 修 | 不够：防漂移计数不防丢边；真修法是代际双检+乐观重放 |
| F2 Option 重载 None | 会静默失效（ensure_fresh_for/unified 对 None 的行为）→ 三态或显式 ensure |
| 未覆盖旁路写入 | P7：git-sync 独立进程 align 写，需 data_version 探针 + delta 追赶 |
| 未提快照一致性 | P6：from_store 需 BEGIN DEFERRED |
| F1 直接上 | 放最后，前置 P5 全套；驱逐前必须 flush；探针禁用 mtime |
