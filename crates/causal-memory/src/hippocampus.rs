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

use std::collections::HashMap;

/// Causal relation type — determines spread coefficient.
/// Inspired by neurotransmitter types:
///   Caused   = excitatory (glutamate) → strong positive spread
///   Enabled  = weak excitatory       → mild positive spread
///   Prevented = inhibitory (GABA)    → NEGATIVE spread (unique to causal-memory)
///   NoEffect = no connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Relation {
    Caused = 0,
    Enabled = 1,
    Prevented = 2,
    NoEffect = 3,
}

impl Relation {
    /// Spread coefficient for activation diffusion.
    /// Pre-multiplied into edge values at build time.
    #[inline]
    pub fn spread_coeff(self) -> f32 {
        match self {
            Relation::Caused => 1.0,
            Relation::Enabled => 0.5,
            Relation::Prevented => -0.3,
            Relation::NoEffect => 0.0,
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "caused" => Relation::Caused,
            "enabled" => Relation::Enabled,
            "prevented" => Relation::Prevented,
            _ => Relation::NoEffect,
        }
    }
}

/// CSR-format causal graph with SoA node attributes.
///
/// Memory layout (all contiguous arrays):
///   row_ptr:   [u32; N+1]  — row i's edges are col_idx[row_ptr[i]..row_ptr[i+1]]
///   col_idx:   [u32; E]    — target node index for each edge
///   values:    [f32; E]    — pre-multiplied weight × spread_coeff
///   rev_*:     same for reverse traversal (trace_cause)
///
/// Hot path (spreading_activation) only touches:
///   row_ptr + col_idx + values + node_activation
/// All are contiguous f32/u32 arrays → zero cache misses.
pub struct CausalGraph {
    num_nodes: usize,

    // Forward CSR (decision → outcome)
    row_ptr: Vec<u32>,
    col_idx: Vec<u32>,
    values: Vec<f32>,
    // Raw weights (before spread_coeff multiplication, for LTP/LTD)
    raw_weights: Vec<f32>,
    edge_relations: Vec<Relation>,
    edge_valid: Vec<bool>,

    // Reverse CSR (outcome → decision, for trace_cause)
    row_ptr_rev: Vec<u32>,
    col_idx_rev: Vec<u32>,
    values_rev: Vec<f32>,

    // Node attributes (SoA — Structure of Arrays)
    node_text: Vec<String>,
    node_activation: Vec<f32>,
    node_q_value: Vec<f32>,
    node_replay_count: Vec<u16>,
    node_last_activated: Vec<i64>,
    node_event_time: Vec<i64>,
    node_sparse_code: Vec<u128>,
    node_task_tag: Vec<Option<String>>,
    node_id_to_idx: HashMap<String, u32>,

    // Config
    decay: f32,
    threshold: f32,
    max_hops: usize,
    ltp_rate: f32,
    ltd_rate: f32,
    gc_threshold: f32,
}

/// Result of a spreading activation query.
#[derive(Debug, Clone)]
pub struct ActivationResult {
    pub node_idx: u32,
    pub activation: f32,
    pub text: String,
    pub task_tag: Option<String>,
}

/// Result of novelty detection.
#[derive(Debug, Clone)]
pub struct NoveltyReport {
    pub surprise: f32,
    pub should_record: bool,
    pub predicted_positive: Vec<String>,
    pub predicted_negative: Vec<String>,
}

/// Result of SWR consolidation.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationStats {
    pub chains_replayed: usize,
    pub ltp_events: usize,
    pub patterns_detected: usize,
    pub forgotten: usize,
}

/// Edge data for building the graph.
#[derive(Debug, Clone)]
pub struct EdgeData {
    pub from_id: String,
    pub to_id: String,
    pub relation: Relation,
    pub weight: f32,
    pub valid: bool,
}

/// Node data for building the graph.
#[derive(Debug, Clone)]
pub struct NodeData {
    pub id: String,
    pub text: String,
    pub event_time: i64,
    pub q_value: f32,
    pub replay_count: u16,
    pub last_activated: i64,
    pub task_tag: Option<String>,
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
            node_text: Vec::new(),
            node_activation: Vec::new(),
            node_q_value: Vec::new(),
            node_replay_count: Vec::new(),
            node_last_activated: Vec::new(),
            node_event_time: Vec::new(),
            node_sparse_code: Vec::new(),
            node_task_tag: Vec::new(),
            node_id_to_idx: HashMap::new(),
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

