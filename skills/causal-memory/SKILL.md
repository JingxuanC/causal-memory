---
name: causal-memory
description: Causal memory for agents — record decisions/outcomes and recall them before acting. Trigger when the agent has causal-memory MCP tools available and is about to make a non-trivial decision (architecture, debugging approach, library/deployment choice), when something fails unexpectedly, or when the user asks to "remember" something.
version: v1.0
---

# Causal Memory Integration

You have access to a **causal memory layer** via MCP tools (`record_decision`,
`search_causal`, …). It records your past decisions and their outcomes as
causal edges, so you learn from experience across sessions and survive
context compaction.

## Setup (if the tools are not configured yet)

`pip install causal-memory` puts the full CLI on PATH as `causal-memory`.
Point any MCP client at it — bare invocation runs the stdio MCP server:

```json
{
  "mcpServers": {
    "causal-memory": {
      "command": "causal-memory",
      "env": { "CAUSAL_MEMORY_DB": "~/.local/share/causal-memory/causal.db" }
    }
  }
}
```

Remote / multi-agent shared memory: `causal-memory http --port 9938`
(Streamable HTTP at `/mcp`; set `CAUSAL_MEMORY_ALLOWED_HOSTS` for non-localhost
access). The console script is the full CLI: `stats`, `sleep`, `distill`,
`export`/`import` — see `causal-memory --help`.

## The core loop (four tools cover 90% of usage)

**Before any non-trivial decision** (architecture choice, debugging approach,
library selection, deployment strategy):

1. Call `search_memory` with your query — it RRF-fuses facts AND causal
   lessons in one call. Need causal lessons specifically? `search_causal`
   with the relevant `task_tag`.
2. For risky or irreversible actions, also call `intervention_query` — it
   forward-simulates what similar past actions caused (safe / warning /
   **danger**).
3. If past experience is relevant, incorporate it into your approach.

**After acting on a decision and observing the result:**

4. Call `record_decision` with `decision`, `outcome`, `relation`
   (caused / enabled / prevented / no_effect), `task_tag`, and
   `confidence_source` (temporal / rule / llm_inferred / user_feedback).
   **Record surprising outcomes especially — those are the most valuable
   lessons.**

**Stable facts** (preferences, tech stack, config): `record_fact` with
`key` / `value` / `scope` (user / session / agent). Replacing an older fact?
`replace_same_key: true`. Retrieve with `search_facts` / `search_memory`.

## Failure postmortem

5. Something failed unexpectedly → `trace_cause` (which past decision caused
   this); root cause more than one hop away → `trace_cause_chain`.

## Corrections

6. A recorded lesson turned out wrong → `invalidate_decision` (soft-delete:
   hidden from search, kept for audit). Wrong cross-task pattern →
   `invalidate_pattern`.

## Rules of engagement

- **Do NOT ask the user before searching or recording — do it proactively.**
- Keep `task_tag` consistent within a domain (e.g. `deployment`,
  `git-workflow`) — stratified mining depends on it.
- Prefer `search_memory` when unsure which layer holds what you need.

Full tool reference (16 tools): `causal_directory`, `search_patterns`,
`counterfactual_query`, `reconstruct_lesson`, `remember`, `resolve_updates`
and the rest — see the repo README "Sixteen MCP tools" section.
