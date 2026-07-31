//! Types for the hippocampus-style causal activation graph.

/// Forward adjacency entry: (target_node, raw_weight, spread_value, relation, valid).
pub(crate) type AdjEdge = (u32, f32, f32, Relation, bool);

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
