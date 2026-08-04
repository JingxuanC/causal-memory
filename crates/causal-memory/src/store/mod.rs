//! Causal store — the core data layer.
//!
//! Ported from spike/grok-causal-memory/src/lib.rs, adapted for async MCP use.
//! SQLite operations are sync; we use tokio::task::spawn_blocking to avoid
//! blocking the async runtime.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{Connection};

pub(crate) static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// SQL schema for causal tables (v4, see migrate::SCHEMA_VERSION).
/// All statements use IF NOT EXISTS; older DBs are upgraded column-by-column
/// by `crate::migrate::migrate`.
pub const CAUSAL_SCHEMA_SQL: &str = r#"
-- Minimal chunks table: ONLY structured memories enter this table.
--   - causal edge endpoints (d{id}, o{id} from record_decision)
--   - distilled items (distill:{ts}:{seq})
-- Raw conversation turns do NOT go here — they live in session_logs.
-- This separation keeps the retrieval pool (BM25 search target) clean:
-- only facts, lessons, and causal relationships are searchable.
CREATE TABLE IF NOT EXISTS chunks (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Session logs: raw conversation turns for audit/replay.
-- Not searched by BM25; available for explicit history queries.
-- v8: `embedding` is the per-turn semantic vector for the recurrence-
-- triggered distill check (RecMem); `distilled_at` marks when the session's
-- turn group was distilled (NULL = still awaiting its recurrence check).
CREATE TABLE IF NOT EXISTS session_logs (
    id TEXT PRIMARY KEY,
    session_id INTEGER NOT NULL,
    turn_index INTEGER NOT NULL,
    speaker TEXT NOT NULL,
    text TEXT NOT NULL,
    event_time INTEGER NOT NULL,
    task_tag TEXT,
    embedding BLOB,
    distilled_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_session_logs_session ON session_logs(session_id);
CREATE INDEX IF NOT EXISTS idx_session_logs_task ON session_logs(task_tag);
CREATE INDEX IF NOT EXISTS idx_session_logs_distilled ON session_logs(distilled_at);

-- Causal edges: decision → outcome
-- Time fields:
--   event_time    = when the decision/outcome actually happened (for temporal ordering)
--   discovered_at = when this edge was written to DB (for audit)
--   valid_to      = NULL = still valid; non-NULL = when this edge was invalidated
-- Access tracking (v3): bumped on every read-path hit (search/trace).
-- Outcome polarity (v4): write-time judgment of the outcome's direction
-- (positive/negative/mixed/neutral); NULL = legacy row, readers fall back to
-- the signal-word heuristic.
CREATE TABLE IF NOT EXISTS causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK(relation IN ('caused','enabled','prevented','no_effect')),
    confidence REAL NOT NULL DEFAULT 0.5,
    discovered_by TEXT NOT NULL DEFAULT 'llm_inferred',
    event_time INTEGER NOT NULL,
    discovered_at INTEGER NOT NULL,
    valid_to INTEGER,
    task_tag TEXT,
    access_count INTEGER NOT NULL DEFAULT 0,
    last_accessed_at INTEGER,
    outcome_polarity TEXT CHECK(outcome_polarity IN ('positive','negative','mixed','neutral')),
    -- v8: id of the edge that superseded this one. Reversible consolidation:
    -- supersession marks instead of deletes; `restore_edge` clears both this
    -- and valid_to when later evidence proves the old memory right.
    superseded_by INTEGER,
    FOREIGN KEY (from_id) REFERENCES chunks(id),
    FOREIGN KEY (to_id) REFERENCES chunks(id)
);
CREATE INDEX IF NOT EXISTS idx_causal_from ON causal_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_causal_to ON causal_edges(to_id);
CREATE INDEX IF NOT EXISTS idx_causal_task ON causal_edges(task_tag);
CREATE INDEX IF NOT EXISTS idx_causal_event_time ON causal_edges(event_time);
CREATE INDEX IF NOT EXISTS idx_causal_valid ON causal_edges(valid_to);

-- Meta causal edges: decision → decision (cross-task patterns)
-- Stratification fields (v5): the miner's replication test fills them;
-- NULL = not yet tested (legacy rows, or edges written outside the miner).
CREATE TABLE IF NOT EXISTS meta_causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK(relation IN ('similar_to','repeated','contradicts','refines')),
    pattern TEXT,
    confidence REAL NOT NULL DEFAULT 0.5,
    discovered_at INTEGER NOT NULL,
    valid_from INTEGER,
    valid_to INTEGER,
    strata_count INTEGER,
    strata TEXT,
    confounded INTEGER,
    simpson INTEGER
);
CREATE INDEX IF NOT EXISTS idx_meta_from ON meta_causal_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_meta_to ON meta_causal_edges(to_id);
CREATE INDEX IF NOT EXISTS idx_meta_valid ON meta_causal_edges(valid_to);

