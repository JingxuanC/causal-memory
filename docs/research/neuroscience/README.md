# Neuroscience

> How the brain handles memory, causality, and consolidation — and what we stole from it.

---

## Papers in this section

| Paper | Year | Core concept | `causal-memory` design it shaped |
|---|---|---|---|
| [Kumaran et al. — CLS Theory](cls-theory.md) | 2016 | Dual-system memory (fast episodic + slow semantic) | `causal_edges` as episodic; `meta_causal_edges` as semantic |
| [Schapiro et al. — Hippocampal Time](hippocampus-temporal.md) | 2017 | Compressed replay resolves temporal ambiguity | Offline consolidation cycle on v0.4+ roadmap |
| [Davachi — Temporal Contiguity](temporal-contiguity.md) | 2006 | Time-adjacent events are treated as causally linked | Confidence levels: `temporal` = 0.4 (weak) |
| [Diekelmann & Born — Sleep Consolidation](sleep-consolidation.md) | 2010 | Memory replay during sleep strengthens causal links | "Sleep" phase: replay + contradiction detection |

---

## The big picture

The brain does not treat memory as a filing cabinet. It treats it as a **causal inference system**:

- The **hippocampus** rapidly encodes discrete events (decision → outcome pairs)
- **Offline replay** during rest/sleep re-evaluates which events are actually causally linked
- The **prefrontal cortex** maintains competing causal hypotheses and resolves conflicts

`causal-memory` v0.2 implements only the first step (hippocampal encoding). The neuroscience papers in this section justify why the next steps (consolidation, replay, conflict resolution) are not optional features — they are **necessary for a functional causal memory system**.
