---
name: causal-memory
description: Causal memory for agents — install/setup the causal-memory MCP server, then record decisions/outcomes and recall them before acting. Trigger when the user asks to install or set up causal-memory/agent memory, when causal-memory MCP tools are available and the agent faces a non-trivial decision (architecture, debugging approach, library/deployment choice), when something fails unexpectedly, or when the user asks to "remember" something.
version: v1.1
---

# Causal Memory

Causal memory records your past decisions and their outcomes as causal edges,
so you learn from experience across sessions and survive context compaction.

This skill has two parts:
- **§1 Setup** — when the `record_decision` / `search_causal` MCP tools are
  NOT available yet: install and configure them yourself.
- **§2 Usage** — once the tools exist: the proactive recall/record loop.

## 1. Setup (do this yourself, then tell the user to restart the client)

1. **Install the package** (ships the full CLI as `causal-memory` on PATH):

   ```bash
   pip install causal-memory        # or: pipx install causal-memory
   causal-memory --help | head -5   # verify the console script works
   ```

2. **Register it as an MCP server** in the user's client. Bare
   `causal-memory` runs the stdio MCP server; the only env worth setting is
   `CAUSAL_MEMORY_DB` (SQLite location, default `~/.causal-memory/causal.db`).

   - **Claude Code** (CLI does the config for you):
     ```bash
     claude mcp add causal-memory -- causal-memory
     ```
   - **Cursor** — edit `~/.cursor/mcp.json` (merge into `mcpServers`):
     ```json
     { "mcpServers": { "causal-memory": { "command": "causal-memory" } } }
     ```
   - **Claude Desktop** — edit
     `~/Library/Application Support/Claude/claude_desktop_config.json`
     (same `mcpServers` shape as above).
   - **Kimi Code** — add to `config.toml`:
     ```toml
     [mcp.servers.causal-memory]
     command = "causal-memory"
     ```

   When editing JSON configs, merge — never overwrite the whole file.

3. **Tell the user to restart the client** (MCP servers load at startup).
   After restart, verify by calling `causal_directory` — an empty directory
   is fine, an error means the server didn't come up.

4. Optional shared/remote mode: `causal-memory http --port 9938` serves MCP
   over Streamable HTTP at `/mcp` (multi-agent shared memory; bind
   `--host 0.0.0.0` for non-localhost access, and set
   `CAUSAL_MEMORY_HTTP_AUTH_TOKEN` to protect the observability routes
   `/metrics` and `/debug/*` when the port is reachable beyond loopback).

## 2. Usage (once the tools are available)

**Do NOT ask the user before searching or recording — do it proactively.**

The core loop (five tools cover 90% of usage):

- **Before any non-trivial decision** (architecture, debugging approach,
  library selection, deployment strategy): call `search_memory` (facts +
  causal lessons, RRF-fused). For risky or irreversible actions, also call
  `intervention_query` — it forward-simulates what similar past actions
  caused (safe / warning / **danger**).
- **When choosing between two concrete options**: `counterfactual_query`
  with both option texts — recorded-outcome comparison, same-context
  branches (natural experiments) when they exist, and a logged falsifiable
  prediction that auto-resolves when either option is later recorded.
- **After acting on a decision and observing the result**: call
  `record_decision` with `decision`, `outcome`, `relation`
  (caused / enabled / prevented / no_effect), `task_tag`,
  `confidence_source`, and **`context`** — a short description of the
  situation (environment, constraints, key parameters). Same task_tag +
  context ⇒ comparable branch: this is the abduction substrate that makes
  counterfactuals same-world. If you weighed multiple options at this
  decision point, ALWAYS record the context. **Record surprising outcomes
  especially — those are the most valuable lessons.**
- **Stable facts** (preferences, tech stack, config): `record_fact` with
  `key` / `value` / `scope`; `replace_same_key: true` when superseding.
- **Failure postmortem**: `trace_cause` (single hop) /
  `trace_cause_chain` (multi-hop root cause).
- **Corrections**: `invalidate_decision` / `invalidate_pattern` (soft-delete,
  kept for audit).
- **Calibration check** (periodic): `prediction_report` — accuracy of past
  counterfactual verdicts per method / per task_tag, plus pending
  predictions.

Keep `task_tag` consistent within a domain (e.g. `deployment`,
`git-workflow`) — stratified pattern mining depends on it.

## 3. Offline session extraction (CLI — backfill memory from past sessions)

All four commands need an LLM: `CAUSAL_MEMORY_LLM_API` +
`CAUSAL_MEMORY_LLM_KEY` (or `DEEPSEEK_API_KEY`). They write into the same
`CAUSAL_MEMORY_DB` store the MCP server reads.

Agent-native session files (`--agent grok|claude|kimi|codex`; kimi = OpenClaw
format — kimi-code wire protocol 1.5 is NOT supported yet):

- **`extract <session-file|dir>`** — the default choice. Batches 15 assistant
  messages per LLM call; one call yields facts/preferences (→ fact layer)
  AND lessons/causal edges in all layers. Cheapest full-coverage path.
- **`judge <session-file|dir>`** — rule-based: pairs tool calls with their
  results into decision→outcome edges, then LLM re-judges only the top-20
  by confidence. Fewest LLM calls; causal edges only.
- **`reasoning <session-file|dir> [max_messages]`** — one LLM call PER
  assistant message, extracting decisions that never became tool calls
  (rejected designs, debated trade-offs). Most thorough, most expensive
  (calls = messages; default cap 30).

Normalized conversation JSON (`{"date": "YYYY-MM-DD", "turns": [[speaker,
message], ...]}` — hand-curated corpora, benchmark ingest):

- **`distill <session.json|dir> [--dry-run]`** — same Distiller as
  `extract`, for non-agent-native input. `--mode recurrence` switches to
  the RecMem flow (embeddings + recurrence gate).

Rule of thumb: backfilling an agent's history → `extract`; tight LLM budget
→ `judge`; hunting "decisions that never became actions" → `reasoning`;
own-format corpora → `distill`.

**Unsupported session format? Convert it yourself.** The turns JSON is the
universal interchange format — any agent can emit it and run `distill`, no
Rust parser needed. `scripts/session_to_turns.py` is the reference
converter (kimi-code wire v1.5 → turns JSON, ~60 lines) and its docstring
IS the format spec: ordered `["speaker", message]` pairs, stitch streaming
deltas, keep the assistant's reasoning prefixed `[think] `, inline
outcome-bearing tool results. Adapt the `convert_*` function to your own
session format (~1 hour of work), then `distill --dry-run` to verify.

Full reference (16 tools): repo README "Sixteen MCP tools" —
github.com/JingxuanC/causal-memory.
