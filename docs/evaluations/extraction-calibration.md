# Extraction Calibration — 抽取器端到端校准（hardening §3.1）

**回答的问题**：提取的因果关系准确吗？——自然语言对话 → `remember`（Distiller/LLM 抽取）→ 因果边，这一段链路的准确率。现有的 `refuter_calibration.rs`（图级 refuter）和 `intervention_calibration.rs`（intervention_query）都是**绕过抽取直接灌边**，本 harness 补齐被它们跳过的那一段。

## 方法

```
seeded 生成（SplitMix64）           注入                    评分
5 类 episode，ground truth 已知  →  真实 remember 路径  →  抽取边 vs ground truth
模板+槽位池，中英各半，2-6 轮        （Distiller + LLM，       模糊匹配 + 混淆矩阵
                                    tempdir 独立 store）
```

**五类 episode**（比例 30/15/15/20/20）：

| 类 | ground truth relation | 考核点 |
|---|---|---|
| `caused` | `caused` | 基本抽取（40% 为决策本身导致坏结果的负向变体） |
| `prevented` | `prevented` | 预防型 |
| `enabled` | `enabled` | 使能型 |
| `no_effect` | 无边 | **伪边率**——结果由明示的其它原因造成，正确行为是不产边 |
| `confounded` | `co_occurrence` | **过声称率**——共同原因同时驱动动作和结果（"leader 同时要求加缓存和扩容"），正确抽取是 PR #28 的 `co_occurrence` 而非 `caused`。§1.3 校准曾发现此类 100% DANGER 过声称，这里是它的端到端回归表 |

**决策文本唯一性**：生成器保证跨 episode 决策文本唯一（槽位池 + 冲突时追加"第 N 轮"区分符）——相同文本会触发 v9 chunk 复用 + 矛盾短路（`invalidate_contradicted_edges`），悄悄毁掉别的 episode 的 ground truth 边。

**匹配**：抽取边 ↔ episode 用归一化包含 或 char-bigram Dice ≥ 0.5（bigram 对中文友好，词 token 对中文失效）；全局贪心指派（每边至多归一个 episode），防止同模板兄弟 episode 互相"偷边"。

**指标**：抽取召回率、relation 混淆矩阵（头条：confounded→caused 过声称率）、no_effect 伪边率、polarity 准确率（live 模式 remember 路径存 NULL polarity，polarity 仅 selftest 可评）。

## CLI

```bash
# 零 LLM：生成 episodes JSONL
cargo run --bin causal-memory-extraction-eval -- gen --episodes 120 --seed 42 --out episodes.jsonl

# 零 LLM：全链路自检（mock 抽取器注入已知错误率，断言评分器精确恢复）
cargo run --bin causal-memory-extraction-eval -- selftest --episodes 300 --seed 42

# live：真实 Distiller 抽取（需 DEEPSEEK_API_KEY 或 CAUSAL_MEMORY_LLM_API+KEY）
cargo run --bin causal-memory-extraction-eval -- run --episodes 100 --seed 42 --out benches/extraction_eval/results

# 可选：LLM 改写增强自然度 + 回验 ground truth 保留（causal_eval narrate 思路）
cargo run --bin causal-memory-extraction-eval -- gen --episodes 120 --seed 42 --narrate-llm
```

## selftest 验证结果（2026-10-02，无 LLM 环境）

`selftest --episodes 300 --seed 42`：mock 注入 miss=15% / rel_err=10% / confounded_overclaim=40% / noeffect_spurious=20%，评分器**逐 episode 精确恢复**注入判定（300/300 verdict 一致），聚合率落在注入旋钮的采样噪声内：

```
extraction recall:   83.5% (207/248 causal episodes)     ← 注入 miss 15%
confounded→caused overclaim: 39.7% (27/68)               ← 注入 40%
no_effect false-edge rate:   19.2%                        ← 注入 20%
polarity accuracy:   100.0% (217/217)                     ← mock 不注入 polarity 错误
```

`--episodes 100`（默认 seed 42）同样全绿。这验证了：生成 → 真实 store 写入 → 读回 → 模糊匹配 → 混淆矩阵，全链路在无 LLM 环境下可复现。

## live run 待办

当前环境无 LLM key，live run 未执行。拿到 key 后跑 `run --episodes 100 --seed 42`，关注点：

1. **confounded→caused 过声称率**是否显著低于 §1.3 图层的 100% 基线——即抽取器 prompt（distill.rs 已含 co_occurrence 第 4 类）是否真的被 LLM 用对；
2. no_effect 伪边率（记忆系统最昂贵的失败模式）；
3. 结果 JSONL 在 `<out>/run_<ts>.jsonl`，汇总在 `<out>/run_<ts>.summary.json`。
