//! Adversarial injection harness (hardening §3.2) — answers "系统能自我
//! 纠错吗？" by injecting 10% pseudo-causal edges into a real store and
//! watching whether the system's own correction mechanisms catch them over
//! multiple rounds of evolution.
//!
//! Difference from `tests/refuter_calibration.rs`: that one is STATIC and
//! graph-level (hand-built CausalGraph, EdgeRefuter once). This harness is
//! DYNAMIC and end-to-end: edges live in a real CausalStore, and every
//! round exercises the actual machinery an agent's memory would run:
//!
//!   1. Refuter sweep — EdgeRefuter grades every valid edge (simulated
//!      periodic health check). Refuters only GRADE (A–F); they never
//!      invalidate. Detection ≠ correction.
//!   2. One consolidate() cycle — Stage 5 BiasAudit (pure statistics,
//!      zero-LLM) stamps bias_flag annotations; merge/GC/decay may retire
//!      edges; stage 1.7 LLM supersession judge SKIPS without an API key
//!      (noted in the report, not faked).
//!   3. Counter-evidence arrival — with probability p per round per live
//!      pseudo edge, a contradicting positive edge re-records the same
//!      decision text. This drives the write-path contradiction
//!      short-circuit (exact decision text + old negative / new positive),
//!      the ONLY zero-LLM mechanism that actually invalidates an edge.
//!      The semantic-contradiction path needs embeddings (no key → silently
//!      skipped, noted in the report).
//!
//! World: planted-community DAG (ported from refuter_calibration) written
//! to a real store with per-node texts drawn from community domain pools
//! (pseudo edges are keyword-indistinguishable). Node created_at is pinned
//! to topological time via SQL after the writes (harness substrate control
//! — same timestamps the calibration planted by hand). Pseudo edges keep
//! raw random orientation, so ~half are temporally inverted.
//!
//! Subcommands:
//!   run [--nodes N] [--seed S] [--rounds T] [--evidence-pct P]
//!   selftest          zero-LLM assertion battery (several scenario configs)

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use causal_memory::consolidate::{consolidate, ConsolidateConfig};
use causal_memory::hippocampus::CausalGraph;
use causal_memory::refute::EdgeRefuter;
use causal_memory::store::CausalStore;

// ─── Seeded PRNG (SplitMix64 — same discipline as the other calibrations) ──

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

// ─── World generation (ported from refuter_calibration's SynthWorld) ──────

/// Domain vocabulary per community — pseudo edges draw from the same pools,
/// so nothing about the TEXT distinguishes a hallucinated edge.
const DOMAIN_WORDS: &[&str] = &[
    "cache", "mutex", "deploy", "index", "trace", "queue", "retry", "shard",
];
const DOMAIN_VERBS: &[&str] = &["tuning", "warmup", "rollout", "guard", "probe"];

struct World {
    node_count: usize,
    community: Vec<usize>,
    node_time: Vec<i64>,
    node_text: Vec<String>,
    /// True causal edges (from < to in topological order).
    true_edges: Vec<(usize, usize)>,
    /// Pseudo edges: (from, to, hard) — hard = d-connected via forward path
    /// or common ancestor, easy = d-separated.
    pseudo_edges: Vec<(usize, usize, bool)>,
}

