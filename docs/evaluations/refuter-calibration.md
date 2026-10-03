# Refuter 校准评估（refuter_calibration）

日期：2026-09-08
测试：`crates/causal-memory/tests/refuter_calibration.rs`（`cargo test --test refuter_calibration -- --nocapture`）
方法：30 个合成 DAG 世界（SplitMix64 seed=42，6–13 节点，planted 社区结构），真边 = DAG 边，伪边 = 随机非邻接对（模拟抽取器幻觉）。伪边按 d-连通性分 hard（有前向路径或共同祖先）/ easy（d-分离）。

## 校准驱动的四处 refuter 修正

| # | 修正 | 动机（校准证据） |
|---|------|------------------|
| R1 | confounder：共享邻居 union < 4 时弃权（Inconclusive） | 稀疏图上的 Jaccard 无信息量，2/3 真边 intersection=0 被判 Refuted |
| R2 | corroboration：移除 "no alt path → Refuted" 分支，永不判否 | DAG 中最小因果链（X→Y→Z）天然零冗余路径，该分支误杀 38% 真边。"无旁证" ≠ "边为假" |
| R3 | placebo：placebo_rate > 0.8 时弃权（Y 是枢纽） | 稠密图中 Y 普遍可达，"谁到达 Y" 对 X→Y  claims 无信息，误杀 29% 真边 |
| R4 | backdoor：保留绝对计数阈值（≥2 条 → Refuted），否决了密度归一化（祖先到达比例）方案 | 归一化方案在稠密图误杀 38% 真边——稠密图给几乎所有节点对铺上祖先路径，比例同样无判别力 |

## 密度-判别力前沿（核心发现）

不同密度制度下（社区内/社区间成边概率），keep（真边 0 refuter 存活）与 flag（伪边 ≥1 refuter 命中）构成一条此消彼长的前沿：

| 制度 | keep（真） | flag（伪） | 问题 |
|------|-----------|-----------|------|
| 稀疏 34/8 | 80.6% | 27.2% | 全图弃权过多（union<4 保护了一切），伪边混入 |
| 中等 40/12 | 72.9% | 35.7% | **推荐工作点**；回归守卫设于此 |
| 稠密 60/20 | 50.7% | 55.4% | "替代解释存在"类测试对真/伪边无差别开火 |

**结论：纯图结构 refuter 存在 keep-flag 前沿，二者之和约 110%，无法同时推高。** 真边与伪边在纯结构特征上共享同一签名；密度只决定证据是否足以裁判，不改变可分离性。

## 回归守卫（写入测试断言）

- keep_rate > 65%（实测 72.9%）
- flag_rate > 30%（实测 35.7%）
- 真边 F 率（≥2 refuter 否决）< 8%（实测 1.9%）

这些是防回归守卫，不是性能目标。

## 对系统设计的建议

1. **结构 refuter 只应作为多证据融合的一层。** NodeData/EdgeData 中未利用的证据——event_time（时序先后）、q_value/replay_count（激活统计）、edge weight/relation 类型——在真实图里携带判别信息，而本基准给所有边相同语义特征（结构 refuter 能用的只有结构）。下一步：让校准器给真/伪边赋予可分离的语义特征（如真边时序一致、伪边时序颠倒），实现并校准一个时序/语义 refuter。
2. **按密度门控 refuter 启用**：稀疏图（union 普遍 < 4）上结构 refuter 输出应降权或跳过，避免伪精确。
3. **政策语义分级**：grade F（≥2 否决）→ 隔离；D（1 否决）→ 人工/LLM 复核队列；A/B/C → 保留。quarantine（伪边 F 率）在中等密度仅 ~3%，说明 F 是保守的安全网，真正的拦截要靠 D 级复核配合语义证据。
4. backdoor refuter 的判别力集中在 moderate 密度制度（hard 伪边带共同祖先的场景）；稠密图上建议关闭或仅作 Inconclusive 提示。

## 复现

```bash
cd /Users/hjx/project/causal-memory
cargo test --test refuter_calibration -- --nocapture   # 打印完整校准报告
```

改密度：测试内 `SynthWorld::generate(&mut rng, n, k, p_in, p_out)`。

## 后续：时序 refuter（多证据融合第一层，同日完成）

为验证"融合 NodeData/EdgeData 非结构证据可突破前沿"的推断，实现第 5 个 refuter **temporal**（`refute.rs`）并扩展校准器种植可分离的时序证据：

- **语义**：event_time(cause) > event_time(effect) → Refuted（时序上不可能）；一端无时间戳 → 弃权；同时刻 → 弃权（粗粒度记录不可判）；cause < effect → Robust。
- **校准器改动**：节点时间戳沿拓扑索引严格递增（+抖动），真边天然时序一致；伪边保持抽取器的原始随机方向（不排序），约一半时序颠倒。
- **结果**（中等密度 40/12 制度）：

| 指标 | 纯结构（4 refuter） | +时序（5 refuter） |
|------|--------------------|--------------------|
| keep（真边存活） | 72.9% | **71.3%**（基本持平） |
| flag（伪边命中 ≥1） | 35.7% | **64.3%** |
| quarantine（伪边 ≥2 否决） | 2.9% | 18.7% |
| 真边误杀（F） | 1.9% | 1.7%（300/300 真边时序 Robust） |

keep+flag 从 ~108% 提到 **135.6%**——前沿被打破，且真边零误伤。时序 refuter 的伪边 Refuted 率 146/305 ≈ 48%，与种植比例（约一半颠倒）吻合，说明它精确捕获了"效果先于原因"这一类伪边，不依赖图密度。

**推广**：同一模式可用于激活统计（真边两端 q_value/replay 相关）、边权一致性（真边 weight 显著高于社区基线）——凡是抽取器幻觉难以同时伪造的证据通道，都可以成为第 6、7 个 refuter。回归守卫已更新：flag > 55%、keep > 65%、真边 F < 8%。
