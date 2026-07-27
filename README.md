# causal-memory

> **A causal memory layer for AI agents.** Records decisions and their outcomes as causal relationships, so agents learn from experience across sessions and survive compaction.
>
> Memory frameworks today (Mem0, Zep, Letta, OpenViking, MemOS) store *what* happened. `causal-memory` stores *why* — the causal link between a decision and its outcome. This is the slice every other memory layer misses.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status: v0.7.0](https://img.shields.io/badge/status-v0.7.0--alpha-orange.svg)](#status)

## Why

Every agent has the same problem: after N compactions, it forgets *why* it made past decisions. It reverts to a state where the same bug gets fixed the same wrong way, the same architecture choice gets re-debated, the same lesson gets relearned.

This happens because **causal information is the most fragile type under text compaction**. Real-LLM benchmark (using grok-build's production compaction prompt):

| Compactions (k) | Textual recall | Causal-table recall |
|---|---|---|
| 1 | 100% | 100% |
| 2 | 85% | 100% |
| 3 | 55% | 100% |
| 5 | **45%** | **100%** |

The causal table doesn't decay because it lives outside the agent's context window — compaction cannot touch it. See [`docs/design.md`](docs/design.md) and the [full benchmark writeup](https://github.com/JingxuanC/agent-teardown/blob/main/spike/grok-causal-memory/bench-RESULTS.md).

## Benchmarks

**LoCoMo** (1,986 questions, deepseek-chat answerer + judge, frozen protocol): overall **52.6%** · cats 1–4 **40.8%** · adversarial abstention **93.3%** — full methodology, per-category results, and measured failure analysis in [`docs/benchmarks/locomo.md`](docs/benchmarks/locomo.md). Honest reading: keyword retrieval is the bottleneck for factual chit-chat QA (that's Mem0/Zep's home turf, not ours); abstention — not hallucinating when memory has no answer — is where this system is strong.

**Compaction survival** (the experiment this system is designed for): see the table above — causal-table recall stays at 100% where text recall collapses.

## What it does

Eight MCP tools. Small surface area is the point.

| Tool | When to call | What it does |
|---|---|---|
| `record_decision` | After completing an action | Logs `decision → outcome` as a causal edge with task tag + confidence; auto-invalidates contradicted older edges for the same decision |
| `search_causal` | Before a non-trivial decision | Retrieves past causal episodes by task or text; ranks by embedding cosine similarity when configured, keyword LIKE otherwise |
| `trace_cause` | When something fails (simple) | Single-hop reverse: which decision caused this outcome |
| `trace_cause_chain` | When something fails (deep) | Multi-hop backward traversal through the causal graph |
| `invalidate_decision` | When a recorded lesson turns out wrong | Soft-invalidates the edge (`valid_to` set) — hidden from search/trace, kept for audit |
| `search_patterns` | To recall cross-task lessons | Searches mined meta edges: `similar_to` / `repeated` / `contradicts` / `refines` |
| `causal_directory` | Pinned in the system prompt | L0 compact pointer list of recent decisions so the agent always knows what experience it holds |
| `intervention_query` | Before taking an action | Pearl Rung-2: predicts what outcomes similar past actions caused, labeled safe / warning / danger |

**Multi-hop example**: "service crashed" ← "OOM" ← "cache had no TTL" ← "Redis configured without expiry". `trace_cause` finds the first hop. `trace_cause_chain` walks the full chain.

## Quick start

```bash
git clone https://github.com/JingxuanC/causal-memory.git
cd causal-memory
cargo build --release
```

Wire into any MCP-compatible agent (Claude Code, Cursor, grok-build, etc.):

```json
{
  "mcpServers": {
    "causal-memory": {
      "command": "/path/to/causal-memory/target/release/causal-memory",
      "env": {
        "CAUSAL_MEMORY_DB": "~/.local/share/causal-memory/causal.db"
      }
    }
  }
}
```

Then copy [`CLAUDE.md`](CLAUDE.md) into your project's system prompt to activate proactive causal memory use. Per [research notes](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md#L143): agents don't proactively call memory tools without explicit prompt instruction.

## How it's different

| Memory type | Stores | Example | Who does it |
|---|---|---|---|
| Flat facts | User preferences | "User prefers TypeScript" | Mem0 |
| Temporal facts | State changes over time | "User was on Pro plan in March" | Zep |
| Self-managed | Agent edits its own memory | Agent decides what to remember | Letta |
| File system | Memory as virtual FS | Directory-based retrieval | OpenViking |
| **Causal** | **Decision → outcome links** | **"Mutex lock caused deadlock"** | **causal-memory (this)** |

**causal-memory is complementary, not competitive.** A complete 7×24 agent may need Mem0 (preferences) + Zep (state) + causal-memory (lessons). This layer fills the causal slice nobody else covers.

## Data path

- Default: `~/.local/share/causal-memory/causal.db`
- Override: `CAUSAL_MEMORY_DB` env var

SQLite file. Portable. No server process. Your data stays on your machine.

## Architecture

```
Agent ←(MCP stdio)→ causal-memory → SQLite (causal_edges table)
```

The `causal_edges` table is never compacted — it's outside the agent's context window. That's the entire point: text compaction cannot destroy what it cannot reach.

## Sleep consolidation

An offline "sleep" cycle (reactivation → generalization → downscaling → REM
integration) that decays stale confidence, garbage-collects low-value edges,
merges duplicates, and mines cross-task patterns:

```bash
causal-memory sleep --dry-run   # preview what would change
causal-memory sleep             # run it (once per day — NOT idempotent)
```

## Semantic search

Optional embeddings rank `search_causal` by cosine similarity instead of LIKE
matching. Any OpenAI-compatible `/v1/embeddings` endpoint works:

```bash
export CAUSAL_MEMORY_EMBED_API=https://api.openai.com/v1   # default: CAUSAL_MEMORY_LLM_API
export CAUSAL_MEMORY_EMBED_KEY=sk-...                      # default: CAUSAL_MEMORY_LLM_KEY
export CAUSAL_MEMORY_EMBED_MODEL=text-embedding-3-small    # optional

causal-memory embed            # backfill embeddings for existing edges
causal-memory embed --limit 50 # partial backfill
```

Unconfigured → automatic fallback to keyword search. Zero-invasive by default.

Existing databases are upgraded automatically on open (schema v3); run
`causal-memory migrate` for an explicit check.

## Status

**v0.7.0 — alpha.** What works:

- ✅ Eight MCP tools (record / search / trace / chain-trace / invalidate / patterns / L0 directory / intervention)
- ✅ SQLite persistence with CHECK constraints + idempotent schema migrations (v4)
- ✅ **Parameterized queries** (no SQL injection risk)
- ✅ Confidence levels (temporal / rule / llm_inferred / user_feedback)
- ✅ Task-aware retrieval + optional **semantic (embedding) retrieval** with keyword fallback
- ✅ **Multi-hop backward traversal** via recursive CTE
- ✅ Rule-based **decision auto-extractor** for grok-build session logs
- ✅ **Invalidation**: manual (`invalidate_decision`) + automatic contradiction short-circuit
- ✅ **Dual-system memory**: offline pattern miner distils meta edges (`similar_to` / `repeated` / `contradicts` / `refines`)
- ✅ **Offline consolidation ("sleep") cycle** with four phases
- ✅ **L0 causal directory** + **Rung-2 intervention queries**
- ✅ 79 tests (unit + e2e suites: migration / pipeline / MCP stdio + benchmark harness)

What's not done yet (honest):

- ❌ Python/TS bindings (Rust binary only)
- ❌ HTTP transport (MCP stdio only)
- ❌ LongMemEval benchmark integration (LoCoMo done — see [Benchmarks](#benchmarks))
- ❌ Cross-agent sharing protocol
- ❌ Reconstructive retrieval (causal subgraph → LLM narrative)
- ❌ Rung 3 counterfactuals — **explicitly out of scope by design**: per
  [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md),
  counterfactual reasoning is practically impossible for agents; we only
  prepare the data structures (temporal validity windows), we do not build it
- ❌ Not yet wired into a production agent end-to-end

Roadmap: see [docs/roadmap.md](docs/roadmap.md).

## Build & test

```bash
cargo build --release    # Build binary
cargo test               # Run unit tests
```

## Research background

This project is the engineering output of 13 research notes on agent memory architecture:
- [Agent's Second Law — anti-degradation (information theory)](https://github.com/JingxuanC/agent-teardown/blob/main/insights/04-anti-entropy.md)
- [LLM is a stateless function](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)
- [Memory company landscape (Letta/Mem0/Zep/OpenViking/MemOS)](https://github.com/JingxuanC/agent-teardown/blob/main/insights/10-memory-frameworks.md)
- [Causal state store — the design this implements](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)
- [Real LLM compaction benchmark](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)

Papers that shaped specific design decisions: [`docs/research-backdrop.md`](docs/research-backdrop.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
