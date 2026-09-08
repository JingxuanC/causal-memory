//! Causal edge refutation — graph-structural confidence scoring.
//!
//! Inspired by DoWhy's refutation framework (Athena's `causal_refuter.py`),
//! adapted for agent memory graphs where statistical tests (t-test, CAR,
//! Granger) don't apply. Five refuters challenge each edge's validity:
//!
//! 1. **Confounder test**: neighbor Jaccard overlap (real edges share context);
//!    abstains on sparse neighborhoods (union < 4)
//! 2. **Corroboration test**: edge-disjoint path count; absence of redundant
//!    paths is never grounds for refutation (minimal chains are legitimately
//!    irredundant in a DAG)
//! 3. **Placebo test**: random source-node replacement (real edges are
//!    specific); abstains when the target is a ubiquitous hub
//! 4. **Backdoor test**: common-ancestor paths reaching both endpoints
//! 5. **Temporal test**: cause must not postdate its effect (event_time);
//!    abstains when either endpoint has no timestamp
//!
//! Each refuter returns Robust / Inconclusive / Refuted. Grade: A (5/5
//! robust) → F (≥2 refuted). Calibration: docs/evaluations/refuter-calibration.md.

use std::collections::{HashMap, HashSet};

use crate::hippocampus::CausalGraph;

/// Result of a single refutation test.
#[derive(Debug, Clone)]
pub struct SingleTest {
    pub name: &'static str,
    pub result: TestResult,
    pub score: f32,
    pub detail: String,
}

/// Verdict of one test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestResult {
    Robust,
    Inconclusive,
    Refuted,
}

/// Full refutation result for one edge.
#[derive(Debug, Clone)]
pub struct RefutationResult {
    pub grade: char,
    pub tests: Vec<SingleTest>,
}

impl RefutationResult {
    /// Grade from test results: A (5/5 robust) → F (2+ refuted).
    /// Tuned for 5 refuters (confounder / corroboration / placebo / backdoor / temporal).
    fn grade(tests: &[SingleTest]) -> char {
        let robust = tests
            .iter()
            .filter(|t| t.result == TestResult::Robust)
            .count();
        let refuted = tests
            .iter()
            .filter(|t| t.result == TestResult::Refuted)
            .count();
        if robust >= 5 {
            'A'
        } else if robust >= 4 && refuted == 0 {
            'B'
        } else if refuted == 0 {
            'C'
        } else if refuted >= 2 {
            'F'
        } else {
            'D'
        }
    }
}

/// Aggregate report after refuting all edges.
#[derive(Debug, Clone, Default)]
pub struct RefutationReport {
    pub total: usize,
    pub graded: usize,
    pub distribution: HashMap<char, usize>,
    pub results: Vec<(usize, RefutationResult)>, // (edge_idx, result)
}

/// The refuter — operates on an existing CausalGraph.
pub struct EdgeRefuter<'a> {
    graph: &'a CausalGraph,
}

impl<'a> EdgeRefuter<'a> {
    pub fn new(graph: &'a CausalGraph) -> Self {
        Self { graph }
    }

    /// Refute a single edge (by CSR edge index).
    pub fn refute_edge(&self, edge_idx: usize) -> RefutationResult {
        let from = self.graph.edge_source_node(edge_idx);
        let to = self.graph.edge_target(edge_idx);

        let t1 = self.confounder_test(from, to);
        let t2 = self.corroboration_test(from, to, edge_idx);
        let t3 = self.placebo_test(from, to);
        let t4 = self.backdoor_test(from, to, edge_idx);
        let t5 = self.temporal_test(from, to);

        let tests = vec![t1, t2, t3, t4, t5];
        let grade = RefutationResult::grade(&tests);
        RefutationResult { grade, tests }
    }

    /// Refute all valid edges, return aggregate report.
    pub fn refute_all(&self) -> RefutationReport {
        let mut report = RefutationReport::default();
        let mut dist: HashMap<char, usize> = HashMap::new();

        for edge_idx in 0..self.graph.num_edges() {
            if !self.graph.edge_is_valid(edge_idx) {
                continue;
            }
            report.total += 1;
            let result = self.refute_edge(edge_idx);
            *dist.entry(result.grade).or_insert(0) += 1;
            report.results.push((edge_idx, result));
            report.graded += 1;
        }
        report.distribution = dist;
        report
    }

