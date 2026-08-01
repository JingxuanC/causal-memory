# causal-memory: Typed-Edge Causal Graph Memory for Long-Running AI Agents

---

## Abstract

Long-running LLM agents that operate across hundreds of tasks and iterative context compressions lose causal information — the knowledge of *why* a decision was made and *what* it led to — faster than any other memory type. We present **causal-memory**, an agent memory system that models all memory types (facts, temporal state, causal edges, co-occurrence patterns) as typed edges on a single graph, processed by a hippocampus-inspired engine with excitatory and inhibitory spreading activation. The system's core innovation is the **excitatory/inhibitory duality**: `caused` edges spread positive activation (a glutamate analogue) while `prevented` edges spread negative activation (a GABA analogue) — no existing memory system implements inhibitory dynamics. In controlled experiments, causal edges maintain 100% recall after five iterative LLM compactions where textual recall collapses to 45% (+20.8pp rescue in combined QA). On the LoCoMo benchmark (1,986 questions), the system achieves 79.1% accuracy under strict judging (~89% at frontier-compatible judge caliber), narrowing the gap to mem0's published 91.6% to 2–3 percentage points attributable to model quality. An end-to-end agent ablation shows that causal memory halves the repeat-mistake rate (67% → 33%) on known-trap tasks. The causal graph functions as an explicit world model: backward traversal performs attribution, forward traversal performs simulation — enabling decision-time *what-if* queries that no "notebook-style" memory system can answer. causal-memory is open-source (Rust, Apache-2.0) with 163 tests and reproducible benchmark harnesses.

---

## 1. Introduction

LLM-based agents are transitioning from single-session tools that complete a task in 30 minutes to autonomous systems expected to operate continuously for days or weeks. This transition exposes a fundamental infrastructure gap: current memory systems are designed for *retrieval* (recalling what happened), not for *causality* (understanding why it happened and predicting what would happen under different choices).

The problem is acute because **causal information is the most fragile memory type under iterative context compaction**. When an agent's conversation history exceeds its context window, the framework summarizes older messages — a lossy operation repeated hundreds of times in continuous operation. Each summarization pass may preserve different aspects of the source, causing progressive semantic drift. We measure this directly: after five compaction passes with a production LLM summarization prompt, textual recall of causal details (which decision led to which outcome) drops to **45%**, while a causal table stored outside the compaction pipeline maintains **100%** recall. The degradation is not gradual but a **sudden-death cliff** between the second and third compaction (85% → 55%).

Existing memory systems do not address this gap. Mem0 extracts and retrieves atomic facts but does not model decision–outcome relationships. Zep stores temporal entity-relation graphs but not causal transitions. Letta's self-managed memory and OpenViking's virtual filesystem optimize for factual recall, not for *why* a past approach succeeded or failed. The closest competitor, HeLa-Mem (ACL 2026), builds a Hebbian graph with excitatory spreading activation — but lacks inhibitory dynamics (no mechanism to express "this decision *prevents* a bad outcome") and does not survive compaction because its graph lives inside the conversation context.

We present **causal-memory**, a system built on three principles:

1. **All memory types are typed edges on one graph.** Facts (+0.8 spread), temporal state (validity intervals as edge metadata), causal edges (caused +1.0 / enabled +0.5 / prevented −0.3), co-occurrence (Hebbian dynamic weight), and meta-patterns (+0.6) coexist on a single CSR-format graph processed by one spreading-activation engine. This unification means retrieval, attribution, and simulation share the same substrate — no glue layer between independent stores.

2. **Excitation and inhibition must coexist.** Biological memory relies on both glutamatergic excitation (LTP) and GABAergic inhibition (LTD). HeLa-Mem builds only the excitatory side. We introduce `prevented` edges that spread **negative activation** (−0.3 coefficient), enabling the system to answer "what stops bad outcomes from happening?" — a question no positive-only system can address. This is not metaphor: the inhibitory pathway changes the dynamics of spreading activation, allowing risk-averse retrieval that surfaces preventive interventions.

3. **The causal graph is an explicit world model.** A `caused` edge `[decision A] → [outcome B]` is a sample of the transition function *f(state, action) → outcome*. Backward traversal (outcome → decision) performs **attribution** — all existing benchmarks measure this. Forward traversal (decision → predicted outcomes) performs **simulation** — no benchmark currently measures this, and no competing memory system offers it. The `intervention_query` tool implements decision-time what-if rollout: given a proposed action, it walks the causal graph forward to predict consequences, labeled by outcome polarity (safe / warning / danger).

