# Roadmap

> Updated 2026-07-28: incorporated learnings from Claude Code teardown (01-04)
> and Vela comparison. Priorities shifted based on real industry analysis.

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

- [ ] Add embedding-based vector search (semantic triplet path, like Mem0g)
- [ ] Run LongMemEval benchmark (need a real number to compare)
- [ ] Add `max_tokens` param to search_causal (respect context budget)
- [ ] `causal-memory stats` command (like Claude Code's /context)

## v0.6.0 — cross-agent sharing + team memory

**Goal**: enable shared causal knowledge (insights/11 §8.5).

- [ ] Causal graph export/import (JSON format)
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

These are open questions, not commitments:

- **Reconstructive retrieval** ([insights/13](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): return fragments, let LLM reconstruct
- **Multi-agent calibration** ([insights/13 §2.5](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): vote on causal accuracy
- **Causal inference beyond Pearl rung 1** ([insights/11 §4](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)): intervention/counterfactual queries
- **Dream integration**: complement Claude Code's Auto Dream as its causal layer
- **ScheduleWakeup causal capture**: record "agent decided to check again in 60s" as a causal decision
