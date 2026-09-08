//! Refuter calibration harness — synthetic ground-truth evaluation of the
//! four refuters (confounder / corroboration / placebo / backdoor).
//!
//! Methodology:
//! 1. Generate random DAGs with **planted community structure** (seeded PRNG,
//!    reproducible). Communities model the semantic assumption behind the
//!    confounder refuter: real causal edges connect topically-related nodes,
//!    which tend to share neighbors; pseudo edges (extractor hallucinations,
//!    spurious correlations) are uniform-random, mostly cross-community.
//! 2. True-causal edges = DAG edges. Pseudo-causal edges = random non-adjacent
//!    node pairs, classified "hard" (d-connected to target via forward path
//!    or common ancestor — graph structure alone can't trivially reject) or
//!    "easy" (d-separated).
//! 3. Run EdgeRefuter on every edge, tally per-refuter outcomes per class.
//! 4. Report operational metrics for a memory-pruning policy:
//!      keep_rate  = true edges with grade A/B/C (zero refuters object)
//!      quarantine = pseudo edges with grade F (≥2 refuters refute → drop)
//!      flag_rate  = pseudo edges with grade D/F (≥1 refuter → review queue)
//!
//! History: an earlier version used pure random DAGs (no communities). That
//! benchmark was dishonest — it lacked the shared-context structure the
//! confounder refuter presumes, so the confounder test fired on ~65% of TRUE
//! edges (survival 19.6%) while also producing the detection rate, proving
//! only that the test is uninformative on structureless graphs. Community
//! planting restores a fair test of each refuter's discriminative power.

use causal_memory::hippocampus::{CausalGraph, EdgeData, NodeData, Relation};
use causal_memory::refute::{EdgeRefuter, TestResult};
use std::collections::{HashMap, HashSet};

// ─── Seeded PRNG (SplitMix64 — deterministic, no external deps) ───────────

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
    fn chance(&mut self, pct: u64) -> bool { self.next() % 100 < pct }
}

// ─── Synthetic DAG generator (planted communities) ─────────────────────────

/// A synthetic world with known ground truth.
struct SynthWorld {
    /// DAG edges = true causal relationships.
    true_edges: Vec<(usize, usize)>,
    /// Pseudo-causal edges (not in DAG) — what a bad extractor might record.
    /// Each tagged with whether it's d-connected ("hard") or not ("easy").
    pseudo_edges: Vec<(usize, usize, bool)>,
    node_count: usize,
    /// Community id per node (planted semantic context).
    community: Vec<usize>,
    /// Per-node timestamp jitter in [0, 5); times stay strictly increasing
    /// along the topological index so all true edges are temporally consistent.
    time_jitter: Vec<i64>,
}

