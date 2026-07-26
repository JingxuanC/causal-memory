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

1. Call `search_causal` with the relevant `task_tag` to check past lessons
2. If past experience is relevant, incorporate it into your approach

### After acting on a decision and observing the result:

3. Call `record_decision` with:
   - `decision`: what you decided
   - `outcome`: what actually happened
   - `relation`: caused / enabled / prevented / no_effect
   - `task_tag`: the task category
   - `confidence_source`: temporal / rule / llm_inferred / user_feedback

### When something fails unexpectedly:

4. Call `trace_cause` with a description of what went wrong

**Do NOT ask the user before searching or recording — do it proactively.**
**Especially record surprising outcomes — those are the most valuable lessons.**
