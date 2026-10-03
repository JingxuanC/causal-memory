#!/usr/bin/env bash
# Remove the causal-memory MCP server registration and skill from Claude Code.
set -euo pipefail

claude mcp remove causal-memory --scope user >/dev/null 2>&1 || true \
  && echo "✓ MCP server removed" || echo "(no registration found)"
rm -f "$HOME/.claude/skills/causal-memory/SKILL.md" \
  && echo "✓ skill removed" || true
rmdir "$HOME/.claude/skills/causal-memory" 2>/dev/null || true
echo "The memory DB (~/.local/share/causal-memory/) is kept."
