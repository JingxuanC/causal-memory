# Causal Memory MCP Integration

> Paste this into your `CLAUDE.md` / `AGENTS.md` / system prompt to activate causal memory.
>
> Per insights/13 §1.3: agents don't proactively call memory tools without instruction.
> This prompt forces proactive use.

## Causal Memory Integration

You have access to a **causal memory layer** via MCP tools. This records your
past decisions and their outcomes, so you can learn from experience across
sessions.

### Before any non-trivial decision (architecture choice, debugging approach,
library selection, deployment strategy):

1. Call `search_memory` with your query — it searches facts AND causal
   lessons at once (RRF-fused). If you know you need causal lessons
   specifically, call `search_causal` with the relevant `task_tag`
2. For risky or irreversible actions, also call `intervention_query` to see
   what outcomes similar past actions caused (safe / warning / danger)
3. **When choosing between two concrete options**, call `counterfactual_query`
   with both option texts — it compares recorded outcomes, shows same-context
   branches (natural experiments) when they exist, and logs a falsifiable
   prediction that resolves automatically when either option is later recorded
4. If past experience is relevant, incorporate it into your approach

### After acting on a decision and observing the result:

5. Call `record_decision` with:
   - `decision`: what you decided
   - `outcome`: what actually happened
   - `relation`: caused / enabled / prevented / no_effect
   - `task_tag`: the task category
   - `confidence_source`: temporal / rule / llm_inferred / user_feedback
   - `context` (important): a short description of the situation the decision
     was made in (environment, constraints, key parameters). Decisions with
     the same task_tag + context become comparable branches — this is what
     powers same-context counterfactual evidence. If you weighed multiple
     options at this decision point, ALWAYS record the context.

### When you learn a stable fact (preference, tech stack, config):

6. Call `record_fact` with `key` (category), `value` (the fact), and
   `scope` (user / session / agent). If the fact replaces an older one
   (e.g. the user switched package managers), set `replace_same_key: true`
   to retire the outdated value. Retrieve later with `search_facts` or
   `search_memory`.

### When something fails unexpectedly:

7. Call `trace_cause` with a description of what went wrong; use
   `trace_cause_chain` when the root cause is more than one hop away

### When a recorded lesson turns out to be wrong:

8. Call `invalidate_decision` so the falsified edge stops surfacing in
   future searches (it stays in the DB for audit)

### Periodically:

9. Call `prediction_report` to check whether the system's counterfactual
   advice is actually any good (accuracy per method / per task_tag).

**Do NOT ask the user before searching or recording — do it proactively.**
**Especially record surprising outcomes — those are the most valuable lessons.**