        // Build node lookup
        for (i, node) in nodes.iter().enumerate() {
            graph.node_id_to_idx.insert(node.id.clone(), i as u32);
        }

        // SoA node attributes
        graph.node_text = nodes.iter().map(|n| n.text.clone()).collect();
        graph.node_activation = vec![0.0; nodes.len()];
        graph.node_q_value = nodes.iter().map(|n| n.q_value).collect();
        graph.node_replay_count = nodes.iter().map(|n| n.replay_count).collect();
        graph.node_last_activated = nodes.iter().map(|n| n.last_activated).collect();
        graph.node_event_time = nodes.iter().map(|n| n.event_time).collect();
        graph.node_sparse_code = nodes.iter().map(|n| simhash(&n.text)).collect();
        graph.node_task_tag = nodes.iter().map(|n| n.task_tag.clone()).collect();

        // Build forward CSR
        let mut adj: Vec<Vec<(u32, f32, f32, Relation, bool)>> = vec![Vec::new(); nodes.len()];

        for edge in edges {
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

        graph.row_ptr = Vec::with_capacity(nodes.len() + 1);
        graph.row_ptr.push(0);
        for node_edges in &adj {
            graph
                .row_ptr
                .push(graph.col_idx.len() as u32 + node_edges.len() as u32);
            for &(target, raw_w, val, rel, valid) in node_edges {
                graph.col_idx.push(target);
                graph.values.push(val);
                graph.raw_weights.push(raw_w);
                graph.edge_relations.push(rel);
                graph.edge_valid.push(valid);
            }
        }

        // Build reverse CSR
        let mut adj_rev: Vec<Vec<(u32, f32)>> = vec![Vec::new(); nodes.len()];
        for (i, node_edges) in adj.iter().enumerate() {
            for &(target, _raw_w, val, _rel, _valid) in node_edges {
                adj_rev[target as usize].push((i as u32, val));
            }
        }

        graph.row_ptr_rev = Vec::with_capacity(nodes.len() + 1);
        graph.row_ptr_rev.push(0);
        for node_edges in &adj_rev {
            graph
                .row_ptr_rev
                .push(graph.col_idx_rev.len() as u32 + node_edges.len() as u32);
            for &(target, val) in node_edges {
                graph.col_idx_rev.push(target);
                graph.values_rev.push(val);
            }
        }

        graph
    }

    /// Find seed nodes by text matching (simplified — future: embedding similarity).
    fn find_seeds(&self, query: &str, task_tag: Option<&str>) -> Vec<u32> {
        let query_lower = query.to_lowercase();
        let mut seeds = Vec::new();

        for i in 0..self.num_nodes {
            // Task tag filter
            if let Some(tag) = task_tag {
                if self.node_task_tag[i].as_deref() != Some(tag) {
                    continue;
                }
            }
            // Text matching
            if self.node_text[i].to_lowercase().contains(&query_lower) {
                seeds.push(i as u32);
            }
        }

        seeds
    }

    /// Core: single-hop spreading activation step (SpMV-style).
    ///
    /// This is the hottest function. It only touches 4 contiguous arrays:
    ///   node_activation (read), row_ptr (read), col_idx (read), values (read)
    /// Output: new_act (write)
    #[inline]
    fn spread_step(&self, activations: &[f32], decay: f32) -> Vec<f32> {
        let mut new_act = vec![0.0_f32; self.num_nodes];

        for i in 0..self.num_nodes {
            let a = activations[i];
            if a.abs() < self.threshold {
                continue;
            }

            // Get node i's outgoing edges (contiguous slice!)
            let start = self.row_ptr[i] as usize;
            let end = self.row_ptr[i + 1] as usize;

            for edge_idx in start..end {
                if !self.edge_valid[edge_idx] {
                    continue;
                }
                let target = self.col_idx[edge_idx] as usize;
                let weight = self.values[edge_idx]; // pre-multiplied: raw_weight × spread_coeff
                new_act[target] += a * weight * decay;
            }
        }

        // Clamp
        for a in &mut new_act {
            *a = a.clamp(-1.0, 1.0);
        }

        new_act
    }

