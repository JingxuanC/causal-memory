# Computational AI / Agent Memory

> How the AI field handles (or fails to handle) memory, and where the gap is.

---

## Papers in this section

| Paper | Year | Core concept | `causal-memory` design it shaped |
|---|---|---|---|
| [Wang et al. — Agent Memory Survey](agent-memory-survey.md) | 2024 | Survey of LLM-based autonomous agents | Market gap identification: no causal memory layer |
| [Park et al. — Generative Agents](generative-agents.md) | 2023 | Multi-day agent simulation with memory streams | Memory stream + reflection as closest precedent |
| [Goyal & Bengio — System 2 Inductive Biases](system2-explicit-representation.md) | 2022 | System 2 cognition needs explicit representations | Externalizing causal structure into explicit graph |
| [Hermes Agent — memory provider ecosystem](hermes-provider-ecosystem.md) | 2026 | First agent runtime with a first-class memory plugin slot (ecosystem analysis, not a paper) | Distribution channel identification; hybrid recall-mode design; PyO3 reprioritization |
| [Rung-3 Prior Art survey](rung3-prior-art.md) | 2026 | Counterfactual reasoning: papers + engineering between us and Pearl's third rung | The five-phase Rung-3 plan (context snapshots → fork edges → micro-SCM → simulation → executable replay + prediction ledger) |

---

## The big picture

The AI field has built sophisticated memory systems for agents — but they are almost entirely **retrieval-augmented (RAG) or flat key-value stores**. None treat causal relationships as first-class citizens.

This section documents the gap. It is not a criticism of existing work (Mem0, Zep, Letta are excellent at what they do). It started as a **boundary definition**; as of v0.9+ it reads as a **market map** — causal-memory is growing into a complete memory system (see the roadmap's "From slice to system" direction), and the Hermes provider slot is its most concrete distribution channel.
