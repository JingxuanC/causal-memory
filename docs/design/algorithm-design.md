# Causal Memory Dynamics: Algorithm Design

> **Status**: Design document for the three algorithmic contributions.
> **Date**: 2026-08-07
> **Target**: Full paper with theoretical analysis + CausalEval experiments.

## Overview

Three algorithmic contributions on a typed causal memory graph, forming a
unified framework:

```
Layer 1 — Inference:  How does activation spread through the graph?
         → Multiplicative Inhibitory Gating (replaces additive spread)

Layer 2 — Adaptation: How do edge weights evolve with experience?
         → Heterogeneous Online Weight Learning (per-type update rules)

Layer 3 — Structure:   How does the graph topology evolve (forgetting)?
         → Quality-Preserving Graph Sparsification (formalizes SWR)
```

Each layer depends on the one below: learning (Layer 2) requires the
inference rule from Layer 1 to define "prediction accuracy"; sparsification
(Layer 3) requires Layers 1+2 to define "retrieval quality" as its objective.

---

# Layer 1: Multiplicative Inhibitory Gating

## Problem

Current spreading activation uses **additive** spread:

$$a(v) = \sum_{u \to v \in E^+} \alpha_{type} \cdot w(u) \cdot d^k + \sum_{u \to v \in E^-} (-0.3) \cdot w(u) \cdot d^k$$

where $E^+$ = excitatory edges (caused/enabled/fact/meta), $E^-$ = prevented edges,
$d$ = decay, $k$ = hop distance.

**The Additive Failure Theorem**: At a node $v$ with in-degree $\deg^+(v)$ and
$n_p$ prevented edges, the net activation is:

$$a(v) = \underbrace{n_c \cdot \bar{w}_+}_{\text{excitation}} - \underbrace{0.3 \cdot n_p \cdot \bar{w}_-}_{\text{inhibition}}$$

When $n_c > 0.3 \cdot n_p \cdot (\bar{w}_-/\bar{w}_+)$, inhibition is
overwhelmed — a single prevented edge cannot suppress the outcome even when
the causal semantics demand "blocked." The current system works on CausalEval
only because the graphs are sparse (low in-degree); on dense production graphs
(2200+ edges), the inhibition signal would be buried.

## Solution: Multiplicative Gating

Replace the additive formula with a **gated** activation:

$$a(v, q) = E(v, q) \cdot \big(1 - I(v, q)\big)$$

where:

$$E(v, q) = \sum_{u \to v \in E^+} \alpha_{type} \cdot w(u) \cdot d^k$$

$$I(v, q) = \sigma\!\left(\sum_{u \to v \in E^-} \beta \cdot w(u) \cdot d^k\right)$$

$\sigma(x) = 1/(1+e^{-x})$ is the sigmoid, bounding $I \in (0, 1)$.

**Key property**: As $I \to 1$ (strong inhibition signal), $a \to 0$ regardless
of how large $E$ is. A single confident prevented edge can gate an arbitrarily
strong excitatory signal. This is **structurally impossible** with additive
spread — you would need infinite negative weight.

## Adaptive Gate Strength

The gate strength $\beta$ should depend on the local graph structure.

