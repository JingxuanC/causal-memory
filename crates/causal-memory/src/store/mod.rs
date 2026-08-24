//! Causal store — the core data layer.
//!
//! Ported from spike/grok-causal-memory/src/lib.rs, adapted for async MCP use.
//! SQLite operations are sync; we use tokio::task::spawn_blocking to avoid
//! blocking the async runtime.

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};

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
    created_at INTEGER NOT NULL,
    q_value REAL NOT NULL DEFAULT 0.5,
    sparse_code TEXT
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
    superseded_by INTEGER,
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

-- Persistent BM25 inverted index (schema v10, architecture hardening B2):
-- token -> chunk_id postings so keyword search narrows candidates instead
-- of scanning every edge. chunk_id namespace: fact:{id} = agent_facts row,
-- otherwise a chunks row. Maintained by CausalStore::index_chunk on every
-- chunk/fact write; queried by search_causal_bm25 / search_facts_bm25.
CREATE TABLE IF NOT EXISTS bm25_index (
    token TEXT NOT NULL,
    chunk_id TEXT NOT NULL,
    PRIMARY KEY (token, chunk_id)
);
CREATE INDEX IF NOT EXISTS idx_bm25_chunk ON bm25_index(chunk_id);

-- Hebbian co-occurrence edges (schema v11, architecture hardening D1):
-- weak associative links between chunks co-activated in the same retrieval.
-- Weight follows the HeLa-Mem formula w=(1-lambda)w + eta per co-activation,
-- persisted so the hippocampus graph rebuilds them on startup.
CREATE TABLE IF NOT EXISTS cooccurrence_edges (
    from_id TEXT NOT NULL REFERENCES chunks(id),
    to_id TEXT NOT NULL REFERENCES chunks(id),
    weight REAL NOT NULL DEFAULT 0.2,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (from_id, to_id)
);
"#;

/// Thread-safe causal store backed by SQLite.
///
/// Architecture hardening A2: the single global `Mutex<Connection>` was a
/// serialization bottleneck — every read and write queued on one lock. This
/// is replaced by a small hand-rolled connection pool (a real pool like
/// r2d2 was the first choice, but crates.io is unreachable in the target
/// environment; this implementation is ~80 lines and zero-dependency).
///
/// WAL mode (apply_pragmas) is what makes pooling safe: concurrent readers
/// no longer block the writer, and the 5s busy timeout turns write
/// contention into waiting instead of SQLITE_BUSY. A connection is checked
/// out for the duration of one store method and returned on drop.
#[derive(Clone)]
pub struct CausalStore {
    pub(crate) conn: Arc<ConnPool>,
    /// A4: pending access_count bumps from read paths, flushed on the next
    /// connection checkout (see acquire). Keeps reads read-only.
    pub(crate) access_buffer: Arc<Mutex<HashSet<i64>>>,
    /// Perf cache (audit 2026-08 #2): edge_id → precomputed entity tokens.
    /// Chunk texts are immutable and edges are append/invalidate-only, so a
    /// cached entry can never go stale within a process — an invalidated
    /// edge simply drops out of the candidate set before the cache is read.
    pub(crate) entity_cache:
        Arc<Mutex<std::collections::HashMap<i64, std::sync::Arc<Vec<String>>>>>,
}

