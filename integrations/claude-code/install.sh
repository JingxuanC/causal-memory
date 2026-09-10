#!/usr/bin/env bash
# Register causal-memory as a user-scope Claude Code MCP server + install
# the activation skill. Idempotent: re-running refreshes both.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${BIN:-$REPO_ROOT/target/release/causal-memory}"
SKILL_SRC="$REPO_ROOT/skills/causal-memory/SKILL.md"
SKILL_DST="$HOME/.claude/skills/causal-memory/SKILL.md"

if [[ ! -x "$BIN" ]]; then
  echo "binary not found: $BIN" >&2
  echo "build it first: cargo build --release -p causal-memory-cli" >&2
  exit 1
fi

# 1. MCP server (user scope = every project). Remove a stale entry first so
#    re-running refreshes the command path cleanly.
claude mcp remove causal-memory --scope user >/dev/null 2>&1 || true
claude mcp add --scope user causal-memory -- "$BIN"
echo "✓ MCP server registered (user scope): causal-memory → $BIN"

# 2. Activation skill.
mkdir -p "$(dirname "$SKILL_DST")"
cp "$SKILL_SRC" "$SKILL_DST"
echo "✓ skill installed: $SKILL_DST"

echo
echo "Verify:  claude mcp list   (causal-memory must connect)"
echo "DB:      ~/.local/share/causal-memory/causal.db (CAUSAL_MEMORY_DB to override)"
