# Changelog

All notable changes to causal-memory are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