-- Edge embeddings for semantic retrieval (Phase 6, populated by the embed path).
CREATE TABLE IF NOT EXISTS edge_embeddings (
    edge_id INTEGER PRIMARY KEY REFERENCES causal_edges(id),
    model TEXT NOT NULL,
    vector BLOB NOT NULL,
    created_at INTEGER NOT NULL
);

-- Agent facts (v6, unified-memory-design Phase 1): flat facts such as
-- "user prefers TypeScript" or "project uses Redis 7.2". Same soft-
-- invalidation semantics as causal edges (valid_to NULL = still valid).
-- UNIQUE(key, value, scope) makes re-recording an existing fact idempotent.
-- Scope is one of the canonical assistant scopes (user/session/agent) or a
-- colon-namespaced custom scope ("tenant:acme", "lme:e47becba") — the colon
-- rule (v7) keeps typo protection for the canonical scopes while allowing
-- multi-tenant and benchmark namespaces.
CREATE TABLE IF NOT EXISTS agent_facts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'user' CHECK(scope IN ('user','session','agent') OR instr(scope, ':') > 1),
    source TEXT NOT NULL DEFAULT 'agent',
    confidence REAL NOT NULL DEFAULT 0.8,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    valid_to INTEGER,
    embedding_model TEXT,
    UNIQUE(key, value, scope)
);
CREATE INDEX IF NOT EXISTS idx_facts_key ON agent_facts(key);
CREATE INDEX IF NOT EXISTS idx_facts_scope ON agent_facts(scope);
CREATE INDEX IF NOT EXISTS idx_facts_valid ON agent_facts(valid_to);

-- Fact embeddings for semantic retrieval (populated when an embedding
-- endpoint is configured; mirrors edge_embeddings).
CREATE TABLE IF NOT EXISTS agent_facts_embeddings (
    fact_id INTEGER PRIMARY KEY REFERENCES agent_facts(id),
    model TEXT NOT NULL,
    vector BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

/// Thread-safe causal store backed by SQLite.
#[derive(Clone)]
pub struct CausalStore {
    pub(crate) conn: Arc<Mutex<Connection>>,
}

impl CausalStore {
    /// Open an in-memory store (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        crate::migrate::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open a file-backed store.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::migrate::migrate(&conn)?;
        Self::seed_id_counter(&conn);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Seed the process-global id counter from the DB.
    fn seed_id_counter(conn: &Connection) {
        let generated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks
                 WHERE id LIKE 'distill:%' OR id GLOB 'd[0-9]*' OR id GLOB 'o[0-9]*'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        ID_COUNTER.fetch_max(generated as u64 + 1, Ordering::Relaxed);
    }

    /// Execute a closure with a reference to the connection (for advanced queries).
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T>,
    {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        f(&conn)
    }
}

// Submodules — each adds methods to `impl CausalStore`.
mod embed;
mod facts;
mod retrieve;
mod types;
mod utils;
mod write;

// Re-export all public types so `causal_memory::store::CausalEntry` still works.
pub use types::*;
pub use utils::{
    containment_similarity, date_tokens, effective_polarity, is_retraction_record,
    outcome_polarity, outcomes_contradict, strip_bracket_prefix,
    RETRACTION_MARKERS, SUPERSEDES_MIN_SHARED_TOKENS, SUPERSEDES_SIM_THRESHOLD,
};

#[cfg(test)]
mod tests;
