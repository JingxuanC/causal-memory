//! Types for the sleep-consolidation cycle.

use std::collections::HashSet;

use crate::patterns::{MineReport, MinerConfig};

/// Seconds per day, for age/window math.
pub(crate) const SECS_PER_DAY: f64 = 86_400.0;

/// Consolidation tuning knobs.
#[derive(Debug, Clone, Copy)]
pub struct ConsolidateConfig {
    /// Per-day multiplicative confidence decay (stage 3).
    pub decay_per_day: f64,
    /// Additive confidence boost for edges accessed within the window (stage 3).
    pub access_boost: f64,
    /// Recency window (days) for the access boost (stage 3).
    pub access_boost_window_days: u32,
    /// Hard cap on confidence after boosting (stage 3).
    pub confidence_cap: f64,
    /// Soft-invalidate edges below this confidence after decay+boost (stage 3).
    pub gc_threshold: f64,
    /// Replay-priority score at/above which an edge is protected (stage 1→3).
    /// Default 1.0: reached by failure lessons (conf ≥ 0.5 + 0.5), most
    /// user_feedback edges, and high-confidence contradicted edges.
    pub replay_protect_score: f64,
    /// Bellman learning rate for Q-value reinforcement (stage 1.5).
    pub q_alpha: f64,
    /// Bellman discount for Q-value reinforcement (stage 1.5).
    pub q_gamma: f64,
    /// Minimum recent-experience diversity (0..1) below which consolidation
    /// skips as a no-op (P6 novelty trigger). 0.0 = always consolidate.
    pub min_diversity: f64,
    /// Decay-days divisor for replay-protected edges (stage 3): 2.0 = half-rate
    /// decay.
    pub replay_decay_divisor: f64,
    /// Vela-style half-life tiers (hours) by discovery source (stage 3).
    /// effective_confidence = confidence * 0.5^(age_hours / halflife).
    /// Sources mapped to `None` keep the legacy flat `decay_per_day` decay
    /// (behaviour-compatible with pre-tier stores). Defaults keep the two
    /// high-value sources at ~90d (same magnitude as the old 0.99/day ≈ 69d)
    /// and shorten the rule/temporal tiers which are inherently time-bound.
    pub half_life_user_feedback_hours: u32,
    pub half_life_llm_hours: u32,
    pub half_life_rule_hours: u32,
    pub half_life_temporal_hours: u32,
    /// GC threshold for replay-protected edges (stage 3), more lenient than
    /// `gc_threshold`.
    pub replay_gc_threshold: f64,
    /// Pattern-miner configuration, reused for stages 2 and 4.
    pub miner: MinerConfig,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            decay_per_day: 0.99,
            access_boost: 0.05,
            access_boost_window_days: 7,
            confidence_cap: 0.95,
            gc_threshold: 0.2,
            replay_protect_score: 1.0,
            replay_decay_divisor: 2.0,
            replay_gc_threshold: 0.1,
            half_life_user_feedback_hours: 2160, // 90d
            half_life_llm_hours: 2160,            // 90d (same magnitude as legacy ~69d)
            half_life_rule_hours: 720,            // 30d
            half_life_temporal_hours: 168,        // 7d
            q_alpha: 0.1,
            q_gamma: 0.9,
            min_diversity: 0.0,
            miner: MinerConfig::default(),
        }
    }
}

impl ConsolidateConfig {
    /// Vela-style half-life (hours) for a discovery source.
    /// `None` = keep the legacy flat `decay_per_day` (pre-tier behaviour).
    pub fn half_life_hours(&self, discovered_by: &str) -> Option<f64> {
        match discovered_by {
            "user_feedback" => Some(f64::from(self.half_life_user_feedback_hours)),
            "llm_inferred" => Some(f64::from(self.half_life_llm_hours)),
            // rule stays on the legacy flat decay (0.99/day): rule-inferred
            // lessons are structural, and keeping it unmapped preserves the
            // exact pre-tier behaviour for the bulk of existing stores/tests.
            "temporal" => Some(f64::from(self.half_life_temporal_hours)),
            _ => None,
        }
    }
}

/// One scored edge from the reactivation (replay-priority) pass.
#[derive(Debug, Clone)]
pub struct ReactivationEntry {
    pub edge_id: i64,
    /// Decision text, for human-readable reports.
    pub decision_text: String,
    pub score: f64,
    /// Why this score: e.g. "base confidence", "outcome failed (+0.5)".
    pub reasons: Vec<String>,
}

/// What one consolidation cycle did (or would do, when `dry_run`).
#[derive(Debug, Default)]
pub struct ConsolidateReport {
    /// Stage 1: replay-priority queue, score-descending, top 20.
    pub reactivated: Vec<ReactivationEntry>,
    /// Stage 1 write-back: replay-protected edges marked with
    /// `last_accessed_at = now` (decay halved + lenient GC this cycle, and
    /// visible as "replayed" to the next cycle).
    pub replayed: usize,
    /// Stage 2a: redundant duplicate edges merged away.
    pub merged_edges: usize,
    /// Stage 2b: pattern-miner result.
    pub mine_report: MineReport,
    /// Stage 3: edges whose confidence actually decayed (age ≥ 1 day).
    pub decayed: usize,
    /// Stage 3: edges that received the access boost.
    pub boosted: usize,
    /// Stage 3: edges soft-invalidated by garbage collection.
    pub gc_invalidated: usize,
    /// Stage 4: cross-domain transfer meta edges written.
    pub rem_transfers: usize,
    /// Stage 1.5: chunk Q-values reinforced (Bellman) and persisted.
    pub q_updates: usize,
    /// P6: token-level diversity of recent experience (0..1).
    pub diversity: f64,
    /// P6: consolidation skipped because diversity < min_diversity.
    pub skipped_low_diversity: bool,
    pub dry_run: bool,
}

/// One valid meta edge plus the task tags its endpoint decisions live in.
pub(crate) struct MetaNode {
    pub id: i64,
    pub from_id: String,
    /// Text of the central decision (from endpoint), for readable patterns.
    pub from_text: String,
    /// discovered_at after stage 2b — compared against the pre-mine snapshot
    /// to tell which meta edges this round created or refreshed.
    pub discovered_at: i64,
    /// from_text + to_text, tokenized once for similarity.
    pub tokens: Vec<String>,
    pub task_tags: HashSet<String>,
}