**Information-theoretic argument**: A node with high in-degree has a high prior
probability of being activated (many paths reach it). A prevented edge arriving
at such a node carries high information (low-probability event: "this is
blocked despite many reasons it should happen"). By the self-information
formula $-\log p$, the surprise of a prevented signal at a high-degree node is:

$$\text{surprise}(v) \approx \log(\deg^+(v) + 1)$$

This motivates a degree-normalized gate:

$$\beta(v) = \beta_0 \cdot \frac{\log(\deg^+(v) + 1)}{\log(\overline{\deg^+} + 1)}$$

where $\beta_0$ is a global constant (tuned on CausalEval) and $\overline{\deg^+}$
is the mean in-degree. High-degree nodes get stronger gates; low-degree nodes
get weaker gates (a prevented edge at a leaf node is less surprising).

## Implementation Plan

### Code Changes

1. **`hippocampus/types.rs`**: `Relation::spread_coeff()` unchanged — the
   pre-multiplied values still carry the type coefficient.

2. **`hippocampus/mod.rs`**: `spread_step()` — split into excitatory and
   inhibitory accumulation:

```rust
fn spread_step_gated(&self, activations: &[f32], decay: f32) -> Vec<f32> {
    let mut excitation = vec![0.0_f32; self.num_nodes];
    let mut inhibition = vec![0.0_f32; self.num_nodes];

    for (i, &a) in activations.iter().enumerate() {
        if a.abs() < self.threshold { continue; }
        let start = self.row_ptr[i] as usize;
        let end = self.row_ptr[i + 1] as usize;
        for edge_idx in start..end {
            if !self.edge_valid[edge_idx] { continue; }
            let target = self.col_idx[edge_idx] as usize;
            let weight = self.values[edge_idx]; // pre-multiplied: raw × coeff
            if weight >= 0.0 {
                excitation[target] += a * weight * decay;
            } else {
                inhibition[target] += a * weight.abs() * decay;
            }
        }
    }

    // Multiplicative gating: a = E × (1 - σ(β·I))
    let mut new_act = vec![0.0_f32; self.num_nodes];
    for i in 0..self.num_nodes {
        let gate = 1.0 / (1.0 + (-self.gate_beta[i]).exp()); // σ(β·I)
        new_act[i] = excitation[i] * (1.0 - gate);
    }
    // Clamp (excitation can still overflow on dense graphs)
    for a in &mut new_act { *a = a.clamp(-1.0, 1.0); }
    new_act
}
```

3. **`hippocampus/mod.rs`**: Pre-compute `gate_beta: Vec<f32>` during `build()`
   from in-degrees.

4. **`hippocampus/mod.rs`**: Add a `MergeMode` enum:
   - `Additive` (current behavior, for backward compat / ablation)
   - `Multiplicative` (new gated mode)
   - `AdaptiveMultiplicative` (with degree-normalized β)

5. **Merge rule** in `spreading_activation_opts`: currently uses abs-max merge.
   For multiplicative mode, the activation is always ≥ 0 (excitation × (1-gate)),
   so negative values no longer appear. The merge can switch to max().

### Experimental Design

3-way comparison on CausalEval (20 graphs × 7 classes = 140 questions):

| Mode | β | Expected C1 | Expected C4 | Expected C6 |
|---|---|---|---|---|
| Additive (current) | -0.3 fixed | 90% | 90% | 20% |
| Multiplicative (fixed β) | β₀ grid search | ≥85% | ≥90% | ? |
| Adaptive Multiplicative | β(v) = f(deg⁺) | ≥85% | ≥90% | ? |

**β sweep**: Run Multiplicative mode with β ∈ {0.5, 1.0, 2.0, 5.0, 10.0} to
find the optimal fixed value, then compare adaptive vs best-fixed.

**Theoretical prediction**: Adaptive should win on C6 (transfer) because
high-degree hub nodes (shared across tasks) need stronger gates to prevent
cross-task interference.

---

# Layer 2: Heterogeneous Online Weight Learning

## Problem

Edge weights are set once at write time (by the LLM extractor or distiller)
and never updated from observed outcomes. The SWR consolidation adjusts weights
globally (LTP/LTD), but this is unsupervised replay — it doesn't use the
actual prediction accuracy of individual edges.

## Solution: Per-Type Prediction Tracking

Each causal edge makes a **prediction**. When the agent acts again, the
prediction is verified:

| Edge type | Prediction | Correct when | Incorrect when |
|---|---|---|---|
| `caused(X→Y)` | "Doing X will cause Y" | Y happens after X | Y does NOT happen |
| `prevented(X→Y)` | "Doing X will prevent Y" | Y does NOT happen after X | Y DOES happen |
| `enabled(X→Y)` | "X is necessary for Y" | Y requires X (harder to verify) | Y happens without X |

The update rule is a tabular Q-update variant:

$$w_{t+1}(e) = w_t(e) + \eta \cdot \big(r(e) - w_t(e)\big)$$

where $r(e) = 1$ if prediction correct, $0$ if incorrect.

**Convergence**: Under stationary conditions (the causal relationship doesn't
change), $w_t \to r^*$ = the true prediction accuracy of the edge. Under
non-stationary conditions (the relationship changes), $w_t$ tracks a moving
average with window $\approx 1/\eta$.

**The prevented-edge asymmetry**: A `prevented` edge predicting "Y won't happen"
faces the **confirmation problem**: Y not happening is expected, so the edge is
"correct" by default. To avoid runaway confidence on untestable predictions:

- Only update when the agent **actually does X** (not when X is merely mentioned)
- Require Y to be **observable** (not just absent)
- Decay confidence over time if no new evidence arrives: $w \leftarrow w \cdot (1 - \lambda_{neglect})$

## Implementation Plan

### Code Changes

1. **`store/mod.rs`**: Add `prediction_correct: Option<bool>` and
   `last_verified_at: Option<i64>` columns to `causal_edges`.

2. **New module `learning.rs`**: The online update engine.

```rust
pub struct OnlineLearner {
    learning_rate: f32,      // η
    neglect_rate: f32,       // λ_neglect (decay when no new evidence)
}

impl OnlineLearner {
    /// Called when the agent performs action X and observes outcome.
    /// Updates all edges X→Y based on whether Y matches the prediction.
    pub fn verify_predictions(
        &self,
        graph: &mut CausalGraph,
        action_chunk_id: &str,
        observed_outcomes: &[String],
        now: i64,
    ) -> VerificationStats {
        // For each outgoing edge from the action node:
        //   - caused(X→Y): if Y ∈ observed → correct (↑w), else → incorrect (↓w)
        //   - prevented(X→Y): if Y ∉ observed → correct (↑w), else → incorrect (↓w)
        //   - enabled(X→Y): skip (requires causal reasoning to verify)
        //   - no_effect: skip
    }
}
```

3. **MCP tool integration**: After `record_decision`, automatically run
   `verify_predictions` on the previous decision's outgoing edges.

### Experimental Design

- **Agent trap-world**: Run 10 rounds, observe weight evolution curves.
  Prediction: prevented edges for "known traps" converge to high weights;
  spurious prevented edges (false positives) decay.

- **CausalEval with learned weights**: Replace fixed confidence with learned
  weights after multi-round simulation. Prediction: C4 (inhibition) should
  improve because true prevented edges get higher confidence.

- **Convergence analysis**: Plot edge weight trajectories over rounds.
  Show that the system distinguishes "true causal edges" (converge to ≥0.7)
  from "noise edges" (converge to ≤0.3).

---

# Layer 3: Quality-Preserving Graph Sparsification

## Problem

SWR consolidation (LTP/LTD/GC) is a hand-tuned heuristic. The triple-criterion
GC rule ("weak AND dormant AND zero-access") works empirically but has no
theoretical optimality guarantee. The question: **can we formalize forgetting
as an optimization problem and prove that our heuristic approximates it?**

## Formalization

**Problem (QPS: Quality-Preserving Sparsification)**:
Given a causal graph $G = (V, E, w)$ and a retrieval quality function
$\text{MAP}(G, Q)$ measured over question set $Q$:

$$\min_{E' \subseteq E} |E'| \quad \text{s.t.} \quad \text{MAP}(G', Q) \geq \text{MAP}(G, Q) - \epsilon$$

where $G' = (V, E', w)$ and $\epsilon$ is the quality tolerance.

**Submodularity hypothesis**: The retrieval quality $\text{MAP}$ is a
**monotone submodular** function of $E'$ (adding edges never decreases MAP,
but with diminishing returns). If this holds, the greedy algorithm achieves
$(1 - 1/e)$-approximation to the complement problem (maximizing edges removed
while preserving quality).

**Connection to existing GC rule**: The triple-criterion GC is a **budget-free
greedy**: instead of maximizing removed edges, it removes the weakest first
and stops when the graph is "sparse enough." This is equivalent to greedy
submodular minimization when the quality function is the coverage of seed→hop
paths.

## Implementation Plan

### Baselines

1. **Random pruning**: Remove random edges until target sparsity.
2. **Weight-based pruning** (current GC): Remove lowest-weight edges.
3. **Spectral sparsification**: Remove edges with lowest effective resistance
   (Spielman-Srivastava 2008).
4. **Coverage-based sparsification** (new): Remove edges that participate in
   the fewest retrieval paths (measured by CausalEval hit-rate contribution).

### Experiment

1. Build a dense graph from production data (2200+ edges).
2. For each sparsification strategy, sweep sparsity ratio from 100% → 10%.
3. At each ratio, run CausalEval and measure C1-C7 accuracy.
4. Plot **sparsity vs accuracy** curves for all strategies.
5. Analyze: Does coverage-based outperform weight-based? Is the quality drop
   graceful or cliff-like?

### Theoretical Analysis

- Prove (or disprove) that MAP is submodular in $E'$ for the specific
  spreading activation dynamics.
- If submodular: greedy achieves $(1-1/e)$ → write the bound.
- If not submodular: identify the violating structure (likely: removing one
  edge can break a multi-hop chain, causing a cliff).

---

# Dependency Graph and Timeline

```
Layer 1 (Gating)     ████████░░░░░░░░░░░░░░  2-3 weeks
Layer 2 (Learning)   ░░░░░░░░████████░░░░░░  3-4 weeks
Layer 3 (Sparsify)   ░░░░░░░░░░░░░░░░██████  3-4 weeks
Paper writing        ░░░░░░░░░░░░░░░░░░░░██  2 weeks
                                           Total: ~10-12 weeks
```

## Paper Structure

| Section | Content |
|---|---|
| 1. Introduction | Agent memory gap → CausalEval → inhibitory gating insight |
| 2. Related Work | HeLa-Mem, MemRL, graph sparsification, spreading activation |
| 3. CausalEval | Graph-grounded benchmark design, 7 capability classes |
| 4. Inhibitory Gating | Additive failure theorem → multiplicative formula → adaptive β |
| 5. Online Learning | Per-type update rules → convergence proof → trap-world results |
| 6. Sparsification | QPS formalization → submodular analysis → sparsity-accuracy curves |
| 7. Experiments | CausalEval (primary), fact-recall (secondary), ablations |
| 8. Discussion | Limitations (C6 transfer, C7 update), future work |

---

# Appendix: Current Activation Math (to be replaced)

```
Current (additive):
  a(v) = Σ(caused: +1.0 × w × d^k) + Σ(enabled: +0.5 × w × d^k)
       + Σ(prevented: -0.3 × w × d^k) + Σ(fact: +0.8 × w × d^k)
       + Σ(meta: +0.6 × w × d^k) + Σ(co_occ: dynamic × w × d^k)

Proposed (multiplicative gating):
  E(v) = Σ(E+ edges: coeff × w × d^k)
  I(v) = σ(β(v) × Σ(E- edges: |coeff| × w × d^k))
  a(v) = E(v) × (1 - I(v))

  β(v) = β₀ × log(deg⁺(v)+1) / log(mean_deg⁺+1)   [adaptive]
  β(v) = β₀                                           [fixed]
```
