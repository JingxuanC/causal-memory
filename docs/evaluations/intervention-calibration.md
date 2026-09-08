# Intervention Query 校准评估（intervention_calibration）

日期：2026-09-09
测试：`crates/causal-memory/tests/intervention_calibration.rs`（`cargo test --test intervention_calibration -- --nocapture`）
方法：10 个合成世界（SplitMix64 seed=7），每世界 4 类 ground-truth 查询，注入真实 store（`CausalStore::open_in_memory` + `record_decision_full` 显式 polarity），走完整 `intervention_query` 路径（embedding→BM25→LIKE 三级种子回退），解析输出标签。

这是方案文档 1.3 节的 CMB Layer-2 对标实验，回答的问题：*`intervention_query` 的 SAFE / WARNING / DANGER / UNKNOWN 标签，在已知 ground truth 下准确率多少？*

## 结果

| Ground-truth 类 | 准确率 | 说明 |
|---|---|---|
| causal_danger（真因果链 → 负面结果） | **10/10 (100%)** | DANGER 全部命中，含 2-hop 链 |
| causal_safe（真因果链 → 正面结果） | **10/10 (100%)** | SAFE 全部命中 |
| prevented（阻止边 → 负面结果被拦截） | **10/10 (100%)** | UNKNOWN 全部命中（chain_label 的 has_prevented 分支） |
| confounded（潜变量混淆，伪 caused 边） | **0/10（DANGER 过声称为 100%）** | 已知结构性缺陷，见下 |

## 核心发现

### 1. 可解类完美，混淆类 100% 过声称——分界线在图外

三个可解类 100% 准确，说明查询层本身的链恢复、polarity 传播、prevented 语义都是对的。混淆类的失败是**结构性**的：

- 系统 relation 词表只有 caused/enabled/prevented（`distill.rs`），**没有观察性/关联性 relation**——每条入库边都是因果断言；
- 链遍历（`store/retrieve/trace.rs` 的递归 CTE）**relation 盲视**，对所有有效边一视同仁；
- 因此抽取器把混淆观察升级为 "caused" 后，查询层无法区分；潜变量根本不在图里，任何下游机制都看不见它。

**修复方向在上游**：(a) 抽取器对混淆观察用独立 relation 或降置信度；(b) refuter 层的混淆标注（backdoor refuter 已在半潜变量场景可用）传递到查询层。修复前，回归守卫把基线钉在 ≤100%，出现部分修复时收紧。

### 2. 种子交叉匹配是真实风险（基线调试中发现）

初版校准器让世界内四个用例共享世界 token，BM25 种子把每个查询都锚到世界内所有决策（40/40 全部 DANGER）。改为每用例独立词表后恢复正常。含义：生产中查询与历史决策共享常见 token 时，`intervention_query` 可能召回不相关链——置信度排序（chain_confidence）部分缓解，但**summary 层看到的是全部链的 pooled 分布**，与查询的相关性没有保证。这与 1.2 refuter 校准的"按密度门控"是同一类教训：证据相关性需要在聚合层显式建模。

## 回归守卫

- causal_danger recall ≥ 80%（实测 100%）
- causal_safe 准确率 ≥ 80%（实测 100%）
- prevented → UNKNOWN ≥ 80%（实测 100%）
- confounded DANGER 过声称 ≤ 100%（实测 100%，已知缺陷基线，修复后收紧）

## 复现

```bash
cd /Users/hjx/project/causal-memory
cargo test --test intervention_calibration -- --nocapture
```
