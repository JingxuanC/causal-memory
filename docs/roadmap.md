# Roadmap

## v0.8.0 — alpha (current)

- ✅ Eight MCP tools: `record_decision` / `search_causal` / `trace_cause` /
  `trace_cause_chain` / `invalidate_decision` / `search_patterns` /
  `causal_directory` / `intervention_query`
- ✅ SQLite persistence + idempotent schema migrations (v4)
- ✅ Temporal schema: `event_time` / `discovered_at` / `valid_to`
- ✅ Invalidation: manual tool + contradiction short-circuit with
  write-time outcome polarity (LLM judge + heuristic fallback)
- ✅ Dual-system memory: meta-edge pattern miner
  (`similar_to` / `repeated` / `contradicts` / `refines`)
- ✅ Sleep consolidation: four-phase offline cycle
- ✅ L0 causal directory + Pearl Rung-2 intervention queries
- ✅ Retrieval: BM25 keyword ranking (default) + optional OpenAI-compatible
  embeddings with cosine ranking; `causal-memory embed` backfill
- ✅ Multi-hop chain linker + recursive-CTE trace
- ✅ LoCoMo benchmark harness + frozen-protocol reports
  (overall 65.0%, abstention 84.3–94.4% across runs — see
  [`benchmarks/locomo.md`](benchmarks/locomo.md))
- ✅ 94 tests (unit + e2e: migration / pipeline / MCP stdio)

## v0.9 — candidates (research-driven)

Ordered by leverage, informed by the 2026-07-27 research sweep
([agent-teardown](https://github.com/JingxuanC/agent-teardown):
Mem0 paper analysis, Anthropic Dreaming analysis, Claude Code teardowns):

- [ ] **LongMemEval integration + public report** — moved up from v1.0.
  Mem0's 2026-07 algorithm reached 93.4 (+25.6, multi-hop +23.1); we need
  our number on the board even if it is not flattering. The LoCoMo harness
  (`causal-memory-locomo`) is the template.
- [ ] **Abstention/answer calibration** — LoCoMo runs 3→5 showed retrieval
  quality and abstention are in tension (93.3% → 84.3% as retrieval
  improved; over-correction cost cats 1–4 −7.6pp). Land the balanced
  answerer prompt and consider passing retrieval-score strength into the
  answering context.
- [ ] **LLM update-resolver for invalidation** — Mem0g parity: replace the
  rule-based contradiction short-circuit with an LLM judge that decides
  whether new evidence refutes an existing edge (the polarity plumbing from
  v0.8.0 is already in place).
- [ ] **L0 file injection** — from the Claude Code memory teardown:
  generate a `CAUSAL_MEMORY.md` (< 200 lines, pointer-style decision
  directory) that agents pin into their system prompt — proactive
  injection, not just the on-demand `causal_directory` tool.
- [ ] **Hybrid retrieval ranking** — BM25 + vector + confidence fusion
  (brute-force cosine does not scale; needs a vector index).
- [ ] **Meta-edge invalidation tool** — meta edges can be mined but not
  revoked individually.

## v1.0 — engineering hardening

- [ ] Multi-tenant support
- [ ] Backup / restore tooling
- [ ] Observability (Prometheus metrics, OpenTelemetry traces)
- [ ] Stable API guarantee
- [ ] Python/TS bindings + MCP HTTP transport

## v1.1 — research directions

These are open questions, not commitments:

- **Reconstructive retrieval** ([insights/13](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): return a causal subgraph and let the agent's LLM reconstruct a narrative — natural task-awareness, lower token cost. Academic momentum: E-mem (arXiv:2601.21714), Mnemis dual-system retrieval (arXiv:2602.15313)
- **Multi-agent calibration** ([insights/13 §2.5](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): multiple agents independently reconstruct, discrepancies flag unreliable memories
- **Cross-agent causal sharing**: export/import format, selective sharing, cross-task abstraction translation (insights/11 §8.5). Prior art to study: Claude Code `team/` directory (shared, read-only memory)

## Competitive position (2026-07-27 research sync)

- **Mem0g / Zep / Mnemis** store entity-relation graphs ("user lives in
  Berlin"); we store causal attribution ("choosing mutex caused a
  deadlock"). Mem0's own paper shows graph structure alone is worth only
  ~2% — the edge *semantics* are the differentiator, not the graph.
- **Anthropic Dreaming** consolidates *text* memory (Markdown files,
  four-phase Orient→Gather→Consolidate→Prune); it does not store causal
  edges or trace multi-hop causes. Position: complementary — Dreaming
  tidies text memory, causal-memory stores and traces causal memory.
- **Engram** (arXiv:2606.09900) independently converged on bi-temporal
  validity (`valid_from`/`valid_to`) — temporal windows are now industry
  consensus, not a differentiator.

## Explicitly out of scope

- **Rung 3 counterfactuals** ("what would have happened if…"): per
  [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md),
  counterfactual reasoning is practically impossible for agents. We only
  prepare the data structures (temporal validity windows) — we do not build it.
  *Watch: Executable Counterfactuals (arXiv:2510.01539) challenges this
  assumption; revisit if the technique matures.*
