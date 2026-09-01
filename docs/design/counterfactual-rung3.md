# Counterfactual Rung 3 — Design

> Status: **Phase A implementing** (2026-09-01). Prior art:
> [research/computational-ai/rung3-prior-art.md](../research/computational-ai/rung3-prior-art.md).
>
> One-line thesis: Pearl's third rung needs **abduction** (conditioning on
> the recorded world state), a **mechanism model**, and — to be science
> rather than imagination — **falsification by future branches**. We cannot
> reach SCM ground truth in open worlds, but we can build the complete
> engineering subset: record what abduction needs, link the natural
> experiments we already accumulate, make every counterfactual claim a
> logged prediction, and rerun literally where the world is code.

## 1. Where we are on the ladder

| Rung | Tool | What it actually computes |
|---|---|---|
| R1 association | `search_causal` | P(outcome \| similar decision) from recorded edges |
| R2 intervention | `intervention_query` | Empirical do-effects: forward chains from similar past actions, task_tag stratification, Simpson warning |
| R3-lite | `counterfactual_query` | Contrastive A/B outcome distributions — but the two sides' evidence comes from **different, unmodeled contexts**; there is no abduction |

The gap is structural, not parametric: `P(Y_x | X=x', Y=y', C)` requires a
recorded `C`. Executable Counterfactuals (arXiv:2510.01539) measured the
same gap in SOTA LLMs (−25–40% when abduction is enforced). Our plan adds
`C` (Phase 0), links same-`C` branches (Phase 1), converts claims into
testable predictions (Phase 2), and reruns `C` where it is executable
(Phase 4 interface).

## 2. Phases

Implementation order and rationale:

| Phase | Name | Ships | Status |
|---|---|---|---|
| 0 | Abduction substrate | `context` param → `causal_edges.context_fingerprint/context_text` (schema v14) | **this PR** |
| 1 | Natural-experiment graph | write-time `decision_forks` pairs; fork-aware `counterfactual_query` | **this PR** |
| 2 | Prediction ledger | `pending_predictions` + auto-resolution on record + `prediction_report` (17th tool) | **this PR** |
| 3 | Estimation & simulation | micro-SCM (noisy-OR over `{caused, enabled, prevented}`) and LLM replay with identifiability gates | deferred (needs fork density) |
| 4 | Executable replay | closed-world task_tags route to a replay plan; result feeds the ledger | interface only |

