# causal-memory

**An agent memory system with a causal core** — facts, temporal state, and
`decision → outcome` causal edges on one local SQLite store. Agents recall
*what* happened, *when* it was true, *why* it worked — and *what would happen
if* they acted differently.

```
pip install causal-memory
```

Requires Python ≥ 3.9. Prebuilt wheels: macOS (Apple Silicon + Intel),
Linux x86_64 (manylinux_2_28), Windows x64. No API key needed for the
default setup.

## 30-second quickstart

```python
from causal_memory import CausalMemory

# One local SQLite file is the whole store. Created on first use.
mem = CausalMemory("~/.local/share/causal-memory/causal.db")
# or CausalMemory.in_memory() for a throwaway store

# Write: record a decision → outcome as a typed causal edge
mem.record_decision(
    "used Redis mutex for cache stampede protection",
    "deadlock under load",
    "caused",            # caused | enabled | prevented | no_effect
    "concurrency",       # task tag — how you retrieve this later
)

# Read: BM25 full-text retrieval works out of the box
print(mem.search_causal(query="cache stampede protection"))

# Ask before acting: forward simulation over recorded outcomes
print(mem.intervention_query("skip the test suite before shipping"))
```

Every method mirrors one of the 14 MCP tools and returns plain text that
agent frameworks (LangChain, LlamaIndex, Hermes, …) can feed straight back
to the model. See the full method list on
[GitHub](https://github.com/JingxuanC/causal-memory#fourteen-mcp-tools).

## Configuration

Everything is optional and configured via environment variables:

| Variable | What it does | Default |
|---|---|---|
| `CAUSAL_MEMORY_EMBED_API` | Embedding endpoint (OpenAI-compatible) for semantic retrieval | unset → BM25 only |
| `CAUSAL_MEMORY_EMBED_KEY` | API key for the embedding endpoint | unset |
| `CAUSAL_MEMORY_EMBED_MODEL` | Embedding model name (e.g. `embedding-3`) | unset |
| `CAUSAL_MEMORY_LLM_*` | LLM endpoint for distill / `remember` auto-extraction | unset → raw storage, no extraction |

Without embedding/LLM variables the package **degrades gracefully**: retrieval
is BM25-only and `remember` stores raw text instead of extracted facts. You
can add them later — the store format is the same.

## What this package is (and isn't)

- ✅ The **Python library**: embed causal memory in your own agent, script,
  or framework integration.
- ✅ The **MCP server command**: the same `pip install` also drops a
  `causal-memory` console script on your PATH — point your MCP client
  config straight at it, no Rust toolchain needed:

  ```json
  {
    "mcpServers": {
      "causal-memory": {
        "command": "causal-memory"
      }
    }
  }
  ```

  The console script is the full CLI: bare `causal-memory` runs the MCP
  stdio server; subcommands cover configuration (`setconfig` / `getconfig`
  / `config-path`), maintenance (`stats`, `sleep`, `migrate`, …) and
  import/export. See `causal-memory --help`.
- ❌ Not a hosted service — everything runs against a local SQLite file
  you own.

## Links

- GitHub (full docs, benchmarks, architecture): https://github.com/JingxuanC/causal-memory
- Changelog: https://github.com/JingxuanC/causal-memory/blob/main/CHANGELOG.md
- License: Apache-2.0
