# Pearl (2009) — Causality: Models, Reasoning, and Inference

## Full Citation

Pearl, J. (2009). *Causality: Models, Reasoning, and Inference* (2nd ed.). Cambridge University Press. ISBN: 978-0521895606

## Abstract

This is the foundational text of modern causal inference. Pearl introduces the **structural causal model (SCM)** framework, which unifies:
- **Graphical models**: DAGs represent causal structure
- **Probability theory**: conditional probabilities encode observational data
- **Intervention calculus**: the do-operator (do(X=x)) represents interventions
- **Counterfactual logic**: subjunctive reasoning about hypothetical worlds

The book defines the **ladder of causation** — three levels of causal reasoning, each strictly more powerful than the last:

1. **Association (Rung 1)**: P(Y|X) — seeing / observing
2. **Intervention (Rung 2)**: P(Y|do(X)) — doing / acting
3. **Counterfactuals (Rung 3)**: P(Y_x|X=x', Y=y) — imagining / retrospecting

## Methodology

The book develops causal inference as a **mathematical formalism**:

1. **Axiomatic foundation**: SCMs are defined using structural equations (Y = f(X, U)) where U represents unobserved noise.

2. **Graphical criteria**: The **back-door criterion**, **front-door criterion**, and **d-separation** provide graphical conditions for identifiability — when can P(Y|do(X)) be estimated from observational data?

3. **Do-calculus**: Three inference rules for manipulating causal expressions:
   - Rule 1: Ignoring observations (P(y|do(x), z) = P(y|do(x)) if Y ⊥ Z | X in G_X)
   - Rule 2: Action/observation exchange (P(y|do(x), do(z)) = P(y|do(x), z) if Y ⊥ Z | X in G_XZ)
   - Rule 3: Ignoring actions (P(y|do(x), do(z)) = P(y|do(x)) if Y ⊥ Z | X in G_XZ(W))

4. **Counterfactual inference**: Counterfactuals are derived by modifying the structural equations and propagating the changes through the graph.

## Key Findings

### 1. The ladder of causation is a strict hierarchy

| Rung | Capability | Example |
|---|---|---|
| 1 (Association) | Prediction from observation | "Smokers have higher lung cancer rates" |
| 2 (Intervention) | Prediction from action | "If I force someone to smoke, will they get cancer?" |
| 3 (Counterfactual) | Retrospective reasoning | "Would this patient have cancer if they had never smoked?" |

Each rung requires strictly more information than the previous. You cannot answer intervention questions with associational data alone.

### 2. Causal DAGs are not just "Bayesian networks with arrows"

A causal DAG encodes **independence assumptions under intervention**, not just under observation. The edge X → Y means "manipulating X changes the distribution of Y" — not just "X and Y are correlated."

### 3. The do-operator makes causality calculus possible

The do-operator (do(X=x)) removes all incoming edges to X in the causal graph, simulating an intervention that breaks the natural causes of X. This allows formal reasoning about "what if I did X?" without running experiments.

### 4. Counterfactuals are computable from structural equations

Given:
- Y = f(X, U)
- Observed: X=x, Y=y

To evaluate "What would Y have been if X had been x'?":
1. Infer U from the observed data
2. Set X = x' (intervention)
3. Compute Y' = f(x', U)

This is the **abduction-action-prediction** algorithm.

## Methodology Critique

| Strength | Limitation |
|---|---|
| Provides a complete, rigorous mathematical framework for causality | Requires knowing the causal graph (or being able to discover it) |
| Do-calculus is provably complete for identifiable causal effects | Many real-world causal queries are not identifiable from observational data |
| Unifies graphical, probabilistic, and counterfactual reasoning | The framework assumes no unobserved confounding (or explicitly models it) |
| Counterfactual algorithm is elegant and intuitive | Structural equations are often unknown in practice; must be estimated |

## Connection to `causal-memory`

### 1. Our current position on the ladder

`causal-memory` v0.2 is a **Rung 1 system** with aspirations:

| Rung | `causal-memory` tool | Status |
|---|---|---|
| 1 (Association) | `search_causal`, `trace_cause` | ✅ Implemented |
| 2 (Intervention) | `counterfactual_query` (planned) | 🔄 v0.5 |
| 3 (Counterfactual) | Full counterfactual reasoning | 🔄 v0.5+ |

This is not a limitation — it's a **principled scope boundary**. We do not claim to implement Pearl's full framework. We implement the subset that is useful for LLM agents *today*, with a clear roadmap for climbing the ladder.

### 2. The causal graph as a "partial SCM"

Our `causal_edges` table is a simplified structural causal model:

```
SCM:  outcome = f(decision, noise)
causal-memory:  outcome_text = f(decision_text, confidence, discovered_by)
```

The differences:
- **No structural equation**: we do not model the functional form f(·)
- **No unobserved confounders**: U is implicit in the `confidence` field
- **No do-calculus**: we cannot compute P(outcome|do(decision)) formally

Instead, we use **empirical frequencies**: if "mutex lock" → "deadlock" has been observed 10 times with high confidence, we treat it as a reliable causal link.

### 3. The `relation` field as a simplified causal semantics

Our `relation` types map to Pearl's causal concepts:

| `relation` | Pearl equivalent | Meaning |
|---|---|---|
| `caused` | Direct causal edge | X → Y (intervening on X changes Y) |
| `enabled` | Necessary condition | X is necessary for Y but not sufficient |
| `prevented` | Inhibitory edge | X → ¬Y (intervening on X reduces Y) |
| `no_effect` | Absence of edge | X ↛ Y (intervening on X does not change Y) |

These are **structural constraints** — they encode what would happen under intervention, not just what has been observed.

### 4. Roadmap: intervention queries (Rung 2)

A v0.5 intervention query would look like:

```json
{
  "tool": "intervention_query",
  "params": {
    "action": "use channel communication",
    "context": "cache stampede protection",
    "predicted_outcome": "deadlock"
  }
}
```

The system would:
1. Check if "channel communication" → "deadlock" exists in the causal graph
2. If not, check for indirect paths (channel → [intermediate] → deadlock)
3. Return: "No direct causal link found. No indirect path with confidence > 0.5. Intervention is predicted safe."

This is **not** full do-calculus — it is heuristic intervention reasoning grounded in the empirical causal graph.

### 5. Roadmap: counterfactual queries (Rung 3)

A counterfactual query:

```json
{
  "tool": "counterfactual_query",
  "params": {
    "actual": "used mutex lock",
    "hypothetical": "used channel communication",
    "observed_outcome": "deadlock"
  }
}
```

The system would:
1. Confirm: "mutex lock" → "deadlock" exists (abduction)
2. Remove "mutex lock" → "deadlock" edge (action: set mutex = channel)
3. Check if "channel communication" → "deadlock" exists (prediction)
4. Return: "Under the counterfactual, deadlock probability estimated at 5% (based on 0 matching edges in causal graph)."

This is the **abduction-action-prediction** algorithm applied to the causal graph.

## Reading order

**Read this if you want to understand the formal target.** Pearl defines what a complete causal inference system looks like. `causal-memory` v0.2 is a small subset, but the roadmap is clear: climb the ladder from association to intervention to counterfactuals.

---

*Last updated: 2026-07-27*
