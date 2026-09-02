# Official plugin directory submission package

> Everything the Console form asks, pre-filled. Submit at
> **https://platform.claude.com/plugins/submit** (individual-author form).
> After approval the plugin is auto-pinned into
> `anthropics/claude-plugins-community` with a SHA that CI bumps on push.

## Form fields

| Field | Value |
|---|---|
| **Plugin name** | `causal-memory` |
| **Repository** | https://github.com/JingxuanC/causal-memory |
| **Plugin path in repo** | `plugins/claude-code` |
| **Version** | 0.9.3 |
| **Author** | JingxuanC |
| **Category** | Memory / Developer productivity |
| **Install (post-approval)** | `claude plugin install causal-memory@claude-plugins-community` |
| **Install (self-serve, today)** | `claude plugin marketplace add JingxuanC/causal-memory && claude plugin install causal-memory@causal-memory` |

## Short description (≤160 chars)

Persistent causal memory for Claude Code — decisions, outcomes, and lessons
that survive compaction, with same-context counterfactuals and a
falsifiable prediction ledger. Local-first, zero telemetry.

## Long description

Claude Code sessions forget. causal-memory gives them a persistent,
causal-structured memory: every decision you make is recorded with its
observed outcome as a typed edge (caused / enabled / prevented), retrieved
by hybrid BM25 + spreading activation, and consolidated with decay and
cross-task pattern mining.

Beyond recall, it implements an honest slice of causal reasoning:

- **Intervention (Rung 2)**: `intervention_query` forward-simulates an
  action's likely outcome from similar past actions (safe/warning/danger).
- **Counterfactuals**: `counterfactual_query` compares two options;
  decisions recorded with a `context` form same-world branches (natural
  experiments) that outrank pooled statistics.
- **Falsifiability**: every counterfactual verdict is logged as a
  prediction that auto-resolves when either option is later recorded;
  `prediction_report` shows per-domain accuracy — the advice grades
  itself.

Ships as: MCP server (stdio, 17 tools), an activation skill teaching when
to recall/record, `/recall <task>` and `/memory-report` commands. Lessons
are shared with Codex / kimi / Hermes / Python integrations via the same
local DB.

## Prerequisites (disclosed)

The plugin registers an MCP server that launches `causal-memory` from
PATH. Users install it via `pip install causal-memory` (PyPI wheels, 4
platforms) or build from the same repo (`cargo build --release`). No
other dependency; no network use by default.

## Privacy & data handling (for the review)

- All memory lives in a **local SQLite file**
  (`~/.local/share/causal-memory/causal.db`). No telemetry, no analytics,
  no third-party endpoints by default.
- The plugin payload is markdown + JSON only — no shipped code executes
  from the plugin itself; the binary comes from our repo/PyPI and runs
  locally.
- Embedding / LLM features are strictly opt-in (user-configured API keys,
  user-configured endpoints); without them, retrieval is pure local BM25.
- Soft-invalidation everywhere: recorded lessons stay auditable; nothing
  is silently deleted.

## Verification evidence

- `claude plugin validate ./plugins/claude-code --strict` → ✔ passed
- Local end-to-end: marketplace add → install → `claude mcp list` →
  `✔ Connected`; component inventory 3 skills + 1 MCP server, ~229-token
  always-on cost
- Self-audit against the directory policy checklist: no credential-store
  access, no cross-service routing, no hardcoded third-party endpoints,
  no coercive skill wording (hardened pre-submission)

## One-line pitch

"The memory that learns from its own advice: causal lessons, same-context
counterfactuals, and a prediction ledger that grades itself — all local."
