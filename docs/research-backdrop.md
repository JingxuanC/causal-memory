# Research Backdrop

> Papers and theoretical foundations that shaped `causal-memory`.
> This is not a bibliography — it's a map of which ideas ended up in which design decisions.

---

## 1. The Core Thesis: LLM is a Stateless Function

**Reference**: [insights/09-stateless-function](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)

The starting point: every LLM inference call starts from scratch. Context is assembled, fed, then discarded. This means **memory is not a feature — it's a mandatory injection layer**. Causal memory is one specific injection strategy optimized for decision→outcome links.

---

## 2. Why Causal? The Compaction Degradation Evidence

**Paper**: [papers/02-compaction-degradation](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)

Real-LLM benchmark (using grok-build's production compaction prompt):

| Compactions (k) | Textual recall | Causal-table recall |
|---|---|---|
| 1 | 100% | 100% |
| 2 | 85% | 100% |
| 3 | 55% | 100% |
| 5 | **45%** | **100%** |

Key finding: **causal information (C-class) decays faster than expected under text compaction**. After k=5, textual recall is below 50%. The causal table survives because it lives outside the compaction pipeline.

---

## 3. Neuroscience: Hippocampus as Causal Inference Engine

**Kumaran, D., Hassabis, D., & McClelland, J. L. (2016).** *"What Learning Systems are Intelligent? Complementary Learning Systems Theory Updated."* Trends in Cognitive Sciences, 20(7), 512-534.

**Why it matters**: CLS theory explains the dual-system architecture we use:
- **Hippocampus** = fast, sparse, episodic (our `causal_edges` table)
- **Neocortex** = slow, statistical, semantic (future: `meta_causal_edges` for cross-task patterns)

The hippocampus is not just a storage device — it's a **causal inference engine** that extracts structure from discrete events via statistical learning. This is exactly what `record_decision` does: each call is a "causal sample" fed into the graph.

---

## 4. Neuroscience: Compressed Replay and Offline Consolidation

**Schapiro, A. C., et al. (2017).** *"The Hippocampus is Necessary for Disambiguating Temporal Sequences."* Hippocampus, 27(11), 1123-1133.

**Diekelmann, S. & Born, J. (2010).** *"The memory function of sleep."* Nature Reviews Neuroscience 11, 114–126.

**Why it matters**: The brain doesn't consolidate memories in real-time. It does **compressed replay during sleep** — reactivating experience sequences in fast-forward to evaluate causal weights and resolve ambiguities.

**Engineering implication**: `causal-memory` v0.2 is real-time only. v0.4+ should introduce an **offline consolidation cycle**:
- Active phase: accumulate raw causal edges
- Consolidation phase: replay recent experience, detect contradictions, merge redundant chains, update `meta_causal_edges`
- This maps directly to the "sleep" mechanism proposed in [insights/05-agi-7x24](https://github.com/JingxuanC/agent-teardown/blob/main/insights/05-agi-7x24.md)

---

## 5. Cognitive Science: Causal Graph Theory

**Sloman, S. A. (2005).** *"Causal Models: How People Think About the World and Its Alternatives."* Oxford University Press.

**Why it matters**: Humans use **directed acyclic graphs (DAGs)** as the default representation for causal knowledge. This is not a metaphor — it's the actual cognitive format. The `causal_edges` table is a flattened DAG edge list.

Sloman's work also supports **intervention reasoning** (Pearl's do-calculus): "If I do X, what happens to Y?" This is the theoretical foundation for future `trace_cause_chain` extensions — not just "what caused Y?" but "what would have happened if I hadn't done X?"

---

## 6. Cognitive Science: Counterfactual Simulation

**Gerstenberg, T., et al. (2021).** *"Counterfactual Simulation in Human Cognition."* Science, 373(6568), 1428-1431.

**Why it matters**: Humans determine causal responsibility by running **counterfactual simulations** — "if I hadn't done X, would Y still have happened?" This is computationally expensive but cognitively natural.

**Engineering implication**: The `trace_cause_chain` tool is a partial implementation of this. Full counterfactual support would require:
- Storing "alternative decisions" at each node (decision forks)
- Evaluating which fork would have prevented the outcome
- This maps to `meta_causal_edges` with `contradicts` relation type (v0.3+ roadmap)

---

## 7. Cognitive Science: Reconstructive Memory

**Schacter, D. L., & Addis, D. R. (2007).** *"The Cognitive Neuroscience of Constructive Memory: Remembering the Past and Imagining the Future."* Philosophical Transactions of the Royal Society B, 362(1481), 773-786.

**Why it matters**: Memory is not playback — it's **reconstruction**. The hippocampus stores "construction blueprints," not raw footage. Every retrieval is a reassembly based on current goals.

**Engineering implication**: This is the theoretical basis for **reconstructive retrieval** (roadmap v1.1+). Instead of returning raw `CausalEntry` records, the system could:
1. Retrieve relevant causal subgraph
2. Feed it to a lightweight LLM layer
3. Generate a coherent "lessons learned" narrative tailored to the current query context

This is more token-efficient and more cognitively natural than dumping raw edges into context.

---

## 8. Neuroscience: Temporal Contiguity Heuristic

**Davachi, L. (2006).** *"Item, Context and Relational Episodic Encoding in Humans."* Current Opinion in Neurobiology, 16(6), 693-700.

**Why it matters**: The brain defaults to "A happened before B, therefore A caused B" (temporal contiguity). This is a **heuristic, not a fact**.

**Engineering implication**: Our confidence levels encode this explicitly:
- `temporal` = 0.4 (weak — just happened in sequence)
- `rule` = 0.7 (strong — matches known causal pattern)
- `user_feedback` = 0.95 (gold standard — human confirmed)

This prevents the system from over-weighting spurious temporal correlations.

---

## 9. AI: Memory Framework Survey

**Wang, L., et al. (2024).** *"A Survey on Large Language Model based Autonomous Agents."* Frontiers of Computer Science, 18(6), 186345.

**Park, J. S., et al. (2023).** *"Generative Agents: Interactive Simulacra of Human Behavior."* UIST 2023.

**Why it matters**: The survey confirms that **current LLM Agent memory systems are almost entirely retrieval-augmented (RAG) paradigms** — none store causal relationships as first-class citizens.

Generative Agents (Stanford Town) has the closest architecture with memory streams + reflection, but their "reflection" is coarse-grained — not decision-level causal links. This is the **market gap** causal-memory fills.

---

## 10. AI: System 2 Deep Learning and Explicit Causal Representation

**Goyal, A., & Bengio, Y. (2022).** *"Inductive Biases for Deep Learning of Higher-Level Cognition."* Proceedings of the Royal Society A, 478(2266), 20210068.

**Why it matters**: Bengio argues that System 2 cognition (planning, causal reasoning, abstraction) requires **explicit object-relation-rule representations**, not end-to-end implicit encoding.

Causal-memory is an implementation of this principle: instead of hoping the LLM "learns" causality from context, we **externalize causal structure into an explicit graph** that the LLM can query, traverse, and reason about.

---

## Reading Order (if you want to follow our reasoning)

1. Start with [insights/09](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md) — the "LLM is stateless" premise
2. Read [papers/02](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md) — the empirical evidence that causal info is fragile
3. Read [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md) — the causal state store design this repo implements
4. Then pick any paper from this list — they're all connected to specific design decisions

---

*This document is a living artifact. As we implement v0.3+ features (meta-causal edges, offline consolidation, reconstructive retrieval), we will update this map with the papers that shaped those decisions.*
