# Multi-hop / open-domain retrieval via graph expansion

> Design for lifting LoCoMo cat1 (multi-hop) and cat3 (open-domain) to ≥80%
> accuracy. All retrieval-side mechanisms are deterministic, LLM-free, and
> one-shot — compatible with the AMC Search contract if we ever re-tag.
> Date: 2026-08-06.

## 0. Diagnosis (measured, 2026-08-06 fixed-judge run)

| Category | Questions | Correct | Acc (valid) | Evidence hit |
|---|---|---|---|---|
| cat1 multi-hop | 282 | 85 | 31.1% | 37% |
| cat3 open-domain | 96 | 46 | 48.4% | 26% |
| all | 1986 | 1388 | 71.7% | 74.4% |

Failure anatomy:

- cat1 needs **2–7 evidence chunks per question**, scattered across sessions
  (2: 80q, 3: 38q, 4: 35q, 5: 16q, 7: 7q).
- 188 wrong: 109 had **zero/partial evidence retrieved** — of the 400
  unretrieved evidence chunks, **398 (99.5%) exist in the store DBs**.
  Retrieval cannot compose across sessions, not an ingest gap.
- cat3 wrongs: profile evidence (the person's facts) rarely enters context.

Root cause: `retrieve()` in the harness does single-pass BM25 + semantic RRF
**over causal_edges**. The graph (nodes = turn chunks, edges = turn adjacency +
distilled causal episodes) already exists in SQLite — it is simply unused.

## 0.5 Measured so far (2026-08-06)

Retrieval-level (evidence_hit, search-only runs, full 1986 questions):

| Category | BM25-only (all past runs) | A1v2 (semantic + entity boost) | Δ |
|---|---|---|---|
| all | 71% | 77% | +123 q |
| cat1 multi-hop | 50% | 66% | +43 |
| cat3 open-domain | 33% | 41% | +7 |
| cat2 temporal | 65% | 68% | +10 |
| cat4 single-hop | 80% | 84% | +37 |
| cat5 adversarial | 80% | 86% | +26 |

Key infrastructure findings:

- **Every historical benchmark score was BM25-only** — no embedding endpoint
  was ever configured at QA time (the 2048-dim vectors in the DBs came from a
  session-scoped HTTP embedder that is gone). The semantic signal had never
  run.
- A1 v1 (entity list fused as a peer RRF list) **regressed** evidence_hit
  (conv0-50: 62%→46%) — a bare entity list has no precision signal (all
  person-anchored edges tie at overlap 1) and its arbitrary ordering
  displaced lexical hits. v2 fixed it: multiplicative boost on cosine
  similarity (`score = sim × (1 + 0.5·overlap)`).
- Local ONNX embeddings now work on this macOS 13 machine (CLT SDK, no full
  Xcode): fastembed `ort-load-dynamic` + onnxruntime 1.26 dylib (1.27/1.28
  are macOS-14 builds and fail to dlopen) + model cache copied out of Docker
  (HF unreachable from the host). Harness gained `--reembed` to overwrite
  mismatched stored vectors with BGE.

## 1. Goal

- cat1: 31.1% → ≥80% (need ~226/282 correct; +141)
- cat3: 48.4% → ≥80% (need ~77/96 correct; +31)
- Overall (all-in): 69.9% → ~78%+

## 2. Mechanisms

### A1 — Entity-anchored expansion (first build; one signal change)

Entity extraction (no LLM):

- Token = mid-sentence capitalized word, len ≥ 2; exclude sentence-initial
  words (prev char is `. ! ? "` or start) and a stoplist of common starters
  (The/She/He/What/When/How/Why/I/We/They/It/This/That/There/But/So/And/
  Do/Does/Did/Have/Has/Was/Were/Will/Can/Could…). Hyphenated/possessive
  suffixes stripped (`Melanie's` → `Melanie`).
- Precompute per-edge entity sets at ingest (edge text = from + to chunk
  text); corpus is ~700 chunks × ~20 tokens — trivial.

Scoring:

- Query entities `Qe`; candidate edge entities `Ee`.
- Entity list ranks candidates by `|Qe ∩ Ee|` (desc), tie-broken by base
  BM25 score; take top-k×2.
- Fuse as a **third ranked list into the existing RRF** (k=60) — scale-free,
  no weight tuning. Entity boost deliberately frequency-favors names
  (Melanie in many chunks is exactly what we want — do NOT apply idf).

Why it targets the failure: "Where has Melanie camped?" — Qe = {Melanie};
every Melanie chunk (across sessions) becomes a candidate. cat1 and cat3 are
overwhelmingly person-anchored; this pulls the whole profile.

### A2 — Hop expansion over the existing graph (second build)

- Seed = current top-k edges → endpoint chunk set `S`.
- **1 hop**: all edges touching any chunk in `S` (turn adjacency + causal),
  via SQL `WHERE e.from_id IN S OR e.to_id IN S`, `valid_to IS NULL`.
- **2 hop**: endpoints of 1-hop edges expanded again **only through distilled
  causal episodes** (decision→outcome; the semantically meaningful jumps).
- Score neighbors: `max(seed score) × λ^hop` (λ ≈ 0.6); keep only neighbors
  sharing ≥1 query entity or ≥1 lexical token with the query (precision
  gate).
- Budget: ≤40 neighbors; final top_k allocation keeps ≥half slots on
  original seeds.
- Fuse as a **fourth ranked list into RRF**.

### A3 — Answer-side (harness only; LLM prompt + judge)

- cat1 answer prompt: "The question may require combining multiple memories;
  list the facts you found, then answer."
- Evidence already carries `[session_N <ts>]` prefixes — keep them; they give
  the model temporal ordering across sessions.
- cat3 answer prompt: "The answer may not be stated directly; infer from the
  person's profile and your own knowledge."
- Judge: preprocess cat3 gold at the first `;` (mem0's convention; LoCoMo
  cat3 golds are long lists — a correct partial answer must not be marked
  wrong for missing trailing items).

## 3. Iteration protocol (cost control)

1. Add `--search-only` to the harness: retrieval + evidence_hit only, **zero
   LLM calls** → iterate A1/A2 cheaply on retrieval quality before spending
   answer+judge calls.
2. Per variant: smoke `--limit 50` (evidence_hit + acc) → full run
   (~50–70 min) only for finalists.
3. Ablation matrix: baseline / +A1 / +A1+A2 / +A1+A2+A3 / +A3 judge-fix.

## 4. Expected impact (honest)

- A1 alone: cat1 evidence_hit 37% → ~70–80%; cat1 acc +15–25pp (person-anchored
  questions dominate the miss set).
- A1+A2: adds session-adjacent + causally-linked context → cat1 ~60–70%.
- +A3: composition prompt → toward 80%. Two iterations may be needed.
- cat3: A1 (profile in context) + A3 prompt/judge → 70–85%. Residual ceiling:
  v4-flash's world knowledge for inference questions.
- Judge strictness remains a floor: even complete evidence can be marked
  wrong on borderline wording (same as cat2's ±14-day rule).

## 5. Risks

- Entity misfires (capitalized noise) → stoplist + ≥1-overlap gate + smoke
  check of retrieved sets.
- Expansion flooding top_k → budget allocation (seeds ≥ half).
- Answer-model nondeterminism at temp 0 (v4-flash) → rely on 1986-question
  aggregate, not smoke deltas.

## 6. AMC compatibility

A1/A2 are deterministic, single-call, LLM-free → contract-compliant if we
later re-tag. A3 lives in the answer side (platform-controlled in AMC) —
harness-only.

## 7. Build order

1. ✅ Phase 0: evidence-in-DB verified (398/400).
2. `--search-only` mode (iteration speed).
3. A1 entity expansion → smoke → full run.
4. A2 hop expansion → smoke → full run.
5. A3 prompts + cat3 judge preprocessing → full run.
6. Ablation report vs baseline 71.7% (all-in 69.9%).
