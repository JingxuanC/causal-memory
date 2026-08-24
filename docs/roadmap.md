# Roadmap

> Updated 2026-08-24: **code-audited sync** — checked every open item
> against the source instead of the docs. Shipped but unticked: half-life
> decay tiers, per-type answer prompting, multi-session retrieval,
> Memora (weekly), layered loading + token budget (incl. the hippocampus
> path fix). New since last update: bounded-forgetting GC budget,
> `disable_spread` ablation switch + first formal ablation run.
>
> Updated 2026-07-30: **positioning shift — from causal layer to complete memory
> system**. The causal layer was the beachhead, not the boundary. causal-memory
> is growing into a complete agent memory system: fact/preference memory and
> temporal state on the same causal-graph skeleton, sharing one
> hippocampus-style engine (typed spreading activation + SWR consolidation).
>
> Triggers for the shift: OpenViking's 80–83% LoCoMo (the factual-recall
> ceiling is an engineering problem, not a law of nature), HeLa-Mem (ACL 2026 —
> Hebbian spreading activation, our closest academic competitor; it builds the
> excitatory side, we own the inhibitory side via `prevented` negative spread),
> and Anthropic's Dreams API (the industrial consolidation pattern: produce a
> new store, never mutate the original).
>
> Previous update 2026-07-28: synced after the v0.9.0 merge — remote
> research-driven features (stratified discovery, counterfactual_query,
> reconstruct_lesson) plus local benchmark suite (LoCoMo runs 1–5, LongMemEval,
> compaction survival) and the dogfooding miner fix. v0.4–v0.7 below are
> *shipped history*, not future plans.

## Direction: complete memory system (planned phases)

Full design: [docs/design/complete-memory-system.md](design/complete-memory-system.md)
(**one graph, one engine, one loop** — all memory types as typed edges on a
single graph; typed spreading activation; immutable consolidation loop) and
[docs/design/unified-memory-design.md](design/unified-memory-design.md)
(fact layer schema + 3 new MCP tools + LLM distill ingest + 4.5-day plan;
§5.1 reconciles this direction with the OpenViking "stay a causal layer"
argument — lightweight self-built fact layer, pluggable storage substrate).

- [x] **Fact/preference layer** — ✅ shipped 2026-07-31 (schema v6):
  `agent_facts` with scope + validity intervals, idempotent upsert,
  same-key retirement (`replace_same_key`), BM25 + optional embedding
  retrieval, `record_fact` / `search_facts` MCP tools. Phase 2
  (`search_memory` RRF fusion) and Phase 3 (`causal-memory distill` LLM
  ingest) shipped the same day. Remaining: LoCoMo rerun targeting 75–80%
- [x] **Hebbian co-occurrence edges** — ✅ shipped 2026-08-18 (schema v11):
  retrieval co-activated chunks build weak associative edges in
  `cooccurrence_edges`, reinforced per co-activation, loaded into the graph
  as CoOccurrence edges on rebuild (the excitatory complement to
  `prevented` negative spread; absorbs HeLa-Mem's core mechanism as a
  subset)
- [x] **SWR 2.0 / Dreams alignment** — ✅ shipped 2026-08-18:
  `sleep --immutable` consolidates into a *new* store (VACUUM INTO a
  timestamped copy, original untouched), `sleep --restore` swaps it back in
  with a backup; `instructions`-style focus parameter still a candidate
  for the narrative layer
- [~] **Query routing + fusion retrieval** — ✅ RRF fusion shipped
  (`search_memory`, 2026-07-31); iterative retrieval with entity/time
  anchors for multi-session questions also shipped (60.2%, see below).
  Remaining: query-type classifier for single-layer routing
- [x] **Q-value dynamics** — ✅ shipped (consolidate Stage 1.5 Bellman
  reinforcement → `chunks.q_value` persistence → hippocampus seeding
  `0.5 + 0.5·Q`). Implementation note: the learned utility weights *node
  activation seeding*, not the stored edge confidence — roadmap's original
  "replaces static confidence as the primary edge weight" was revised to
  node-utility seeding (edge-weight variant deferred until a CausalEval
  A/B can verify no retrieval regression)

Mechanism absorption (from the 2026-07-30 deep dives, deduplicated):

- [ ] **Triple-criterion GC** — prune only when structurally weak AND dormant
  AND zero recent access (HeLa-Mem adaptive forgetting; avoids deleting old
  but still-active edges)
- [ ] **Flip-path marking** — tag results as direct-seed vs spreading-surfaced
  so upper layers can do Top-k ∪ Top-m unions (HeLa-Mem dual-path retrieval)
- [x] **Layered loading + token budget** — ✅ shipped: L0/L1/L2
  (`detail_level`) + strict `max_tokens` on `search_causal`, incl. the
  hippocampus spreading path (threaded 2026-08-24 — the params were
  silently dead on that path before) and on `search_memory` (facts +
  causal sections share one budget; default output byte-identical)