impl World {
    fn generate(rng: &mut Rng, n: usize, k: usize, p_in: u64, p_out: u64) -> Self {
        let community: Vec<usize> = (0..n).map(|_| rng.below(k)).collect();
        let mut true_edges: HashSet<(usize, usize)> = HashSet::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let p = if community[i] == community[j] {
                    p_in
                } else {
                    p_out
                };
                if rng.chance(p) {
                    true_edges.insert((i, j));
                }
            }
        }
        let mut true_edges: Vec<(usize, usize)> = true_edges.into_iter().collect();
        true_edges.sort_unstable();

        // Pseudo edges: 10% of the true-edge count, uniform random
        // non-adjacent pairs, RAW orientation kept (a hallucinating extractor
        // proposes A→B regardless of record order) — ~half end up temporally
        // inverted, planting separable temporal evidence.
        let pseudo_count = (true_edges.len() / 10).max(4);
        let mut pseudo_edges: Vec<(usize, usize, bool)> = Vec::new();
        let mut attempts = 0;
        while pseudo_edges.len() < pseudo_count && attempts < pseudo_count * 20 + 100 {
            attempts += 1;
            let (a, b) = (rng.below(n), rng.below(n));
            if a == b
                || true_edges.contains(&(a.min(b), a.max(b)))
                || pseudo_edges.iter().any(|&(x, y, _)| (x, y) == (a, b))
            {
                continue;
            }
            pseudo_edges.push((a, b, false));
        }

        let node_time: Vec<i64> = (0..n)
            .map(|i| (i as i64 + 1) * 10 + rng.below(5) as i64)
            .collect();
        let node_text: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "{} {} tactic {i}",
                    DOMAIN_WORDS[community[i] % DOMAIN_WORDS.len()],
                    DOMAIN_VERBS[i % DOMAIN_VERBS.len()],
                )
            })
            .collect();

        let mut world = Self {
            node_count: n,
            community,
            node_time,
            node_text,
            true_edges,
            pseudo_edges,
        };
        world.classify_pseudo();
        world
    }

    /// Hard = d-connected to the target through the true-edge graph (forward
    /// path or common ancestor); easy = d-separated.
    fn classify_pseudo(&mut self) {
        let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
        for &(f, t) in &self.true_edges {
            adj.entry(f).or_default().push(t);
        }
        let mut ancestors: HashMap<usize, HashSet<usize>> = HashMap::new();
        for node in 0..self.node_count {
            let mut anc = HashSet::new();
            let mut stack = vec![node];
            let mut visited = HashSet::new();
            while let Some(cur) = stack.pop() {
                if !visited.insert(cur) {
                    continue;
                }
                for &(f, t) in &self.true_edges {
                    if t == cur {
                        anc.insert(f);
                        stack.push(f);
                    }
                }
            }
            ancestors.insert(node, anc);
        }
        let mut reach: HashMap<usize, HashSet<usize>> = HashMap::new();
        for node in 0..self.node_count {
            let mut r = HashSet::new();
            let mut stack = vec![node];
            while let Some(cur) = stack.pop() {
                if let Some(nexts) = adj.get(&cur) {
                    for &nx in nexts {
                        if r.insert(nx) {
                            stack.push(nx);
                        }
                    }
                }
            }
            reach.insert(node, r);
        }
        for pe in self.pseudo_edges.iter_mut() {
            let (from, to, _) = *pe;
            let fwd = reach.get(&from).is_some_and(|r| r.contains(&to));
            let backdoor = ancestors.get(&from).is_some_and(|af| {
                ancestors
                    .get(&to)
                    .is_some_and(|at| af.iter().any(|a| at.contains(a)))
            });
            pe.2 = fwd || backdoor;
        }
    }
}

// ─── Run configuration + per-edge tracking ─────────────────────────────────

#[derive(Clone)]
struct Scenario {
    nodes: usize,
    communities: usize,
    p_in: u64,
    p_out: u64,
    seed: u64,
    rounds: usize,
    /// Per-round probability that a still-valid pseudo edge receives one
    /// piece of contradicting evidence.
    evidence_pct: u64,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            nodes: 48,
            communities: 4,
            // Density regime matched to refuter_calibration's fair-test
            // configuration (its keep-flag frontier finding): dense enough
            // communities that the confounder refuter has evidence to judge
            // with. Sparser worlds (n=80/k=8) make Jaccard ~0 everywhere and
            // the sweep fires on true edges indiscriminately — measured, not
            // guessed.
            p_in: 40,
            p_out: 2,
            seed: 42,
            rounds: 10,
            evidence_pct: 30,
        }
    }
}

