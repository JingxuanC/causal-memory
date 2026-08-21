//! Hippocampus-style causal activation graph.
//!
//! CSR (Compressed Sparse Row) format for cache-friendly spreading activation.
//! Maps to hippocampal regions:
//!   DG  → SimHash pattern separation (sparse codes for dedup)
//!   CA3 → Spreading activation (the core retrieval mechanism)
//!   CA1 → Novelty detection (predicted vs actual comparison)
//!   SWR → Offline replay consolidation (LTP/LTD/GC)
//!
//! Design principles:
//! - SQLite = persistence layer ("synaptic connections")
//! - CausalGraph = computation layer ("neural activation")
//! - SoA (Structure of Arrays) for cache-friendly hot paths
//! - f32 for activation/weight (SIMD-friendly, half cache footprint vs f64)
//! - Pre-multiplied spread coefficients in values[]
//!
//! Limitations:
//! - text_jaccard_similarity uses whitespace tokenization; Chinese text (no
//!   spaces) produces one giant token, making novelty detection unreliable.
//!   Future: switch to character bigrams or a real tokenizer.
//! - rand_seed uses a fixed-seed xorshift for deterministic testing. Set
//!   CAUSAL_GRAPH_RANDOM_SEED to enable true randomness in production.

use std::collections::{HashMap, HashSet};

mod types;
pub(crate) mod utils;

pub use types::{
    ActivationResult, ConsolidationDelta, ConsolidationResult, ConsolidationStats, EdgeData,
    NodeData, NoveltyReport, Relation,
};

/// (P5) Novelty-gate modes (Nemori FEP, arXiv:2508.03341).
///
/// The cheap entropy check is word-frequency surprise; the prediction gap is
/// SEMANTIC surprise — what the model expected to happen vs what actually
/// happened. Hybrid runs entropy first and only pays for the LLM on
/// borderline cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoveltyMode {
    /// Word-frequency surprise (the existing `detect_novelty` behavior).
    #[default]
    Entropy,
    /// Semantic surprise: an LLM predicts the outcome of the decision; the
    /// gap between prediction and reality is the surprise.
    PredictionGap,
    /// Entropy first; borderline surprises (0.4..=0.7) defer to the LLM.
    Hybrid,
}

impl NoveltyMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "prediction_gap" | "prediction-gap" | "gap" => Self::PredictionGap,
            "hybrid" => Self::Hybrid,
            _ => Self::Entropy,
        }
    }
}

/// (P5) Pure gate: does the entropy surprise need the expensive LLM
/// prediction-gap check? Confident entropy verdicts (surprise ≤ 0.4 means
/// "predicted it", > 0.7 means "genuinely new") stand on their own; the
/// borderline band defers to semantic surprise.
pub fn needs_prediction_gap(surprise: f32) -> bool {
    (0.4..=0.7).contains(&surprise)
}

use types::AdjEdge;
use utils::{rand_seed, simhash, text_jaccard_similarity, WEIGHT_CAP};

/// CSR-format causal graph with SoA node attributes.
///
/// Memory layout (all contiguous arrays):
///   row_ptr:   [u32; N+1]  — row i's edges are col_idx[row_ptr[i]..row_ptr[i+1]]
///   col_idx:   [u32; E]    — target node index for each edge
///   values:    [f32; E]    — pre-multiplied weight × spread_coeff
///   edge_valid:[bool; E]   — whether edge is still valid (shared fwd+rev)
///
/// Hot path (spreading_activation) only touches:
///   row_ptr + col_idx + values + local activations Vec
/// All are contiguous arrays → cache-friendly access pattern.
#[derive(Clone)]
pub struct CausalGraph {
    num_nodes: usize,

    // Forward CSR (decision → outcome)
    row_ptr: Vec<u32>,
    col_idx: Vec<u32>,
    values: Vec<f32>,
    raw_weights: Vec<f32>,
    edge_relations: Vec<Relation>,
    edge_valid: Vec<bool>,

    // Reverse CSR (outcome → decision, for trace_cause)
    // Maps each reverse edge back to the forward edge index for validity checks.
    row_ptr_rev: Vec<u32>,
    col_idx_rev: Vec<u32>,
    values_rev: Vec<f32>,
    rev_to_fwd_idx: Vec<u32>, // rev edge i → forward edge index for valid check

    // Node attributes (SoA — Structure of Arrays)
    node_text: Vec<String>,
    node_ids: Vec<String>,
    node_q_value: Vec<f32>,
    node_replay_count: Vec<u16>,
    node_last_activated: Vec<i64>,
    node_event_time: Vec<i64>,
    node_sparse_code: Vec<u128>,
    node_task_tag: Vec<Option<String>>,
    node_scope: Vec<Option<String>>,
    node_id_to_idx: HashMap<String, u32>,

    // Phase C (one-graph-convergence): write-path patches. Inserting an
    // edge into the middle of a CSR array is O(E) and shifts every stored
    // CSR edge index, so edges added since the last full build live in
    // per-node overlay maps consulted alongside the CSR segments by the
    // spread steps. A full `from_store` rebuild folds them in (the
    // overlays are dropped and the store is the truth).
    patch_fwd: HashMap<u32, Vec<PatchEdge>>,
    patch_rev: HashMap<u32, Vec<PatchEdge>>,

    /// Phase C: node indices retired since the last full build (e.g. a
    /// fact replaced under the same key). Retired nodes neither seed nor
    /// surface in results; the next full rebuild drops them entirely
    /// (from_store filters `valid_to IS NULL`).
    retired_nodes: std::collections::HashSet<u32>,

    /// Phase C: incremental inverted index — distinct token → chunk-node
    /// indices (fact/scope nodes excluded). Maintained by `append_node`,
    /// consumed by `link_fact_node`, so the write-path linker costs
    /// O(fact tokens) instead of re-tokenizing every node per write. The
    /// rebuild-time linker (`entity_link_facts`) stays store-side; both
    /// use the same thresholds and weights.
    token_index: HashMap<String, Vec<u32>>,

    // Config
    decay: f32,
    threshold: f32,
    max_hops: usize,
    ltp_rate: f32,
    ltd_rate: f32,
    gc_threshold: f32,
}

/// Phase C: one overlay edge — the "other" node, its pre-multiplied spread
/// value, and its validity flag (flippable in O(1), mirroring `edge_valid`).
#[derive(Debug, Clone, Copy)]
struct PatchEdge {
    other: u32,
    value: f32,
    valid: bool,
}

impl CausalGraph {
    /// Create a new graph with default parameters.
    pub fn new() -> Self {
        Self {
            num_nodes: 0,
            row_ptr: vec![0],
            col_idx: Vec::new(),
            values: Vec::new(),
            raw_weights: Vec::new(),
            edge_relations: Vec::new(),
            edge_valid: Vec::new(),
            row_ptr_rev: vec![0],
            col_idx_rev: Vec::new(),
            values_rev: Vec::new(),
            rev_to_fwd_idx: Vec::new(),
            node_text: Vec::new(),
            node_ids: Vec::new(),
            node_q_value: Vec::new(),
            node_replay_count: Vec::new(),
            node_last_activated: Vec::new(),
            node_event_time: Vec::new(),
            node_sparse_code: Vec::new(),
            node_task_tag: Vec::new(),
            node_scope: Vec::new(),
            node_id_to_idx: HashMap::new(),
            patch_fwd: HashMap::new(),
            patch_rev: HashMap::new(),
            retired_nodes: std::collections::HashSet::new(),
            token_index: HashMap::new(),
            decay: 0.7,
            threshold: 0.1,
            max_hops: 5,
            ltp_rate: 1.05,
            ltd_rate: 0.99,
            gc_threshold: 0.05,
        }
    }

