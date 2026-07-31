# Memora Benchmark — causal-memory

Evaluation on [Memora](https://arxiv.org/abs/2604.20006) (ACL 2026) — the
first benchmark with an explicit **forgetting** dimension: FAMA (Forgetting-
Aware Memory Accuracy) rewards both recalling what should be remembered and
*not* recalling what was deleted or superseded.

**Headline (distill + fact layer, run 20260731_012137): MPA 46.8% (+12.9pp
vs raw) · FAA 72.1% · mean FAMA 31.0 — 10/10 personas, all sessions
distilled, zero fallbacks.**
Raw headline (weekly, 10 personas, 150 questions, 735 probes): FAMA 27.2 ·
MPA (recall) 33.9% · FAA (forgetting) 80.8%.

## Distill + fact-layer run (run 20260731_012137, weekly, 10 personas)

Same dataset, answerer, judge and top-k as the raw baseline; ingest changes
to LLM distillation per session with kind-based routing: `Fact`/`Preference`
items → the `agent_facts` layer (scope `user`, per-persona DB; supersedes
hints retire outdated values via `retire_facts_by_hint`, retire-before-record
order after review caught a self-retire window), `Lesson`/`Event`
items → the causal layer; heavy sessions additionally dual-write raw turns
(quantitative detail the distiller compresses away). At QA time fact lines
are placed FIRST in the prompt. Persona-level idempotency via the
`distill_done` marker table (a persona whose LLM calls ALL failed is left
unmarked and redone next run).

| Persona | FAMA | MPA | FAA |
|---|---|---|---|
| academic_researcher | 28.8 | 43.4% | 75.0% |
| business_executive | 31.2 | 41.5% | 83.3% |
| content_writer | 31.0 | 43.9% | 79.2% |
| creative_designer | 37.2 | 55.3% | 66.7% |
| financial_analyst | 34.3 | 54.4% | 57.9% |
| management_consultant | 22.4 | 45.7% | 64.7% |
| marketing_manager | 28.5 | 41.5% | 85.2% |
| sales_manager | 35.2 | 50.0% | 66.7% |
| software_engineer | 34.8 | 45.5% | 74.1% |
| startup_founder | 26.6 | 46.8% | 68.0% |
| **mean** | **31.0** | **46.8%** | **72.1%** |

Read: MPA jumps 33.9% → 46.8% (+12.9pp) — atomic facts are exactly what
memory-presence probes ask about. FAA dips 80.8% → 72.1%: retired facts
are correctly hidden, but dual-written raw turns still carry the deleted
values verbatim, and the forgetting judge counts any mention as a failure.
That trade is visible in the per-persona spread (FAA 58–85%); tightening
the retraction-record filter to dual-written raw chunks is the known fix.
An earlier run of this same configuration with the pre-fix record order
(record-then-retire) scored MPA 46.0% / FAA 71.4% / FAMA 30.8 — the
self-retire window cost ~1pp here, consistent with its conservative
direction.

## Raw baseline

## Frozen protocol

| Parameter | Value |
|---|---|
| Dataset | `geniesinc/Memora` data (weekly scale; monthly/quarterly harness-verified, not yet run) |
| Git commit | `107bb68` |
| Answerer | `deepseek-chat`, temp 0 (balanced-abstention + temporal normalization + latest-wins prompt) |
| Judge | single-judge `deepseek-chat`, prompts ported 1:1 from official `agent_eval/memory_to_answer.py`. **Protocol note: official Table 3 uses a 3-judge majority vote via OpenRouter — our numbers are not directly comparable to the paper's** |
| Retrieval | BM25 top-10 over per-persona DB (`search_causal_bm25`) |
| Ingest | one chunk per turn (`[date] speaker: message`), adjacent-turn `caused` edges, `event_time` = session date |

Reproduce:

```bash
export DEEPSEEK_API_KEY=...
./target/release/causal-memory-memora run \
    --memora-root /path/to/Memora --scale weekly --concurrency 8
```

## Results (run 20260729_132923, weekly)

| Task type | FAMA | MPA | FAA |
|---|---|---|---|
| Remembering | 21.4 | 27.7% (72/260) | 82.7% (124/150) |
| Recommending | 43.3 | 53.5% (69/129) | 77.4% (82/106) |
| Reasoning | 22.2 | 22.2% (20/90) | — (no forgetting probes) |
| **overall** | **27.2** | **33.9%** | **80.8%** |

Paper Table 3 context (weekly Remembering, different judge protocol):
A-Mem 71.82 · LangMem 71.16 · Nemori 65.06 · MemoryOS 51.84 · MemoBase
43.60 · Mem-0 40.42. Our Remembering FAMA is 21.4 — below the field.

## Analysis

- **FAA 80.8% is the structural strength.** Memora is the only benchmark
  that scores forgetting, and the temporal schema + latest-wins contract
  deliver it without any curated-update pipeline. This is the same asset
  as LongMemEval's 96.7% abstention — the system is *trustworthy about
  what not to say*.
- **MPA 33.9% is the bottleneck — for the third time.** LoCoMo (keyword
  ceiling → BM25 fix), LongMemEval (multi-session synthesis), and now
  Memora (recall across 150 sessions/persona) all show the same gap:
  **flat raw-turn ingest + BM25 loses to LLM-curated memory pipelines**
  (A-Mem/LangMem distil structured notes per session; we store raw turns).
  The fix is the same single piece of work in all three: an LLM
  distillation step at ingest (facts/lessons extracted per session, then
  indexed), not more retrieval tuning.
- Reasoning FAMA is low for the whole field in the paper too (best 30.0)
  — multi-session synthesis remains unsolved everywhere.

## Protocol caveats

- Single-judge deepseek-chat vs the paper's 3-judge vote: absolute numbers
  carry judge bias; treat cross-system comparisons as indicative.
- Monthly (600 sessions/persona) and quarterly (2000) are harness-verified
  but not yet run (ingest volume and cost; quarterly ≈ 4,665 LLM calls).
  The interesting hypothesis for quarterly: FAA should *hold* while
  flat-text systems degrade with history length — the compaction-survival
  claim at calendar scale.
