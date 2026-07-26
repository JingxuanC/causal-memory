# Kumaran, Hassabis & McClelland (2016) — Complementary Learning Systems Theory

## Full Citation

Kumaran, D., Hassabis, D., & McClelland, J. L. (2016). What Learning Systems are Intelligent? Complementary Learning Systems Theory Updated. *Trends in Cognitive Sciences*, 20(7), 512–534. https://doi.org/10.1016/j.tics.2016.05.004

## Abstract

This paper updates the influential Complementary Learning Systems (CLS) theory, originally proposed by McClelland, McNaughton, and O'Reilly (1995). CLS posits that the brain contains two distinct but interacting memory systems:

- The **hippocampal system**: rapid, one-shot learning of arbitrary associations; sparse, pattern-separated representations; supports episodic memory and episodic future thinking.
- The **neocortical system**: slow, statistical learning of structured regularities; overlapping, distributed representations; supports semantic memory, concepts, and generalization.

The 2016 update integrates recent findings on replay, pattern completion, and the role of the hippocampus in imagination and planning.

## Methodology

The paper is a **theoretical review**, not an empirical study. It synthesizes:
- Human neuropsychology (amnesic patients with hippocampal damage)
- Rodent electrophysiology (place cells, replay, sharp-wave ripples)
- Human fMRI studies of memory consolidation
- Computational modeling (connectionist models of catastrophic interference)

The core methodology is **theoretical unification**: showing that disparate empirical phenomena (replay, imagination, transfer learning) can be explained by a single dual-system architecture.

## Key Findings

### 1. The hippocampus is not just a storage device — it's a causal inference engine

> "The hippocampus extracts the statistical structure of events and experiences, enabling the formation of causal models of the environment." (p. 518)

This reframes the hippocampus from "where memories are stored" to "where causal structure is inferred from sparse samples." Each episodic memory is a **data point** for causal learning.

### 2. Replay is not consolidation — it's re-evaluation

Traditional view: replay transfers memories from hippocampus to cortex.
Updated view: replay **re-evaluates** the causal and predictive structure of experiences. It resolves ambiguities ("was A→B or B→A?") and detects higher-order patterns.

### 3. Imagination and memory share the same neural substrate

The hippocampus is active both during memory retrieval and future simulation. This suggests that "remembering the past" and "imagining the future" are computationally equivalent — both involve **constructing coherent narratives from stored components**.

## Methodology Critique

| Strength | Limitation |
|---|---|
| Unifies decades of disparate findings | Lacks quantitative predictions; hard to falsify |
| Grounded in multiple empirical domains (human, rodent, computational) | Some claims (e.g., "causal model formation") are inferential, not directly observed |
| Successfully explains why hippocampal damage impairs both memory and imagination | Does not specify the algorithmic mechanism of causal inference |

## Connection to `causal-memory`

### Direct mapping

| CLS component | `causal-memory` equivalent | Status |
|---|---|---|
| Hippocampal fast learning | `causal_edges` table (real-time `record_decision`) | ✅ v0.2 implemented |
| Neocortical slow learning | `meta_causal_edges` table (cross-task pattern mining) | 🔄 v0.3 planned |
| Offline replay | Consolidation cycle: replay + contradiction detection | 🔄 v0.4 planned |
| Pattern completion | `trace_cause` (partial cue → full causal episode) | ✅ v0.2 implemented |
| Imagination/simulation | `trace_cause_chain` + counterfactual queries | 🔄 v0.5 planned |

### Design decision driven by this paper

The **dual-table schema** (`causal_edges` + `meta_causal_edges`) is not an implementation convenience — it's a direct architectural copy of the CLS dual-system. We deliberately separate:
- **Fast, lossless episodic storage** (hippocampus → `causal_edges`)
- **Slow, structured semantic abstraction** (neocortex → `meta_causal_edges`)

This also explains why we **refuse to compact `causal_edges`**: the hippocampus does not compress its episodic traces during initial encoding. Compression happens later, during replay-mediated consolidation — and only the **generalized structure** (not the raw episodes) moves to the semantic system.

## Reading order

**Read this first** if you want to understand why `causal-memory` has two tables instead of one.

---

*Last updated: 2026-07-27*
