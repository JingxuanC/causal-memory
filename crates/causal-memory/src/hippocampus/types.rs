//! Types for the hippocampus-style causal activation graph.

/// Forward adjacency entry: (target_node, raw_weight, spread_value, relation, valid).
pub(crate) type AdjEdge = (u32, f32, f32, Relation, bool);

/// Causal relation type — determines spread coefficient.
/// Inspired by neurotransmitter types:
///   Caused   = excitatory (glutamate) → strong positive spread
///   Enabled  = weak excitatory       → mild positive spread
///   Prevented = inhibitory (GABA)    → NEGATIVE spread (unique to causal-memory)
///   NoEffect = no connection
///   Fact     = semantic association  → mild positive spread (P1: typed-edge unification)
///   Meta     = cortical top-down     → weak positive spread (P1)
///   CoOccurrence = Hebbian LTP       → weak positive, weight is dynamic (P2)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Relation {
    Caused = 0,
    Enabled = 1,
    Prevented = 2,
    NoEffect = 3,
    Fact = 4,
    Meta = 5,
    CoOccurrence = 6,
}

impl Relation {
    /// Spread coefficient for activation diffusion.
    /// Pre-multiplied into edge values at build time.
    #[inline]
    pub fn spread_coeff(self) -> f32 {
        match self {
            Relation::Caused => 1.0,
            Relation::Fact => 0.8,
            Relation::Meta => 0.6,
            Relation::Enabled => 0.5,
            // CoOccurrence coefficient is multiplied by the dynamic Hebbian
            // weight at build time, so the base is 1.0 here; the weight
            // itself starts at ~0.2 (HeLa-Mem η=0.02, λ=0.995).
            Relation::CoOccurrence => 1.0,
            Relation::Prevented => -0.3,
            Relation::NoEffect => 0.0,
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "caused" => Relation::Caused,
            "enabled" => Relation::Enabled,
            "prevented" => Relation::Prevented,
            "fact" => Relation::Fact,
            "meta" => Relation::Meta,
            "co_occurrence" | "co-occurrence" => Relation::CoOccurrence,
            _ => Relation::NoEffect,
        }
    }
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

/// A single auditable change produced by SWR consolidation.
/// Part of the immutable delta log (P3: Dreams-aligned consolidation).
#[derive(Debug, Clone)]
pub struct ConsolidationDelta {
    /// "ltp" | "ltd" | "gc" | "replay" | "distill" | "q_update"
    pub op: &'static str,
    pub edge_idx: u32,
    pub node_idx: u32,
    pub old_value: f32,
    pub new_value: f32,
}

/// Immutable consolidation result (P3: Dreams-aligned).
///
/// `swr_consolidate_immutable` computes a delta against the current graph,
/// applies it to a **clone**, and returns the clone + a full audit log.
/// The original graph is never mutated — the caller decides whether to swap.
pub struct ConsolidationResult {
    /// The new graph (original + all deltas applied). Clone of the input.
    pub new_graph: super::CausalGraph,
    /// Every change, in execution order (audit log).
    pub delta_log: Vec<ConsolidationDelta>,
    /// Aggregate stats (same fields as the old mutable path).
    pub stats: ConsolidationStats,
    /// The instructions string that steered this run (if any).
    pub instructions: Option<String>,
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
    /// Fact nodes only: the fact's scope ("user"/"session"/"agent" or a
    /// colon-namespaced custom scope like "lme:{qid}" / "tenant:acme").
    /// Phase A entity linking uses it to keep cross-scope links out
    /// (benchmark/multi-tenant isolation); None on chunk/scope nodes.
    pub scope: Option<String>,
}