We evaluate causal-memory across five dimensions. **Compaction survival**: causal edges maintain 100% recall after 5 compactions, rescuing combined QA accuracy by +20.8pp. **LoCoMo benchmark**: 79.1% overall (strict judge) via a six-configuration optimization matrix isolating prompt engineering (+4.6pp), retrieval budget (+3.8pp), and semantic retrieval (+1.1pp). At a frontier-compatible judge caliber, the system scores ~89%, narrowing the gap to mem0's 91.6% to 2–3pp attributable to answerer model quality. **LongMemEval**: multi-session accuracy improves from 41.4% to 57.9% through iterative query expansion and session-level context widening. **Agent ablation**: repeat-mistake rate on known traps drops from 67% to 33%. **Model sensitivity**: a reasoning model (deepseek-v4-pro) achieves 82.3% per-question accuracy, confirming the architecture gap is a model gap.

**Contributions.** (1) A typed-edge causal graph with seven edge types and inhibitory spreading activation — the first memory system to implement negative activation spread. (2) Compaction survival as a first-class benchmark: causal edges are immune to iterative summarization. (3) An end-to-end demonstration that causal memory halves repeat-mistake rates in trap-avoidance tasks. (4) Forward simulation via intervention queries, positioning agent memory as an explicit world model. (5) A reproducible, open-source system (Rust, 13 MCP tools, 163 tests, four benchmark harnesses).

---

## 2. Related Work

### 2.1 Agent Memory Architectures

The agent memory landscape in 2026 comprises five architectural patterns. **Fact-extraction systems** (Mem0) use LLM distillation at ingest to extract atomic facts, then retrieve via multi-signal fusion (semantic + keyword + entity matching). **Temporal knowledge graphs** (Zep/Graphiti) store entity-relation edges with validity intervals. **Self-managed memory** (Letta) lets the agent decide what to remember and forget, operating its own memory as files. **Virtual filesystems** (OpenViking, VLDB 2026) organize memories as a browsable directory tree with tiered loading (L0/L1/L2). **Associative graphs** (HeLa-Mem, ACL 2026) model conversation history as a dynamic Hebbian graph with spreading-activation retrieval and reflective consolidation.

All five patterns share a common limitation: they are **"notebook" architectures** — they answer "what happened?" but not "why did it happen?" or "what would happen if I did X instead?" None models the causal relationship between a decision and its observed outcome as a first-class memory type. None survives context compaction, because all store their data either inside the conversation context or in stores that are not queryable at decision time via the agent's tool interface.

### 2.2 Associative and Consolidation Approaches

HeLa-Mem (ACL 2026) is the closest academic competitor. It models conversation turns as nodes in a Hebbian graph where edge weights strengthen through co-activation during retrieval (*w*(*t*+1) = (1−λ)·*w*(*t*) + η·𝕀(co-active), λ=0.995, η=0.02). A reflective agent identifies hub nodes and distills them into semantic knowledge. Dual-path retrieval combines base activation (embedding + keyword + temporal decay) with spreading activation. Ablation shows spreading activation contributes −2.55pp and consolidation −4.87pp when removed.

causal-memory absorbs HeLa-Mem's Hebbian mechanism as one of seven edge types (`co_occurrence`, spread coefficient +0.2 × dynamic weight) and adds the **inhibitory side** that HeLa-Mem lacks. The `prevented` edge type (spread coefficient −0.3) implements negative activation — a GABA analogue — enabling the system to express and retrieve preventive interventions. Anthropic's Dreams API (2026) introduces immutable consolidation (produce a new store, never mutate the original); we align our SWR 2.0 consolidation to this principle. MemRL (arXiv:2601.03192) proposes Q-value dynamics for memory utility; we implement Bellman-style updates as a node-level weight.

### 2.3 World Models and Causal Reasoning

Graph World Models (arXiv:2604.27895) formalize graph-structured world models into three layers: Connector (spatial topology), Simulator (physical dynamics), and Reasoner (causal/semantic logic). causal-memory maps to the **Reasoner layer**: its typed edges encode causal and semantic relationships, and spreading activation traverses them for associative reasoning. In Pearl's causal hierarchy, `trace_cause` implements Rung-1 (observation: "what caused this?"), `intervention_query` implements Rung-2 (intervention: "what happens if I do X?"), and `counterfactual_query` implements an empirical Rung-2.5 (contrastive: "how would outcomes differ under an alternative?"). We explicitly do not claim Rung-3 structural-causal-model counterfactuals, which require assumptions practically impossible to verify for agent interactions.

---

## 3. System Design

### 3.1 Typed-Edge Taxonomy

All memory in causal-memory is represented as edges on a single graph. Each edge carries a type that determines its behavior in spreading activation:

Table: Edge types and spread coefficients.

