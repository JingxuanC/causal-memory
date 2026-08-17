# CausalEval — graph-grounded causal memory benchmark

> A benchmark for the capabilities that fact-recall suites (LoCoMo, LongMemEval,
> mem0's benchmarks) cannot measure: typed causal reasoning, inhibition,
> intervention prediction, lesson transfer, and memory updates.
> Core idea: **the causal graph is the answer key.** Conversations are generated
> FROM a known typed causal graph, so every question has an unambiguous,
> graph-derived gold answer.
> Date: 2026-08-06.

## 1. Why

LoCoMo cat1-5 measure fact recall; mem0 dominates there (its judge, its top-k,
its extractor). causal-memory's differentiators — typed causal edges
(caused/enabled/prevented), inhibitory negative spread, intervention/counter-
factual queries, reversible supersede, cross-task meta edges — are invisible on
those suites. CausalEval exists to measure them, and to run baselines (mem0,
plain RAG) on the SAME conversations so the delta is attributable.

## 2. Design principles

1. **Graph = ground truth.** Random typed DAGs are generated deterministically;
   gold answers are derived from the graph, never hand-annotated. Zero ambiguity,
   controllable difficulty (depth, edge mix, noise).
2. **Capability-classified questions.** Each question maps to one causal
   capability (C1-C7) with its own accuracy column.
3. **Narrativized, not synthetic-looking.** An LLM turns each graph into
   multi-session two-person dialogues; events appear rephrased, causality stays
   implicit — the memory system must EXTRACT it, not read it off a list.
4. **Baseline-runnable.** Conversations are plain text (LoCoMo-shaped); mem0 /
   any add/search system can ingest them. Per-capability Δ vs baseline is the
   deliverable.
5. **Same pipeline as production.** Ingest via the real store (turn chunks +
   temporal edges + LLM distill), retrieval via the real store methods, judge
   fixed (strict, with retry + JSON mode).

## 3. Generation pipeline

```
① graph generator (deterministic, seeded)
   typed DAG: nodes = (person, action/event text), edges ∈ {caused, enabled, prevented}
   → graphs.json: nodes, edges, task_tags, event order (temporal ground truth)

② narrativizer (LLM, deepseek)
   graph → N sessions of two-person dialogue
   constraint: every node's action/event must appear ≥1× (may be rephrased)
   → conversations.json (LoCoMo-shaped: sessions → turns with speakers/dates)

③ verification pass (deterministic)
   per node: key tokens must occur in the generated text (fuzzy token overlap)
   missing → regenerate that graph with the missing events called out (max 2 tries)

④ question generator (deterministic, from the graph)
   per graph, per capability class → qa.json
   gold derived from graph structure + event semantics
```

## 4. Question classes

| Class | Question shape | Gold source | Capability under test |
|---|---|---|---|
| C1 attribution | "Why did `<outcome>` happen?" (depth ≥2 chains) | backward causal chain (root decision → … → outcome) | trace_cause / trace_cause_chain |
| C2 intervention | "If `<person>` does `<decision>` again, what will happen?" | forward outcome per graph (caused/enabled edges, prevented blockers) | intervention_query (forward sim) |
| C3 counterfactual | "Should `<person>` do X or Y?" (two sibling decisions, known outcomes) | the option whose outcome is positive/safe | counterfactual_query |
| C4 inhibition | "What stopped `<outcome>` from happening?" / "how can it be prevented?" | prevented edges targeting the node | prevented negative spread |
| C5 temporal order | "Which happened first, A or B?" (on a chain) | graph construction order | temporal-causal reasoning |
| C6 lesson transfer | Two isomorphic decisions in DIFFERENT task tags, opposite outcomes; "facing situation B, what should `<person>` avoid?" | the failure lesson | meta-edge mining / cross-task transfer |
| C7 update | Phase-2 conversation: the user later says the old lesson was wrong; "what does `<person>` now believe?" | the corrected statement | supersede / reversible retirement |

C7 is two-phase: the dialogue includes a later turn that falsifies an earlier
lesson; the question is asked AFTER that turn. A correct answer requires the
memory system to have retired the old conclusion (a fact store that never
updates surfaces the stale lesson → wrong).

## 5. Data format

```
causal_eval/data/graph_{i}.json        — the ground-truth graph
causal_eval/data/graph_{i}_conv.json   — narrated conversations (LoCoMo-shaped)
causal_eval/data/graph_{i}_qa.json     — questions + graph-derived gold
```

Conversation shape mirrors LoCoMo (`conversation[].qa[]` with
`category ∈ {11..17}`, `question`, `answer`, `evidence[]` = chunk ids), so the
harness and the answer/judge pipeline reuse the proven LoCoMo machinery.

## 6. Metrics

- Per-class accuracy (C1-C7) — valid accuracy, errors excluded (judge-retry
  machinery keeps errors ~0).
- Causal evidence hit rate: fraction of questions where the gold causal edge(s)
  were retrieved (search-only mode, zero LLM).
- Intervention agreement: C2 predicted outcome vs graph-truth outcome.
- Inhibition false-positive rate: prevented warnings on questions where nothing
  is blocked (distractor questions).
- **Δ vs baseline**: same conversations through mem0 (or a BM25-only store
  variant) — the differentiation proof.
- Compaction survival (existing compact experiment) reported alongside.

## 7. Scale & cost

- v1: 10 graphs × 12-15 questions ≈ 120-150 questions. Generation LLM cost:
  10 graphs × ~2 narration calls (retries) ≈ 20 calls. Run cost ≈ LoCoMo at
  the same question count (~15-20 min).
- Target: 20 graphs ≈ 250 questions (LoCoMo-scale).

## 8. Harness design (benches/causal_eval/)

```
causal-memory-causal-eval generate --graphs 10 --out data/   # graph + questions (deterministic)
causal-memory-causal-eval narrate  --data data/              # LLM narrativization + verify
causal-memory-causal-eval run      --data data/ [--search-only] [--topk N] [--concurrency N]
```

- `run` reuses the store's ingest (turn chunks + temporal edges + distill),
  retrieval (BM25 + entity-boosted semantic + hop via `store::retrieve`),
  answer+judge (chat_json + retry, strict judge).
- `--search-only` for zero-LLM evidence iteration (same as LoCoMo harness).

## 9. Deliverables

- docs/causal-eval-2026.md (this design)
- benches/causal_eval/main.rs (generator + harness)
- data/ (generated, versioned seed)
- results/ + per-capability report
- README table row: CausalEval per-class scores + Δ vs mem0

## 10. Review fixes (grok audit, 2026-08-07) + v2 results

External audit found 3 critical + 4 structural flaws; all fixed:

1. **evidence_hit id-space mismatch** → text-space matching (retrieved edge
   endpoint text covers the gold node's key tokens). 71% → **97%** evidence hit.
2. **C7 never tested update** → generator now emits a falsification phase
   (node 10 `invalidates` node 3) + C7 gold = the correction.
3. **C6 wasn't transfer** → twin story is now ISOMORPHIC (X'→Y'→Z') with a
   `similar_to(X, X')` meta edge; C6 gold = Z'.
4. **No depth ≥ 2 chains** → 0→1→2 caused chain with distinct failure +
   escalation texts; test asserts longest path ≥ 2.
5. **C2 ignored the preventer** → question phrased "without any other changes".
6. **C4/C7 collapsed** → preventer is a distinct good practice (asserted).
7-12. Question wording, traversal-derived evidence, `--limit`, neutral
   `no_effect` turn edges, CoT fallback removal, depth test.

v2 per-class (70 questions, evidence 97%):

| Class | Acc | Note |
|---|---|---|
| C5 temporal | 100% | strength |
| C3 counterfactual | 80% | strength |
| C1 attribution | 70% | — |
| C2 intervention | 40% | — |
| C4 inhibition | 20% | model conflates fix vs preventer |
| C6 transfer | 20% | meta edges not activated in run path |
| C7 update | 10% | supersede not triggered by generic distill |

With retrieval no longer a confounder, the remaining failures are genuine
reasoning/update gaps — the benchmark now isolates capabilities as designed.
