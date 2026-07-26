# Research Backdrop

> Papers and theoretical foundations that shaped `causal-memory`.
> This is not a bibliography — it's a map of which ideas ended up in which design decisions.

For the **full systematic research documentation** (BibTeX, detailed abstracts, methodology critiques, and per-paper design traces), see [`docs/research/`](research/) — organized by theme:

- [`neuroscience/`](research/neuroscience/) — how the brain handles memory, causality, and consolidation
- [`cognitive-psychology/`](research/cognitive-psychology/) — how humans represent and reason about causal knowledge
- [`computational-ai/`](research/computational-ai/) — what the AI field does and where the causal memory gap is
- [`causal-inference/`](research/causal-inference/) — formal foundations (Pearl, Spirtes)

This page provides a **quick-reference summary**. For depth, follow the links above.

---

## 1. The Core Thesis: LLM is a Stateless Function

**Reference**: [insights/09-stateless-function](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)

Every LLM inference call starts from scratch. Memory is not a feature — it's a mandatory injection layer. Causal memory is one specific injection strategy optimized for decision→outcome links.

---

## 2. Why Causal? The Compaction Degradation Evidence

**Paper**: [papers/02-compaction-degradation](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)

Real-LLM benchmark (grok-build's production compaction prompt):

| Compactions (k) | Textual recall | Causal-table recall |
|---|---|---|
| 1 | 100% | 100% |
| 2 | 85% | 100% |
| 3 | 55% | 100% |
| 5 | **45%** | **100%** |

Key finding: **causal information decays faster than expected under text compaction**. The causal table survives because it lives outside the compaction pipeline.

---

## 3. Neuroscience

### Kumaran, Hassabis & McClelland (2016) — CLS Theory

**Key idea**: The brain has two memory systems — hippocampus (fast, episodic) and neocortex (slow, semantic).

**Design connection**: Our dual-table schema (`causal_edges` + `meta_causal_edges`) directly copies this architecture. We refuse to compact `causal_edges` because the hippocampus does not compress episodic traces during initial encoding.

**Deep dive**: [`neuroscience/cls-theory.md`](research/neuroscience/cls-theory.md)

### Schapiro et al. (2017) — Hippocampal Replay

**Key idea**: The hippocampus resolves temporal ambiguity via **compressed replay** during rest — not faithful playback, but structured re-evaluation.

**Design connection**: v0.4 offline consolidation cycle ("sleep") is directly inspired by this. Replay detects contradictions, merges redundant chains, and updates `meta_causal_edges`.

**Deep dive**: [`neuroscience/hippocampus-temporal.md`](research/neuroscience/hippocampus-temporal.md)

### Davachi (2006) — Temporal Contiguity

**Key idea**: The brain defaults to "A happened before B, therefore A caused B" — a heuristic, not a fact.

**Design connection**: Our confidence levels encode this explicitly: `temporal` = 0.4 (weak), `rule` = 0.7 (strong), `user_feedback` = 0.95 (gold standard). This prevents over-weighting spurious temporal correlations.

**Deep dive**: [`neuroscience/temporal-contiguity.md`](research/neuroscience/temporal-contiguity.md)

### Diekelmann & Born (2010) — Sleep Consolidation

**Key idea**: Sleep actively transforms memory via selective reactivation, gist extraction, and synaptic down-selection.

**Design connection**: The v0.4 consolidation cycle includes: reactivation (priority queue), generalization (meta_causal_edges), and down-selection (confidence decay + garbage collection).

**Deep dive**: [`neuroscience/sleep-consolidation.md`](research/neuroscience/sleep-consolidation.md)

---

## 4. Cognitive Psychology

### Sloman (2005) — Causal Graph Theory

**Key idea**: Humans use **directed acyclic graphs (DAGs)** as the default representational format for causal knowledge.

**Design connection**: `causal_edges` is a flattened DAG edge list. The `relation` types (`caused`, `enabled`, `prevented`, `no_effect`) encode structural constraints that causal models must satisfy.

**Deep dive**: [`cognitive-psychology/causal-graph-theory.md`](research/cognitive-psychology/causal-graph-theory.md)

### Gerstenberg et al. (2021) — Counterfactual Simulation

**Key idea**: Humans determine causal responsibility by running **mental simulations** of counterfactual worlds — "if I hadn't done X, would Y still have happened?"

**Design connection**: `trace_cause_chain` is a partial implementation of this. Future v0.5+ could add full counterfactual queries ("if I had used channels instead of mutexes...").

**Deep dive**: [`cognitive-psychology/counterfactual-simulation.md`](research/cognitive-psychology/counterfactual-simulation.md)

### Schacter & Addis (2007) — Reconstructive Memory

**Key idea**: Memory is not playback — it's **reconstruction**. The hippocampus stores "construction blueprints," not raw footage. Every retrieval reassembles stored components.

**Design connection**: This is the theoretical basis for **reconstructive retrieval** (v1.1+). Instead of returning raw edges, the system retrieves a causal subgraph and generates a coherent "lessons learned" narrative.

**Deep dive**: [`cognitive-psychology/reconstructive-memory.md`](research/cognitive-psychology/reconstructive-memory.md)

---

## 5. Computational AI

### Wang et al. (2024) — Agent Memory Survey

**Key idea**: Current LLM agent memory systems are almost entirely RAG-based. **None store causal relationships as a primary data structure.**

**Design connection**: This is our primary evidence that causal memory is a genuine market gap, not a feature that existing systems "just haven't gotten around to."

**Deep dive**: [`computational-ai/agent-memory-survey.md`](research/computational-ai/agent-memory-survey.md)

### Park et al. (2023) — Generative Agents

**Key idea**: Persistent memory + periodic reflection enables emergent social behavior. But reflection is coarse-grained (text summaries), not decision-level causal links.

**Design connection**: Generative Agents is the closest precedent. We extend it by making reflection structured (`meta_causal_edges`) and causal (`causal_edges`).

**Deep dive**: [`computational-ai/generative-agents.md`](research/computational-ai/generative-agents.md)

### Goyal & Bengio (2022) — System 2 Inductive Biases

**Key idea**: System 2 cognition (planning, causal reasoning) requires **explicit object-relation-rule representations**, not end-to-end implicit encoding.

**Design connection**: `causal-memory` is an implementation of this principle. Instead of hoping the LLM "learns" causality, we externalize causal structure into an explicit graph.

**Deep dive**: [`computational-ai/system2-explicit-representation.md`](research/computational-ai/system2-explicit-representation.md)

---

## 6. Causal Inference (Formal Foundations)

### Pearl (2009) — Causality

**Key idea**: The **ladder of causation** — three levels: association (seeing), intervention (doing), counterfactual (imagining). Each strictly more powerful.

**Design connection**: v0.2 is Rung 1 (`search_causal`). v0.5 roadmap includes Rung 2 (intervention queries) and Rung 3 (counterfactual reasoning). Pearl provides the formal target.

**Deep dive**: [`causal-inference/pearl-causality.md`](research/causal-inference/pearl-causality.md)

### Spirtes, Glymour & Scheines (2000) — PC Algorithm

**Key idea**: Automated causal discovery from observational data via conditional independence testing.

**Design connection**: The v0.3 `meta_causal_edges` activation is inspired by PC. We will mine cross-task patterns from accumulated causal edges using similar constraint-based methods.

**Deep dive**: [`causal-inference/pc-algorithm.md`](research/causal-inference/pc-algorithm.md)

---

## BibTeX

All papers: [`docs/research/references.bib`](research/references.bib)

```bash
# Import into Zotero
zotero docs/research/references.bib
```

---

## Reading Order

1. Start with [`insights/09`](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md) — the "LLM is stateless" premise
2. Read [`papers/02`](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md) — the empirical evidence
3. Read [`insights/11`](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md) — the design this implements
4. Then explore [`docs/research/`](research/) by theme — each paper is connected to a specific design decision

---

*This document is a living artifact. As we implement v0.3+ features, we update the research map with the papers that shaped those decisions.*