impl SynthWorld {
    /// Generate a random DAG with `n` nodes, planted `k` communities.
    ///
    /// True edges: within-community pairs get `p_in`%, cross-community `p_out`%.
    /// Lower indices = earlier in topological order (indices ARE the order).
    fn generate(rng: &mut Rng, n: usize, k: usize, p_in: u64, p_out: u64) -> Self {
        let community: Vec<usize> = (0..n).map(|_| rng.below(k)).collect();

        // True edges: prob depends on community co-membership.
        let mut true_edges: HashSet<(usize, usize)> = HashSet::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let p = if community[i] == community[j] { p_in } else { p_out };
                if rng.chance(p) {
                    true_edges.insert((i, j));
                }
            }
        }
        let true_edges: Vec<(usize, usize)> = true_edges.into_iter().collect();

        // Pseudo edges: uniform random non-adjacent pairs (mostly cross-community).
        // CRITICAL: keep the raw (a, b) orientation — do NOT sort by topological
        // index. A hallucinating extractor proposes A→B regardless of which was
        // recorded first, so ~half of pseudo edges are temporally inverted and
        // carry separable temporal evidence for the temporal refuter.
        let mut pseudo_edges = Vec::new();
        let pseudo_count = true_edges.len().max(4); // 1:1 ratio, floor of 4
        let mut attempts = 0;
        while pseudo_edges.len() < pseudo_count && attempts < pseudo_count * 20 {
            attempts += 1;
            let a = rng.below(n);
            let b = rng.below(n);
            if a == b { continue; }
            if a < b && true_edges.contains(&(a, b)) { continue; }
            if a > b && true_edges.contains(&(b, a)) { continue; }
            pseudo_edges.push((a, b, false)); // hardness classified below
        }

        let time_jitter: Vec<i64> = (0..n).map(|_| rng.below(5) as i64).collect();
        let mut world = Self { true_edges, pseudo_edges, node_count: n, community, time_jitter };
        world.classify_pseudo();
        world
    }

    /// Classify each pseudo edge as hard (d-connected to its target through
    /// the existing true-edge graph) or easy (d-separated).
    fn classify_pseudo(&mut self) {
        // Forward adjacency.
        let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
        for &(f, t) in &self.true_edges {
            adj.entry(f).or_default().push(t);
        }
        // Ancestor sets via reverse reachability.
        let mut ancestors: HashMap<usize, HashSet<usize>> = HashMap::new();
        for node in 0..self.node_count {
            let mut anc = HashSet::new();
            let mut stack: Vec<usize> = vec![node];
            let mut visited = HashSet::new();
            while let Some(cur) = stack.pop() {
                if !visited.insert(cur) { continue; }
                for &(f, t) in &self.true_edges {
                    if t == cur {
                        anc.insert(f);
                        if !visited.contains(&f) { stack.push(f); }
                    }
                }
            }
            ancestors.insert(node, anc);
        }
        // Forward reachability closure.
        let mut reach: HashMap<usize, HashSet<usize>> = HashMap::new();
        for node in 0..self.node_count {
            let mut r = HashSet::new();
            let mut stack = vec![node];
            while let Some(cur) = stack.pop() {
                if let Some(nexts) = adj.get(&cur) {
                    for &nx in nexts {
                        if r.insert(nx) { stack.push(nx); }
                    }
                }
            }
            reach.insert(node, r);
        }

        for pe in self.pseudo_edges.iter_mut() {
            let (from, to, _) = *pe;
            // Hard if: (a) forward path exists (mediated alternative), or
            //          (b) common ancestor exists (backdoor confounding path).
            let fwd = reach.get(&from).map_or(false, |r| r.contains(&to));
            let backdoor = ancestors.get(&from)
                .map_or(false, |af| ancestors.get(&to)
                    .map_or(false, |at| af.iter().any(|a| at.contains(a))));
            pe.2 = fwd || backdoor;
        }
    }

    fn make_nodes(&self) -> Vec<NodeData> {
        (0..self.node_count)
            .map(|i| NodeData {
                id: format!("n{i}"),
                text: format!("node {i}"),
                // Strictly increasing timestamps along the topological index:
                // every true edge is temporally consistent, and pseudo edges
                // (kept in raw random orientation, see generate) are inverted
                // ~half the time — planting separable temporal evidence.
                event_time: (i as i64 + 1) * 10 + self.time_jitter[i],
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            })
            .collect()
    }

    fn make_edge(from: usize, to: usize) -> EdgeData {
        EdgeData {
            from_id: format!("n{from}"),
            to_id: format!("n{to}"),
            relation: Relation::Caused,
            weight: 1.0,
            valid: true,
        }
    }

    /// Build the CausalGraph from true edges only (pseudo edges are added
    /// separately so each can be refuted individually).
    fn build_graph(&self) -> CausalGraph {
        let nodes = self.make_nodes();
        let edges: Vec<EdgeData> = self.true_edges
            .iter()
            .map(|&(f, t)| Self::make_edge(f, t))
            .collect();
        CausalGraph::build(&nodes, &edges)
    }
}

// ─── Calibration stats ────────────────────────────────────────────────────

#[derive(Default, Debug)]
struct RefuterStats {
    /// (robust, inconclusive, refuted) per class, "worst refuter" per edge.
    true_causal: [usize; 3],
    pseudo_hard: [usize; 3],
    pseudo_easy: [usize; 3],
    /// Grades per class (A=4 robust, B=3, C=0 refuted, D=1 refuted, F=≥2 refuted).
    grades_true: HashMap<char, usize>,
    grades_pseudo: HashMap<char, usize>,
}

fn bucket(r: TestResult) -> usize {
    match r { TestResult::Robust => 0, TestResult::Inconclusive => 1, TestResult::Refuted => 2 }
}

// ─── The calibration test ─────────────────────────────────────────────────

