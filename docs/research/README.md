# Research

> Systematic documentation of the neuroscience, cognitive psychology, computational AI, and causal inference papers that shaped `causal-memory`.

This is not a bibliography. It is a **design trace**: each paper is mapped to a specific design decision in the codebase.

---

## Directory structure

```
docs/research/
├── README.md                 ← You are here
├── references.bib            ← BibTeX for all papers
├── neuroscience/             ← How the brain handles memory and causality
│   ├── README.md
│   ├── cls-theory.md         ← Kumaran 2016: dual-system memory
│   ├── hippocampus-temporal.md ← Schapiro 2017: replay and pattern completion
│   ├── temporal-contiguity.md ← Davachi 2006: time as causal heuristic
│   └── sleep-consolidation.md ← Diekelmann & Born 2010: offline consolidation
├── cognitive-psychology/     ← How humans represent and reason about causality
│   ├── README.md
│   ├── causal-graph-theory.md ← Sloman 2005: mental causal models
│   ├── counterfactual-simulation.md ← Gerstenberg 2021: simulation-based judgment
│   └── reconstructive-memory.md ← Schacter & Addis 2007: memory as reconstruction
├── computational-ai/         ← What the AI field does (and does not do)
│   ├── README.md
│   ├── agent-memory-survey.md ← Wang 2024: the causal memory gap
│   ├── generative-agents.md ← Park 2023: memory stream + reflection
│   ├── system2-explicit-representation.md ← Goyal & Bengio 2022: explicit structure
│   └── hermes-provider-ecosystem.md ← Hermes Agent 2026: memory provider slot ecosystem
└── causal-inference/         ← Formal foundations
    ├── README.md
    ├── pearl-causality.md    ← Pearl 2009: the ladder of causation
    └── pc-algorithm.md       ← Spirtes 2000: automated causal discovery
```

---

## How to read this

### If you want to understand the biological basis

Start with [`neuroscience/`](neuroscience/):
1. `cls-theory.md` — why two memory tables?
2. `sleep-consolidation.md` — why offline consolidation?
3. `hippocampus-temporal.md` — how does multi-hop tracing work biologically?

### If you want to understand the cognitive basis

Start with [`cognitive-psychology/`](cognitive-psychology/):
1. `causal-graph-theory.md` — why a graph structure?
2. `counterfactual-simulation.md` — why multi-hop tracing?
3. `reconstructive-memory.md` — why reconstructive retrieval (roadmap)?

### If you want to understand the market gap

Start with [`computational-ai/`](computational-ai/):
1. `agent-memory-survey.md` — what exists and what's missing
2. `generative-agents.md` — the closest precedent
3. `system2-explicit-representation.md` — why externalize causal structure?

### If you want the formal math

Start with [`causal-inference/`](causal-inference/):
1. `pearl-causality.md` — the ladder of causation (where we are, where we're going)
2. `pc-algorithm.md` — how to automatically discover patterns from causal data

---

## BibTeX

All papers are in [`references.bib`](references.bib). You can import it into any reference manager (Zotero, Mendeley, JabRef).

```bibtex
@article{kumaran2016cls, ...}
@book{sloman2005causal, ...}
@article{wang2024survey, ...}
@book{pearl2009causality, ...}
```

---

## Living document

This directory is a **living artifact**. As we implement v0.3+ features, we will add papers and update the design traces:

| Feature | Papers to add |
|---|---|
| Semantic/vector search | Mikolov et al. (word2vec), Reimers & Gurevych (Sentence-BERT) |
| Offline consolidation | Rasch & Born (2013) reactivation during sleep; Nadel et al. (2012) systems consolidation |
| Reconstructive retrieval | Bartlett (1932) Remembering; Conway & Pleydell-Pearce (2000) self-memory system |
| Cross-agent sharing | Hutchins (1995) distributed cognition; Ostrom (1990) governing the commons |

---

*Last updated: 2026-07-27*
