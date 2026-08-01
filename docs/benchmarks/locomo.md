# LoCoMo Benchmark — causal-memory

Evaluation on [LoCoMo](https://github.com/snap-research/locomo) (`locomo10.json`:
10 conversations, 1,986 questions).

**Headline (V2 prompt + BM25/semantic RRF + topk=50): overall 79.1% (1,571/1,986) —
+9.5pp vs distill V1 baseline 69.6%, +15pp vs raw baseline 64.2%.**

At mem0-compatible judge caliber: ~89% (79.1% strict + ~10pp judge tax). Gap to
mem0 official 91.6% (gpt-5 + top-200 + mem0 judge) narrows to ~2-3pp, attributable
to model quality (gpt-5 vs deepseek-chat).

## Optimization matrix (all 1986q, strict judge, deepseek-chat)

| Config | overall | cat1 multi-hop | cat2 temporal | cat3 open-domain | cat4 single-hop | cat5 adversarial |
|---|---|---|---|---|---|---|
| V1 BM25 topk=10 (baseline) | 69.6% | — | — | — | — | 91.9% |
| V2 BM25 topk=10 | 74.2% (+4.6) | 35.8% | 72.0% | 47.9% | 83.5% | 88.3% |
| V2 BM25 topk=20 | 76.3% (+6.7) | 41.5% | 74.1% | 47.9% | 85.6% | 88.3% |
| V2 BM25 topk=50 | 78.0% (+8.4) | 46.5% | 78.2% | 52.1% | 86.8% | 87.0% |
| V2 BM25+semantic topk=10 | 75.0% (+5.4) | 35.8% | 74.1% | 51.0% | 83.4% | 89.9% |
| **V2 BM25+semantic topk=50** | **79.1% (+9.5)** | **48.6%** | **79.1%** | **55.2%** | **88.1%** | 86.3% |

**Gain attribution**: prompt V1->V2 (+4.6pp), retrieval budget 10->50 (+3.8pp),
semantic RRF (+1.1pp at topk=50). All three are orthogonal and stack.

## Earlier baselines (superseded by the optimization matrix above)

