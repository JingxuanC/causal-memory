# Performance & precision audit — 2026-08-06

## Latency (measured, conv 0 = 199 questions, 449 edges)

| Config | Per query |
|---|---|
| Retrieval only (BM25 + entity + hop) | ~45ms (debug) |
| Semantic + entity-boost + hop (release) | ~30ms |

- Embedding (ONNX BGE, ~20ms/query) is ~2/3 of per-query cost; retrieval itself ~10ms.
- **Verdict: no latency problem at evaluation scale (hundreds–thousands of edges).**

### Fixed

1. **Embedder init hang**: with `local-embed` compiled in but model/dylib unavailable,
   `init_embedder()` attempted an HF download that stalled ~150s on unreachable
   hosts. Now: cache-dir pre-check (`FASTEMBED_CACHE_DIR` / `.fastembed_cache`),
   instant clear error; creating the cache dir is the opt-in for
   download-on-first-use. (embed.rs)

### Scaling risks (10k+ edges — production long-session corpora)

| # | Pattern | Where | Fix direction |
|---|---|---|---|
| 2 | Per-query full re-tokenization of every edge (entity sets) | `search_causal_entity`, `search_causal_hop` | persist token/entity sets at ingest (column or side table) |
| 3 | Per-query BM25 index rebuild over all candidates | `search_causal_bm25` (documented trade-off: exact per-filter IDF) | cache index per (task_tag-filter), invalidate on write |
| 4 | Brute-force cosine over all edge embeddings | `search_causal_semantic`, `_entity_boosted` | ANN (hnsw) past ~5k edges |
| 5 | Every read writes SQLite (`record_access`) | all search paths (already one batched UPDATE) | optional access-tracking flag for hot read paths (AMC server unaffected — separate store) |

## Precision (measured via `--search-only` evidence_hit, conv 0, 199 q)

### Parameter sweep — plateau, not a lever

| RRF_K \ ENTITY_BOOST | 0 | 0.5 | 1.0 | 2.0 |
|---|---|---|---|---|
| 30 | 162 | 163 | 164 | 164 |
| 60 | 163 | 162 | 163 | 162 |
| 120 | 162 | 164 | 163 | 163 |

All within ±1-2 questions of each other (noise). Rank-based RRF fusion is already
robust to weighting; neither knob moves evidence coverage. Both are env-overridable
now (`CAUSAL_MEMORY_RRF_K`, `CAUSAL_MEMORY_ENTITY_BOOST`) for future datasets.

### The real retrieval lever: recall depth

| Config | evidence_hit (conv 0) |
|---|---|
| topk 10 (default) | 163-164/199 (82%) |
| **topk 20** | **172/199 (86%)** |

The remaining evidence misses sit at ranks >20 — signal coverage, not fusion
weighting. Suggest running the next full ablation at `--topk 20`.

### Precision frontier (unchanged)

A3 (answer side): cat1 composition prompt + cat3 inference prompt + judge gold
truncation. Ceiling analysis: cat1 73% / cat3 65.6% even with perfect answering
on evidence-present questions (docs/design/multi-hop-expansion.md §0.5).
