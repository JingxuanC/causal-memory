# Changelog

All notable changes to causal-memory are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.3] - Unreleased

### Added
- **Flip-path marking (recall provenance)** — every spreading-activation
  result now carries `hop` (0 = direct seed, N = lit in hop N) and `via`
  (the winning edge's relation + source). `search_causal` / `search_memory`
  accept `explain=true` (MCP + Python bindings) to append per-hit tags like
  `[spread hop=2 via prevented←"skip tests"]`; default output is
  byte-identical to before (activation values and ranking untouched).
- **Recall audit (schema v13)** — every recall persists a best-effort row
  to the new `recall_audit` table (query, seeds, hop summary, per-result
  provenance, latency; retention: 30 days / newest 10k rows, swept
  amortized). Audit write failures never affect retrieval (counter + warn).
- **Observability endpoints (MCP HTTP + AMC servers)** — hand-rolled
  in-process registry, no metrics/OTel crates: `/metrics` (Prometheus
  text: RED per tool + recall seeds/activated/results + store gauges +
  uptime), `/healthz`, `/readyz` (store probe), `/debug/recall?query=...`
  (live full recall trace as JSON), `/debug/recalls` (persisted audit
  rows). Structured JSON logs on stderr via `CAUSAL_MEMORY_LOG_FORMAT=json`.
  OTLP export deferred until a collector exists.

### Fixed
- **`stats` on an empty database** no longer errors (`MIN/MAX/AVG` return
  NULL on zero rows; now COALESCEd to the initial q_value 0.5).
- **Broken-pipe panic when piping to `head`/`less`** — the CLI restores
  SIGPIPE's default disposition (Rust ignores it by default), so both the
  cargo binary and the pip console script die quietly like normal Unix
  tools instead of panicking on `println!`.

## [0.9.2] - 2026-08-25

### Added
- **`causal-memory` console script in the PyPI package** — `pip install
  causal-memory` now also installs the full CLI / MCP server command (no
  Rust toolchain needed): bare `causal-memory` runs the MCP stdio server,
  and every cargo subcommand (`stats`, `sleep`, `export` / `import`,
  `wiki`, …) works the same. The cargo binary and the console script
  share one dispatcher (`causal-memory-cli` refactored to lib+bin).
- **JSON config file + `setconfig` commands** (`config.rs`): configuration
  can live in `$CAUSAL_MEMORY_CONFIG` or
  `~/.local/share/causal-memory/config.json` instead of process env (env
  still wins). Manage it with `causal-memory setconfig KEY=VALUE …`
  (whitelisted keys, empty value deletes), `getconfig` (`*_KEY` values
  masked), and `config-path` — no more hand-exporting env vars into every
  agent process.

## [0.9.1] - 2026-08-25

### Fixed
- **UTF-8 char-boundary panic in the `remember` op** (`memory/ops.rs`):
  truncating multi-byte text could slice inside a UTF-8 sequence and panic.
- **hermes-plugin**: `on_memory_write` now matches the Hermes
  `MemoryProvider` ABC (accepts `metadata`); persisted `config.json` is
  reloaded on `initialize`; offline test suite no longer leaks LLM env vars.

### Changed
- The PyPI page now shows a pip-user quickstart (install, 30-second example,
  env-var configuration table, MCP-binary pointer) instead of the
  cargo-centric repo README.

## [0.9.0] - 2026-08-24

### Benchmarks
- **LongMemEval-S full-pipeline headline: 76.4% overall (382/500) at
  11,524 avg ctx tokens** (run 20260821_161122, git 5064b90): +4.0pp over
  the 8/20 run while cutting per-query context 32% (17,016 → 11,524;
  answer-phase input 8.51M → 5.76M tokens across 500 questions).
  multi-session 60.2% (+4.5), temporal-reasoning 69.9% (+7.5),
  single-session-preference 80.0% (+13.3), abstention 96.7%, evidence
  hit flat at 89.2%. All 48 verdict flips evidence-stable — the gain is
  the dilution cut, not retrieval luck. Docs updated with the mem0
  comparison (official 94.4% @ 6.8K tok/q on platform stack; independent
  same-harness repro 73.8%; judge-caliber discount documented).

### Added
- **`scripts/audit_fact_links.py`** — stdlib-only replication of the
  fact↔chunk linker policy (`entity_link_facts` / `link_fact_node` /
  `component_stats`): exact tokenizer, LINK_STOPWORDS, df filter,
  scope isolation, and top-8 truncation order. Reproduces the real-DB
  numbers (3,117 links / 7,857 edges / 29 components at ≥3+df≤20 vs
  9,764 / 21,151 / 17 pre-fix) and dumps random links for manual
  precision re-sampling (`--sample N`).
- **Phase C — incremental graph lifecycle (write-path patches)**
  (one-graph-convergence): `CausalGraph` gains live-patch APIs —
  `append_node` (O(1) SoA append), `add_patch_edge` (per-node overlay
  maps; a CSR middle insert is O(E) and shifts every stored edge index,
  so patches ride alongside the CSR segments in both spread directions),
  `invalidate_edges_between` (O(deg) validity flip), and
  `retire_node`/revive (superseded fact nodes neither seed nor surface
  until the next rebuild drops them). `record_decision` /
  `record_fact` (including replace, which retires the superseded nodes)
  and `invalidate_decision` patch the live graph, so new memories are
  visible to the very next query — verified by a dirty-counter assertion
  (no lazy rebuild fired) and a differential assertion (patched results
  == fully-rebuilt results across two instances on the same store). The
  lazy 5-writes/30s full rebuild stays as the drift bound; Phase B's
  seed-miss rebuild now rarely triggers.
- **Phase D — one consolidation loop over all types**
  (one-graph-convergence):
  - Fact half-life: stage 3 `downscale_facts` decays `agent_facts.confidence`
    by age from `updated_at` on the slowest tier (user_feedback, 90d —
    facts are high-trust "what is" knowledge); below `gc_threshold` facts
    retire. Report gains `facts_decayed` / `facts_gc` (dry-run counts too).
  - Fact supersession lineage (schema v12): `agent_facts.superseded_by`
    — `record_fact_replacing` retires AND records which fact replaced
    which in one write; revive clears it. The id powers the graph's
    write-path retire. Full soft-supersession display deferred to its own
    eval A/B (the current knowledge-update contract — new value replaces,
    old exits retrieval — is pinned by the MCP e2e).
  - REM/meta mining input includes facts: the pattern miner's input is a
    unified `MineItem` list — valid facts participate first-class
    (`fact:{id}`, stratum = scope), `similar_to` only (no outcome
    semantics), and the mined meta edges wire fact nodes into the causal
    content graph (+0.6 Meta spread). Fact-free stores (CausalEval) mine
    identically.
- **CausalEval 140-question regression (post Phase C+D)**: overall
  111/140 = 79.3% vs the 8/19 detect baseline 117/140 = 83.6%. The delta
  is judge noise, not retrieval: `evidence_hit` is statistically flat
  (122 vs 125; only 2 lost / 1 gained), and 16 of the 20 verdict flips
  had evidence retrieved in BOTH runs. Both lost-evidence questions are
  C16 (the cross-domain category that swings 7-9/20 run to run). Results
  archived as `benches/causal_eval/results_phaseCD_20260820.jsonl`.

### Fixed
- **Fact↔chunk link precision (entity-link false positives)**: the
  rebuild-time (`entity_link_facts`) and incremental (`link_fact_node`)
  linkers now share one policy — ≥3 **distinct non-stopword** shared
  tokens plus a df filter (tokens present in >20 chunks don't count),
  replacing the ≥2-token bar; `link_fact_node` also gained the stopword,
  df, and scope-isolation filters it previously lacked (write-path and
  rebuild now agree). Real-DB audit: sampled link precision 17%→33%
  (strict) / 29%→75% (lenient); fact links 9,764→3,116 (−68%, ~2.2/fact);
  graph 21,151→7,857 valid edges, 17→29 components (isolation restored).
- **Date-math questions misrouted into the aggregation pipeline**
  (retrieval quality): `looks_aggregation`'s "how many" phrase matched
  "How many days ago did I buy a smoker?" — a date-ARITHMETIC question
  over ONE event, not an enumeration — arming the full-session expansion
  (≤80 injected chunks), the 500-fact wide queries, and the verification
  loop. Gold needs exactly one turn; the result was 2-3x context
  inflation with ~64% noise tokens and answer dilution (LongMemEval
  temporal-reasoning lost 11 questions under the multipass pipeline; 7
  were date-math). The carve-out excludes "days/weeks/months/years/hours/
  long ago" and "how many ... between"; true aggregations ("How many
  books did I buy?") contain neither pattern and are unaffected.
  A/B on the 133 temporal questions: date-math subset 65%→74% (+9pp)
  with ctx median 23,988→9,914 tokens (-59%); non-date control flat
  (53/87→51/87); zero evidence flips — a pure noise-reduction gain.
  Context: the 8/9 baseline predated the multipass pipeline (Step A,
  8/19), so the first full-500 multipass run conflated this with the
  Phase A-D changes — source-level tracing showed entity links never
  participate in bench retrieval (hippocampus_boost is env-gated).
- **BM25 index candidates could exceed SQLite's host-variable limit**
  (pre-existing v10 bug, surfaced by LongMemEval's 246k-chunk shared
  store): `search_causal_bm25` / `search_facts_bm25` resolve candidate
  chunk ids from the persistent inverted index WITHOUT a task_tag
  filter (the tag applies to causal_edges below), so a few common tokens
  matched 98k+ chunk ids — the `IN (...)` list blew past SQLite's
  999-variable floor, the statement failed to prepare, and callers
  silently degraded to empty results. Both paths now cap index
  candidates at 900 and fall back to the task_tag-bounded full scan (the
  pre-index behavior) for oversized sets; regression test at 950 edges.
  First caught as LongMemEval 29.2% (empty retrieval) vs the 73.2%
  baseline.
- **Phase C+D review fixes** (3 regressions + 1 config):
  - Idempotent fact re-records no longer stack duplicate overlay edges —
    `add_patch_edge` upserts by (from, to): a repeat write updates the
    weight instead of adding a copy, so activation is never inflated and
    the overlay never grows unboundedly (regression test pins the
    one-edge activation value).
  - `link_fact_node` uses an incremental token→chunk inverted index
    (maintained by `append_node`, rebuilt by `build`) instead of
    re-tokenizing every graph node per fact write — write-path linking is
    now O(fact tokens × hits), the same shape as the rebuild-time linker,
    instead of O(V) tokenizes per write.
  - Fact participants no longer enter the stratified-replication pool:
    a fact's scope leaked into the strata set, clearing `confounded` for
    single-domain decision pairs (strata len ≥ 2 without any actual
    cross-task replication). Only causal endpoints pool; facts still mine
    `similar_to` as endpoints (regression test pins confounded=true
    despite a matching fact).
  - `half_life_fact_hours` (default 2160) replaces the hardcoded reuse of
    the user_feedback tier — facts have their own tunable decay knob.
- **Phase A entity-link overlap counted repeated tokens**: a chunk whose
  text repeated a word contributed the same token twice to the overlap
  count, letting a single distinct shared token fake "≥ 2 distinct
  tokens" and link below the documented threshold. Posting lists are now
  deduped at index-build time; regression test included (pure-function
  test, no store needed — the linker is extracted as
  `entity_link_facts`).
- **`MemoryHit.score` dual semantics**: the spread path returned
  `1/rank` while the RRF fallback returned `1/(60+rank)`. Both paths now
  use the RRF formula, so the field has one meaning across modes.

### Changed
- **Phase B review refactor (net −193 lines)**: `search_memory` /
  `search_memory_entries` converge on one shared presentation — both
  retrieval paths (spread engine, dual-pool RRF) produce the same
  `RankedHits` shape and share D4 routing + grouped rendering
  (`render_unified`) and hit materialization (`hits_from_ranked`). The
  unified engine moved to its own module (`memory/unified.rs`) with
  seeding (`unified_seed_ids`), freshness (`ensure_fresh_for`), typed
  split (`split_typed`) and edge ranking (`rank_edges_by_activation`) as
  named steps. `search_memory_entries` now joins hop-expansion edges in
  the RRF fusion like the text tool already did (they were computed but
  silently discarded before). `bm25_seed_ids` builds its scope filter
  conditionally, matching the module's SQL convention.

### Added
- **Phase B — unified retrieval engine (one engine)**
  (one-graph-convergence): `search_memory` / `search_memory_entries` are now
  served by a single spreading-activation run over the whole typed graph.
  Seeding is store-side and type-unified (`bm25_seed_ids`: the persistent
  BM25 index over BOTH `fact:{id}` and chunk namespaces, ranked by distinct
  token overlap, scope-filtered — plus semantic seeds when an embedder is
  configured); the graph's substring matches union in
  (`spreading_activation_seeded`, built on the extracted
  `spread_and_collect` core). Results split back into typed display rows —
  facts in activation order (`facts_by_ids`), causal edges ranked by their
  strongest activated endpoint (`edges_touching_chunks`). The dual-pool RRF
  path stays as fallback and A/B regression control; D4 intent routing and
  the grouped fact/causal display are unchanged. **Freshness (Phase C
  preview)**: a store-resolved seed that maps to no graph node proves the
  graph predates the write (the lazy 5-writes/30s rebuild hasn't fired) —
  the engine rebuilds once instead of silently dropping the seed, so it is
  never weaker than the store-direct path it replaces (caught by the MCP
  e2e: a fresh fact under the lazy threshold must still surface). Output
  tag `[unified/spread]` / mode `"spread"`.
- **Phase A — fact entity linking into the causal graph**
  (one-graph-convergence): `from_store` now deterministically links each
  `agent_facts` node to chunk nodes sharing ≥ 2 distinct tokens
  (`patterns::tokenize`, ASCII words + CJK bigrams; no LLM). Edges are
  bidirectional `Fact` edges (weight `0.3 + 0.1·overlap`, cap 0.8), so
  fact seeds reach causal chains and causal seeds surface facts —
  previously facts formed isolated scope-hub islands and spreading
  activation could never cross between the two memory layers. An
  inverted token→chunk index keeps linking O(total tokens); a per-fact
  cap (8) keeps generic keys from wiring to half the store.
  `record_fact` now marks the graph dirty (same lazy-rebuild contract as
  `record_decision`/`remember`). Deviation from the plan: the key→value
  self-link is skipped — the fact node text already carries
  `{key}: {value}` and linking runs on it.
- **AMC server on the production pipeline (single-track)**: the Agent
  Memory Challenge server is now a thin HTTP frontend over the shared
  `Memory` facade — `/add` → `remember`, `/search` →
  `search_memory_entries` — the same pipeline as MCP stdio/HTTP and the
  Python bindings. The private AmcStore/lexical scoring/private RRF
  (~400 lines) is deleted; per-user_id store isolation is physical (one
  db per user). `--write-mode distill|raw` controls the write-time
  strategy: full distillation (default; honest degrade to raw with a
  warning when no LLM env) vs pre-gatekeeping raw turns — both modes
  share the same BM25 + semantic + entity retrieval stack, so an A/B
  rerun of the leaderboard isolates the value of write-time
  distillation.
- **`search_memory_entries` + `MemoryHit`** (facade): the structured core
  of `search_memory` — both layers, per-layer semantic/BM25 fallthrough,
  hop expansion, RRF fusion, top-k — returning machine-readable hits
  for non-LLM frontends; the text tool wraps it unchanged.
- **`remember_raw_turns`** (facade): raw conversation turns into the
  retrieval pool with adjacent temporal edges (no write-time LLM).
- **`resolve_updates` MCP tool (15th)**: the C7 knowledge-update pass
  (candidate scan + LLM judge, the same pipeline sleep runs as stage 1.7)
  is now callable by agents — preview by default, `apply=true` writes.
  Exposed on stdio + HTTP via the shared facade, mirrored in the Python
  bindings. Detection previously lived only in manual CLI/sleep cycles.
- **`SupersessionAction` (Retire | Annotate)** parameterizes what the
  judge does to a falsified edge: hard-invalidate (sleep default) or
  soft-supersede (annotated, stays retrievable). `find_falsified_candidates`
  now returns the new-edge id so annotation carries full provenance.
- **CausalEval three-arm supersession experiment**
  (`--supersession-mode oracle|detect|detect-retire`, per-arm db files):
  - oracle (ground-truth + annotate): overall 84%, C7 100%
  - detect (production pipeline, resolver action): overall 84%, C7 100%,
    C3 100% — **the resolver itself fires 0 candidates on distilled
    corpora** (chunk-reuse never happens); C7 here is solved by the
    distill-path `supersedes` hint + negation memories, not by the
    resolver or the oracle annotation
  - detect-retire skipped: structurally identical to detect when the
    resolver contributes nothing
  Conclusion: the LLM resolver is for `record_decision`-path stores
  (real agent usage, validated on the live store); distilled corpora
  detect supersession at distill time instead.
- **Soft supersession** (`annotate_superseded`): mark an edge
  `superseded_by` without hiding it — "superseded ≠ false". The old lesson
  stays fully retrievable (counterfactual gold intact) while carrying
  provenance; MCP search output annotates it
  ("⚠ superseded later by a newer memory")
- CausalEval v13: seeds graph `invalidates` edges as soft supersession and
  surfaces the correction in the answer evidence — **C7 update 50% → 100%
  (20/20), confirmed at the full 140-question scale** after narrating the
  previously-empty graphs 10-19 (126s LLM cost, event-coverage verified);
  overall 78% on 140q vs mem0 65% on the shared protocol
- `analyze_results.py`: per-category accuracy vs v12/mem0 baselines for
  CausalEval result files (committed under benches/causal_eval/)
- Entity-token cache on `CausalStore` (audit 2026-08 #2): edge entity
  tokens computed once per process; `search_causal_entity` scans ids only
  and fetches texts on cache misses — measured **31.7x** faster warm
  queries at 5k edges (14.8ms → 0.47ms; `#[ignore]`d probe
  `probe_entity_cache` reproduces)
- Optimization wave 2 design + verification record:
  `docs/evaluations/optimization-plan-2026-08-17.md`
- Benchmark harness distill mode (unified-memory-design Phase 4): all three
  harnesses accept `--ingest raw|distill` (+ `--ingest-only`), writing to
  separate `*_distill.db` files so raw baselines stay intact; kind-based
  routing (facts/preferences → `agent_facts` with supersedes retirement,
  lessons/events → causal layer) and fact-lines-first answer prompts.
  Same-harness results (deepseek-chat answerer + judge, frozen protocols):
  LoCoMo 64.2% → **69.6%** (+5.4pp), LongMemEval 61.8% → **69.6%** (+7.8pp,
  knowledge-update 76.9% → 85.9%), Memora weekly 10 personas MPA 33.9% →
  **46.8%** (+12.9pp)
- Resumable distillation: per-question/per-persona `distill_done` marker
  tables — interrupted runs redo cleanly (item-level idempotency), and a
  unit whose LLM calls ALL failed (rate-limit storm, balance outage) is
  deliberately left unmarked instead of frozen as "successfully empty"
- Distill robustness: 3 retries with 2s/4s backoff on transient API errors
  (was 1); memora raw-ingest no longer duplicates turn edges on redo
- Unified retrieval (unified-memory-design Phase 2): `search_memory` MCP
  tool — facts + causal lessons fused by Reciprocal Rank Fusion (RRF,
  k=60) in one call; one query embedding serves both layers; cross-layer
  agreement outranks single-layer rank-1 hits (13 tools total)
- LLM distill ingest (unified-memory-design Phase 3): `causal-memory
  distill <session.json|dir> [--dry-run]` — one LLM call per session routes
  distilled facts/preferences → `agent_facts` (with supersedes-driven
  retirement of outdated values) and lessons/events → the causal store's
  existing `record_distilled` path
- Fact layer (unified-memory-design Phase 1, schema v6): flat facts
  ("user prefers TypeScript") alongside causal edges — `agent_facts` +
  `agent_facts_embeddings` tables, idempotent upsert on (key, value, scope),
  soft invalidation with revive-on-re-record, `replace_same_key` retirement
  of outdated values ("user switched to pnpm" invalidates "user uses npm"),
  BM25 default + optional embedding retrieval; new MCP tools `record_fact`
  and `search_facts` (12 tools total)
- BM25 keyword retrieval replaces LIKE as the default ranking for text
  queries (`search_causal_bm25`): token-overlap ranking, so word order and
  phrasing differences no longer zero out hits; the embedding/semantic path
  is unchanged and BM25 is its fallback
- LoCoMo evaluation harness with a frozen-protocol baseline (runs 1-5:
  temporal grounding, BM25 lift to 65.0% overall / 74.4% hit rate,
  abstention-aware answerer at 94.4%)
- Schema v5: stratified-replication fields on `meta_causal_edges`
  (`strata_count` / `strata` / `confounded` / `simpson`; NULL = untested)
- Stratified causal discovery (pattern miner upgrade, honest stand-in for a
  PC-style CI test): candidate patterns are grouped by decision-token
  signature and promoted at full confidence only when they hold in ≥ 2
  distinct task_tag strata; single-stratum patterns are marked `confounded`
  at half confidence, and direction flips across strata are flagged `simpson`
  (both surface in `search_patterns` output); re-mining upgrades/downgrades
  existing conclusions
- `intervention_query` stratified adjustment (engineering backdoor check):
  per-task_tag terminal-outcome distribution vs pooled, explicit Simpson's
  paradox warning when they disagree, optional `task_tag` filter parameter
- Sleep reactivation is now real consolidation, not just a report: replay
  priority feeds downscaling (protected edges decay at half rate with a
  lenient GC threshold — retention ∝ priority × recency × confidence), and
  replayed edges are marked via `last_accessed_at`, forming a cross-cycle
  replay → consolidate → survive feedback loop
- `counterfactual_query` MCP tool (10 tools total): contrastive/empirical
  counterfactual — compares recorded outcome distributions of a decision vs
  an alternative (semantic seeding with BM25 fallback), with a fixed
  disclaimer that this is NOT a Pearl Rung-3 SCM counterfactual
- `reconstruct_lesson` MCP tool: reconstructive retrieval (Schacter 2007) —
  Markov-blanket causal subgraph (new `store::markov_blanket`) serialized as
  ≤120-char stubs, LLM narrative reconstruction when configured
  (`llm::reconstruct_narrative`), and optional multi-sample calibration
  (`calibrate >= 2` independent reconstructions + token-Jaccard agreement,
  low agreement flags unreliable memories); degrades to stubs-only without
  an LLM
- `causal-memory export` / `import` subcommands for cross-agent causal
  sharing (insights/11 §8.5): JSONL with `format_version: 1`, chunk/edge/
  meta_edge records, best-effort secret redaction (sk-…, Bearer, password
  assignments, private-key headers; `--no-redact` to disable), filters
  (task-tag / min-confidence / since / include-invalidated), idempotent
  import keyed on (from_text, to_text, relation, event_time) with
  FNV-1a(text) chunk ids, `--dry-run` and `--task-tag` override
- `causal-memory bench-compaction`: reproducible harness for the
  compaction-degradation experiment — seeded deterministic scenario
  generator, independent session per compression depth, keyword-scored gold
  QA (no LLM judge), markdown report (`bench-results-<timestamp>.md`);
  compaction prompt lives in `benches/compaction_prompt.txt`
- `causal-memory bench-agent`: end-to-end trap-world ablation — the same
  LLM agent solves seeded trap-family tasks with (B) vs without (A) causal
  memory attached. Measured with glm-4-plus (seed 42): repeat-mistake rate
  **67% (A) vs 33% (B)**, post-search first-action hit rate 57%, both groups
  6/6 solved; results and full transcripts archived in
  `benches/agent/results/`. Debugging the harness surfaced and fixed three
  real issues: the record_memory observation was indistinguishable from the
  agent's self-echo (now a numbered `recorded: …` receipt), agents recorded
  imagined results without ever acting (harness now hard-blocks record
  before any real command observation), and search/record spirals (capped
  at 2 searches / 1 record per task); the action parser also learned to take
  the first balanced JSON object (LLMs emit record+finish in one reply)

### Fixed
- Test suite resynced with the v0.3 extractor and write-time gatekeeping
  (6 stale failures → 322/322 green): `pipeline_e2e` updated for the two-tier
  filter (routine successful builds are no longer extracted; fixture now
  yields one text-strategy bridge and the failure→success `refines` pattern
  is asserted gone with rationale), memora harness tests assert the
  session_logs-only raw path (no chunks/edges; persona scoping via task_tag),
  and `mcp_stdio_end_to_end` expects the 14-tool set (`remember` added)
- `cargo clippy --workspace -- -D warnings` clean again: 38+ lints fixed
  (blank lines after doc comments in store/retrieve, dead v0.3 leftovers
  `SUCCESS_MARKERS`/turn-edge constants, unused imports/variables, guarded
  unwraps rewritten, `HashMap` entry API, `&Path` over `&PathBuf`)
- `backfill` (no local-embed) no longer panics before printing its
  configuration help when embedding env vars are unset
- `embed.rs` / `llm.rs` HTTP clients now have an explicit 8s timeout —
  previously an unreachable endpoint hung the synchronous `record_decision`
  tool path until the 60s MCP tool timeout, even though the edge was already
  written (failure still falls back silently, per the zero-intrusion contract).
  The timeout is overridable via `CAUSAL_MEMORY_HTTP_TIMEOUT_SECS` for
  long-context callers (benches)

### Documentation
- `docs/` reorganized into `design/`, `benchmarks/`, `evaluations/`, `paper/`
  (plus existing `research/`) with a new `docs/README.md` documentation map;
  all cross-references updated
- README: stale facts corrected — 14 MCP tools (adds `remember` and
  `reconstruct_lesson`, removes the long-gone `trace_cause_cross_session`),
  322 workspace tests, HTTP transport documented as shipped (was listed as
  missing)
- PROFILE.md: license corrected to Apache-2.0 (said MIT); benchmark and
  test/tool counts synced with README and `docs/benchmarks/`
- `.gitignore`: regenerable `graph-*.html` visualization dumps and local
  `*.cordis.yml` agent configs (may contain credentials) plus bench `*.log`
  artifacts are now excluded from accidental commits

## [0.8.0] - 2026-07-27

### Added
- Schema v4: `outcome_polarity TEXT` column on `causal_edges`
  (`positive` / `negative` / `mixed` / `neutral`, NULL for legacy rows)
- Write-time outcome polarity (`llm::judge_polarity`): when an LLM is
  configured, `record_decision` judges the outcome's polarity **as the direct
  result of the decision** and persists it; unconfigured or any failure falls
  back to the signal-word heuristic. The new `mixed` category covers compound
  outcomes ("deadlock under load; fixed by switching to channels") that the
  heuristic used to force into success
- `intervention_query` chain labels now read the stored polarity first:
  `mixed` gets its own `⚠️ WARNING (mixed outcome)` label instead of a
  misleading `✅ SAFE`; NULL polarity keeps the exact pre-v4 heuristic behavior
  (label logic extracted into the pure `chain_label` function)
- Contradiction short-circuit (exact-match and semantic paths) prefers stored
  polarity over the text heuristic, with a conservative rule: only
  negative-old + positive-new auto-invalidates; `mixed`/`neutral` never
  trigger on either side
- `causal-memory polarity [--db <PATH>] [--limit N]` backfill subcommand
  (LLM judge when configured, heuristic otherwise; idempotent)
- `intervention_query` semantic seeding: embeds the action and walks forward
  chains from cosine-similar past decisions (`similar_decision_edges` +
  `trace_effect_chain_from_ids`), falling back to the LIKE path with
  `[semantic]` / `[keyword]` output markers
- Semantic contradiction candidates on `record_decision`: paraphrased
  duplicates of a decision (cosine ≥ 0.85) with contradicting outcomes are
  soft-invalidated alongside the exact-text path

## [0.7.0] - 2026-07-27

### Added
- Schema migration mechanism (`src/migrate.rs`): `PRAGMA user_version`-driven,
  idempotent, transactional migrations with a `table_info` fallback probe for
  pre-marker v0.6 DBs; `causal-memory migrate` subcommand
- Schema v3: `access_count` / `last_accessed_at` access tracking on
  `causal_edges`, `edge_embeddings` table, meta-edge indexes
- `invalidate_decision` MCP tool + automatic contradiction short-circuit:
  recording a new outcome that contradicts an existing edge for the same
  decision soft-invalidates the old edge (`valid_to` set, kept for audit)
- Dual-system memory (`src/patterns.rs`): offline pattern miner distils
  `similar_to` / `repeated` / `contradicts` / `refines` meta edges into
  `meta_causal_edges` (Jaccard over tokenized decision text + outcome
  polarity); `search_patterns` MCP tool
- Sleep consolidation (`src/consolidate.rs`), a four-phase offline cycle —
  reactivation scoring, generalization (dedup + pattern mining), synaptic
  downscaling (age decay, access boost, GC; `user_feedback` edges immune),
  REM cross-domain integration; `causal-memory sleep [--dry-run]` subcommand
- `causal_directory` MCP tool — L0 compact pointer list of recent decisions,
  meant to be pinned in the agent's system prompt
- `intervention_query` MCP tool — Pearl Rung-2: query what outcomes similar
  past actions caused before acting; returns predicted effects with causal
  paths labeled safe / warning / danger
- Semantic retrieval (`src/embed.rs`): optional OpenAI-compatible embeddings
  (`CAUSAL_MEMORY_EMBED_*`, falls back to `CAUSAL_MEMORY_LLM_*`) rank
  `search_causal` by cosine similarity, with automatic keyword fallback when
  unconfigured; `causal-memory embed` backfill subcommand
- MCP tool surface grows 4 → 8: `record_decision`, `search_causal`,
  `trace_cause`, `trace_cause_chain`, `invalidate_decision`,
  `search_patterns`, `causal_directory`, `intervention_query`
- 3 e2e test suites: migration (`migration_e2e.rs`), extraction→link→trace→
  sleep→mine→invalidate pipeline (`pipeline_e2e.rs`), MCP stdio round-trip
  (`mcp_e2e.rs`); 63 tests total (60 unit + 3 e2e)

### Fixed
- `migrate`: v0/v1 DBs carrying a legacy `created_at` column now backfill
  `event_time` / `discovered_at` from it and drop the column cleanly
- `migrate`: pre-v0.6 DBs with a bare `meta_causal_edges` table (no
  `discovered_at` / `valid_from` / `valid_to`) are now patched
  column-by-column — previously the v3 meta indexes failed to build on them
- Outcome polarity word boundaries: English success signals are matched on
  word boundaries (patterns.rs tokenize style), so "unresolved" no longer
  hits "resolved"; when failure and success signals co-occur, success wins
  ("deadlock resolved" is a success — the failure word names the fixed
  problem)

## [0.6.0] - 2026-07-27

### Added
- Proper temporal schema (commit `0e3ad67`): `causal_edges` gains
  `event_time` (when the decision/outcome happened), `discovered_at` (when
  the edge was written), and `valid_to` (NULL = still valid);
  `meta_causal_edges` gains `discovered_at` / `valid_from` / `valid_to`
- All search/trace queries filter `valid_to IS NULL`; `trace_cause_chain`
  CTE walks only currently-valid edges
- `record_decision_at` accepts an explicit `event_time`; chain linker orders
  by `event_time`

## [0.5.0] - 2026-07-27

### Added
- Chain linker (`src/chain_linker.rs`, commit `e396e19`): post-processing
  pass that bridges flat edges into multi-hop chains (temporal+failure,
  text-overlap, temporal-adjacent strategies); `causal-memory link`
  subcommand

### Fixed
- Extractor now reads real timestamps from `events.jsonl` (was: `now()` for
  every edge, which made the chain linker's temporal ordering never match)
- `trace_cause_chain` CTE ordering: `WHERE depth >= 2 ORDER BY depth DESC`
  so deepest multi-hop chains surface first (was: 1-hop chains dominated)

## [0.4.1] - 2026-07-26

### Added
- `benches/bench_retrieval.rs` — targeted retrieval benchmark comparing
  keyword LIKE, task-tag filter, and causal (task+keyword) strategies
- Retrieval benchmark honest finding: at k=0 compactions, causal retrieval
  does NOT outperform simple keyword search (expected — value is in
  anti-compaction survival, not fresh retrieval)

## [0.4.0] - 2026-07-26

### Added
- `src/reasoning_extractor.rs` — extracts high-value decisions from
  `assistant.content` text using LLM, addressing v0.3's finding that
  tool_call events are mostly trivial
- `reasoning` subcommand: `causal-memory reasoning <session-dir> [max]`
- Real validation: 15 assistant messages → 18 decisions extracted,
  17 edges inserted (vs v0.3 tool_call extraction's 10% confidence)

## [0.3.1] - 2026-07-26

### Added
- `trace_cause_chain` — multi-hop causal trace via SQLite recursive CTE
- SQL parameterization across `search_causal` and `trace_cause` (injection-safe)
- `docs/research-backdrop.md` — paper-to-design-decision map
- `ChainHop` struct for multi-hop chain results

### Changed
- `README.md` updated for 4 MCP tools and v0.3.x feature set

## [0.3.0] - 2026-07-26

### Added
- `src/llm.rs` — LLM judge integration (OpenAI-compatible API)
- `judge` subcommand: extract + re-judge top decisions via LLM
- Env-config: `CAUSAL_MEMORY_LLM_API` / `CAUSAL_MEMORY_LLM_KEY` / `CAUSAL_MEMORY_LLM_MODEL`
- Zero-invasive default (falls back to rule-based when no API configured)

## [0.2.1] - 2026-07-26

### Fixed
- Outcome-overwrite bug: each tool_call now consumes its own tool_completed
  event by ordered name matching (was: HashMap by tool_name → last outcome)
- 7 real errors in test session now correctly captured (were lost in v0.2.0)

### Changed
- Confidence grading: 0.3-0.8 based on content-relation analysis
- Source distribution now includes `llm_inferred` tier (was binary temporal/rule)

## [0.2.0] - 2026-07-26

### Added
- `src/extractor.rs` — decision auto-extractor from grok-build session logs
- `extract` subcommand: parse chat_history.jsonl + events.jsonl
- Real validation: extracted 204 decisions from a real session
- `DECISION_WORTHY_TOOLS` filter to skip low-value read-only operations

## [0.1.0] - 2026-07-26

### Added
- Initial release
- 3 MCP tools: `record_decision`, `search_causal`, `trace_cause`
- SQLite-backed causal store with `causal_edges` and `meta_causal_edges` tables
- `CLAUDE.md` integration template
- 6 unit tests
