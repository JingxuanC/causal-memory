# causal-memory — Claude Code plugin

Persistent causal memory for Claude Code: decisions, outcomes, and lessons
recorded as typed causal edges that survive compaction and restarts — plus
same-context counterfactuals (natural experiments), a falsifiable
prediction ledger, and an activation skill that teaches when to recall and
when to record.

## What's inside

| Component | What it does |
|---|---|
| `.mcp.json` | Registers the `causal-memory` MCP server (stdio, 17 tools) with the plugin |
| `skills/causal-memory` | Activation skill: WHEN to search / intervene / counterfactual / record (with `context`) / report |
| `commands/recall` | `/recall <task or decision>` — experience check before acting |
| `commands/memory-report` | `/memory-report` — prediction-ledger calibration + memory inventory |

## Prerequisites

The plugin launches `causal-memory` from PATH:

```bash
pip install causal-memory        # wheel ships the MCP server binary
```

(or build from repo: `git clone https://github.com/JingxuanC/causal-memory
&& cargo build --release -p causal-memory-cli` and put
`target/release/causal-memory` on PATH — then optional `causal-memory
setconfig EMBED_API=... / LLM_API=...` for semantic + LLM features;
without them everything degrades gracefully to BM25.)

## Install (from this repo's marketplace)

```bash
claude plugin marketplace add JingxuanC/causal-memory
claude plugin install causal-memory@causal-memory
```

The DB lives at `~/.local/share/causal-memory/causal.db` by default
(`CAUSAL_MEMORY_DB` to override) and is shared with every other
causal-memory integration (Codex, kimi CLI, Hermes, Python) — lessons
cross-pollinate across agents.

## Memory hygiene

- `causal-memory sleep` periodically consolidates (decay, pattern mining);
  `causal-memory stats` shows fork density and prediction accuracy.
