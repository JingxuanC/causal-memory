# Hermes Agent — The Memory Provider Ecosystem Play

> **Source**: NousResearch `hermes-agent` — docs, GitHub issues, and the LanceDB
> memory plugin (surveyed 2026-07-31).
>
> - Memory providers docs: https://hermes-agent.nousresearch.com/docs/user-guide/features/memory-providers
> - Provider guide (plugins dir): https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/memory-providers.md
> - Tiered memory proposal: https://github.com/NousResearch/hermes-agent/issues/22551 (2026-05-09)
> - LanceDB plugin + LongMemEval harness: https://github.com/lancedb/hermes-agent-memory (2026-05-25)
> - LanceDB writeup: https://www.lancedb.com/blog/semantic-memory-for-hermes-agent-with-lancedb
> - mem0 pairing writeups: https://mem0.ai/blog/how-memory-works-in-hermes-agent-(and-how-to-improve-it) (2026-07-18)
>
> **Type**: ecosystem analysis (not a paper). Tracked because Hermes Agent is
> the first agent runtime to make long-term memory a **first-class plugin
> slot**, which changes the distribution question for causal-memory.

---

## What Hermes Agent does

Hermes Agent (NousResearch) is a personal-agent runtime in the OpenClaw
family. Its memory story is layered, and the important layer is the open one:

| Layer | Implementation | Status |
|---|---|---|
| Curated memory | `MEMORY.md` + `USER.md`, agent-written, frozen into the system prompt (~800 / ~500 token budgets) | built-in |
| Session search | full transcripts in local SQLite FTS5, on-demand query | built-in |
| Tiered memory | ST-M (20s TTL) / MT-M (20min) / LT-M with promotion heuristics | proposed (issue #22551) |
| Self-learning | cited in survey papers as a "five-layer self-learning memory architecture" with a self-evolution loop | in progress |
| **External memory provider slot** | `memory.provider` config + plugin interface; Hindsight, Memori, Supermemory, Honcho, LanceDB, mem0 already shipping providers | **open, ecosystem forming** |

Three design choices matter for us:

### 1. The provider slot is a distribution channel, not a competitor

Hermes deliberately does **not** try to build the best memory algorithm
in-house. It standardizes the slot and lets memory projects compete for it.
Every provider ships the same shape: passive per-turn ingest (sync path),
optional prefetch auto-injection, and provider-specific tools
(`hindsight_retain/recall/reflect`, `lancedb_remember/recall/forget`, ...).

**None of the current providers stores causal relationships.** Hindsight is
an entity-relation knowledge graph; LanceDB/mem0/Supermemory are flat-fact
stores; Honcho is dialectic user modeling. The entire slot is "what happened"
— the "why" slice is unoccupied.

### 2. Recall modes operationalize the proactivity problem

Providers choose among three recall modes:

| Mode | Auto-inject from prefetch | Provider tools | Fit |
|---|---|---|---|
| `context` | yes | no | hands-off, predictable context |
| `tools` | no | yes | model chooses when to retrieve |
| `hybrid` | yes | yes | richest context, highest token use |

This is the engineering answer to the finding in
[insights/13](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)
that agents do not proactively call memory tools. It maps directly onto our
own split: `causal_directory` / L0 injection is the passive path;
`search_causal` / `intervention_query` are the tools path. A causal-memory
Hermes provider should ship `hybrid` semantics from day one.

### 3. Providers are now expected to arrive with benchmarks

The LanceDB plugin bundles a LongMemEval-S harness comparing five retrieval
variants (FTS5 baseline / vector / hybrid-RRF / hybrid-linear / cross-encoder)
with isolated stores per variant. The bar for entering this ecosystem is
"bring your own benchmark comparison." causal-memory already exceeds it:
LoCoMo (5 frozen-protocol runs), LongMemEval, compaction survival, τ²-bench,
and the agent ablation — no current Hermes provider has anything close to the
compaction-survival experiment.

## Connection to `causal-memory`

### Threat

Hermes built-in layers + a graph provider (Hindsight) cover the
fact / entity-relation territory of our
[complete-memory-system direction](../../roadmap.md#direction-complete-memory-system-planned-phases).
If Hindsight becomes the default provider, its PostgreSQL + knowledge-graph
shape sets user expectations for what "memory" means in this ecosystem.

### Opportunity (larger)

The provider slot is the cheapest distribution channel available to us:

1. **The causal slice is empty in this ecosystem.** Typed causal edges,
   `prevented` negative spread, and compaction survival have no incumbent.
2. **Our benchmark suite is the strongest entry ticket.** The expected
   comparison structure ("Hermes built-in FTS5 baseline vs provider") is
   exactly our existing narrative: "Hermes built-in memory after k=5
   compactions vs Hermes + causal layer."
3. **Our MCP stdio design doesn't fit the slot directly** — Hermes providers
   are in-process Python plugins. This raises the priority of the planned
   PyO3 Python bindings (roadmap v1.0) above HTTP transport.

### Action items

- [ ] Hermes provider plugin on top of PyO3 bindings (`causal_memory_retain`
      / `causal_memory_recall` / `causal_memory_reflect`-style tools, hybrid
      recall mode)
- [ ] Reprioritize: Python bindings before HTTP transport in the v1.0 plan
- [ ] Benchmark variant mirroring the LanceDB structure: Hermes FTS5 baseline
      vs causal-memory provider on LongMemEval + a compaction-survival run
      inside the Hermes stack
