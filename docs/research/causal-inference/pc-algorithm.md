# Spirtes, Glymour & Scheines (2000) — Causation, Prediction, and Search

## Full Citation

Spirtes, P., Glymour, C., & Scheines, R. (2000). *Causation, Prediction, and Search* (2nd ed.). MIT Press. ISBN: 978-0262194402

## Abstract

This book introduces the **PC algorithm** and related constraint-based methods for **automated causal discovery** — inferring causal DAGs from observational data alone. The key insight is that conditional independence relationships in data impose constraints on the possible causal structures. By systematically testing these constraints, the algorithm can narrow down the set of causal graphs consistent with the data.

The PC algorithm (named after its authors: **P**eter Spirtes and **C**lark Glymour):
1. Starts with a fully connected graph
2. Tests conditional independence between all pairs of variables
3. Removes edges where variables are independent (conditioned on subsets of other variables)
4. Orients remaining edges using collider detection (V-structures)
5. Propagates orientation constraints to resolve ambiguous edges

## Methodology

The book develops:

1. **Formal foundations**: Causal graphs are defined as representations of probability distributions over variables. The **causal Markov condition** and **faithfulness assumption** are introduced as axioms linking graphs to distributions.

2. **The PC algorithm**: A polynomial-time algorithm for causal discovery under the assumption of no unobserved confounders (or with explicit handling of latent variables via the FCI algorithm).

3. **Statistical tests**: The algorithm requires a test for conditional independence. The book discusses:
   - Parametric tests (Gaussian: partial correlation)
   - Non-parametric tests (kernel-based, mutual information)
   - Discrete tests (G², χ²)

4. **Extensions**: FCI (Fast Causal Inference) for latent variables, CDNI for non-linear relationships, and conservative variants for robustness.

## Key Findings

### 1. Causal structure is identifiable from observational data (with assumptions)

Under the causal Markov condition and faithfulness, the true causal graph is **identifiable up to Markov equivalence** from observational data. This means:
- You cannot distinguish X → Y from Y → X if both are consistent with the data
- But you *can* distinguish X → Y → Z from X ← Y → Z using collider detection

### 2. The faithfulness assumption is strong but testable

Faithfulness: if the true causal graph implies an independence, that independence holds in the data (no "accidental" cancellations).

This is violated in cases of:
- **Path cancellation**: two paths between X and Y have opposite effects that cancel out
- **Deterministic relationships**: Y is a deterministic function of X, masking other dependencies

The book discusses conservative variants that weaken the faithfulness assumption.

### 3. Latent variables complicate but do not prevent causal discovery

The FCI algorithm extends PC to handle unobserved confounders. It produces a **partial ancestral graph (PAG)** instead of a DAG, representing equivalence classes of causal structures with latent variables.

### 4. Sample complexity is a practical barrier

The number of conditional independence tests grows combinatorially with the number of variables. For n variables, the worst-case complexity is O(n² · 2ⁿ). In practice, the algorithm is feasible for n < 100 with appropriate sparsity assumptions.

## Methodology Critique

| Strength | Limitation |
|---|---|
| Provides a principled, automated method for causal discovery | Faithfulness assumption is strong and often violated in real data |
| PC algorithm is polynomial-time (under sparsity) | Sample complexity is high; small datasets yield unreliable graphs |
| Handles latent variables via FCI | FCI is much slower and less reliable than PC |
| Well-tested on synthetic and real datasets | Real-world variables are often not well-defined (what is "intelligence"?) |

## Connection to `causal-memory`

### 1. `meta_causal_edges` as automated causal discovery

Our v0.3 roadmap includes activating the `meta_causal_edges` table for **cross-task pattern mining**. The PC algorithm provides the theoretical framework for this:

**Input**: A set of causal edges from multiple tasks
```
[concurrency] mutex → deadlock
[caching] no_ttl → memory_leak → OOM
[database] missing_index → slow_query → timeout
```

**PC algorithm applied**:
1. Extract variables: {mutex, deadlock, no_ttl, memory_leak, OOM, missing_index, slow_query, timeout}
2. Test conditional independences: Is "deadlock" independent of "OOM" given "memory_leak"?
3. Build graph: shared causes, common effects, mediators

**Output**: A higher-level causal structure
```
resource_misconfiguration → system_failure
  ├── [concurrency] mutex → deadlock
  ├── [caching] no_ttl → memory_leak → OOM
  └── [database] missing_index → slow_query → timeout
```

This is **not** running the PC algorithm on raw data — it is running it on the **causal graph itself**, treating edges as data points.

### 2. From empirical causal graph to formal causal model

`causal-memory` v0.2 stores **empirical causal links** (observed decision→outcome pairs). The PC algorithm suggests a path to **formal causal models**:

1. **Accumulate** enough causal edges to detect statistical patterns
2. **Run PC** on the edge set to discover higher-order structure
3. **Produce** a validated causal graph for each `task_tag`
4. **Query** the graph using Pearl's do-calculus for intervention reasoning

This bridges the gap between "agent experience" (empirical) and "causal inference" (formal).

### 3. Handling confounders in agent causal graphs

A key challenge in agent causal graphs is **confounding by context**:
- Agent makes decision D in context C
- Outcome O occurs
- Was D the cause of O, or was C the cause?

The PC algorithm's collider detection helps here:
- If D and O are both correlated with C
- But D and O are independent given C
- Then C is a confounder, not D

This could be used to **correct confidence levels**: edges that survive confounder-adjusted tests get higher confidence.

### 4. Practical limitations → heuristic approximation

The PC algorithm's sample complexity is prohibitive for small causal graphs (agents might only have 50–100 edges). Instead of running full PC, we can use **heuristic variants**:

- **Frequent pattern mining**: find decision→outcome pairs that appear across multiple tasks
- **Contradiction detection**: if D→O in task A but D→¬O in task B, flag for review
- **Similarity clustering**: cluster edges by decision/outcome text similarity

These are approximations of PC's core functions, adapted to the agent's sparse-data regime.

## Reading order

Read after Pearl (2009). Pearl tells you *what* a causal model is; Spirtes tells you *how to discover one* from data. Together they define the v0.5 roadmap: learn causal structure from agent experience, then query it using do-calculus.

---

*Last updated: 2026-07-27*