| Edge type | Spread coefficient | Biological analogue | Semantics |
|---|---|---|---|
| `caused` | +1.0 | Glutamate (strong excitatory) | Decision A produced outcome B |
| `fact` | +0.8 | Semantic association | Subject-predicate-object fact |
| `meta` | +0.6 | Cortical top-down | Cross-task abstracted pattern |
| `enabled` | +0.5 | Weak excitatory | Decision A made outcome B possible |
| `co_occurrence` | +0.2 × *w*(*t*) | Hebbian LTP (dynamic) | A and B frequently co-activated |
| `prevented` | **−0.3** | **GABA (inhibitory)** | Decision A stopped outcome B |
| `no_effect` | 0.0 | — | No causal relationship |

Temporal semantics are edge metadata (`valid_from`, `valid_to`, `event_time`), not a separate edge type — all edges carry validity intervals. The `prevented` type is unique to this system: no competing memory architecture implements negative activation. When a `prevented` edge is traversed during spreading activation, it *decreases* the activation of the target node, modeling inhibitory neurotransmission.

### 3.2 CSR Spreading Activation

The graph is stored in Compressed Sparse Row (CSR) format — contiguous arrays for row pointers, column indices, and values — enabling cache-friendly sparse matrix–vector multiplication (SpMV). Both forward (decision → outcome) and reverse (outcome → decision) CSR matrices are maintained, with a `rev_to_fwd_idx` mapping for bidirectional edge-validity checks.

Seed selection combines DG (dentate gyrus) SimHash pattern separation (128-bit sparse codes for near-duplicate detection) with BM25 keyword matching. Activations propagate for up to 5 hops with a decay factor of 0.7 per hop and a threshold of 0.1. The merge rule uses absolute-value maximum (not signed maximum), allowing negative activations from `prevented` edges to replace zero — a design choice that enables inhibitory signals to suppress previously neutral nodes. Seed activations are weighted by node Q-value (0.5 + 0.5 × *Q*), giving proven-useful memories stronger initial activation.

### 3.3 SWR Consolidation (Immutable)

Offline consolidation follows the sharp-wave ripple (SWR) pattern: replay random causal chains, strengthen frequently traversed paths (LTP, ×1.05 capped at 2.0), globally decay all edges (LTD, ×0.99 with replay-count protection), and garbage-collect edges meeting a triple criterion (structurally weak AND temporally dormant AND zero recent access). The consolidation produces a **delta log** — every LTP, LTD, and GC operation recorded with old and new values — applied to a **clone** of the graph. The original graph is never mutated; the caller reviews the delta and decides whether to swap. This mirrors Anthropic's Dreams API principle of immutable consolidation. Consolidation triggers automatically when novelty entropy (Shannon entropy over replay-count buckets) exceeds 0.6.

### 3.4 Fact Layer and Unified Retrieval

Flat facts ("user prefers TypeScript") are stored in a separate `agent_facts` table with scope, confidence, and soft-invalidation semantics mirroring causal edges. The unified `search_memory` tool fuses results from the fact layer and the causal layer using Reciprocal Rank Fusion (RRF, *k* = 60). Both layers support a dual retrieval path: BM25 (Okapi, k1=1.2, b=0.75, token-overlap ranking) and optional semantic (cosine similarity over stored embeddings). When embeddings are configured — via HTTP API (OpenAI/ZhiPu) or local ONNX inference (fastembed-rs, BAAI/bge-small-en-v1.5, 384-dim) — the query is embedded once and serves both layers. Results support layered loading (L0 one-line summary ~50 tokens, L1 overview ~200 tokens, L2 full text) with a strict token budget for context-window efficiency.

### 3.5 MCP Integration and Forward Simulation

The system exposes 13 tools via the Model Context Protocol (MCP). The write path (`record_decision`, `record_fact`) creates typed edges with automatic contradiction detection (a new edge for the same decision with a different outcome soft-invalidates the old one). The read path (`search_memory`, `search_causal`, `trace_cause`, `trace_cause_chain`) retrieves memories via the mechanisms above. `trace_cause_cross_session` follows meta-causal bridges across task boundaries.

The simulation path — `intervention_query` — performs forward graph traversal from a proposed decision, collecting the outcomes that similar past decisions caused, labeled by polarity (safe / warning / danger). This implements Pearl's Rung-2 intervention: "if I do X (not merely observe X), what will happen?" `counterfactual_query` compares the recorded outcome distributions of two alternative decisions in similar past situations — an empirical, not structural, counterfactual.

---

*Section 4 (Experiments) is in [`docs/paper-section4-experiments.md`](paper-section4-experiments.md).*

---

## 5. Discussion

### 5.1 Excitatory/Inhibitory Duality