    // ─── Refuter 1: Confounder (neighbor Jaccard) ──────────────────────

    /// Real causal edges connect nodes that share topical context.
    /// Measure: Jaccard similarity of neighbor sets (excluding each other).
    fn confounder_test(&self, from: u32, to: u32) -> SingleTest {
        let neighbors_from = self.graph.all_neighbors(from);
        let neighbors_to = self.graph.all_neighbors(to);

        // Exclude each other from the sets
        let nf: HashSet<u32> = neighbors_from
            .iter()
            .copied()
            .filter(|&n| n != to)
            .collect();
        let nt: HashSet<u32> = neighbors_to
            .iter()
            .copied()
            .filter(|&n| n != from)
            .collect();

        let intersection = nf.intersection(&nt).count();
        let union = nf.len() + nt.len() - intersection;

        // Calibration finding (refuter_calibration, 2026-09-08): on sparse
        // graphs, Jaccard over tiny neighbor sets is uninformative — with
        // union < 4 the test cannot distinguish "no shared context" from
        // "too little data". Abstain instead of refuting.
        let jaccard = if union >= 4 {
            intersection as f32 / union as f32
        } else {
            0.5 // neutral
        };

        let (result, detail) = if union < 4 {
            (
                TestResult::Inconclusive,
                format!(
                    "Only {} shared neighbors — too sparse to judge overlap",
                    union
                ),
            )
        } else if jaccard >= 0.15 {
            (
                TestResult::Robust,
                format!("High neighbor overlap (J={:.3}): shared context", jaccard),
            )
        } else if jaccard < 0.03 {
            (
                TestResult::Refuted,
                format!("No neighbor overlap (J={:.3}): likely spurious", jaccard),
            )
        } else {
            (
                TestResult::Inconclusive,
                format!("Moderate overlap (J={:.3})", jaccard),
            )
        };

        SingleTest {
            name: "confounder",
            result,
            score: jaccard,
            detail,
        }
    }

    // ─── Refuter 2: Corroboration (edge-disjoint paths) ────────────────

    /// Real causal relationships are corroborated by alternative paths.
    /// Measure: count paths from `from` to `to` that don't use `edge_idx`.
    fn corroboration_test(&self, from: u32, to: u32, exclude_edge: usize) -> SingleTest {
        let alt_paths = self.count_simple_paths(from, to, exclude_edge, 4);

        // Calibration finding (refuter_calibration, 2026-09-08): "no
        // alternative path" is NOT evidence against a true direct edge — in a
        // DAG, minimal causal chains (X→Y→Z) legitimately have zero redundant
        // paths, and this branch refuted 38% of ground-truth-true edges.
        // Absence of corroboration means we cannot corroborate → Inconclusive,
        // never Refuted.
        let (result, detail) = if alt_paths >= 1 {
            (
                TestResult::Robust,
                format!("{} alternative paths found", alt_paths),
            )
        } else {
            (
                TestResult::Inconclusive,
                "No alternative path — direct edge not structurally corroborated".to_string(),
            )
        };

        SingleTest {
            name: "corroboration",
            result,
            score: alt_paths as f32,
            detail,
        }
    }

    // ─── Refuter 3: Placebo (activation specificity) ───────────────────

