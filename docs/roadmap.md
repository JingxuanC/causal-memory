# Roadmap

> Updated 2026-07-28: incorporated learnings from Claude Code teardown (01-04)
> and Vela comparison. Priorities shifted based on real industry analysis.

## v0.9.0 — alpha (current)

- ✅ Ten MCP tools: `record_decision` / `search_causal` / `trace_cause` /
  `trace_cause_chain` / `invalidate_decision` / `search_patterns` /
  `causal_directory` / `intervention_query` / `counterfactual_query` /
  `reconstruct_lesson`
- ✅ SQLite persistence with CHECK constraints + idempotent schema migrations (v5)
- ✅ Temporal schema: `event_time` / `discovered_at` / `valid_to` on all edges
- ✅ Invalidation: manual tool + automatic contradiction short-circuit on `record_decision`
- ✅ Dual-system memory: offline pattern miner distils meta edges
  (`similar_to` / `repeated` / `contradicts` / `refines`)
- ✅ Sleep consolidation: four-phase offline cycle with real replay —
  reactivation priority feeds downscaling (half-rate decay, lenient GC) and
  replayed edges are marked for the next cycle
- ✅ L0 causal directory for system-prompt pinning
- ✅ Pearl Rung-2 intervention queries with task_tag stratified adjustment
  (Simpson's-paradox warning on confounded pooled estimates)
- ✅ Contrastive (empirical) counterfactuals: `counterfactual_query` compares
  recorded outcomes of decision vs alternative (not an SCM counterfactual)
- ✅ Reconstructive retrieval: `reconstruct_lesson` — Markov-blanket subgraph
  + LLM narrative + optional multi-sample calibration
- ✅ Write-time outcome polarity (LLM judge + heuristic fallback, `mixed`
  category), preferred by labels and contradiction checks
- ✅ Stratified causal discovery: miner promotes only patterns replicated in
  ≥ 2 task_tag strata; `confounded` / `simpson` flags on meta edges
- ✅ Semantic retrieval: OpenAI-compatible embeddings + cosine ranking,
  BM25 keyword fallback; `causal-memory embed` backfill
- ✅ BM25 keyword retrieval as the default text-query ranking (replaces LIKE)
- ✅ Cross-agent sharing: `causal-memory export` / `import` (JSONL, idempotent,
  best-effort redaction)
- ✅ Benchmarks: LoCoMo harness (frozen protocol, see docs/benchmarks/locomo.md)
  + reproducible `causal-memory bench-compaction`
- ✅ Multi-hop chain linker + recursive-CTE trace
- ✅ Rule-based decision auto-extractor + LLM judge / reasoning extractor
- ✅ 118 tests (115 unit + 3 e2e suites)

## v0.1.0 — alpha ✅

- ✅ Three MCP tools (record_decision / search_causal / trace_cause)
- ✅ SQLite persistence with CHECK constraints
- ✅ Confidence levels (temporal / rule / llm_inferred / user_feedback)
- ✅ Task-aware retrieval

## v0.2.0 — auto-extractor ✅

- ✅ Rule-based decision extractor (tool_call events)
- ✅ Outcome-overwrite fix (ordered queue)
- ✅ Graded causal confidence (0.3-0.8)

## v0.3.0 — LLM judge ✅

- ✅ DeepSeek/OpenAI-compatible LLM judge
- ✅ Reasoning-level extraction (assistant.content → decisions via LLM)
- ✅ Multi-hop causal trace (recursive CTE + chain_linker)
- ✅ SQL parameterization
- ✅ Temporal schema (event_time + discovered_at + valid_to)

## v0.4.0 — consolidation + anti-degradation (next)

**Goal**: turn causal-memory from "storage" into "living memory" that
improves over time. Inspired by Claude Code Dream + Vela Reflector.

- [ ] **Consolidate command** (Dream-inspired four-phase):
  - Orient: scan existing causal_edges
  - Gather: extract new decisions from recent sessions
  - Consolidate: merge similar edges, prune low-confidence
  - Prune: activate meta_causal_edges (cross-task patterns)
- [ ] **Half-life decay** (Vela-inspired):
  - Add `halflife_hours` column (default 720h = 30 days)
  - effective_confidence = confidence * 0.5^(age_hours / halflife_hours)
  - Four tiers like Vela: 24h (session) / 168h (week) / 720h (month) / 2160h (quarter)
- [ ] **Content-hash dedup** (Vela-inspired):
  - Hash(from_id + to_id + relation) → skip duplicates on insert
  - If duplicate found, update confidence to max(existing, new)
- [ ] **noveltyEntropy trigger** (Vela-inspired):
  - Calculate entropy of recent decision texts
  - Trigger consolidate when entropy > threshold (not fixed interval)
  - More intelligent than Claude Code's "every 24h"

## v0.5.0 — vector retrieval + benchmark

**Goal**: close the gap with Mem0g (which has dual-route retrieval + 93.4 on LongMemEval).

- [x] Add embedding-based vector search (done: embeddings + cosine ranking, BM25 keyword path)
- [x] Run a public benchmark (done: LoCoMo — see docs/benchmarks/locomo.md; LongMemEval itself still open)
- [ ] Add `max_tokens` param to search_causal (respect context budget)
- [ ] `causal-memory stats` command (like Claude Code's /context)

## v0.6.0 — cross-agent sharing + team memory

**Goal**: enable shared causal knowledge (insights/11 §8.5).

- [x] Causal graph export/import (done in v0.9.0: JSONL `export` / `import`, idempotent + redacted)
- [ ] Team memory: shared causal_edges (read-only, like Claude Code's team/)
- [ ] Causal abstraction layer (translate task-specific → cross-task patterns)

## v0.7.0 — conditional triggers + feedback loops

**Goal**: capture Vela-style conditional signal chains.

- [ ] Record conditional triggers: "cron result < threshold → action"
- [ ] `meta_causal_edges` activation: detect recurring decision patterns
- [ ] Feedback loop tracing: "decision A → outcome B → triggered action C"

## v1.0.0 — production ready

- [ ] Python bindings (PyO3)
- [ ] MCP HTTP transport
- [ ] Multi-tenant support
- [ ] Backup / restore / migration
- [ ] Observability (Prometheus, OpenTelemetry)
- [ ] Security hardening
- [ ] Stable API guarantee

## Beyond v1 — research directions

- [x] **Reconstructive retrieval** ([insights/13](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)) —
  **implemented (v0.9.0)** as `reconstruct_lesson`: returns the Markov-blanket
  causal subgraph as compact stubs and lets the LLM reconstruct the lesson
  narrative; degrades to stubs-only without an LLM
- [x] **Multi-agent calibration** ([insights/13 §2.5](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)) —
  **implemented (v0.9.0, engineering form)**: `reconstruct_lesson
  --calibrate=N` generates N independent reconstructions and flags low
  pairwise agreement (token Jaccard) as unreliable memories
- [x] **Causal inference beyond Pearl rung 1** ([insights/11 §4](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)) —
  **implemented**: Rung-2 `intervention_query` (with stratified adjustment)
  and the contrastive/empirical Rung-3 subset (`counterfactual_query`)
- [x] **Cross-agent causal sharing** (insights/11 §8.5) — **implemented
  (v0.9.0)** as `causal-memory export` / `import`: JSONL format with
  versioned header, content-keyed idempotent import, best-effort secret
  redaction; selective sharing via task-tag/confidence/time filters.
  Cross-task abstraction translation remains open
- [ ] **Dream integration**: complement Claude Code's Auto Dream as its causal layer
- [ ] **ScheduleWakeup causal capture**: record "agent decided to check again in 60s" as a causal decision

## Explicitly out of scope

- **Rung 3 SCM counterfactuals** (structural-causal-model reasoning: "what
  *would* have happened, given the mechanism"): per
  [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md),
  this remains practically impossible for agents and we do not build it. The
  project owner has, however, shipped the honest engineering subset:
  `counterfactual_query` is a **contrastive/empirical** counterfactual — it
  compares recorded outcomes of documented alternatives in similar
  situations and says so on every output ("not a Pearl Rung-3 SCM
  counterfactual").