impl CausalStore {
    /// Open an in-memory store (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let pool = ConnPool::new(None)?;
        let conn = pool.take_conn()?;
        crate::migrate::migrate(&conn)?;
        pool.release(conn);
        Ok(Self {
            conn: Arc::new(pool),
            access_buffer: Arc::new(Mutex::new(HashSet::new())),
            entity_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Open a file-backed store.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let pool = ConnPool::new(Some(path.as_ref().to_path_buf()))?;
        let conn = pool.take_conn()?;
        crate::migrate::migrate(&conn)?;
        Self::seed_id_counter(&conn);
        pool.release(conn);
        Ok(Self {
            conn: Arc::new(pool),
            access_buffer: Arc::new(Mutex::new(HashSet::new())),
            entity_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Check out a pooled connection for the duration of one method.
    /// This is what every store method uses instead of locking a global
    /// connection. The guard returns the connection to the pool on drop
    /// (rolling back any dangling transaction first).
    pub(crate) fn acquire(&self) -> Result<PooledConn> {
        let conn = self.conn.take_conn()?;
        // A4: pending access bumps land on this checkout, before any query
        // runs — readers stay read-only, counters lag by at most one method.
        self.flush_access_buffer(&conn);
        Ok(PooledConn {
            conn: Some(conn),
            pool: Arc::clone(&self.conn),
        })
    }

    /// Connection pragmas for the file-backed store (architecture hardening A1):
    /// - journal_mode=WAL: concurrent readers + a single writer no longer
    ///   block each other (the HTTP multi-connection mode depends on this);
    /// - busy_timeout=5s: a contended writer waits instead of failing with
    ///   SQLITE_BUSY the moment another connection holds the write lock;
    /// - synchronous=NORMAL: WAL-safe durability (a crash can lose at most
    ///   the last checkpoint, never corrupt), removing a synchronous fsync
    ///   from every committed write.
    ///
    /// In-memory stores (open_in_memory) skip this: WAL is meaningless for
    /// :memory: and the defaults are fine for tests.
    fn apply_pragmas(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
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

    /// Maintain the persistent BM25 inverted index (B2): tokenize `text`
    /// and record (token, chunk_id) postings. INSERT OR IGNORE makes this
    /// idempotent for reused chunks. Called from every chunk creation site
    /// (distill writes and record_decision); facts use the `fact:{id}`
    /// namespace via the same helper.
    /// Public so bench harnesses that insert chunks directly (locomo's raw
    /// turn ingest) can keep the persistent BM25 index complete.
    pub fn index_chunk(conn: &Connection, chunk_id: &str, text: &str) -> Result<()> {
        let tokens = crate::patterns::tokenize(text);
        if tokens.is_empty() {
            return Ok(());
        }
        let mut stmt =
            conn.prepare("INSERT OR IGNORE INTO bm25_index (token, chunk_id) VALUES (?1, ?2)")?;
        for tok in tokens {
            stmt.execute(params![tok, chunk_id])?;
        }
        Ok(())
    }

    /// Hebbian co-occurrence learning (D1): record that `pairs` of chunk
    /// ids were co-activated in one retrieval. New pairs start at 0.2;
    /// every further co-activation strengthens the pair by +0.05 up to 1.0.
    /// (The raw HeLa-Mem steady-state formula w=(1-lambda)w+eta with
    /// lambda=0.995/eta=0.02 has steady state ~0.02 — below the 0.2 initial
    /// weight, so bumps would decay the very link they reinforce. Additive
    /// growth keeps the intended semantics: more co-activation = stronger,
    /// capped association. A time-driven decay can live in consolidation.)
    pub fn bump_cooccurrences(&self, pairs: &[(String, String)]) -> Result<usize> {
        if pairs.is_empty() {
            return Ok(0);
        }
        let conn = self.acquire()?;
        let now = chrono::Utc::now().timestamp();
        let mut stmt = conn.prepare(
            "INSERT INTO cooccurrence_edges (from_id, to_id, weight, updated_at)
             VALUES (?1, ?2, 0.2, ?3)
             ON CONFLICT(from_id, to_id) DO UPDATE SET
                 weight = MIN(1.0, weight + 0.05),
                 updated_at = excluded.updated_at",
        )?;
        let mut n = 0usize;
        for (a, b) in pairs {
            if a == b {
                continue;
            }
            stmt.execute(rusqlite::params![a, b, now])?;
            n += 1;
        }
        Ok(n)
    }

    /// Load co-occurrence edges (chunk id pairs + weights) for the
    /// hippocampus graph rebuild (D1).
    pub fn load_cooccurrences(&self) -> Result<Vec<(String, String, f64)>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT from_id, to_id, weight FROM cooccurrence_edges ORDER BY weight DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Execute a closure with a reference to the connection (for advanced queries).
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T>,
    {
        let conn = self.acquire()?;
        f(&conn)
    }
}

// ─── Hand-rolled connection pool (A2) ────────────────────────────────

/// A tiny zero-dependency connection pool. Keeps up to `max_idle`
/// connections open; beyond that, connections are closed on return.
/// All pooled connections are file-backed (or in-memory) with the same
/// pragmas applied, so WAL concurrency semantics hold across the pool.
pub(crate) struct ConnPool {
    idle: Mutex<Vec<Connection>>,
    path: Option<PathBuf>,
    max_idle: usize,
}

impl ConnPool {
    /// Create a pool. `Some(path)` = file-backed (pragmas applied);
    /// `None` = in-memory (test only). The first connection is opened
    /// here so WAL/pragma setup happens exactly once.
    fn new(path: Option<PathBuf>) -> Result<Self> {
        let is_file = path.is_some();
        let pool = ConnPool {
            idle: Mutex::new(Vec::new()),
            path,
            max_idle: if is_file { 8 } else { 4 },
        };
        let first = pool.open_conn()?;
        pool.idle
            .lock()
            .map_err(|e| anyhow!("pool lock: {e}"))?
            .push(first);
        Ok(pool)
    }

    fn open_conn(&self) -> Result<Connection> {
        let conn = match &self.path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        if self.path.is_some() {
            CausalStore::apply_pragmas(&conn)?;
        }
        Ok(conn)
    }

    /// Take a connection from the pool (or open a new one).
    fn take_conn(&self) -> Result<Connection> {
        match self
            .idle
            .lock()
            .map_err(|e| anyhow!("pool lock: {e}"))?
            .pop()
        {
            Some(c) => Ok(c),
            None => self.open_conn(),
        }
    }

    /// Return a connection to the pool (or close it when over capacity).
    fn release(&self, conn: Connection) {
        // Never return a connection mid-transaction: roll back so the next
        // user starts from a clean autocommit state.
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        let mut idle = match self.idle.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if idle.len() < self.max_idle {
            idle.push(conn);
        }
    }
}

/// RAII guard: a connection checked out of the pool, returned on drop.
pub(crate) struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<ConnPool>,
}

impl Deref for PooledConn {
    type Target = Connection;
    // conn is Some for the whole borrowable lifetime; it is only taken out
    // inside Drop when the connection returns to the pool.
    #[allow(clippy::expect_used, reason = "conn is invariantly Some outside Drop")]
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("pooled conn present")
    }
}

impl DerefMut for PooledConn {
    #[allow(clippy::expect_used, reason = "conn is invariantly Some outside Drop")]
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("pooled conn present")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.pool.release(c);
        }
    }
}

// Submodules — each adds methods to `impl CausalStore`.
mod embed;
mod facts;
pub mod retrieve;
mod types;
mod utils;
mod write;

// Re-export all public types so `causal_memory::store::CausalEntry` still works.
pub use types::*;
pub use utils::{
    containment_similarity, date_tokens, effective_polarity, is_retraction_record,
    outcome_polarity, outcomes_contradict, strip_bracket_prefix, RETRACTION_MARKERS,
    SUPERSEDES_MIN_SHARED_TOKENS, SUPERSEDES_SIM_THRESHOLD,
};

#[cfg(test)]
mod probe_perf;
#[cfg(test)]
mod tests;
