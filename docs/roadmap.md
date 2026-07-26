# Roadmap

## v0.1.0 — alpha (current)

- ✅ Three MCP tools (record_decision / search_causal / trace_cause)
- ✅ SQLite persistence with CHECK constraints
- ✅ Confidence levels (temporal / rule / llm_inferred / user_feedback)
- ✅ Task-aware retrieval
- ✅ CLAUDE.md integration template

## v0.2.0 — decision auto-extractor

**Goal**: agent no longer needs to manually call `record_decision`. Background process extracts decisions from wire logs / event streams / chat history.

- [ ] Rule-based decision extractor (wire log Op types → decision events)
- [ ] Confidence booster (background job: temporal → rule / llm_inferred)
- [ ] End-to-end test with a real agent session

## v0.3.0 — Python bindings + broader reach

- [ ] PyO3 bindings (`pip install causal-memory`)
- [ ] TypeScript/Node bindings (for Cursor / Claude Code ecosystem)
- [ ] MCP HTTP transport (beyond stdio, for remote agents)

## v0.4.0 — benchmark & validation

- [ ] LongMemEval integration (compare with Mem0 / Zep / Letta head-to-head)
- [ ] Multi-session causal retention study (does causal table still hold at k=200?)
- [ ] Public benchmark report

## v0.5.0 — cross-agent sharing

- [ ] Causal graph export/import format (JSON)
- [ ] Translation layer for cross-task causal abstraction (insights/11 §8.5)
- [ ] Selective sharing (private vs shareable causal edges)

## v1.0.0 — production ready

- [ ] Multi-tenant support
- [ ] Backup / restore / migration tools
- [ ] Observability (Prometheus metrics, OpenTelemetry traces)
- [ ] Hardened security review
- [ ] Stable API guarantee

## Beyond v1 — research directions

These are open questions, not commitments:

- **Reconstructive retrieval** ([insights/13](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): instead of returning stored text, return fragments and let the agent's LLM reconstruct — natural task-awareness, lower token cost
- **Multi-agent calibration** ([insights/13 §2.5](https://github.com/JingxuanC/agent-teardown/blob/main/insights/13-reconstructive-memory.md)): multiple agents independently reconstruct, discrepancies flag unreliable memories
- **Causal inference beyond Pearl rung 1** ([insights/11 §4](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md)): intervention and counterfactual queries
