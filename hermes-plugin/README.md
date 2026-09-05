# hermes-causal-memory

Hermes memory-provider plugin backed by [causal-memory](https://github.com/JingxuanC/causal-memory) — flat facts + decision→outcome causal lessons on one local SQLite store, with spreading-activation recall.

## Why this provider

- **Causal core, not just notes.** Lessons are recorded as `decision →(caused/enabled/prevented)→ outcome` edges. `causal_trace` walks them backward for post-mortems; inhibitory (`prevented`) edges surface as warnings, not silence.
- **Hybrid recall.** `prefetch` runs the unified engine: a BM25/semantic seeding layer answers the literal query, then spreading activation over the causal graph adds associative lessons the wording never mentions (governed by a Collins & Loftus fan-out constraint so hubs can't flood the result).
- **Compaction survival.** Lessons are extracted OUTSIDE the context window — Hermes compaction can drop conversation history, but the causal store is untouched. `on_pre_compress` is wired as the differentiation hook (currently a conservative no-op; see below).
- **Profile isolation.** The store lives under `<hermes_home>/causal-memory/causal.db` — per-profile by construction.

## Cloud sync — session-end auto-commit

When the session ends (CLI exit, `/reset`, gateway session expiry) and the
store has a **remote** provisioned for this agent, the provider snapshots the
session's recorded lessons and pushes them on a background thread —
`causal-memory session-commit -m '<L0>' --push <agent_id> --db <store>`.
Nothing recorded this session → `nothing to commit`, silent no-op. The hook
never raises or blocks teardown.

Provision once per store (config `agent_id` = the remote name):

```
# Cloud: register an agent namespace on your sync server (mints a per-agent token)
causal-memory cloud register athena https://cm.example.com --db <this store>

# …or file remote (NAS / shared disk / USB sneaker-net)
causal-memory remote add athena /Volumes/backup/causal-memory --db <this store>
```

Then configure the provider (via `hermes memory setup` or the config keys):

| key | meaning |
|---|---|
| `agent_id` | remote to push to (empty = disable auto-commit) |
| `server_url` | informational (the CLI resolves the remote from the store's config) |
| `auto_commit` | set false to keep snapshots manual |

Requires the `causal-memory` CLI on `PATH` (dev override:
`CAUSAL_MEMORY_CLI=/path/to/binary`). Missing CLI / no agent_id / disabled →
silent no-op.

## Install

### Development (directory layout)

```
$HERMES_HOME/plugins/causal-memory/
├── __init__.py    → re-export the provider (see below)
├── plugin.yaml    → copy from src/hermes_causal_memory/plugin.yaml
├── cli.py         → from hermes_causal_memory.cli import *
└── README.md      → this file
```

`__init__.py` must NAME the provider class, not star-import it:

```python
"""causal-memory MemoryProvider plugin (directory layout shim)."""

from hermes_causal_memory import CausalMemoryProvider, register

__all__ = ["CausalMemoryProvider", "register"]
```

Hermes's memory-provider discovery (`plugins/memory/_is_memory_provider_dir`)
text-scans `__init__.py` for the literal strings `register_memory_provider`
or `MemoryProvider` before it ever imports the module — a bare
`from hermes_causal_memory import *` contains neither, so the plugin is
silently skipped (listed by `hermes plugins list` but never loaded as a
provider). `CausalMemoryProvider` contains the `MemoryProvider` substring,
so the explicit re-export passes the scan.

The plugin needs the `causal_memory` bindings importable by the Hermes Python (`pip install causal-memory` once it is on PyPI, or `maturin develop` from the causal-memory repo).

### Packaged (future)

```
pip install hermes-causal-memory
```

Hermes discovers it through the `hermes_agent.memory_providers` entry point (`causal-memory = hermes_causal_memory:register`).

## Configuration

Single-provider rule: set causal-memory as THE memory provider.

```yaml
memory:
  provider: causal-memory
```

Config keys (deliberately minimal, no secrets):

| key | default | meaning |
|---|---|---|
| `db_path` | `""` (auto) | store path; empty = `<hermes_home>/causal-memory/causal.db` |
| `prefetch_budget` | `500` | max tokens returned per prefetch recall (0 = unlimited) |

## Tools

| tool | maps to | purpose |
|---|---|---|
| `causal_search` | `search_memory` | fused fact + lesson recall (query, task_tag?, detail_level?, max_tokens?) |
| `causal_record` | `record_decision` | record a decision→outcome lesson after acting |
| `causal_trace` | `trace_cause` | backward trace from a bad outcome |

## Hooks

| hook | behavior |
|---|---|
| `system_prompt_block` | `causal_directory` — an L0 pointer list of recent lessons, compact enough to pin in the system prompt |
| `prefetch` | budgeted `search_memory` (hybrid recall, `prefetch_budget` tokens) |
| `sync_turn` | `remember()` on a daemon thread — **never blocks the turn** |
| `on_memory_write` | mirrored into the fact layer (`scope="agent"`, replace-on-rewrite) |
| `on_pre_compress` | conservative no-op (compaction survival is already structural; LLM distill TODO) |
| `on_session_end` | conservative no-op (session distill needs an LLM key; TODO) |
| `shutdown` | drains pending remember threads, releases the store |

## CLI

```
hermes causal-memory stats [--db PATH]
```

Read-only store statistics (chunks / valid edges by relation / facts / mined patterns / q_value distribution).

## Tests

Hermes-free (fake ctx + direct provider calls):

```
pip install -e . --no-deps   # causal-memory dep resolves separately
pytest tests/
```
