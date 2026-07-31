# mem0 评测对齐 + 结果可视化（实施级设计文档）

> 读者：实现者（grok）。每个任务给出确切文件、协议约束和验收标准。
> 原则沿用在先约定：**harness 级实验可以认数据集标签，lib 级代码（`crates/`）不许**；
> 所有数字必须同 harness、同模型、同检索库、同协议的 controlled delta；
> 双口径结果必须诚实标注，不许只报宽松口径。

## 背景：mem0 官方评测套件调研结论

调研对象：mem0 官方独立评测仓 `mem0ai/memory-benchmarks`（commit `4b61c5d`，2026-04），
本地克隆在 `/Users/hjx/Documents/kimi/workspace/memory-benchmarks/`（参考用，勿提交进本仓）。

官方结果（仓内 `results/platform/locomo_results.json`，gpt-5 answerer + gpt-5 judge，top_k=200）：

| benchmark | overall | 分类目 |
|---|---|---|
| LoCoMo platform v3 | **91.6%** | multi-hop 93.3 / temporal 92.8 / open-domain 76.0 / single-hop 92.3 |
| LongMemEval platform | **93.4%** | multi-session 86.5 / temporal 93.2 / knowledge-update 96.2 |
| LoCoMo OSS 栈（gpt-5） | 91.0 | — |
| LoCoMo OSS（llama4-maverick / gemma4） | 88.6 | — |

**关键结论**：换成开源小模型只掉 2~5pp，说明 mem0 的高分不全是模型功劳，
相当部分来自 **答案 prompt 工程、检索预算（top-200）、judge 口径**三个非检索因素。
这正是我们可以低成本对齐的部分。

**类目编号映射（对比时务必换算，mem0 与我们编号不同）**：

| mem0 | 含义 | 我们 |
|---|---|---|
| cat1 | multi-hop | cat3 |
| cat2 | temporal | cat2 |
| cat3 | open-domain | cat4 |
| cat4 | single-hop | cat1 |
| cat5 | adversarial（不计分） | cat5（我们单独报） |

**三重不可比因素（论文/文档必须标注）**：
1. 答题/判分模型：gpt-5 vs 我们 deepseek-chat；
2. judge 口径：mem0 judge 极宽松（部分给分、日期 ±14 天容忍、evidence 只用于接受）；
3. 检索预算：top-200 + BM25/实体融合 vs 我们 top-10 单信号。

---

## Task E1：LoCoMo 答案 prompt 移植（7 步推理式）

**动机**：mem0 的 ANSWER_GENERATION_PROMPT 是 7 步推理式（`benchmarks/locomo/prompts.py`），
比我们的一句话指令精细一个量级。移植零风险纯收益——不动检索、不动库、不动判分，
只换答题行为。重点治：列表题过度过滤失分（Step 6 INCLUSION CHECK）、
时序题相对日期错误（Step 5）、lost-in-the-middle（Step 1）。

**位置**：`benches/locomo/main.rs`

**改动 1 — prompt 常量**。替换 `ANSWER_SYSTEM_PROMPT`（第 51 行附近）为下面的 V2。
注意 **cat5（adversarial）必须保留弃权能力**——mem0 的 "NEVER say not mentioned"
只适用于 cat1-4（他们 cat5 不计分），我们 cat5 单独计分且考的就是拒答。
实现方式：两个常量，按 `qa.category == 5` 分派（harness 认标签，合规）：