    /// Build graph from node and edge lists (typically loaded from SQLite).
    pub fn build(nodes: &[NodeData], edges: &[EdgeData]) -> Self {
        let mut graph = Self::new();
        graph.num_nodes = nodes.len();

        // Build node lookup map FIRST (O(N))
        for (i, node) in nodes.iter().enumerate() {
            graph.node_id_to_idx.insert(node.id.clone(), i as u32);
        }

        // SoA node attributes
        graph.node_text = nodes.iter().map(|n| n.text.clone()).collect();
        graph.node_ids = nodes.iter().map(|n| n.id.clone()).collect();
        graph.node_q_value = nodes.iter().map(|n| n.q_value).collect();
        graph.node_replay_count = nodes.iter().map(|n| n.replay_count).collect();
        graph.node_last_activated = nodes.iter().map(|n| n.last_activated).collect();
        graph.node_event_time = nodes.iter().map(|n| n.event_time).collect();
        graph.node_sparse_code = nodes.iter().map(|n| simhash(&n.text)).collect();
        graph.node_task_tag = nodes.iter().map(|n| n.task_tag.clone()).collect();
        graph.node_scope = nodes.iter().map(|n| n.scope.clone()).collect();

        // Build forward adjacency list (no fwd_idx yet — CSR index assigned during CSR build)
        let mut adj: Vec<Vec<AdjEdge>> = vec![Vec::new(); nodes.len()];

        for edge in edges.iter() {
            let from = graph.node_id_to_idx.get(&edge.from_id);
            let to = graph.node_id_to_idx.get(&edge.to_id);
            if let (Some(&from_idx), Some(&to_idx)) = (from, to) {
                adj[from_idx as usize].push((
                    to_idx,
                    edge.weight,
                    edge.weight * edge.relation.spread_coeff(),
                    edge.relation,
                    edge.valid,
                ));
            }
        }

        // Build forward CSR, simultaneously accumulating reverse adjacency
        // with the CORRECT CSR edge index (not input array index).
        // Bug fix: fwd_idx must be the CSR position (= col_idx.len() before push),
        // not the input edges[] position. Input order ≠ CSR order when edges
        // from different source nodes are interleaved (which from_store() always
        // produces via ORDER BY event_time).
        let mut adj_rev: Vec<Vec<(u32, f32, u32)>> = vec![Vec::new(); nodes.len()];
        // rev tuple: (source_node, value, csr_edge_index)

        graph.row_ptr = Vec::with_capacity(nodes.len() + 1);
        graph.row_ptr.push(0);
        // row_ptr invariant: seeded with 0 above, so .last() is always Some.
        // Each iteration pushes exactly one entry, preserving the invariant.
        for (node_idx, node_edges) in adj.iter().enumerate() {
            #[allow(clippy::expect_used, reason = "row_ptr invariant: seeded with 0")]
            let prev = *graph.row_ptr.last().expect("row_ptr seeded with a 0 above");
            graph.row_ptr.push(prev + node_edges.len() as u32);
            for &(target, raw_w, val, rel, valid) in node_edges {
                // CSR index for this edge = current length of col_idx (before push)
                let csr_edge_idx = graph.col_idx.len() as u32;

                graph.col_idx.push(target);
                graph.values.push(val);
                graph.raw_weights.push(raw_w);
                graph.edge_relations.push(rel);
                graph.edge_valid.push(valid);

                // Record in reverse adjacency with the CSR index
                adj_rev[target as usize].push((node_idx as u32, val, csr_edge_idx));
            }
        }

        // Build reverse CSR from adj_rev (indices are already correct CSR positions)
        graph.row_ptr_rev = Vec::with_capacity(nodes.len() + 1);
        graph.row_ptr_rev.push(0);
        for node_edges in &adj_rev {
            #[allow(clippy::expect_used, reason = "row_ptr_rev invariant: seeded with 0")]
            let prev = *graph
                .row_ptr_rev
                .last()
                .expect("row_ptr_rev seeded with a 0 above");
            graph.row_ptr_rev.push(prev + node_edges.len() as u32);
            for &(target, val, csr_idx) in node_edges {
                graph.col_idx_rev.push(target);
                graph.values_rev.push(val);
                graph.rev_to_fwd_idx.push(csr_idx);
            }
        }

        // Phase C: (re)build the incremental token index — distinct tokens
        // → chunk-node indices, deduped posting lists. This mirrors what
        // append_node maintains patch-side, so post-rebuild writes link
        // against the full node set without re-tokenizing it.
        graph.token_index.reserve(nodes.len());
        for (i, node) in nodes.iter().enumerate() {
            if node.id.starts_with("fact:") || node.id.starts_with("scope:") {
                continue;
            }
            let distinct: std::collections::HashSet<String> =
                crate::patterns::tokenize(&node.text).into_iter().collect();
            for tok in distinct {
                graph.token_index.entry(tok).or_default().push(i as u32);
            }
        }

        graph
    }

    /// Find seed nodes by text matching.
    /// Returns empty vec for empty/whitespace-only queries (prevents activating all nodes).
    fn find_seeds(&self, query: &str, task_tag: Option<&str>) -> Vec<u32> {
        let query_lower = query.to_lowercase();
        if query_lower.trim().is_empty() {
            return Vec::new(); // Bug fix #3: empty query would match everything
        }

        let mut seeds = Vec::new();
        for i in 0..self.num_nodes {
            // Phase C: retired nodes never seed (a replaced fact must not
            // activate from a query match while patches are in flight).
            if self.retired_nodes.contains(&(i as u32)) {
                continue;
            }
            if let Some(tag) = task_tag {
                if self.node_task_tag[i].as_deref() != Some(tag) {
                    continue;
                }
            }
            if self.node_text[i].to_lowercase().contains(&query_lower) {
                seeds.push(i as u32);
            }
        }
        seeds
    }

    /// Core: single-hop spreading activation step (SpMV-style).
    #[inline]
    fn spread_step(&self, activations: &[f32], decay: f32) -> Vec<f32> {
        let mut new_act = vec![0.0_f32; self.num_nodes];

        for (i, &a) in activations.iter().enumerate() {
            if a.abs() < self.threshold {
                continue;
            }

            let start = self.row_ptr[i] as usize;
            let end = self.row_ptr[i + 1] as usize;

            for edge_idx in start..end {
                if !self.edge_valid[edge_idx] {
                    continue; // Skip invalidated edges
                }
                let target = self.col_idx[edge_idx] as usize;
                let weight = self.values[edge_idx];
                new_act[target] += a * weight * decay;
            }

            // Phase C: write-path patch edges from this node.
            if let Some(patches) = self.patch_fwd.get(&(i as u32)) {
                for p in patches {
                    if p.valid {
                        new_act[p.other as usize] += a * p.value * decay;
                    }
                }
            }
        }

        for a in &mut new_act {
            *a = a.clamp(-1.0, 1.0);
        }
        new_act
    }

    /// Reverse single-hop step (for trace_cause: outcome → decision).
    /// Bug fix #1: now checks edge_valid via rev_to_fwd_idx mapping.
    #[inline]
    fn spread_step_rev(&self, activations: &[f32], decay: f32) -> Vec<f32> {
        let mut new_act = vec![0.0_f32; self.num_nodes];

        for (i, &a) in activations.iter().enumerate() {
            if a.abs() < self.threshold {
                continue;
            }

            let start = self.row_ptr_rev[i] as usize;
            let end = self.row_ptr_rev[i + 1] as usize;

            for rev_idx in start..end {
                // Bug fix #1: check forward edge validity
                let fwd_idx = self.rev_to_fwd_idx[rev_idx] as usize;
                if !self.edge_valid[fwd_idx] {
                    continue;
                }
                let target = self.col_idx_rev[rev_idx] as usize;
                let weight = self.values_rev[rev_idx];
                new_act[target] += a * weight * decay;
            }

            // Phase C: write-path patch edges pointing INTO this node.
            if let Some(patches) = self.patch_rev.get(&(i as u32)) {
                for p in patches {
                    if p.valid {
                        new_act[p.other as usize] += a * p.value * decay;
                    }
                }
            }
        }

        for a in &mut new_act {
            *a = a.clamp(-1.0, 1.0);
        }
        new_act
    }

    /// Full K-hop spreading activation (CA3 pattern completion).
    ///
    /// `reverse = false`: forward (decision → outcome)
    /// `run_hebbian = true`: update co-occurrence weights after retrieval (default
    ///   for external queries); `false` for internal calls like novelty detection
    ///   that should not mutate the graph as a side effect.
    pub fn spreading_activation(
        &mut self,
        query: &str,
        task_tag: Option<&str>,
        reverse: bool,
    ) -> Vec<ActivationResult> {
        self.spreading_activation_opts(query, task_tag, reverse, true)
    }

