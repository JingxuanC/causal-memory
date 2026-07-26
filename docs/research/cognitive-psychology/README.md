# Cognitive Psychology

> How humans represent, reason about, and retrieve causal knowledge — and what we copied.

---

## Papers in this section

| Paper | Year | Core concept | `causal-memory` design it shaped |
|---|---|---|---|
| [Sloman — Causal Graph Theory](causal-graph-theory.md) | 2005 | Humans use DAGs as default causal representation | `causal_edges` schema = flattened DAG edge list |
| [Gerstenberg et al. — Counterfactual Simulation](counterfactual-simulation.md) | 2021 | Causal judgment via "what if I hadn't done X?" | `trace_cause_chain` as partial counterfactual implementation |
| [Schacter & Addis — Reconstructive Memory](reconstructive-memory.md) | 2007 | Memory is reconstruction, not playback | Reconstructive retrieval on v1.1+ roadmap |

---

## The big picture

Cognitive psychology reveals that human causal reasoning is **not** logical deduction applied to a database of facts. Instead, it is:

- **Graph-structured**: we think in cause→effect networks, not flat associations
- **Counterfactual**: we determine causality by imagining alternatives
- **Constructive**: every retrieval rebuilds the memory from components

`causal-memory` v0.2 implements the graph structure (DAG edge list). The cognitive psychology papers in this section justify why counterfactual reasoning and reconstructive retrieval are the next necessary layers.
