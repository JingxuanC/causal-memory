# Computational AI / Agent Memory

> How the AI field handles (or fails to handle) memory, and where the gap is.

---

## Papers in this section

| Paper | Year | Core concept | `causal-memory` design it shaped |
|---|---|---|---|
| [Wang et al. — Agent Memory Survey](agent-memory-survey.md) | 2024 | Survey of LLM-based autonomous agents | Market gap identification: no causal memory layer |
| [Park et al. — Generative Agents](generative-agents.md) | 2023 | Multi-day agent simulation with memory streams | Memory stream + reflection as closest precedent |
| [Goyal & Bengio — System 2 Inductive Biases](system2-explicit-representation.md) | 2022 | System 2 cognition needs explicit representations | Externalizing causal structure into explicit graph |

---

## The big picture

The AI field has built sophisticated memory systems for agents — but they are almost entirely **retrieval-augmented (RAG) or flat key-value stores**. None treat causal relationships as first-class citizens.

This section documents the gap. It is not a criticism of existing work (Mem0, Zep, Letta are excellent at what they do). It is a **boundary definition**: causal-memory does not compete with these systems; it fills a hole they do not address.
