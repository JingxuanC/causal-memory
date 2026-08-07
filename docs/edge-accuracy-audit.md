# Causal Edge Labeling Accuracy Audit

> Date: 2026-08-07
> Method: 100 edges randomly sampled from the production DB, re-judged by an
> independent LLM (deepseek-chat, temp=0) against the stored `relation` label.

## Results

| Metric | Value |
|---|---|
| Edges audited | 100 |
| Agreement (stored == rejudged) | **83%** |
| Mismatches | 17 (all `caused` → `enabled`) |
| Prevented/no_effect errors | **0** |

## Interpretation

The 17 mismatches are all in the `caused` vs `enabled` gray zone — cases like
"run_terminal_command(X) → output Y" where the auditor classified the
relationship as `enabled` (the command made the output possible) rather than
`caused` (the command directly produced the output). Both labels are defensible.

**No severe misclassifications**: zero edges were labeled `caused` when they
should have been `prevented` or `no_effect`. The causal polarity is reliable.

## Implication for the paper

Edge labeling accuracy ≥ 83% is sufficient for the causal graph to be a
trustworthy substrate. The error is conservative (over-labeling as `caused`
rather than `enabled`), which means the spreading activation dynamics are
slightly stronger than they should be — a safe direction for a system that
warns about prevented outcomes.
