# LoCoMo Benchmark — causal-memory

Baseline evaluation of causal-memory on the [LoCoMo](https://github.com/snap-research/locomo)
long-conversational-memory benchmark (`locomo10.json`: 10 conversations, 1,986 questions).

**Headline (run 2): overall 57.8% (1,148/1,986) · cats 1–4: 48.6% (749/1,540) · adversarial abstention: 89.5% (399/446) · zero judge errors.**

This is an honest baseline, published with the same spirit as the v0.4.1 retrieval
finding: causal-memory is a **causal layer**, not a general-purpose factual memory —
and this benchmark measures mostly the latter. See [Failure analysis](#failure-analysis).

## Frozen protocol

| Parameter | Value |
|---|---|
| Dataset | `locomo10.json` (snap-research/locomo, commit-pinned copy in `benches/locomo/data/`) |
| Git commit | `b76571f` (+ harness commit) |
| Answerer model | `deepseek-chat`, temperature 0.0, max_tokens 200 |
| Judge model | `deepseek-chat`, temperature 0.0 |
| Retrieval | `search_causal` keyword LIKE + keyword fan-out fallback, top-k = 10 |
| Ingest | one chunk per dialog turn (`[session date] speaker: text`), id = `dia_id`; consecutive cross-speaker turns linked by `caused` edges (confidence 0.4, temporal) |
| Categories | 1 (single-hop), 2 (temporal), 3 (multi-hop), 4 (open-domain), 5 (adversarial) |
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

### Run 2 — 20260727_151234 (commit `bea2201`, temporal-grounding prompt)

| Category | n | Accuracy |
|---|---|---|
| 1 single-hop | 282 | 22.3% |
| 2 temporal | 321 | **49.8%** |
| 3 multi-hop | 96 | 19.8% |
| 4 open-domain | 841 | 60.3% |
| 5 adversarial | 446 | **89.5%** |
| **overall** | 1,986 | **57.8%** |
| cats 1–4 (Genesys protocol) | 1,540 | **48.6%** |
| evidence retrieval hit rate | — | 61.2% |

### Run 1 — 20260727_022016 (commit `b76571f`, first frozen baseline)

| Category | n | Accuracy |
|---|---|---|
| 1 single-hop | 282 | 20.6% |
| 2 temporal | 321 | 20.2% |
| 3 multi-hop | 96 | 17.7% |
| 4 open-domain | 841 | 58.1% |
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
Mem0 66.9 · Zep 75.14 · Genesys 85.55 (gpt-4o-mini answerer, cats 1–4).

## Failure analysis

Two distinct bottlenecks, measured rather than guessed:

1. **Retrieval (40% of questions).** Evidence hit rate is ~60% — for 2 in 5
   questions the gold-evidence chunk never reaches the answerer. Keyword LIKE
   retrieval is the ceiling here; this is exactly the v0.4.1 finding
   (keyword retrieval ≈ causal retrieval on fresh data) restated at scale.
   Fix path: embedding-based retrieval (`search_causal_semantic` is already
   implemented — DeepSeek has no embeddings endpoint, so this needs a
   second provider), BM25 ranking, hybrid fusion.

2. ~~Temporal normalization~~ **(fixed in run 2).** Run 1's answerer echoed
   relative dates from the dialog ("yesterday", "next month") instead of
   resolving them against the session date stamped on every chunk (gold
   "7 May 2023" vs predicted "Yesterday (2023-05-08)"). One prompt line
   demanding absolute dates took cat 2 from 20.2% to 49.8%.

**Strength: adversarial abstention (89.5–93.3%).** When the information is
not in memory, the system says so instead of hallucinating — the property
that matters most for an agent memory layer you actually trust. Run 2 shows
this is tensioned against answer eagerness and needs deliberate tuning.

## What this benchmark does not measure

LoCoMo tests factual recall from chit-chat. It does not test the thing
causal-memory is built for: **survival of decision→outcome structure across
context compaction** (see `docs/design.md` — text recall degrades to 45%
after 5 compactions while the causal table stays at 100%). A compaction-aware
variant of this benchmark (compress sessions k times before QA) is future
work — that is the experiment this system is designed to win.
