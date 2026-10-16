#!/usr/bin/env bash
# Remove the causal-memory MCP entry and the AGENTS.md guidance section.
set -euo pipefail

AGENTS="$HOME/.codex/AGENTS.md"
START='<!-- causal-memory:start -->'
END='<!-- causal-memory:end -->'

codex mcp remove causal-memory >/dev/null 2>&1 \
  && echo "✓ MCP server removed" || echo "(no registration found)"

if [[ -f "$AGENTS" ]] && grep -q "$START" "$AGENTS"; then
  python3 - "$AGENTS" "$START" "$END" <<'PYEOF'
import sys
path, start, end = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path).read()
pre, rest = text.split(start, 1)
_, post = rest.split(end, 1)
open(path, "w").write(pre.rstrip() + ("\n" if pre.strip() else "") + post.lstrip("\n"))
PYEOF
  echo "✓ AGENTS.md section removed"
else
  echo "(no AGENTS.md section found)"
fi
rm -f "$HOME/.codex/prompts/causal-memory.md" \
  && echo "✓ slash prompt removed" || true

echo "The memory DB (~/.local/share/causal-memory/) is kept."
