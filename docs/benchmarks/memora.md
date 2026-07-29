# Memora Benchmark — causal-memory

Evaluation on [Memora](https://arxiv.org/abs/2604.20006) (ACL 2026) — the
first benchmark with an explicit **forgetting** dimension: FAMA (Forgetting-
Aware Memory Accuracy) rewards both recalling what should be remembered and
*not* recalling what was deleted or superseded.

**Headline (weekly, 10 personas, 150 questions, 735 probes): FAMA 27.2 ·
MPA (recall) 33.9% · FAA (forgetting) 80.8%.**

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
