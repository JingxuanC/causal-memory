# Causal Inference

> The formal foundations of causal reasoning — from Pearl's ladder of causation to automated causal discovery.

---

## Papers in this section

| Paper | Year | Core concept | `causal-memory` design it shaped |
|---|---|---|---|
| [Pearl — Causality](pearl-causality.md) | 2009 | The ladder of causation: association → intervention → counterfactual | Schema design (`caused`/`enabled`/`prevented`); roadmap for intervention queries |
| [Spirtes et al. — PC Algorithm](pc-algorithm.md) | 2000 | Automated causal discovery from observational data | `meta_causal_edges` mining: automatic pattern extraction from causal graph |

---

## The big picture

Causal inference is a mature mathematical field with well-defined formalisms (Bayesian networks, do-calculus, structural causal models). `causal-memory` v0.2 implements only the most basic layer (associational retrieval). The papers in this section define the **formal target** — what a fully-featured causal memory system should eventually support.

The **ladder of causation** (Pearl) provides the roadmap:

| Rung | Name | Question type | `causal-memory` support |
|---|---|---|---|
| 1 | Association | "What is?" | ✅ `search_causal` (correlational) |
| 2 | Intervention | "What if I do?" | 🔄 v0.5: `counterfactual_query` |
| 3 | Counterfactual | "What if I had done?" | 🔄 v0.5: full counterfactual reasoning |

We are currently at Rung 1. The papers in this section explain why Rungs 2 and 3 are the natural next steps.
