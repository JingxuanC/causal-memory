# Design

> Why causal-memory exists, what design choices it makes, and the theory behind it.

## The problem

LLM agents are [stateless functions](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md). Every inference call starts from scratch with whatever context was assembled. After N compactions:

- They forget **why** they made past decisions
- They relearn the same lessons
- They repeat the same mistakes

Mem0, Zep, and Letta all address aspects of this problem — but the founding diagnosis was narrower and sharper: the link between "I decided X" and "X caused Y" is the most fragile type of information under text compaction. That diagnosis remains true, and it turned out to be the right **beachhead rather than the whole system**: the causal layer is now the core of a complete memory system (see [README — From slice to system](../README.md#how-its-different)).

## The empirical evidence

Real benchmark using grok-build's production compaction prompt (the 9-section Structured template), with a real LLM doing the compression:

| Compactions (k) | Textual recall | Causal-table recall | Gap |
|---|---|---|---|
| 1 | 100% | 100% | 0 |
| 2 | 85% | 100% | **15pp** |
| 3 | 55% | 100% | **45pp** |
| 5 | 45% | 100% | **55pp** |

At k=5, textual recall is below 50% — the agent has lost most of its experience. But the causal table is still at 100%, because it's outside the compaction pipeline.

**Full benchmark writeup with real LLM outputs**: [bench-RESULTS.md](https://github.com/JingxuanC/agent-teardown/blob/main/spike/grok-causal-memory/bench-RESULTS.md)

## The design

### What we store

Two tables that started as an overlay on any existing memory system — now the core of a standalone store:

```sql
-- Causal edges: decision → outcome
CREATE TABLE causal_edges (
    from_id TEXT,           -- decision chunk ID
    to_id TEXT,             -- outcome chunk ID
    relation TEXT,          -- 'caused' | 'enabled' | 'prevented' | 'no_effect'
    confidence REAL,        -- 0.0-1.0
    discovered_by TEXT,     -- 'temporal' | 'rule' | 'llm_inferred' | 'user_feedback'
    task_tag TEXT,          -- task category for retrieval
    ...
);

-- Meta causal edges: decision → decision (cross-task patterns)
CREATE TABLE meta_causal_edges (...);
```

### Why we don't store full text in the causal table

The decision and outcome **text** lives in a regular `chunks` table (could be Mem0, could be plain SQLite). The causal_edges table only stores the **relationship** + metadata. This is deliberate:

- Text can be compacted (it's fine — text comes back via search when needed)
- The causal **link** cannot be compacted (that's the whole point)
- Keeping text out of causal_edges keeps the table small and fast

### Confidence levels

Not all causal links are equally certain. Per [insights/11 §3 step three](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md):

| Level | Source | Default confidence | Use when |
|---|---|---|---|
| `temporal` | Time-adjacent events | 0.4 | Quick heuristic, weak evidence |
| `rule` | Pattern matching | 0.7 | "edit → test fail" style rules |
| `llm_inferred` | LLM judgment | 0.6 | Scaled default for most cases |
| `user_feedback` | User explicitly confirmed | 0.95 | Gold standard, sparse |

Search returns results ordered by confidence — high-confidence lessons surface first.

### Why 13 tools (and no more)

The MCP surface is intentionally minimal — every tool must earn its place by
covering a distinct moment in the agent's decision loop:

- `record_decision` — write path (after action); also runs the contradiction
  short-circuit that auto-invalidates falsified older edges
- `search_causal` — read path (before action)
- `record_fact` — fact write path: stable "what is" information
  (preferences / tech stack / config), idempotent on (key, value, scope)
- `search_facts` — fact read path: semantic/BM25 fact retrieval
- `search_memory` — unified read path: RRF-fused facts + causal lessons when
  the agent doesn't know which layer holds the answer
- `trace_cause` — diagnostic path, single hop (after failure)
- `trace_cause_chain` — diagnostic path, multi-hop root cause
- `invalidate_decision` — correction path (a recorded lesson turns out wrong)
- `search_patterns` — meta path: cross-task lessons distilled by the offline
  miner, not raw episodes
- `causal_directory` — L0 path: a compact pointer list pinned in the system
  prompt so the agent knows what experience it holds without searching
- `intervention_query` — Pearl Rung-2 path: predict the likely outcome of an
  action *before* taking it, from similar past actions
- `counterfactual_query` — comparison path: choosing *between two concrete
  options*, weigh their recorded track records (contrastive/empirical, not
  SCM counterfactual)
- `reconstruct_lesson` — distillation path: turn a past episode into a
  *transferable* narrative lesson instead of raw records

More tools would mean more decisions for the agent: "which tool do I call?" That cognitive overhead reduces usage. Per [insights/14 §2.2](https://github.com/JingxuanC/agent-teardown/blob/main/insights/14-on-deep-digging.md): "complete-looking is the enemy of depth" — and in tool design, "feature-rich is the enemy of used." Each of the thirteen maps to one unambiguous call moment; anything further would overlap.

### Why MCP

- **Composability**: any MCP-compatible agent can mount this (Claude Code, Cursor, grok-build, LangGraph via MCP adapter)
- **Non-replacing**: doesn't compete with the agent's built-in memory; adds the causal layer alongside
- **Isolation**: separate process, separate DB, no entanglement with the agent's state

This is the same pattern Mem0's OpenMemory uses — memory-as-MCP-tool. We differ in **what** we store (causal edges, not flat facts).

## What we don't do (and why)

| Don't | Why |
|---|---|
| Keep fact/state memory in a separate store | No longer out of scope — the system is growing to cover factual and temporal memory natively on the same causal-graph skeleton (see README "From slice to system"); the exclusivity that remains is *how*, not *what*: one typed-edge graph and one hippocampus-style engine instead of per-type stores |
| Modify agent's context | We're a side table, not a context replacement |
| Rung-3 **SCM** counterfactuals | Structural-causal-model reasoning is practically impossible for agents ([insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)); we ship only the contrastive/empirical subset (`counterfactual_query` over recorded alternatives) |

## Mechanisms added since v0.4

### Schema migrations

The store version-marks the DB with `PRAGMA user_version` and migrates
idempotently in a single transaction (`src/migrate.rs`, current: v4). A
`table_info` probe handles pre-marker v0.6 DBs; legacy columns (e.g.
`created_at`) are backfilled into the temporal columns and dropped. Opening
any older DB upgrades it automatically; `causal-memory migrate` runs an
explicit check.

### Temporal validity & invalidation

Every edge carries `event_time` (when it happened), `discovered_at` (when we
learned it), and `valid_to` (NULL = still valid). Invalidation is always
soft: the edge disappears from search/trace but stays in the DB for audit.
Two paths set it: the `invalidate_decision` tool (manual correction), and the
contradiction short-circuit inside `record_decision` — when a new outcome for
the same decision falsifies an existing edge (rule-based polarity check), the
old edge is invalidated automatically.

### Dual-system memory (meta layer activated)

`meta_causal_edges` existed since v0.1 but was empty until the offline
pattern miner (`src/patterns.rs`) activated it: similar decisions across
tasks are distilled into `similar_to` / `repeated` / `contradicts` /
`refines` meta edges (Jaccard similarity over tokenized decision text +
outcome polarity). Raw `causal_edges` are the fast episodic system; meta
edges are the slow semantic system — the agent's "abstracted lessons",
queryable via `search_patterns`.

### Sleep consolidation

An offline cycle (`src/consolidate.rs`, `causal-memory sleep`) modeled on the
memory-consolidation literature, in four phases: **reactivation** (score
edges for replay priority — failures and user feedback first),
**generalization** (merge duplicates, run the pattern miner), **downscaling**
(exponential confidence decay by age, access-based boost, garbage collection
of sub-threshold edges; `user_feedback` edges are never GC'd), and **REM
integration** (link similar patterns across disjoint task tags). Designed to
run once per day; not idempotent by design.

### L0 causal directory

`causal_directory` returns a compact one-line-pointer list of recent
decisions, meant to be pinned in the agent's system prompt. It answers "what
experience do I hold?" at zero search cost; the pointer texts feed
`trace_cause` / `search_causal` / `intervention_query` for full details.

### Rung-2 intervention queries

`intervention_query` is Pearl Rung 2 ("doing", not "seeing"): before taking
an action, it looks up outcomes of similar past actions and returns predicted
effects with causal paths and confidence, labeled safe / warning / danger,
with a task_tag-stratified adjustment that warns when the pooled estimate is
confounded. Rung 3 in the SCM sense stays out of scope (see the table above);
the contrastive/empirical subset ships as `counterfactual_query`.

### Semantic retrieval with keyword fallback

Causal retrieval is still task + confidence first. But when an
OpenAI-compatible embedding endpoint is configured (`CAUSAL_MEMORY_EMBED_*`,
falling back to `CAUSAL_MEMORY_LLM_*`), edges get vector embeddings and
`search_causal` ranks by cosine similarity (`src/embed.rs`). Unconfigured →
silent fallback to **BM25** (`src/bm25.rs`: Okapi BM25, k1=1.2, b=0.75,
Robertson IDF, per-task IDF scope, pure Rust, zero dependencies — replaced
plain LIKE after the LoCoMo benchmark showed substring matching capped
evidence hit rate at ~60%; BM25 lifts it to ~74%). This mirrors the
LLM-judge contract: zero-invasive default, capability upgrade when
configured.

### Write-time outcome polarity (LLM judge + heuristic fallback)

The signal-word polarity heuristic has a documented quirk: when failure and
success signals co-occur, success wins. That is right for contradiction
detection but wrong for intervention labels — "deadlock under load; fixed by
switching to channels" is not a SAFE outcome for the decision that caused the
deadlock. Since v4, polarity is judged **once at write time**
(`llm::judge_polarity` when an LLM is configured, else the heuristic) and
persisted on the edge (`outcome_polarity`: positive / negative / mixed /
neutral; NULL for legacy rows). The new **mixed** category covers compound
outcomes instead of forcing them into positive/negative. Read paths only read
the column: `intervention_query` labels mixed chains `⚠️ WARNING` instead of a
misleading `✅ SAFE`, and the contradiction short-circuit prefers stored
polarity under a conservative rule — only negative-old + positive-new
auto-invalidates, mixed/neutral never trigger on either side. NULL polarity
everywhere falls back to the exact pre-v4 heuristic behavior;
`causal-memory polarity` backfills legacy rows on demand.

### Stratified causal discovery (engineering CI test)

The pattern miner originally promoted any similar decision pair to a meta
edge — including patterns that only hold inside one task_tag (a classic
confound). Since v5, candidate pairs are grouped by their shared
decision-token signature and a pattern is promoted at full confidence only
when it holds in ≥ 2 distinct strata (a stratified replication test — the
honest engineering stand-in for PC-style conditional independence, Spirtes
2000). Single-stratum patterns are kept but marked `confounded` at half
confidence ("only seen in task_tag=X, may be domain-specific"), and groups
whose outcome direction flips between strata are flagged `simpson`. Re-mining
re-tests and upgrades/downgrades existing conclusions.

### Stratified intervention queries

`intervention_query` pooled all similar past actions into one confidence —
but task_tag is an obvious confounder (the same action can have opposite
outcome distributions in different task types). The handler now reports the
terminal-outcome distribution per stratum alongside the pooled one (stored
polarity first, heuristic fallback) and prints an explicit Simpson's-paradox
warning when the pooled majority and the stratum majority disagree ("pooled
estimate likely confounded"). An optional `task_tag` parameter restricts the
displayed chains to one stratum.

### Write-time replay consolidation

Sleep stage 1 (reactivation) used to be a pure report — scores computed,
printed, discarded. Replay is now re-evaluation, per Schapiro 2017: the
priority scores feed stage 3 (downscaling), where replay-protected edges
decay at half rate and use a lenient GC threshold (retention ∝ priority ×
recency × confidence, not age alone), and replayed edges are marked via
`last_accessed_at` after downscaling — so the next cycle sees them as
recently accessed, closing a replay → consolidate → survive feedback loop.
Recently accessed edges also earn a replay-priority bonus, and a
`recently accessed` reason appears in the top-N report.

### Contrastive counterfactuals & reconstructive retrieval

`counterfactual_query` implements the honest engineering subset of Rung 3:
given a decision and an alternative, it retrieves recorded episodes similar
to each (semantic seeding with LIKE fallback) and compares their outcome
distributions — every output carries the disclaimer that this is a
contrastive/empirical counterfactual, not an SCM one. `reconstruct_lesson`
implements Schacter-style reconstructive retrieval: it fetches the Markov
blanket around seeded edges (parents, children, co-parents, size-capped) as
compact stubs, lets an LLM reconstruct a coherent lesson narrative from them,
and optionally runs multi-sample calibration (N independent reconstructions;
low token-Jaccard agreement flags unreliable memories, the engineering form
of insights/13 §2.5 multi-agent calibration).

### Cross-agent sharing & reproducible benchmarking

`causal-memory export` / `import` share causal memory between agents
(insights/11 §8.5): a versioned JSONL format (chunks + edges + meta edges),
best-effort secret redaction on export, and idempotent import keyed on
content — (from_text, to_text, relation, event_time) for edges, FNV-1a(text)
for chunk ids — so cross-database ids never collide and re-importing is a
no-op. `causal-memory bench-compaction` turns the papers/02 compaction
experiment into a public harness: a seeded deterministic scenario generator,
an independent session per compression depth, keyword-scored gold QA (no
LLM judge in the loop), and a markdown report replicating the paper's table.

## Theoretical foundation

This project is the engineering output of 13 research notes. The key ones:

1. **[insights/09](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)** — LLM is a stateless function. Memory = retrieval + injection. Causal memory is one specific injection strategy.
2. **[insights/10](https://github.com/JingxuanC/agent-teardown/blob/main/insights/10-memory-frameworks.md)** — Survey of 8 memory projects. None store causal relationships as first-class citizens. This is the market gap.
3. **[insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)** — The causal state store design (this implementation).
4. **[papers/02](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)** — Real LLM benchmark proving causal info decays fastest under text compaction.
