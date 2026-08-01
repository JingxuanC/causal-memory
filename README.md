# causal-memory

> **An agent memory system with a causal core — and the only one that models inhibition.**
>
> Facts, temporal state, and `decision → outcome` causal edges on one SQLite store,
> powered by a hippocampus-style engine: typed spreading activation (excitatory
> *and* inhibitory), Hebbian co-occurrence reinforcement, Q-value dynamics, and
> immutable SWR consolidation. Agents recall *what* happened, *when* it was true,
> *why* it worked — and *what would happen if* they acted differently.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status: v0.9.0](https://img.shields.io/badge/status-v0.9.0--alpha-orange.svg)](#status)

---

## Why

Every agent forgets *why* it made past decisions after a few context compactions.
It re-fixes the same bug the same wrong way, re-debates the same architecture
choice, relearns the same lesson.

This happens because **causal information is the most fragile type under text
compaction**. Real-LLM benchmark (grok-build's production compaction prompt):

| Compactions (k) | Textual recall | Causal-table recall |
|---|---|---|
| 1 | 100% | 100% |
| 2 | 85% | 100% |
| 3 | 55% | 100% |
| 5 | **45%** | **100%** |

The causal table survives because it lives **outside the agent's context window** —
compaction cannot touch it.

---

## Benchmarks

### LoCoMo (1,986 questions)

| Config | Overall | Δ vs baseline |
|---|---|---|
| V1 BM25 topk=10 (raw baseline) | 64.2% | — |
| V1 BM25 topk=10 (distill + fact layer) | 69.6% | +5.4pp |
| V2 BM25 topk=10 (7-step prompt) | 74.2% | +4.6pp |
| V2 BM25 topk=50 | 78.0% | +8.4pp |
| **V2 BM25 + semantic RRF topk=50** | **79.1%** | **+9.5pp** |

At mem0-compatible judge caliber: **84.1%** (gap to mem0 91.6% ≈ 7.5pp, largely model gap).
Gap to mem0 official 91.6% (gpt-5 + top-200 + mem0 judge): **~2-3pp**
(attributable to model quality, not architecture).

### LongMemEval (500 questions)

| Config | Multi-session | Temporal | Composed overall |
|---|---|---|---|
| distill V1 baseline | 41.4% | 69.9% | ~69.6% |
| P7 (per-noun expansion) | 50.4% | 77.4% | ~74.0% |
| **P8 (session expansion)** | **57.9%** | **77.9%** | **~75.8%** |

### Compaction survival (the experiment this system is designed for)

Text-only memory collapses 65.0% → **44.5%** after 5 compactions.
Text + never-compacted causal edges: **65.3%** (+20.8pp rescue, indistinguishable
from zero compaction). Full data in [`docs/benchmarks/locomo.md`](docs/benchmarks/locomo.md).

### Agent ablation (trap-world, end-to-end)

Same LLM (glm-4-plus, seed 42) with vs without causal memory:
**repeat-mistake rate 67% → 33%** on 2nd+ trap exposures.

### Three-model comparison (LoCoMo V2, strict judge)

| Model | Overall | Non-error accuracy |
|---|---|---|
| deepseek-chat | **74.2%** (0 errors) | 74.2% |
| deepseek-v4-pro | 48.3% (459 API timeouts) | **82.3%** |
| glm-5.2 | 56.6% | 58.1% |

---

## What makes it different

| Capability | causal-memory | mem0 | Zep | Letta | OpenViking | HeLa-Mem | Dreams |
|---|---|---|---|---|---|---|---|
| Typed causal semantics (caused/enabled/prevented) | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **prevented negative spread (inhibitory)** | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Hebbian co-occurrence edges (excitatory) | ✅ | ❌ | ❌ | ❌ | ❌ | ✅ | ❌ |
| Immutable consolidation (delta + clone) | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ✅ |
| Episodic / semantic coexistence | ✅ | ❌ | ❌ | ❌ | ❌ | ⚠️ | ✅ |
| Retrieval activation trace | ✅ | ❌ | ❌ | ❌ | ✅ | ❌ | ❌ |
| Layered loading (L0/L1/L2) + token budget | ✅ | ❌ | ❌ | ⚠️ | ✅ | ❌ | ❌ |
| Compaction survival evidence | ✅ +20.8pp | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Q-value dynamic utility | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Forward simulation (intervention_query) | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| One graph unifying all memory types | ✅ | ❌ | ⚠️ | ❌ | ⚠️ | ⚠️ | ❌ |
| Local ONNX embedding (offline) | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |

**Core innovation: the excitatory/inhibitory duality.** HeLa-Mem (ACL 2026) builds
the excitatory side (Hebbian co-activation, positive spread). causal-memory adds
the inhibitory side (`prevented` edges spread **negative** activation — a GABA
analogue). A complete memory needs both: "what caused this" *and* "what prevents
this from happening again."

**The causal graph is designed as an explicit world model.** A `caused` edge is a transition
function sample `f(state, action) → outcome`. Backward traversal (attribution) is validated
on all benchmarks; forward traversal (simulation via `intervention_query`) is implemented
but not yet benchmarked for prediction accuracy. See the
[world-model analysis](https://github.com/JingxuanC/agent-teardown/blob/main/papers/daily/2026-08-01-world-model-analysis.md).

---

## Quick start

```bash
git clone https://github.com/JingxuanC/causal-memory.git
cd causal-memory
cargo build --release
```

### MCP integration (Claude Code, Cursor, grok-build, etc.)

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

Then copy [`CLAUDE.md`](CLAUDE.md) into your project's system prompt to activate
proactive causal memory use.

### With local embeddings (no API key needed)

```bash
cargo build --release --features local-embed
# Uses BAAI/bge-small-en-v1.5 (384 dims, ~130MB, downloads once then offline)
# No CAUSAL_MEMORY_EMBED_* env vars needed — falls back to local automatically
```

### With HTTP embeddings (OpenAI/ZhiPu/etc.)

```bash
export CAUSAL_MEMORY_EMBED_API=https://open.bigmodel.cn/api/paas/v4
export CAUSAL_MEMORY_EMBED_KEY=your-key
export CAUSAL_MEMORY_EMBED_MODEL=embedding-3
```

---

## Thirteen MCP tools

| Tool | When to call | What it does |
|---|---|---|
| `record_decision` | After acting on a decision | Logs `decision → outcome` as a causal edge with relation type, confidence, and task tag |
| `search_causal` | Before a non-trivial decision | BM25 + optional semantic retrieval of past causal episodes. Supports `detail_level` (L0/L1/L2) and `max_tokens` budget |
| `record_fact` | When learning a stable fact | Records flat facts (preferences / tech stack) with scope + confidence; idempotent, optional same-key retirement |
| `search_facts` | When you need "what is" info | BM25 + optional semantic retrieval over the fact layer |
| `search_memory` | When unsure which type | **Unified retrieval**: facts + causal lessons fused by Reciprocal Rank Fusion (RRF) |
| `trace_cause` | When something fails (simple) | Single-hop reverse: which decision caused this outcome |
| `trace_cause_chain` | When something fails (deep) | Multi-hop backward traversal through the causal graph |
| `trace_cause_cross_session` | Cross-session failure analysis | Meta-causal bridges connect chains across task boundaries |
| `invalidate_decision` | When a lesson turns out wrong | Soft-invalidate (hidden from search, kept for audit) |
| `search_patterns` | To recall cross-task lessons | Mined meta edges: `similar_to` / `repeated` / `contradicts` / `refines` |
| `causal_directory` | Pinned in system prompt | L0 compact pointer list so the agent always knows what it holds |
| `intervention_query` | **Before taking an action** | Pearl Rung-2: predicts outcomes of similar past actions (safe/warning/danger). **Forward simulation** |
| `counterfactual_query` | When choosing between options | Contrastive empirical counterfactual: compares recorded outcomes of decision vs alternative |
| `reconstruct_lesson` | To get a distilled lesson | Reconstructive retrieval: Markov-blanket subgraph → LLM narrative |

---

## Architecture

```
                    ┌─────────────────────────────────┐
                    │       causal-memory (Rust)       │
                    │                                  │
  Agent ←(MCP)────→│  13 tools                        │
                    │    ↓                             │
                    │  Unified search (RRF fusion)     │
                    │    ↓              ↓              │
                    │  BM25 + cosine   Fact layer      │
                    │    ↓              ↓              │
                    │  ┌──── Hippocampus engine ────┐  │
                    │  │ CSR graph + spreading act. │  │
                    │  │  caused (+1.0)  enabled (+0.5)│
                    │  │  prevented (-0.3) ← GABA    │  │
                    │  │  fact (+0.8)  meta (+0.6)   │  │
                    │  │  co_occurrence (+Hebbian)   │  │
                    │  │ DG SimHash · CA1 novelty    │  │
                    │  │ SWR consolidate (immutable) │  │
                    │  │ Q-value dynamics (MemRL)    │  │
                    │  └─────────────────────────────┘  │
                    │    ↓                             │
                    │  SQLite (causal.db)              │
                    └─────────────────────────────────┘
```

The `causal_edges` table is never compacted — it lives outside the agent's context
window. That's the entire point.

---

## Edge types (typed-edge taxonomy)

| Edge type | Spread coeff | Biological analogue | Status |
|---|---|---|---|
| `caused` | +1.0 | Glutamate (strong excitatory) | ✅ |
| `fact` | +0.8 | Semantic association | ✅ |
| `meta` | +0.6 | Cortical top-down | ✅ |
| `enabled` | +0.5 | Weak excitatory | ✅ |
| `co_occurrence` | +0.2 × w(t) | Hebbian LTP (dynamic) | ✅ |
| **`prevented`** | **−0.3** | **GABA (inhibitory)** | ✅ unique |
| `no_effect` | 0.0 | No connection | ✅ |

---

## Sleep consolidation

```bash
causal-memory sleep --dry-run   # preview what would change
causal-memory sleep             # run consolidation cycle
```

Immutable SWR 2.0: produces a delta + clone (original graph untouched), with full
audit log. Triple-criterion GC (weak AND dormant AND zero-access). Triggers
automatically when novelty entropy exceeds threshold.

---

## Research background

This project is the engineering output of 17 research notes on agent memory
architecture ([insights/01-17](https://github.com/JingxuanC/agent-teardown/tree/main/insights)),
a teardown of 7 production agent frameworks, and deep analysis of 10+ memory
research papers. Key references:

- **HeLa-Mem** (ACL 2026) — Hebbian spreading activation (our closest competitor; we add the inhibitory side)
- **Anthropic Dreams API** — immutable consolidation pattern (aligned in SWR 2.0)
- **OpenViking** (VLDB 2026) — layered loading L0/L1/L2 (absorbed into retrieval)
- **MemRL** (arXiv:2601.03192) — Q-value memory dynamics (implemented as P4)
- **Graph World Models** (arXiv:2604.27895) — causal-memory maps to "Graph as Reasoner"

Design docs: [`docs/complete-memory-system.md`](docs/complete-memory-system.md),
[`docs/hippocampus-design.md`](docs/hippocampus-design.md),
[`docs/architecture.md`](docs/architecture.md).

---

## Build & test

```bash
cargo build --release                    # Build binary
cargo test                              # Run 170 tests (default features)
cargo test --features local-embed       # Run with ONNX embedding tests
cargo clippy --workspace -- -D warnings # Lint (rust-skills baseline)
```

Workspace lints configured in `Cargo.toml`: correctness/suspicious → deny,
style/complexity/perf → warn, unwrap_used → warn.

---

## Status

**v0.9.0 — alpha.** What works:

- ✅ 13 MCP tools + cross-session tracing
- ✅ SQLite persistence with idempotent migrations (schema v7)
- ✅ Fact layer + unified RRF retrieval + LLM distill ingest
- ✅ Hippocampus engine: CSR spreading activation, DG SimHash, CA1 novelty, SWR 2.0 (immutable)
- ✅ Edge types: caused/enabled/prevented/fact/meta/co_occurrence
- ✅ Hebbian co-occurrence reinforcement (P2)
- ✅ Q-value Bellman dynamics (P4)
- ✅ Novelty-entropy consolidation trigger (P6)
- ✅ Layered loading L0/L1/L2 + token budget (P5)
- ✅ Semantic retrieval: HTTP (ZhiPu/OpenAI) + local ONNX (fastembed)
- ✅ Multi-hop backward traversal + cross-session meta bridges
- ✅ Offline consolidation ("sleep") with immutable delta + clone
- ✅ Cross-agent sharing (export/import, redacted + idempotent)
- ✅ Benchmark harnesses: LoCoMo (79.1%), LongMemEval (~75.8%), Memora, compaction survival, agent ablation
- ✅ 170 tests + clippy clean

What's not done yet:

- ❌ Python/TS bindings (PyO3 planned)
- ❌ HTTP transport (MCP stdio only)
- ❌ Rung 3 SCM counterfactuals (out of scope per [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md))
- ❌ Forward-simulation benchmark (designed, not yet run)

Roadmap: [`docs/roadmap.md`](docs/roadmap.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