impl Scenario {
    fn describe(&self) -> String {
        format!(
            "nodes={} k={} seed={} rounds={} evidence={}%",
            self.nodes, self.communities, self.seed, self.rounds, self.evidence_pct
        )
    }
}

/// What invalidated an edge (the correction mechanism that fired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mechanism {
    /// Write-path contradiction short-circuit (counter-evidence write).
    Contradiction,
    /// A consolidate() stage (merge / GC — the pipeline does not expose
    /// per-edge sub-stage attribution).
    Consolidate,
}

#[derive(Default, Clone)]
struct EdgeTrack {
    flagged_at: Option<usize>,      // first round with refuter grade D/F
    quarantined_at: Option<usize>,  // first round with refuter grade F
    bias_flagged_at: Option<usize>, // first round with a BiasAudit stamp
    invalidated: Option<(usize, Mechanism)>,
}

#[derive(Default)]
struct RunReport {
    scenario: String,
    true_count: usize,
    pseudo_hard: usize,
    pseudo_easy: usize,
    pseudo_tracks: Vec<(bool /*hard*/, EdgeTrack)>,
    /// True edges ever graded D/F by the refuter (false-flag events).
    true_flagged: usize,
    /// True edges invalidated at any point, split by mechanism.
    true_invalidated_contradiction: usize,
    true_invalidated_consolidate: usize,
    /// Counter-evidence writes that also killed ≥1 true edge sharing the
    /// decision chunk (contradiction collateral).
    collateral_events: usize,
    evidence_writes: usize,
    /// Distinct bias-flagged edges at end of run (pseudo / true / other).
    bias_flagged_pseudo: usize,
    bias_flagged_true: usize,
    bias_flagged_other: usize,
    /// LLM-only correction paths that were unavailable this run.
    skipped_llm_paths: Vec<&'static str>,
}

fn percentile(sorted: &[usize], p: f64) -> String {
    if sorted.is_empty() {
        return "—".into();
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx].to_string()
}

impl RunReport {
    fn detection(&self, hard: bool) -> (usize, usize) {
        let total = self
            .pseudo_tracks
            .iter()
            .filter(|(h, _)| *h == hard)
            .count();
        let flagged = self
            .pseudo_tracks
            .iter()
            .filter(|(h, t)| *h == hard && t.flagged_at.is_some())
            .count();
        (flagged, total)
    }

    fn invalidated_by(&self, mech: Mechanism) -> usize {
        self.pseudo_tracks
            .iter()
            .filter(|(_, t)| t.invalidated.is_some_and(|(_, m)| m == mech))
            .count()
    }