```rust
const ANSWER_SYSTEM_PROMPT_V2: &str = r#"You are answering a question using retrieved memories from past conversations between two people. Follow these reasoning steps IN ORDER.

## Step 1: SCAN ALL MEMORIES
Read EVERY memory from first to last. Do NOT stop after finding the first relevant memory — important details are often scattered across the whole list, including near the end. Give equal weight to ALL memories regardless of position.

## Step 2: ENTITY VERIFICATION
Confirm each relevant memory is about the correct person/entity. If the question asks about Person A and a memory attributes something to Person B, do not use it for A — unless B is the other speaker in the same conversation, in which case it is still valid shared evidence, but check the attribution.

## Step 3: COMBINE AND CROSS-REFERENCE
- COMBINE facts from multiple memories about the same topic.
- For listing/counting questions, extract EVERY distinct item from ALL memories, then re-scan specifically for each category of answer.
- For counting questions ("how many times"), enumerate each distinct instance explicitly with its date or context BEFORE giving a final count. Do not estimate — list, then count.
- DECOMPOSE complex sentences: "an immersive X with Y" contains multiple distinct facts.

## Step 4: SELECT THE BEST ANSWER
- ALWAYS choose the MOST SPECIFIC detail available. A proper name, title, or number beats a generic description.
- Report what someone actually DID, not what was offered or available. "Has not tried X yet" means X was NOT done.
- Repetition of a generic fact across memories does NOT make it more correct than one memory with a more specific answer.

## Step 5: TEMPORAL GROUNDING
- Resolve all relative time expressions ("yesterday", "last week", "last year") against the date attached to each memory, and answer with an ABSOLUTE date or period (e.g. "7 May 2023", "June 2023").
- For "how long" questions, find explicit start and end dates, then compute. Do not guess.
- When MULTIPLE instances of similar events exist at different dates, enumerate them with dates before picking: past tense + "the" → the instance closest to (before) the conversation's latest date; future tense → the earliest planned date.

## Step 6: INCLUSION CHECK (for lists and counts)
If you found items you are tempted to exclude — STOP. Include them unless you have STRONG evidence they are wrong. The most common mistake is dropping relevant items through overly strict filtering. More items is better than fewer when there is supporting evidence.

## Step 7: COMMIT AND ANSWER
Give a direct, specific answer after "ANSWER:". NEVER say "not specified", "not mentioned", or "the memories don't say" when ANY memory contains relevant information. No hedging.
- NEVER invent specific names, titles, places, or dates that do not appear in any memory. If no memory contains the requested detail, answer with what the memories DO contain.
- Keep the final answer short: a few words or one or two sentences."#;

// cat5 沿用现有允许拒答的 prompt（现 ANSWER_SYSTEM_PROMPT 原样保留，
// 改名 ANSWER_SYSTEM_PROMPT_ADVERSARIAL）。
```

**改动 2 — 答案标记解析**。V2 prompt 要求推理后输出 `ANSWER:` 标记，解析侧：
`predicted = raw.rsplit("ANSWER:", 1).last().trim()`（与 mem0 `run.py:468` 一致）。
`ANSWER_MAX_TOKENS` 从 200 调到 **800**（7 步推理需要 token 预算，最终答案仍短）。
judge 输入仍只给解析后的短答案。

**改动 3 — 记忆按时间正序呈现**。mem0 把检索结果按 created_at 升序排（叙事化，
防位置偏置），且不向答题模型展示检索分数/排名。我们的 `memory_lines()`
（第 351 行）目前按检索分数序。改动：在 `answer_question()` 里对 `retrieved`
按 `CausalEntry.event_time` 升序排序后再进 `memory_lines()`（`event_time`
字段现成，`crates/causal-memory/src/store/types.rs:30`）。distill 模式的
facts 层保持**仍放最前**（高精层协议不变），causal 部分内部按时间序。

**红线**：
- 不动 `retrieve()`、不动 `crates/`、不动 ingest；用**现有** `*_distill.db` 直接重跑 QA；
- judge 协议不变（E3 才动 judge）；top-k 默认 10 不变（E2 才动预算）；
- `--prompt-version v1|v2` 加 CLI 开关（默认 v2），v1 保留可复现旧结果。

**验收**：`run --ingest distill` 全量 10 对话 QA（约 $2），输出 per-category 表与
69.6% 基线（`docs/benchmarks/` 记录的 distill 结果）逐类目对比。
预期收益主要来自 cat1(single-hop) 列表题与 cat2(temporal)；若总分下降必须回查原因，
不许直接提交回退数据。

---

## Task E2：cutoff 评测（一次检索，多档判分）

