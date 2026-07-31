# causal-memory

> **A complete agent memory system with a causal core.** Facts and preferences, temporal state, and `decision → outcome` causal edges — one SQLite store, one hippocampus-style engine (typed spreading activation + SWR consolidation) — so agents recall *what* happened, *when* it was true, and *why* it worked.
>
> `causal-memory` started as the layer that stores *why* — the causal link every other framework (Mem0, Zep, Letta, OpenViking, MemOS) misses — and proved that slice survives compaction when text memory collapses. That layer is now the core of a full memory system: the causal graph stays the skeleton, and factual/temporal memory grows on the same neural-inspired machinery.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status: v0.9.0](https://img.shields.io/badge/status-v0.9.0--alpha-orange.svg)](#status)

## Why

Every agent has the same problem: after N compactions, it forgets *why* it made past decisions. It reverts to a state where the same bug gets fixed the same wrong way, the same architecture choice gets re-debated, the same lesson gets relearned.

This happens because **causal information is the most fragile type under text compaction**. Real-LLM benchmark (using grok-build's production compaction prompt):

| Compactions (k) | Textual recall | Causal-table recall |
|---|---|---|
| 1 | 100% | 100% |
| 2 | 85% | 100% |
| 3 | 55% | 100% |
| 5 | **45%** | **100%** |

The causal table doesn't decay because it lives outside the agent's context window — compaction cannot touch it. See [`docs/design.md`](docs/design.md), [`docs/architecture.md`](docs/architecture.md) (the current system map), and the [full benchmark writeup](https://github.com/JingxuanC/agent-teardown/blob/main/spike/grok-causal-memory/bench-RESULTS.md).

## Benchmarks

**LoCoMo** (1,986 questions, deepseek-chat answerer + judge, frozen protocol): **distill + fact layer 69.6%** vs raw-ingest 64.2% (**+5.4pp**), same harness/judge/protocol — gains concentrate where atomic facts should help (temporal +11.6pp, multi-hop +11.5pp), abstention intact at 91.9%. Honest reading: the design doc predicted 75–80%; cat-1 list completeness and cat-3 counterfactual/abstention mismatch are the documented bottlenecks. Full methodology and failure analysis in [`docs/benchmarks/locomo.md`](docs/benchmarks/locomo.md).

**LongMemEval** (500 questions, official judge templates): **distill + fact layer 69.6%** vs raw-ingest 61.8% (**+7.8pp**) — knowledge-update **76.9% → 85.9%** (the supersedes mechanism's home turf), preference 23.3% → 36.7%, abstention unchanged. Methodology and per-type analysis in [`docs/benchmarks/longmemeval.md`](docs/benchmarks/longmemeval.md).

**Memora** (ACL 2026, weekly scale, 10 personas, forgetting-aware): distill + fact layer MPA **33.9% → 46.8%** (+12.9pp) · FAA 72.1% · mean FAMA 31.0 — raw baselines and the FAA trade-off analysis in [`docs/benchmarks/memora.md`](docs/benchmarks/memora.md).

**Compaction survival** (the experiment this system is designed for): LoCoMo sessions compressed 5× before QA — text-only memory collapses 65.0% → **44.5%**; text + never-compacted causal edges holds at **65.3%** (+20.8pp rescue, indistinguishable from zero compaction). Full data in [`docs/benchmarks/locomo.md`](docs/benchmarks/locomo.md#compaction-survival-run-20260727_174000-k--5).

**Agent ablation** (end-to-end, `causal-memory bench-agent --tasks 6 --steps 12 --condition both`): the same LLM agent (glm-4-plus) solves seeded trap-family tasks with vs without causal memory. Repeat-mistake rate on 2nd+ trap exposures: **67% without memory → 33% with memory** (both groups 6/6 solved; post-search first-action hit rate 57%). Reading: the memory tax is ~1 extra step per task; the payoff is not re-stepping into a trap you already fell into. Raw results + full transcripts in [`benches/agent/results/`](benches/agent/results/).

## What it does

Thirteen MCP tools. Small surface area is the point.

| Tool | When to call | What it does |
|---|---|---|
| `record_decision` | After completing an action | Logs `decision → outcome` as a causal edge with task tag + confidence; auto-invalidates contradicted older edges for the same decision |
| `search_causal` | Before a non-trivial decision | Retrieves past causal episodes by task or text; ranks by embedding cosine similarity when configured, BM25 otherwise |
| `record_fact` | When learning a stable fact | Records flat facts (preferences / tech stack / config) with scope + confidence; idempotent on (key, value, scope), optional same-key retirement of outdated values |
| `search_facts` | When you need "what is" info | Retrieves facts by semantic similarity when configured, BM25 otherwise; without a query, lists the most recently updated facts |
| `search_memory` | When unsure which memory type | Unified retrieval: facts + causal lessons fused by Reciprocal Rank Fusion (RRF) into one ranked list |
| `trace_cause` | When something fails (simple) | Single-hop reverse: which decision caused this outcome |
| `trace_cause_chain` | When something fails (deep) | Multi-hop backward traversal through the causal graph |
| `invalidate_decision` | When a recorded lesson turns out wrong | Soft-invalidates the edge (`valid_to` set) — hidden from search/trace, kept for audit |
| `search_patterns` | To recall cross-task lessons | Searches mined meta edges: `similar_to` / `repeated` / `contradicts` / `refines`, with confounded / Simpson flags |
| `causal_directory` | Pinned in the system prompt | L0 compact pointer list of recent decisions so the agent always knows what experience it holds |
| `intervention_query` | Before taking an action | Pearl Rung-2: predicts what outcomes similar past actions caused, labeled safe / warning / danger, with task_tag-stratified confound check |
| `counterfactual_query` | When choosing between two options | Contrastive (empirical) counterfactual: compares recorded outcome distributions of decision vs alternative — explicitly not an SCM counterfactual |
| `reconstruct_lesson` | To get the distilled lesson of an episode | Reconstructive retrieval: Markov-blanket causal subgraph + LLM lesson narrative, with optional multi-sample calibration |

**Multi-hop example**: "service crashed" ← "OOM" ← "cache had no TTL" ← "Redis configured without expiry". `trace_cause` finds the first hop. `trace_cause_chain` walks the full chain.

## Quick start

```bash
git clone https://github.com/JingxuanC/causal-memory.git
cd causal-memory
cargo build --release
```

Wire into any MCP-compatible agent (Claude Code, Cursor, grok-build, etc.):

```json
{
  "mcpServers": {
    "causal-memory": {
      "command": "/path/to/causal-memory/target/release/causal-memory",
      "env": {
        "CAUSAL_MEMORY_DB": "~/.local/share/causal-memory/causal.db"
      }
    }
  }
}
```

Then copy [`CLAUDE.md`](CLAUDE.md) into your project's system prompt to activate proactive causal memory use. Per [research notes](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md#L143): agents don't proactively call memory tools without explicit prompt instruction.

## How it's different

| Memory type | Stores | Example | Who does it |
|---|---|---|---|
| Flat facts | User preferences | "User prefers TypeScript" | Mem0 |
| Temporal facts | State changes over time | "User was on Pro plan in March" | Zep |
| Entity-relation graph | Typed relations between entities | "User lives in Berlin" | Mem0g, Mnemis |
| Text consolidation | Markdown notes, tidied offline | "Dreaming" merges/prunes notes | Claude Code Auto Dream |
| Self-managed | Agent edits its own memory | Agent decides what to remember | Letta |
| File system | Memory as virtual FS | Directory-based retrieval | OpenViking |
| **Causal** | **Decision → outcome links** | **"Mutex lock caused deadlock"** | **causal-memory (this)** |

causal-memory started in the last row. It is growing to cover the others — but with a different architecture: not separate stores per memory type, but **one graph with typed edges** (fact / state / causal / co-occurrence) processed by one engine.

**From slice to system.** The causal layer was the beachhead, not the destination. The compaction-survival experiment proved causal edges are the most compaction-resistant memory type; the hippocampus merge proved typed spreading activation works; and the fact/preference layer is now **landed and benchmarked**: LLM-distilled facts route to a scoped `agent_facts` store with supersedes retirement, lessons/events keep flowing to the causal layer — and the same-harness deltas are LoCoMo **+5.4pp**, LongMemEval **+7.8pp**, Memora MPA **+12.1pp** (see [Benchmarks](#benchmarks)). What stays exclusive to this system: typed causal weights (`prevented` edges spread **negative** activation — no other system does inhibitory spread), compaction survival as a first-class benchmark, and consolidation modeled on sharp-wave ripples rather than text distillation.

The 2026 agent-memory landscape now has cloud-API (Mem0), filesystem (Letta, OpenViking), temporal-graph (Zep), and associative (HeLa-Mem's Hebbian graph, ACL 2026) entrants. causal-memory's claim: **memory types should share one neural-inspired substrate** — hippocampal episodic traces consolidated into neocortical semantic patterns — rather than four independent stores glued together by the agent. HeLa-Mem builds the excitatory side (Hebbian co-activation); this system adds the inhibitory side (`prevented` negative spread). A complete memory needs both.

## Data path

- Default: `~/.local/share/causal-memory/causal.db`
- Override: `CAUSAL_MEMORY_DB` env var

SQLite file. Portable. No server process. Your data stays on your machine.

## Architecture

```
Agent ←(MCP stdio)→ causal-memory → SQLite (causal_edges table)
```

The `causal_edges` table is never compacted — it's outside the agent's context window. That's the entire point: text compaction cannot destroy what it cannot reach.

## Sleep consolidation

An offline "sleep" cycle (reactivation → generalization → downscaling → REM
integration) that decays stale confidence, garbage-collects low-value edges,
merges duplicates, and mines cross-task patterns:

```bash
causal-memory sleep --dry-run   # preview what would change
causal-memory sleep             # run it (once per day — NOT idempotent)
```

## Semantic search

Optional embeddings rank `search_causal` by cosine similarity instead of BM25
matching. Any OpenAI-compatible `/v1/embeddings` endpoint works:

```bash
export CAUSAL_MEMORY_EMBED_API=https://api.openai.com/v1   # default: CAUSAL_MEMORY_LLM_API
export CAUSAL_MEMORY_EMBED_KEY=sk-...                      # default: CAUSAL_MEMORY_LLM_KEY
export CAUSAL_MEMORY_EMBED_MODEL=text-embedding-3-small    # optional

causal-memory embed            # backfill embeddings for existing edges
causal-memory embed --limit 50 # partial backfill
```

Unconfigured → automatic fallback to keyword search. Zero-invasive by default.

Existing databases are upgraded automatically on open (schema v5); run
`causal-memory migrate` for an explicit check.

## Sharing & benchmarks

Share causal memory between agents (e.g. a team's agents pooling lessons) as
versioned JSONL, with best-effort secret redaction on export and idempotent,
content-keyed import:

```bash
causal-memory export lessons.jsonl --task-tag caching --min-confidence 0.5
causal-memory import lessons.jsonl --task-tag agent-b   # tag the source
causal-memory import lessons.jsonl --dry-run            # stats only, no writes
```

Reproduce the compaction-degradation benchmark above on your own LLM:

```bash
export CAUSAL_MEMORY_LLM_API=https://api.deepseek.com/v1
export CAUSAL_MEMORY_LLM_KEY=sk-...
causal-memory bench-compaction --compressions 5 --seed 42
# prints the recall table and writes bench-results-<timestamp>.md
```

Run the end-to-end agent ablation (same LLM, with vs without causal memory):

```bash
causal-memory bench-agent --tasks 6 --steps 12 --condition both --seed 42
# writes benches/agent/results/bench-agent-{results,transcript-*}-<timestamp>.md
```

## Status

**v0.9.0 — alpha.** What works:

- ✅ Thirteen MCP tools (record / search / facts / unified / trace / chain-trace / invalidate / patterns / L0 directory / intervention / counterfactual / reconstruct)
- ✅ SQLite persistence with CHECK constraints + idempotent schema migrations (v6)
- ✅ **Fact layer** (`agent_facts`, unified-memory-design Phase 1): flat facts with scope + soft invalidation, idempotent upsert, same-key retirement, BM25 + optional embedding retrieval
- ✅ **Unified retrieval** (`search_memory`, Phase 2): RRF fusion of facts + causal layers, one query embedding serving both
- ✅ **LLM distill ingest** (`causal-memory distill <session.json|dir>`, Phase 3): one LLM call per session extracts facts → `agent_facts` (with supersedes retirement) + lessons/events → causal store
- ✅ **Parameterized queries** (no SQL injection risk)
- ✅ Confidence levels (temporal / rule / llm_inferred / user_feedback)
- ✅ Task-aware retrieval + optional **semantic (embedding) retrieval** with keyword fallback
- ✅ **Multi-hop backward traversal** via recursive CTE
- ✅ Rule-based **decision auto-extractor** for grok-build session logs
- ✅ **Invalidation**: manual (`invalidate_decision`) + automatic contradiction short-circuit (stored-polarity aware)
- ✅ **Write-time outcome polarity** (LLM judge + heuristic fallback, `mixed` category) driving labels and contradiction checks
- ✅ **BM25 keyword retrieval** as the default text-query ranking (semantic path unchanged, LIKE demoted to tag-only listing)
- ✅ **Dual-system memory**: offline pattern miner with **stratified replication test** (confounded / Simpson flags)
- ✅ **Offline consolidation ("sleep") cycle** with real replay: reactivation priority feeds decay protection and cross-cycle marking
- ✅ **L0 causal directory** + **Rung-2 intervention queries** with stratified confound warning
- ✅ **Contrastive counterfactuals** (`counterfactual_query`) + **reconstructive retrieval** (`reconstruct_lesson` with optional calibration)
- ✅ **Cross-agent sharing** (`export` / `import`, redacted + idempotent)
- ✅ **Benchmarks**: LoCoMo harness (see [Benchmarks](#benchmarks)) + reproducible `bench-compaction`
- ✅ 218 tests (215 unit + 3 e2e suites: migration / pipeline / MCP stdio)

What's not done yet (honest):

- ❌ Python/TS bindings (Rust binary only)
- ❌ HTTP transport (MCP stdio only)
- ❌ LongMemEval benchmark integration (LoCoMo done — see [Benchmarks](#benchmarks))
- ❌ Rung 3 **SCM** counterfactuals — structural-causal-model reasoning stays
  out of scope per
  [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md);
  we ship only the contrastive/empirical subset (`counterfactual_query`)
- ❌ Not yet wired into a production agent end-to-end

Roadmap: see [docs/roadmap.md](docs/roadmap.md).

## Build & test

```bash
cargo build --release    # Build binary
cargo test               # Run unit tests
```

## Research background

This project is the engineering output of 13 research notes on agent memory architecture:
- [Agent's Second Law — anti-degradation (information theory)](https://github.com/JingxuanC/agent-teardown/blob/main/insights/04-anti-entropy.md)
- [LLM is a stateless function](https://github.com/JingxuanC/agent-teardown/blob/main/insights/09-stateless-function.md)
- [Memory company landscape (Letta/Mem0/Zep/OpenViking/MemOS)](https://github.com/JingxuanC/agent-teardown/blob/main/insights/10-memory-frameworks.md)
- [Causal state store — the design this implements](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)
- [Real LLM compaction benchmark](https://github.com/JingxuanC/agent-teardown/blob/main/papers/02-compaction-degradation.md)

Papers that shaped specific design decisions: [`docs/research-backdrop.md`](docs/research-backdrop.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