    /// Spreading activation with explicit Hebbian control.
    /// Pass `run_hebbian=false` for internal computations (novelty detection,
    /// consolidation preview) that should not have retrieval side effects.
    pub fn spreading_activation_opts(
        &mut self,
        query: &str,
        task_tag: Option<&str>,
        reverse: bool,
        run_hebbian: bool,
    ) -> Vec<ActivationResult> {
        // `reverse = true`: backward (outcome → decision, for trace_cause)
        let seeds = self.find_seeds(query, task_tag);
        if seeds.is_empty() {
            return Vec::new();
        }
        self.spread_and_collect(&seeds, reverse, run_hebbian)
    }

    /// Phase B (one-graph-convergence): seeded variant for the unified
    /// engine. Seeds arrive from the store's resolvers (persistent BM25
    /// index over ALL node types + optional semantic vectors) instead of
    /// substring matching alone; the graph's own substring matches union
    /// in. One spread over the whole typed graph follows.
    pub fn spreading_activation_seeded(
        &mut self,
        query: &str,
        seed_ids: &[String],
        task_tag: Option<&str>,
        run_hebbian: bool,
    ) -> Vec<ActivationResult> {
        let mut seeds = self.find_seeds(query, task_tag);
        for id in seed_ids {
            if let Some(&idx) = self.node_id_to_idx.get(id) {
                if !seeds.contains(&idx) {
                    seeds.push(idx);
                }
            }
        }
        if seeds.is_empty() {
            return Vec::new();
        }
        self.spread_and_collect(&seeds, false, run_hebbian)
    }

    /// The shared spread engine: Q-weighted seeding → hops (forward or
    /// reverse) with abs-max merge → threshold-collect → abs-sort →
    /// optional Hebbian update.
    ///
    /// Merge rule: uses absolute-value max (|new| > |old| → replace). This
    /// allows negative activations from prevented edges to replace zero
    /// (which signed-max could not: -0.126 > 0.0 is false). A node receiving
    /// both caused (+) and prevented (-) signals shows whichever is stronger.
    fn spread_and_collect(
        &mut self,
        seeds: &[u32],
        reverse: bool,
        run_hebbian: bool,
    ) -> Vec<ActivationResult> {
        let mut activations = vec![0.0_f32; self.num_nodes];
        let now = chrono::Utc::now().timestamp();
        for &seed in seeds {
            // P4: Q-value-weighted seeding. High-Q nodes (proven useful)
            // get stronger initial activation; low-Q nodes still seed but
            // weaker, so they don't dominate. Q defaults to 0.5 when unset.
            let q = self.node_q_value[seed as usize];
            activations[seed as usize] = 0.5 + 0.5 * q; // maps [0,1] Q → [0.5,1.0] seed
            self.node_last_activated[seed as usize] = now;
        }

        for _ in 0..self.max_hops {
            let new_act = if reverse {
                self.spread_step_rev(&activations, self.decay)
            } else {
                self.spread_step(&activations, self.decay)
            };

            let mut changed = false;
            for i in 0..self.num_nodes {
                if new_act[i].abs() >= self.threshold {
                    if activations[i].abs() < self.threshold {
                        changed = true;
                    }
                    // Merge: keep the value with larger absolute magnitude.
                    // This allows negative activations (from prevented edges) to
                    // replace zero, which signed-max cannot do.
                    // Design deviation #7 resolved: abs-max is correct because
                    // a node receiving both caused (+) and prevented (-) signals
                    // should show whichever is stronger.
                    if new_act[i].abs() > activations[i].abs() {
                        activations[i] = new_act[i];
                        self.node_last_activated[i] = now;
                    }
                }
            }

            if !changed {
                break;
            }
        }

        let mut results: Vec<ActivationResult> = activations
            .iter()
            .enumerate()
            .filter(|(i, &a)| a.abs() >= self.threshold && !self.retired_nodes.contains(&(*i as u32)))
            .map(|(i, &a)| ActivationResult {
                node_idx: i as u32,
                activation: a,
                text: self.node_text[i].clone(),
                task_tag: self.node_task_tag[i].clone(),
            })
            .collect();

        // Sort by absolute activation (strongest signal first, regardless of sign)
        // Sort by absolute activation descending (strongest first, regardless
        // of sign). total_cmp handles NaN deterministically (NaN sorts last),
        // so this never panics even if a future bug lets NaN leak in.
        results.sort_by(|a, b| b.activation.abs().total_cmp(&a.activation.abs()));

        // P2: Hebbian update — co-activated nodes wire together. Fire after
        // the activation set is known so frequently co-retrieved nodes
        // strengthen their associative connection over time. Skipped for
        // internal calls (novelty detection, consolidation preview) via
        // run_hebbian=false — those should not have retrieval side effects.
        if run_hebbian {
            let active: Vec<u32> = results.iter().map(|r| r.node_idx).collect();
            self.hebbian_update(&active, 0.995, 0.02);
        }

        results
    }

    /// CA1 novelty detection: compare predicted outcomes with actual.
    ///
    /// WARNING: text_jaccard_similarity uses whitespace tokenization.
    /// Chinese text (no spaces) produces one giant token, making similarity
    /// near-zero and surprise near-1.0 for everything. This is a known
    /// limitation (#10 in review). For Chinese-heavy use, switch to
    /// character bigrams or a real tokenizer.
    pub fn detect_novelty(&mut self, decision_text: &str, actual_outcome: &str) -> NoveltyReport {
        // Internal computation: use run_hebbian=false so novelty detection
        // doesn't have the side effect of strengthening co-occurrence edges.
        let predicted = self.spreading_activation_opts(decision_text, None, false, false);

        let predicted_positive: Vec<String> = predicted
            .iter()
            .filter(|r| r.activation > 0.0)
            .take(5)
            .map(|r| r.text.clone())
            .collect();

        let predicted_negative: Vec<String> = predicted
            .iter()
            .filter(|r| r.activation < 0.0)
            .take(3)
            .map(|r| r.text.clone())
            .collect();

        let predicted_text = predicted_positive.join(" ");
        let similarity = text_jaccard_similarity(&predicted_text, actual_outcome);
        let surprise = 1.0 - similarity;

        NoveltyReport {
            surprise,
            should_record: surprise > 0.5,
            predicted_positive,
            predicted_negative,
        }
    }

    // ─── P5: Hybrid novelty gating (Nemori FEP prediction gap) ────────────

    /// (P5) Novelty detection with a pluggable prediction-gap fallback.
    ///
    /// `predict` is the semantic-prediction closure: given the decision text,
    /// it returns what the model EXPECTS to happen (the caller wires the
    /// llm::chat call; tests supply a stub). `None` = prediction unavailable,
    /// the entropy verdict stands.
    ///
    /// - `Entropy`: existing word-frequency surprise, no LLM.
    /// - `PredictionGap`: the LLM's predicted outcome vs the actual outcome —
    ///   semantic surprise (Nemori's FEP prediction gap). Stronger but costs
    ///   one LLM call per check.
    /// - `Hybrid`: entropy first; only borderline surprises (0.4..=0.7)
    ///   pay for the LLM disambiguation.
    pub fn detect_novelty_with_mode(
        &mut self,
        decision_text: &str,
        actual_outcome: &str,
        mode: NoveltyMode,
        predict: &mut dyn FnMut(&str) -> Option<String>,
    ) -> NoveltyReport {
        let entropy_report = self.detect_novelty(decision_text, actual_outcome);
        match mode {
            NoveltyMode::Entropy => entropy_report,
            NoveltyMode::PredictionGap => {
                Self::prediction_gap_report(entropy_report, decision_text, actual_outcome, predict)
            }
            NoveltyMode::Hybrid => {
                if needs_prediction_gap(entropy_report.surprise) {
                    Self::prediction_gap_report(
                        entropy_report,
                        decision_text,
                        actual_outcome,
                        predict,
                    )
                } else {
                    entropy_report
                }
            }
        }
    }