**动机**：mem0 一次 search top-200 后在 10/20/50/200 四档分别答题判分，画出
"检索预算 vs 准确率"曲线，把**召回失败**和**答题失败**拆开归因。我们目前
top-10 一档，无法回答"是不是检索预算不够"。

**位置**：`benches/locomo/main.rs`

**实现**：
1. CLI 加 `--topk-cutoffs LIST`（如 `10,20,50`），与 `--topk` 互斥；
2. 检索一次，`topk = max(cutoffs)`；distill 模式 facts 层也用同一上限；
3. 对每个 cutoff：facts 优先 + causal 补齐到该档条数 → 答题 → 判分；
4. `ResultRow` 加 `cutoff_results: Map<String, CutoffResult>`（字段对齐 mem0：
   `judgment / score / generated_answer / memories_evaluated`），summary JSON
   输出每档 overall + per-category 准确率。

**成本**：N 档 = N 倍答题+判分调用。建议跑 `10,20,50` 三档（约 $6）。
我们单对话库规模只有几百条 entry，50 档已接近"全库"，不必上到 200。

**验收**：输出准确率-档位曲线表；若 50 档显著高于 10 档（+5pp 以上），
说明检索预算是瓶颈，把结论写进 `docs/benchmarks/locomo.md`，
为后续检索扩展（P7 类工作）提供量化依据；若基本持平，说明瓶颈在答题/抽取，
同样记录。

---

## Task E3：judge 对齐（双口径判分 + rejudge 子命令）

**动机**：mem0 judge 极宽松（列表题答对 1 项即 CORRECT、日期 ±14 天、时长 ±50%、
额外细节不扣分、evidence 只用于接受不用于拒绝）。我们 judge 严格得多。
两个口径的数字差就是"judge 口径税"——必须量化它，否则无法与 mem0 数字对比，
也无法在论文里诚实站位。

**位置**：`benches/locomo/main.rs`

**改动 1 — mem0 风格 judge 常量**（适配我们的 JSON verdict 格式）：

```rust
const JUDGE_SYSTEM_PROMPT_MEM0: &str = r#"You are evaluating conversational AI memory recall. Label the predicted answer as CORRECT or WRONG.

Rules:
1. PARTIAL CREDIT: If the predicted answer includes AT LEAST ONE correct item from the gold answer's list, mark CORRECT. Only mark WRONG if NONE of the gold items appear.
2. PARAPHRASES COUNT: Same concept in different words is CORRECT. Emotions in the same positive/negative family count as paraphrases.
3. EXTRA DETAIL IS FINE: A longer answer that includes the gold answer's key facts plus more is CORRECT. Never penalize detail.
4. DATE TOLERANCE: Dates within 14 days are CORRECT. Durations within 50% are CORRECT. Converting relative dates to the correct absolute date is CORRECT.
5. SEMANTIC OVERLAP: Judge whether the answer addresses the same topic and captures the core idea. Different wording or specificity should not cause WRONG.
6. SAME REFERENT: If the answer references the same core entity/concept as the gold answer, mark CORRECT even with different descriptions.

ONLY mark WRONG if: the answer contains ZERO correct items from the gold answer, or addresses a completely different topic.

Respond with ONLY a JSON object: {"verdict": "correct" or "incorrect", "reason": "<one sentence>"}"#;
```

**改动 2 — CLI**：`--judge-style strict|mem0`（默认 strict，保持既有协议）。
cat5 判分逻辑（对抗题专项）不变，两种 style 都只作用于 cat1-4。

**改动 3 — `rejudge` 子命令**：读已有结果 JSON（含 `predicted` 字段），
**不重答**，只对每题重新判分（judge 调用约 1540 次，成本 <$1），输出新 summary。
用法：`causal-memory-locomo rejudge --input results/<run>.json --judge-style mem0`。

**验收**：产出 2×2 表——{strict, mem0 judge} × {v1, v2 prompt} 四个总分，
全部写进 `docs/benchmarks/locomo.md`。**报告纪律**：对外头条数字一律用
strict judge；mem0 口径只以 "J-score (mem0-compatible judge)" 名义并列呈现。

