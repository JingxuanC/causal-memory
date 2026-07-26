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

### Why 3 tools (not more)

The MCP surface is intentionally minimal:

- `record_decision` — write path (after action)
- `search_causal` — read path (before action)
- `trace_cause` — diagnostic path (after failure)

More tools would mean more decisions for the agent: "which tool do I call?" That cognitive overhead reduces usage. Per [insights/14 §2.2](https://github.com/JingxuanC/agent-teardown/blob/main/insights/14-on-deep-digging.md): "complete-looking is the enemy of depth" — and in tool design, "feature-rich is the enemy of used."

### Why MCP

- **Composability**: any MCP-compatible agent can mount this (Claude Code, Cursor, grok-build, LangGraph via MCP adapter)
- **Non-replacing**: doesn't compete with the agent's built-in memory; adds the causal layer alongside
- **Isolation**: separate process, separate DB, no entanglement with the agent's state

This is the same pattern Mem0's OpenMemory uses — memory-as-MCP-tool. We differ in **what** we store (causal edges, not flat facts).

## What we don't do (and why)

| Don't | Why |
|---|---|
| Replace Mem0/Zep/Letta | Causal memory is **complementary** to flat/temporal/self-managed memory |
| Vector embedding search | Vector search is for semantic similarity; causal retrieval uses task + confidence, not cosine |
| Auto-extract decisions (v0.1) | Manual `record_decision` is more reliable; auto-extraction is v0.2 |
| Modify agent's context | We're a side table, not a context replacement |

## Theoretical foundation

This project is the engineering output of 13 research notes. The key ones:

1. **[insights/09](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)** — LLM is a stateless function. Memory = retrieval + injection. Causal memory is one specific injection strategy.
2. **[insights/10](https://github.com/JingxuanC/agent-teardown/blob/main/insights/10-memory-frameworks.md)** — Survey of 8 memory projects. None store causal relationships as first-class citizens. This is the market gap.
3. **[insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)** — The causal state store design (this implementation).
4. **[papers/02](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)** — Real LLM benchmark proving causal info decays fastest under text compaction.
