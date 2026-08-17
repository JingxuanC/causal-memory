# P8 / P9 实验设计文档（实施级）

> 读者：实现者（grok）。每个任务都给了确切的文件、函数签名、协议约束和验收标准。
> 原则沿用在先约定：**harness 级实验可以认数据集标签（上限探针），lib 级代码不许**；
> 所有数字必须同 harness、同模型、同 judge、同协议的 controlled delta，诚实标注口径。

---

## P8：LME 检索路径 —— session 扩展 + 跨 session 因果追踪

### 背景（已实现到这一步）

- P7（逐名词多查询扩展，已提交 `f5be144`）：multi-session 41.4% → **50.4%**，
  temporal-reasoning 69.9% → **77.4%**，合成总分 ≈74.0%。
- 确诊的病因（写在 `docs/benchmarks/longmemeval.md`）：multi-session 证据**完整覆盖**仅
  45%（P7 后），完整覆盖时准确率 76.7% vs 不完整 26.8%——瓶颈是证据集完整性。
- P7 后完整覆盖还差 55 个百分点没补上：逐名词查询捞到了更多碎片，但**碎片不等于完整
  session**——一个 session 30+ 轮，命中 2 轮还是缺上下文。

### Task A：session 扩展（零 LLM 成本，先做）

**位置**：`benches/longmemeval/main.rs` 的 `retrieve()`，P7 合并之后返回之前。

**依据**：LME chunk id 格式是 `{question_id}::{session_id}::{turn}`（文件头注释第 21 行），
session 归属可以直接从 id 解析，不需要任何额外索引。

**算法**：
1. 对 P7 合并后的 entries，按 `session_id`（从 decision/outcome chunk id 第二段解析）分组；
2. 每个 session 打分 = 命中 entry 数（或 BM25 分数和，取实现方便且可复现的）；
3. 取 top 3-5 个 session，把它们的**全部 chunks** 拉出来（`task_tag = question_id` 过滤不变，
   天然硬隔离），按 turn 排序，cap 在 ~40 条以内防 prompt 爆炸；
4. 与已有 entries 按 edge_id 去重合并后返回。

**约束**：
- 只对 `COVERAGE_LIMITED = ["multi-session", "temporal-reasoning"]` 生效（与 P7 一致）；
- 不引入任何 LLM 调用；
- prompt 组装侧（`memory_lines`）不用改，但注意行数预算：如果 evidence 行数翻倍，
  检查 answer prompt 是否超过模型上下文，必要时对 session 内轮次做截断（保首尾）。

### Task B：跨 session 因果追踪（Task A 不够再做）

**API（现成，勿改签名）**：`crates/causal-memory/src/store/retrieve.rs:860`

```rust
pub fn trace_cause_cross_session(
    &self,
    outcome_description: &str,
    max_depth: usize,
    min_confidence: f64,
    max_meta_bridges: usize,
) -> Result<Vec<super::CrossSessionChain>>
```

**接法**：在 `retrieve()` 里对 coverage-limited 题型，以 `q.question` 为
`outcome_description`，参数起点 `max_depth=3, min_confidence=0.3, max_meta_bridges=2`；
把返回链上的节点文本映射回本题的 chunk 集合。

**红线**：返回结果**必须过滤到当前 question 的 task_tag 范围**——跨 session ≠ 跨题，
LME 协议根基是逐题硬隔离。`CrossSessionChain` 里任何不属于本题 `task_tag` 的节点必须丢弃。
如果该 API 本身不带 task_tag 过滤，在调用侧按 chunk id 前缀 `{question_id}::` 过滤。

### 验收（顺序执行，绿了才走下一步）

1. `./target/release/causal-memory-longmemeval run --data benches/longmemeval/data/longmemeval_s_cleaned.json --ingest distill --qtype multi-session --concurrency 64`（<$1，标记机制自动跳过 ingest）
   - **目标：multi-session ≥ 55%**，完整覆盖率 ≥ 55%
