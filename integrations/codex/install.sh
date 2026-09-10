#!/usr/bin/env bash
# Register causal-memory as a Codex MCP server + append the activation
# guidance to ~/.codex/AGENTS.md. Idempotent: re-running refreshes both.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${BIN:-$REPO_ROOT/target/release/causal-memory}"
AGENTS="$HOME/.codex/AGENTS.md"
START='<!-- causal-memory:start -->'
END='<!-- causal-memory:end -->'

if [[ ! -x "$BIN" ]]; then
  echo "binary not found: $BIN" >&2
  echo "build it first: cargo build --release -p causal-memory-cli" >&2
  exit 1
fi

# 1. MCP server (remove first so re-runs refresh the command path).
codex mcp remove causal-memory >/dev/null 2>&1 || true
codex mcp add causal-memory -- "$BIN"
echo "✓ MCP server registered: causal-memory → $BIN"

# 2. AGENTS.md activation section (marker-delimited, replace-if-present).
mkdir -p "$HOME/.codex"
touch "$AGENTS"
python3 - "$AGENTS" "$START" "$END" <<'PYEOF'
import sys

path, start, end = sys.argv[1], sys.argv[2], sys.argv[3]
section = f"""{start}
## Causal memory (MCP tools — use proactively, do not ask first)

You have a persistent causal-memory store recording past decisions and
their outcomes. It survives compaction and restarts.

- **Before any non-trivial decision** (architecture, debugging approach,
  library selection, deployment strategy): call `search_memory` with your
  query (facts + causal lessons, fused). For risky or irreversible actions,
  also call `intervention_query` on the action (safe / warning / danger).
- **When choosing between two concrete options**: call
  `counterfactual_query` with both option texts — recorded-outcome
  comparison, same-context branches (natural experiments) when they exist,
  and a logged falsifiable prediction.
- **After acting and observing the result**: call `record_decision` with
  `decision` / `outcome` / `relation` (caused / enabled / prevented /
  no_effect) / `task_tag` / `confidence_source`, and **`context`** — a
  short description of the situation (environment, constraints, key
  parameters). Same task_tag + context ⇒ comparable branch; if you weighed
  multiple options at this decision point, ALWAYS record the context.
  Record surprising outcomes especially — those are the most valuable
  lessons.
- **Stable facts** (preferences, tech stack, config): `record_fact` with
  `key` / `value` / `scope`; `replace_same_key: true` when superseding.
- **When something fails unexpectedly**: `trace_cause` (single hop) /
  `trace_cause_chain` (multi-hop root cause).
- **When a recorded lesson turns out wrong**: `invalidate_decision`.
- **Periodically**: `prediction_report` — accuracy of past counterfactual
  verdicts per method / per task_tag; it keeps the advice honest.
{end}
"""
text = open(path).read()
if start in text and end in text:
    pre, rest = text.split(start, 1)
    _, post = rest.split(end, 1)
    text = pre + section + post
else:
    text = text.rstrip() + ("\n\n" if text.strip() else "") + section + "\n"
open(path, "w").write(text)
PYEOF
echo "✓ AGENTS.md guidance: $AGENTS"

# 3. Native slash prompt: /causal-memory <task> (Codex custom prompts).
PROMPT_SRC="$REPO_ROOT/plugins/codex/prompts/causal-memory.md"
PROMPT_DST_DIR="$HOME/.codex/prompts"
if [[ -f "$PROMPT_SRC" ]]; then
  mkdir -p "$PROMPT_DST_DIR"
  cp "$PROMPT_SRC" "$PROMPT_DST_DIR/causal-memory.md"
  echo "✓ slash prompt installed: /causal-memory ($PROMPT_DST_DIR/causal-memory.md)"
fi

echo
echo "Verify:  codex mcp list   (causal-memory must appear)"
echo "DB:      ~/.local/share/causal-memory/causal.db (CAUSAL_MEMORY_DB to override)"