    fn latencies(&self, f: fn(&EdgeTrack) -> Option<usize>) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .pseudo_tracks
            .iter()
            .filter_map(|(_, t)| f(t))
            .collect();
        v.sort_unstable();
        v
    }

    fn median_invalidation_latency(&self) -> Option<usize> {
        let lats = self.latencies(|t| t.invalidated.map(|(r, _)| r));
        if lats.is_empty() {
            None
        } else {
            Some(lats[lats.len() / 2])
        }
    }

    fn render(&self) -> String {
        let pct = |n: usize, d: usize| {
            if d == 0 {
                "—".into()
            } else {
                format!("{:.1}%", n as f64 * 100.0 / d as f64)
            }
        };
        let (ef, et) = self.detection(false);
        let (hf, ht) = self.detection(true);
        let inv_contra = self.invalidated_by(Mechanism::Contradiction);
        let inv_consol = self.invalidated_by(Mechanism::Consolidate);
        let pseudo_total = self.pseudo_tracks.len();
        let never = pseudo_total
            - self
                .pseudo_tracks
                .iter()
                .filter(|(_, t)| t.flagged_at.is_some() || t.invalidated.is_some())
                .count();
        let flag_lat = self.latencies(|t| t.flagged_at);
        let inv_lat = self.latencies(|t| t.invalidated.map(|(r, _)| r));
        let bias_flagged = self
            .pseudo_tracks
            .iter()
            .filter(|(_, t)| t.bias_flagged_at.is_some())
            .count();
        let mut s = String::new();
        s.push_str(&format!("scenario: {}\n", self.scenario));
        s.push_str(&format!(
            "world: {} true edges, {} pseudo ({} hard / {} easy)\n",
            self.true_count, pseudo_total, self.pseudo_hard, self.pseudo_easy
        ));
        s.push_str(&format!(
            "pseudo detection (grade D/F):   easy {} ({}/{}), hard {} ({}/{})\n",
            pct(ef, et),
            ef,
            et,
            pct(hf, ht),
            hf,
            ht
        ));
        s.push_str(&format!(
            "pseudo invalidated:             {} of {} (contradiction {}, consolidate {})\n",
            inv_contra + inv_consol,
            pseudo_total,
            inv_contra,
            inv_consol
        ));
        s.push_str(&format!(
            "correction latency (rounds):    to flag median {} p90 {}; to invalidation median {} p90 {}\n",
            percentile(&flag_lat, 0.5),
            percentile(&flag_lat, 0.9),
            percentile(&inv_lat, 0.5),
            percentile(&inv_lat, 0.9),
        ));
        s.push_str(&format!(
            "residual:                       {} pseudo edge(s) never flagged nor invalidated\n",
            never
        ));
        s.push_str(&format!(
            "true-edge collateral:           {} ever flagged D/F (advisory only), {} invalidated ({} contradiction / {} consolidate)\n",
            self.true_flagged,
            self.true_invalidated_contradiction + self.true_invalidated_consolidate,
            self.true_invalidated_contradiction,
            self.true_invalidated_consolidate
        ));
        s.push_str(&format!(
            "counter-evidence writes:        {} ({} with true-edge collateral)\n",
            self.evidence_writes, self.collateral_events
        ));
        s.push_str(&format!(
            "pseudo edges bias-flagged: {}; distinct bias-flagged edges: {} pseudo / {} true / {} other\n",
            bias_flagged,
            self.bias_flagged_pseudo,
            self.bias_flagged_true,
            self.bias_flagged_other
        ));
        if !self.skipped_llm_paths.is_empty() {
            s.push_str(&format!(
                "LLM-only paths skipped (no key): {}\n",
                self.skipped_llm_paths.join("; ")
            ));
        }
        s
    }
}

// ─── The experiment ────────────────────────────────────────────────────────

const SECS_PER_DAY: i64 = 86_400;

