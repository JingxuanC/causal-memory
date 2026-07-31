# mem0 评测对齐 · 收尾任务（实施级，F5-F8）

> 读者：实现者（grok）。接续 `docs/mem0-eval-followup.md`（F1/F4 已完成并验证，
> F2 数字已落盘但表述需修正，F3 只有脚手架）。
> 原则不变：harness 级可认数据集标签，`crates/` 不动；**每任务独立 commit**；
> summary JSON 必须记录 `prompt_version` / `judge_style` / 完整命令行；
> 类目用新映射（1=multi-hop, 2=temporal, 3=open-domain, 4=single-hop, 5=adversarial）。

## 当前基线（已验证，勿重跑）

- LoCoMo V2 × strict = **74.2%**；V2 × mem0 judge = **84.1%**（judge 税 +9.9pp，
  multi-hop 35.8%→73.0%）；同口径距 mem0 官方 91.6% 仅 7.5pp。
- LME（P7+P8，V1 prompt）合成 ≈ **75.4%**；E4 V2 实测 70.8%（回退，见 F7）。
- cat5 修复（event_time 排序守卫）已进代码，**未验证**。

---

## Task F5：补完 2×2 表 —— V1 × mem0 judge（<$1）

F1 提交的"V1 行级数据未保留"判断**有误**。V1 distill 全量 run 的 per-conv
jsonl 就在 `benches/locomo/results/` 根目录：

```
run_20260730_182024_conv0.jsonl   run_20260730_182305_conv4.jsonl   run_20260730_182501_conv7.jsonl
run_20260730_182112_conv1.jsonl   run_20260730_182352_conv5.jsonl   run_20260730_182538_conv8.jsonl
run_20260730_182133_conv2.jsonl   run_20260730_182420_conv6.jsonl   run_20260730_182608_conv9.jsonl
```

（各自 summary 的 accuracy 加权后与 `run_distill_full_20260730_summary.json`
的 69.6% 一致，可先核验这个对应关系再跑。）

**做法**：`rejudge --input` 分别指向这 10 个文件（目录模式注意别把
`e1_v2_full/` 和无关 run 扫进去——建议先把这 10 个 jsonl 拷到
`results/v1_distill_full/` 再 rejudge），`--judge-style mem0`。

**验收**：`docs/benchmarks/locomo.md` E3 小节的 2×2 表补全 V1 行
（strict 69.6% / mem0 待填），judge 税按 prompt 版本分解。

---

## Task F6：E2 cutoff 三档实跑（~$6）

F3 只落了 CLI 脚手架。接受替代方案：3 次独立 `--topk N` 全量跑
（检索是本地确定性的，统计上与单检索多档切分等效；ingest 幂等不重蒸馏）。

**跑法**：`--topk 10`（已有 74.2%，勿重跑）、`--topk 20`、`--topk 50`，
V2 prompt + strict judge + 现有 `*_distill.db`。

**验收**：`docs/benchmarks/locomo.md` 加 cutoff 小节：overall + per-category
三档表；按判读标准下结论（50 档比 10 档 >5pp → 检索预算瓶颈，为实体层/
BM25 融合立项；持平 → 结构瓶颈，转向 P8 类工作）。
**报告时注明协议差异**：mem0 官方 top-200 是"单检索+切档"，我们是"三次
独立检索"，等效但不同构。

---

## Task F7：F2 表述修正 + E4 结果入文档（零成本，最高优先）

1. **修正基线对照**：F2 commit 的 "69.6% → 70.8% (+1.2pp)" 是跨检索栈比较
   （69.6% 是 P7/P8 之前的旧基线）。同栈（P7+P8）V1 合成 ≈75.4%，
   E4 V2 = 70.8% 是 **−4.6pp 回退**。commit message 改不了，但
   `docs/benchmarks/longmemeval.md` 必须写对。
2. **E4 小节写入** `docs/benchmarks/longmemeval.md`：per-type 表
   （V1 vs V2 × 6 题型）+ 结论——**LME 保留 V1 为默认**，V2 对 LME 的
   precision 题型有害（temporal −12.5pp、preference −20pp：inclusion
   check 过度包含、temporal grounding 与 LME 日期格式冲突）。LoCoMo 与
   LME 的 prompt 分派规则显式写明（harness 级、按 benchmark 分派，
   不按题型分派——V2 在 LME 没有任何题型赢到值得例外的程度：
   ssa +1.8pp 不足覆盖 temporal 损失）。
3. **补 summary 字段**：E4 的 `run_20260731_152928_summary.json` 缺
   `prompt_version`——给 LME harness 补该字段（后续 run 自动带），
   已落盘的 summary 手工补写 `"prompt_version": "v2"` 并注明是补录。

---

## Task F8：cat5 修复验证跑（~$2）

cat5 守卫（`main.rs:1056/1119` 的 `qa.category != 5`）已进代码但未验证。

**跑法**：`--prompt-version v2 --categories 5` 全对话单类目跑（446 题，
成本远低于全量）。**预期**：回到 V1 的 ~91.9%（88.3% + 修复的 ~3.6pp）。
若显著低于 91.9%，说明 F4 归因不完整，回 failure analysis 重查
（第二嫌疑：`ANSWER:` 解析路径）。

**验收**：cat5 数字更新进 `docs/benchmarks/locomo.md` E1 小节
（标注 "V2 + cat5 排序守卫"），overall 合成数相应更新。

---

## 执行顺序

| 序 | 任务 | 成本 | 产出 |
|---|---|---|---|
| 1 | F7 表述修正 + E4 入文档 | 0 | 叙事诚实性（先堵嘴） |
| 2 | F5 V1 rejudge | <$1 | 完整 2×2 judge 税矩阵 |
| 3 | F8 cat5 验证 | ~$2 | cat5 回归确认 |
| 4 | F6 cutoff 两档 | ~$4 | 检索预算归因 |

**红线重申**：每任务独立 commit；summary 带 `prompt_version`/`judge_style`/
命令行；`crates/` 不动；对外头条数字用 strict judge 口径，mem0 口径只以
"J-score (mem0-compatible)" 并列。
