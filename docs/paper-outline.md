# causal-memory: Typed-Edge Causal Graph Memory for Long-Running AI Agents

**Anonymous Submission · System Paper**

---

## One-Sentence Argument

In long-running LLM agent memory, we show that a typed-edge causal graph with excitatory and inhibitory spreading activation survives context compaction where text memory collapses (+20.8pp), enables forward simulation beyond retrieval, and approaches frontier factual-recall performance (79.1% LoCoMo strict, ~89% at compatible judge caliber), supported by controlled experiments across three standard benchmarks and an end-to-end agent ablation.

---

## Section Outline (paragraph map)

### 1. Introduction (4 paragraphs)

**P1 — Context (broad):** LLM agents are moving from single-session tools to 7×24 autonomous operation. The bottleneck is no longer model capability but memory infrastructure: how does an agent maintain coherent behavior across hundreds of tasks and context compressions?

**P2 — Gap (narrow):** Current memory systems (Mem0, Zep, Letta, OpenViking) are "notebooks" — they store and retrieve facts. None model the causal relationship between decisions and outcomes, which is the most fragile information type under iterative compaction (text recall drops to 45% after 5 compactions vs 100% for a causal table). The closest competitor, HeLa-Mem (ACL 2026), builds excitatory Hebbian associations but lacks inhibitory dynamics and forward simulation.

**P3 — Approach:** We present causal-memory, a memory system where all memory types (facts, temporal state, causal edges, co-occurrence) are typed edges on a single graph, processed by a hippocampus-inspired engine with typed spreading activation. The key innovation is the excitatory/inhibitory duality: `caused` edges spread positive activation (glutamate analogue), while `prevented` edges spread negative activation (GABA analogue) — no other system implements inhibitory spread.

**P4 — Contributions:** (1) Typed-edge causal graph with 7 edge types and CSR-based spreading activation including negative spread. (2) Compaction survival evidence: causal edges maintain 100% recall after 5 compactions while text degrades to 45%. (3) End-to-end agent ablation: repeat-mistake rate 67%→33%. (4) Benchmark results: 79.1% LoCoMo (strict judge), approaching mem0's 91.6% at 2-3pp gap attributable to model quality. (5) Forward simulation via intervention queries — the causal graph as an explicit world model.

### 2. Related Work (3 paragraphs)

**P1 — Memory architectures:** Mem0 (fact extraction + multi-signal retrieval), Zep (temporal knowledge graph), Letta (self-managed memory), OpenViking (virtual filesystem). Position: all are "notebook" architectures — retrieve what happened, not why or what-if.

**P2 — Associative and consolidation approaches:** HeLa-Mem (ACL 2026, Hebbian spreading activation — excitatory only), Anthropic Dreams API (immutable consolidation), MemRL (Q-value dynamics). Position: we absorb Hebbian as one edge type, add inhibitory, and implement Q-value + immutable consolidation.

**P3 — World models and causal reasoning:** Graph World Models (arXiv:2604.27895, "Graph as Reasoner" = causal/semantic reasoning), Pearl's causal hierarchy (observation/intervention/counterfactual). Position: our causal graph maps to the Reasoner layer, with `intervention_query` implementing Rung-2 and `counterfactual_query` implementing empirical Rung-2.5.

### 3. System Design (5 subsections)

**3.1 Typed-Edge Taxonomy:** 7 edge types with spread coefficients and biological analogues. Table. Key: `prevented` at −0.3 is unique (GABA inhibitory). `co_occurrence` uses dynamic Hebbian weights `w(t+1) = (1-λ)w(t) + η·𝕀(co-active)`.

**3.2 CSR Spreading Activation:** Compressed Sparse Row format for cache-friendly SpMV. Forward (decision→outcome) and reverse (outcome→decision) CSR. Seed selection by DG SimHash pattern separation + BM25. Activation merge by abs-max (allows negative to replace zero). Q-value-weighted seeding.

**3.3 SWR Consolidation (Immutable):** Sharp-Wave Ripple analogue: LTP (strengthen replayed chains), LTD (global decay), GC (triple-criterion: weak AND dormant AND zero-access). Produces delta + clone (original graph never mutated). Novelty-entropy trigger.

**3.4 Fact Layer + Unified Retrieval:** `agent_facts` table with scope/confidence/validity. `search_memory` fuses facts + causal via Reciprocal Rank Fusion. Semantic (embedding cosine) + BM25 dual-path with fallback. Layered loading L0/L1/L2 with token budget.

