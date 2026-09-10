---
name: causal-memory
description: Causal memory for agents — install/setup the causal-memory MCP server, then record decisions/outcomes and recall them before acting. Trigger when the user asks to install or set up causal-memory/agent memory, when causal-memory MCP tools are available and the agent faces a non-trivial decision (architecture, debugging approach, library/deployment choice), when something fails unexpectedly, or when the user asks to "remember" something.
version: v1.3
---

# Causal Memory

Causal memory records your past decisions and their outcomes as causal edges,
so you learn from experience across sessions and survive context compaction.

This skill has two parts:
- **§1 Setup** — when the `record_decision` / `search_causal` MCP tools are
  NOT available yet: install and configure them yourself.
- **§2 Usage** — once the tools exist: the proactive recall/record loop.

## 1. Setup (do this yourself, then tell the user to restart the client)

The bundled `install.sh` (next to this SKILL.md, shipped with the skill) does
steps 1–2 below automatically — installs the server, symlinks the skill, and
registers the MCP server in every detected client (Claude Code / Cursor /
Opencode / Claude Desktop / Kimi Code). Prefer it over hand-editing:

```bash
bash <skill_dir>/install.sh        # 交互式引导; --yes 全自动; --uninstall 卸载
```

Then jump to step 3 (restart). The numbered steps below are the manual
fallback.

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
   - **Opencode** — edit `~/.config/opencode/opencode.jsonc` (or opencode.json):
     ```json
     { "mcp": { "causal-memory": {"type":"local", "command": "causal-memory", "enabled":true, "environment":{
       "CAUSAL_MEMORY_DB": ".local/share/causal-memory/causal.db"
     } } } }
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

## 3. Context recovery & sync (CLI git-sync)

The store snapshots like git — but the git semantics are **self-implemented**
(no git binary or wire protocol involved): content-addressed sha256 objects
under `<db>.cm/objects/`, plain-text refs, hand-rolled fast-forward checks.
A "remote" is one of exactly two things: a **shared directory** (file
remote, zero infrastructure) or a **sync server** (`https` object store —
`GET/PUT {base}/objects/{hash}` + `/refs/heads/main`, bearer auth; see
contrib/docker for the server image). GitHub/GitLab repos do NOT work as
remotes.

**Primary purpose: context recovery.** Memory is the agent's *private*
context, snapshotted under its `agent_id` namespace. `commit && push` keeps
the cloud copy current; on a new machine `clone <agent_id>` restores the
full context in one shot; `checkout <hash>` rolls back to any snapshot.
Single-writer assumption → no merge algorithm; "merge" is just the
idempotent import.

- **`commit -m <msg>`** — snapshot the whole store (full truth incl.
  invalidated edges, no redaction). On a fresh clone with no local changes
  it correctly reports "nothing to commit".
- **`push [<remote|path>]` / `pull`** — upload / import commits
  (fast-forward checked, idempotent; pull propagates forget/supersede).
- **`clone <path|remote>`** — build a fresh DB from a remote + set origin.
  This is the context-restore entry point: new machine → `clone` → back to
  work with full memory.
- **`checkout <hash|HEAD|HEAD~N>`** — hard-reset the DB to a snapshot
  (rollback; auto-backup first); **`log --oneline`** walks the chain
  without opening the DB.
- **`remote add|list|remove`** — named remotes; **`cloud register
  <agent_id> <server-url>`** — per-agent tokens against a sync server.
- **`session-commit [<session>] [--push R]`** — snapshot a session's
  lessons and optionally push; designed for host auto-commit hooks
  (skips empty stores). Keeps the cloud copy from ever going stale.

Team pattern (a natural extension, not the primary goal): point several
agents at one shared remote (a shared directory is enough to start), each
`clone`s once, then `commit && push` after meaningful work and `pull` at
session start.

## 4. Offline session extraction (CLI — backfill memory from past sessions)

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
Rust parser needed. The bundled `scripts/session_to_turns.py` is the
reference converter (kimi-code wire v1.5 → turns JSON, ~60 lines) and its
docstring IS the format spec: ordered `["speaker", message]` pairs, stitch
streaming deltas, keep the assistant's reasoning prefixed `[think] `,
inline outcome-bearing tool results. Adapt the `convert_*` function to your
own session format (~1 hour of work), then `distill --dry-run` to verify.

## 5. Bundled scripts (ops & integration helpers)

Shipped in the `scripts/` directory **next to this SKILL.md** — resolve
`<skill_dir>` from the skill listing path (e.g.
`~/.agents/skills/causal-memory/scripts/…` once installed, or
`scripts/…` at the source repo root). Repo source of truth:
`scripts/` in github.com/JingxuanC/causal-memory.

- **`session_to_turns.py`** — CLI converter, agent session log → turns JSON
  (the §4 interchange format). Use when the user's session format has no
  Rust parser and they want history backfilled:
  ```bash
  python3 <skill_dir>/scripts/session_to_turns.py <session-file> /tmp/turns.json
  causal-memory distill /tmp/turns.json --dry-run   # verify, then drop --dry-run
  ```
  Input is kimi-code wire.jsonl; for other agents copy the `convert_*`
  function and adapt (docstring is the format spec). stdlib only.

- **`audit_fact_links.py`** — read-only store audit: fact↔chunk entity
  links, replicating the Rust linker policy; reports orphaned / mis-linked
  facts. Run after large imports, migrations, or when `search_facts`
  surfaces stale entries:
  ```bash
  python3 <skill_dir>/scripts/audit_fact_links.py            # default db
  python3 <skill_dir>/scripts/audit_fact_links.py --db /path/to/causal.db --sample 5
  ```
  Options: `--db` (default `~/.local/share/causal-memory/causal.db`),
  `--min-tokens N`, `--df-limit N` (0 disables), `--no-compare`,
  `--sample N`. stdlib only; never writes.

- **`causal_memory_client.py`** — importable Python client (NOT a CLI) for
  scripting against the MCP server over **both** transports. Requires
  `pip install requests`. Use for bulk reads/writes, deployment probes, or
  automation beyond single MCP tool calls:
  ```python
  import sys; sys.path.insert(0, "<skill_dir>/scripts")
  from causal_memory_client import CausalMemoryClient

  cm = CausalMemoryClient.http("http://localhost:9938/mcp")   # remote deployment
  assert cm.health()
  print(cm.search_memory("deploy rollback"))

  cm = CausalMemoryClient.stdio("causal-memory")              # local, spawns the binary
  cm.record_decision("chose HRP over equal-weight", "sharpe 1.8 vs 1.1",
                     "caused", "portfolio", context="120d lookback, 10 names")
  cm.close()
  ```
  Wraps all 17 tools (`record_decision` / `search_*` / `trace_*` /
  `intervention_query` / `counterfactual_query` / `reconstruct_lesson` …).

Full reference (17 tools): repo README "MCP tools" —
github.com/JingxuanC/causal-memory.