fn run_scenario(cfg: &Scenario) -> Result<RunReport> {
    let mut rng = Rng::new(cfg.seed);
    let world = World::generate(&mut rng, cfg.nodes, cfg.communities, cfg.p_in, cfg.p_out);
    let store = CausalStore::open_in_memory()?;

    // ── Write the world into a real store. Edge event_times are unique
    // sequence numbers so the graph's ORDER BY event_time edge order maps
    // positionally back to DB edge ids. Chunk created_at is then pinned to
    // topological node time via SQL — the substrate-level equivalent of the
    // calibration's hand-set NodeData timestamps.
    let mut seq = 0i64;
    let mut true_ids: HashSet<i64> = HashSet::new();
    let mut pseudo_ids: Vec<(i64, bool)> = Vec::new(); // (edge_id, hard)
    let mut write_edge = |from: usize,
                          to: usize,
                          polarity: &str,
                          discovered_by: &str,
                          conf: f64,
                          outcome_override: Option<&str>|
     -> Result<i64> {
        seq += 1;
        let (_, edge_id) = store.record_decision_full(
            &world.node_text[from],
            outcome_override.unwrap_or(&world.node_text[to]),
            "caused",
            Some(DOMAIN_WORDS[world.community[from] % DOMAIN_WORDS.len()]),
            conf,
            discovered_by,
            seq,
            Some(polarity),
            None,
            None,
        )?;
        Ok(edge_id)
    };
    // Write order matters: the contradiction short-circuit fires when a
    // POSITIVE edge lands on a decision chunk that already has a valid
    // NEGATIVE edge. World construction must be neutral, so all positive
    // edges are written before any negative one — the rule can never see
    // (old negative, new positive) during setup. (Round-1 debugging showed
    // 33 true edges silently invalidated by interleaved writes.)
    let mut neg_true: Vec<(usize, usize)> = Vec::new();
    for &(f, t) in &world.true_edges {
        if rng.chance(75) {
            true_ids.insert(write_edge(f, t, "positive", "rule", 0.7, None)?);
        } else {
            neg_true.push((f, t));
        }
    }
    for (f, t) in neg_true {
        true_ids.insert(write_edge(f, t, "negative", "rule", 0.7, None)?);
    }
    for &(f, t, hard) in &world.pseudo_edges {
        // Hallucinated edges are negative-outcome claims ("X caused bad Y") —
        // the shape the contradiction short-circuit can later falsify.
        let id = write_edge(f, t, "negative", "rule", 0.6, None)?;
        pseudo_ids.push((id, hard));
    }
    // Pin chunk timestamps to topological time (temporal-refuter evidence).
    store.with_conn(|conn| {
        for (i, text) in world.node_text.iter().enumerate() {
            conn.execute(
                "UPDATE chunks SET created_at = ?1 WHERE text = ?2",
                rusqlite::params![world.node_time[i], text],
            )?;
        }
        Ok(())
    })?;

    let base_now = chrono::Utc::now().timestamp();
    let mut tracks: Vec<EdgeTrack> = vec![EdgeTrack::default(); pseudo_ids.len()];
    let mut true_flagged: HashSet<i64> = HashSet::new();
    let mut true_invalidated: HashSet<i64> = HashSet::new();
    let mut true_inv_contradiction = 0usize;
    let mut true_inv_consolidate = 0usize;
    let mut report = RunReport {
        scenario: cfg.describe(),
        true_count: world.true_edges.len(),
        pseudo_hard: world.pseudo_edges.iter().filter(|e| e.2).count(),
        pseudo_easy: world.pseudo_edges.iter().filter(|e| !e.2).count(),
        skipped_llm_paths: vec![
            "stage 1.7 resolve_supersessions (LLM judge) — no config, skipped",
            "invalidate_semantic_contradictions (needs embeddings) — silently off",
        ],
        ..Default::default()
    };

    let config = ConsolidateConfig::default();
    let mut ev_rng = Rng(cfg.seed ^ 0xE71D);

    for round in 1..=cfg.rounds {
        let now = base_now + round as i64 * SECS_PER_DAY;

        // ── 1. Refuter sweep (periodic health check) ─────────────────────
        let graph = CausalGraph::from_store(&store)?;
        // edge_idx → DB id mapping: the graph's CSR re-sorts edges by source
        // node, so event_time ordering does NOT survive. Map by endpoint
        // chunk ids instead (our worlds never duplicate a (from, to) pair).
        let mut by_pair: HashMap<(String, String), Vec<i64>> = HashMap::new();
        for e in store.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, from_id, to_id FROM causal_edges")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(anyhow::Error::from)
        })? {
            by_pair.entry((e.1, e.2)).or_default().push(e.0);
        }
        let mut edge_id_of_idx: Vec<Option<i64>> = vec![None; graph.num_edges()];
        for edge_idx in 0..graph.num_edges() {
            let fid = graph
                .node_id(graph.edge_source_node(edge_idx) as usize)
                .to_string();
            let tid = graph
                .node_id(graph.edge_target(edge_idx) as usize)
                .to_string();
            edge_id_of_idx[edge_idx] = by_pair.get_mut(&(fid, tid)).and_then(|v| v.pop());
        }
        let refuter = EdgeRefuter::new(&graph);
        let ref_report = refuter.refute_all();
        if std::env::var("ADV_DEBUG").is_ok() && round == 1 {
            let mut dist_true = HashMap::new();
            let mut dist_pseudo = HashMap::new();
            let mut refuted_true: HashMap<&str, usize> = HashMap::new();
            let mut refuted_pseudo: HashMap<&str, usize> = HashMap::new();
            for (edge_idx, result) in &ref_report.results {
                let Some(id) = edge_id_of_idx[*edge_idx] else {
                    continue;
                };
                let is_pseudo = pseudo_ids.iter().any(|(pid, _)| *pid == id);
                let dist = if is_pseudo {
                    &mut dist_pseudo
                } else {
                    &mut dist_true
                };
                *dist.entry(result.grade).or_insert(0) += 1;
                for t in &result.tests {
                    if t.result == causal_memory::refute::TestResult::Refuted {
                        let tally = if is_pseudo {
                            &mut refuted_pseudo
                        } else {
                            &mut refuted_true
                        };
                        *tally.entry(t.name).or_insert(0) += 1;
                    }
                }
            }
            eprintln!("round1 grades true: {dist_true:?} pseudo: {dist_pseudo:?}");
            eprintln!("round1 refuted-by-test true: {refuted_true:?} pseudo: {refuted_pseudo:?}");
            // Mapping sanity: graph edge_idx ↔ DB id via event_time order.
            let mut mismatches = 0;
            for edge_idx in 0..graph.num_edges() {
                let Some(id) = edge_id_of_idx[edge_idx] else {
                    continue;
                };
                let e = store.get_edge(id).unwrap().unwrap();
                let gf = graph.node_text(graph.edge_source_node(edge_idx) as usize);
                let gt = graph.node_text(graph.edge_target(edge_idx) as usize);
                if e.decision_text != gf || e.outcome_text != gt {
                    mismatches += 1;
                    if mismatches <= 5 {
                        eprintln!(
                            "  MISMATCH idx {edge_idx} id {id}: store [{} → {}] vs graph [{} → {}]",
                            e.decision_text, e.outcome_text, gf, gt
                        );
                    }
                }
            }
            eprintln!("  mapping mismatches: {mismatches}/{}", graph.num_edges());
            for (edge_idx, result) in &ref_report.results {
                let Some(id) = edge_id_of_idx[*edge_idx] else {
                    continue;
                };
                let is_pseudo = pseudo_ids.iter().any(|(pid, _)| *pid == id);
                let temporal_refuted = result.tests.iter().any(|t| {
                    t.name == "temporal" && t.result == causal_memory::refute::TestResult::Refuted
                });
                if temporal_refuted {
                    let from = graph.edge_source_node(*edge_idx);
                    let to = graph.edge_target(*edge_idx);
                    eprintln!(
                        "  temporal-refuted {} edge {id}: {} (t={}) → {} (t={})",
                        if is_pseudo { "pseudo" } else { "TRUE" },
                        graph.node_text(from as usize),
                        graph.node_event_time(from as usize),
                        graph.node_text(to as usize),
                        graph.node_event_time(to as usize),
                    );
                }
            }
        }
        for (edge_idx, result) in &ref_report.results {
            let Some(id) = edge_id_of_idx[*edge_idx] else {
                continue;
            };
            if let Some(pos) = pseudo_ids.iter().position(|(pid, _)| *pid == id) {
                let track = &mut tracks[pos];
                match result.grade {
                    'F' => {
                        track.quarantined_at.get_or_insert(round);
                        track.flagged_at.get_or_insert(round);
                    }
                    'D' => {
                        track.flagged_at.get_or_insert(round);
                    }
                    _ => {}
                }
            } else if true_ids.contains(&id) && matches!(result.grade, 'D' | 'F') {
                true_flagged.insert(id);
            }
        }

        // ── 2. Consolidate cycle (BiasAudit, merge, decay/GC, …) ─────────
        let pre_valid: HashSet<i64> = store.all_valid_edges()?.iter().map(|e| e.edge_id).collect();
        let creport = consolidate(&store, &config, false, now)?;
        let _ = &creport;
        // BiasAudit is annotation-only (its flags are a review queue, not a
        // correction). Track first-stamp rounds as a separate channel.
        for fe in store.bias_flagged_edges()? {
            if let Some(pos) = pseudo_ids.iter().position(|(pid, _)| *pid == fe.edge_id) {
                tracks[pos].bias_flagged_at.get_or_insert(round);
            }
        }
        for id in &pre_valid {
            if store.get_edge(*id)?.is_some_and(|e| e.valid_to.is_some()) {
                if let Some(pos) = pseudo_ids.iter().position(|(pid, _)| pid == id) {
                    if tracks[pos].invalidated.is_none() {
                        tracks[pos].invalidated = Some((round, Mechanism::Consolidate));
                    }
                } else if true_ids.contains(id) && true_invalidated.insert(*id) {
                    true_inv_consolidate += 1;
                }
            }
        }

        // ── 3. Counter-evidence arrival (drives contradiction correction) ─
        let live: Vec<usize> = tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.invalidated.is_none())
            .map(|(i, _)| i)
            .collect();
        for pos in live {
            if !ev_rng.chance(cfg.evidence_pct) {
                continue;
            }
            let (pid, _) = pseudo_ids[pos];
            let entry = store.get_edge(pid)?.expect("live pseudo edge");
            if entry.valid_to.is_some() {
                continue;
            }
            let from_text = entry.decision_text.clone();
            let outcome = format!("recheck round {round}: {from_text} worked cleanly this time");
            // True edges sharing the decision chunk are exposed to the same
            // contradiction rule — snapshot them to measure collateral.
            let exposed_true: Vec<i64> = true_ids
                .iter()
                .copied()
                .filter(|id| !true_invalidated.contains(id))
                .filter(|id| {
                    store
                        .get_edge(*id)
                        .ok()
                        .flatten()
                        .is_some_and(|e| e.valid_to.is_none() && e.decision_text == from_text)
                })
                .collect();
            write_edge(
                // from/to indices only locate texts; the outcome is the
                // recheck text, so `to` is unused here.
                world.pseudo_edges[pos].0,
                world.pseudo_edges[pos].1,
                "positive",
                "user_feedback",
                0.9,
                Some(&outcome),
            )?;
            report.evidence_writes += 1;
            if store.get_edge(pid)?.is_some_and(|e| e.valid_to.is_some()) {
                tracks[pos].invalidated = Some((round, Mechanism::Contradiction));
            }
            for tid in exposed_true {
                if store.get_edge(tid)?.is_some_and(|e| e.valid_to.is_some())
                    && true_invalidated.insert(tid)
                {
                    true_inv_contradiction += 1;
                    report.collateral_events += 1;
                }
            }
        }
    }

    report.pseudo_tracks = pseudo_ids
        .iter()
        .zip(tracks)
        .map(|((_, hard), t)| (*hard, t))
        .collect();
    report.true_flagged = true_flagged.len();
    report.true_invalidated_contradiction = true_inv_contradiction;
    report.true_invalidated_consolidate = true_inv_consolidate;
    // Distinct bias-flagged edges by class at end of run.
    let pseudo_set: HashSet<i64> = pseudo_ids.iter().map(|(id, _)| *id).collect();
    for fe in store.bias_flagged_edges()? {
        if pseudo_set.contains(&fe.edge_id) {
            report.bias_flagged_pseudo += 1;
        } else if true_ids.contains(&fe.edge_id) {
            report.bias_flagged_true += 1;
        } else {
            report.bias_flagged_other += 1;
        }
    }
    Ok(report)
}

