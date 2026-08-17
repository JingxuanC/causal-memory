# mem0 评测对齐 · 后续任务（实施级，F1-F4）

> 读者：实现者（grok）。接续 `docs/mem0-eval-alignment.md`（E1/E3-pilot/E4-code/V1 已完成）。
> 原则不变：harness 级可认数据集标签，`crates/` 一行不许动；每任务独立 commit；
> 所有数字落盘进 `docs/benchmarks/`；**类目名已修正**（1=multi-hop, 2=temporal,
> 3=open-domain, 4=single-hop, 5=adversarial），对外材料一律用新映射。

## 现状基线（已落盘，勿重跑）

| 项 | 数字 | 位置 |
|---|---|---|
| LoCoMo V1 distill（strict judge） | 69.6% overall | `benches/locomo/results/run_distill_full_20260730_summary.json` 及同前缀 jsonl |
| LoCoMo V2（strict judge） | 74.2%（1474/1986） | `benches/locomo/results/e1_v2_full/run_20260731_144222_*` |
| E3 conv0 pilot（mem0 judge） | 86.4%（172/199），strict 同集 72.9% | `e1_v2_full/*conv0*rejudged*` |
| LME P7+P8（V1 prompt） | multi-session 57.9%，temporal 77.9% | `benches/longmemeval/results/p8_*` |

---

## Task F1：修 rejudge bug + 全量双口径（最高优先，成本 <$2）

### Bug（先修）

`benches/locomo/main.rs` 的 `rejudge` 子命令：输入 glob 没排除自己的输出，
导致 `e1_v2_full/` 里出现 `run_..._conv0.jsonl_rejudged_mem0.jsonl_rejudged_mem0.jsonl`
（23:05 的产物在 23:08 被当输入重判）。两次结果差 1 题（171 vs 172），
authority 含糊。

**修法**：
1. rejudge 的输入枚举排除任何文件名含 `_rejudged_` 的 jsonl；
2. rejudge 输出写到独立目录：`<out>/rejudged_<style>/`，不再以追加后缀方式
   放原目录；
3. 删掉现有双后缀文件，重跑 conv0 验证输出唯一。

### 全量 rejudge

对**两个已有 run**（V1 baseline 和 V2）分别用 mem0 judge 重判，产出 2×2 表：

| | strict judge | mem0 judge |
|---|---|---|
| V1 prompt | 69.6%（已有） | F1 补 |
| V2 prompt | 74.2%（已有） | conv0 pilot 86.4% → 全量待跑 |

- V2 全量 rejudge：`run_20260731_144222_conv*.jsonl`（10 个文件，1986 题）
- V1 全量 rejudge：`run_distill_full_20260730_*` 同前缀 jsonl
- 成本：2 × 1986 次 judge 调用，<$2

**验收**：2×2 表（overall + per-category，新类目名）写进
`docs/benchmarks/locomo.md` 的 E3 小节；注明 conv0 pilot 的 +13.5pp 与全量值的差异；
记录 judge 抖动观察（temp=0 两次判分差 1/199，~0.5%）作为方法论 caveat。
**报告纪律不变**：头条数字用 strict，mem0 口径以 "J-score (mem0-compatible)"
并列，不单独对外引用。

---

## Task F2：E4 全量 LME QA（V2 prompt × P7+P8 检索，~$3）

E4 代码已进（`--prompt-version v2`，abstention 保留），但**没有实测数字**。

**跑法**：现有 distill 库（勿重新 ingest），全量 500 题，
`--prompt-version v2`，其余协议与 P8 final run 完全一致
（P7 逐名词扩展 + P8 session 扩展，multi-session-only 守卫不变）。

**对照基线**（V1 prompt，同检索栈）：
- multi-session 57.9%（p8_multisession_final）
- temporal 77.9%（p8_temporal_v2）
- 其余题型用 distill baseline 的既有数（`docs/benchmarks/longmemeval.md`）

**验收**：per-type 表（6 题型 × V1/V2）+ 合成 overall 写进
`docs/benchmarks/longmemeval.md`；若某题型 V2 回退 >2pp，先 diff 错题再决定
是否保留 V2 为该题型默认（允许按题型分派 prompt 版本——harness 认标签合规，
但必须在文档里明示分派规则）。

---

## Task F3：E2 cutoff 评测（LoCoMo，~$6）

设计文档 Task E2 被跳过了，补做。multi-hop 差 mem0 官方 57.5pp（35.8% vs 93.3%，
注意 mem0 数字是 gpt-5 + 宽松 judge + top-200，不可直接比），需要 cutoff 曲线
定位瓶颈在检索预算还是召回结构。

**实现**（`benches/locomo/main.rs`）：
1. CLI `--topk-cutoffs 10,20,50`，与 `--topk` 互斥；
2. 一次检索 topk=50，facts 层同上限；每档 facts 优先 + causal 补齐到档位数；
3. V2 prompt（沿用 E1），strict judge（与基线同口径）；
4. `ResultRow` 加 `cutoff_results`（字段对齐 mem0：`judgment/score/
   generated_answer/memories_evaluated`），summary 输出每档 overall+per-category。

**验收**：准确率-档位曲线表进 `docs/benchmarks/locomo.md`。
判读标准写进文档：50 档比 10 档高 >5pp → 检索预算是瓶颈，给检索扩展
（实体层/BM25 融合）立量化依据；基本持平 → 瓶颈在召回结构/抽取，
转向 P8 类结构工作。

---

## Task F4：cat5 回退归因（零 API 成本，先做分析再谈修）

E1 V2 后 cat5 91.9% → 88.3%（−3.6pp = 16 题）。grok 的注释说 cat5 仍走 V1
prompt——同 prompt 同 judge 不该掉，变量在别处。三个嫌疑：
1. V2 的记忆时间正序重排（`event_time` 升序）渗到了 cat5 的记忆呈现；
2. `ANSWER:` 标记 rsplit 解析对 cat5 生效（V1 prompt 不发标记，rsplit 行为
   需核对——若模型偶尔自发输出 "ANSWER:" 字样会被截断）；
3. 判分路径变化。

**做法**：纯本地分析，不调 API：
1. 从 `run_distill_full_20260730_*`（V1）和 `run_20260731_144222_*`（V2）
   导出 cat5 错题集，取差集（V2 错 V1 对的 16 题±）；
2. 对每题对比： predicted 文本差异、记忆列表顺序差异、是否有 "ANSWER:" 截断痕迹；
3. 结论写进 `docs/benchmarks/locomo.md` failure analysis 小节，
   **先归因后修**——不许直接改 prompt 蒙。

---

## 执行顺序

| 序 | 任务 | 成本 | 产出 |
|---|---|---|---|
| 1 | F1 bug 修复 + 全量双口径 | <$2 | 2×2 表（论文级关键数字） |
| 2 | F4 cat5 归因 | 0 | failure analysis 更新 |
| 3 | F2 E4 全量 LME | ~$3 | LME per-type V2 表 |
| 4 | F3 E2 cutoff | ~$6 | 检索预算-准确率曲线 |

**红线重申**：每任务独立 commit（不要再 E1+P8 或 E3+E4+V1 混合提交）；
summary JSON 记录完整命令行、`prompt_version`、`judge_style`、git commit；
`crates/` 不动；类目用新映射。
