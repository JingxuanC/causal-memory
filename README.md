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
[![Tests: 206](https://img.shields.io/badge/tests-206-brightgreen.svg)](#build--test)

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

### Traditional benchmarks (fact-recall, vs mem0)

| Benchmark | Score | Baseline | Δ | vs mem0 |
|---|---|---|---|---|
| **LoCoMo** (1,986q, strict judge, deepseek-v4-pro) | **79.1%** | 69.6% | +9.5pp | 91.6% (−12.5pp) |
| **LoCoMo** (mem0-judge) | **84.1%** | — | — | 91.6% (−7.5pp) |
| **LoCoMo** (1,986q, distill, v2, deepseek-chat, 2026-08-05) | **73.8%** | — | — | — |
| **LongMemEval** (500q, raw ingest) | **70.8%** overall / 88.6% evidence-hit | — | — | — |
| **LongMemEval** (500q, distill, v2, deepseek-chat, 2026-08-05) | **75.2%** | 74.4% (pre-fix) | +0.8pp | — |
| **Memora** MPA (10 personas) | **67.4%** | 47.0% | +20.4pp | 71.8% (−4.4pp) |
| **Memora** FAMA | **49.0** | 31.0 | +18.0 | — |
| Compaction survival | **100%** | 45% | — | — |
| Agent repeat-mistake | **33%** | 67% | −34pp | — |
| **bench-memory** (synthetic, LLM end-to-end, deepseek-chat) | fact recall 80% · causal recall 100% · chain 100% | — | — | — |

**Memora improvement breakdown** (47.0% → 67.4%, +20.4pp from 6 architecture changes):

| Layer | Change | MPA gain |
|---|---|---|
| Write-time gatekeeping | raw turns → `session_logs` (not `chunks`) | +3.9pp |
| Extraction cap | 10→30 items/session, tokens 1500→4000 | +10.5pp |
| V3 extraction prompt | 130-line prompt with 6 rules + 5 few-shot | +3.5pp |
| Dedup context | 50-item sequential context window | +1.5pp |
| Semantic RRF | BM25 + ZhiPu embedding cosine fusion | +1.0pp |

### Causal capability benchmark (unique to causal-memory)

These test capabilities that **no fact store (mem0, Zep, Letta) can offer**.
206 tests, all passing.

| Capability | What it proves | Tests |
|---|---|---|
| **Prevented-edge warning** | `prevented` edge spreads −0.3 activation (GABA analogue); produces "this is blocked" warnings | 2 |
| **Trace-cause attribution** | Backward CSR traversal finds root cause (direct + indirect) | 2 |
| **Multi-hop causal chain** | Forward K-hop spreading reaches 2-3 hop outcomes | 2 |
| **Inhibitory filtering** | Prevented outcomes appear as negative, not false positives | 1 |
| **Mixed-signal disambiguation** | Same node gets + (caused) or − (prevented) based on query | 1 |
| **Intervention comparison** | Same outcome (crash) has +0.9 for "skip tests" and −0.3 for "add tests" | 4 |
| **SWR consolidation** | LTP strengthens replayed edges, LTD weakens unvisited, GC forgets dormant | 5 |
| **Q-value dynamics** | Good decisions (reward=1.0) rank higher; Bellman propagates to parents | 3 |
| **Novelty entropy** | Diverse experience (entropy > 0.6) triggers consolidation; uniform does not | 3 |
| **Sleep-wake cycle** | Memory system evolves over time through consolidation feedback loop | 1 |
| **Meta-edge mining** | Cross-session pattern discovery (similar_to / repeated) | 3 |
| **Hebbian co-occurrence** | Repeated co-activation strengthens connection; non-co-active decays | 3 |
| **Edge type round-trip** | caused + enabled + prevented all survive SQLite serialization | 2 |
| **Trace cause in store** | trace_cause finds root cause through multi-hop chain | 1 |

---

## What makes it different

| Capability | causal-memory | mem0 | Zep | Letta | HeLa-Mem |
|---|---|---|---|---|---|
| Typed causal semantics (caused/enabled/prevented) | ✅ | ❌ | ❌ | ❌ | ❌ |
| **prevented negative spread (inhibitory)** | ✅ | ❌ | ❌ | ❌ | ❌ |
| Hebbian co-occurrence edges (excitatory) | ✅ | ❌ | ❌ | ❌ | ✅ |
| Immutable consolidation (delta + clone) | ✅ | ❌ | ❌ | ❌ | ❌ |
| Q-value dynamic utility | ✅ | ❌ | ❌ | ❌ | ❌ |
| Forward simulation (intervention_query) | ✅ | ❌ | ❌ | ❌ | ❌ |
| SWR offline consolidation (LTP/LTD/GC) | ✅ | ❌ | ❌ | ❌ | ❌ |
| Novelty-entropy consolidation trigger | ✅ | ❌ | ❌ | ❌ | ❌ |
| Meta-edge cross-session pattern mining | ✅ | ❌ | ❌ | ❌ | ❌ |
| Compaction survival evidence | ✅ +20.8pp | ❌ | ❌ | ❌ | ❌ |
| One graph unifying all memory types | ✅ | ❌ | ⚠️ | ❌ | ⚠️ |
| Write-time gatekeeping (raw → session_logs) | ✅ | ✅ | ❌ | ❌ | ❌ |
| Local ONNX embedding (offline) | ✅ | ✅ | ❌ | ❌ | ❌ |

**Core innovation: the excitatory/inhibitory duality.** HeLa-Mem (ACL 2026) builds
the excitatory side (Hebbian co-activation, positive spread). causal-memory adds
the inhibitory side (`prevented` edges spread **negative** activation — a GABA
analogue). A complete memory needs both: "what caused this" *and* "what prevents
this from happening again."

---

## Architecture

```
  ┌───────────────────────────────────────────────┐
  │           causal-memory (Rust, MCP)            │
  │                                                │
  │  13 tools ← Agent (stdio)                      │
  │    ↓                                           │
  │  Write-time gatekeeping                        │
  │    raw turns → session_logs (audit only)       │
  │    distill → facts + causal edges (searchable) │
  │    ↓                                           │
  │  Unified retrieval (RRF fusion)                │
  │    BM25 + semantic cosine → RRF merge          │
  │    Fact layer (BM25 + embeddings)              │
  │    ↓                                           │
  │  ┌──── Hippocampus engine ──────────────────┐  │
  │  │ CSR graph + spreading activation          │  │
  │  │  caused (+1.0)   enabled (+0.5)           │  │
  │  │  prevented (−0.3) ← GABA inhibitory       │  │
  │  │  fact (+0.8)     meta (+0.6)              │  │
  │  │  co_occurrence (Hebbian, dynamic)         │  │
  │  │                                            │  │
  │  │ DG: SimHash pattern separation             │  │
  │  │ CA3: K-hop spreading (forward + reverse)   │  │
  │  │ CA1: Novelty entropy trigger               │  │
  │  │ SWR: LTP/LTD/GC (immutable delta + clone)  │  │
  │  │ Q-value: Bellman dynamics (MemRL-style)    │  │
  │  └────────────────────────────────────────────┘  │
  │    ↓                                           │
  │  SQLite (causal.db) — never compacted          │
  └───────────────────────────────────────────────┘
```

The `causal_edges` table is never compacted — it lives outside the agent's context
window. That's the entire point.

---

## Edge types

| Edge type | Spread coeff | Biological analogue | Meaning |
|---|---|---|---|
| `caused` | +1.0 | Glutamate (strong excitatory) | "Doing X caused Y" |
| `fact` | +0.8 | Semantic association | "User is/has Z" |
| `meta` | +0.6 | Cortical top-down | Cross-task pattern link |
| `enabled` | +0.5 | Weak excitatory | "Doing X enabled Y" |
| `co_occurrence` | dynamic | Hebbian LTP | "X and Y frequently co-occur" |
| **`prevented`** | **−0.3** | **GABA (inhibitory)** | **"Doing X prevented Y"** |
| `no_effect` | 0.0 | — | No causal relationship |

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

### With local embeddings (no API key needed)

```bash
cargo build --release --features local-embed
# Uses BAAI/bge-small-en-v1.5 (384 dims, ~130MB, downloads once then offline)
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
| `record_decision` | After acting on a decision | Logs `decision → outcome` as a causal edge with relation type |
| `search_causal` | Before a non-trivial decision | BM25 + semantic retrieval of past causal episodes |
| `record_fact` | When learning a stable fact | Records flat facts with scope + confidence; idempotent |
| `search_facts` | When you need "what is" info | BM25 + semantic retrieval over the fact layer |
| `search_memory` | When unsure which type | Unified: facts + causal lessons fused by RRF |
| `trace_cause` | When something fails | Single-hop reverse: which decision caused this outcome |
| `trace_cause_chain` | Deep failure analysis | Multi-hop backward traversal through the causal graph |
| `trace_cause_cross_session` | Cross-session analysis | Meta-causal bridges connect chains across task boundaries |
| `invalidate_decision` | When a lesson is wrong | Soft-invalidate (hidden from search, kept for audit) |
| `search_patterns` | To recall cross-task lessons | Mined meta edges: similar_to / repeated / contradicts / refines |
| `causal_directory` | Pinned in system prompt | L0 compact pointer list of what the agent knows |
| `intervention_query` | **Before taking an action** | Forward simulation: predicts outcomes (safe/warning/danger) |
| `counterfactual_query` | When choosing between options | Contrastive: compares recorded outcomes of two alternatives |

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

## Causal distill pipeline

The distiller extracts structured memories from raw conversations:

```
Raw conversation → V3 extraction prompt (130 lines, 6 rules, 5 few-shot)
                   ↓
  Fact/Preference → agent_facts table (BM25 + embedding searchable)
  Lesson/Event    → causal edges (self-referential, searchable)
  Causal          → proper directed edge: decision → outcome
                    with relation type (caused/enabled/prevented)
```

Raw turns go to `session_logs` (audit/replay only) — they never enter the
retrieval pool. This write-time gatekeeping keeps BM25 precision high.

---

## System layer coverage

All 16 designed layers have end-to-end validation (206 tests):

| Layer | Benchmark | Tests |
|---|---|---|
| Fact layer | Memora / LoCoMo | — |
| Causal edges (caused/enabled/prevented) | Capability | 12 |
| Hippocampus spreading activation | Capability | — |
| SWR consolidation (LTP/LTD/GC) | Longitudinal | 5 |
| Q-value dynamics | Longitudinal | 3 |
| Novelty entropy trigger | Longitudinal | 3 |
| Sleep-wake cycle | Longitudinal | 1 |
| Meta-edge pattern mining | Advanced | 3 |
| Co-occurrence Hebbian | Advanced | 3 |
| Intervention query (forward sim) | Advanced | 4 |
| Trace cause chain | Capability | 2 |
| Inhibitory ablation | Inhibition | 2 |
| Distill / retrieval / facts | Memora / LoCoMo / LME | — |
| Compaction survival | Compact | — |
| Agent trap-world | Agent | — |
| Pipeline e2e | Migration / Pipeline | 2 |

---

## Build & test

```bash
cargo build --release                    # Build binary
cargo test -p causal-memory             # Run 206 tests
cargo test --features local-embed       # Run with ONNX embedding tests
cargo clippy --workspace -- -D warnings # Lint
```

## Agent Memory Challenge (AMC/01)

causal-memory enters the [Agent Memory Leaderboard](https://agentmemories.ai/competition/)
first evaluation cycle via a standalone Add/Search integration server:

```bash
cargo build --release --bin causal-memory-amc
./target/release/causal-memory-amc --db amc.db --port 8787
# POST /add (store memory, user_id-isolated) · POST /search (ordered evidence) · GET /health
```

Docker route: `docker build -t causal-memory-amc . && docker run -p 8787:8787 -v amc-data:/data causal-memory-amc`.
Submission details, method description, and the participation checklist live in
[`docs/amc-2026.md`](docs/amc-2026.md).

Test suite breakdown:
- **166** library unit tests (types, store, distill, patterns, hippocampus)
- **12** causal capability tests (prevented warning, trace-cause, multi-hop, mixed-signal)
- **13** longitudinal dynamics tests (SWR, Q-value, novelty entropy, sleep-wake cycle)
- **11** advanced dynamics tests (meta-edges, Hebbian, intervention query)
- **2** inhibitory ablation tests
- **2** end-to-end pipeline tests

---

## Research background

This project is the engineering output of 17 research notes on agent memory
architecture ([insights/01-17](https://github.com/JingxuanC/agent-teardown/tree/main/insights)),
a teardown of 7 production agent frameworks, and deep analysis of 10+ memory
research papers. Key references:

- **HeLa-Mem** (ACL 2026) — Hebbian spreading activation (our closest competitor; we add the inhibitory side)
- **Anthropic Dreams API** — immutable consolidation pattern (aligned in SWR 2.0)
- **mem0** — write-time gatekeeping architecture (adopted: session_logs separation)
- **MemRL** (arXiv:2601.03192) — Q-value memory dynamics (implemented)
- **Graph World Models** — causal-memory maps to "Graph as Reasoner"

---

## Status

**v0.9.0 — alpha.**

What works (16/16 layers with end-to-end validation):

- ✅ 13 MCP tools + cross-session tracing
- ✅ Write-time gatekeeping (session_logs separation, V3 distill prompt)
- ✅ BM25 + semantic RRF unified retrieval
- ✅ Hippocampus engine: CSR spreading activation, DG SimHash, CA1 novelty, SWR 2.0
- ✅ 7 edge types: caused/enabled/prevented/fact/meta/co_occurrence/no_effect
- ✅ Causal distill: extracts typed causal edges (caused/enabled/prevented) from conversations
- ✅ Hebbian co-occurrence reinforcement
- ✅ Q-value Bellman dynamics
- ✅ Novelty-entropy consolidation trigger + sleep-wake cycle
- ✅ Meta-edge cross-session pattern mining
- ✅ Forward simulation (intervention_query) with prevented-edge warnings
- ✅ Benchmark harnesses: LoCoMo, LongMemEval, Memora, compaction, agent ablation, capability, longitudinal, advanced
- ✅ 206 tests + clippy clean

What's not done yet:

- ❌ Python/TS bindings (PyO3 planned)
- ❌ HTTP transport (MCP stdio only)
- ❌ Forward-simulation prediction-accuracy benchmark (designed, not yet run)
- ❌ 7×24 production deployment validation

## License

Apache-2.0. See [LICENSE](LICENSE).
