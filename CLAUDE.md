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
   with both option texts — it compares recorded outcomes and shows same-context
   branches (natural experiments) when they exist. It logs a falsifiable
   prediction that resolves later **when a new `record_decision` under the same
   `task_tag` carries matching option text** — comparing two options that are
   already recorded leaves the prediction pending by design. A pending (or
   empty) ledger is not a broken one.
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

### How retrieval actually behaves (read before concluding a tool is broken):

- **Retrieval is keyword-based (BM25) unless an embedding endpoint is
  configured** (`CAUSAL_MEMORY_EMBED_API` + `_KEY` + `_MODEL`, or they fall
  back to the `CAUSAL_MEMORY_LLM_*` values). Without it there is no semantic
  leg, so **quote the original wording**: reuse the exact words a lesson was
  recorded with, especially the outcome polarity (`成功` / `失败` / `无效` /
  `no_effect`). An abstract paraphrase can miss an edge that exists — that is
  a phrasing mismatch, not a missing memory.
- `trace_cause` is single-hop and needs lexical overlap with the recorded
  **outcome** text. If it returns nothing, retry with the original wording
  before concluding there is no cause.
- `trace_cause_chain` returns empty until the graph holds multi-hop chains
  (it needs more than one hop). `search_patterns` returns empty until pattern
  mining has produced meta-edges. In both cases an empty result means
  "not enough data yet", never "broken" — do not skip them permanently; the
  store grows.

### Host notes

- The store lives outside the context window: it survives compaction and
  restarts, which is the whole point — a lesson recorded once keeps paying.
- `remember` may not exist in every host: where the name is already taken,
  the plugin excludes it and you record facts with `record_fact` instead.

**Do NOT ask the user before searching or recording — do it proactively.**
**Especially record surprising outcomes — those are the most valuable lessons.**
