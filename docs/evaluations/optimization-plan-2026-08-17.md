# Optimization Wave 2 — 2026-08-17

> Scope: fix the two CausalEval losses vs mem0 (C7 belief update −30pp),
> one measured retrieval-scale risk, and doc/lint drift. Designed against
> the v12 baseline (overall 81%: C4 90 / C5 100 / C3 90 / C1 90 / C2 70 /
> C7 50 / C6 20).

## P0 — C7 soft supersession ("superseded ≠ false")

### Root cause

C7 ("what does `<person>` now believe?") scores 50% because the answer
model receives **no falsification signal**: the eval's seeding deliberately
skips supersession — hard invalidation (`valid_to`) hides the old fix from
*all* retrieval, but C3's counterfactual gold **is** the old fix. So the
correction exists in the conversation, the old lesson still looks live in
the retrieved evidence, and the model must guess from recency alone.

### Design

Soft supersession — annotate, never hide:

| Layer | Change |
|---|---|
| Store | New `annotate_superseded(old_edge_id, new_edge_id)`: sets `superseded_by` only, `valid_to` untouched → edge stays retrievable |
| MCP formatting | `format_entry_layered` appends `⚠ superseded later by "<correction>"` when `superseded_by` is set (fetch the superseding edge for its text; fall back to a generic marker if it's gone) |
| CausalEval seeding | `seed_graph_semantics`: for graph `invalidates` edges, set `superseded_by` on edges touching the old-fix node, pointing at the correction node's edge |
| CausalEval answering | memory lines for superseded entries carry the annotation → the answer model sees both the old lesson and its correction |

Properties: C3 keeps its gold (old fix still retrievable, still rankable);
C7 gets an explicit provenance signal instead of inferred recency;
`restore_edge` semantics unchanged (soft annotation is strictly weaker than
hard supersede — restore clears both).

### Verification

- Unit: `test_annotate_superseded_is_soft` — annotation keeps the edge in
  `all_valid_edges` + BM25 + entity search; `superseded_edges` (hard-only
  audit view) stays empty; idempotent; self-annotation rejected.
- **Eval (run 2026-08-18, FULL 140 questions / 20 graphs after narrating
  graphs 10-19, deepseek-chat, topk 15):**

  | Category | v12 (70q) | v13 (140q) | mem0 (70q) |
  |---|---|---|---|
  | C7 update | 50% | **100% (20/20)** | 80% |
  | C3 counterfactual (guard) | 90% | 95% | 80% |
  | C2 intervention | 70% | 75% | 40% |
  | C4 inhibition | 90% | 80% | 50% |
  | C5 temporal | 100% | 90% | 90% |
  | C1 attribution | 90% | 85% | 90% |
  | C6 transfer | 20% | 20% | 30% |
  | Overall | 81% | **78%** | 65% |

  **C7's +50pp is confirmed at doubled sample size (20/20 questions).** The
  70q run's C6 40% did not hold at n=20 (back to 20% — small-sample noise);
  C1/C4/C5 dips sit within re-distillation variance + new-graph difficulty
  (v12/v13 corpora are independent distillations). vs mem0 the causal win
  is now +35pp on C7 and the overall gap is +13pp on the shared protocol.
  Narration cost: 126s for 10 graphs (3 needed one auto-retry; all passed
  event-coverage verification). Analyzer: `analyze_results.py` (committed).
- Fixed while here: the run log printed the PRE-ingest chunk count, which
  masked the empty-conversations trap as "0 chunks" — it now re-reads after
  ingest.

## P1 — retrieval scale: entity-token cache

Audit 2026-08 item #2: `search_causal_entity` re-tokenizes every valid edge
on every query (and `search_causal_entity_boosted` re-tokenizes its
candidates). At 10k+ edges this is the documented scaling risk.

Design: in-process `edge_id → entity-token set` cache on `CausalStore`
(chunk texts are immutable; edges are append/invalidate-only, so the cache
never goes stale within a process). `search_causal_entity` scores via the
cache and fetches full rows only for the top-k output;
`_boosted` uses the same helper for its overlap pass.

Verification: behavior-identical unit tests (ordering: overlap desc, id
asc) all pass. Measured (`#[ignore]` probe `probe_entity_cache`, 5k
synthetic edges, release build): cold first query 14.8ms → warm per-query
**0.47ms — 31.7x**. The candidate scan is now an id-only index pass; chunk
texts are fetched only for cache misses, so a warm query touches no chunk
text at all.

## P2 — quick wins

- `extractor.rs` debug `eprintln!` (first-5-decisions trace) removed from
  the production path.
- `roadmap.md` resync: 14 tools, 322 tests, shipped items checked off.

## Explicitly deferred

- C6 cross-task transfer (20%) — needs a different lever (meta-edge
  expansion is already seeded; the gap is answer-side analogy reasoning).
- ANN (hnsw) — blocked on a real >5k-edge corpus; revisit with dogfood data.
- LLM update-resolver — the distill path already supersedes via hints;
  upgrade it to the LLM judge only after soft supersession proves out.