// ─── Selftest (zero-LLM assertion battery) ─────────────────────────────────

fn selftest() -> Result<()> {
    // Scenario A: default world, evidence at 30%/round.
    let a = run_scenario(&Scenario::default())?;
    print!("=== A (default, evidence 30%) ===\n{}", a.render());

    let (ef, et) = a.detection(false);
    let (hf, ht) = a.detection(true);
    assert!(
        et > 0 && ht > 0,
        "world must contain both pseudo classes ({et}/{ht})"
    );
    assert!(
        ef * ht >= hf * et,
        "easy detection rate must be >= hard: easy {ef}/{et}, hard {hf}/{ht}"
    );
    assert!(
        ef * 2 >= et,
        "easy pseudo edges are mostly detectable: {ef}/{et}"
    );
    // Consolidation (merge/GC/decay/bias-audit) must never kill a true edge
    // on this timescale; contradiction collateral is bounded (it can only
    // hit negative true edges sharing the pseudo edge's decision chunk).
    assert_eq!(
        a.true_invalidated_consolidate,
        0,
        "consolidate must not kill true edges: {}",
        a.render()
    );
    assert!(
        a.true_invalidated_contradiction * 10 <= a.true_count,
        "contradiction collateral must stay under 10% of true edges: {}",
        a.render()
    );
    // With 30%/round evidence over 10 rounds, nearly every pseudo edge sees
    // counter-evidence (0.7^10 ≈ 3% escape) and the contradiction path
    // invalidates it.
    let contra = a.invalidated_by(Mechanism::Contradiction);
    assert!(
        contra * 5 >= a.pseudo_tracks.len() * 4,
        "contradiction path should correct ≥80% of pseudo edges: {contra}/{}",
        a.pseudo_tracks.len()
    );

    // Scenarios B/C: evidence-rate contrast — faster evidence, faster fix.
    let slow = run_scenario(&Scenario {
        evidence_pct: 10,
        seed: 42,
        ..Scenario::default()
    })?;
    let fast = run_scenario(&Scenario {
        evidence_pct: 50,
        seed: 42,
        ..Scenario::default()
    })?;
    print!("=== B (evidence 10%) ===\n{}", slow.render());
    print!("=== C (evidence 50%) ===\n{}", fast.render());
    match (
        slow.median_invalidation_latency(),
        fast.median_invalidation_latency(),
    ) {
        (Some(s), Some(f)) => assert!(
            f < s,
            "higher evidence rate must correct faster: 50% median {f} vs 10% median {s}"
        ),
        (s, f) => panic!("both scenarios must invalidate some pseudo edges: {s:?}/{f:?}"),
    }

    // Scenario D: no counter-evidence — the honest negative result.
    let no_ev = run_scenario(&Scenario {
        evidence_pct: 0,
        seed: 42,
        ..Scenario::default()
    })?;
    print!("=== D (no evidence) ===\n{}", no_ev.render());
    assert_eq!(
        no_ev.invalidated_by(Mechanism::Contradiction),
        0,
        "no evidence ⇒ no contradiction corrections"
    );
    assert_eq!(
        no_ev.true_invalidated_contradiction + no_ev.true_invalidated_consolidate,
        0,
        "no evidence ⇒ zero true-edge collateral"
    );

    println!("selftest OK — all assertions held");
    Ok(())
}

// ─── CLI ───────────────────────────────────────────────────────────────────

const USAGE: &str = "causal-memory-adversarial-eval <subcommand> [opts]
  run [--nodes N] [--seed S] [--rounds T] [--evidence-pct P]
  selftest                    (zero-LLM assertion battery)";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match cmd.as_str() {
        "run" => {
            let mut cfg = Scenario::default();
            if let Some(v) = get("--nodes").and_then(|v| v.parse().ok()) {
                cfg.nodes = v;
            }
            if let Some(v) = get("--seed").and_then(|v| v.parse().ok()) {
                cfg.seed = v;
            }
            if let Some(v) = get("--rounds").and_then(|v| v.parse().ok()) {
                cfg.rounds = v;
            }
            if let Some(v) = get("--evidence-pct").and_then(|v| v.parse().ok()) {
                cfg.evidence_pct = v;
            }
            let report = run_scenario(&cfg)?;
            print!("{}", report.render());
            Ok(())
        }
        "selftest" => selftest(),
        other => {
            eprintln!("unknown subcommand: {other}\n{USAGE}");
            std::process::exit(2);
        }
    }
}
