# Design

> Why causal-memory exists, what design choices it makes, and the theory behind it.

## The problem

LLM agents are [stateless functions](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md). Every inference call starts from scratch with whatever context was assembled. After N compactions:

- They forget **why** they made past decisions
- They relearn the same lessons
- They repeat the same mistakes

This is not a memory problem in general — Mem0, Zep, Letta all address aspects of it. It's specifically a **causal memory** problem: the link between "I decided X" and "X caused Y" is the most fragile type of information under text compaction.

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

Two tables on top of any existing memory system:

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

### Why 8 tools (and no more)

The MCP surface is intentionally minimal — every tool must earn its place by
covering a distinct moment in the agent's decision loop:

- `record_decision` — write path (after action); also runs the contradiction
  short-circuit that auto-invalidates falsified older edges
- `search_causal` — read path (before action)
- `trace_cause` — diagnostic path, single hop (after failure)
- `trace_cause_chain` — diagnostic path, multi-hop root cause
- `invalidate_decision` — correction path (a recorded lesson turns out wrong)
- `search_patterns` — meta path: cross-task lessons distilled by the offline
  miner, not raw episodes
- `causal_directory` — L0 path: a compact pointer list pinned in the system
  prompt so the agent knows what experience it holds without searching
- `intervention_query` — Pearl Rung-2 path: predict the likely outcome of an
  action *before* taking it, from similar past actions

More tools would mean more decisions for the agent: "which tool do I call?" That cognitive overhead reduces usage. Per [insights/14 §2.2](https://github.com/JingxuanC/agent-teardown/blob/main/insights/14-on-deep-digging.md): "complete-looking is the enemy of depth" — and in tool design, "feature-rich is the enemy of used." Each of the eight maps to one unambiguous call moment; anything further would overlap.

### Why MCP

- **Composability**: any MCP-compatible agent can mount this (Claude Code, Cursor, grok-build, LangGraph via MCP adapter)
- **Non-replacing**: doesn't compete with the agent's built-in memory; adds the causal layer alongside
- **Isolation**: separate process, separate DB, no entanglement with the agent's state

This is the same pattern Mem0's OpenMemory uses — memory-as-MCP-tool. We differ in **what** we store (causal edges, not flat facts).

## What we don't do (and why)

| Don't | Why |
|---|---|
| Replace Mem0/Zep/Letta | Causal memory is **complementary** to flat/temporal/self-managed memory |
| Modify agent's context | We're a side table, not a context replacement |
| Rung-3 counterfactuals | Practically impossible for agents ([insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)); we only prepare the data structures |

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
effects with causal paths and confidence, labeled safe / warning / danger.
Rung 3 (counterfactuals) stays out of scope — see the table above.

### Semantic retrieval with keyword fallback

Causal retrieval is still task + confidence first. But when an
OpenAI-compatible embedding endpoint is configured (`CAUSAL_MEMORY_EMBED_*`,
falling back to `CAUSAL_MEMORY_LLM_*`), edges get vector embeddings and
`search_causal` ranks by cosine similarity (`src/embed.rs`). Unconfigured →
silent fallback to keyword LIKE. This mirrors the LLM-judge contract:
zero-invasive default, capability upgrade when configured.

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

## Theoretical foundation

This project is the engineering output of 13 research notes. The key ones:

1. **[insights/09](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)** — LLM is a stateless function. Memory = retrieval + injection. Causal memory is one specific injection strategy.
2. **[insights/10](https://github.com/JingxuanC/agent-teardown/blob/main/insights/10-memory-frameworks.md)** — Survey of 8 memory projects. None store causal relationships as first-class citizens. This is the market gap.
3. **[insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)** — The causal state store design (this implementation).
4. **[papers/02](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)** — Real LLM benchmark proving causal info decays fastest under text compaction.
