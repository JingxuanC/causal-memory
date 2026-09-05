# Claude Code integration

**Preferred: the native plugin** (marketplace install — see
`plugins/claude-code/`):

```bash
pip install causal-memory                      # binary on PATH
claude plugin marketplace add JingxuanC/causal-memory
claude plugin install causal-memory@causal-memory
```

The script below is the no-marketplace fallback (direct MCP registration
+ skill copy).

## What it wires up

1. **MCP server**: `causal-memory` (stdio, no args). All 17 tools:
   `record_decision` (with `context`) / `search_memory` /
   `counterfactual_query` / `prediction_report` / …
2. **Skill**: `~/.claude/skills/causal-memory/SKILL.md` — teaches Claude
   Code WHEN to call the tools (before non-trivial decisions, when choosing
   between options, after observing outcomes, ALWAYS record `context` when
   multiple options were weighed).

## Install

```bash
./install.sh            # uses target/release/causal-memory by default
BIN=/other/path ./install.sh
```

Verify:

```bash
claude mcp list         # causal-memory must appear and connect
```

Then in any Claude Code session: `/causal-memory` surfaces the skill, or
just rely on the skill's trigger description.

## Uninstall

```bash
./uninstall.sh
```

## Notes

- The DB defaults to `~/.local/share/causal-memory/causal.db` (shared with
  other integrations). Override with `CAUSAL_MEMORY_DB`.
- Embedding / LLM features read `CAUSAL_MEMORY_EMBED_*` / `CAUSAL_MEMORY_LLM_*`
  (configure once via `causal-memory setconfig`); without them retrieval
  degrades gracefully to BM25.
- HTTP transport alternative: `causal-memory http --port 9938` +
  `claude mcp add --transport http causal-memory http://127.0.0.1:9938/mcp`
  (set `CAUSAL_MEMORY_HTTP_AUTH_TOKEN` before exposing the port).