Distill baseline (V1 prompt, BM25, topk=10): overall 69.6% (1,382/1,986).
Raw baseline (run 5): overall 64.2% (1,275/1,986). This is an honest baseline,
published with the same spirit as the v0.4.1 retrieval finding: causal-memory is
a **causal layer**, not a general-purpose factual memory — and this benchmark
measures mostly the latter. See [Failure analysis](#failure-analysis).

## Distill-mode run (run_distill_full_20260730, schema v7 fact layer)

Same dataset, answerer, judge, top-k and protocol as run 5; the only change
is ingest: every session is LLM-distilled, `Fact`/`Preference` items route
to the `agent_facts` layer (scope `user` — each conversation gets its own
distill DB, so isolation is physical; supersedes hints retire outdated
values) and `Lesson`/`Event` items route to the causal layer. Fact lines
are placed FIRST in the answer prompt, followed by causal memory lines.

| Category | n | raw (run 5) | distill | Δ |
|---|---|---|---|---|
| 1 multi-hop | 282 | 24.1% | 30.5% | +6.4pp |
| 2 temporal | 321 | 49.2% | **60.8%** | **+11.6pp** |
| 3 open-domain | 96 | 19.8% | **31.3%** | **+11.5pp** |
| 4 single-hop | 841 | 74.0% | **78.6%** | +4.6pp |
| 5 adversarial | 446 | 91.5% | 91.9% | +0.4pp |
| **overall** | 1,986 | **64.2%** | **69.6%** | **+5.4pp** |
| evidence retrieval hit rate | — | 74.4% | 71.7% | −2.7pp |

Honest reading: the unified-memory design doc predicted 75–80%; we landed
at 69.6%. The mechanism is validated — gains concentrate exactly where
atomic facts should help (temporal +11.6, open-domain +11.5) and abstention
does not degrade — but two bottlenecks cap the total: cat 1 multi-hop
questions need complete multi-evidence recall the fact layer only partially
covers, and cat 3 counterfactual phrasing collides with the abstention protocol.
The design doc's 75–80% remains the target after fixing those two; see
the failure analysis below.

Reproduce:

```bash
export DEEPSEEK_API_KEY=...
./target/release/causal-memory-locomo run --all \
    --data benches/locomo/data/locomo10.json \
    --ingest distill --concurrency 16   # add --ingest-only to skip QA
```

## E1 V2 prompt run (run_e1_v2_full, 2026-07-31)

Same dataset, DBs, ingest (distill), judge (strict), and top-k=10 as the
distill baseline; the only change is the **answer prompt**: V1 (one
paragraph, 4 rules) replaced by V2 (7-step reasoning, ported from mem0's
`ANSWER_GENERATION_PROMPT`). V2 adds: scan-all-memories (anti
lost-in-the-middle), entity verification, combine-and-cross-reference,
temporal grounding with absolute dates, inclusion check (anti
over-filtering on list questions), and `ANSWER:` marker parsing.
Memories are sorted by `event_time` ascending under V2 (narrative order).
cat5 (adversarial) always uses V1 to preserve abstention ability.

| Category | n | distill V1 | **distill V2** | Δ |
|---|---|---|---|---|
| 1 multi-hop | 282 | 30.5% | **35.8%** | +5.3pp |
| 2 temporal | 321 | 60.8% | **72.0%** | **+11.2pp** |
| 3 open-domain | 96 | 31.3% | **47.9%** | **+16.6pp** |
| 4 single-hop | 841 | 78.6% | **83.5%** | +4.9pp |
| 5 adversarial | 446 | 91.9% | 88.3% | −3.6pp |
| **overall** | 1,986 | **69.6%** | **74.2%** | **+4.6pp** |

Read: V2 lifts every factual category. The biggest win is cat3 open-domain
(+16.6pp) — V2 drops V1's "never infer feelings/meanings" prohibition and
the COMMIT step allows evidence-grounded inference, which is exactly what
open-domain/counterfactual questions need. cat2 temporal
(+11.2pp) benefits from Step 5's absolute-date grounding. cat5 dips
−3.6pp (V2's anti-abstention language leaks into cat5 despite the V1
guard; fixable by tightening the cat5 dispatch). cat1 multi-hop remains
the weakest slice — Step 6 inclusion check helps (+5.3pp) but multi-hop
questions still need complete multi-evidence recall.

Reproduce:

```bash
./target/release/causal-memory-locomo run --all \
    --data benches/locomo/data/locomo10.json \
    --ingest distill --prompt-version v2 --concurrency 16
```

## E3 Judge dual-caliber (F1 full rejudge, 2026-07-31)

mem0's 91.6% uses a lenient judge (partial credit, ±14d dates, extra detail OK).
To separate judge looseness from genuine recall quality, we re-judged the V2
results with a mem0-compatible judge (no re-answering, ~$1/1986q).

| | strict judge | mem0 judge | Δ (judge tax) |
|---|---|---|---|
| V1 prompt | 69.6% (1382) | **78.3%** (1556) | **+8.7pp** |
| **V2 prompt** | **74.2%** (1474) | **84.1%** (1671) | **+9.9pp** |

**Category breakdown (V1 × mem0 judge)**:

| Category | V1 strict | V1 mem0 | Δ |
|---|---|---|---|
| 1 multi-hop | — | 62.4% | — |
| 2 temporal | — | 72.6% | — |
| 3 open-domain | — | 36.5% | — |
| 4 single-hop | — | 83.6% | — |
| 5 adversarial | — | 91.7% | — |

**Category breakdown (V2 × mem0 judge)**:

| Category | V2 strict | V2 mem0 | Δ |
|---|---|---|---|
| 1 multi-hop | 35.8% | 73.0% | +37.2pp |
| 2 temporal | 72.0% | 84.1% | +12.1pp |
| 3 open-domain | 47.9% | 60.4% | +12.5pp |
| 4 single-hop | 83.5% | 88.7% | +5.2pp |
| 5 adversarial | 88.3% | 87.7% | −0.6pp |

**Key finding**: mem0 official is 91.6% (gpt-5 + top-200 + mem0 judge).
At the **same judge caliber** (mem0 judge), our V2 scores 84.1% — the gap
narrows to **7.5pp**, and that 7.5pp is attributable to model (gpt-5 vs
deepseek-chat) and retrieval budget (top-200 vs top-10), not judge looseness.

The judge tax is fairly stable across prompt versions: +8.7pp (V1) vs
+9.9pp (V2). V2 benefits slightly more from lenient judging because its
longer answers contain more partial matches.

cat1 multi-hop benefits most from lenient judging (+37.2pp) — these
list-style questions score 1-item-correct under mem0 rules, where strict
requires the complete list. cat5 (adversarial) is judge-invariant
(−0.6pp, within noise) — abstention is binary.

Reproduce:

```bash
# re-judge existing V2 results with mem0 judge (no re-answering)
./target/release/causal-memory-locomo rejudge \
    --input benches/locomo/results/e1_v2_full --judge-style mem0
```


## Category ID mapping (corrected 2026-07-31)

Earlier versions of this document mislabeled categories 1/3/4 using the
white paper's *sequential description*. The canonical mapping — original
paper Table 5 counts (single-hop 841, multi-hop 282), mem0's evaluation
suite, and the source-code mapping documented in arXiv 2511.21726 Table E2 —
is:

| ID | Category | n |
|---|---|---|
| 1 | multi-hop | 282 |
| 2 | temporal | 321 |
| 3 | open-domain | 96 |
| 4 | single-hop | 841 |
| 5 | adversarial | 446 |

All tables in this document have been corrected to this mapping. Note the
ATANT audit (arXiv 2604.10981) further observes that the IDs do not cleanly
match question shapes (cat 1 questions are mostly "what" lookups; cat 4 gold
answers are paraphrased reflections) — we keep the canonical names for
cross-paper comparability but per-category labels should be read with that
caveat.

## Frozen protocol

| Parameter | Value |
|---|---|
| Dataset | `locomo10.json` (snap-research/locomo, commit-pinned copy in `benches/locomo/data/`) |
| Git commit | `b76571f` (+ harness commit) |
| Answerer model | `deepseek-chat`, temperature 0.0, max_tokens 200 |
| Judge model | `deepseek-chat`, temperature 0.0 |
| Retrieval | run 1–2: keyword LIKE + fan-out fallback · run 3: **BM25** (Okapi, k1=1.2, b=0.75, Robertson IDF) · top-k = 10 |
| Ingest | one chunk per dialog turn (`[session date] speaker: text`), id = `dia_id`; consecutive cross-speaker turns linked by `caused` edges (confidence 0.4, temporal) |
| Categories | 1 (multi-hop), 2 (temporal), 3 (open-domain), 4 (single-hop), 5 (adversarial) — see "Category ID mapping" note below |
| Run | 2026-07-27, single run, all 1,986 questions, 0 errors |

Reproduce:

```bash
export DEEPSEEK_API_KEY=...
cargo build --release
./target/release/causal-memory-locomo run --all \
    --data benches/locomo/data/locomo10.json --concurrency 8
```

Per-question results: `benches/locomo/results/run_<ts>_conv<N>.jsonl`
(gitignored); summary: `run_<ts>_summary.json`.

## Results

### Run 5 — 20260727_165556 (commit `a40ae4d`→`360543f`, balanced prompt — **adopted**)

| Category | n | Accuracy |
|---|---|---|
| 1 multi-hop | 282 | 24.1% |
| 2 temporal | 321 | 49.2% |
| 3 open-domain | 96 | 19.8% |
| 4 single-hop | 841 | 74.0% |
| 5 adversarial | 446 | **91.5%** |
| **overall** | 1,986 | **64.2%** |
| cats 1–4 | 1,540 | **56.3%** |
| evidence retrieval hit rate | — | 74.4% |

### Run 4 — 20260727_162401 (abstention-max prompt, overshot)

| Category | n | Accuracy |
|---|---|---|
| 5 adversarial | 446 | 94.4% |
| cats 1–4 | 1,540 | 51.8% (over-refusal) |
| **overall** | 1,986 | 61.3% |

### Run 3 — 20260727_155001 (commit `09d256d`, BM25 retrieval)

| Category | n | Accuracy |
|---|---|---|
| 1 multi-hop | 282 | 25.9% |
| 2 temporal | 321 | 57.6% |
| 3 open-domain | 96 | 24.0% |
| 4 single-hop | 841 | **75.4%** |
| 5 adversarial | 446 | 84.3% |
| **overall** | 1,986 | **65.0%** |
| cats 1–4 (Genesys protocol) | 1,540 | **59.4%** |
| evidence retrieval hit rate | — | **74.4%** |

### Run 2 — 20260727_151234 (commit `bea2201`, temporal-grounding prompt)

| Category | n | Accuracy |
|---|---|---|
| 1 multi-hop | 282 | 22.3% |
| 2 temporal | 321 | **49.8%** |
| 3 open-domain | 96 | 19.8% |
| 4 single-hop | 841 | 60.3% |
| 5 adversarial | 446 | **89.5%** |
| **overall** | 1,986 | **57.8%** |
| cats 1–4 (Genesys protocol) | 1,540 | **48.6%** |
| evidence retrieval hit rate | — | 61.2% |

### Run 1 — 20260727_022016 (commit `b76571f`, first frozen baseline)

| Category | n | Accuracy |
|---|---|---|
| 1 multi-hop | 282 | 20.6% |
| 2 temporal | 321 | 20.2% |
| 3 open-domain | 96 | 17.7% |
| 4 single-hop | 841 | 58.1% |
| 5 adversarial | 446 | 93.3% |
| **overall** | 1,986 | **52.6%** |
| cats 1–4 | 1,540 | **40.8%** |
| evidence retrieval hit rate | — | 60.2% |

### Run 1 → Run 2: one prompt line, controlled experiment

Retrieval, DBs, models, temperature, top-k all unchanged; the only delta is a
temporal-grounding instruction in the answerer prompt (resolve relative dates
against the `[session_N YYYY-MM-DD]` chunk prefix).

- **cat 2 (temporal): 20.2% → 49.8% (+29.6pp)** — the predicted bottleneck,
  fixed at the prompt level. This confirms the failure was in answer
  normalization, not in memory.
- cats 1/3/4: +1.7/+2.1/+2.2pp — within judge variance.
- **cat 5 (abstention): 93.3% → 89.5% (−3.8pp)** — a real regression: the
  added instruction made the answerer slightly more eager to commit. Tuning
  abstention back without losing the temporal gain is open work.

Context (published, different answerer models — not strictly comparable):
Mem0 66.9 · Zep 75.14 · Genesys 85.55 (gpt-4o-mini answerer, cats 1–4) ·
**Letta plain-files agent 74.0** (2026-07 — a filesystem with no graph at
all beats most structured systems on this benchmark; evidence that LoCoMo
rewards recall volume, not structure).

### Run 2 → Run 3: BM25 retrieval, controlled experiment

DBs, prompts, models, top-k unchanged; the only delta is retrieval:
LIKE substring + fan-out → Okapi BM25 (k1=1.2, b=0.75, Robertson IDF,
pure-Rust `bm25.rs`, now the library's default keyword ranking).

- **evidence hit rate: 61.2% → 74.4% (+13.2pp)** — BM25 did exactly what it
  was built for. LIKE requires literal substring overlap; BM25 scores
  unordered term matches with tf saturation and length normalization.
- **overall: 57.8% → 65.0%; cats 1–4: 48.6% → 59.4%** — within ~7pp of
  Mem0's published 66.9 (different answerer, not strictly comparable).
- cat 4 (single-hop): 60.3% → 75.4% — biggest beneficiary.
- **cat 5 (abstention): 89.5% → 84.3%** — the decline continues. This is now
  a confirmed trend across three runs: *better retrieval → more
  plausible-looking context → more committed answers on unanswerable
  questions*. Retrieval quality and abstention are in real tension;
  see failure analysis.

### Run 3 → Run 4 → Run 5: the abstention dial, three controlled prompts

- Run 4 (abstention-max: "answer only when a memory DIRECTLY states the
  fact"): cat 5 84.3% → **94.4%**, but cats 1–4 −7.6pp — the model started
  refusing answerable questions (cat 3 fell to 13.5%).
- Run 5 (balanced: keep the direct-evidence rule + "a memory that directly
  addresses the question MUST be answered — a partial answer beats a
  refusal"): **cat 5 91.5% with cats 1–4 back to 56.3%** (vs run 3's
  84.3% / 59.4%). Run 5's prompt is the adopted production default:
  abstention within 3pp of the max at ~95% of the best cats 1–4 score.

## Failure analysis

Three distinct bottlenecks, measured rather than guessed:

1. **Retrieval (shrinking).** Evidence hit rate 60.2% → 74.4% after BM25.
   Remaining ~26% misses: paraphrases with no term overlap (needs embeddings
   — DeepSeek has no embeddings endpoint, so this needs a second provider),
   multi-evidence questions where only part of the evidence is retrieved.

2. ~~Temporal normalization~~ **(fixed in run 2).** Run 1's answerer echoed
   relative dates from the dialog ("yesterday", "next month") instead of
   resolving them against the session date stamped on every chunk (gold
   "7 May 2023" vs predicted "Yesterday (2023-05-08)"). One prompt line
   demanding absolute dates took cat 2 from 20.2% to 49.8%.

3. **Abstention erosion (the emergent finding of run 3, addressed in run 5).**
   Adversarial accuracy fell monotonically across runs 1–3: 93.3% → 89.5% →
   84.3% — as retrieval got better, the answerer got bolder on questions it
   should refuse. The system retrieves *something* plausible for almost
   every question, and plausible context invites hallucination. Run 5's
   balanced prompt (direct-evidence rule + must-answer counterweight)
   recovered abstention to 91.5% at ~95% of the best cats 1–4 score.

**Strength: adversarial abstention is the property that matters most for an
agent memory layer you actually trust** — and it is now a documented,
tunable dial rather than an accident.

## What this benchmark does not measure

LoCoMo tests factual recall from chit-chat. It does not test the thing
causal-memory is built for: **survival of decision→outcome structure across
context compaction** (see `docs/design.md` — text recall degrades to 45%
after 5 compactions while the causal table stays at 100%). That experiment
now exists: `causal-memory-locomo compact --compact 5` replays LoCoMo with
sessions compressed k times, contrasting text-only memory (condition A)
against text + un-compacted causal edges (condition B).

## Compaction survival (run 20260727_174000, k = 5)

Every session compressed 5 times by an LLM before QA; the causal edges are
extracted from the *original* text and never compacted (they live outside
the context window — that is the architecture).

| Category | A: text-only | B: text + causal | B − A |
|---|---|---|---|
| 1 multi-hop | 23.0% | 23.4% | +0.4pp |
| 2 temporal | 23.7% | 52.6% | **+28.9pp** |
| 3 open-domain | 25.0% | 20.8% | −4.2pp |
| 4 single-hop | 36.6% | 75.3% | **+38.7pp** |
| 5 adversarial | 91.9% | 91.5% | −0.4pp |
| **overall** | **44.5%** | **65.3%** | **+20.8pp** |

Read: five compactions collapse text-only memory from 65.0% (uncompressed
run 3) to 44.5%. Adding the never-compacted causal edges brings the system
back to 65.3% — **statistically indistinguishable from having no
compaction at all**. Temporal and single-hop questions benefit most: dates
and facts survive verbatim in edge text. cat 3 (open-domain) is the one
regression (small n=96) — compressed text plus flat adjacent-turn edges
does not help counterfactual/inferential synthesis; a real
decision-extractor (not adjacent-turn pairing) is the fix.

This is the experiment the system is designed to win, and it did.
