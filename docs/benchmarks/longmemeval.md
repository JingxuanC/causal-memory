# LongMemEval Benchmark — causal-memory

Evaluation on [LongMemEval](https://github.com/xiaowu0162/LongMemEval)
(`longmemeval_s_cleaned`, 500 questions, ~115k-token chat histories per question).

**Headline: overall 61.8% (309/500) · abstention 96.7% (29/30) · evidence hit rate 84.4% · zero judge errors.**

## Frozen protocol

| Parameter | Value |
|---|---|
| Dataset | `longmemeval_s_cleaned.json` (500 questions; mirrored copy in `benches/longmemeval/data/`) |
| Git commit | `0f6e5bd` |
| Answerer model | `deepseek-chat`, temperature 0.0 |
| Judge | `deepseek-chat`, temperature 0.0 — prompt templates ported 1:1 from official `evaluate_qa.py` (yes/no verdict, temporal off-by-one tolerance, knowledge-update latest-wins, preference rubric, abstention) |
| Retrieval | BM25 top-10 (`search_causal_bm25`), per-question hard isolation (`task_tag` = question_id) |
| Ingest | one chunk per haystack turn (`[session date] role: content`), adjacent-turn `caused` edges |
| Run | 2026-07-27, single run, all 500 questions, 0 errors |

Reproduce:

```bash
export DEEPSEEK_API_KEY=...
./target/release/causal-memory-longmemeval run \
    --data benches/longmemeval/data/longmemeval_s_cleaned.json --concurrency 8
```

## Results (run 20260727_175219)

| Question type | n | Accuracy |
|---|---|---|
| single-session-assistant | 56 | **96.4%** |
| single-session-user | 70 | **90.0%** |
| knowledge-update | 78 | 76.9% |
| temporal-reasoning | 133 | 61.7% |
| multi-session | 133 | 32.3% |
| single-session-preference | 30 | 23.3% |
| **overall** | 500 | **61.8%** |
| abstention (`_abs`) | 30 | **96.7%** |

Context (published): Mem0 2026-07 algorithm 93.4 · Zep 63.8 (2025 data,
different answerer models — not strictly comparable).

## Analysis

- **Abstention 96.7%** — the standout result, consistent with the LoCoMo
  adversarial finding (91.5%). Refusing to answer when memory has no answer
  is this system's most reliable behavior.
- **knowledge-update 76.9%** — the category LongMemEval was built for
  (facts that change over time). The session-date stamps + latest-wins
  prompt rule do the work; the v0.6 temporal schema is what makes this
  natural.
- **multi-session 32.3%** — the main weakness. These need evidence from
  several sessions synthesized; BM25 top-10 over ~1000 chunks/question
  retrieves fragments but not the full evidence set. Fix path: query
  decomposition / iterative retrieval / higher top-k for this type.
- **single-session-preference 23.3%** — rubric-judged preference questions;
  short factual answers score poorly against qualitative rubrics. Likely
  an answer-style mismatch more than a memory failure (needs per-type
  answer prompting).
- evidence hit rate 84.4% — retrieval finds the gold session-turn most of
  the time; the remaining gap is synthesis, not search.

## Honest positioning

61.8% overall is far from Mem0's 93.4. But note the composition: Mem0's
gains come from temporal/multi-hop question types where a dedicated
memory-update pipeline helps; our wins are abstention and single-session
recall. causal-memory is a causal layer, and LongMemEval measures general
factual memory — we publish this number so the gap is explicit and
measurable, the same discipline as the LoCoMo baselines.