- [~] **Formal ablation** — harness + engine switches shipped
  2026-08-24 (`disable_inhibition` / `disable_spread`,
  `benches/ablation`). First run (n=100, LongMemEval distill store):
  no-spread −1pt evidence hit / −186 pool tokens; no-inhibition and
  no-swr vacuous on that store (1 `prevented` edge; never consolidated,
  all q_value=0.5) — rerun on a consolidated store still owed
- [~] **Token-efficiency benchmark** — per-question token accounting
  shipped in the LongMemEval harness (avg ctx/ans tokens in every run
  summary); the dedicated cross-system comparison vs OpenViking's
  34–91% savings claim is still open

## Current state — v0.9.0+ (main)

**Sixteen MCP tools**: `record_decision` / `search_causal` / `record_fact` /
`search_facts` / `search_memory` / `trace_cause` / `trace_cause_chain` /
`invalidate_decision` / `invalidate_pattern` / `resolve_updates` /
`search_patterns` / `causal_directory` / `intervention_query` /
`counterfactual_query` / `reconstruct_lesson` / `remember` — over stdio
**and** HTTP transport (`causal-memory-http --port 9938`).

Core capabilities (all shipped, all tested):

- **Fact layer** (2026-07-31, unified-memory-design Phase 1): `agent_facts`
  table + embeddings (schema v6), idempotent upsert on (key, value, scope),
  soft invalidation + same-key retirement (`replace_same_key`), BM25 +
  optional semantic retrieval, revive-on-re-record
- **Unified retrieval** (2026-07-31, Phase 2): `search_memory` RRF-fuses
  facts + causal layers into one ranked list
- **LLM distill ingest** (2026-07-31, Phase 3): `causal-memory distill`
  routes distilled facts/preferences → `agent_facts` (supersedes retirement)
  and lessons/events → the causal store
- Temporal schema (v6) + idempotent migrations; `valid_to` invalidation
  (manual + contradiction short-circuit with write-time polarity)
- Dual-system memory: meta-edge pattern miner with **stratified
  replication** (patterns must hold in ≥ 2 task_tag strata; `confounded` /
  `simpson` flags) **and** explosion guards (dedup groups, boilerplate
  stripping, threshold 0.65, top-5 / max-1000 caps) — the two halves of
  the v0.9.0 merge
- Sleep consolidation: four-phase cycle; reactivation scores feed
  downscaling (half-rate decay for replay-protected edges), replay marks
  carry into the next cycle
- Forgetting: bounded GC budget (`max(floor=50, 20% of population)`,
  weakest-first, edges and facts independently), half-life decay tiers,
  diversity-gated cycles (`sleep --auto`)
- Pearl ladder: Rung-2 `intervention_query` (stratified, Simpson warning)
  + contrastive empirical `counterfactual_query` (honestly labeled: not SCM)
- Reconstructive retrieval: `reconstruct_lesson` (Markov-blanket subgraph
  → LLM narrative, `--calibrate=N` multi-reconstruction agreement)
