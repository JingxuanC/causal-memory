# Changelog

All notable changes to causal-memory are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
