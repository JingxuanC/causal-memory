# Architecture

> The current system at a glance: three write channels, two memory layers in
> one SQLite store, one fused retrieval path. For the theory see
> [`design.md`](design.md); for the build plan see
> [`unified-memory-design.md`](unified-memory-design.md) (Phases 1–4, all landed).

## The map

```
        WRITE (three channels)                    STORE (SQLite, schema v7)
┌───────────────────────────────┐
│ raw ingest                    │── one chunk per turn ──────────▶ ┌──────────────┐
│   (every turn, verbatim)      │── adjacent-turn "caused" edges ─▶│ chunks       │
├───────────────────────────────┤                                  │ (raw turns)  │
│ distill (1 LLM call/session)  │                                  └──────────────┘
│   routes each item by kind:   │   Fact / Preference ───────────▶ ┌──────────────┐
│                               │     record_fact                  │ agent_facts  │ ◀─┐
│                               │     (scoped: user/session/agent  │ (fact layer) │   │
│                               │      or namespaced lme:<qid>)    └──────────────┘   │
│                               │                                  soft-invalidate,   │ supersedes
│                               │   Lesson / Event ──────────────▶ ┌──────────────┐   │ retire-before-record
│                               │     record_distilled             │ causal_edges │   │ (old value retired,
│                               │     (decision → outcome edges)   │ (causal)     │   │  new value written;
├───────────────────────────────┤                                  └──────────────┘   │  retrieval never sees
│ MCP direct writes (13 tools)  │── record_decision / record_fact ──────────────────────┘  stale values)
│   (agent-authored at runtime) │    replace_same_key = atomic swap
└───────────────────────────────┘
   heavy sessions also dual-write raw turns (quantitative detail survives distillation);
   distill_done markers make interrupted runs resumable (all-failed units stay unmarked)

        RETRIEVE (two paths, one fusion)          ANSWER
┌───────────────────────────────┐
│ search_facts_bm25             │── fact hits (retired excluded) ─┐
│   (scope-filtered)            │                                  ▼
├───────────────────────────────┤                        ┌──────────────────┐    facts FIRST,
│ search_causal_bm25            │── causal hits ────────▶│ search_memory    │──▶ then causal
│   (task_tag hard isolation)   │                        │ RRF fusion k=60  │    memory lines
└───────────────────────────────┘                        └──────────────────┘
                                                                  │
 hippocampus: typed spreading activation over the same graph      ▼
 (prevented edges spread NEGATIVE activation — inhibitory         answer prompt
  spread, unique to this system); consolidation modeled on
  sharp-wave ripples, not text distillation
```

## Why one store instead of per-type stores

Mem0/Zep-style systems keep a separate store per memory type (vector store +
graph store + profile store) and glue them at the agent layer. causal-memory
puts both layers in **one SQLite file on one skeleton**:

- facts and causal edges share scope/tag conventions, so retrieval isolation
  is uniform (`scope` for facts, `task_tag` for edges);
- the supersedes mechanism works across both: a superseded fact is
  soft-invalidated in `agent_facts`, a superseded lesson spawns a negation
  record in `causal_edges`;
- one BM25 index family, one optional embedding index per layer, one fusion
  point (`search_memory` RRF k=60);
- the whole memory is one file: copyable, diffable, compaction-proof by
  construction (it lives outside the context window).

## What each layer is for

| Layer | Holds | Answers | Exclusive mechanism |
|---|---|---|---|
| `chunks` | raw turns, verbatim | quantitative detail, verbatim quotes | dual-write fallback for heavy sessions |
| `agent_facts` | atomic facts/preferences ("user uses pnpm") | factual recall, knowledge-update, preference | supersedes retirement (retire-before-record) |
| `causal_edges` | decision→outcome, caused/enabled/prevented | why something happened, what changed, lessons | `prevented` negative spread; compaction survival |

## Measured effect of the split (same harness, same judge, frozen protocols)

| Benchmark | raw-only | distill + fact layer | Δ |
|---|---|---|---|
| LoCoMo (1,986 q) | 64.2% | 69.6% | +5.4pp |
| LongMemEval-S (500 q) | 61.8% | 69.6% | +7.8pp |
| Memora weekly (MPA, 10 personas) | 33.9% | 46.8% | +12.9pp |

Gains concentrate where atomic facts should help (temporal +11.6pp,
knowledge-update +9.0pp, preference +13.4pp); abstention and saturated
categories are untouched. Honest bottlenecks and per-category tables:
[`docs/benchmarks/`](../benchmarks/).

## Known frontier (not yet built)

- **multi-session synthesis is the weakest slice** (LongMemEval 41.4%).
  Diagnosis from our own run data: full evidence coverage yields 64.7%
  accuracy vs 26.8% on partial coverage — the bottleneck is evidence-set
  completeness, not reasoning. Planned: session expansion along `caused`
  edges (type-agnostic, zero LLM cost), then runtime query-topology
  analysis (the system infers how many evidence points a question needs —
  no benchmark-type labels at runtime).
- LoCoMo cat-1 list completeness and cat-3 counterfactual/abstention
  protocol mismatch (documented in `docs/benchmarks/locomo.md`).
- Memora FAA tightening: the retraction-record filter should extend to
  dual-written raw chunks.