    /// (P5) Replace the entropy verdict with the semantic prediction-gap
    /// verdict. Falls back to the entropy report when no prediction is
    /// available.
    fn prediction_gap_report(
        entropy_report: NoveltyReport,
        decision_text: &str,
        actual_outcome: &str,
        predict: &mut dyn FnMut(&str) -> Option<String>,
    ) -> NoveltyReport {
        match predict(decision_text) {
            Some(predicted) => {
                let similarity = text_jaccard_similarity(&predicted, actual_outcome);
                let surprise = 1.0 - similarity;
                NoveltyReport {
                    surprise,
                    should_record: surprise > 0.5,
                    predicted_positive: if predicted.is_empty() {
                        entropy_report.predicted_positive
                    } else {
                        vec![predicted]
                    },
                    predicted_negative: entropy_report.predicted_negative,
                }
            }
            None => entropy_report,
        }
    }

    /// SWR (Sharp-Wave Ripple) offline consolidation.
    ///
    /// Replays random causal chains:
    /// 1. Forward replay → LTP (strengthen edges, capped at WEIGHT_CAP)
    /// 2. Increment replay counts
    /// 3. Global LTD (decay all edges, protect well-replayed ones)
    /// 4. GC (forget edges below threshold with no replay history)
    ///
    /// Deviation #6 acknowledged: reverse replay / pattern detection is not yet
    /// implemented. patterns_detected stays 0. Future: walk chains in reverse,
    /// detect sub-chain similarity, create meta_causal_edges.
    pub fn swr_consolidate(&mut self, num_replays: usize) -> ConsolidationStats {
        let mut stats = ConsolidationStats::default();
        if self.num_nodes == 0 {
            return stats;
        }

        for _ in 0..num_replays {
            let seed = (rand_seed() as usize) % self.num_nodes;
            let chain = self.walk_chain(seed, self.max_hops);
            if chain.len() < 2 {
                continue;
            }
            stats.chains_replayed += 1;

            // LTP with cap (#8: prevent unbounded weight growth)
            for window in chain.windows(2) {
                let from = window[0] as usize;
                let to = window[1] as usize;
                if let Some(edge_idx) = self.find_edge(from, to) {
                    let raw = self.raw_weights[edge_idx];
                    // Bug fix #8: cap weight to prevent drift
                    self.raw_weights[edge_idx] = (raw * self.ltp_rate).min(WEIGHT_CAP);
                    self.values[edge_idx] =
                        self.raw_weights[edge_idx] * self.edge_relations[edge_idx].spread_coeff();
                    stats.ltp_events += 1;
                }
            }

            for &node_idx in &chain {
                self.node_replay_count[node_idx as usize] =
                    self.node_replay_count[node_idx as usize].saturating_add(1);
            }
        }

        // LTD: iterate by node→edge range (O(N+E), not O(N×E) via edge_source)
        for node_idx in 0..self.num_nodes {
            let start = self.row_ptr[node_idx] as usize;
            let end = self.row_ptr[node_idx + 1] as usize;
            let protection = if self.node_replay_count[node_idx] > 3 {
                0.5
            } else {
                1.0
            };
            for edge_idx in start..end {
                if !self.edge_valid[edge_idx] {
                    continue;
                }
                let raw = self.raw_weights[edge_idx];
                let new_raw = raw * (1.0 - (1.0 - self.ltd_rate) * protection);
                self.raw_weights[edge_idx] = new_raw;
                self.values[edge_idx] = new_raw * self.edge_relations[edge_idx].spread_coeff();
            }
        }

        // GC: iterate by node→edge range (O(N+E))
        for node_idx in 0..self.num_nodes {
            let start = self.row_ptr[node_idx] as usize;
            let end = self.row_ptr[node_idx + 1] as usize;
            for edge_idx in start..end {
                if !self.edge_valid[edge_idx] {
                    continue;
                }
                if self.raw_weights[edge_idx].abs() < self.gc_threshold
                    && self.node_replay_count[node_idx] == 0
                {
                    self.edge_valid[edge_idx] = false;
                    stats.forgotten += 1;
                }
            }
        }

        stats
    }

    // ─── P3: Immutable consolidation (Dreams-aligned) ─────────────────────

    /// SWR consolidation that produces an immutable result.
    ///
    /// Computes all LTP/LTD/GC changes as a delta, applies them to a **clone**
    /// of the graph, and returns the clone + full audit log. The original
    /// graph (`self`) is never mutated — the caller reviews the result and
    /// decides whether to swap.
    ///
    /// `instructions` is an optional high-level focus string (Dreams-style):
    /// "focus on causal lessons; ignore routine operations". It is carried in
    /// the result for auditability and future LLM-guided consolidation.
    pub fn swr_consolidate_immutable(
        &self,
        num_replays: usize,
        instructions: Option<&str>,
    ) -> ConsolidationResult {
        let mut new_graph = self.clone();
        let mut delta_log = Vec::new();
        let mut stats = ConsolidationStats::default();

        if new_graph.num_nodes == 0 {
            return ConsolidationResult {
                new_graph,
                delta_log,
                stats,
                instructions: instructions.map(|s| s.to_string()),
            };
        }

        for _ in 0..num_replays {
            let seed = (rand_seed() as usize) % new_graph.num_nodes;
            let chain = new_graph.walk_chain(seed, new_graph.max_hops);
            if chain.len() < 2 {
                continue;
            }
            stats.chains_replayed += 1;

            for window in chain.windows(2) {
                let from = window[0] as usize;
                let to = window[1] as usize;
                if let Some(edge_idx) = new_graph.find_edge(from, to) {
                    let old_raw = new_graph.raw_weights[edge_idx];
                    let new_raw = (old_raw * new_graph.ltp_rate).min(WEIGHT_CAP);
                    new_graph.raw_weights[edge_idx] = new_raw;
                    new_graph.values[edge_idx] =
                        new_raw * new_graph.edge_relations[edge_idx].spread_coeff();
                    delta_log.push(ConsolidationDelta {
                        op: "ltp",
                        edge_idx: edge_idx as u32,
                        node_idx: from as u32,
                        old_value: old_raw,
                        new_value: new_raw,
                    });
                    stats.ltp_events += 1;
                }
            }
            for &node_idx in &chain {
                let idx = node_idx as usize;
                let old = new_graph.node_replay_count[idx];
                new_graph.node_replay_count[idx] = old.saturating_add(1);
                delta_log.push(ConsolidationDelta {
                    op: "replay",
                    edge_idx: 0,
                    node_idx,
                    old_value: old as f32,
                    new_value: new_graph.node_replay_count[idx] as f32,
                });
            }
        }

        // LTD phase
        for node_idx in 0..new_graph.num_nodes {
            let start = new_graph.row_ptr[node_idx] as usize;
            let end = new_graph.row_ptr[node_idx + 1] as usize;
            let protection = if new_graph.node_replay_count[node_idx] > 3 {
                0.5
            } else {
                1.0
            };
            for edge_idx in start..end {
                if !new_graph.edge_valid[edge_idx] {
                    continue;
                }
                let old_raw = new_graph.raw_weights[edge_idx];
                let new_raw = old_raw * (1.0 - (1.0 - new_graph.ltd_rate) * protection);
                new_graph.raw_weights[edge_idx] = new_raw;
                new_graph.values[edge_idx] =
                    new_raw * new_graph.edge_relations[edge_idx].spread_coeff();
                if (old_raw - new_raw).abs() > 1e-9 {
                    delta_log.push(ConsolidationDelta {
                        op: "ltd",
                        edge_idx: edge_idx as u32,
                        node_idx: node_idx as u32,
                        old_value: old_raw,
                        new_value: new_raw,
                    });
                }
            }
        }

        // GC phase — triple criterion (HeLa-Mem adaptive forgetting):
        // delete only if weak AND zero replay AND dormant (not recently activated).
        let gc_age = 86400 * 30; // 30-day dormancy threshold (δ_age)
        let now = chrono::Utc::now().timestamp();
        for node_idx in 0..new_graph.num_nodes {
            let start = new_graph.row_ptr[node_idx] as usize;
            let end = new_graph.row_ptr[node_idx + 1] as usize;
            let dormant = now - new_graph.node_last_activated[node_idx] > gc_age;
            let zero_access = new_graph.node_replay_count[node_idx] == 0;
            for edge_idx in start..end {
                if !new_graph.edge_valid[edge_idx] {
                    continue;
                }
                if new_graph.raw_weights[edge_idx].abs() < new_graph.gc_threshold
                    && zero_access
                    && dormant
                {
                    new_graph.edge_valid[edge_idx] = false;
                    delta_log.push(ConsolidationDelta {
                        op: "gc",
                        edge_idx: edge_idx as u32,
                        node_idx: node_idx as u32,
                        old_value: new_graph.raw_weights[edge_idx],
                        new_value: 0.0,
                    });
                    stats.forgotten += 1;
                }
            }
        }

        ConsolidationResult {
            new_graph,
            delta_log,
            stats,
            instructions: instructions.map(|s| s.to_string()),
        }
    }