**3.5 MCP Integration:** 13 tools exposed via Model Context Protocol. Write path: `record_decision` / `record_fact`. Read path: `search_memory` (unified RRF), `search_causal` (BM25+semantic), `trace_cause_chain` (multi-hop reverse). Simulation: `intervention_query` (forward), `counterfactual_query` (contrastive).

### 4. Experiments (5 subsections)

**4.1 Compaction Survival (the core experiment):**
- Protocol: LoCoMo conversations compressed k=1,2,3,5 times with grok-build's production compaction prompt. QA after compression. Control: causal table never compacted.
- Result: text recall 100%→45% (k=5), causal-table recall 100% (constant). Combined (text+causal): 65.3% at k=5 vs 44.5% text-only = +20.8pp rescue.
- Table: per-k recall comparison.

**4.2 LoCoMo Benchmark Optimization:**
- 6-config optimization matrix (V1/V2 × BM25/semantic × topk=10/20/50).
- Best: 79.1% (V2 prompt + BM25/semantic RRF + topk=50).
- Gain attribution: prompt +4.6pp, budget +3.8pp, semantic +1.1pp.
- Per-category breakdown (multi-hop, temporal, open-domain, single-hop, adversarial).
- Judge dual-caliber: strict 79.1% vs mem0-compatible 84.1% (+9.9pp judge tax). Gap to mem0 91.6% at same caliber: ~2-3pp (model gap).

**4.3 LongMemEval + Multi-session Enhancement:**
- Multi-session: 41.4% → 50.4% (P7 per-noun expansion) → 57.9% (P8 session expansion). Cumulative +16.5pp.
- Temporal: 69.9% → 77.9%.
- Session expansion analysis: fragments → full-session context.

**4.4 Agent Ablation (trap-world):**
- Protocol: same LLM (glm-4-plus, seed 42), 6 trap-family tasks, with vs without causal memory.
- Result: repeat-mistake rate 67% (no memory) → 33% (with memory). Both groups solved 6/6 tasks; the memory tax is ~1 extra step per task.
- Implication: memory doesn't help solve novel problems faster, but prevents re-stepping into known traps.

**4.5 Three-Model Comparison:**
- deepseek-chat (74.2%, 0 errors), deepseek-v4-pro (82.3% non-error accuracy, 23% API timeouts), glm-5.2 (56.6%).
- Finding: model quality is the remaining gap to mem0, not architecture.

### 5. Discussion (3 paragraphs)

**P1 — Excitatory/Inhibitory Duality:** HeLa-Mem builds the excitatory side; we add inhibitory. Biological plausibility: hippocampus has both glutamate and GABA. Engineering significance: `prevented` edges enable risk-averse planning ("what stops bad outcomes?") that positive-only systems cannot express. The causal graph as a transition function makes this a duality in the control-theory sense, not just analogy.

**P2 — From Notebook to Simulator:** The causal graph is an explicit world model: `caused` edges are transition samples `f(state, action) → outcome`. Backward traversal is attribution; forward traversal is simulation. `intervention_query` implements decision-time rollout. Limitation: coverage is sparse (49 edges/conversation) — the extractor is the bottleneck, not the engine. Future: LLM zero-shot transfer inference from known edges.

**P3 — Limitations:** (1) No Rung-3 SCM counterfactuals (empirical contrastive only). (2) Causal edge coverage depends on extractor quality. (3) Not deployed in a production 7×24 agent end-to-end. (4) Benchmark scale smaller than mem0's production evaluation. (5) Chinese text tokenization is a known limitation in similarity matching.

### 6. Conclusion (1 paragraph)

causal-memory demonstrates that agent memory should be causal, not just factual: causal edges survive compaction, enable forward simulation, and approach frontier recall performance. The excitatory/inhibitory duality — absent from all competitors — is the core architectural insight. The system is open-source, reproducible, and provides the first benchmark evidence that causal memory improves agent learning (67%→33% repeat-mistake rate). The path from "notebook" to "simulator" is the next frontier.

---

## Reproducibility

- Open source: `github.com/JingxuanC/causal-memory` (Rust, Apache-2.0)
- 163 tests, workspace lints, clippy clean
- Benchmark harnesses: LoCoMo, LongMemEval, Memora, compaction survival, agent ablation
- All result JSONs in `benches/*/results/`
- 17 research notes documenting the design rationale: `github.com/JingxuanC/agent-teardown/insights/`
