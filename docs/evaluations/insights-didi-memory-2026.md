# DiDi memory research — Darwinian Memory + UltraHorizon

> Two ICML 2026 papers from DiDi L-Lab (Hongze Mi, Naiqiang Tan, Haotian Luo et al.)
> recorded as external corroboration for causal-memory's two core architectural theses:
> "memory as an ecosystem" and "context is the failure surface — not just forgetting,
> but locking." Date: 2026-08-06.

## 1. Darwinian Memory (arXiv:2601.22528, ICML 2026)

**Problem**: MLLM GUI agents doing long-horizon cross-application tasks hit three
bottlenecks: limited context windows; granularity mismatch (high-level intent vs
low-level execution); and **context pollution** — static accumulation of outdated
experiences drives agents into hallucination.

**Method (DMS)**:

| Component | What it does | causal-memory analogue |
|---|---|---|
| Memory decomposition | Trajectories → independent reusable atomic units | distill: decision → causal edge units |
| **Utility-driven natural selection** | Track survival value (frequency, recency, reliability) → prune suboptimal paths, inhibit high-risk plans | **Q-value dynamics + SWR consolidation** (LTP strengthen / LTD weaken / triple-criterion GC) |
| Inhibiting high-risk plans | Selection pressure suppresses bad strategies | **prevented edges spread −0.3 activation** (GABA analogue) |

**Results**: training-free, +18.0% success rate, +33.9% execution stability, lower
latency, on real-world multi-app (GUI agent) benchmarks.

**Assessment**: validates the "memory as an ecosystem under selection pressure"
direction with a lightweight, untyped design. causal-memory's typed causal graph +
bidirectional (excitatory/inhibitory) activation is the more structured version of
the same idea. Notably, DMS has **no dedicated memory benchmark** — it is evaluated
on existing GUI agent tasks.

## 2. UltraHorizon (arXiv:2509.21766)

**What**: a benchmark for ultra long-horizon, partially observable agent tasks.
Exploration as a unifying task across three environments: agents must iteratively
uncover hidden rules via sustained reasoning, planning, **memory management**, and
tool use. Trajectories average 200k+ tokens / 400+ tool calls (heaviest setting),
35k+ tokens / 60+ calls (standard).

**Key findings**:

1. LLM agents consistently underperform humans on long-horizon tasks.
2. **Simple scaling fails** — bigger models don't fix the gap.
3. Trajectory analysis: 8 error types from **two root causes**:
   - **in-context locking**: the agent is locked into early context — initial
     wrong assumptions or stale environment state cannot be overwritten by new
     evidence. Not "forgetting" — the opposite: "remembered and can't revise."
   - functional capability gaps (reasoning/planning/tool use).

**Relation to causal-memory**: in-context locking is the mirror image of
compaction loss — the second context failure mode:

| Failure mode | Symptom | causal-memory countermeasure |
|---|---|---|
| Compaction loss | Context compression forgets causality | causal table lives outside the context window (never compacted; 5× compaction: 45% text recall vs 100% causal recall) |
| **In-context locking** | Old conclusions in context can't be revised | **supersede / reversible retirement** (`superseded_by` + `restore_edge`) — the memory system explicitly unlocks stale conclusions so retrieval stops surfacing them |

The paper's "scaling fails" conclusion corroborates the architectural thesis:
the gap is memory architecture, not model parameters.

## 3. Positioning note — the memory-benchmark vacuum

| Who | Memory system | Own memory benchmark? |
|---|---|---|
| DiDi L-Lab (DMS) | ✅ untyped utility-selection | ❌ (evaluated on generic GUI tasks) |
| mem0 | ✅ (add/search API) | ⚠️ memory-benchmarks (system-bound) |
| causal-memory | ✅ typed causal graph | ✅ LoCoMo / LongMemEval / Memora harnesses |
| AMC/01 (Agent Memory Challenge) | — (platform) | ✅ first standardized Add/Search memory leaderboard — causal-memory is a participant |

Big labs ship memory systems but nobody has established the evaluation standard —
the AMC entry (v0.9.0-amc1) is our position in that vacuum.