Phases 0/1/2 are cheap, unconditionally useful, and prerequisite for
everything else. Phase 3 waits until the fork graph is dense enough to
fit mechanisms (watch metric: fork pairs per task_tag). Phase 4 has a
working external engine (stepback's dirty-set replay) — we define the
interface and route, we do not build a trace format yet.

## 3. Phase 0 — abduction substrate (schema v14)

### Schema

```sql
ALTER TABLE causal_edges ADD COLUMN context_fingerprint TEXT;
ALTER TABLE causal_edges ADD COLUMN context_text TEXT;
CREATE INDEX IF NOT EXISTS idx_causal_fingerprint
    ON causal_edges(context_fingerprint) WHERE context_fingerprint IS NOT NULL;
```

- `context_fingerprint` — `task_tag + "\x1f" + normalize(context)` where
  `normalize` = lowercase + collapse runs of whitespace. Stored as the
  normalized string itself (not a hash): greppable, debuggable, indexable.
  `NULL` = no context recorded (legacy behavior, excluded from fork logic).
- `context_text` — the raw context description as given (audit/display).

### Write path

- `CausalStore::record_decision_full` gains `context: Option<&str>`.
  `record_decision_at` (test/bench helper) passes `None` — unchanged
  signature.
- MCP `record_decision` gains optional `context` param: *"Short
  description of the situation the decision was made in (environment,
  constraints, key parameters). Records with the same task_tag + context
  become comparable branches."* Agents that don't send it lose nothing.
- `CausalEntry` carries both fields through (`ENTRY_COLUMNS` +1 each).
- Soft-invalidation, supersession, GC, sleep: untouched — the new columns
  ride along on the existing edge lifecycle.

## 4. Phase 1 — natural-experiment graph (`decision_forks`)

```sql
CREATE TABLE IF NOT EXISTS decision_forks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_id_a INTEGER NOT NULL,   -- lower edge id
    edge_id_b INTEGER NOT NULL,   -- higher edge id
    fingerprint TEXT NOT NULL,    -- shared context_fingerprint
    discovered_at INTEGER NOT NULL,
    UNIQUE(edge_id_a, edge_id_b),
    FOREIGN KEY (edge_id_a) REFERENCES causal_edges(id),
    FOREIGN KEY (edge_id_b) REFERENCES causal_edges(id)
);
CREATE INDEX IF NOT EXISTS idx_forks_a ON decision_forks(edge_id_a);
CREATE INDEX IF NOT EXISTS idx_forks_b ON decision_forks(edge_id_b);
```

### Semantics

A fork pair = two **valid** edges sharing a context fingerprint whose
decision chunks differ (`from_id` different). Same decision text re-recorded
with a different outcome is the contradiction/supersession path, not a
fork. Pair is stored id-ordered; `INSERT OR IGNORE` dedups.

### Write-time detection (inside `record_decision_full`)

After inserting edge E with fingerprint F:

1. `SELECT` valid edges with fingerprint F, `from_id != E.from_id`, ordered
   by `discovered_at DESC`, **capped at 10** (guards against n² blowup on
   hot fingerprints).
2. Insert pairs `(min(id,E), max(id,E))` for each.

Detection failure never blocks recording (best-effort, like embedding).

### Fork-aware `counterfactual_query`

Unchanged comparison first (backward-compatible output), then an extra
section when fork evidence exists:

```
🔀 Same-context branches (natural experiments, n pairs):
  [ctx] …fingerprint…
    A: "used mysql" →(caused)→ "migration locks" [negative]
    B: "used postgres" →(caused)→ "clean cutover" [positive]
```

Implementation: after both `side_evidence` retrievals, for each retrieved
edge look up its fork siblings (`fork_siblings_for_edges`); a sibling whose
decision text is also retrieved on the *other* side renders as a paired
row. Same-context pairs outweigh aggregate distributions in the verdict
ordering (a fork pair is evidence about one world; distributions are
evidence about many) — the verdict function takes paired evidence first,
falls back to distribution diff. Pairs also render standalone (sibling not
on the other side) at reduced weight, since they still pin the context.

**Known limitation (found while testing)**: `side_evidence` retrieves on
decision AND outcome text, so two options that share vocabulary (e.g.
"use the main model" vs "use a small model" — and outcomes that share
words like "migration") contaminate each other's pools, flattening the
distribution contrast toward a tie. Fork pairs are immune (they compare
recorded same-context branches, not query-side pools). Competitive
separation — dropping from each side the entries the other side ranks
higher — is a Phase-3 refinement.

## 5. Phase 2 — prediction ledger

```sql
CREATE TABLE IF NOT EXISTS pending_predictions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL,
    option_a TEXT NOT NULL,          -- decision text as queried
    option_b TEXT NOT NULL,          -- alternative text as queried
    task_tag TEXT,
    verdict TEXT NOT NULL,           -- 'prefer_a' | 'prefer_b' | 'no_difference'
    method TEXT NOT NULL,            -- 'contrastive' (later: micro_scm | llm_sim | executable)
    confidence REAL NOT NULL,        -- evidence strength at prediction time
    evidence TEXT,                   -- compact JSON snapshot (dists, fork count)
    resolved_at INTEGER,             -- NULL = pending
    resolved_option TEXT,            -- 'a' | 'b' — which option reality took
    actual_polarity TEXT,            -- observed outcome polarity
    correct INTEGER                  -- 1/0; NULL = ambiguous (mixed/neutral actual)
);
```

### Logging

Every `counterfactual_query` writes one pending row (best-effort; failures
never break the query). Output gains a footer:
`📐 Prediction #N logged — resolved automatically when either option is recorded.`

### Resolution (automatic, inside `record_decision`)

When an edge is recorded whose `decision_text` equals a pending
prediction's `option_a` or `option_b` (exact-text match — consistent with
chunk reuse semantics):

