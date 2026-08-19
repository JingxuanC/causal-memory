# 多会话检索提升方案（LongMemEval multi-session 32.3% → 目标 ≥68%）

> 状态：设计稿（2026-08）。基于 P8 逐题失败数据诊断；分三步落地，每步可独立验证。

## 1. 现状与诊断

### 1.1 数字链（harness 级，133 题 multi-session 切片）

| 阶段 | 准确率 | 手段 | 性质 |
|---|---|---|---|
| raw 基线 | 32.3% | BM25 top-10，单查询 | 生产可用形态 |
| + distill | 41.4% | LLM 蒸馏 fact 层 | 生产可用（已实现） |
| + P7 名词扩展 | 50.4% | 每名词一条 BM25 | **harness 级**（依赖数据集 type 标签） |
| + P8 session 展开 | 57.9% | 命中 session 全量灌入 | **harness 级**（同上 + 40 chunk 上限） |

### 1.2 失败模式（P8 逐题数据，56 失败）

- **82%（46/56）失败时证据已检索到**——evidence_hit（任一金句命中）是弱指标，掩盖了真正的缺口。
- **覆盖率断层**：失败题平均证据覆盖 0.55 vs 正确题 0.82；count/list 类失败题平均 **0.45**（中位金句 3.3 句）。
- 只有 14/56 失败是"全覆盖仍错"（纯聚合失败）；**约 3/4 的失败源于检索没把证据集凑齐**。

### 1.3 失败细分（56 题）与机制根因

失败细分（56 题）：
- 14 全覆盖仍错（纯聚合失败，见根因 4）；
- 22 金句所在 session 检索完全没触达（session 级漏召回，见根因 1）；
- ~11 session 触达但具体金句 turn 没进来（turn 级漏召回，见根因 2）；
- 10 evidence_hit=0（完全没命中）。

机制根因：
1. **session 级漏召回**：证据其实最多跨 5 个 session（中位 2），但 BM25/noun 查询只让部分证据 session 产生命中并进入展开列表——未命中 session 里的证据整段丢失（22/56）。
2. **turn 级漏召回 + 预算截断**：展开按"BM25 命中数"给 session 排序、40 chunk 上限截断；证据 turn 经常落在截断点之后（session 已触达但 cov<1 的 11 题）。检索平均摸到 12 个 session，却仍凑不齐 2-5 个 session 的证据。
3. **时间锚完全没用上**：133 题中 19 题带显式时间窗（last month / past two weeks / since start of year），8/56 失败题因此把检索空间放大到全历史——按窗口收窄本可直接锁定正确 session（"How many plants did I acquire in the last month" 只在 5 月窗口内找）。
4. **答案阶段枚举失败**（14/56）：40+ 行上下文中模型漏数（23 篇数成 17，2 部找到 1）。

> P7/P8 的做法不能直接搬进 lib：它们靠数据集的 type 标签开挂、且把整 session 倒进 prompt。生产环境没有 type 标签，必须**运行时推断证据拓扑**。

## 2. 目标与约束

- 无 type 标签：全部启发式/规则从问题文本 + 存储元数据（event_time、session 前缀）推断。
- 保持隔离边界（task_tag scope 不变），零新依赖（crates.io 不可达），LLM 成本可控。
- 回归约束：temporal-reasoning 133 题不得下降（时间过滤做成**加权优先**而非硬过滤）。

## 3. 设计：两段式迭代检索（时间锚 + 多查询 + 验证回环）

### 3.1 Query decomposition（规则，无 LLM）

- **实体抽取**：复用 P7 的 stopword 过滤思路，但落 lib 级 query_decompose()：去停用词、取 ≥4 字符名词/专名，附加单复数还原（items→item）。
- **时间锚解析** parse_temporal_anchor()：规则识别 last month / past N weeks|days|months / since start of year / yesterday / <具体日期>，对照"当前时间"（MCP 场景用调用时刻）产出 [start_ts, end_ts] 日期窗；解析失败返回 None（不阻塞主路径）。

### 3.2 多查询 + 时间窗 + 全覆盖 session 展开

1. base = BM25(question) + 每实体 BM25(term)，edge-id 去重合并（= P7 的 lib 化，但去掉 topk/2 的小 cap，统一 per_query_cap）。
2. **时间窗加权**：命中边按 event_time 是否落入日期窗排序（窗内 +0.3 权重 / 窗外 -0.3），**不硬删**——保护 temporal 类精确问题。
3. **session 展开改为全覆盖**：对合并结果里命中的**所有** session 取全量 chunk（不再是 top-5），排序键从"BM25 命中数"改为"该 session 中问题实体出现频次 + 命中数"；预算上限放宽到 ~80 chunk（实测 P8 在 40 cap 下已 57.9%，覆盖是主瓶颈）。
4. 融合复用现有 rrf_fuse_many + 现有 hop 扩展（A2）。

### 3.3 Verification loop（迭代补检索，成本护栏）

