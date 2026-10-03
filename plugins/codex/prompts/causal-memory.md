Recall past experience relevant to "$ARGUMENTS" from causal memory, then
advise.

Steps:

1. Call `search_memory` with "$ARGUMENTS" (fused facts + causal lessons).
2. If a hit's task_tag matches the current domain, call `search_causal`
   restricted to it for depth.
3. Two concrete options on the table? Call `counterfactual_query` with
   BOTH option texts — same-context branches (natural experiments) outrank
   pooled statistics when they exist. Risky/irreversible action? Call
   `intervention_query` and heed the safe/warning/danger label.

Answer with:
- **Relevant experience** (≤5 bullets: decision → outcome, task_tag,
  confidence)
- **Implication** for "$ARGUMENTS" (short paragraph)

If nothing relevant is stored, say so plainly and proceed — absence of
evidence is not evidence of safety. After acting and observing the result,
record the lesson with `record_decision`, passing `context` (environment,
constraints, key parameters) — ALWAYS when multiple options were weighed:
same task_tag + context becomes a comparable branch for future
counterfactuals.