- `resolved_option` = which side matched; `actual_polarity` = the edge's
  stored polarity.
- `correct`:
  - verdict `prefer_X`, reality took X → correct iff polarity = positive
    (negative ⇒ wrong; mixed/neutral ⇒ NULL, ambiguous).
  - verdict `prefer_X`, reality took the other → correct iff polarity =
    negative (it turned out X's rival failed — the preference was right);
    positive ⇒ 0; mixed/neutral ⇒ NULL.
  - `no_difference` → resolves with correct=NULL (evidence about either
    branch is weak evidence about indifference).
- A prediction resolves **once** (first matching record wins); later
  re-records of the same option don't re-resolve (they're supersession
  territory).

### `prediction_report` (17th MCP tool, read-only)

```
📐 Prediction ledger: 3 resolved / 5 pending (method=contrastive)
   accuracy 2/2 (100%, 1 ambiguous excluded)
   by task_tag: causal-memory 2/2 · debugging 0/0
   pending: #4 "reuse main model" vs "small dedicated model" (causal-memory, 2026-09-01)
```

Per-method and per-task_tag accuracy, pending list with ages. This is the
calibration loop's dashboard — over time it tells us which R3 method is
trustworthy where. Python binding mirrors it (`prediction_report()`).

## 6. Phase 3 (deferred) — estimation & simulation

Trigger: ≥ 30 fork pairs within one task_tag stratum. Then:

1. **Micro-SCM**: fit noisy-OR/noisy-AND-NOT gates over
   `{caused, enabled, prevented}` → outcome polarity classes, per stratum,
   EM from recorded edges; query = abduction (posterior over context
   variables given the observed episode) → action (swap the decision
   node's mechanism) → prediction. Identifiability gate à la cfid: when
   the stratum's fork density is too low, answer "not identifiable —
   degrading to contrastive" (labels: point / set / not-identifiable).
2. **LLM replay** (`counterfactual_simulate`): retrieve the episode's
   full context snapshot + trajectory, ask the LLM to re-run with the
   alternative; score with the `reconstruct_lesson --calibrate` agreement
   mechanism; label `confidence_source: llm_inferred`; predictions from
   this path enter the ledger with `method='llm_sim'` so calibration can
   separate its trust level from contrastive's.

## 7. Phase 4 (interface) — executable replay

`counterfactual_query` output routes closed-world task_tags (build / test /
config / bench) to a replay plan:

```
🧪 This looks like a closed-world decision (task_tag=build).
   Instead of estimating: replay it. Plan:
   1. Locate the recorded outcome edge (#123, event_time …)
   2. Apply the alternative as a patch/flag in a sandbox
   3. Execute; record result with record_decision(…, context=<same>)
   → the prediction ledger resolves automatically.
```

The engine (stepback-style trace + dirty-set replay) is future work; the
routing, the plan template, and the ledger feedback path are defined now
so `method='executable'` rows can exist from day one.

## 8. Test plan

Unit (store): fingerprint normalization; migration v13→v14 (column adds
+ idempotent re-run); fork pair creation on second distinct decision in
same context; no fork on same-text re-record; pair cap; fork lookup by
edge ids.

Unit (memory facade): `record_decision` with context stores both columns;
`counterfactual_query` renders fork section when present, footer always;
prediction row logged per query; resolution matrix (verdict × taken option
× polarity → correct value); `no_difference` → NULL; single resolution;
`prediction_report` math (accuracy excludes ambiguous; per-tag split).

Regression: byte-identical `counterfactual_query` output when no forks
exist and ledger write fails open; `record_decision` without context
param unchanged; migration e2e on a v13 fixture DB; full
`cargo test --workspace`; PyO3 smoke (new `record_decision(context=…)`,
`counterfactual_query`, `prediction_report`).

## 9. Non-goals

- No SCM ground-truth claims in open worlds — every output keeps its
  honesty label; the ladder is "recorded-evidence → paired-evidence →
  gated-estimate → replayed-fact".
- No LLM dependency in Phases 0/1/2 (deterministic; hermetic tests).
- No change to edge lifecycle (invalidation/supersession/GC/sleep) or to
  retrieval ranking.