    /// Reverse single-hop step (for trace_cause: outcome → decision).
    #[inline]
    fn spread_step_rev(&self, activations: &[f32], decay: f32) -> Vec<f32> {
        let mut new_act = vec![0.0_f32; self.num_nodes];

        for i in 0..self.num_nodes {
            let a = activations[i];
            if a.abs() < self.threshold {
                continue;
            }

            let start = self.row_ptr_rev[i] as usize;
            let end = self.row_ptr_rev[i + 1] as usize;

            for edge_idx in start..end {
                let target = self.col_idx_rev[edge_idx] as usize;
                let weight = self.values_rev[edge_idx];
                new_act[target] += a * weight * decay;
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
    /// `reverse = true`:  backward (outcome → decision, for trace_cause)
    pub fn spreading_activation(
        &mut self,
        query: &str,
        task_tag: Option<&str>,
        reverse: bool,
    ) -> Vec<ActivationResult> {
        let seeds = self.find_seeds(query, task_tag);
        if seeds.is_empty() {
            return Vec::new();
        }

        // Initialize activations
        let mut activations = vec![0.0_f32; self.num_nodes];
        let now = chrono::Utc::now().timestamp();
        for &seed in &seeds {
            activations[seed as usize] = 1.0;
            self.node_last_activated[seed as usize] = now;
        }

        // K-hop diffusion
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
                    // Winner-takes-all: keep the stronger activation
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

        // Collect results sorted by |activation|
        let mut results: Vec<ActivationResult> = activations
            .iter()
            .enumerate()
            .filter(|(_, &a)| a.abs() >= self.threshold)
            .map(|(i, &a)| ActivationResult {
                node_idx: i as u32,
                activation: a,
                text: self.node_text[i].clone(),
                task_tag: self.node_task_tag[i].clone(),
            })
            .collect();

        results.sort_by(|a, b| b.activation.abs().partial_cmp(&a.activation.abs()).unwrap());
        results
    }

    /// CA1 novelty detection: compare predicted outcomes with actual.
    ///
    /// Uses spreading activation from the decision to predict expected outcomes,
    /// then compares with the actual outcome text.
    pub fn detect_novelty(&mut self, decision_text: &str, actual_outcome: &str) -> NoveltyReport {
        // Predict: forward spread from decision
        let predicted = self.spreading_activation(decision_text, None, false);

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

        // Compare predicted vs actual (simplified text similarity)
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

    /// SWR (Sharp-Wave Ripple) offline consolidation.
    ///
    /// Replays random causal chains:
    /// 1. Forward replay → LTP (strengthen edges along the chain)
    /// 2. Reverse replay → pattern detection
    /// 3. Global LTD (decay all edges, protect well-replayed ones)
    /// 4. GC (forget edges below threshold)
    pub fn swr_consolidate(&mut self, num_replays: usize) -> ConsolidationStats {
        let mut stats = ConsolidationStats::default();
        if self.num_nodes == 0 {
            return stats;
        }

        for _ in 0..num_replays {
            // 1. Pick a random seed node
            let seed = (rand_seed() as usize) % self.num_nodes;
            let chain = self.walk_chain(seed, self.max_hops);
            if chain.len() < 2 {
                continue;
            }
            stats.chains_replayed += 1;

            // 2. LTP: strengthen edges along the chain
            for window in chain.windows(2) {
                let from = window[0] as usize;
                let to = window[1] as usize;
                if let Some(edge_idx) = self.find_edge(from, to) {
                    let raw = self.raw_weights[edge_idx];
                    self.raw_weights[edge_idx] = raw * self.ltp_rate;
                    self.values[edge_idx] =
                        self.raw_weights[edge_idx] * self.edge_relations[edge_idx].spread_coeff();
                    stats.ltp_events += 1;
                }
            }

            // 3. Increment replay counts
            for &node_idx in &chain {
                self.node_replay_count[node_idx as usize] =
                    self.node_replay_count[node_idx as usize].saturating_add(1);
            }
        }

        // 4. LTD: global decay (protect well-replayed nodes)
        for edge_idx in 0..self.values.len() {
            if !self.edge_valid[edge_idx] {
                continue;
            }
            // Find the source node for this edge to check replay protection
            let source = self.edge_source(edge_idx);
            let protection = if self.node_replay_count[source] > 3 {
                0.5
            } else {
                1.0
            };
            let raw = self.raw_weights[edge_idx];
            let new_raw = raw * (1.0 - (1.0 - self.ltd_rate) * protection);
            self.raw_weights[edge_idx] = new_raw;
            self.values[edge_idx] = new_raw * self.edge_relations[edge_idx].spread_coeff();
        }

        // 5. GC: forget weak, un-replayed edges
        for edge_idx in 0..self.values.len() {
            if !self.edge_valid[edge_idx] {
                continue;
            }
            let source = self.edge_source(edge_idx);
            if self.raw_weights[edge_idx].abs() < self.gc_threshold
                && self.node_replay_count[source] == 0
            {
                self.edge_valid[edge_idx] = false;
                stats.forgotten += 1;
            }
        }

        stats
    }

    /// Walk a causal chain from a seed node (forward, along caused edges).
    fn walk_chain(&self, seed: usize, max_len: usize) -> Vec<u32> {
        let mut chain = vec![seed as u32];
        let mut current = seed;

        for _ in 0..max_len {
            let start = self.row_ptr[current] as usize;
            let end = self.row_ptr[current + 1] as usize;

            // Find the first valid caused edge
            let next = (start..end)
                .find(|&i| self.edge_valid[i] && self.edge_relations[i] == Relation::Caused);

            match next {
                Some(edge_idx) => {
                    let target = self.col_idx[edge_idx];
                    if chain.contains(&target) {
                        break; // Avoid cycles
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

    /// Find the source node for a given edge index.
    fn edge_source(&self, edge_idx: usize) -> usize {
        // Binary search in row_ptr
        self.row_ptr
            .iter()
            .position(|&r| r as usize > edge_idx)
            .map(|p| p - 1)
            .unwrap_or(0)
    }

    /// Get number of nodes.
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }

    /// Get number of edges.
    pub fn num_edges(&self) -> usize {
        self.col_idx.len()
    }

    /// Get number of valid edges.
    pub fn num_valid_edges(&self) -> usize {
        self.edge_valid.iter().filter(|&&v| v).count()
    }

    /// Get a node's text by index.
    pub fn node_text(&self, idx: usize) -> &str {
        &self.node_text[idx]
    }

    /// Get a node's activation value by index.
    pub fn node_activation(&self, idx: usize) -> f32 {
        self.node_activation[idx]
    }

    /// Get a node's q_value by index.
    pub fn node_q_value(&self, idx: usize) -> f32 {
        self.node_q_value[idx]
    }

    /// Get a node's replay count by index.
    pub fn node_replay_count(&self, idx: usize) -> u16 {
        self.node_replay_count[idx]
    }
}

// ─── Helper functions ──────────────────────────────────────────────

/// SimHash for pattern separation (DG analog).
/// Produces a 128-bit sparse code from text.
fn simhash(text: &str) -> u128 {
    let mut bits = [0_i32; 128];
    for token in text.to_lowercase().split_whitespace() {
        let hash = fnv1a_64(token);
        for i in 0..64 {
            if (hash >> i) & 1 == 1 {
                bits[i] += 1;
            } else {
                bits[i] -= 1;
            }
        }
        // Use a second hash for bits 64-127
        let hash2 = fnv1a_64(&format!("{}#2", token));
        for i in 0..64 {
            if (hash2 >> i) & 1 == 1 {
                bits[i + 64] += 1;
            } else {
                bits[i + 64] -= 1;
            }
        }
    }
    let mut result: u128 = 0;
    for i in 0..128 {
        if bits[i] > 0 {
            result |= 1u128 << i;
        }
    }
    result
}

/// FNV-1a 64-bit hash.
fn fnv1a_64(s: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Simple Jaccard text similarity (token overlap).
fn text_jaccard_similarity(a: &str, b: &str) -> f32 {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    let set_a: std::collections::HashSet<&str> = a_lower.split_whitespace().collect();
    let set_b: std::collections::HashSet<&str> = b_lower.split_whitespace().collect();
    if set_a.is_empty() && set_b.is_empty() {
        return 1.0;
    }
    let intersection = set_a.intersection(&set_b).count() as f32;
    let union = set_a.union(&set_b).count() as f32;
    intersection / union
}

/// Simple pseudo-random seed (deterministic for testing).
fn rand_seed() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(0x1234567890ABCDEF);
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

impl Default for CausalGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ─── SQLite loading ────────────────────────────────────────────────

impl CausalGraph {
    /// Load graph from a CausalStore's SQLite database.
    pub fn from_store(store: &crate::store::CausalStore) -> anyhow::Result<Self> {
        store.with_conn(|conn| {
            // Load chunks (nodes)
            let mut node_stmt =
                conn.prepare("SELECT id, text, created_at FROM chunks ORDER BY created_at ASC")?;
            let node_rows = node_stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;

            let mut nodes: Vec<NodeData> = Vec::new();
            for row in node_rows {
                let (id, text, event_time) = row?;
                nodes.push(NodeData {
                    id,
                    text,
                    event_time,
                    q_value: 0.5,
                    replay_count: 0,
                    last_activated: 0,
                    task_tag: None,
                });
            }

            // Load causal_edges
            let mut edge_stmt = conn.prepare(
                "SELECT from_id, to_id, relation, confidence, valid_to, task_tag
                 FROM causal_edges ORDER BY event_time ASC",
            )?;
            let mut edges: Vec<EdgeData> = Vec::new();
            let edge_rows = edge_stmt.query_map([], |row| {
                let from_id: String = row.get(0)?;
                let to_id: String = row.get(1)?;
                let relation_str: String = row.get(2)?;
                let confidence: f64 = row.get(3)?;
                let valid_to: Option<i64> = row.get(4)?;
                let task_tag: Option<String> = row.get(5)?;

                Ok((from_id, to_id, relation_str, confidence, valid_to, task_tag))
            })?;

            for row in edge_rows {
                let (from_id, to_id, relation_str, confidence, valid_to, task_tag) = row?;
                let relation = Relation::from_str_lossy(&relation_str);
                let valid = valid_to.is_none();

                // Propagate task_tag to nodes
                if let Some(ref tag) = task_tag {
                    for n in &mut nodes {
                        if (n.id == from_id || n.id == to_id) && n.task_tag.is_none() {
                            n.task_tag = Some(tag.clone());
                        }
                    }
                }

                // Propagate q_value to decision nodes
                for n in &mut nodes {
                    if n.id == from_id && n.q_value == 0.5 {
                        n.q_value = confidence as f32;
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

            Ok(Self::build(&nodes, &edges))
        })
    }
}

// ─── Unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_graph() -> CausalGraph {
        // Build the Redis cache stampede chain from papers/02:
        //   "used Redis" --caused--> "cache stampede"
        //   "used mutex" --caused--> "deadlock"
        //   "used channel" --caused--> "fixed race condition"
        //   "used mutex" --prevented--> "fixed race condition"
        let nodes = vec![
            NodeData {
                id: "d1".into(),
                text: "used Redis for caching".into(),
                event_time: 1000,
                q_value: 0.8,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("caching".into()),
            },
            NodeData {
                id: "o1".into(),
                text: "cache stampede DB overloaded".into(),
                event_time: 1001,
                q_value: 0.8,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("caching".into()),
            },
            NodeData {
                id: "d2".into(),
                text: "used mutex lock".into(),
                event_time: 1002,
                q_value: 0.7,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
            NodeData {
                id: "o2".into(),
                text: "deadlock crash".into(),
                event_time: 1003,
                q_value: 0.7,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
            NodeData {
                id: "d3".into(),
                text: "used channel single-flight".into(),
                event_time: 1004,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
            NodeData {
                id: "o3".into(),
                text: "fixed race condition".into(),
                event_time: 1005,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
        ];

        let edges = vec![
            EdgeData {
                from_id: "d1".into(),
                to_id: "o1".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            },
            EdgeData {
                from_id: "d2".into(),
                to_id: "o2".into(),
                relation: Relation::Caused,
                weight: 0.85,
                valid: true,
            },
            EdgeData {
                from_id: "d3".into(),
                to_id: "o3".into(),
                relation: Relation::Caused,
                weight: 0.95,
                valid: true,
            },
            // Mutex PREVENTED the fix (negative causal edge!)
            EdgeData {
                from_id: "d2".into(),
                to_id: "o3".into(),
                relation: Relation::Prevented,
                weight: 0.6,
                valid: true,
            },
        ];

        CausalGraph::build(&nodes, &edges)
    }

    #[test]
    fn test_graph_built_correctly() {
        let graph = make_test_graph();

        assert_eq!(graph.num_nodes(), 6);
        assert_eq!(graph.num_edges(), 4);
        assert_eq!(graph.num_valid_edges(), 4);
    }

    #[test]
    fn test_csr_structure() {
        let graph = make_test_graph();

        // Node 0 (d1) has 1 outgoing edge
        assert_eq!(graph.row_ptr[1] - graph.row_ptr[0], 1);
        // Node 2 (d2) has 2 outgoing edges (caused deadlock + prevented fix)
        assert_eq!(graph.row_ptr[3] - graph.row_ptr[2], 2);
    }

    #[test]
    fn test_spreading_activation_forward() {
        let mut graph = make_test_graph();

        // Search from "Redis" should find both the decision and its outcome
        let results = graph.spreading_activation("Redis", None, false);

        assert!(!results.is_empty());
        // The seed node itself should be top (activation = 1.0)
        assert!(results[0].text.contains("Redis"));
        assert!(results[0].activation >= 0.99);

        // The outcome "cache stampede" should be activated via spreading
        let stampede = results.iter().find(|r| r.text.contains("cache stampede"));
        assert!(
            stampede.is_some(),
            "cache stampede should be in results. Got: {:?}",
            results.iter().map(|r| &r.text[..]).collect::<Vec<_>>()
        );
        assert!(
            stampede.unwrap().activation > 0.0,
            "cache stampede activation should be positive"
        );
    }

    #[test]
    fn test_spreading_activation_reverse() {
        let mut graph = make_test_graph();

        // Reverse search from "deadlock" should find "mutex"
        let results = graph.spreading_activation("deadlock", None, true);

        assert!(!results.is_empty());
        // The decision that caused the deadlock should be activated
        let has_mutex = results.iter().any(|r| r.text.contains("mutex"));
        assert!(
            has_mutex,
            "Expected to find mutex decision in reverse search"
        );
    }

    #[test]
    fn test_prevented_negative_spread() {
        let mut graph = make_test_graph();

        // Spread from "mutex" (d2) — it has:
        //   caused → deadlock (positive)
        //   prevented → fixed race condition (NEGATIVE)
        let results = graph.spreading_activation("mutex", None, false);

        // Find "deadlock" and "fixed race condition" in results
        let deadlock = results.iter().find(|r| r.text.contains("deadlock"));
        let fixed = results.iter().find(|r| r.text.contains("fixed race"));

        assert!(deadlock.is_some(), "deadlock should be activated");
        assert!(
            deadlock.unwrap().activation > 0.0,
            "deadlock activation should be positive (caused)"
        );

        assert!(fixed.is_some(), "fixed race condition should be activated");
        assert!(
            fixed.unwrap().activation < 0.0,
            "fixed race condition activation should be NEGATIVE (prevented) — \
             this is the unique innovation: inhibitory causal spread"
        );
    }

    #[test]
    fn test_task_tag_filter() {
        let mut graph = make_test_graph();

        // Search only in "concurrency" tasks
        let results = graph.spreading_activation("used", Some("concurrency"), false);

        // Should only return concurrency-tagged nodes
        for r in &results {
            assert_eq!(
                r.task_tag.as_deref(),
                Some("concurrency"),
                "All results should be tagged 'concurrency'"
            );
        }
    }

    #[test]
    fn test_empty_query_returns_empty() {
        let mut graph = make_test_graph();
        let results = graph.spreading_activation("nonexistent_xyzzy", None, false);
        assert!(results.is_empty());
    }

    #[test]
    fn test_novelty_detection_high_surprise() {
        let mut graph = make_test_graph();

        // "used Redis" normally causes "cache stampede"
        // If actual outcome is "everything works great" → high surprise
        let report = graph.detect_novelty("used Redis", "everything works great perfectly");

        assert!(
            report.surprise > 0.3,
            "Should be surprising: {}",
            report.surprise
        );
    }

    #[test]
    fn test_novelty_detection_low_surprise() {
        let mut graph = make_test_graph();

        // "used Redis" normally causes "cache stampede"
        // If actual outcome mentions "cache stampede" → low surprise
        let report = graph.detect_novelty("used Redis", "cache stampede DB overloaded");

        assert!(
            report.surprise < 0.8,
            "Should not be very surprising: {}",
            report.surprise
        );
    }

    #[test]
    fn test_swr_consolidation_ltp() {
        let mut graph = make_test_graph();

        let stats = graph.swr_consolidate(20);

        assert!(
            stats.chains_replayed > 0,
            "Should have replayed some chains"
        );
        assert!(stats.ltp_events > 0, "Should have LTP events");

        // After consolidation, some nodes should have replay_count > 0
        let replayed_count = (0..graph.num_nodes)
            .filter(|&i| graph.node_replay_count(i) > 0)
            .count();
        assert!(
            replayed_count > 0,
            "Some nodes should have replay_count > 0"
        );
    }

    #[test]
    fn test_swr_gc_forgets_weak_edges() {
        let mut graph = make_test_graph();

        // Run many consolidation cycles to trigger LTD
        let stats = graph.swr_consolidate(100);

        // Some edges might be forgotten (if they weren't on replayed chains)
        // At minimum, consolidation should complete without panic
        assert!(stats.chains_replayed > 0);
    }

    #[test]
    fn test_simhash_consistency() {
        let hash1 = simhash("used Redis for caching");
        let hash2 = simhash("used Redis for caching");
        let hash3 = simhash("completely different text about dogs");

        assert_eq!(hash1, hash2, "Same text should produce same hash");
        assert_ne!(hash1, hash3, "Different text should produce different hash");
    }

    #[test]
    fn test_simhash_similarity() {
        let h1 = simhash("used mutex lock for concurrency");
        let h2 = simhash("used mutex lock for threading");
        let h3 = simhash("bought fresh vegetables today");

        let dist_12 = (h1 ^ h2).count_ones();
        let dist_13 = (h1 ^ h3).count_ones();

        assert!(
            dist_12 < dist_13,
            "Similar texts should have smaller Hamming distance: {} vs {}",
            dist_12,
            dist_13
        );
    }

    #[test]
    fn test_jaccard_similarity() {
        let sim = text_jaccard_similarity("hello world foo", "hello world bar");
        assert!(sim > 0.0 && sim < 1.0);

        let same = text_jaccard_similarity("hello world", "hello world");
        assert!((same - 1.0).abs() < 0.001);

        let none = text_jaccard_similarity("aaa", "zzz");
        assert!((none - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_relation_spread_coefficients() {
        assert_eq!(Relation::Caused.spread_coeff(), 1.0);
        assert_eq!(Relation::Enabled.spread_coeff(), 0.5);
        assert_eq!(Relation::Prevented.spread_coeff(), -0.3);
        assert_eq!(Relation::NoEffect.spread_coeff(), 0.0);
    }

    #[test]
    fn test_empty_graph() {
        let graph = CausalGraph::new();

        assert_eq!(graph.num_nodes(), 0);
        assert_eq!(graph.num_edges(), 0);

        let mut g = graph;
        let results = g.spreading_activation("anything", None, false);
        assert!(results.is_empty());
    }

    #[test]
    fn test_multi_hop_spread() {
        // Build a 3-hop chain: A → B → C → D
        let nodes = vec![
            NodeData {
                id: "a".into(),
                text: "start decision".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "b".into(),
                text: "middle outcome".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "c".into(),
                text: "middle decision".into(),
                event_time: 2,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "d".into(),
                text: "final outcome".into(),
                event_time: 3,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
        ];
        let edges = vec![
            EdgeData {
                from_id: "a".into(),
                to_id: "b".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "b".into(),
                to_id: "c".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "c".into(),
                to_id: "d".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
        ];

        let mut graph = CausalGraph::build(&nodes, &edges);

        // Spread from "start" should reach "final outcome" (3 hops)
        let results = graph.spreading_activation("start", None, false);

        let has_final = results.iter().any(|r| r.text.contains("final outcome"));
        assert!(
            has_final,
            "Multi-hop spread should reach the final node. Results: {:?}",
            results.iter().map(|r| &r.text).collect::<Vec<_>>()
        );

        // Activation should decay over hops
        let start_act = results
            .iter()
            .find(|r| r.text.contains("start"))
            .map(|r| r.activation)
            .unwrap_or(0.0);
        let final_act = results
            .iter()
            .find(|r| r.text.contains("final outcome"))
            .map(|r| r.activation)
            .unwrap_or(0.0);
        assert!(
            start_act > final_act,
            "Start activation ({}) should be > final activation ({})",
            start_act,
            final_act
        );
    }

    #[test]
    fn test_from_store() {
        use crate::store::CausalStore;

        let store = CausalStore::open_in_memory().unwrap();

        // Add some decisions
        store
            .record_decision(
                "used Redis for caching",
                "cache stampede",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "used mutex lock",
                "deadlock crash",
                "caused",
                Some("concurrency"),
                0.85,
                "rule",
            )
            .unwrap();

        let graph = CausalGraph::from_store(&store).unwrap();

        assert!(
            graph.num_nodes() >= 4,
            "Should have at least 4 nodes (2 decisions + 2 outcomes)"
        );
        assert!(graph.num_edges() >= 2, "Should have at least 2 edges");
    }
}
