# causal-memory: Typed-Edge Causal Graph Memory for Long-Running AI Agents

**Anonymous Submission · System Paper**

---

## One-Sentence Argument

We introduce CausalEval, the first benchmark for agent causal memory, and show
that a typed-edge causal graph with inhibitory spreading activation achieves
+40pp over mem0 on inhibition reasoning (distinguishing root-cause fixes from
blast-radius limiters) while remaining competitive on fact recall — a capability
no flat fact store can offer.

---

## Section Outline (paragraph map)

### 1. Introduction (4 paragraphs)

**P1 — Context (broad):** LLM agents are moving from single-session tools to 7×24 autonomous operation. The bottleneck is no longer model capability but memory infrastructure: how does an agent maintain coherent behavior across hundreds of tasks and context compressions?

**P2 — Gap (narrow):** Current memory systems (Mem0, Zep, Letta) are "notebooks" — they store and retrieve facts. None model the causal relationship between decisions and outcomes, which is the most fragile information type under iterative compaction (text recall drops to 45% after 5 compactions vs 100% for a causal table). Moreover, **no benchmark exists** to measure causal capabilities — all existing suites (LoCoMo, LongMemEval, Memora) test fact recall only. The closest competitor, HeLa-Mem (ACL 2026), builds excitatory Hebbian associations but lacks inhibitory dynamics and forward simulation.

**P3 — Approach:** We present causal-memory, a memory system where all memory types are typed edges on a single graph, processed by a hippocampus-inspired engine with excitatory/inhibitory spreading activation. We also introduce **CausalEval**, a graph-grounded benchmark where the causal graph IS the answer key: conversations are generated from typed DAGs, questions are derived from graph structure, and gold answers have zero ambiguity.

**P4 — Contributions:** (1) **CausalEval**: 7-capability benchmark with graph-derived ground truth, runnable by any add/search memory system. (2) **Inhibitory semantics**: `prevented` edges spread negative activation (GABA analogue) — CausalEval shows +40pp over mem0 on inhibition reasoning. (3) Typed-edge causal graph with 7 edge types, CSR spreading activation, SWR consolidation, and Q-value dynamics (231 tests). (4) **Honest evaluation**: competitive on fact recall (LoCoMo 79.1%), superior on causal reasoning (CausalEval overall 71% vs mem0 65%), with documented limitations (C6 transfer, C7 update). (5) Edge labeling accuracy audit: 83% agreement with independent LLM re-judgment, 0 severe misclassifications.

### 2. Related Work (3 paragraphs)

**P1 — Memory architectures:** Mem0 (fact extraction + multi-signal retrieval), Zep (temporal knowledge graph), Letta (self-managed memory). Position: all are "notebook" architectures — retrieve what happened, not why or what-if.

**P2 — Associative and consolidation approaches:** HeLa-Mem (ACL 2026, Hebbian spreading activation — excitatory only), Anthropic Dreams API (immutable consolidation), MemRL (Q-value dynamics). Position: we absorb Hebbian as one edge type, add inhibitory, and implement Q-value + immutable consolidation.

**P3 — Benchmark gap:** LoCoMo, LongMemEval, Memora all measure fact recall. No benchmark tests causal attribution, intervention prediction, inhibitory reasoning, or lesson transfer. CausalEval fills this gap with graph-grounded, capability-classified questions.

### 3. System Design (5 subsections)

**3.1 Typed-Edge Taxonomy:** 7 edge types with spread coefficients and biological analogues. Key: `prevented` at −0.3 is unique (GABA inhibitory). `co_occurrence` uses dynamic Hebbian weights.

**3.2 CSR Spreading Activation:** Compressed Sparse Row format for cache-friendly SpMV. Forward/reverse CSR. Seed selection by DG SimHash + BM25. Activation merge by abs-max (allows negative to replace zero).

**3.3 SWR Consolidation (Immutable):** LTP/LTD/GC with delta+clone. Novelty-entropy trigger.

**3.4 Fact Layer + Unified Retrieval:** RRF fusion of facts + causal. BM25 + semantic dual-path.

**3.5 Multi-Agent Session Adapters:** grok, Claude Code, Kimi (OpenClaw), Codex — all produce the same `ParsedSession` IR.

### 4. Experiments (5 subsections)

**4.1 CausalEval (the primary experiment):**
- Protocol: 10 graphs × 7 capability classes = 70 questions. Graph-grounded gold. Same conversations, same LLM, same judge for both causal-memory and mem0.
- Headline result: C4 Inhibition +40pp (90% vs 50%). C2 Intervention +30pp. Overall 71% vs 65%.
- Per-capability table with evidence-hit rates (retrieval vs reasoning decomposition).
- Limitations: C6 transfer 20% (meta-edge coverage gap), C7 update 50% (judge variance).

**4.2 Fact-Recall Benchmarks (secondary):**
- LoCoMo 79.1% strict (vs mem0 91.6% — fact recall is mem0's specialty, not our contribution).
- LongMemEval 75.2% (roughly tied with mem0 74.4%).
- Memora MPA 67.4% (vs mem0 71.8%).
- Compaction survival: 100% vs 45% (external table = structurally immune).
- Framing: "competitive but not superior on fact recall — the value is in causal capabilities."

**4.3 Edge Labeling Accuracy:**
- 100 random edges, independent LLM re-judgment: 83% agreement.
- 17 mismatches all `caused`→`enabled` (gray zone, both defensible).
- 0 severe errors (no `prevented`/`no_effect` mislabels).

**4.4 Agent Ablation (trap-world):**
- Repeat-mistake rate: 67% → 33%. Memory prevents re-stepping into known traps.

**4.5 Capability Tests:**
- 231 tests covering all 16 designed layers: spreading activation, inhibitory ablation, SWR consolidation, Q-value dynamics, novelty entropy, meta-edge mining, Hebbian co-occurrence.

### 5. Discussion (3 paragraphs)

**P1 — Excitatory/Inhibitory Duality:** HeLa-Mem builds the excitatory side; we add inhibitory. The C4 +40pp result validates that inhibitory semantics are not just biologically motivated — they produce measurable accuracy gains on a capability that flat fact stores cannot express.

**P2 — Benchmark Design Philosophy:** CausalEval's "graph = answer key" design eliminates annotation ambiguity and enables controlled difficulty. This is a methodological contribution independent of the system.

**P3 — Limitations & Future Work:** (1) C6 cross-task transfer (20%) needs better meta-edge coverage in the retrieval path. (2) C7 update (50%) needs judge variance reduction (larger scale, stronger judge model). (3) No Rung-3 SCM counterfactuals. (4) Not deployed in 7×24 production. (5) CausalEval scale (70 questions) needs expansion to ~250 for tighter confidence intervals.
