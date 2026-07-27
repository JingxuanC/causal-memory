# Roadmap

## v0.7.0 — alpha (current)

- ✅ Eight MCP tools: `record_decision` / `search_causal` / `trace_cause` /
  `trace_cause_chain` / `invalidate_decision` / `search_patterns` /
  `causal_directory` / `intervention_query`
- ✅ SQLite persistence with CHECK constraints + idempotent schema migrations (v4)
- ✅ Temporal schema: `event_time` / `discovered_at` / `valid_to` on all edges
- ✅ Invalidation: manual tool + automatic contradiction short-circuit on `record_decision`
- ✅ Dual-system memory: offline pattern miner distils meta edges
  (`similar_to` / `repeated` / `contradicts` / `refines`)
- ✅ Sleep consolidation: four-phase offline cycle (reactivation →
  generalization → downscaling → REM integration), `causal-memory sleep`
- ✅ L0 causal directory for system-prompt pinning
- ✅ Pearl Rung-2 intervention queries (predict outcomes of similar past actions)
- ✅ Semantic retrieval: OpenAI-compatible embeddings + cosine ranking,
  keyword LIKE fallback; `causal-memory embed` backfill
- ✅ Multi-hop chain linker + recursive-CTE trace
- ✅ Rule-based decision auto-extractor + LLM judge / reasoning extractor
- ✅ 73 tests (70 unit + 3 e2e suites)

## v0.8 — candidates

- [x] Conflict detection via LLM judge: replace the signal-word polarity
  heuristic in contradiction detection with the existing LLM-judge path —
  **implemented (v0.8.0)**: write-time `outcome_polarity` column (LLM judge +
  heuristic fallback, new `mixed` category); contradiction short-circuit
  prefers stored polarity; `intervention_query` labels read it
- [ ] Semantic retrieval upgrades: vector index (brute-force cosine does not
  scale), hybrid ranking (keyword + vector + confidence)
- [ ] Meta-edge invalidation tool (meta edges are currently mined but cannot
  be revoked individually)

## v1.0 — engineering hardening

- [ ] Multi-tenant support
- [ ] Backup / restore tooling
- [ ] Observability (Prometheus metrics, OpenTelemetry traces)
- [ ] Stable API guarantee
- [ ] Python/TS bindings + MCP HTTP transport
- [ ] LongMemEval benchmark integration + public report

## v1.1 — research directions

These are open questions, not commitments:

- **Reconstructive retrieval** ([insights/13](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): return a causal subgraph and let the agent's LLM reconstruct a narrative — natural task-awareness, lower token cost
- **Multi-agent calibration** ([insights/13 §2.5](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): multiple agents independently reconstruct, discrepancies flag unreliable memories
- **Cross-agent causal sharing**: export/import format, selective sharing, cross-task abstraction translation (insights/11 §8.5)

## Explicitly out of scope

- **Rung 3 counterfactuals** ("what would have happened if…"): per
  [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md),
  counterfactual reasoning is practically impossible for agents. We only
  prepare the data structures (temporal validity windows) — we do not build it.
