# Adversarial Injection Calibration — 对抗注入 harness（hardening §3.2）

**回答的问题**：系统能自我纠错吗？——故意注入 10% 伪因果边到真实 store，多轮演进中跟踪每条伪边的状态机（hidden → flagged → invalidated），测量检测率 / 误杀率 / 纠正延迟。

与 `tests/refuter_calibration.rs` 的区别：那个是**静态图级**（手工构图 + 单次 EdgeRefuter）；本 harness 是**动态端到端**——边活在真实 CausalStore 里，每轮跑真实机制链。

## 方法

```
seeded 生成（SplitMix64，planted-community DAG，移植自 refuter_calibration）
  → 写入真实 CausalStore（record_decision_full，唯一 event_time；
      chunk created_at 用 SQL 钉到拓扑时刻 = 校准时手工 NodeData 的等价物）
  → T=10 轮纠错循环，每轮：
      1. EdgeRefuter 五 refuter 全量扫（模拟定期健康检查）
      2. consolidate() 一个完整周期（Stage 5 BiasAudit 等）
      3. 反证到达：每条存活伪边 30%/轮 概率收到一条矛盾新边
         （同 decision 文本 + 正向 outcome → write-path 矛盾短路）
```

**零 LLM 可用性勘察结论**（哪些机制在无 key 环境真实可用）：

| 机制 | 零 LLM | 实际作用 |
|---|---|---|
| EdgeRefuter 五 refuter | ✅ 纯图/时序 | **只打 A–F 等级，不写库、不 invalidate**——advisory only |
| BiasAudit（consolidate Stage 5） | ✅ 纯统计 | 只标 `bias_flag` 审查队列，检索/衰减/GC 无视 |
| write-path 矛盾短路（exact-text + polarity） | ✅ | **唯一真正 invalidate 伪边的零 LLM 机制** |
| consolidate merge/GC/decay | ✅ | 本时间尺度上无效（见下） |
| resolve_supersessions（stage 1.7） | ❌ 需 LLM judge | 无 key 跳过，报告注明 |
| invalidate_semantic_contradictions | ❌ 需 embeddings | 无 key 静默跳过，报告注明 |

## 实测（seed 42，n=48/k=4/p_in=40/p_out=2，120 真边 + 12 伪边（8 hard / 4 easy），10 轮）

| 场景 | 检测率 easy/hard | 伪边纠正 | 失效延迟中位 | 真边误伤 |
|---|---|---|---|---|
| A: 反证 30%/轮 | 100% / 87.5% | 11/12（全部经矛盾短路） | flag 1 轮，invalidate 3 轮 | 4 条（3.3%，矛盾 collateral） |
| B: 反证 10%/轮 | 同上 | 8/12 | invalidate 5 轮 | 2 条 |
| C: 反证 50%/轮 | 同上 | 12/12 | invalidate 2 轮 | 5 条 |
| D: 无反证 | 同上 | **0/12** | — | 0 条 |

## 结论：系统能自我纠错吗？——诚实回答

1. **检测层：灵敏但无判别力地过敏，且纯属建议**。refuter 扫描第一轮就标了 92% 伪边（D/F），但代价是 **71% 真边也被标 D/F**（85/120，主要来自 backdoor——稠密累积图上共同祖先普遍存在——和 confounder 的跨社区 J=0）。F 级（quarantine 档）有 2.6× 判别力（伪 58% vs 真 22%），但：**refute 只输出报告，从不写库**——没有任何自动消费者。检测 ≠ 纠错。
2. **真正纠错的只有 write-path 矛盾短路**，且完全由后续反证驱动：有反证时 92–100% 伪边被 invalidate，中位延迟随反证速率从 5 轮降到 2 轮（B vs C 对照钉死）；**无反证时纠正率为 0**（场景 D）——结构性伪边（refuter 漏检的那 1 条）和已标记伪边都会永远留在库里。
3. **collateral 存在但有限**：矛盾规则按 decision 文本精确匹配，同节点的负向真边会被连带 invalidate（A 场景 4/120 = 3.3%）。语义上这接近"该决策的旧教训被新证据证伪"，属 C7 灰色地带，不是纯误杀。
4. **consolidate 在此时间尺度对伪边零作用**（10 天衰减 0.99^10 ≈ 0.90，远高于 GC 阈值 0.2；merge 未命中）；BiasAudit 在合成重复结构上误标真边（19–36 条 low_variance，同节点多条同极性出边），0 条伪边——它的设计目标是真实 agent 的重复决策，不是注入检测。
5. **LLM 依赖路径（stage 1.7 judge、语义矛盾）在本环境未参与**；有 key 时 stage 1.7 是第二个潜在纠错机制，待测。

**一句话**：系统的自我纠错 = 「矛盾短路 × 后续反证」，检测层（refuter/BiasAudit）目前只是人工审查队列的生产者。要让"系统能自我纠错"成立，缺的一环是把 refuter F 级 / bias_flag 接到自动处置路径上（这正是 §3.2 要暴露的缺口）。

## 复现

```bash
cargo run --bin causal-memory-adversarial-eval -- run       # 默认场景 A
cargo run --bin causal-memory-adversarial-eval -- run --evidence-pct 0   # 场景 D
cargo run --bin causal-memory-adversarial-eval -- selftest  # 零 LLM 断言电池
ADV_DEBUG=1 ... run                                         # round-1 逐 refuter 诊断
```

## selftest 断言（全部实测钉住）

- easy 检测率 ≥ hard（实测 100% ≥ 87.5%），easy ≥ 50%
- 30%/10 轮反证下矛盾路径纠正 ≥80% 伪边（实测 92%）
- consolidate 对真边零误杀；矛盾 collateral < 10% 真边
- 延迟随反证速率单调下降（50% 档中位 2 < 10% 档中位 5）
- 零反证场景：零矛盾纠正、零真边误伤（诚实阴性结果）