- Retrieval: BM25 default + optional embeddings with cosine ranking;
  entity-token cache kills the per-query re-tokenization cost (audit 2026-08
  #2)
- Cross-agent sharing: `causal-memory export` / `import` (JSONL, idempotent,
  best-effort redaction)
- 368 tests (unit + e2e: migration / pipeline / MCP stdio)

## Benchmarks (frozen protocols, all published in docs/benchmarks/)

| Benchmark | Result | Note |
|---|---|---|
| LoCoMo (1,986 q) | overall 64.2% (adopted prompt) · abstention 91.5% | 5 controlled runs; raw-QA best 65.0% |
| LongMemEval (500 q, 2026-08-22 full pipeline) | **overall 76.4% · multi-session 60.2% · temporal 69.9% · abstention 96.7% @ 11.5K tok/q** | vs 72.4% (8/20) at -32% token cost; mem0 official 94.4% @ 6.8K, ind. repro 73.8% — caliber gap, see docs/benchmarks/longmemeval.md |
| **Compaction survival (k=5)** | text-only 44.5% vs text+causal **65.3%** | causal edges fully offset 5 compactions (+20.8pp) |
| **Agent ablation (trap world)** | repeat-mistake 67% (no memory) → **33%** (with memory) | glm-4-plus, seed 42, both 6/6 solved; post-search hit 57% |

Dogfooding: wired as MCP server into a live agent (kimi CLI), seeded with
948 real edges extracted from the development session of this project;
first real `sleep` run exposed and fixed the meta-edge combinatorial
explosion (17,496 → 119 edges, 26.5s → 0.8s).

## Shipped history (collapsed)

- **v0.1** — 3 MCP tools, SQLite schema, confidence levels
- **v0.2** — rule auto-extractor, outcome-overwrite fix, graded confidence
- **v0.3** — LLM judge, reasoning extractor, multi-hop CTE, SQL parameterization
- **v0.4** — reasoning-level extraction, retrieval bench + honest finding
  (causal ≈ keyword on fresh data; value is compaction survival)
- **v0.5** — chain linker: multi-hop actually works (timestamps, bridge edges)
- **v0.6** — temporal schema (event_time / discovered_at / valid_to)
- **v0.7** — migrations, invalidation write-path, meta miner, sleep cycle,
  L0 directory, Rung-2, embeddings, e2e suites
- **v0.8** — semantic intervention matching, write-time outcome polarity
- **v0.9** — replay→consolidation loop, stratified discovery, empirical
  counterfactuals, reconstructive retrieval, export/import

## Next (unordered candidates)

Benchmark-driven:

- [~] **Memora benchmark** (arXiv:2604.20006) — weekly scale shipped:
  full FAMA protocol port (`benches/memora`), 17 runs, FAMA 31.0 /
  MPA 46.8% / FAA 72.1% (single-judge, not directly comparable to the
  official 3-judge vote). Remaining: monthly/quarterly scales (harness
  supports them, never run)
- [x] **Multi-session retrieval** — ✅ shipped (LongMemEval multi-session
  32.3% → 60.2%): query decomposition with temporal anchors
  (`parse_temporal_anchor` / `retrieve_multi_pass`), multi-session-only
  hippocampus spreading, P8 session expansion
- [x] **Per-type answer prompting** — ✅ shipped: per-question-type
  answer contracts (knowledge-update / multi-session / preference
  rules); preference 13.3% → 56.7% → 80.0%
- [x] `max_tokens` budget param on `search_causal` — ✅ (see mechanism
  absorption above); [ ] `causal-memory stats` (Claude Code `/context`
  analogue) still open

Memory-quality:

- [x] **Soft supersession** — ✅ shipped 2026-08-17 (`annotate_superseded`):
  superseded edges stay retrievable with `superseded_by` provenance;
  CausalEval C7 50% → 100% with C3 unharmed. The LLM-judge upgrade below
  now builds on this instead of hard invalidation
- [x] **Half-life decay tiers** (Vela-inspired) — ✅ shipped:
  `halflife_hours` per provenance tier (user_feedback 2160h / llm 2160h /
  temporal 168h / fact 2160h), `effective = confidence · 0.5^(age/halflife)`
  for edges and facts; unmapped sources (`distill`, `rule`) intentionally
  keep the legacy flat `decay_per_day`
- [~] **noveltyEntropy trigger** — diversity gate shipped: `sleep --auto`
  computes normalized Shannon entropy over the last 64 chunks and skips
  the cycle below `min_diversity` (0.4). Remaining: auto-invocation
  (today an external caller must still start `sleep`)
- [ ] **LLM update-resolver**: replace rule-based contradiction detection
  with the LLM judge for invalidation decisions (polarity plumbing ready)
- [x] **Meta-edge invalidation tool** — ✅ shipped 2026-08-24
  (`invalidate_pattern`, 16th MCP tool): soft-deletes via the existing
  `meta_causal_edges.valid_to` (readers already filtered it), live graph
  patched immediately like `invalidate_decision`; `search_patterns`
  output now carries `(#<id>)` as the revocation handle
- [ ] Hybrid retrieval ranking (BM25 + vector + confidence fusion)

Ecosystem:

- [ ] **Hermes Agent memory provider** — the first agent runtime with a
  first-class memory plugin slot; no current provider stores causal edges.
  Entry ticket: our benchmark suite (see
  [docs/research/computational-ai/hermes-provider-ecosystem.md](research/computational-ai/hermes-provider-ecosystem.md)).
  Requires PyO3 bindings — reprioritized above HTTP transport
- [ ] **L0 file injection**: generate `CAUSAL_MEMORY.md` (< 200 lines,
  pointer-style) for constant system-prompt pinning — proactive, vs the
  on-demand `causal_directory` tool
- [ ] Team memory: shared read-only causal edges (Claude Code `team/`
  analogue); cross-task abstraction translation (insights/11 §8.5)
- [ ] Dream integration: position as the causal layer under Claude Code's
  Auto Dream (text consolidation ≠ causal storage)

## v1.0 — engineering hardening

- [x] **Python bindings (PyO3)** — ✅ shipped 2026-08-19: orchestration logic
  extracted from the MCP server into a shared library facade
  (`causal_memory::memory::Memory`, 15 ops, MCP behavior preserved 1:1);
  `crates/causal-memory-py` binds it as the `causal_memory` Python module
  (abi3 ≥ 3.9, maturin build, `CausalMemory` class mirroring all 15 tools,
  pytest smoke suite). Remaining for the ecosystem entry: PyPI publishing +
  CI wheels, then the Hermes provider slot
- [ ] TS bindings (after Python)
- [x] MCP HTTP transport — ✅ shipped (`causal-memory http`, Streamable
  HTTP, stateless mode); auth/multi-tenant hardening still open
- [ ] Multi-tenant support
- [ ] Backup / restore tooling (migrations already done)
- [ ] Observability (Prometheus, OpenTelemetry)
- [ ] Stable API guarantee

## Explicitly out of scope

- **Rung 3 SCM counterfactuals** (structural-causal-model reasoning): per
  [insights/11](https://github.com/JingxuanC/agent-teardown/blob/main/insights/11-causal-state-store.md),
  practically impossible for agents. The honest engineering subset shipped:
  `counterfactual_query` is contrastive/empirical over recorded
  alternatives, labeled as such on every output.
  *Watch: Executable Counterfactuals (arXiv:2510.01539) challenges this;
  revisit if the technique matures.*