2. 同命令 `--qtype temporal-reasoning`——**目标 ≥ 78%**，且不允许比 77.4% 回退超过 1pp
3. 两个都绿 → 全量 500 题（~$2，拿到无争议的整跑数字）；任一不达标 → 记录数字、
   分析失败题、不要硬调 prompt 凑分
4. 文档：结果写进 `docs/benchmarks/longmemeval.md` 的 P8 小节（同 P7 小节的表格格式），
   注明是哪一步（A / A+B）达到的

---

## P9：P1-P6 动力学 × trap-world 对照实验

### 为什么需要这个实验

P1-P6（Hebbian 共现强化、不可变 SWR 巩固、Q 值动力学、新颖度熵触发、分层加载）全是
**时间维度的动力学**——LoCoMo/LME/Memora 的静态一次性协议里它们没有出场机会。
trap-world bench（`crates/causal-memory-cli/src/bench_agent.rs`，975 行，已有）是唯一
天然有"重复暴露 + 时间跨度"的考场：同族陷阱第 2 次遇到时，记忆有没有让 agent 变聪明。

### 实验设计

**三个条件**（同一模型、同一 seed、同一任务集、同温度）：

| 条件 | 记忆 | 动力学 | 测量的是什么 |
|---|---|---|---|
| A | 无 | — | 基线（已有数据：重复踩坑率 67%） |
| B | 有（现有检索） | 全关 | 静态记忆的价值（已有数据：33%） |
| C | 有 | **全开** | 动力学的增量价值（新） |

**条件 C 的具体接法**（都走已有 API，勿新造机制）：
- 检索时 `spreading_activation_opts(query, None, false, /*run_hebbian=*/true)`
  ——共激活节点自动连线（P2，`ff124df` 已把它做成 opt-in，这里就是 opt 的场景）
- agent 踩坑/成功时按结果调 `update_q_value()`（P4，Bellman 备份；奖励 r：踩坑 0、
  成功 1，α/γ 用 lib 默认）
- 每个 task block（建议每 4 个任务一块）结束后：算 `novelty_entropy()`（P6），
  > 0.6 则执行一次 `swr_consolidate_immutable()`（P3），把 delta_log 写进结果文件

**任务规模**：`generate_tasks(seed, k=12, n_families=4)`，整套重复 **R=5 轮**
（每轮同 seed 重新生成同一任务序列，保证重复暴露语义一致），三条件共
3 × 12 × 5 = 180 个 episode。先跑 R=1 的 pilot 验证接线正确再全量。

**指标**（现有统计结构 `repeat_exposures/repeat_trapped` 直接复用）：
1. **重复踩坑率**（2 次+暴露时再次踩坑的比例）——主指标：C 必须显著低于 B
2. 首动作命中率（检索后第一步就踩对的比例）——检索质量
3. 平均步数随暴露次数的趋势——学习曲线形态
4. 开销：C 比 B 每任务多花的步数/调用数（动力学不是免费的，诚实记账）

### 验收与产出

1. 接线单测：条件 C 下至少一次 Hebbian 权重实际上升、一次 Q 值实际更新、
   一次 SWR 巩固实际触发（mock 或短 pilot 验证，不许全量跑完才发现没接上）
2. 结果文档 `docs/benchmarks/agent-dynamics.md`：三条件对比表 + 学习曲线 +
   开销账本；若 C ≤ B 无显著差异，**如实写"动力学在此场景无增量"**——
   这也是有价值的结果，不许硬凑
3. 结果文件落 `benches/agent/results/`（json + md，沿用现有命名）
4. 模型/密钥：先检查 bench_agent 用的模型环境变量（README 记录上次是 glm-4-plus），
   跑之前向所有者确认模型选择和预算上限

---

## 全局约束（两个任务都适用）

- 提交前 `cargo test --workspace` 全绿 + 无 clippy deny 级错误
- 不改 lib 公共 API 签名；新增参数走 `_opts` 变体或新函数
- commit message 写明：测量条件、样本量、协议口径（harness 实验还是 lib 行为）
- 文档数字必须可由 `benches/*/results/` 里的 summary json 复算
