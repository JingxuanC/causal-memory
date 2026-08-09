# Causal Edge Refutation: Real-Time Confidence Scoring for LLM-Extracted Causal Edges

> Inspired by Athena's DoWhy integration (`causal_refuter.py`) and Pearl's
> refutation framework. Adapted for agent memory graphs where statistical
> methods (t-test, CAR, Granger) don't apply — we use graph-structural tests
> instead.
>
> Date: 2026-08-10

## Problem

LLM-extracted causal edges have 83% accuracy (audited). The 17% errors are
indistinguishable from correct edges at write time — the system trusts every
`caused`/`prevented`/`enabled` label equally. This undermines every downstream
capability: spreading activation, intervention query, trace_cause.

Statistical refutation (DoWhy's t-test, placebo on treatment dates) requires
numerical outcome data — not available in text-based agent memory. We need
**graph-structural refutation**: tests that use the graph topology itself to
challenge each edge's validity.

## Design: Three Graph-Structural Refuters

Each refuter returns `robust` / `refuted` / `inconclusive`. An edge that
passes ≥2 of 3 gets grade A/B; failing ≥2 gets D/F.

### Refuter 1: Neighbor Overlap (Confounder Test)

**Hypothesis**: A real causal edge X→Y should be explained by X and Y's
neighborhood — X is a plausible cause of Y because they share topical/semantic
context. A spurious edge connects nodes with no overlapping context.

**Test**: Compute Jaccard similarity of the neighbor sets of X and Y
(excluding each other). Real edges have high neighbor overlap (they operate
in the same domain); spurious edges have near-zero overlap.

```
J(N(X), N(Y)) = |N(X) ∩ N(Y)| / |N(X) ∪ N(Y)|
```

- J ≥ 0.15 → `robust` (shared context)
- J < 0.03 → `refuted` (no shared context — likely spurious)
- Otherwise → `inconclusive`

**Why this works**: In agent memory, `Edit(hippocampus/mod.rs)` → `error: could not compile` share neighbors (same file, same build session). A spurious edge like `Bash(ls /tmp)` → `Edit(src/main.rs)` has no shared neighbors.

### Refuter 2: Path Redundancy (Corroboration Test)

**Hypothesis**: A real causal relationship should be corroborated by at least
one alternative path. If X→Y is the ONLY path from X to Y, it might be a
one-off coincidence; if there are multiple paths, the causal claim is
reinforced.

**Test**: Count the number of edge-disjoint paths from X to Y (excluding the
direct edge X→Y itself).

- ≥1 alternative path → `robust` (corroborated)
- 0 alternative paths, but in-degree(Y) ≥ 2 → `inconclusive` (Y has other causes but not from X's cluster)
- 0 alternative paths, in-degree(Y) ≤ 1 → `refuted` (isolated edge — no corroboration)

**Why this works**: If "deployed without tests → production crash" is real, there should also be paths like "deployed without tests → regression → production crash". A spurious edge has no such redundancy.

### Refuter 3: Activation Specificity (Placebo Test)

**Hypothesis**: A real causal edge X→Y means activating X should specifically
reach Y through this edge. If we replace X with a random node Z of similar
degree, Z should NOT reach Y with similar activation strength.

**Test**:
1. Run spreading activation from X, record Y's activation: `a_real`
2. Pick 5 random nodes Z with similar degree (±30% of deg(X))
3. Run spreading activation from each Z, record Y's activation: `a_placebo`
4. Compare: `specificity = a_real / (mean(a_placebo) + ε)`

- specificity ≥ 2.0 → `robust` (X activates Y 2x stronger than random)
- specificity < 1.0 → `refuted` (random nodes activate Y just as much)
- Otherwise → `inconclusive`

**Why this works**: This is the graph-analogue of Athena's placebo test. Athena replaces the treatment date with a random date; we replace the source node with a random node.

## Grading

| Grade | Condition | Meaning |
|---|---|---|
| A | 3/3 robust | High confidence — edge is almost certainly real |
| B | 2/3 robust, 0 refuted | Moderate confidence — likely real |
| C | 1/3 robust, 0 refuted | Uncertain — no positive or negative signal |
| D | 1+ refuted, 0 robust | Low confidence — likely spurious |
| F | 2+ refuted | Very low confidence — almost certainly wrong |

The grade is stored as a new column `refutation_grade` on `causal_edges`.
Retrieval can optionally filter by grade (e.g., `search_causal` only returns
grade A/B edges).

## Implementation Plan

### Schema Change

```sql
ALTER TABLE causal_edges ADD COLUMN refutation_grade TEXT;  -- A/B/C/D/F
ALTER TABLE causal_edges ADD COLUMN refutation_detail TEXT; -- JSON: per-test results
```

### New Module: `crates/causal-memory/src/refute.rs`

```rust
pub struct EdgeRefuter<'a> {
    graph: &'a CausalGraph,
}

impl<'a> EdgeRefuter<'a> {
    /// Run all 3 refuters on one edge, return grade + details.
    pub fn refute_edge(&self, from_idx: u32, to_idx: u32) -> RefutationResult {
        let r1 = self.confounder_test(from_idx, to_idx);
        let r2 = self.corroboration_test(from_idx, to_idx);
        let r3 = self.placebo_test(from_idx, to_idx);
        let grade = Self::grade(&[&r1, &r2, &r3]);
        RefutationResult { grade, tests: vec![r1, r2, r3] }
    }

    /// Refute all edges in the graph, return grade distribution.
    pub fn refute_all(&self) -> RefutationReport {
        // Iterate all valid edges, refute each, collect stats
    }
}

pub enum TestResult { Robust, Inconclusive, Refuted }

pub struct RefutationResult {
    pub grade: char,          // A/B/C/D/F
    pub tests: Vec<SingleTest>,
}

pub struct SingleTest {
    pub name: &'static str,   // "confounder" / "corroboration" / "placebo"
    pub result: TestResult,
    pub score: f32,           // Jaccard for confounder, path count for corroboration, etc.
    pub detail: String,       // Human-readable interpretation
}
```

### CLI Integration

```bash
# Refute all edges, print grade distribution
causal-memory refute

# Refute and store grades in the DB
causal-memory refute --store

# Search only using grade A/B edges
causal-memory search_causal --min-grade B "query"
```

### Retrieval Integration

`search_causal_bm25` and `search_causal_hop` add an optional `min_grade`
filter. Edges below the threshold are excluded from candidates before BM25
ranking. This means low-confidence edges don't pollute retrieval results.

## Expected Impact

### On Production DB (1300 edges)

Predicted grade distribution (based on 83% accuracy audit):
- ~1080 edges (83%): grade A/B (real edges, pass ≥2 tests)
- ~130 edges (10%): grade C (uncertain)
- ~90 edges (7%): grade D/F (likely spurious)

Filtering out D/F edges from retrieval should improve BM25 precision by
reducing noise candidates.

### On CausalEval

If we only propagate through grade A/B edges in the hippocampus, C4 (inhibition)
and C1 (attribution) should improve — spurious edges that currently dilute
the signal would be filtered.

### On the Paper

This is the answer to "how do you know the LLM's causal labels are correct?"
— we don't just trust them, we **refute** each edge with three independent
graph-structural tests, and only high-grade edges enter the retrieval pool.

## Complexity Analysis

| Refuter | Complexity | Notes |
|---|---|---|
| Confounder (neighbor Jaccard) | O(deg(X) + deg(Y)) | Single set operation |
| Corroboration (path count) | O(\|V\| + \|E\|) per edge | BFS with edge removal |
| Placebo (activation specificity) | O(5 × spreading_activation) | 5 random seeds × K-hop |

For the full graph (1300 edges): ~1300 × O(|V|+|E|) = manageable (seconds).

## Athena Parallel

| Athena (quantitative) | causal-memory (agent) |
|---|---|
| Placebo: random treatment date | Placebo: random source node |
| Random common cause: add noise to CAR series | Confounder: Jaccard of neighbor sets |
| Data subset: bootstrap 70% of samples | Corroboration: count edge-disjoint paths |
| Grade A/B/C/D/F based on p-values | Grade A/B/C/D/F based on test pass rate |
| Validates "event → stock price" causality | Validates "decision → outcome" causality |

Same framework, different domain, different test mechanisms — but the
**philosophy is identical**: don't trust the causal claim, try to refute it.
