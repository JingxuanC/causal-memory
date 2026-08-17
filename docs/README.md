# Documentation Map

> Start here. Everything under `docs/` organized by what you're looking for.

| Looking for… | Go to |
|---|---|
| What this project is, quick start, benchmarks | [README.md](../README.md) |
| How to activate the MCP tools in your agent | [CLAUDE.md](../CLAUDE.md) |
| Version history | [CHANGELOG.md](../CHANGELOG.md) |
| Where the project is headed | [roadmap.md](roadmap.md) |

## Layout

```
docs/
├── design/        Architecture & algorithm design docs
├── benchmarks/    Benchmark harnesses, protocols & results
├── evaluations/   Audit reports, experiment logs, optimization plans
├── paper/         Paper drafts, outline, review & LaTeX source
└── research/      Literature survey (notes, references.bib, 中文翻译)
```

## design/ — how it works

| Doc | What it covers |
|---|---|
| [design.md](design/design.md) | Original founding design: the causal-layer beachhead diagnosis |
| [complete-memory-system.md](design/complete-memory-system.md) | 完整记忆系统架构 — one graph, one engine, one loop (the full system) |
| [unified-memory-design.md](design/unified-memory-design.md) | 三层统一记忆架构 — facts + causal + unified retrieval (RRF) |
| [architecture.md](design/architecture.md) | Component-level architecture (MCP server, store, engine) |
| [algorithm-design.md](design/algorithm-design.md) | Algorithm formalization: gated propagation + online learning + graph sparsification |
| [hippocampus-design.md](design/hippocampus-design.md) | Hippocampus engine: CSR spreading activation, DG/CA3/CA1, SWR |
| [refutation-design.md](design/refutation-design.md) | Causal-edge refutation: real-time confidence scoring (A/B/C/D/F) for LLM-extracted edges |
| [multi-hop-expansion.md](design/multi-hop-expansion.md) | Multi-hop / open-domain retrieval via graph expansion |

## benchmarks/ — measuring it

| Doc | What it covers |
|---|---|
| [causal-eval-2026.md](benchmarks/causal-eval-2026.md) | **CausalEval** — our graph-grounded causal memory benchmark (primary) |
| [locomo.md](benchmarks/locomo.md) | LoCoMo fact-recall results & optimization matrix |
| [longmemeval.md](benchmarks/longmemeval.md) | LongMemEval results |
| [memora.md](benchmarks/memora.md) | Memora weekly persona results |
| [tau2-airline.md](benchmarks/tau2-airline.md) | τ²-bench airline behavioral A/B |
| [amc-2026.md](benchmarks/amc-2026.md) | Agent Memory Challenge (AMC/01) submission details |

## evaluations/ — what we found

| Doc | What it covers |
|---|---|
| [edge-accuracy-audit.md](evaluations/edge-accuracy-audit.md) | Causal-edge labeling accuracy audit (83% agreement, 0 severe) |
| [performance-audit-2026-08.md](evaluations/performance-audit-2026-08.md) | Performance & precision audit |
| [optimization-plan-2026-08.md](evaluations/optimization-plan-2026-08.md) | 2026-08 optimization plan |
| [code-review-retrieval.md](evaluations/code-review-retrieval.md) | Retrieval & write-path code review (2026-08-01) |
| [p8-p9-experiments.md](evaluations/p8-p9-experiments.md) | P8/P9 实验设计 |
| [mem0-eval-alignment.md](evaluations/mem0-eval-alignment.md) | mem0 评测对齐设计与实施 |
| [mem0-eval-followup.md](evaluations/mem0-eval-followup.md) | mem0 对齐后续任务 (F1-F4) |
| [mem0-eval-final.md](evaluations/mem0-eval-final.md) | mem0 对齐收尾 (F5-F8) |
| [insights-didi-memory-2026.md](evaluations/insights-didi-memory-2026.md) | DiDi Darwinian Memory + UltraHorizon research notes |

## paper/ — writing it up

| Doc | What it covers |
|---|---|
| [paper-outline.md](paper/paper-outline.md) | Paper outline |
| [paper-full-draft.md](paper/paper-full-draft.md) | Full draft |
| [paper-section4-experiments.md](paper/paper-section4-experiments.md) | Section 4 (experiments) |
| [paper-review.md](paper/paper-review.md) | Nature-style reviewer assessment |
| [paper-latex/](paper/paper-latex/) | LaTeX source |

## research/ — background

Literature survey across causal inference, neuroscience, cognitive psychology,
and computational AI. See [research/README.md](research/README.md) for the index,
[design-lineage.md](research/design-lineage.md) for how the papers map to design
decisions, and [references.bib](research/references.bib) for the bibliography.
中文翻译在 [zh/](research/zh/)。

Project-level research backdrop: [research-backdrop.md](research-backdrop.md)
([中文](research-backdrop.zh.md)).