HeLa-Mem (ACL 2026) demonstrates that Hebbian spreading activation — the excitatory side — improves multi-hop retrieval (−2.55pp when ablated). causal-memory confirms this (the `co_occurrence` edge type contributes to associative recall) but adds the inhibitory side via `prevented` edges. The biological motivation is direct: the hippocampus contains both glutamatergic excitatory neurons and GABAergic inhibitory interneurons. An excitatory-only system can answer "what caused this outcome?" but not "what prevents this outcome?" — the latter requires negative activation to suppress the bad-outcome node when a preventive intervention is present.

The duality has engineering consequences beyond analogy. In risk-averse planning, an agent asks: "Given my current state, what bad outcomes are possible, and which of my known interventions prevent them?" Spreading activation from the current state surfaces `caused` edges to bad outcomes (positive activation) and `prevented` edges from interventions (negative activation to those same outcomes). The net activation at each bad-outcome node reflects whether a preventive intervention exists — information that no positive-only system can compute.

### 5.2 From Notebook to Simulator

The causal graph is an explicit world model in the sense of Physical Intelligence's definition (arXiv:2607.06401): a compression of the state-transition process under finite resources. Each `caused` edge is a sample of the transition function *f*(*s*, *a*) → *s*′. Backward traversal (trace_cause) performs attribution — answering "which decision led to this outcome?" Forward traversal (intervention_query) performs simulation — answering "what will happen if I make this decision?"

This positions causal-memory beyond the "notebook" category occupied by all current memory systems. A notebook records what happened; a simulator predicts what will happen. The shift requires causal structure: a pure fact store cannot simulate because facts lack directionality (knowing "the user uses Redis" does not predict "using Redis will cause cache stampede"). The causal graph's typed edges provide the directionality and polarity needed for forward rollout.

The current limitation is **coverage**: a typical 419-turn conversation yields only 49 distilled causal edges — 12% of turns. The bottleneck is the extractor (LLM distillation prompt), not the engine. Richer extraction (implicit causal links from conversational structure) and LLM zero-shot transfer inference (using known edges as few-shot examples to predict unseen transitions) are future work.

### 5.3 Limitations

(1) **No Rung-3 counterfactuals.** Structural causal model reasoning requires assumptions (graph completeness, no unobserved confounders) that are practically unverifiable for agent interactions. We ship only the contrastive/empirical subset. (2) **Extractor-dependent coverage.** The quality and density of causal edges depend on the LLM distillation prompt; richer extraction is an engineering improvement, not an architectural change. (3) **Not deployed in a production 7×24 agent.** The system is validated via benchmarks and ablation, not via longitudinal deployment. (4) **Benchmark scale.** Our evaluation uses 1,986 + 500 + 150 questions across three datasets; mem0's production evaluation operates at larger scale with more diverse user populations. (5) **Chinese text tokenization.** The BM25 tokenizer uses whitespace splitting; Chinese text (no spaces) produces one giant token, degrading keyword matching. Character-bigram tokenization is a known fix not yet implemented. (6) **Forward simulation is unbenchmarked.** The `intervention_query` API is implemented and functional but has no dedicated benchmark measuring prediction accuracy; designing such a benchmark is our highest-priority future work.

---

## 6. Conclusion

Agent memory should be causal, not just factual. We have shown that causal edges — typed relationships between decisions and their outcomes — survive iterative context compaction where all text-based memory collapses, that they halve repeat-mistake rates in end-to-end agent tasks, and that they enable forward simulation (what-if queries) that no notebook-style memory system can offer. The excitatory/inhibitory duality, absent from all competitors including HeLa-Mem (ACL 2026), is the core architectural insight: a complete memory system needs both "what caused this" and "what prevents this," just as the biological hippocampus needs both glutamate and GABA.

The path forward is not higher benchmark scores on existing QA tasks — it is the design of benchmarks that measure what causal memory uniquely offers: compaction survival, forward simulation accuracy, and cross-session causal attribution. The causal graph as an explicit world model is the framing that connects agent memory to the broader goal of systems that learn from experience and predict the consequences of their actions.

causal-memory is open-source at `github.com/JingxuanC/causal-memory` (Rust, Apache-2.0, 163 tests, four benchmark harnesses). The design rationale is documented in 17 research notes at `github.com/JingxuanC/agent-teardown/insights`.

---

## Reproducibility

All experiments use the same frozen protocol: deepseek-chat answerer (temperature 0.0) and judge (temperature 0.0, strict JSON verdict), LLM-distilled ingest, BM25 (Okapi k1=1.2 b=0.75) + optional semantic retrieval (ZhiPu embedding-3, 2048-dim, cosine RRF fusion). Result files (JSONL per-question + JSON summary) are in `benches/*/results/`. The compaction survival benchmark uses grok-build's production compaction prompt (9-section structured template). The agent ablation uses glm-4-plus, seed 42, 6 trap-family tasks.
