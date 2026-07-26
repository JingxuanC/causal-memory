# Schapiro et al. (2017) — Hippocampus and Temporal Sequence Disambiguation

## Full Citation

Schapiro, A. C., Turk-Browne, N. B., Botvinick, M. M., & Norman, K. A. (2017). Complementary Learning Systems within the Hippocampus: A Neural Network Modelling Approach to Reconciling Episodic Memory with Statistical Learning. *Philosophical Transactions of the Royal Society B*, 372(1711), 20160049. https://doi.org/10.1098/rstb.2016.0049

## Abstract

The hippocampus supports both episodic memory (remembering specific events) and statistical learning (extracting regularities across events). These functions appear contradictory: episodic memory requires pattern separation (distinct representations for similar events), while statistical learning requires pattern completion (generalization across similar events).

Schapiro et al. propose that the hippocampus contains **two complementary subsystems**:
- **Dentate gyrus / CA3**: pattern separation — creates distinct episodic codes
- **CA1 / subiculum**: pattern completion — generalizes across statistically similar events

Replay mechanisms arbitrate between these subsystems, allowing the hippocampus to simultaneously support specificity and generalization.

## Methodology

The paper combines:
1. **Computational modeling**: A neural network model of the hippocampus with explicit pattern separation (DG) and pattern completion (CA1) pathways.
2. **Human fMRI**: Participants learned temporal sequences with overlapping elements; hippocampal activity tracked both item-specific and statistical predictions.
3. **Rodent electrophysiology**: Review of replay studies showing that replay sequences often deviate from actual experience — suggesting replay is not faithful playback but **structured re-evaluation**.

## Key Findings

### 1. The hippocampus resolves temporal ambiguity via replay

When two events (A→B and A→C) share a common element (A), the hippocampus uses replay to "test" which continuation is more likely given the current context. This is **not retrieval** — it's **causal inference via simulation**.

### 2. Replay sequences are not faithful copies

Rodent replay often includes:
- **Reverse replay**: events played backward (evaluating consequences)
- **Novel sequences**: combinations never experienced (simulation)
- **Compressed time**: seconds of experience replayed in milliseconds

This proves replay is **generative**, not reproductive.

### 3. Pattern separation vs. completion is dynamically regulated

The hippocampus does not fix a separation/completion trade-off. Instead, it dynamically shifts based on:
- **Novelty**: novel events → more separation
- **Predictability**: predictable events → more completion
- **Reward**: high-stakes events → more separation (preserve detail)

## Methodology Critique

| Strength | Limitation |
|---|---|
| Computational model makes testable predictions | Model is simplified; omits many biological details |
| fMRI + modeling convergence strengthens claims | fMRI temporal resolution is too coarse to observe replay directly |
| Successfully reconciles two seemingly contradictory functions | Does not explain how the arbitration mechanism works algorithmically |

## Connection to `causal-memory`

### 1. Offline consolidation as "compressed replay"

The v0.4 roadmap includes an **offline consolidation cycle** inspired directly by this paper:

```
Active phase (16h):  agent operates, causal_edges accumulate
Consolidation phase (4h):  "replay" recent causal chains
  - Detect contradictions (same decision → opposite outcomes)
  - Merge redundant edges (A→B and A'→B' are actually the same pattern)
  - Elevate high-confidence patterns to meta_causal_edges
Deep maintenance (4h):  DB compaction, index optimization
```

This is not a metaphor. The paper shows that replay is **essential** for resolving ambiguities that cannot be resolved during real-time encoding.

### 2. Confidence-based separation/completion trade-off

Our confidence levels encode the same trade-off:
- **High confidence** (0.9+, user_feedback) → preserve detail (pattern separation)
- **Low confidence** (0.4, temporal) → generalize (pattern completion)

Future versions could dynamically adjust retrieval: high-stakes queries get high-separation retrieval (specific episodes); routine queries get high-completion retrieval (general patterns).

### 3. Reverse replay → `trace_cause_chain`

The paper's finding that rodents replay sequences **backward** to evaluate consequences is the biological basis for our multi-hop backward tracing. Reverse replay is nature's `trace_cause_chain`.

## Reading order

Read after Kumaran et al. (2016) CLS theory. This paper answers the question: "*How* does the hippocampus do causal inference?" (Answer: via replay-mediated arbitration between separation and completion.)

---

*Last updated: 2026-07-27*