    // ─── P2: Hebbian co-occurrence weight update ──────────────────────────

    /// Update Hebbian co-occurrence edge weights based on the current
    /// activation set (HeLa-Mem formula 1): w(t+1) = (1-λ)·w(t) + η·𝕀(co-active).
    /// λ=0.995 (decay), η=0.02 (reinforcement). Called after each retrieval.
    pub fn hebbian_update(&mut self, active_nodes: &[u32], lambda: f32, eta: f32) {
        let active_set: std::collections::HashSet<u32> = active_nodes.iter().copied().collect();
        for edge_idx in 0..self.edge_relations.len() {
            if self.edge_relations[edge_idx] != Relation::CoOccurrence
                || !self.edge_valid[edge_idx]
            {
                continue;
            }
            let from = self.find_source_of_edge(edge_idx);
            let co_active =
                active_set.contains(&(from as u32)) && active_set.contains(&self.col_idx[edge_idx]);
            let old_w = self.raw_weights[edge_idx];
            let new_w = (1.0 - lambda) * old_w + if co_active { eta } else { 0.0 };
            let new_w = new_w.min(WEIGHT_CAP);
            self.raw_weights[edge_idx] = new_w;
            self.values[edge_idx] = new_w * Relation::CoOccurrence.spread_coeff();
        }
    }

    // ─── P4: Q-value dynamics (MemRL-style) ───────────────────────────────

    /// Bellman-style Q-value update: Q ← Q + α·[r + γ·max_Q(neighbors) − Q].
    /// Q_init = 0 for new nodes (per MemRL: failure is inherently valuable).
    pub fn update_q_value(&mut self, node_idx: u32, reward: f32, alpha: f32, gamma: f32) {
        let idx = node_idx as usize;
        if idx >= self.num_nodes {
            return;
        }
        let old_q = self.node_q_value[idx];
        let start = self.row_ptr[idx] as usize;
        let end = self.row_ptr[idx + 1] as usize;
        let max_next_q = (start..end)
            .filter(|&i| self.edge_valid[i])
            .map(|i| self.node_q_value[self.col_idx[i] as usize])
            .fold(0.0_f32, f32::max);
        let new_q = old_q + alpha * (reward + gamma * max_next_q - old_q);
        self.node_q_value[idx] = new_q.clamp(0.0, 1.0);
    }

    /// Q-learning by chunk id — the production entry point. Consolidation
    /// rewards the endpoint chunks of replay-protected edges; the server's
    /// seeding (`0.5 + 0.5·Q`) then favors proven-useful nodes on the next
    /// activation. Returns false when the chunk is not a graph node.
    pub fn update_q_value_by_chunk_id(
        &mut self,
        chunk_id: &str,
        reward: f32,
        alpha: f32,
        gamma: f32,
    ) -> bool {
        match self.node_id_to_idx.get(chunk_id) {
            Some(&idx) => {
                self.update_q_value(idx, reward, alpha, gamma);
                true
            }
            None => false,
        }
    }

    /// Write the graph's Q values back to `chunks.q_value` (v9 persistence) —
    /// without this, Bellman updates die with the in-memory graph and the
    /// learned utility never reaches the next session.
    pub fn persist_q_values(&self, store: &crate::store::CausalStore) -> anyhow::Result<()> {
        store.with_conn(|conn| {
            let mut stmt = conn.prepare("UPDATE chunks SET q_value = ?1 WHERE id = ?2")?;
            for (id, &idx) in &self.node_id_to_idx {
                stmt.execute(rusqlite::params![
                    self.node_q_value[idx as usize],
                    id
                ])?;
            }
            Ok(())
        })
    }

    // ─── P6: Novelty-entropy trigger ──────────────────────────────────────

    /// Entropy over replay-count buckets. High = diverse/surprising recent
    /// experience = worth consolidating. Returns 0.0–1.0; trigger at > 0.6.
    pub fn novelty_entropy(&self) -> f32 {
        if self.num_nodes == 0 {
            return 0.0;
        }
        let mut counts = [0u32; 8];
        for i in 0..self.num_nodes {
            let rc = self.node_replay_count[i] as usize;
            let bucket = match rc {
                0 => 0,
                1 => 1,
                2..=3 => 2,
                4..=7 => 3,
                8..=15 => 4,
                16..=31 => 5,
                32..=63 => 6,
                _ => 7,
            };
            counts[bucket] += 1;
        }
        let n = self.num_nodes as f32;
        let mut entropy = 0.0;
        for &c in &counts {
            if c == 0 {
                continue;
            }
            let p = c as f32 / n;
            entropy -= p * p.log2();
        }
        entropy / 3.0 // normalize by log2(8)
    }

    /// Walk a causal chain from a seed node (forward, along caused edges).
    fn walk_chain(&self, seed: usize, max_len: usize) -> Vec<u32> {
        let mut chain = vec![seed as u32];
        let mut current = seed;

        for _ in 0..max_len {
            let start = self.row_ptr[current] as usize;
            let end = self.row_ptr[current + 1] as usize;

            let next = (start..end)
                .find(|&i| self.edge_valid[i] && self.edge_relations[i] == Relation::Caused);

            match next {
                Some(edge_idx) => {
                    let target = self.col_idx[edge_idx];
                    if chain.contains(&target) {
                        break;
                    }
                    chain.push(target);
                    current = target as usize;
                }
                None => break,
            }
        }
        chain
    }

    /// Find edge index from source to target (forward).
    fn find_edge(&self, from: usize, to: usize) -> Option<usize> {
        let start = self.row_ptr[from] as usize;
        let end = self.row_ptr[from + 1] as usize;
        (start..end).find(|&i| self.col_idx[i] as usize == to)
    }

    /// Find the source node of an edge by CSR index (O(log N) binary search).
    fn find_source_of_edge(&self, edge_idx: usize) -> usize {
        self.row_ptr
            .partition_point(|&rp| (rp as usize) <= edge_idx)
            .saturating_sub(1)
    }

    /// Get the relation type of an edge by CSR index (for testing/debugging).
    pub fn edge_relation_at(&self, edge_idx: usize) -> Relation {
        self.edge_relations[edge_idx]
    }

    /// Ablation switch: zero out all `Prevented` (inhibitory) edge values.
    ///
    /// After calling this, prevented edges still exist in the graph structure
    /// but contribute zero spread — equivalent to treating them as `NoEffect`.
    /// Used by the inhibitory ablation experiment (paper §4.6) to isolate the
    /// contribution of negative activation.
    ///
    /// This is irreversible for the graph instance (rebuild from store to undo).
    pub fn disable_inhibition(&mut self) {
        // Zero forward values (O(E))
        for edge_idx in 0..self.edge_relations.len() {
            if self.edge_relations[edge_idx] == Relation::Prevented {
                self.values[edge_idx] = 0.0;
            }
        }
        // Zero reverse values in a single pass (O(E)) using rev_to_fwd_idx
        for rev_idx in 0..self.rev_to_fwd_idx.len() {
            let fwd_idx = self.rev_to_fwd_idx[rev_idx] as usize;
            if fwd_idx < self.edge_relations.len()
                && self.edge_relations[fwd_idx] == Relation::Prevented
            {
                self.values_rev[rev_idx] = 0.0;
            }
        }
    }