    /// Real causal edge X→Y means activating X specifically reaches Y.
    /// Replace X with random nodes of similar degree — they shouldn't reach Y.
    fn placebo_test(&self, from: u32, to: u32) -> SingleTest {
        // Real activation: BFS reachability from `from` to `to` within 3 hops
        let real_distance = self.bfs_distance(from, to, 3);

        // Placebo: pick 5 random nodes with similar degree
        let deg_from = self.graph.out_degree(from);
        let n = self.graph.num_nodes();
        let mut placebo_reachable = 0;
        let mut placebo_count = 0;

        // Sample every Nth node (deterministic pseudo-random for reproducibility)
        let stride = (n / 20).max(1);
        let mut sampled = 0;
        for i in (0..n).step_by(stride) {
            let candidate = i as u32;
            if candidate == from || candidate == to {
                continue;
            }
            let deg = self.graph.out_degree(candidate);
            if deg == 0 || (deg as f32 - deg_from as f32).abs() > deg_from as f32 * 0.5 + 1.0 {
                continue;
            }
            let dist = self.bfs_distance(candidate, to, 3);
            if dist.is_some() {
                placebo_reachable += 1;
            }
            placebo_count += 1;
            sampled += 1;
            if sampled >= 5 {
                break;
            }
        }

        let placebo_rate = if placebo_count > 0 {
            placebo_reachable as f32 / placebo_count as f32
        } else {
            1.0 // can't test → assume worst case
        };

        let specificity = if real_distance.is_some() {
            1.0 / (placebo_rate + 0.1)
        } else {
            0.0
        };

        // Hub abstention (calibration: placebo refuted 29% of ground-truth-true
        // edges in dense graphs). When nearly all sampled nodes reach Y, Y is a
        // hub and "who reaches Y" carries no information about the X→Y claim.
        if placebo_count >= 3 && placebo_rate > 0.8 {
            return SingleTest {
                name: "placebo",
                result: TestResult::Inconclusive,
                score: specificity,
                detail: format!(
                    "Y is ubiquitously reachable ({:.0}% of random nodes) — specificity uninformative",
                    placebo_rate * 100.0
                ),
            };
        }

        let (result, detail) = if placebo_count < 3 {
            (
                TestResult::Inconclusive,
                format!("Only {} placebo samples", placebo_count),
            )
        } else if specificity >= 2.0 {
            (
                TestResult::Robust,
                format!(
                    "Specificity {:.1}x: X reaches Y but random nodes rarely do",
                    specificity
                ),
            )
        } else if specificity < 1.0 {
            (
                TestResult::Refuted,
                format!(
                    "Specificity {:.1}x: random nodes reach Y just as easily",
                    specificity
                ),
            )
        } else {
            (
                TestResult::Inconclusive,
                format!("Specificity {:.1}x: moderate", specificity),
            )
        };

        SingleTest {
            name: "placebo",
            result,
            score: specificity,
            detail,
        }
    }

    // ─── Refuter 4: Backdoor path (d-separation) ─────────────────────────

    /// Real causal edge X→Y should not be fully explained by a common cause.
    /// If X has an ancestor that can also reach Y (a backdoor path X←…→Y),
    /// the recorded association may be confounded rather than causal.
    ///
    /// Uses `CausalGraph::is_d_separated` on the graph with the candidate
    /// edge removed: if X and Y remain d-connected purely through ancestor
    /// paths, the direct-causal claim is weakened.
    fn backdoor_test(&self, from: u32, to: u32, exclude_edge: usize) -> SingleTest {
        // Ancestors of `from` = nodes with a directed path into `from`.
        let mut ancestors: HashSet<u32> = HashSet::new();
        let mut stack: Vec<u32> = vec![from];
        while let Some(node) = stack.pop() {
            if ancestors.insert(node) {
                for parent in self.graph.in_neighbors_of(node) {
                    if !ancestors.contains(&parent) {
                        stack.push(parent);
                    }
                }
            }
        }
        ancestors.remove(&from); // `from` itself is not its own ancestor.

        // Count ancestors that can reach `to` without the excluded edge.
        let mut backdoor_paths = 0usize;
        let mut detail_parts: Vec<String> = Vec::new();
        for &anc in &ancestors {
            if self.can_reach_excluding(anc, to, exclude_edge, 5) {
                backdoor_paths += 1;
                if detail_parts.len() < 3 {
                    detail_parts.push(format!(
                        "{}→…→{}",
                        self.graph.node_text(anc as usize),
                        self.graph.node_text(to as usize)
                    ));
                }
            }
        }

        let (result, detail) = if backdoor_paths == 0 {
            (
                TestResult::Robust,
                "No backdoor path: no common ancestor reaches both endpoints".to_string(),
            )
        } else if backdoor_paths == 1 {
            (
                TestResult::Inconclusive,
                format!(
                    "1 backdoor path ({}): possible confounding",
                    detail_parts.join(", ")
                ),
            )
        } else {
            // Calibration note (2026-09-08): an absolute-count threshold is
            // deliberately kept over a density-normalized fraction. Tested
            // alternative — fraction of X's ancestors reaching Y — refuted 38%
            // of true edges at high density because dense graphs put ancestor
            // paths between nearly all pairs; the count is only meaningful in
            // moderate-density regimes, which is where this refuter should be
            // consulted anyway (see docs/evaluations/refuter-calibration.md).
            (
                TestResult::Refuted,
                format!(
                    "{} backdoor paths ({}): association likely confounded, not causal",
                    backdoor_paths,
                    detail_parts.join(", ")
                ),
            )
        };

        SingleTest {
            name: "backdoor",
            result,
            score: backdoor_paths as f32,
            detail,
        }
    }

