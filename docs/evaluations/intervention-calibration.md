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

## 修复落地（2026-09-09 同日，分支 feat/intervention-query-hardening）

### Fix 1: co_occurrence relation 全链路

| 层 | 改动 |
|---|---|
| 抽取器（distill.rs） | CausalRelation 新增 `CoOccurrence` 变体；prompt 明确"机制不清或疑似共同原因时用 co_occurrence 替代 caused"；parse 兼容 associated/correlated 别名 |
| 存储（schema v16） | causal_edges.relation CHECK 拓宽（重建表迁移，INSERT SELECT 带 COALESCE 容忍旧库空列） |
| 查询（trace.rs） | 链遍历 CTE 排除 co_occurrence/no_effect——观察性关联不再进入 do() 式前向链 |
| 校准 | 新增 confounded_tagged 类：**0% DANGER 过声称**（混淆观察被正确标注时系统不再过声称） |

残留局限：抽取器若仍把混淆观察错标为 caused（confounded 类），过声称保持 100%——这部分信号不在图中，只能靠抽取器判别力或 refuter 层标注改进，守卫继续钉住基线。

### Fix 2: BM25 种子相关性门控

- 新增 `search_causal_bm25_gated`（相对分数线：0.3 × top），仅 intervention_query 种子回退使用；recall 导向的检索调用方保持原行为。
- 单元测试覆盖门控丢弃弱匹配；原基线"共享 token → 40/40 全 DANGER"场景在门控下不再发生。

## 回归守卫

- causal_danger recall ≥ 80%（实测 100%）
- causal_safe 准确率 ≥ 80%（实测 100%）
- prevented → UNKNOWN ≥ 80%（实测 100%）
- confounded（extractor 错标 caused）DANGER 过声称 ≤ 100%（实测 100%，已知缺陷基线）
- **confounded_tagged（extractor 正确标注 co_occurrence）DANGER 过声称 = 0%（硬守卫）**

## 复现

```bash
cd /Users/hjx/project/causal-memory
cargo test --test intervention_calibration -- --nocapture
```