---

## Task E4：LME 答案 prompt 对齐

**位置**：`benches/longmemeval/main.rs`

**改动**：把 E1 的 V2 prompt 移植到 LME harness 的答题段，差异点：
1. **弃权题必须保留**：LME 有 ground truth 为 "I don't know / not mentioned" 的题
   （参考 mem0 `benchmarks/longmemeval/run.py:114` 的 Abstention Case），
   Step 7 的"禁止说不知道"对这子集要放松——按 question_type 或 ground truth
   是否可答条件化（harness 认标签合规）；判分侧对齐 mem0 规则：记忆为空或不会
   导致自信的错误答案 → PASS。
2. 记忆行已带 `[session_id date]` 前缀（现协议），保持，确认按时间正序；
3. P7 的逐名词查询扩展、`COVERAGE_LIMITED` 路由**不动**——E4 只换答题 prompt，
   与 P7/P8 是正交变量。

**验收**：现有 distill 库全量 500 题 QA（约 $3），对比 69.6% 基线分题型 delta 表。
注意与 P8（grok 并行进行中，`main.rs` 与 `retrieve.rs` 有未提交改动）
错开：**E4 等 P8 合并后再动 `benches/longmemeval/main.rs`**，先只做 E1-E3。

---

## Task V1：结果可视化

**动机**：mem0 套件内置 Next.js 结果看板（`src/`，SQLite 存储）：分类目条形图、
multi-cutoff 表、**逐题检查器**（展开看检索 memories / 生成答案 / judge reasoning）、
多 run 对比。逐题检查器 + run 对比正是我们 ablation 最缺的，现在只能手写脚本翻 JSON。

**Phase 1 — adapter 借鸡生蛋（先做，零前端开发）**：
1. 脚本 `scripts/export_mem0ui.py`（Python，标准库 + sqlite3，不进 Cargo）：
   读 `benches/*/results/*.json`（含 E2 的 cutoff_results），转成 mem0 UI 的
   SQLite schema（参考 `/Users/hjx/Documents/kimi/workspace/memory-benchmarks/src/lib/schema.ts`
   和 `db.ts`，以实际代码为准）；
2. 在该克隆仓里 `npm install && npm run dev -- -p 3001`，用他们的 UI 看我们的数据；
3. 类目映射按背景节表格换算，adapter 里统一成 mem0 编号。

**Phase 2 — 自建轻量报告（仅当 Phase 1 验证有价值且嫌依赖重）**：
`scripts/render_report.py` 生成自包含静态 HTML（零依赖，内联 JS），
只保留三视图：逐题检查器（可按类目/正确性过滤）、cutoff 表、多 run 对比表。
产物放 `benches/*/results/report.html`，可提交。

**验收**：Phase 1 能在浏览器里打开任意一次 run 并逐题检查；Phase 2（如做）
HTML 离线可开。

---

## 执行顺序与成本预算

| 序 | 任务 | 成本 | 依赖 |
|---|---|---|---|
| 1 | E1 LoCoMo prompt V2 | ~$2 | 无 |
| 2 | E3 judge 双口径 + rejudge | <$1 | 无（可复用 E1 前旧结果） |
| 3 | E2 cutoff 评测 | ~$6 | 建议跑在 E1 之后（v2 prompt + cutoff 一次出） |
| 4 | V1 Phase 1 adapter | 0 | 无 |
| 5 | E4 LME prompt 对齐 | ~$3 | **等 P8 合并** |
| 6 | V1 Phase 2（可选） | 0 | Phase 1 验证后 |

**通用红线**：
- 每个任务独立 commit，summary JSON 记录完整命令行与模型版本；
- `crates/` 一行不许动；
- 与 grok 的 P8/P9 并行工作错开文件：E1-E3 只碰 `benches/locomo/main.rs`，
  E4 碰 `benches/longmemeval/main.rs` 前确认 P8 已合并；
- 所有新数字进 `docs/benchmarks/` 对应文档，严格 vs mem0 口径分开列。
