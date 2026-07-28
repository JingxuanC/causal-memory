# τ²-bench (airline) — causal-memory behavioral A/B

The first behavioral test of causal-memory on a third-party agent benchmark:
does attaching causal memory change what an agent *does*, not just what it
*retains*?

**Headline (paired tasks, n=39): control 89.7% → memory 97.4% pass rate.
Memory flipped 4 tasks from fail to pass vs 1 flipped back (+3 net), at the
cost of +2 MCP calls per task (+23% LLM cost). Directionally positive;
not yet statistically significant (binomial p ≈ 0.19) — needs a second
domain and more repetitions.**

## Setup

| Parameter | Value |
|---|---|
| Benchmark | [τ²-bench](https://github.com/sierra-research/tau2-bench) airline domain, tasks 0–49 |
| Agent | stock τ² `LLMAgent`, `deepseek-chat`, temperature 0, seed 300 |
| User simulator | `deepseek-chat` |
| Evaluation | τ² official evaluator (deterministic final-DB comparison) |
| Memory server | `causal-memory` (MCP stdio, post-v0.9.0 build) |
| Memory protocol | before each task: `search_causal(domain + scenario)` → lessons injected into system prompt; after each task: `record_decision(tool-sequence summary, pass/fail + reason, task_tag=domain)` |
| Control | identical setup minus the two MCP calls |
| Experiment code | `agent-memory-eval/` (out-of-repo): `memory_agent.py`, `run_experiment.py`, frozen per-task logs |

Both conditions ran all 50 tasks sequentially (memory DB accumulates in
task order). No shared mutable state between streams.

## Results

| | Control | Memory |
|---|---|---|
| Passed (raw) | 38/50 | 39/50 |
| Evaluator crashes (τ² `ActionCheck` bug, excluded) | 7 | 10 |
| **Passed (paired, both evaluated, n=39)** | **35/39 = 89.7%** | **38/39 = 97.4%** |
| Memory flips (fail → pass) | — | tasks 1, 2, 18, 34 |
| Control flips (pass → fail) | — | task 7 |
| LLM calls / task | 16.4 | 15.7 (+2 MCP calls) |
| Total cost | $0.066 | $0.081 |

Protocol compliance was 100%: every memory-condition task issued exactly
one `search_causal` before and one `record_decision` after. Retrieval was
live: e.g. task 1's system prompt contained task 0's recorded strategy
(`get_user_details → get_reservation_details → …`, confidence 95%).

## Caveats (honest)

1. **Small n.** 4-vs-1 flips in 39 paired tasks is directionally positive
   but not significant. Plan: retail domain + 3 repetitions with different
   seeds for variance estimates.
2. **Lesson quality is crude.** `record_decision` stores the tool-call
   sequence, not a semantic lesson ("always verify the user before
   modifying a reservation"). Retrieval still matched on scenario keywords
   — a proper LLM lesson-summarizer should sharpen both storage and recall.
3. **Evaluator instability.** τ²'s `ActionCheck` crashed on 7–10/50 tasks
   (version bug, not agent failure). Paired comparison excludes them, but
   the asymmetry (7 vs 10) adds noise.
4. deepseek-chat's airline baseline (~90%) is much stronger than the
   gpt-4o numbers in the τ² paper (~50%) — headroom for memory gains is
   compressed; the 4 flipped tasks are the interesting tail.

## What this establishes (and what it doesn't)

- ✅ Agents will use causal memory when it is wired in (100% protocol
  compliance, retrieval visibly consumed in system prompts).
- ✅ First directional evidence of behavioral gain on a standard benchmark.
- ❌ Not yet proof of effect size — needs replication (retail domain,
  multiple seeds) and semantic lesson extraction before claiming a number.
