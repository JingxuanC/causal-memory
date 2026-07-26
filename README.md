# causal-memory

> **A causal memory layer for AI agents.** Records decisions and their outcomes as causal relationships, so agents learn from experience across sessions and survive compaction.
>
> Memory frameworks today (Mem0, Zep, Letta, OpenViking, MemOS) store *what* happened. `causal-memory` stores *why* — the causal link between a decision and its outcome. This is the slice every other memory layer misses.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status: v0.1.0](https://img.shields.io/badge/status-v0.1.0--alpha-orange.svg)](#status)

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

## What it does

Three MCP tools. That's it — small surface area is the point.

| Tool | When to call | What it does |
|---|---|---|
| `record_decision` | After completing an action | Logs `decision → outcome` as a causal edge with task tag + confidence |
| `search_causal` | Before a non-trivial decision | Retrieves past causal episodes by task or text, ordered by confidence |
| `trace_cause` | When something fails | Reverse-traces which past decision caused a given outcome |

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

## Status

**v0.1.0 — alpha.** Working MCP server, unit tests pass, builds clean. What works:

- ✅ Three MCP tools (record / search / trace)
- ✅ SQLite persistence with CHECK constraints
- ✅ Confidence levels (temporal / rule / llm_inferred / user_feedback)
- ✅ Task-aware retrieval
- ✅ Reverse causal lookup for failure attribution

What's not done yet (honest):

- ❌ No decision auto-extractor (agent must call `record_decision` manually)
- ❌ No Python/TS bindings (Rust binary only)
- ❌ No LongMemEval benchmark integration
- ❌ No cross-agent sharing protocol
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

## License

Apache-2.0. See [LICENSE](LICENSE).
