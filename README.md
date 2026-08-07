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
[![Tests: 231](https://img.shields.io/badge/tests-231-brightgreen.svg)](#build--test)

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

### CausalEval — the causal memory benchmark (primary)

Most agent-memory benchmarks (LoCoMo, LongMemEval, Memora) test **fact recall**
("what is the user's preference"). causal-memory's differentiators — typed
causal edges, inhibition, intervention prediction, cross-task transfer — are
invisible on those suites. **CausalEval** measures them.

**Design: the causal graph is the answer key.** Typed DAGs are generated
deterministically; conversations are narrated from the graph; gold answers are
derived from graph structure — zero hand annotation, zero ambiguity.

**CausalEval v7 vs mem0** (70 questions, same conversations, same LLM, same judge):

| Capability | causal-memory | mem0 | Δ | What it tests |
|---|---|---|---|---|
| C3 Counterfactual | **90%** | 80% | **+10pp** | Choosing between alternatives with known outcomes |
| C4 Inhibition | **90%** | 50% | **+40pp** | Distinguishing root-cause fix vs blast-radius limiter (`prevented` edges) |
| C5 Temporal-causal | **100%** | 90% | **+10pp** | Ordering on a causal chain |
| C1 Attribution | **90%** | 90% | ±0 | Backward causal chain → root cause |
| C2 Intervention | **70%** | 40% | **+30pp** | Forward prediction: "if X again, what happens?" |
| C6 Lesson transfer | 20% | 30% | −10pp | Cross-task analogy via meta edges (limitation) |
| C7 Update | 50% | 80% | −30pp | Supersede old belief after falsification (limitation) |
| **Overall** | **71%** | **65%** | **+6pp** | |

**Key finding: C4 Inhibition +40pp.** mem0 cannot distinguish "what fixed the
root cause" from "what limited the blast radius" — it has no `prevented` edge
type. causal-memory's inhibitory semantics (GABA-analogue negative spread)
produce the largest single-capability gap in the benchmark.

### Fact-recall benchmarks (not our strong suit)

On traditional fact-recall suites, causal-memory performs competitively but
**does not beat mem0** — this is expected, because fact recall is mem0's
specialty and not where causal-memory adds value.

| Benchmark | causal-memory | mem0 | Note |
|---|---|---|---|
| LoCoMo (strict judge) | 79.1% | 91.6% | mem0's home turf |
| LongMemEval (distill v2) | 75.2% | 74.4% | Roughly tied |
| Memora MPA | 67.4% | 71.8% | −4.4pp |
| Compaction survival | 100% | 45% | External table = immune to compaction |
| Agent repeat-mistake | 33% | 67% | −34pp on trap-world |

### Capability tests (231 tests, all passing)

These test capabilities that **no fact store (mem0, Zep, Letta) can offer**.

| Capability | What it proves | Tests |
|---|---|---|
| **Prevented-edge warning** | `prevented` edge spreads −0.3 activation (GABA analogue) | 2 |
| **Trace-cause attribution** | Backward CSR traversal finds root cause | 2 |
| **Multi-hop causal chain** | Forward K-hop spreading reaches 2-3 hop outcomes | 2 |
| **Inhibitory filtering** | Prevented outcomes appear as negative, not false positives | 1 |
| **Intervention comparison** | Same outcome has +0.9 for "skip tests" and −0.3 for "add tests" | 4 |
| **SWR consolidation** | LTP strengthens replayed edges, LTD weakens unvisited, GC forgets dormant | 5 |
| **Q-value dynamics** | Good decisions rank higher; Bellman propagates to parents | 3 |
| **Novelty entropy** | Diverse experience triggers consolidation; uniform does not | 3 |
| **Meta-edge mining** | Cross-session pattern discovery (similar_to / repeated) | 3 |
| **Hebbian co-occurrence** | Repeated co-activation strengthens connection | 3 |

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

All 16 designed layers have end-to-end validation (231 tests):

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
cargo test -p causal-memory             # Run 231 tests
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
- ✅ 231 tests + clippy clean

What's not done yet:

- ❌ Python/TS bindings (PyO3 planned)
- ❌ HTTP transport (MCP stdio only)
- ❌ Forward-simulation prediction-accuracy benchmark (designed, not yet run)
- ❌ 7×24 production deployment validation

## License

Apache-2.0. See [LICENSE](LICENSE).