#[test]
fn refuter_calibration_synthetic() {
    let mut rng = Rng::new(42);
    let world_count = 30;
    let mut all_stats = RefuterStats::default();
    // [true_r, true_i, true_f, pseudo_r, pseudo_i, pseudo_f]
    let mut per_refuter: HashMap<&str, [usize; 6]> = HashMap::new();

    for _ in 0..world_count {
        let n = 6 + rng.below(8);            // 6–13 nodes
        let k = 2 + rng.below(2);            // 2–3 communities (bigger clusters)
        let world = SynthWorld::generate(&mut rng, n, k, 40, 12);

        // Refute all TRUE edges on the clean graph.
        let g_true = world.build_graph();
        let refuter = EdgeRefuter::new(&g_true);
        for &(f, t) in &world.true_edges {
            let fi = g_true.node_index_of(&format!("n{f}")).unwrap();
            let ti = g_true.node_index_of(&format!("n{t}")).unwrap();
            let eidx = (0..g_true.num_edges())
                .find(|&i| g_true.edge_source_node(i) == fi && g_true.edge_target(i) == ti);
            let Some(eidx) = eidx else { continue };
            let r = refuter.refute_edge(eidx);
            for test in &r.tests {
                let entry = per_refuter.entry(test.name).or_insert([0; 6]);
                entry[bucket(test.result)] += 1;
            }
            let worst = r.tests.iter().map(|t| bucket(t.result)).max().unwrap_or(0);
            all_stats.true_causal[worst] += 1;
            *all_stats.grades_true.entry(r.grade).or_insert(0) += 1;
        }

        // Refute each PSEUDO edge by adding it to the graph temporarily.
        for &(f, t, hard) in &world.pseudo_edges {
            let nodes = world.make_nodes();
            let mut edges: Vec<EdgeData> = world.true_edges
                .iter()
                .map(|&(af, at)| SynthWorld::make_edge(af, at))
                .collect();
            edges.push(SynthWorld::make_edge(f, t));
            let g = CausalGraph::build(&nodes, &edges);

            let refuter = EdgeRefuter::new(&g);
            let fi = g.node_index_of(&format!("n{f}")).unwrap();
            let ti = g.node_index_of(&format!("n{t}")).unwrap();
            let eidx = (0..g.num_edges())
                .find(|&i| g.edge_source_node(i) == fi && g.edge_target(i) == ti);
            let Some(eidx) = eidx else { continue };
            let r = refuter.refute_edge(eidx);
            for test in &r.tests {
                let entry = per_refuter.entry(test.name).or_insert([0; 6]);
                entry[3 + bucket(test.result)] += 1;
            }
            let worst = r.tests.iter().map(|t| bucket(t.result)).max().unwrap_or(0);
            if hard { all_stats.pseudo_hard[worst] += 1; }
            else { all_stats.pseudo_easy[worst] += 1; }
            *all_stats.grades_pseudo.entry(r.grade).or_insert(0) += 1;
        }
    }

    // ── Report ──
    println!("\n══════ REFUTER CALIBRATION ({} worlds, planted communities) ══════", world_count);
    println!("\nAggregate (worst refuter verdict per edge):");
    println!("  True causal:  Robust={} Inconc={} Refuted={}",
        all_stats.true_causal[0], all_stats.true_causal[1], all_stats.true_causal[2]);
    println!("  Pseudo hard:  Robust={} Inconc={} Refuted={}",
        all_stats.pseudo_hard[0], all_stats.pseudo_hard[1], all_stats.pseudo_hard[2]);
    println!("  Pseudo easy:  Robust={} Inconc={} Refuted={}",
        all_stats.pseudo_easy[0], all_stats.pseudo_easy[1], all_stats.pseudo_easy[2]);
    println!("  Grades true:   {:?}", all_stats.grades_true);
    println!("  Grades pseudo: {:?}", all_stats.grades_pseudo);

    println!("\nPer-refuter (true vs pseudo):");
    for (name, c) in &per_refuter {
        println!("  {:13}: true[R={} I={} F={}]  pseudo[R={} I={} F={}]",
            name, c[0], c[1], c[2], c[3], c[4], c[5]);
    }

    // ── Operational metrics (memory-pruning policy) ──
    // Policy: grade F (≥2 refuters) → quarantine/drop; D (1 refuter) → flag
    // for review; A/B/C (0 refuters) → keep.
    let g = |m: &HashMap<char, usize>, c: char| m.get(&c).copied().unwrap_or(0);
    let total_true: usize = all_stats.grades_true.values().sum();
    let total_pseudo: usize = all_stats.grades_pseudo.values().sum();
    let true_kept = g(&all_stats.grades_true, 'A') + g(&all_stats.grades_true, 'B')
        + g(&all_stats.grades_true, 'C');
    let pseudo_quarantined = g(&all_stats.grades_pseudo, 'F');
    let pseudo_flagged = pseudo_quarantined + g(&all_stats.grades_pseudo, 'D');

    let keep_rate = true_kept as f64 / total_true.max(1) as f64;
    let quarantine_rate = pseudo_quarantined as f64 / total_pseudo.max(1) as f64;
    let flag_rate = pseudo_flagged as f64 / total_pseudo.max(1) as f64;

    println!("\nKey metrics (grade-based policy):");
    println!("  Keep rate        (true, 0 refuters):    {:.1}%", keep_rate * 100.0);
    println!("  Quarantine rate  (pseudo, ≥2 refuters): {:.1}%", quarantine_rate * 100.0);
    println!("  Flag rate        (pseudo, ≥1 refuter):  {:.1}%", flag_rate * 100.0);

    // ── Assertions (regression guards, set from observed calibration ──
    // run 2026-09-08, moderate-density community regime 40/12 + temporal
    // evidence): keep=71.3%, flag=64.3%, true-F=1.7%.
    // The temporal refuter lifted flag from 35.7% → 64.3% while keep held —
    // breaking the structural keep/flag frontier (~108% → 135%).
    let true_f = g(&all_stats.grades_true, 'F') as f64 / total_true.max(1) as f64;
    assert!(keep_rate > 0.65, "true-edge keep rate too low: {:.1}%", keep_rate * 100.0);
    assert!(flag_rate > 0.55, "pseudo-edge flag rate too low: {:.1}%", flag_rate * 100.0);
    assert!(true_f < 0.08, "too many true edges quarantined: {:.1}%", true_f * 100.0);
}
