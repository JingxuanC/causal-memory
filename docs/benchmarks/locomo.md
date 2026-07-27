# LoCoMo Benchmark — causal-memory

Baseline evaluation of causal-memory on the [LoCoMo](https://github.com/snap-research/locomo)
long-conversational-memory benchmark (`locomo10.json`: 10 conversations, 1,986 questions).

**Headline: overall 52.6% (1,045/1,986) · cats 1–4: 40.8% (629/1,540) · adversarial abstention: 93.3% (416/446) · zero judge errors.**

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

## Results (run 20260727_022016)

| Category | n | Accuracy | Notes |
|---|---|---|---|
| 1 single-hop | 282 | 20.6% | retrieval bottleneck |
| 2 temporal | 321 | 20.2% | relative-date normalization (see below) |
| 3 multi-hop | 96 | 17.7% | needs both retrieval hops |
| 4 open-domain | 841 | 58.1% | |
| 5 adversarial | 446 | **93.3%** | abstention strength |
| **overall** | 1,986 | **52.6%** | |
| cats 1–4 (Genesys protocol) | 1,540 | **40.8%** | |
| evidence retrieval hit rate | — | 60.2% | gold-evidence chunk in top-10 |

Context (published, different answerer models — not strictly comparable):
Mem0 66.9 · Zep 75.14 · Genesys 85.55 (gpt-4o-mini answerer, cats 1–4).

## Failure analysis

Two distinct bottlenecks, measured rather than guessed:

1. **Retrieval (40% of questions).** Evidence hit rate is 60.2% — for 2 in 5
   questions the gold-evidence chunk never reaches the answerer. Keyword LIKE
   retrieval is the ceiling here; this is exactly the v0.4.1 finding
   (keyword retrieval ≈ causal retrieval on fresh data) restated at scale.
   Fix path: embedding-based retrieval (`search_causal_semantic` is already
   implemented — DeepSeek has no embeddings endpoint, so this needs a
   second provider), BM25 ranking, hybrid fusion.

2. **Temporal normalization (dominates category 2 even when hit=True).**
   The answerer echoes relative dates from the dialog ("yesterday",
   "next month", "last year") instead of resolving them against the session
   date that is stamped on every chunk. Example: gold "7 May 2023", predicted
   "Yesterday (2023-05-08)". Fix path: answerer prompt that includes the
   session reference date and demands absolute dates.

**Strength: adversarial abstention (93.3%).** When the information is not in
memory, the system says so instead of hallucinating — the property that
matters most for an agent memory layer you actually trust.

## What this benchmark does not measure

LoCoMo tests factual recall from chit-chat. It does not test the thing
causal-memory is built for: **survival of decision→outcome structure across
context compaction** (see `docs/design.md` — text recall degrades to 45%
after 5 compactions while the causal table stays at 100%). A compaction-aware
variant of this benchmark (compress sessions k times before QA) is future
work — that is the experiment this system is designed to win.
