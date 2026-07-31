//! Public data types returned by the store.

use rusqlite;

/// Result of `CausalStore::record_distilled`.
#[derive(Debug, Clone)]
pub struct RecordDistilledOutcome {
    /// The (new or pre-existing duplicate) distilled chunk id.
    pub chunk_id: String,
    /// Edge id of the new self-edge; None when `duplicate`.
    pub edge_id: Option<i64>,
    /// True when the same distilled text was already stored (idempotent skip).
    pub duplicate: bool,
    /// Edges soft-invalidated via `supersedes` matching (all candidates at
    /// or above the similarity threshold, not just the best one).
    pub invalidated_edge_ids: Vec<i64>,
}

/// A causal retrieval result returned to the agent.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CausalEntry {
    pub edge_id: i64,
    pub decision_id: String,
    pub decision_text: String,
    pub outcome_id: String,
    pub outcome_text: String,
    pub relation: String,
    pub confidence: f64,
    pub task_tag: Option<String>,
    pub event_time: i64,
    pub valid_to: Option<i64>,
    pub access_count: i64,
    pub last_accessed_at: Option<i64>,
    pub discovered_by: String,
    pub discovered_at: i64,
    /// Stored write-time outcome polarity (v4); None for legacy rows.
    pub outcome_polarity: Option<String>,
}

/// Columns selected when materializing a `CausalEntry` (order matters, see `entry_from_row`).
pub(crate) const ENTRY_COLUMNS: &str = "ce.id, cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence,
         ce.task_tag, ce.event_time, ce.valid_to, ce.access_count, ce.last_accessed_at,
         ce.discovered_by, ce.discovered_at, ce.outcome_polarity";

/// Map a row selected with `ENTRY_COLUMNS` (plus the standard chunk joins) to a `CausalEntry`.
pub(crate) fn entry_from_row(row: &rusqlite::Row) -> rusqlite::Result<CausalEntry> {
    Ok(CausalEntry {
        edge_id: row.get(0)?,
        decision_id: row.get(1)?,
        decision_text: row.get(2)?,
        outcome_id: row.get(3)?,
        outcome_text: row.get(4)?,
        relation: row.get(5)?,
        confidence: row.get(6)?,
        task_tag: row.get(7)?,
        event_time: row.get(8)?,
        valid_to: row.get(9)?,
        access_count: row.get(10)?,
        last_accessed_at: row.get(11)?,
        discovered_by: row.get(12)?,
        discovered_at: row.get(13)?,
        outcome_polarity: row.get(14)?,
    })
}

/// A flat agent fact ("user prefers TypeScript"), v6 fact layer.
/// `valid_to` semantics mirror causal edges: None = still valid.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct AgentFact {
    pub id: i64,
    pub key: String,
    pub value: String,
    pub scope: String,
    pub source: String,
    pub confidence: f64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AgentFact {
    /// Text indexed for retrieval: key tokens plus value tokens.
    pub fn search_text(&self) -> String {
        format!("{} {}", self.key.replace('_', " "), self.value)
    }
}

/// Map a row `(id, key, value, scope, source, confidence, created_at, updated_at)`
/// to an `AgentFact` (order matters, same discipline as `entry_from_row`).
pub(crate) fn fact_from_row(row: &rusqlite::Row) -> rusqlite::Result<AgentFact> {
    Ok(AgentFact {
        id: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        scope: row.get(3)?,
        source: row.get(4)?,
        confidence: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// A single hop in a multi-hop causal chain.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ChainHop {
    pub hop: usize,
    pub edge_id: i64,
    pub decision_id: String,
    pub decision_text: String,
    pub outcome_id: String,
    pub outcome_text: String,
    pub relation: String,
    pub confidence: f64,
    pub chain_confidence: f64,
    /// Stored write-time outcome polarity (v4); None for legacy rows.
    pub outcome_polarity: Option<String>,
}

/// A compact decision directory entry (for L0 system-prompt injection).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DecisionDirectoryEntry {
    pub id: String,
    pub task_tag: Option<String>,
    pub decision_snippet: String,
    pub outcome_snippet: String,
    pub relation: String,
}

/// A meta-causal edge: decision → decision cross-task pattern.
/// This is the "neocortex" layer (slow semantic abstraction) over the
/// "hippocampus" layer of episodic `causal_edges`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct MetaEdge {
    pub id: i64,
    pub from_id: String,
    pub to_id: String,
    pub relation: String,
    pub pattern: Option<String>,
    pub confidence: f64,
    pub discovered_at: i64,
    pub valid_to: Option<i64>,
    /// Echo of the decision text at each endpoint (joined from chunks).
    pub from_text: String,
    pub to_text: String,
    /// Stratified-replication test results (v5); None = not yet tested.
    /// `strata` is a JSON array of task tags in which the pattern holds.
    pub strata_count: Option<i64>,
    pub strata: Option<String>,
    pub confounded: Option<bool>,
    pub simpson: Option<bool>,
}

/// A segment of causal chain within one session (task_tag).
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SessionSegment {
    pub task_tag: Option<String>,
    pub hops: Vec<ChainHop>,
}

/// Cross-session causal chain: multiple session segments linked by
/// meta-causal edges (pattern-miner bridges).
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CrossSessionChain {
    pub segments: Vec<SessionSegment>,
    pub overall_confidence: f64,
}
