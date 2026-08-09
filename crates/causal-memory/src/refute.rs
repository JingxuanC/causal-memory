//! Causal edge refutation — graph-structural confidence scoring.
//!
//! Inspired by DoWhy's refutation framework (Athena's `causal_refuter.py`),
//! adapted for agent memory graphs where statistical tests (t-test, CAR,
//! Granger) don't apply. Three graph-structural refuters challenge each
//! edge's validity:
//!
//! 1. **Confounder test**: neighbor Jaccard overlap (real edges share context)
//! 2. **Corroboration test**: edge-disjoint path count (real edges have redundancy)
//! 3. **Placebo test**: random source-node replacement (real edges are specific)
//!
//! Each refuter returns Robust / Inconclusive / Refuted. An edge passing
//! ≥2 tests gets grade A/B; failing ≥2 gets D/F.

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
    /// Grade from test results: A (3/3 robust) → F (2+ refuted).
    fn grade(tests: &[SingleTest]) -> char {
        let robust = tests.iter().filter(|t| t.result == TestResult::Robust).count();
        let refuted = tests.iter().filter(|t| t.result == TestResult::Refuted).count();
        if robust >= 3 { 'A' }
        else if robust >= 2 && refuted == 0 { 'B' }
        else if refuted == 0 { 'C' }
        else if refuted >= 2 { 'F' }
        else { 'D' }
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

        let tests = vec![t1, t2, t3];
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
        let nf: HashSet<u32> = neighbors_from.iter().copied().filter(|&n| n != to).collect();
        let nt: HashSet<u32> = neighbors_to.iter().copied().filter(|&n| n != from).collect();

        let intersection = nf.intersection(&nt).count();
        let union = nf.len() + nt.len() - intersection;
        let jaccard = if union > 0 {
            intersection as f32 / union as f32
        } else {
            0.0
        };

        let (result, detail) = if jaccard >= 0.15 {
            (TestResult::Robust, format!("High neighbor overlap (J={:.3}): shared context", jaccard))
        } else if jaccard < 0.03 {
            (TestResult::Refuted, format!("No neighbor overlap (J={:.3}): likely spurious", jaccard))
        } else {
            (TestResult::Inconclusive, format!("Moderate overlap (J={:.3})", jaccard))
        };

        SingleTest { name: "confounder", result, score: jaccard, detail }
    }

    // ─── Refuter 2: Corroboration (edge-disjoint paths) ────────────────

    /// Real causal relationships are corroborated by alternative paths.
    /// Measure: count paths from `from` to `to` that don't use `edge_idx`.
    fn corroboration_test(&self, from: u32, to: u32, exclude_edge: usize) -> SingleTest {
        let alt_paths = self.count_simple_paths(from, to, exclude_edge, 4);

        let in_degree_to = self.graph.in_degree(to);

        let (result, detail) = if alt_paths >= 1 {
            (TestResult::Robust, format!("{} alternative paths found", alt_paths))
        } else if in_degree_to >= 2 {
            (TestResult::Inconclusive, format!("No alt path, but in-degree={}", in_degree_to))
        } else {
            (TestResult::Refuted, "No alternative path and low in-degree — isolated edge".to_string())
        };

        SingleTest { name: "corroboration", result, score: alt_paths as f32, detail }
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

        let (result, detail) = if placebo_count < 3 {
            (TestResult::Inconclusive, format!("Only {} placebo samples", placebo_count))
        } else if specificity >= 2.0 {
            (TestResult::Robust, format!("Specificity {:.1}x: X reaches Y but random nodes rarely do", specificity))
        } else if specificity < 1.0 {
            (TestResult::Refuted, format!("Specificity {:.1}x: random nodes reach Y just as easily", specificity))
        } else {
            (TestResult::Inconclusive, format!("Specificity {:.1}x: moderate", specificity))
        };

        SingleTest { name: "placebo", result, score: specificity, detail }
    }

    // ─── Graph helpers ─────────────────────────────────────────────────

    /// Count simple paths from `from` to `to` (excluding one edge), up to max_hops.
    /// Uses bounded DFS.
    fn count_simple_paths(&self, from: u32, to: u32, exclude_edge: usize, max_hops: usize) -> usize {
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
            self.dfs_count(neighbor, target, exclude_edge, hops_left - 1, visited, count);
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
