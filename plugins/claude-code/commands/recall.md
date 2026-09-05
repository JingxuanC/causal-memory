---
allowed-tools: mcp__causal-memory__search_memory, mcp__causal-memory__search_causal, mcp__causal-memory__search_facts, mcp__causal-memory__intervention_query, mcp__causal-memory__counterfactual_query
description: Recall past experience relevant to a task or decision before acting on it
argument-hint: <what you are about to do or decide>
disable-model-invocation: false
---

Before acting on "$ARGUMENTS", recall everything relevant from causal
memory:

1. Call `search_memory` with "$ARGUMENTS" (fused facts + causal lessons).
2. If any hit's task_tag looks like the current domain, call
   `search_causal` restricted to that tag for depth.
3. Judgment call, two concrete options in play? Call
   `counterfactual_query` with BOTH option texts — same-context branches
   (natural experiments) beat pooled statistics when they exist.
4. Risky or irreversible action? Call `intervention_query` on it and heed
   the safe/warning/danger label.

Then answer, in this order:
- **Relevant experience** (max 5 bullets: decision → outcome, with
  task_tag and confidence)
- **What it implies** for "$ARGUMENTS" (one short paragraph)
- If you are about to record anything new afterwards, remember to pass
  `context` on `record_decision` — especially when options were weighed.

If memory holds nothing relevant, say so plainly and proceed; absence of
evidence is not evidence of safety.