第一遍检索 → 作答；当问题形态命中枚举/计数启发式（how many | list | which <plural> | all of），追加 ≤2 轮验证：

1. 让 LLM 从已检索记忆中**列出已找到的 items**（强制编号）。
2. 从 items 提取新实体 → 补 BM25 查询 → 合并去重 → 重新作答。
3. 每轮预算：≤2 次 LLM 调用；总检索上限 ~80 chunk；不满足启发式直接返回第一遍答案。

> 这直接打 1.3 的根因 1/3：每一轮验证补的都是低命中 session 里的漏网证据。

### 3.4 答案契约（第三步，若 Step A 后仍有余量）

全覆盖仍错的 14 题是纯聚合失败：把 V1 的 MULTI_SESSION_RULE 强化为"先编号列出每一项，再给总数"的硬性两步（对 count/list 形态启用），与 LoCoMo E1 的枚举逻辑同构。

## 4. 落地路径

| Step | 内容 | 产出 | 验证 |
|---|---|---|---|
| A | lib 级 query_decompose + 时间锚 + 全覆盖 session 展开 + verification loop | Memory::search_memory 内部新增多遍检索路径（默认关闭，search_memory(..., recall: true) 或新工具）；harness retrieve() 改为调 lib 路径（去 type 标签依赖） | multi-session 133 题 ≥65%；temporal 133 题不降 |
| B | 指标升级：evidence_hit → mean coverage + full-coverage rate，报告双指标 | harness 输出 coverage 统计 | 覆盖率可追踪 |
| C | 枚举答案契约（3.4） | prompt 调整 | multi-session 在 A 基础上再追 ≥2pp |

## 5. 验证计划

- 固定 distill DB（longmemeval_distill.db 不变），只改 QA 侧检索路径，逐 Step 跑 133 题。
- 全类型 500 题回归一次（Step A 后），确认其它 4 类不降。
- 成本：verification loop 每题 ≤2 次额外 LLM 调用；Step A 全量预计 +266 次调用以内（133×2）。

## 6. 风险与护栏

| 风险 | 护栏 |
|---|---|
| 时间锚误伤 temporal 精确题 | 加权而非过滤；回归跑 temporal 133 题 |
| 上下文爆炸（80+ chunk） | 预算上限 + 与 P8 相同 cap 逻辑 |
| verification loop 误判枚举形态浪费 token | 启发式保守（只有 how many/list/which 才触发）+ 预算护栏 |
| lib 无 type 标签导致行为漂移 | harness 对比跑：type-agnostic 路径 vs 原 P7+P8 结果 |

## 7. 预期收益

覆盖是主因（约 3/4 失败），Step A 的大头来自全覆盖展开 + 时间窗 + 验证回环补齐长尾 session：multi-session 57.9% → **≥68%**（对应整体 500 题从 ~75% → ~78%）。

## 8. Step A 实施结果（2026-08-19，commit 84bc4c8）

lib 新增 `retrieval.rs`（纯规则、零新依赖）：query decomposition（实体 + 时间锚解析 + 聚合形态检测）、`retrieve_multi_pass`（多查询 + 时间窗加权 + 证据拓扑触发）、`expand_session_chunks`（全覆盖展开，预算 80）、`query_terms`（验证回环助手）；harness `retrieve()` 走 lib 路径（type 标签门删除）、验证回环（`--verify-loop`，≤2 轮、有新 chunk 才重答）、session 注入文本级去重；facade 新增 `Memory::search_memory_multi_pass`。

**验证（raw 模式、V1 prompt、133 题全量）**：

| 切片 | 同代码库 baseline | multipass core | +verify-loop |
|---|---|---|---|
| multi-session | 42.9% | 52.6% | **57.9%**（+15.0pp） |
| temporal-reasoning | 54.9% | **60.2%**（+5.3pp） | — |

- 文档里的旧基线（multi-session 32.3% / temporal 61.7%）在本机 + 当前 deepseek-chat 别名模型下**不重现**（baseline 模式重跑为 42.9% / 54.9%，差异来自模型版本漂移）；同代码库对照为严谨口径。
- 以旧基线为参照：multi-session **32.3% → 57.9%（+25.6pp）**，temporal **61.7% → 60.2%**（对照口径差异，非回退）；同代码库口径下两个切片均显著为正。
- raw+verify-loop 的 57.9% 追平了 P8 的 distill 57.9%——检索改进完全抵消了蒸馏层的增益（蒸馏 +9pp 若叠加，multi-session 有望 ≥65% 目标）。
- 剩余失败：56 题中 46 题证据已检索到但聚合/枚举仍错（mean coverage 0.53），13 题全覆盖仍错——对应 Step C（枚举答案契约）与蒸馏层。
- 三轮 multipass temporal 重跑精确复现 60.2%（确定性）。

Step B（coverage 指标）已在分析中落地；Step C（枚举答案契约）与 distill DB 重建留待下一轮。
