# Codex CLI integration

Causal memory as a Codex MCP server + global `AGENTS.md` activation
guidance.

## What it wires up

1. **MCP server**: `codex mcp add causal-memory -- <bin>` (stdio). All 17
   tools, same as every other surface.
2. **AGENTS.md guidance**: a causal-memory section appended to
   `~/.codex/AGENTS.md` (Codex's global guidance file) teaching WHEN to
   call the tools — including the Rung-3 activation rules: record
   `context` when options were weighed, `counterfactual_query` when
   choosing between two options, `prediction_report` periodically.

## Install

```bash
./install.sh            # uses target/release/causal-memory by default
BIN=/other/path ./install.sh
```

Verify:

```bash
codex mcp list          # causal-memory must appear
codex mcp get causal-memory
```

## Uninstall

```bash
./uninstall.sh          # removes the MCP entry + the AGENTS.md section
```

## Notes

- Same shared DB as other integrations (`~/.local/share/causal-memory/
  causal.db`), so lessons learned in Claude Code / kimi / Hermes are
  visible to Codex and vice versa.
- The AGENTS.md section is delimited by `<!-- causal-memory:start -->` /
  `<!-- causal-memory:end -->` markers — safe to re-run, removes cleanly.
