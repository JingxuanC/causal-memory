# Sloman (2005) — Causal Models: How People Think About the World and Its Alternatives

## Full Citation

Sloman, S. A. (2005). *Causal Models: How People Think About the World and Its Alternatives*. Oxford University Press. ISBN: 978-0195183115

## Abstract

Sloman presents a comprehensive theory of human causal reasoning based on **causal Bayesian networks** (directed acyclic graphs with probabilistic dependencies). The central claim is that humans do not reason about causality by memorizing correlations — instead, they maintain **mental causal models** (DAGs) that encode their beliefs about how the world works.

Key topics:
- **Causal model theory**: people represent knowledge as graphs, not associations
- **Intervention reasoning**: "What happens if I do X?" (Pearl's do-calculus)
- **Counterfactual reasoning**: "What would have happened if I had done Y instead?"
- **Explanation and prediction**: causal models support both forward prediction and backward explanation

## Methodology

The book synthesizes:
- **Experimental psychology**: human participants judge causal strength, make predictions, and evaluate explanations; their responses are compared to Bayesian network predictions.
- **Computational modeling**: causal Bayesian networks are used as normative benchmarks; deviations reveal human biases (e.g., temporal order bias, mechanism bias).
- **Philosophy of science**: the book engages with Hume, Mill, and Pearl to ground psychological findings in formal causal inference.

Key experimental paradigms:
- **Causal learning from contingency**: participants observe event co-occurrence and infer causal strength (ΔP, power PC theory).
- **Causal reasoning from structure**: given a causal graph, participants predict outcomes under interventions.
- **Explanation selection**: given an outcome, participants choose the "best" explanation from a set of candidate causes.

## Key Findings

### 1. The causal model is the default representational format

> "People do not merely learn that events are correlated; they learn *why* — they construct causal models." (p. 47)

This is the central thesis. When humans observe:
- "Smoking correlates with lung cancer"

They do not store this as a correlation matrix entry. Instead, they construct:
- Smoking → [mechanism] → Lung Cancer

The mechanism may be vague ("something in smoke damages cells"), but the **directional structure** is explicit.

### 2. Causal reasoning is compositional

Given:
- A → B (smoking causes yellow fingers)
- B → C (yellow fingers do not cause cancer)
- A → C (smoking causes cancer)

Humans correctly infer that B is not a cause of C, even though B correlates with C. This requires **structural reasoning** over a graph, not just statistical association. This is the **explaining away** phenomenon.

### 3. Interventions are cognitively privileged

Humans find it easier to reason about **interventions** ("What if I force X?") than **observations** ("Given that I observe X..."). This maps directly to Pearl's distinction between **do(X)** and **see(X)**.

### 4. Counterfactuals are the gold standard for causal attribution

To determine whether A caused B, humans mentally simulate:
- "If A had not happened, would B still have happened?"

If the answer is "no," A is judged a cause of B. This is computationally expensive but cognitively natural.

## Methodology Critique

| Strength | Limitation |
|---|---|
| Unifies experimental psychology with formal causal inference (Pearl) | Some experiments use simplified causal structures; real-world reasoning is more complex |
| Bayesian networks provide a precise computational framework | Bayesian networks assume fixed structure; humans may dynamically revise structure |
| Successfully explains explaining away, screening off, and other structural phenomena | Does not fully explain how causal models are *learned* from sparse data |
| Counterfactual account aligns with legal/moral reasoning | Counterfactual simulation is computationally intractable for large graphs |

## Connection to `causal-memory`

### 1. `causal_edges` = flattened mental causal model

Our schema is a direct implementation of Sloman's theory:

```sql
-- Each row is an edge in the mental causal model
INSERT INTO causal_edges (from_id, to_id, relation, confidence)
VALUES ('mutex_lock', 'deadlock', 'caused', 0.85);
```

The graph is flattened into a relational table for efficiency, but the **semantic structure** is identical to a causal Bayesian network.

### 2. `relation` types encode structural constraints

| Relation | Sloman equivalent | Example |
|---|---|---|
| `caused` | Direct causal link | A → B |
| `enabled` | Necessary but not sufficient | Fuel enables fire |
| `prevented` | Inhibitory link | Vaccine → ¬Disease |
| `no_effect` | Explicit null link | A ↛ B (learned negative) |

These are not arbitrary labels — they map to the **structural constraints** that causal models must satisfy (e.g., `no_effect` prevents spurious paths in `trace_cause_chain`).

### 3. `trace_cause` = backward explanation; `trace_cause_chain` = structural pathfinding

- `trace_cause`: "Given outcome B, what decision A could explain it?" → **backward explanation**
- `trace_cause_chain`: "Follow the causal structure backward from B to find the root cause" → **structural pathfinding**

Future extensions (v0.5+) could add:
- **Intervention queries**: "If I had used channel instead of mutex, would the deadlock still have occurred?"
- **Explaining away**: "Given that both A and B could cause C, and I observe A, does B still matter?"

### 4. `confidence` = subjective causal strength

Sloman shows that humans judge causal strength on a continuous scale, not a binary {cause, not-cause}. Our `confidence` field (0.0–1.0) directly models this. The ordering:
- `temporal` (0.4) < `llm_inferred` (0.6) < `rule` (0.7) < `user_feedback` (0.95)

...is a subjective scale of **evidential strength**, exactly as Sloman describes.

## Reading order

**Read this first** if you want to understand why a graph structure is necessary for causal memory. Flat key-value stores (Mem0) or vectors (Zep) cannot support the structural reasoning that causal models require.

---

*Last updated: 2026-07-27*