    /// Activation floor — nodes with |activation| below this are filtered
    /// from results (D1 uses it to decide which nodes count as co-active).
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Connectivity statistics over VALID edges (undirected BFS over the
    /// CSR). Returns (component_count, largest_component_size,
    /// isolated_node_count, valid_edge_count). A component is a weakly
    /// connected set of nodes via valid edges. Used to quantify
    /// one-graph convergence (facts linking isolated causal pairs into
    /// clusters) and to catch cross-scope link explosions at benchmark
    /// scale.
    pub fn component_stats(&self) -> (usize, usize, usize, usize) {
        let mut visited = vec![false; self.num_nodes];
        let mut comps: Vec<usize> = Vec::new();
        let valid_edges = self.edge_valid.iter().filter(|v| **v).count();
        for start in 0..self.num_nodes {
            if visited[start] {
                continue;
            }
            let mut stack = vec![start];
            visited[start] = true;
            let mut size = 0usize;
            while let Some(n) = stack.pop() {
                size += 1;
                for e in self.row_ptr[n] as usize..self.row_ptr[n + 1] as usize {
                    if !self.edge_valid[e] {
                        continue;
                    }
                    let t = self.col_idx[e] as usize;
                    if !visited[t] {
                        visited[t] = true;
                        stack.push(t);
                    }
                }
                for e in self.row_ptr_rev[n] as usize..self.row_ptr_rev[n + 1] as usize {
                    let fwd = self.rev_to_fwd_idx[e] as usize;
                    if !self.edge_valid[fwd] {
                        continue;
                    }
                    let t = self.col_idx_rev[e] as usize;
                    if !visited[t] {
                        visited[t] = true;
                        stack.push(t);
                    }
                }
            }
            comps.push(size);
        }
        let max = comps.iter().copied().max().unwrap_or(0);
        let isolated = comps.iter().filter(|&&c| c == 1).count();
        (comps.len(), max, isolated, valid_edges)
    }

    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }

    pub fn num_edges(&self) -> usize {
        self.col_idx.len()
    }

    pub fn num_valid_edges(&self) -> usize {
        self.edge_valid.iter().filter(|&&v| v).count()
    }

    pub fn node_text(&self, idx: usize) -> &str {
        &self.node_text[idx]
    }

    /// The chunk id backing a node (D1: co-occurrence edges are keyed by
    /// chunk id, so retrieval results need the id, not just the text).
    pub fn node_id(&self, idx: usize) -> &str {
        &self.node_ids[idx]
    }

    /// Phase B: does the graph know this node id? A store-resolved seed
    /// that maps to no node means the graph predates the write — the
    /// unified engine uses this as a freshness signal.
    pub fn has_node(&self, id: &str) -> bool {
        self.node_id_to_idx.contains_key(id)
    }

    /// Phase C: append a node to the graph (write-path patch). Node arrays
    /// are plain `Vec`s, so appending is O(1); the new node's CSR rows
    /// start empty (zero-width) — its edges go through [`add_patch_edge`].
    /// Returns the new node index. No-op (returns the existing index) when
    /// the id already exists — callers patch after any write, including
    /// chunk reuse where only the edge is new.
    pub fn append_node(&mut self, data: NodeData) -> u32 {
        if let Some(&idx) = self.node_id_to_idx.get(&data.id) {
            return idx;
        }
        let idx = self.num_nodes as u32;
        self.node_id_to_idx.insert(data.id.clone(), idx);
        self.node_text.push(data.text);
        let idx_id = data.id;
        self.node_ids.push(idx_id.clone());
        self.node_q_value.push(data.q_value);
        self.node_replay_count.push(data.replay_count);
        self.node_last_activated.push(data.last_activated);
        self.node_event_time.push(data.event_time);
        self.node_sparse_code.push(crate::hippocampus::utils::simhash(&self.node_text[idx as usize]));
        self.node_task_tag.push(data.task_tag);
        self.node_scope.push(data.scope);
        self.num_nodes += 1;
        // Zero-width CSR rows for the new node on both sides.
        self.row_ptr.push(*self.row_ptr.last().unwrap_or(&0));
        self.row_ptr_rev.push(*self.row_ptr_rev.last().unwrap_or(&0));
        // Phase C: keep the incremental token index current for chunk
        // nodes (fact/scope nodes are not link targets). Posting lists are
        // deduped (distinct tokens only), matching the rebuild-time linker.
        let is_link_target =
            !(idx_id.starts_with("fact:") || idx_id.starts_with("scope:"));
        if is_link_target {
            let distinct: std::collections::HashSet<String> =
                crate::patterns::tokenize(&self.node_text[idx as usize])
                    .into_iter()
                    .collect();
            for tok in distinct {
                self.token_index.entry(tok).or_default().push(idx);
            }
        }
        idx
    }

    /// Phase C: add an edge between (possibly just-appended) nodes without
    /// rebuilding. The edge lives in the overlay maps until the next full
    /// `from_store`; spread steps consult it in both directions.
    /// Idempotent: a duplicate (from, to, relation) — e.g. an idempotent
    /// fact re-record — updates the weight instead of stacking a copy, so
    /// repeated writes never inflate activation or grow the overlay.
    pub fn add_patch_edge(&mut self, from: u32, to: u32, relation: Relation, weight: f32) {
        let value = weight * relation.spread_coeff();
        // Reviving write: a re-appended node leaves the retired set (the
        // store-side revive path re-records a previously retired fact).
        self.retired_nodes.remove(&from);
        self.retired_nodes.remove(&to);
        let upsert = |patches: &mut Vec<PatchEdge>, other: u32, value: f32| {
            if let Some(p) = patches.iter_mut().find(|p| p.other == other) {
                p.value = value;
                p.valid = true;
            } else {
                patches.push(PatchEdge {
                    other,
                    value,
                    valid: true,
                });
            }
        };
        upsert(self.patch_fwd.entry(from).or_default(), to, value);
        upsert(self.patch_rev.entry(to).or_default(), from, value);
    }

    /// Phase C: retire a node from seeding and result surfacing (a fact
    /// replaced under the same key). Its edges stay so activation may
    /// still bridge THROUGH it, but its own text never appears; the next
    /// full rebuild removes it completely. Returns true when the id was
    /// live in the graph.
    pub fn retire_node(&mut self, id: &str) -> bool {
        match self.node_id_to_idx.get(id) {
            Some(&idx) => self.retired_nodes.insert(idx),
            None => false,
        }
    }

    /// Phase C: flip validity off for every edge (CSR or patch) between the
    /// two chunk ids — the O(1)-amortized graph reaction to
    /// `invalidate_decision`, so a falsified lesson stops spreading
    /// immediately instead of after the next lazy rebuild.
    /// Returns the number of edges flipped.
    pub fn invalidate_edges_between(&mut self, from_id: &str, to_id: &str) -> usize {
        let (Some(&from), Some(&to)) =
            (self.node_id_to_idx.get(from_id), self.node_id_to_idx.get(to_id))
        else {
            return 0;
        };
        let mut flipped = 0usize;
        // CSR forward rows of `from`.
        let start = self.row_ptr[from as usize] as usize;
        let end = self.row_ptr[from as usize + 1] as usize;
        for edge_idx in start..end {
            if self.col_idx[edge_idx] == to && self.edge_valid[edge_idx] {
                self.edge_valid[edge_idx] = false;
                flipped += 1;
            }
        }
        // Overlay edges in both directions.
        if let Some(patches) = self.patch_fwd.get_mut(&from) {
            for p in patches {
                if p.other == to && p.valid {
                    p.valid = false;
                    flipped += 1;
                }
            }
        }
        if let Some(patches) = self.patch_rev.get_mut(&to) {
            for p in patches {
                if p.other == from && p.valid {
                    p.valid = false;
                    flipped += 1;
                }
            }
        }
        flipped
    }

