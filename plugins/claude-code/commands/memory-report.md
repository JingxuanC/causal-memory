---
allowed-tools: mcp__causal-memory__prediction_report, mcp__causal-memory__causal_directory, mcp__causal-memory__search_causal, mcp__causal-memory__search_memory, mcp__causal-memory__search_facts
description: Calibration + inventory check of your causal memory (prediction ledger accuracy, what you know)
disable-model-invocation: false
---

Report the health of this workspace's causal memory. Do exactly this:

1. Call `prediction_report` and show the verdict — accuracy per method and
   per task_tag, pending predictions. If the ledger is empty, say so and
   explain (one sentence) that `counterfactual_query` verdicts become
   falsifiable predictions that auto-resolve when either option is later
   recorded.
2. Call `causal_directory` (limit 10) and summarize what experience exists
   as a compact bullet list (task_tag — the one-line lesson).
3. Flag anything notable: task_tags with dense same-context branches
   (forks make counterfactuals same-world), falsified predictions
   (accuracy < 50% in a stratum means those lessons deserve
   `invalidate_decision`), or stale pending predictions.

Keep the whole report under 200 words. No preamble — start with the
prediction ledger line.
