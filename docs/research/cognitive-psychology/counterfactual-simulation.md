# Gerstenberg et al. (2021) — Counterfactual Simulation in Human Cognition

## Full Citation

Gerstenberg, T., Goodman, N. D., Lagnado, D. A., & Tenenbaum, J. B. (2021). A Counterfactual Simulation Model of Causal Judgments for Physical Events. *Psychological Review*, 128(5), 936–975. https://doi.org/10.1037/rev0000280

## Abstract

How do humans determine whether one event caused another? The traditional answer (probabilistic contrast: ΔP = P(effect|cause) – P(effect|¬cause)) fails for physical events where the cause and effect are deterministic.

Gerstenberg et al. propose the **Counterfactual Simulation Model (CSM)**: to judge whether A caused B, humans:
1. Mentally simulate what would have happened if A had not occurred
2. Compare the simulated outcome to the actual outcome
3. If B would not have occurred without A, then A is judged a cause of B

This is implemented as a **probabilistic program** (using Church/Lisp-like generative models) that simulates physical dynamics (ballistics, collisions, trajectories).

## Methodology

The paper uses three converging methods:

1. **Behavioral experiments**: participants watch videos of physical events (billiard-ball collisions, domino chains) and rate causal responsibility on a 7-point scale.

2. **Computational modeling**: the CSM is implemented as a generative probabilistic program. It takes a physical scene, removes the candidate cause, resimulates the dynamics with noise, and computes the probability that the effect still occurs.

3. **Model comparison**: CSM is compared against alternative models:
   - ΔP (probabilistic contrast)
   - Force dynamics (Wolff's model)
   - Covariation models
   - Heuristic models

CSM consistently outperforms alternatives in predicting human judgments.

## Key Findings

### 1. Counterfactuals are computed via simulation, not logic

> "Causal judgments arise from mental simulations of what would have happened in counterfactual worlds." (p. 938)

This is not symbolic logic ("if ¬A then ¬B"). It is **physical simulation**: the cognitive system runs an approximate physics engine forward from a modified initial state and observes what happens.

### 2. Noise is essential to the simulation

The CSM injects noise into the simulation (e.g., slight variations in ball velocity, angle). Without noise, counterfactuals would be deterministic and brittle:
- "If the ball had been 1mm to the left, would the collision still have happened?"
- With noise: sometimes yes, sometimes no → probability of causation

This probabilistic approach maps to our **confidence** field: causal judgment is not binary but a probability distribution.

### 3. Multiple causes are graded, not exclusive

When multiple candidates could have caused an effect, humans assign **graded responsibility** to each:
- A contributed 60%, B contributed 40%

This requires **structural counterfactuals** — simulating the absence of each candidate while holding the others constant.

### 4. The model generalizes to non-physical domains

While the paper focuses on physical events (billiard balls), the authors argue that the same simulation mechanism applies to social, legal, and abstract causal reasoning — just with different "physics engines" (social rules, legal statutes, logical constraints).

## Methodology Critique

| Strength | Limitation |
|---|---|
| CSM outperforms all competitor models on physical domain tasks | Physical domains have well-defined "physics engines"; social/abstract domains do not |
| Probabilistic programming provides a principled framework | Computationally expensive; real-time human judgments may use approximations |
| Model predicts fine-grained graded judgments, not just binary | Assumes access to a generative model of the domain; humans may not always have this |
| Beautifully integrates psychology, AI, and philosophy | Limited to simple physical scenes; scaling to complex real-world causation is untested |

## Connection to `causal-memory`

### 1. `trace_cause_chain` is a partial CSM implementation

The CSM answers: "If I had not done A, would B still have happened?"

`trace_cause_chain` answers: "Which A led to B, and what led to A?"

Both require **traversing a causal structure** and evaluating counterfactual alternatives. The difference is computational cost:
- CSM: full physical simulation (expensive, domain-specific)
- `trace_cause_chain`: graph traversal over stored causal edges (cheap, domain-general)

For an LLM agent, the "physics engine" is **not** a billiard-ball simulator — it's the **agent's own causal graph**. To evaluate "If I had used channels instead of mutexes," the agent queries its causal graph for the mutex→deadlock edge and checks whether a channel→deadlock edge exists.

### 2. Graded causality → confidence-weighted chains

CSM assigns graded responsibility (A: 60%, B: 40%). Our `trace_cause_chain` returns **confidence-weighted paths**:

```
Chain 1 (chain confidence: 61%):
  mutex_lock →(0.9)→ deadlock
Chain 2 (chain confidence: 34%):
  no_ttl →(0.8)→ cache_expiry →(0.85)→ memory_leak →(0.5)→ OOM
```

The agent can compare chains by their **product confidence** — exactly analogous to CSM's graded responsibility.

### 3. Future: counterfactual intervention queries (v0.5+)

A full CSM-inspired extension would add:

```json
{
  "tool": "counterfactual_query",
  "params": {
    "actual_decision": "used mutex lock",
    "counterfactual_decision": "used channel communication",
    "outcome": "deadlock"
  }
}
```

This would:
1. Look up `actual_decision → outcome` edge
2. Look up `counterfactual_decision → ?` edge
3. Return: "Under the counterfactual, the probability of deadlock would have been X%"

This requires storing **alternative decisions** (decision forks) in the graph — the next major schema evolution.

### 4. Noise injection → confidence calibration

CSM's insight that counterfactuals require noise injection maps to our confidence calibration:
- High-confidence edges (user_feedback) = low noise → deterministic counterfactuals
- Low-confidence edges (temporal) = high noise → uncertain counterfactuals

When evaluating a counterfactual, the system should sample from the confidence distribution, not just use the point estimate.

## Reading order

Read after Sloman (2005). Sloman tells you *that* humans use causal graphs; Gerstenberg tells you *how* they evaluate causal claims (via simulation). Together they justify both the graph structure and the multi-hop reasoning mechanism.

---

*Last updated: 2026-07-27*