    /// Phase C: entity-link one (just-appended) fact node against the
    /// chunk nodes already in the graph — the in-graph mirror of
    /// `entity_link_facts`, using the same thresholds and weights, so a
    /// fresh fact is wired into the causal content immediately (not after
    /// the next rebuild). Facts link to chunks sharing
    /// ≥ [`FACT_LINK_MIN_TOKENS`] distinct tokens; bidirectional; capped
    /// at [`FACT_LINK_MAX_PER_FACT`].
    pub fn link_fact_node(&mut self, fact_idx: u32) {
        let fact_text = self.node_text[fact_idx as usize].clone();
        let fact_scope = self.node_scope[fact_idx as usize].clone();
        let fact_tokens: std::collections::HashSet<String> =
            crate::patterns::tokenize(&fact_text)
                .into_iter()
                .filter(|t| !LINK_STOPWORDS.contains(&t.as_str()))
                // df filter: a token present in > FACT_LINK_DF_LIMIT chunks
                // (posting-list length) is too generic to count toward a link.
                .filter(|t| {
                    self.token_index
                        .get(t)
                        .map(|v| v.len() <= FACT_LINK_DF_LIMIT)
                        .unwrap_or(true)
                })
                .collect();
        if fact_tokens.is_empty() {
            return;
        }

        // Distinct-shared-token count per in-scope chunk node via the
        // incremental inverted index — O(fact tokens × hits), no per-node
        // re-tokenize. Scope isolation mirrors entity_link_facts: a
        // colon-namespaced fact scope links only to chunks whose task_tag
        // matches the scope suffix.
        let mut overlap: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
        for tok in &fact_tokens {
            if let Some(chunks) = self.token_index.get(tok) {
                for &ci in chunks {
                    if !scope_matches(
                        fact_scope.as_deref(),
                        self.node_task_tag[ci as usize].as_deref(),
                    ) {
                        continue;
                    }
                    *overlap.entry(ci).or_insert(0) += 1;
                }
            }
        }

        let mut linked: Vec<(u32, usize)> = overlap
            .into_iter()
            .filter(|&(_, n)| n >= FACT_LINK_MIN_TOKENS)
            .collect();
        linked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        linked.truncate(FACT_LINK_MAX_PER_FACT);
        for (ci, n) in linked {
            let weight = (0.3 + 0.1 * n as f32).min(0.8);
            self.add_patch_edge(fact_idx, ci, Relation::Fact, weight);
            self.add_patch_edge(ci, fact_idx, Relation::Fact, weight);
        }
    }

    pub fn node_q_value(&self, idx: usize) -> f32 {
        self.node_q_value[idx]
    }

    pub fn node_replay_count(&self, idx: usize) -> u16 {
        self.node_replay_count[idx]
    }

    /// Get raw weight of an edge by forward index.
    pub fn edge_raw_weight(&self, edge_idx: usize) -> f32 {
        self.raw_weights[edge_idx]
    }

    /// Check if a forward edge is valid.
    pub fn edge_is_valid(&self, edge_idx: usize) -> bool {
        self.edge_valid[edge_idx]
    }

    // ─── Refutation support: graph query helpers ────────────────────

    /// Find the source node of a given edge (binary search on row_ptr).
    pub fn edge_source_node(&self, edge_idx: usize) -> u32 {
        self.row_ptr
            .partition_point(|&rp| (rp as usize) <= edge_idx)
            .saturating_sub(1) as u32
    }

    /// All neighbors (both in and out) of a node.
    pub fn all_neighbors(&self, node: u32) -> Vec<u32> {
        let mut neighbors = HashSet::new();
        // Forward neighbors
        let start = self.row_ptr[node as usize] as usize;
        let end = self.row_ptr[(node + 1) as usize] as usize;
        for i in start..end {
            neighbors.insert(self.col_idx[i]);
        }
        // Reverse neighbors
        let start_r = self.row_ptr_rev[node as usize] as usize;
        let end_r = self.row_ptr_rev[(node + 1) as usize] as usize;
        for i in start_r..end_r {
            neighbors.insert(self.col_idx_rev[i]);
        }
        neighbors.into_iter().collect()
    }

    /// Out-degree of a node (valid edges only).
    pub fn out_degree(&self, node: u32) -> usize {
        let start = self.row_ptr[node as usize] as usize;
        let end = self.row_ptr[(node + 1) as usize] as usize;
        (start..end).filter(|&i| self.edge_valid[i]).count()
    }

    /// In-degree of a node (valid edges only).
    pub fn in_degree(&self, node: u32) -> usize {
        let start = self.row_ptr_rev[node as usize] as usize;
        let end = self.row_ptr_rev[(node + 1) as usize] as usize;
        (start..end)
            .filter(|&i| {
                let fwd = self.rev_to_fwd_idx[i] as usize;
                self.edge_valid[fwd]
            })
            .count()
    }

    /// Out-neighbors of a node, with edge indices.
    /// Returns Vec<(target_node, edge_idx)>.
    pub fn out_neighbors_of(&self, node: u32) -> Vec<(u32, usize)> {
        let start = self.row_ptr[node as usize] as usize;
        let end = self.row_ptr[(node + 1) as usize] as usize;
        (start..end)
            .map(|i| (self.col_idx[i], i))
            .collect()
    }

    /// Target node of a given edge index.
    pub fn edge_target(&self, edge_idx: usize) -> u32 {
        self.col_idx[edge_idx]
    }
}

impl Default for CausalGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ─── SQLite loading ────────────────────────────────────────────────

/// Phase A (one-graph-convergence): minimum number of distinct shared
/// tokens before an entity link is created between a fact node and a
/// chunk node. Conservative on purpose — a false link wires unrelated
/// activation into every downstream query.
const FACT_LINK_MIN_TOKENS: usize = 3;
/// Tokens appearing in more than this many chunks are too generic to drive
/// a fact↔chunk link (audit 2026-08-21: df>20 tokens like dates/"agent"/"server"
/// produced the bulk of false positives; df<=20 + >=3 tokens doubles link
/// precision, 17% -> 33% strict / 29% -> 75% lenient).
const FACT_LINK_DF_LIMIT: usize = 20;

/// Phase A: max chunk links per fact — keeps a generic fact (key like
/// `language`) from wiring itself to half the store.
const FACT_LINK_MAX_PER_FACT: usize = 8;