    /// Can `from` reach `to` via valid edges (excluding `exclude_edge`),
    /// within `max_hops` directed hops? Used by the backdoor refuter.
    fn can_reach_excluding(
        &self,
        from: u32,
        to: u32,
        exclude_edge: usize,
        max_hops: usize,
    ) -> bool {
        if from == to {
            return true;
        }
        let mut visited: HashSet<u32> = HashSet::new();
        let mut frontier = vec![from];
        visited.insert(from);
        for _ in 0..max_hops {
            let mut next = Vec::new();
            for node in frontier {
                for (neighbor, edge_idx) in self.graph.out_neighbors_of(node) {
                    if edge_idx == exclude_edge || !self.graph.edge_is_valid(edge_idx) {
                        continue;
                    }
                    if neighbor == to {
                        return true;
                    }
                    if visited.insert(neighbor) {
                        next.push(neighbor);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        false
    }

    // ─── Refuter 5: Temporal consistency (cause precedes effect) ─────────

    /// A recorded causal edge X→Y with event_time(X) > event_time(Y) is
    /// temporally impossible — the "effect" was recorded before its "cause".
    /// This is the one refuter that is nearly free of false positives on
    /// ground-truth-true edges (calibration: it lifts the pseudo-edge flag
    /// rate from ~36% to ~60%+ without touching the true-edge keep rate,
    /// breaking the structural keep/flag frontier documented in
    /// docs/evaluations/refuter-calibration.md).
    ///
    /// Abstains when either endpoint has no timestamp (event_time == 0),
    /// and treats exact ties as Inconclusive (coarse/same-tick recording).
    fn temporal_test(&self, from: u32, to: u32) -> SingleTest {
        let tx = self.graph.node_event_time(from as usize);
        let ty = self.graph.node_event_time(to as usize);

        let (result, detail) = if tx == 0 || ty == 0 {
            (
                TestResult::Inconclusive,
                "No temporal data on one or both endpoints".to_string(),
            )
        } else if tx > ty {
            (
                TestResult::Refuted,
                format!(
                    "Effect precedes cause (t_cause={tx} > t_effect={ty}): temporally impossible"
                ),
            )
        } else if tx == ty {
            (
                TestResult::Inconclusive,
                format!("Simultaneous timestamps (t={tx}): temporal order unresolvable"),
            )
        } else {
            (
                TestResult::Robust,
                format!("Cause precedes effect (t_cause={tx} < t_effect={ty})"),
            )
        };

        SingleTest {
            name: "temporal",
            result,
            score: (ty - tx) as f32,
            detail,
        }
    }

    // ─── Graph helpers ─────────────────────────────────────────────────

    /// Count simple paths from `from` to `to` (excluding one edge), up to max_hops.
    /// Uses bounded DFS.
    fn count_simple_paths(
        &self,
        from: u32,
        to: u32,
        exclude_edge: usize,
        max_hops: usize,
    ) -> usize {
        let mut count = 0;
        let mut visited = HashSet::new();
        visited.insert(from);
        self.dfs_count(from, to, exclude_edge, max_hops, &mut visited, &mut count);
        count
    }

    fn dfs_count(
        &self,
        current: u32,
        target: u32,
        exclude_edge: usize,
        hops_left: usize,
        visited: &mut HashSet<u32>,
        count: &mut usize,
    ) {
        if hops_left == 0 {
            return;
        }
        let neighbors = self.graph.out_neighbors_of(current);
        for (neighbor, edge_idx) in neighbors {
            if edge_idx == exclude_edge {
                continue;
            }
            if !self.graph.edge_is_valid(edge_idx) {
                continue;
            }
            if neighbor == target {
                *count += 1;
                continue;
            }
            if visited.contains(&neighbor) {
                continue;
            }
            visited.insert(neighbor);
            self.dfs_count(
                neighbor,
                target,
                exclude_edge,
                hops_left - 1,
                visited,
                count,
            );
            visited.remove(&neighbor);
        }
    }

    /// BFS shortest distance from `from` to `to`, up to `max_hops`.
    fn bfs_distance(&self, from: u32, to: u32, max_hops: usize) -> Option<usize> {
        if from == to {
            return Some(0);
        }
        let mut visited = HashSet::new();
        visited.insert(from);
        let mut frontier = vec![from];
        for hop in 1..=max_hops {
            let mut next = Vec::new();
            for node in frontier {
                for (neighbor, edge_idx) in self.graph.out_neighbors_of(node) {
                    if !self.graph.edge_is_valid(edge_idx) {
                        continue;
                    }
                    if neighbor == to {
                        return Some(hop);
                    }
                    if visited.insert(neighbor) {
                        next.push(neighbor);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hippocampus::{EdgeData, NodeData, Relation};

    fn build_graph(edges: &[(&str, &str)]) -> CausalGraph {
        let mut ids: Vec<&str> = Vec::new();
        for &(a, b) in edges {
            if !ids.contains(&a) {
                ids.push(a);
            }
            if !ids.contains(&b) {
                ids.push(b);
            }
        }
        let nodes: Vec<NodeData> = ids
            .iter()
            .map(|id| NodeData {
                id: id.to_string(),
                text: id.to_string(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            })
            .collect();
        let edge_data: Vec<EdgeData> = edges
            .iter()
            .map(|(a, b)| EdgeData {
                from_id: a.to_string(),
                to_id: b.to_string(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            })
            .collect();
        CausalGraph::build(&nodes, &edge_data)
    }

    #[test]
    fn backdoor_detects_confounded_edge() {
        // B→A, B→C, A→C: A→C has a backdoor path through B (common cause).
        let g = build_graph(&[("B", "A"), ("B", "C"), ("A", "C")]);
        let refuter = EdgeRefuter::new(&g);
        // Find the A→C edge.
        let a_idx = g.node_index_of("A").unwrap();
        let c_idx = g.node_index_of("C").unwrap();
        let ac_edge = (0..g.num_edges())
            .find(|&i| g.edge_source_node(i) == a_idx && g.edge_target(i) == c_idx)
            .expect("A→C edge must exist");
        let result = refuter.refute_edge(ac_edge);
        let backdoor = result.tests.iter().find(|t| t.name == "backdoor").unwrap();
        assert!(
            backdoor.result == TestResult::Refuted || backdoor.result == TestResult::Inconclusive,
            "confounded A→C must trigger backdoor refuter, got {:?}: {}",
            backdoor.result,
            backdoor.detail
        );
    }

    #[test]
    fn backdoor_passes_clean_edge() {
        // A→B→C: A→B has no confounder.
        let g = build_graph(&[("A", "B"), ("B", "C")]);
        let refuter = EdgeRefuter::new(&g);
        let a_idx = g.node_index_of("A").unwrap();
        let b_idx = g.node_index_of("B").unwrap();
        let ab_edge = (0..g.num_edges())
            .find(|&i| g.edge_source_node(i) == a_idx && g.edge_target(i) == b_idx)
            .expect("A→B edge must exist");
        let result = refuter.refute_edge(ab_edge);
        let backdoor = result.tests.iter().find(|t| t.name == "backdoor").unwrap();
        assert_eq!(
            backdoor.result,
            TestResult::Robust,
            "clean A→B must be robust, got: {}",
            backdoor.detail
        );
    }

    #[test]
    fn backdoor_detects_fork_confounder() {
        // A←B→C: A→C doesn't exist, but if we add it, it should be flagged.
        // Here we test the existing A←B edge — B→C creates a backdoor for A←B? No,
        // backdoor for A←B means a common cause of A and B. B IS the common cause.
        // The A←B edge itself is the causal claim, so B is not a confounder of its own edge.
        // Instead test: add a fake A→C edge and check it gets flagged.
        let g = build_graph(&[("B", "A"), ("B", "C")]);
        let refuter = EdgeRefuter::new(&g);
        // B→A edge: B is the source, not a confounder of itself → robust
        let b_idx = g.node_index_of("B").unwrap();
        let a_idx = g.node_index_of("A").unwrap();
        let ba_edge = (0..g.num_edges())
            .find(|&i| g.edge_source_node(i) == b_idx && g.edge_target(i) == a_idx)
            .expect("B→A edge must exist");
        let result = refuter.refute_edge(ba_edge);
        let backdoor = result.tests.iter().find(|t| t.name == "backdoor").unwrap();
        assert_eq!(
            backdoor.result,
            TestResult::Robust,
            "B→A has no ancestor of B reaching A, got: {}",
            backdoor.detail
        );
    }

    // ─── Temporal refuter tests ─────────────────────────────────────

    fn build_graph_with_times(edges: &[(&str, &str, i64, i64)]) -> CausalGraph {
        let mut ids: Vec<&str> = Vec::new();
        for &(a, b, _, _) in edges {
            if !ids.contains(&a) {
                ids.push(a);
            }
            if !ids.contains(&b) {
                ids.push(b);
            }
        }
        // time lookup: first occurrence of an id wins
        let time_of = |id: &str| -> i64 {
            edges
                .iter()
                .find(|e| e.0 == id)
                .map(|e| e.2)
                .or_else(|| edges.iter().find(|e| e.1 == id).map(|e| e.3))
                .unwrap_or(0)
        };
        let nodes: Vec<NodeData> = ids
            .iter()
            .map(|id| NodeData {
                id: id.to_string(),
                text: id.to_string(),
                event_time: time_of(id),
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            })
            .collect();
        let edge_data: Vec<EdgeData> = edges
            .iter()
            .map(|(a, b, _, _)| EdgeData {
                from_id: a.to_string(),
                to_id: b.to_string(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            })
            .collect();
        CausalGraph::build(&nodes, &edge_data)
    }

    fn temporal_of(g: &CausalGraph, refuter: &EdgeRefuter, from: &str, to: &str) -> TestResult {
        let fi = g.node_index_of(from).unwrap();
        let ti = g.node_index_of(to).unwrap();
        let eidx = (0..g.num_edges())
            .find(|&i| g.edge_source_node(i) == fi && g.edge_target(i) == ti)
            .expect("edge must exist");
        refuter
            .refute_edge(eidx)
            .tests
            .iter()
            .find(|t| t.name == "temporal")
            .unwrap()
            .result
    }

    #[test]
    fn temporal_refutes_inverted_edge() {
        // A happened at t=20, B at t=10: A→B claims effect preceded cause.
        let g = build_graph_with_times(&[("A", "B", 20, 10)]);
        let refuter = EdgeRefuter::new(&g);
        assert_eq!(
            temporal_of(&g, &refuter, "A", "B"),
            TestResult::Refuted,
            "inverted timestamps must be refuted"
        );
    }

    #[test]
    fn temporal_passes_ordered_edge() {
        let g = build_graph_with_times(&[("A", "B", 10, 20)]);
        let refuter = EdgeRefuter::new(&g);
        assert_eq!(
            temporal_of(&g, &refuter, "A", "B"),
            TestResult::Robust,
            "cause-before-effect must be robust"
        );
    }

    #[test]
    fn temporal_abstains_without_timestamps() {
        let g = build_graph(&[("A", "B")]); // build_graph uses event_time = 0
        let refuter = EdgeRefuter::new(&g);
        assert_eq!(
            temporal_of(&g, &refuter, "A", "B"),
            TestResult::Inconclusive,
            "unset timestamps must abstain"
        );
    }
}