impl CausalGraph {
    /// Load graph from a CausalStore's SQLite database.
    ///
    /// Bug fix #2: uses node_id_to_idx for O(1) lookups during tag/q_value
    /// propagation, instead of O(N) inner loop per edge.
    pub fn from_store(store: &crate::store::CausalStore) -> anyhow::Result<Self> {
        store.with_conn(|conn| {
            // Load chunks
            let mut node_stmt =
                conn.prepare("SELECT id, text, created_at, q_value FROM chunks ORDER BY created_at ASC")?;
            let node_rows = node_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, f32>(3)?,
                ))
            })?;

            let mut nodes: Vec<NodeData> = Vec::new();
            let mut id_to_idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for row in node_rows {
                let (id, text, event_time, q_value) = row?;
                id_to_idx.insert(id.clone(), nodes.len());
                nodes.push(NodeData {
                    id,
                    text,
                    event_time,
                    q_value,
                    replay_count: 0,
                    last_activated: 0,
                    task_tag: None,
                    scope: None,
                });
            }

            // Load edges
            let mut edge_stmt = conn.prepare(
                "SELECT from_id, to_id, relation, confidence, valid_to, task_tag
                 FROM causal_edges ORDER BY event_time ASC",
            )?;
            let mut edges: Vec<EdgeData> = Vec::new();
            let edge_rows = edge_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,         // from_id
                    row.get::<_, String>(1)?,         // to_id
                    row.get::<_, String>(2)?,         // relation
                    row.get::<_, f64>(3)?,            // confidence
                    row.get::<_, Option<i64>>(4)?,    // valid_to
                    row.get::<_, Option<String>>(5)?, // task_tag
                ))
            })?;

            for row in edge_rows {
                let (from_id, to_id, relation_str, confidence, valid_to, task_tag) = row?;
                let relation = Relation::from_str_lossy(&relation_str);
                let valid = valid_to.is_none();

                // Bug fix #2: O(1) lookup via id_to_idx, not O(N) scan
                if let Some(ref tag) = task_tag {
                    if let Some(&fi) = id_to_idx.get(&from_id) {
                        if nodes[fi].task_tag.is_none() {
                            nodes[fi].task_tag = Some(tag.clone());
                        }
                    }
                    if let Some(&ti) = id_to_idx.get(&to_id) {
                        if nodes[ti].task_tag.is_none() {
                            nodes[ti].task_tag = Some(tag.clone());
                        }
                    }
                }
                if let Some(&fi) = id_to_idx.get(&from_id) {
                    if nodes[fi].q_value == 0.5 {
                        nodes[fi].q_value = confidence as f32;
                    }
                }

                edges.push(EdgeData {
                    from_id,
                    to_id,
                    relation,
                    weight: confidence as f32,
                    valid,
                });
            }

            // P1: Load agent_facts as fact-type nodes + fact-type edges.
            // Phase A: fact_indices feeds the entity linking below.
            let mut fact_indices: Vec<usize> = Vec::new();
            let mut fact_stmt = conn.prepare(
                "SELECT id, key, value, scope, confidence FROM agent_facts WHERE valid_to IS NULL",
            )?;
            let fact_rows = fact_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            })?;
            for row in fact_rows {
                let (fact_id, key, value, scope, confidence) = row?;
                let fact_node_id = format!("fact:{fact_id}");
                let scope_node_id = format!("scope:{scope}");
                if !id_to_idx.contains_key(&scope_node_id) {
                    id_to_idx.insert(scope_node_id.clone(), nodes.len());
                    nodes.push(NodeData {
                        id: scope_node_id.clone(),
                        text: format!("[{scope} scope]"),
                        event_time: 0,
                        q_value: 0.5,
                        replay_count: 0,
                        last_activated: 0,
                        task_tag: None,
                        scope: Some(scope.clone()),
                    });
                }
                fact_indices.push(nodes.len());
                id_to_idx.insert(fact_node_id.clone(), nodes.len());
                nodes.push(NodeData {
                    id: fact_node_id.clone(),
                    text: format!("{key}: {value}"),
                    event_time: 0,
                    q_value: confidence as f32,
                    replay_count: 0,
                    last_activated: 0,
                    task_tag: Some(key.clone()),
                    scope: Some(scope.clone()),
                });
                edges.push(EdgeData {
                    from_id: scope_node_id,
                    to_id: fact_node_id,
                    relation: Relation::Fact,
                    weight: confidence as f32,
                    valid: true,
                });
            }

            // P1: Load meta_causal_edges as Meta-type edges (+0.6 spread).
            let has_meta = conn
                .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta_causal_edges'")?
                .query_row([], |r| r.get::<_, i64>(0))?
                > 0;
            if has_meta {
                let mut meta_stmt = conn.prepare(
                    "SELECT from_id, to_id, confidence FROM meta_causal_edges WHERE valid_to IS NULL",
                )?;
                let meta_rows = meta_stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                })?;
                for row in meta_rows {
                    let (m_from, m_to, m_conf) = row?;
                    if id_to_idx.contains_key(&m_from) && id_to_idx.contains_key(&m_to) {
                        edges.push(EdgeData {
                            from_id: m_from,
                            to_id: m_to,
                            relation: Relation::Meta,
                            weight: m_conf as f32,
                            valid: true,
                        });
                    }
                }
            }

            // D1: Hebbian co-occurrence edges — weak associative links
            // between chunks co-activated in retrieval, loaded from the
            // persistent table so learned associations survive restarts.
            let has_cooc = conn
                .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cooccurrence_edges'")?
                .query_row([], |r| r.get::<_, i64>(0))?
                > 0;
            if has_cooc {
                let mut cooc_stmt = conn.prepare(
                    "SELECT from_id, to_id, weight FROM cooccurrence_edges",
                )?;
                let cooc_rows = cooc_stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                })?;
                for row in cooc_rows {
                    let (c_from, c_to, c_w) = row?;
                    if id_to_idx.contains_key(&c_from) && id_to_idx.contains_key(&c_to) {
                        edges.push(EdgeData {
                            from_id: c_from,
                            to_id: c_to,
                            relation: Relation::CoOccurrence,
                            weight: c_w as f32,
                            valid: true,
                        });
                    }
                }
            }

            // Phase A: entity-link fact nodes into the causal content
            // graph — pure function over node data (unit-testable).
            edges.extend(entity_link_facts(&nodes, &fact_indices));

            Ok(Self::build(&nodes, &edges))
        })
    }
}

/// Phase A (one-graph-convergence): entity-link fact nodes into the causal
/// content graph (deterministic, no LLM). Facts otherwise form isolated
/// scope-hub islands, so spreading activation can never cross between
/// lessons and facts. A fact links to a chunk when they share
/// ≥ [`FACT_LINK_MIN_TOKENS`] **distinct** tokens (`patterns::tokenize`:
/// ASCII words + CJK bigrams). Edges are created in BOTH directions —
/// fact seeds reach causal chains, causal seeds surface facts. An inverted
/// token→chunk index keeps this O(total tokens) instead of
/// O(facts × chunks).
///
/// Scope isolation (Phase A hardening, 2026-08-21): a fact with a
/// colon-namespaced scope ("lme:{qid}", "tenant:acme") links ONLY to
/// chunks whose task_tag matches the scope suffix — 500-question corpora
/// share one store, and cross-question links would both pollute the
/// isolation boundary and explode the graph (48万 nodes, up to 144万
/// spurious edges). Canonical scopes (user/session/agent, the
/// single-agent store) keep the original all-chunk behavior so real
/// memory stays connected. Link tokens that are too generic to be
/// discriminative are skipped (retrieval stopwords are separate).
///
/// Pure function over node data — unit-testable without a store.
/// Tokens too generic to drive a fact↔chunk link (a fact sharing only
/// "user"+"project" with a chunk is not a semantic connection).
const LINK_STOPWORDS: &[&str] = &[
    "user", "project", "code", "build", "using", "used", "want", "like",
    "get", "got", "make", "made", "need", "way", "work", "worked",
    "thing", "stuff", "issue", "problem", "fix", "fixed", "use", "went",
];

/// Colon-namespaced fact scope → strict chunk-task_tag isolation.
fn scope_matches(fact_scope: Option<&str>, chunk_tag: Option<&str>) -> bool {
    match fact_scope {
        Some(s) if s.contains(':') => chunk_tag
            .map(|t| t == s.rsplit(':').next().unwrap_or(s))
            .unwrap_or(false),
        _ => true, // canonical scope / no scope: single-agent store
    }
}

fn entity_link_facts(nodes: &[NodeData], fact_indices: &[usize]) -> Vec<EdgeData> {
    // Inverted token → chunk index. Posting lists are deduped at build
    // time (a chunk whose text repeats a token appears once per token), so
    // the overlap count below is the number of DISTINCT shared tokens —
    // exactly what the threshold documents.
    let mut token_to_chunks: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if node.id.starts_with("fact:") || node.id.starts_with("scope:") {
            continue; // only chunk nodes are link targets
        }
        let distinct: std::collections::HashSet<String> =
            crate::patterns::tokenize(&node.text).into_iter().collect();
        for tok in distinct {
            token_to_chunks.entry(tok).or_default().push(i);
        }
    }

    let mut edges = Vec::new();
    for &fi in fact_indices {
        let fact = &nodes[fi];
        let fact_id = fact.id.clone();
        let fact_tokens: std::collections::HashSet<String> =
            crate::patterns::tokenize(&fact.text)
                .into_iter()
                .filter(|t| !LINK_STOPWORDS.contains(&t.as_str()))
                // df filter: a token present in > FACT_LINK_DF_LIMIT chunks
                // (posting-list length) is too generic to count toward a link.
                .filter(|t| token_to_chunks.get(t).map(|v| v.len() <= FACT_LINK_DF_LIMIT).unwrap_or(true))
                .collect();
        if fact_tokens.is_empty() {
            continue;
        }

        // Distinct-shared-token count per in-scope chunk (idx → overlap).
        let mut overlap: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for tok in &fact_tokens {
            if let Some(chunks) = token_to_chunks.get(tok) {
                for &ci in chunks {
                    if !scope_matches(fact.scope.as_deref(), nodes[ci].task_tag.as_deref()) {
                        continue;
                    }
                    *overlap.entry(ci).or_insert(0) += 1;
                }
            }
        }

        // Conservative threshold; deterministic order (overlap desc, idx
        // asc); per-fact cap.
        let mut linked: Vec<(usize, usize)> = overlap
            .into_iter()
            .filter(|&(_, n)| n >= FACT_LINK_MIN_TOKENS)
            .collect();
        linked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        linked.truncate(FACT_LINK_MAX_PER_FACT);

        for (ci, n) in linked {
            let weight = (0.3 + 0.1 * n as f32).min(0.8);
            edges.push(EdgeData {
                from_id: fact_id.clone(),
                to_id: nodes[ci].id.clone(),
                relation: Relation::Fact,
                weight,
                valid: true,
            });
            edges.push(EdgeData {
                from_id: nodes[ci].id.clone(),
                to_id: fact_id.clone(),
                relation: Relation::Fact,
                weight,
                valid: true,
            });
        }
    }
    edges
}

#[cfg(test)]
mod tests;
