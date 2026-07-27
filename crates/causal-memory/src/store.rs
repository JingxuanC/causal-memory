//! Causal store — the core data layer.
//!
//! Ported from spike/grok-causal-memory/src/lib.rs, adapted for async MCP use.
//! SQLite operations are sync; we use tokio::task::spawn_blocking to avoid
//! blocking the async runtime.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// SQL schema for causal tables (v4, see migrate::SCHEMA_VERSION).
/// All statements use IF NOT EXISTS; older DBs are upgraded column-by-column
/// by `crate::migrate::migrate`.
pub const CAUSAL_SCHEMA_SQL: &str = r#"
-- Minimal chunks table (host agent writes decisions/outcomes as chunks)
CREATE TABLE IF NOT EXISTS chunks (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

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
    FOREIGN KEY (from_id) REFERENCES chunks(id),
    FOREIGN KEY (to_id) REFERENCES chunks(id)
);
CREATE INDEX IF NOT EXISTS idx_causal_from ON causal_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_causal_to ON causal_edges(to_id);
CREATE INDEX IF NOT EXISTS idx_causal_task ON causal_edges(task_tag);
CREATE INDEX IF NOT EXISTS idx_causal_event_time ON causal_edges(event_time);
CREATE INDEX IF NOT EXISTS idx_causal_valid ON causal_edges(valid_to);

-- Meta causal edges: decision → decision (cross-task patterns)
CREATE TABLE IF NOT EXISTS meta_causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK(relation IN ('similar_to','repeated','contradicts','refines')),
    pattern TEXT,
    confidence REAL NOT NULL DEFAULT 0.5,
    discovered_at INTEGER NOT NULL,
    valid_from INTEGER,
    valid_to INTEGER
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
"#;

/// Thread-safe causal store backed by SQLite.
#[derive(Clone)]
pub struct CausalStore {
    conn: Arc<Mutex<Connection>>,
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
}

/// Columns selected when materializing a `CausalEntry` (order matters, see `entry_from_row`).
const ENTRY_COLUMNS: &str = "ce.id, cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence,
         ce.task_tag, ce.event_time, ce.valid_to, ce.access_count, ce.last_accessed_at,
         ce.discovered_by, ce.discovered_at";

/// Map a row selected with `ENTRY_COLUMNS` (plus the standard chunk joins) to a `CausalEntry`.
fn entry_from_row(row: &rusqlite::Row) -> rusqlite::Result<CausalEntry> {
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
    })
}

/// Failure signal words (lowercased substring match, EN + ZH).
/// Kept as substring match: English failure words have many inflections
/// ("failed", "errors", "crashed", "timeouts") that a token match would miss,
/// and substring false positives are rare for these words.
const FAILURE_SIGNALS: &[&str] = &[
    "fail", "error", "crash", "deadlock", "timeout", "panic", "失败", "报错", "死锁", "崩溃",
];

/// Chinese success signal words (lowercased substring match).
const SUCCESS_SIGNALS_ZH: &[&str] = &["成功", "通过", "修复"];

/// English success signal tokens (exact word match after splitting on
/// non-alphanumeric characters, same style as `patterns::tokenize`).
const SUCCESS_TOKENS_EN: &[&str] = &[
    "ok",
    "pass",
    "passed",
    "fixed",
    "resolved",
    "succeed",
    "succeeds",
    "succeeded",
];

fn contains_signal(text: &str, signals: &[&str]) -> bool {
    let lower = text.to_lowercase();
    signals.iter().any(|s| lower.contains(s))
}

/// English success words are matched on word boundaries so "unresolved" does
/// not hit "resolved" and "invoke"/"compass" do not hit "ok"/"pass".
/// Inflections of "success" ("successful", "successfully") are covered by a
/// prefix check that excludes the "unsuccess…" negation.
fn contains_success_signal(text: &str) -> bool {
    let lower = text.to_lowercase();
    if contains_signal(&lower, SUCCESS_SIGNALS_ZH) {
        return true;
    }
    lower.split(|c: char| !c.is_alphanumeric()).any(|tok| {
        SUCCESS_TOKENS_EN.contains(&tok)
            || (tok.starts_with("success") && !tok.starts_with("unsuccess"))
    })
}

/// Outcome polarity: `Some(false)` = clearly failure, `Some(true)` = clearly
/// success, `None` = neutral.
///
/// When both failure and success signals co-occur, success wins: the failure
/// word names the problem that was fixed ("deadlock resolved",
/// "fixed the error"), so the outcome itself is a success.
/// Exported for the Phase-3 pattern miner (same-direction / refinement checks).
pub fn outcome_polarity(text: &str) -> Option<bool> {
    let fail = contains_signal(text, FAILURE_SIGNALS);
    let success = contains_success_signal(text);
    match (fail, success) {
        (_, true) => Some(true),
        (true, false) => Some(false),
        (false, false) => None,
    }
}

/// Rule-based contradiction check between two outcomes of the same decision.
///
/// Returns true when one side is clearly a failure and the other side is not
/// (success or neutral) — i.e. the new evidence falsifies the old lesson.
/// Both-failure and both-success/neutral pairs are NOT contradictions.
pub fn outcomes_contradict(old: &str, new: &str) -> bool {
    match (outcome_polarity(old), outcome_polarity(new)) {
        (Some(false), other) => other != Some(false),
        (other, Some(false)) => other != Some(false),
        _ => false,
    }
}

/// Effective polarity of an edge's outcome for contradiction checks and
/// intervention labels: a stored polarity (v4) wins over the text heuristic —
/// 'negative' counts as failure, 'positive' as success, and 'mixed'/'neutral'
/// as neither (they never auto-invalidate and never label a chain SAFE/DANGER
/// on their own). `None` (legacy rows) falls back to the signal-word
/// heuristic on the outcome text.
pub fn effective_polarity(stored: Option<&str>, outcome_text: &str) -> Option<bool> {
    match stored {
        Some("negative") => Some(false),
        Some("positive") => Some(true),
        Some(_) => None,
        None => outcome_polarity(outcome_text),
    }
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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record a decision and its outcome, creating the causal edge.
    pub fn record_decision(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: Option<&str>,
        confidence: f64,
        discovered_by: &str,
    ) -> Result<String> {
        self.record_decision_at(
            decision,
            outcome,
            relation,
            task_tag,
            confidence,
            discovered_by,
            chrono::Utc::now().timestamp(),
        )
    }

    /// Record with an explicit event_time (for extractors that know the real event time).
    /// discovered_at defaults to now() (DB write time).
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision_at(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: Option<&str>,
        confidence: f64,
        discovered_by: &str,
        event_time: i64,
    ) -> Result<String> {
        self.record_decision_full(
            decision,
            outcome,
            relation,
            task_tag,
            confidence,
            discovered_by,
            event_time,
            None,
        )
    }

    /// Record with an explicit event_time and a pre-judged outcome polarity
    /// (v4: positive/negative/mixed/neutral, judged by the LLM or the
    /// heuristic at the caller). `None` stores NULL — read paths then fall
    /// back to the signal-word heuristic.
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision_full(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: Option<&str>,
        confidence: f64,
        discovered_by: &str,
        event_time: i64,
        outcome_polarity: Option<&str>,
    ) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let db_time = chrono::Utc::now().timestamp();
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dec_id = format!("d{}{}", event_time, seq);
        let out_id = format!("o{}{}", event_time, seq);

        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&dec_id, decision, event_time],
        )?;
        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&out_id, outcome, event_time],
        )?;
        // Contradiction short-circuit (rule-based, no LLM): if the same decision
        // already has valid edges whose outcome is contradicted by the new one,
        // the old lesson is falsified by the new evidence — soft-invalidate it.
        // Must run BEFORE inserting the new edge so the new edge is never matched.
        Self::invalidate_contradicted_edges(&conn, decision, outcome, outcome_polarity, db_time)?;
        conn.execute(
            "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag, outcome_polarity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![&dec_id, &out_id, relation, confidence, discovered_by, event_time, db_time, task_tag, outcome_polarity],
        )?;
        Ok(dec_id)
    }

    /// Soft-invalidate valid edges on the same decision text whose outcome
    /// contradicts the new outcome. Returns the number of invalidated edges.
    ///
    /// Conservative rule: only "old edge clearly negative AND new edge clearly
    /// positive" auto-invalidates. A stored polarity (v4) wins over the text
    /// heuristic — 'negative' counts as failure, 'positive' as success, and
    /// 'mixed'/'neutral' never trigger on either side; edges with NULL stored
    /// polarity fall back to the signal-word heuristic on the outcome text.
    fn invalidate_contradicted_edges(
        conn: &Connection,
        decision: &str,
        new_outcome: &str,
        new_polarity: Option<&str>,
        now: i64,
    ) -> Result<usize> {
        let mut stmt = conn.prepare(
            "SELECT ce.id, ct.text, ce.outcome_polarity
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE cf.text = ?1 AND ce.valid_to IS NULL",
        )?;
        let old_edges: Vec<(i64, String, Option<String>)> = stmt
            .query_map(params![decision], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let new_eff = effective_polarity(new_polarity, new_outcome);
        let mut invalidated = 0;
        for (edge_id, old_outcome, old_polarity) in old_edges {
            let old_eff = effective_polarity(old_polarity.as_deref(), &old_outcome);
            if old_eff == Some(false) && new_eff == Some(true) {
                conn.execute(
                    "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2",
                    params![now, edge_id],
                )?;
                invalidated += 1;
            }
        }
        Ok(invalidated)
    }

    /// Semantic extension of the contradiction short-circuit: soft-invalidate
    /// valid edges whose decision text DIFFERS from `decision` (same-text
    /// edges are the exact-match path's job) but whose embedding is highly
    /// similar to `query_embedding`, when the old outcome contradicts
    /// `new_outcome`. Pure sync — the caller (MCP/CLI layer) supplies the
    /// embedding, or skips this entirely when embeddings are unavailable.
    /// Returns the number of invalidated edges.
    ///
    /// Uses the same conservative polarity rule as
    /// `invalidate_contradicted_edges` (stored polarity wins, only
    /// negative-old + positive-new invalidates, NULL falls back to heuristic).
    pub fn invalidate_semantic_contradictions(
        &self,
        decision: &str,
        new_outcome: &str,
        new_polarity: Option<&str>,
        query_embedding: &[f32],
        min_similarity: f64,
    ) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let mut stmt = conn.prepare(
            "SELECT ce.id, ct.text, ce.outcome_polarity, ee.vector
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             JOIN edge_embeddings ee ON ee.edge_id = ce.id
             WHERE cf.text != ?1 AND ce.valid_to IS NULL",
        )?;
        let rows = stmt.query_map(params![decision], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let new_eff = effective_polarity(new_polarity, new_outcome);
        let mut invalidated = 0;
        for row in rows {
            let (edge_id, old_outcome, old_polarity, blob) =
                row.map_err(|e| anyhow!("Query failed: {e}"))?;
            // Skip corrupt blobs instead of failing the whole scan.
            let Ok(vec) = crate::embed::blob_to_vec(&blob) else {
                continue;
            };
            if crate::embed::cosine_similarity(query_embedding, &vec) < min_similarity {
                continue;
            }
            let old_eff = effective_polarity(old_polarity.as_deref(), &old_outcome);
            if old_eff == Some(false) && new_eff == Some(true) {
                conn.execute(
                    "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2",
                    params![now, edge_id],
                )?;
                invalidated += 1;
            }
        }
        Ok(invalidated)
    }

    /// Soft-invalidate an edge: set valid_to = now. Returns true if a row was
    /// actually invalidated; false if the edge does not exist or was already
    /// invalidated (no-op).
    pub fn invalidate_edge(&self, edge_id: i64) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let n = conn.execute(
            "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![now, edge_id],
        )?;
        Ok(n > 0)
    }

    /// Persist an LLM re-judged confidence on all valid edges originating
    /// from a decision chunk. Returns the number of edges updated.
    /// Used by the CLI `judge` path — previously the re-judged confidence
    /// was only printed, never written back, so DB confidence stayed at
    /// whatever the rule-based extractor had set.
    pub fn rejudge_decision(
        &self,
        from_id: &str,
        confidence: f64,
        discovered_by: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let n = conn.execute(
            "UPDATE causal_edges SET confidence = ?1, discovered_by = ?2
             WHERE from_id = ?3 AND valid_to IS NULL",
            params![confidence.clamp(0.0, 1.0), discovered_by, from_id],
        )?;
        Ok(n)
    }

    /// Fetch a single edge by id, including its invalidation status and audit
    /// fields. Unlike the read paths, this does NOT filter on valid_to.
    pub fn get_edge(&self, edge_id: i64) -> Result<Option<CausalEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.id = ?1"
        );
        let entry = conn
            .query_row(&sql, params![edge_id], entry_from_row)
            .optional()?;
        Ok(entry)
    }

    /// Search past causal episodes by task tag and/or text similarity.
    /// Returns entries ordered by confidence descending.
    pub fn search_causal(
        &self,
        task_tag: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<CausalEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;

        let mut sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL"
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(tag) = task_tag {
            sql.push_str(" AND ce.task_tag = ?");
            bind.push(Box::new(tag.to_string()));
        }
        if let Some(q) = query {
            sql.push_str(" AND (cf.text LIKE ? OR ct.text LIKE ?)");
            let pattern = format!("%{}%", q);
            bind.push(Box::new(pattern.clone()));
            bind.push(Box::new(pattern));
        }
        sql.push_str(" ORDER BY ce.confidence DESC");

        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), entry_from_row)?;
        let entries = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;
        Self::record_access(&conn, entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// BM25 keyword retrieval over valid edges (`valid_to IS NULL`).
    ///
    /// Each edge's document is `decision_text + " " + outcome_text`, tokenized
    /// with `patterns::tokenize` (English words minus stop words, Chinese
    /// bigrams). With `task_tag` set, the candidate set is filtered FIRST and
    /// the index is built only over that task's edges, so IDF statistics are
    /// computed within the task domain rather than diluted across all tasks.
    /// Returns up to `limit` entries ordered by BM25 score descending.
    ///
    /// Implementation note: the index is rebuilt per query in memory. Edge
    /// counts are in the hundreds-to-thousands range, so rebuild + full scan
    /// costs well under a millisecond — a persisted index (or FTS5) is
    /// deliberately not introduced at this scale, mirroring the brute-force
    /// rationale of `search_causal_semantic`.
    ///
    /// An empty query (no tokens after tokenization, e.g. stop-words-only)
    /// falls back to the plain task_tag listing of `search_causal`, truncated
    /// to `limit`. Access tracking is recorded like all other read paths.
    pub fn search_causal_bm25(
        &self,
        task_tag: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<CausalEntry>> {
        let query_tokens = crate::patterns::tokenize(query);
        if query_tokens.is_empty() {
            let mut entries = self.search_causal(task_tag, None)?;
            entries.truncate(limit);
            return Ok(entries);
        }

        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL"
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(tag) = task_tag {
            sql.push_str(" AND ce.task_tag = ?");
            bind.push(Box::new(tag.to_string()));
        }
        sql.push_str(" ORDER BY ce.id");

        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), entry_from_row)?;
        let candidates: Vec<CausalEntry> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;

        let index = crate::bm25::Bm25Index::build(candidates.iter().map(|e| {
            (
                e.edge_id.to_string(),
                crate::patterns::tokenize(&format!("{} {}", e.decision_text, e.outcome_text)),
            )
        }));
        let scored = index.search(&query_tokens, limit);

        let by_id: std::collections::HashMap<i64, CausalEntry> =
            candidates.into_iter().map(|e| (e.edge_id, e)).collect();
        let entries: Vec<CausalEntry> = scored
            .iter()
            .filter_map(|(key, _)| key.parse::<i64>().ok())
            .filter_map(|id| by_id.get(&id).cloned())
            .collect();
        Self::record_access(&conn, entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }
    pub fn put_embedding(&self, edge_id: i64, model: &str, vector: &[f32]) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        conn.execute(
            "INSERT INTO edge_embeddings (edge_id, model, vector, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(edge_id) DO UPDATE SET
                 model = excluded.model,
                 vector = excluded.vector,
                 created_at = excluded.created_at",
            params![
                edge_id,
                model,
                crate::embed::vec_to_blob(vector),
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(())
    }

    /// Semantic search: cosine-rank `query_vec` against the embeddings of all
    /// valid edges, optionally filtered by task_tag. Returns the top `limit`
    /// entries with their similarity, descending. Access tracking is recorded.
    ///
    /// Implementation note: brute-force scan in Rust memory. Edge counts are in
    /// the hundreds-to-thousands range, so a full scan per query costs well
    /// under a millisecond — a vector index (sqlite-vec / ANN) is deliberately
    /// not introduced at this scale.
    pub fn search_causal_semantic(
        &self,
        query_vec: &[f32],
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(CausalEntry, f64)>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;

        let mut sql = format!(
            "SELECT {ENTRY_COLUMNS}, ee.vector
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             JOIN edge_embeddings ee ON ee.edge_id = ce.id
             WHERE ce.valid_to IS NULL"
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(tag) = task_tag {
            sql.push_str(" AND ce.task_tag = ?");
            bind.push(Box::new(tag.to_string()));
        }

        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok((entry_from_row(row)?, row.get::<_, Vec<u8>>(14)?))
        })?;

        let mut scored: Vec<(CausalEntry, f64)> = Vec::new();
        for row in rows {
            let (entry, blob) = row.map_err(|e| anyhow!("Query failed: {e}"))?;
            // Skip rows with corrupt blobs instead of failing the whole search.
            let Ok(vec) = crate::embed::blob_to_vec(&blob) else {
                continue;
            };
            let sim = crate::embed::cosine_similarity(query_vec, &vec);
            scored.push((entry, sim));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Self::record_access(&conn, scored.iter().map(|(e, _)| e.edge_id))?;
        Ok(scored)
    }

    /// Semantic seed lookup for intervention queries: cosine-rank
    /// `query_embedding` against valid edge embeddings and keep only edges at
    /// or above `min_similarity`. Delegates to `search_causal_semantic` —
    /// results come back sorted by similarity descending, so filtering the
    /// top `limit` by threshold is equivalent to filtering before truncation.
    /// Access tracking is recorded for the candidates.
    pub fn similar_decision_edges(
        &self,
        query_embedding: &[f32],
        limit: usize,
        min_similarity: f64,
    ) -> Result<Vec<(CausalEntry, f64)>> {
        let mut scored = self.search_causal_semantic(query_embedding, None, limit)?;
        scored.retain(|(_, sim)| *sim >= min_similarity);
        Ok(scored)
    }

    /// Valid edges that have no embedding yet (for CLI backfill).
    /// Returns (edge_id, "decision outcome") pairs — the same text shape the
    /// record path embeds.
    pub fn edges_without_embedding(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT ce.id, cf.text, ct.text
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             LEFT JOIN edge_embeddings ee ON ee.edge_id = ce.id
             WHERE ee.edge_id IS NULL AND ce.valid_to IS NULL
             ORDER BY ce.id
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let edge_id: i64 = row.get(0)?;
            let decision: String = row.get(1)?;
            let outcome: String = row.get(2)?;
            Ok((edge_id, format!("{decision} {outcome}")))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Valid edges that have no stored outcome polarity yet (for the CLI
    /// `polarity` backfill). Returns (edge_id, decision, outcome) triples.
    pub fn edges_without_polarity(&self, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT ce.id, cf.text, ct.text
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.outcome_polarity IS NULL AND ce.valid_to IS NULL
             ORDER BY ce.id
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<(i64, String, String)>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Store the outcome polarity of an edge (v4). The CHECK constraint
    /// rejects values outside positive/negative/mixed/neutral.
    pub fn set_outcome_polarity(&self, edge_id: i64, polarity: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        conn.execute(
            "UPDATE causal_edges SET outcome_polarity = ?1 WHERE id = ?2",
            params![polarity, edge_id],
        )?;
        Ok(())
    }

    /// Trace which decisions could have caused a given outcome (reverse lookup).
    pub fn trace_cause(&self, outcome_description: &str) -> Result<Vec<CausalEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let pattern = format!("%{}%", outcome_description);
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ct.text LIKE ?1 AND ce.valid_to IS NULL
             ORDER BY ce.confidence DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern], entry_from_row)?;
        let entries = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;
        Self::record_access(&conn, entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// Multi-hop causal trace: follow causal chains backward from an outcome.
    ///
    /// Uses SQLite recursive CTE to walk the causal graph. Each hop multiplies
    /// confidence (chain degrades as length grows). Paths are pruned by
    /// `min_confidence` and `max_depth`.
    ///
    /// This is the key differentiator from single-hop `trace_cause`: real
    /// debugging requires chains like
    ///   service crashed ← OOM ← cache had no TTL ← Redis configured without expiry
    pub fn trace_cause_chain(
        &self,
        outcome_description: &str,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<ChainHop>>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let pattern = format!("%{}%", outcome_description);

        let sql = r#"
            WITH RECURSIVE chain(node_id, path_json, depth, chain_confidence) AS (
                -- Anchor: edges whose outcome matches the query.
                SELECT ce.from_id,
                       json_array(json_object(
                           'hop', 1,
                           'edge_id', ce.id,
                           'from_id', ce.from_id,
                           'to_id', ce.to_id,
                           'rel', ce.relation,
                           'conf', ce.confidence,
                           'pol', ce.outcome_polarity
                       )),
                       1,
                       ce.confidence
                FROM causal_edges ce
                JOIN chunks c ON c.id = ce.to_id
                WHERE c.text LIKE ?1
                  AND ce.confidence >= ?2
                  AND ce.valid_to IS NULL

                UNION ALL

                -- Recursive: walk backward from node_id (the previous hop's decision)
                -- to find the decision that caused it.
                SELECT ce2.from_id,
                       json_insert(ch.path_json, '$[#]', json_object(
                           'hop', ch.depth + 1,
                           'edge_id', ce2.id,
                           'from_id', ce2.from_id,
                           'to_id', ce2.to_id,
                           'rel', ce2.relation,
                           'conf', ce2.confidence,
                           'pol', ce2.outcome_polarity
                       )),
                       ch.depth + 1,
                       ch.chain_confidence * ce2.confidence
                FROM causal_edges ce2
                JOIN chain ch ON ce2.to_id = ch.node_id
                WHERE ch.depth < ?3
                  AND ce2.confidence >= ?2
                  AND ch.chain_confidence * ce2.confidence >= ?2
                  AND ce2.valid_to IS NULL
            )
            SELECT path_json FROM chain
            WHERE depth >= 2
            ORDER BY depth DESC, chain_confidence DESC
            LIMIT 50
            "#;

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![pattern, min_confidence, max_depth as i64], |row| {
            let path_json: String = row.get(0)?;
            Ok(path_json)
        })?;

        let paths_json: Vec<String> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("CTE query failed: {e}"))?;

        let mut chains: Vec<Vec<ChainHop>> = Vec::new();
        for path_json in paths_json {
            let hops: Vec<serde_json::Value> =
                match serde_json::from_str::<Vec<serde_json::Value>>(&path_json) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

            let mut chain = Vec::new();
            let mut running_conf = 1.0;
            for hop_val in hops {
                let hop = hop_val["hop"].as_u64().unwrap_or(0) as usize;
                let edge_id = hop_val["edge_id"].as_i64().unwrap_or(0);
                let from_id = hop_val["from_id"].as_str().unwrap_or("").to_string();
                let to_id = hop_val["to_id"].as_str().unwrap_or("").to_string();
                let rel = hop_val["rel"].as_str().unwrap_or("").to_string();
                let conf = hop_val["conf"].as_f64().unwrap_or(0.5);
                let pol = hop_val["pol"].as_str().map(String::from);
                running_conf *= conf;

                // Resolve text from chunks (single query per chain, or inline if cached).
                // For v0.2 we do a lightweight lookup per node.
                let (dec_text, out_text) =
                    Self::resolve_chunk_pair(&conn, &from_id, &to_id).unwrap_or_default();

                chain.push(ChainHop {
                    hop,
                    edge_id,
                    decision_id: from_id.clone(),
                    decision_text: dec_text,
                    outcome_id: to_id.clone(),
                    outcome_text: out_text,
                    relation: rel,
                    confidence: conf,
                    chain_confidence: running_conf,
                    outcome_polarity: pol,
                });
            }
            if !chain.is_empty() {
                chains.push(chain);
            }
        }
        Self::record_access(&conn, chains.iter().flatten().map(|hop| hop.edge_id))?;
        Ok(chains)
    }

    /// 正向多跳:从匹配 decision 文本的 chunk 出发,沿 from_id→to_id 向下游走。
    /// 返回多条链,每条链 Vec<ChainHop>(hop 从 1 开始),chain_confidence 逐跳相乘。
    /// 与 trace_cause_chain 对称:递归 CTE、max_depth、min_confidence 剪枝、
    /// valid_to IS NULL 过滤、depth 降序/链置信度降序、access 追踪(access_count+1)。
    ///
    /// 与 trace_cause_chain 的唯一差异(除方向外):不过滤 depth >= 2。
    /// 反向有单跳 trace_cause 兜底,正向没有对应的单跳查询,因此 depth >= 1
    /// 的链都返回(单跳直接后果对干预查询同样有价值)。
    ///
    /// `prevented`/`no_effect` 边照常参与游走,relation 原样回显,由调用方
    /// (intervention_query / agent)判断语义。
    pub fn trace_effect_chain(
        &self,
        decision_description: &str,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<ChainHop>>> {
        let pattern = format!("%{}%", decision_description);
        self.trace_effect_chain_impl(
            "JOIN chunks c ON c.id = ce.from_id WHERE c.text LIKE ?1",
            vec![Box::new(pattern)],
            max_depth,
            min_confidence,
        )
    }

    /// Forward multi-hop variant anchored on explicit decision chunk ids
    /// (semantic seed path of intervention_query): identical recursive walk
    /// to `trace_effect_chain`, but the anchor is `ce.from_id IN (...)`
    /// instead of a LIKE on the decision text.
    pub fn trace_effect_chain_from_ids(
        &self,
        decision_ids: &[String],
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<ChainHop>>> {
        if decision_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (1..=decision_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let anchor = format!("WHERE ce.from_id IN ({placeholders})");
        let binds = decision_ids
            .iter()
            .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        self.trace_effect_chain_impl(&anchor, binds, max_depth, min_confidence)
    }

    /// Shared forward-walk implementation. `anchor` is the JOIN/WHERE fragment
    /// of the anchor SELECT and owns placeholders ?1..=?N; min_confidence and
    /// max_depth are bound at ?N+1 / ?N+2.
    fn trace_effect_chain_impl(
        &self,
        anchor: &str,
        anchor_binds: Vec<Box<dyn rusqlite::ToSql>>,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<ChainHop>>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let conf_p = anchor_binds.len() + 1;
        let depth_p = anchor_binds.len() + 2;

        let sql = format!(
            r#"
            WITH RECURSIVE chain(node_id, path_json, depth, chain_confidence) AS (
                -- Anchor: edges whose decision matches the query.
                SELECT ce.to_id,
                       json_array(json_object(
                           'hop', 1,
                           'edge_id', ce.id,
                           'from_id', ce.from_id,
                           'to_id', ce.to_id,
                           'rel', ce.relation,
                           'conf', ce.confidence,
                           'pol', ce.outcome_polarity
                       )),
                       1,
                       ce.confidence
                FROM causal_edges ce
                {anchor}
                  AND ce.confidence >= ?{conf_p}
                  AND ce.valid_to IS NULL

                UNION ALL

                -- Recursive: walk forward from node_id (the previous hop's outcome)
                -- to find what it caused next.
                SELECT ce2.to_id,
                       json_insert(ch.path_json, '$[#]', json_object(
                           'hop', ch.depth + 1,
                           'edge_id', ce2.id,
                           'from_id', ce2.from_id,
                           'to_id', ce2.to_id,
                           'rel', ce2.relation,
                           'conf', ce2.confidence,
                           'pol', ce2.outcome_polarity
                       )),
                       ch.depth + 1,
                       ch.chain_confidence * ce2.confidence
                FROM causal_edges ce2
                JOIN chain ch ON ce2.from_id = ch.node_id
                WHERE ch.depth < ?{depth_p}
                  AND ce2.confidence >= ?{conf_p}
                  AND ch.chain_confidence * ce2.confidence >= ?{conf_p}
                  AND ce2.valid_to IS NULL
            )
            SELECT path_json FROM chain
            WHERE depth >= 1
            ORDER BY depth DESC, chain_confidence DESC
            LIMIT 50
            "#
        );

        let mut stmt = conn.prepare(&sql)?;
        let max_depth_i = max_depth as i64;
        let mut bind_refs: Vec<&dyn rusqlite::ToSql> =
            anchor_binds.iter().map(|b| b.as_ref()).collect();
        bind_refs.push(&min_confidence);
        bind_refs.push(&max_depth_i);
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            let path_json: String = row.get(0)?;
            Ok(path_json)
        })?;

        let paths_json: Vec<String> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("CTE query failed: {e}"))?;

        let mut chains: Vec<Vec<ChainHop>> = Vec::new();
        for path_json in paths_json {
            let hops: Vec<serde_json::Value> =
                match serde_json::from_str::<Vec<serde_json::Value>>(&path_json) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

            let mut chain = Vec::new();
            let mut running_conf = 1.0;
            for hop_val in hops {
                let hop = hop_val["hop"].as_u64().unwrap_or(0) as usize;
                let edge_id = hop_val["edge_id"].as_i64().unwrap_or(0);
                let from_id = hop_val["from_id"].as_str().unwrap_or("").to_string();
                let to_id = hop_val["to_id"].as_str().unwrap_or("").to_string();
                let rel = hop_val["rel"].as_str().unwrap_or("").to_string();
                let conf = hop_val["conf"].as_f64().unwrap_or(0.5);
                let pol = hop_val["pol"].as_str().map(String::from);
                running_conf *= conf;

                let (dec_text, out_text) =
                    Self::resolve_chunk_pair(&conn, &from_id, &to_id).unwrap_or_default();

                chain.push(ChainHop {
                    hop,
                    edge_id,
                    decision_id: from_id.clone(),
                    decision_text: dec_text,
                    outcome_id: to_id.clone(),
                    outcome_text: out_text,
                    relation: rel,
                    confidence: conf,
                    chain_confidence: running_conf,
                    outcome_polarity: pol,
                });
            }
            if !chain.is_empty() {
                chains.push(chain);
            }
        }
        Self::record_access(&conn, chains.iter().flatten().map(|hop| hop.edge_id))?;
        Ok(chains)
    }

    /// Bump access counters for edges returned by a read-path query.
    /// Single UPDATE with deduped ids; no-op on empty input.
    fn record_access(conn: &Connection, edge_ids: impl Iterator<Item = i64>) -> Result<()> {
        let mut ids: Vec<i64> = edge_ids.collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "UPDATE causal_edges
             SET access_count = access_count + 1, last_accessed_at = ?1
             WHERE id IN ({placeholders})"
        );
        conn.execute(
            &sql,
            rusqlite::params_from_iter(std::iter::once(now).chain(ids)),
        )?;
        Ok(())
    }

    fn resolve_chunk_pair(
        conn: &Connection,
        from_id: &str,
        to_id: &str,
    ) -> Result<(String, String)> {
        let dec_text: String = conn.query_row(
            "SELECT text FROM chunks WHERE id = ?1",
            params![from_id],
            |row| row.get(0),
        )?;
        let out_text: String = conn.query_row(
            "SELECT text FROM chunks WHERE id = ?1",
            params![to_id],
            |row| row.get(0),
        )?;
        Ok((dec_text, out_text))
    }

    /// Get recent decisions for L0 directory (system prompt injection).
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<DecisionDirectoryEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT cf.id, ce.task_tag, cf.text, ct.text, ce.relation
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             ORDER BY ce.event_time DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let dec_text: String = row.get(2)?;
            let out_text: String = row.get(3)?;
            Ok(DecisionDirectoryEntry {
                id: row.get(0)?,
                task_tag: row.get(1)?,
                decision_snippet: if dec_text.len() > 80 {
                    format!("{}...", &dec_text[..80])
                } else {
                    dec_text
                },
                outcome_snippet: if out_text.len() > 80 {
                    format!("{}...", &out_text[..80])
                } else {
                    out_text
                },
                relation: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Get top decisions by confidence (high-value lessons first).
    pub fn top_decisions_by_confidence(&self, limit: usize) -> Result<Vec<DecisionDirectoryEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT cf.id, ce.task_tag, cf.text, ct.text, ce.relation
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             ORDER BY ce.confidence DESC, ce.event_time DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let dec_text: String = row.get(2)?;
            let out_text: String = row.get(3)?;
            Ok(DecisionDirectoryEntry {
                id: row.get(0)?,
                task_tag: row.get(1)?,
                decision_snippet: if dec_text.chars().count() > 80 {
                    format!("{}...", dec_text.chars().take(80).collect::<String>())
                } else {
                    dec_text
                },
                outcome_snippet: if out_text.chars().count() > 80 {
                    format!("{}...", out_text.chars().take(80).collect::<String>())
                } else {
                    out_text
                },
                relation: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Get all valid causal edges (for the pattern miner). Ordered by edge id
    /// so pair iteration is deterministic across runs.
    pub fn all_valid_edges(&self) -> Result<Vec<CausalEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL
             ORDER BY ce.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], entry_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Idempotent write of a meta-causal edge.
    ///
    /// If a valid meta edge with the same (from_id, to_id, relation) already
    /// exists, its confidence/pattern/discovered_at are refreshed and its id
    /// returned; otherwise a new row is inserted. Running the miner twice
    /// therefore never duplicates meta edges.
    pub fn upsert_meta_edge(
        &self,
        from_id: &str,
        to_id: &str,
        relation: &str,
        pattern: &str,
        confidence: f64,
    ) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM meta_causal_edges
                 WHERE from_id = ?1 AND to_id = ?2 AND relation = ?3 AND valid_to IS NULL",
                params![from_id, to_id, relation],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE meta_causal_edges
                     SET confidence = ?1, pattern = ?2, discovered_at = ?3
                     WHERE id = ?4",
                    params![confidence, pattern, now, id],
                )?;
                Ok(id)
            }
            None => {
                conn.execute(
                    "INSERT INTO meta_causal_edges
                         (from_id, to_id, relation, pattern, confidence, discovered_at, valid_from)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![from_id, to_id, relation, pattern, confidence, now, now],
                )?;
                Ok(conn.last_insert_rowid())
            }
        }
    }

    /// Search mined cross-task patterns (meta-causal edges).
    ///
    /// - `query`: LIKE match against the pattern summary or either endpoint's
    ///   decision text.
    /// - `task_tag`: keep meta edges where at least one endpoint decision chunk
    ///   belongs to a causal edge with this task tag.
    ///
    /// Only valid meta edges are returned, ordered by confidence descending.
    pub fn search_patterns(
        &self,
        query: Option<&str>,
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MetaEdge>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut sql = String::from(
            "SELECT m.id, m.from_id, m.to_id, m.relation, m.pattern, m.confidence,
                    m.discovered_at, m.valid_to, cf.text, ct.text
             FROM meta_causal_edges m
             JOIN chunks cf ON cf.id = m.from_id
             JOIN chunks ct ON ct.id = m.to_id
             WHERE m.valid_to IS NULL",
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(q) = query {
            sql.push_str(" AND (m.pattern LIKE ? OR cf.text LIKE ? OR ct.text LIKE ?)");
            let pattern = format!("%{q}%");
            bind.push(Box::new(pattern.clone()));
            bind.push(Box::new(pattern.clone()));
            bind.push(Box::new(pattern));
        }
        if let Some(tag) = task_tag {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM causal_edges ce
                              WHERE ce.task_tag = ?
                                AND (ce.from_id = m.from_id OR ce.from_id = m.to_id))",
            );
            bind.push(Box::new(tag.to_string()));
        }
        sql.push_str(" ORDER BY m.confidence DESC LIMIT ?");
        bind.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok(MetaEdge {
                id: row.get(0)?,
                from_id: row.get(1)?,
                to_id: row.get(2)?,
                relation: row.get(3)?,
                pattern: row.get(4)?,
                confidence: row.get(5)?,
                discovered_at: row.get(6)?,
                valid_to: row.get(7)?,
                from_text: row.get(8)?,
                to_text: row.get(9)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Count causal edges (for diagnostics).
    pub fn count_edges(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM causal_edges", [], |row| row.get(0))?;
        Ok(n)
    }

    /// Execute a closure with a reference to the connection (for advanced queries).
    /// Used by chain_linker to avoid duplicating the Mutex logic.
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T>,
    {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        f(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_search() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used Redis with mutex lock",
                "deadlock — holder crashed without releasing",
                "caused",
                Some("concurrency"),
                0.85,
                "llm_inferred",
            )
            .unwrap();
        store
            .record_decision(
                "switched to channel/single-flight",
                "successfully fixed race condition",
                "caused",
                Some("concurrency"),
                0.95,
                "user_feedback",
            )
            .unwrap();

        // Search by task
        let results = store.search_causal(Some("concurrency"), None).unwrap();
        assert_eq!(results.len(), 2);
        // Higher confidence first
        assert!(results[0].confidence >= results[1].confidence);

        // Search by query text
        let results = store.search_causal(None, Some("mutex")).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].decision_text.contains("mutex"));

        // Trace cause
        let causes = store.trace_cause("deadlock").unwrap();
        assert_eq!(causes.len(), 1);
        assert!(causes[0].decision_text.contains("mutex"));

        // Recent decisions
        let dir = store.recent_decisions(5).unwrap();
        assert_eq!(dir.len(), 2);
    }

    #[test]
    fn test_multi_hop_trace() {
        let store = CausalStore::open_in_memory().unwrap();

        // Build a 3-hop chain:
        // A: "configured Redis without TTL" → B: "cache entries never expired"
        // B: "cache entries never expired" → C: "memory grew unbounded"
        // C: "memory grew unbounded" → D: "service OOM and crashed"
        let _id_a = store
            .record_decision(
                "configured Redis without TTL",
                "cache entries never expired",
                "caused",
                Some("caching"),
                0.8,
                "llm_inferred",
            )
            .unwrap();
        let _id_b = store
            .record_decision(
                "cache entries never expired",
                "memory grew unbounded",
                "caused",
                Some("caching"),
                0.85,
                "llm_inferred",
            )
            .unwrap();
        // Link B's outcome to C's decision — but B's outcome is not a decision chunk.
        // For this test we create a synthetic chain by making the "outcome" of step 1
        // the "decision" text of step 2. In production the auto-extractor would handle
        // this via outcome-to-decision bridging.
        let _id_c = store
            .record_decision(
                "memory grew unbounded",
                "service OOM and crashed",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();

        // Manually create bridge edges (the chain_linker would do this automatically)
        // These connect outcome_i → decision_j so the CTE can walk multi-hop.
        store.with_conn(|conn| {
            // outcome of A (cache entries never expired) → decision of B (cache entries never expired)
            // But since record_decision creates new chunk IDs each time, we link by text match.
            // Instead, link outcome of B → decision of C:
            conn.execute(
                "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                 SELECT ct.id, cf2.id, 'caused', 0.7, 'rule', 0, 0, 'caching'
                 FROM causal_edges ce1
                 JOIN chunks ct ON ct.id = ce1.to_id
                 JOIN causal_edges ce2 ON ce2.id != ce1.id
                 JOIN chunks cf2 ON cf2.id = ce2.from_id
                 WHERE ct.text = 'memory grew unbounded' AND cf2.text = 'memory grew unbounded'",
                [],
            )?;
            // outcome of A → decision of B
            conn.execute(
                "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                 SELECT ct.id, cf2.id, 'caused', 0.7, 'rule', 0, 0, 'caching'
                 FROM chunks ct
                 JOIN chunks cf2 ON cf2.text = 'cache entries never expired'
                 WHERE ct.text = 'cache entries never expired' AND ct.id LIKE 'o%' AND cf2.id LIKE 'd%' AND cf2.text != 'configured Redis without TTL'",
                [],
            )?;
            Ok(())
        }).unwrap();

        // Single-hop still works
        let single = store.trace_cause("OOM").unwrap();
        assert!(!single.is_empty());

        // Multi-hop: search for "crashed" and walk back 3 hops
        let chains = store.trace_cause_chain("crashed", 5, 0.3).unwrap();
        assert!(!chains.is_empty(), "expected at least one causal chain");
        // Every returned chain must be multi-hop (trace_cause_chain filters depth >= 2)
        let max_len = chains.iter().map(|c| c.len()).max().unwrap();
        assert!(max_len >= 2, "expected a multi-hop (depth >= 2) chain");
        assert!(
            chains.iter().all(|c| c.len() >= 2),
            "trace_cause_chain must only return chains with depth >= 2"
        );
    }

    /// Test helper: insert a raw chunk-to-chunk causal edge, creating chunks on
    /// demand. Chunk ids are derived from the text so the same text is always
    /// the same node — this lets us build clean multi-hop graphs without the
    /// bridge edges record_decision would need.
    fn link(store: &CausalStore, from: &str, to: &str, relation: &str, conf: f64) -> i64 {
        store
            .with_conn(|conn| {
                for text in [from, to] {
                    conn.execute(
                        "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, 1000)",
                        params![format!("chunk:{text}"), text],
                    )?;
                }
                conn.execute(
                    "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
                     VALUES (?1, ?2, ?3, ?4, 'rule', 1000, 1000)",
                    params![format!("chunk:{from}"), format!("chunk:{to}"), relation, conf],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .unwrap()
    }

    #[test]
    fn test_trace_effect_chain_three_hops() {
        let store = CausalStore::open_in_memory().unwrap();
        // A → B → C → D forward chain
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        link(&store, "state bravo", "state charlie", "caused", 0.8);
        link(&store, "state charlie", "state delta", "caused", 0.7);

        let chains = store.trace_effect_chain("action alpha", 5, 0.1).unwrap();
        // CTE yields chains of depth 1, 2 and 3 from the same anchor
        let full = chains
            .iter()
            .find(|c| c.len() == 3)
            .expect("expected the full 3-hop chain");
        // Hop order and texts
        assert_eq!(full[0].hop, 1);
        assert_eq!(full[0].decision_text, "action alpha");
        assert_eq!(full[0].outcome_text, "state bravo");
        assert_eq!(full[1].hop, 2);
        assert_eq!(full[1].decision_text, "state bravo");
        assert_eq!(full[1].outcome_text, "state charlie");
        assert_eq!(full[2].hop, 3);
        assert_eq!(full[2].decision_text, "state charlie");
        assert_eq!(full[2].outcome_text, "state delta");
        // chain_confidence multiplies hop by hop
        assert!((full[0].chain_confidence - 0.9).abs() < 1e-9);
        assert!((full[1].chain_confidence - 0.9 * 0.8).abs() < 1e-9);
        assert!((full[2].chain_confidence - 0.9 * 0.8 * 0.7).abs() < 1e-9);
    }

    #[test]
    fn test_trace_effect_chain_branching() {
        let store = CausalStore::open_in_memory().unwrap();
        // One decision, two downstream effects
        link(&store, "action alpha", "outcome one", "caused", 0.9);
        link(&store, "action alpha", "outcome two", "enabled", 0.8);

        let chains = store.trace_effect_chain("action alpha", 3, 0.1).unwrap();
        assert_eq!(chains.len(), 2, "each downstream edge is its own chain");
        let terminals: Vec<&str> = chains
            .iter()
            .map(|c| c.last().unwrap().outcome_text.as_str())
            .collect();
        assert!(terminals.contains(&"outcome one"));
        assert!(terminals.contains(&"outcome two"));
    }

    #[test]
    fn test_trace_effect_chain_min_confidence_pruning() {
        let store = CausalStore::open_in_memory().unwrap();
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        link(&store, "state bravo", "state charlie", "caused", 0.4);

        // 0.4 edge is below the per-edge threshold and also drags the running
        // chain confidence (0.9*0.4=0.36) below 0.5 — must be pruned.
        let chains = store.trace_effect_chain("action alpha", 5, 0.5).unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 1);
        assert_eq!(chains[0][0].outcome_text, "state bravo");
    }

    #[test]
    fn test_trace_effect_chain_max_depth() {
        let store = CausalStore::open_in_memory().unwrap();
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        link(&store, "state bravo", "state charlie", "caused", 0.9);
        link(&store, "state charlie", "state delta", "caused", 0.9);

        let chains = store.trace_effect_chain("action alpha", 2, 0.1).unwrap();
        let max_len = chains.iter().map(|c| c.len()).max().unwrap();
        assert_eq!(max_len, 2, "max_depth=2 must cap chain length at 2");
    }

    #[test]
    fn test_trace_effect_chain_excludes_invalidated() {
        let store = CausalStore::open_in_memory().unwrap();
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        let edge_bc = link(&store, "state bravo", "state charlie", "caused", 0.9);
        assert!(store.invalidate_edge(edge_bc).unwrap());

        let chains = store.trace_effect_chain("action alpha", 5, 0.1).unwrap();
        assert!(
            chains
                .iter()
                .flatten()
                .all(|h| h.outcome_text != "state charlie"),
            "invalidated edges must not appear in forward chains"
        );
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 1);
    }

    #[test]
    fn test_trace_effect_chain_access_tracking() {
        let store = CausalStore::open_in_memory().unwrap();
        let edge_ab = link(&store, "action alpha", "state bravo", "caused", 0.9);
        let edge_bc = link(&store, "state bravo", "state charlie", "caused", 0.9);

        assert_eq!(store.get_edge(edge_ab).unwrap().unwrap().access_count, 0);
        assert_eq!(store.get_edge(edge_bc).unwrap().unwrap().access_count, 0);

        store.trace_effect_chain("action alpha", 5, 0.1).unwrap();

        let ab = store.get_edge(edge_ab).unwrap().unwrap();
        let bc = store.get_edge(edge_bc).unwrap().unwrap();
        assert_eq!(ab.access_count, 1, "hit edges get access_count + 1");
        assert_eq!(bc.access_count, 1);
        assert!(ab.last_accessed_at.is_some());
        assert!(bc.last_accessed_at.is_some());
    }

    #[test]
    fn test_invalidate_edge() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used global mutable cache",
                "data race under concurrent writes",
                "caused",
                Some("concurrency"),
                0.85,
                "user_feedback",
            )
            .unwrap();

        let edge_id = store.search_causal(Some("concurrency"), None).unwrap()[0].edge_id;

        // Invalidate
        assert!(store.invalidate_edge(edge_id).unwrap());

        // All read paths stop returning it
        assert!(store
            .search_causal(Some("concurrency"), None)
            .unwrap()
            .is_empty());
        assert!(store.search_causal(None, Some("cache")).unwrap().is_empty());
        assert!(store.trace_cause("data race").unwrap().is_empty());
        assert!(store
            .trace_cause_chain("data race", 3, 0.1)
            .unwrap()
            .is_empty());

        // get_edge still sees it, with valid_to set and audit fields populated
        let edge = store.get_edge(edge_id).unwrap().expect("edge must exist");
        assert!(edge.valid_to.is_some());
        assert_eq!(edge.decision_text, "used global mutable cache");
        assert_eq!(edge.discovered_by, "user_feedback");
        assert!(edge.discovered_at > 0);
        // search_causal hit it once before invalidation
        assert_eq!(edge.access_count, 1);
        assert!(edge.last_accessed_at.is_some());

        // Re-invalidate is a no-op
        assert!(!store.invalidate_edge(edge_id).unwrap());
        // Unknown id: false, no error
        assert!(!store.invalidate_edge(999_999).unwrap());
        assert!(store.get_edge(999_999).unwrap().is_none());
    }

    #[test]
    fn test_contradiction_short_circuit() {
        let store = CausalStore::open_in_memory().unwrap();

        // Same decision recorded twice with opposite outcomes: the new evidence
        // falsifies the old lesson, so the old edge is auto-invalidated.
        store
            .record_decision(
                "用方案A部署",
                "部署失败 error: port already in use",
                "caused",
                Some("deploy"),
                0.7,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "用方案A部署",
                "部署成功",
                "caused",
                Some("deploy"),
                0.95,
                "user_feedback",
            )
            .unwrap();

        assert_eq!(store.count_edges().unwrap(), 2);

        // Only the new edge survives on read paths
        let results = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome_text, "部署成功");
        assert!(results[0].valid_to.is_none());
        let new_edge_id = results[0].edge_id;

        // The old edge is invalidated but auditable via get_edge
        let edges: Vec<i64> = store
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT id FROM causal_edges ORDER BY id")?;
                let ids = stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(ids)
            })
            .unwrap();
        let old_edge_id = edges
            .iter()
            .find(|id| **id != new_edge_id)
            .copied()
            .unwrap();
        let old_edge = store.get_edge(old_edge_id).unwrap().unwrap();
        assert!(old_edge.valid_to.is_some());
        assert!(old_edge.outcome_text.contains("部署失败"));

        // trace_cause on the old failure outcome no longer returns it
        assert!(store.trace_cause("port already in use").unwrap().is_empty());
    }

    #[test]
    fn test_contradiction_not_triggered_by_same_direction() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "用方案B部署",
                "部署失败 error",
                "caused",
                Some("deploy"),
                0.7,
                "rule",
            )
            .unwrap();
        // Same-direction (also failure): NOT a contradiction, both stay valid.
        store
            .record_decision(
                "用方案B部署",
                "部署再次失败 timeout",
                "caused",
                Some("deploy"),
                0.7,
                "rule",
            )
            .unwrap();
        let results = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(
            results.len(),
            2,
            "both same-direction edges must stay valid"
        );
    }

    #[test]
    fn test_outcomes_contradict() {
        // Contradicting pairs (one failure, one not) — EN
        assert!(outcomes_contradict(
            "deploy failed with error",
            "deploy succeeded"
        ));
        assert!(outcomes_contradict(
            "deadlock: holder crashed",
            "fixed the race condition"
        ));
        assert!(outcomes_contradict(
            "all tests pass",
            "timeout in integration test"
        ));
        // Contradicting pairs — ZH
        assert!(outcomes_contradict("部署失败,报错端口占用", "部署成功"));
        assert!(outcomes_contradict("服务崩溃", "问题已修复,运行正常"));
        // Failure vs neutral counts as contradiction (old lesson falsified)
        assert!(outcomes_contradict(
            "panic: index out of bounds",
            "deploy finished"
        ));

        // Same direction — NOT contradictions
        assert!(!outcomes_contradict(
            "deploy failed",
            "another error occurred"
        ));
        assert!(!outcomes_contradict("死锁复现", "再次崩溃"));
        assert!(!outcomes_contradict(
            "deploy succeeded",
            "all checks passed"
        ));
        assert!(!outcomes_contradict("部署成功", "测试通过"));
        // Both neutral — NOT a contradiction
        assert!(!outcomes_contradict("deploy finished", "rollout completed"));
        // Success overrides a co-occurring failure word — same direction, NOT a contradiction
        assert!(!outcomes_contradict("fixed the error", "deploy succeeded"));
    }

    #[test]
    fn test_outcome_polarity_word_boundaries() {
        // "unresolved" must NOT hit the "resolved" success token
        assert_eq!(outcome_polarity("unresolved issue"), None);
        // Failure word names the problem that was fixed → success
        assert_eq!(outcome_polarity("deadlock resolved"), Some(true));
        assert_eq!(outcome_polarity("deploy success"), Some(true));
        // Inflections of "success" still match on word boundaries
        assert_eq!(
            outcome_polarity("cargo build completed successfully"),
            Some(true)
        );
        assert_eq!(outcome_polarity("build succeeded"), Some(true));
        // ...but the negated form does not
        assert_eq!(outcome_polarity("deploy unsuccessful"), None);
        // Short-token boundary checks: "invoke"/"compass" are not "ok"/"pass"
        assert_eq!(outcome_polarity("invoke compass"), None);
        assert_eq!(outcome_polarity("all tests pass"), Some(true));
        // ZH signals keep substring matching
        assert_eq!(outcome_polarity("问题已修复,运行正常"), Some(true));
    }

    #[test]
    fn test_semantic_search() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used mutex",
                "deadlock",
                "caused",
                Some("concurrency"),
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "switched to channel",
                "fixed race",
                "caused",
                Some("concurrency"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "redis without ttl",
                "stampede",
                "caused",
                Some("caching"),
                0.7,
                "rule",
            )
            .unwrap();

        // Fresh in-memory DB → edge ids are 1, 2, 3 in insertion order.
        store.put_embedding(1, "test", &[1.0, 0.0, 0.0]).unwrap();
        store.put_embedding(2, "test", &[0.9, 0.1, 0.0]).unwrap();
        store.put_embedding(3, "test", &[0.0, 1.0, 0.0]).unwrap();

        // Ranking: descending cosine similarity, exact match first.
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 10)
            .unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].0.edge_id, 1);
        assert!((res[0].1 - 1.0).abs() < 1e-6);
        assert!(res[0].1 > res[1].1 && res[1].1 > res[2].1);

        // task_tag filter
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], Some("caching"), 10)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0.edge_id, 3);

        // limit
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 1)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0.edge_id, 1);

        // Invalidated edges must not appear.
        store.invalidate_edge(1).unwrap();
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 10)
            .unwrap();
        assert!(res.iter().all(|(e, _)| e.edge_id != 1));

        // Edges without an embedding never appear in semantic results.
        store
            .record_decision("no vector edge", "nothing", "caused", None, 0.5, "rule")
            .unwrap();
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 10)
            .unwrap();
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_put_embedding_overwrites() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision("d", "o", "caused", None, 0.5, "rule")
            .unwrap();
        store.put_embedding(1, "test", &[1.0, 0.0]).unwrap();
        // Overwrite with a different vector — must replace, not duplicate/fail.
        store.put_embedding(1, "test", &[0.0, 1.0]).unwrap();
        let res = store.search_causal_semantic(&[1.0, 0.0], None, 10).unwrap();
        assert_eq!(res.len(), 1);
        assert!(
            res[0].1 < 1e-6,
            "overwritten vector must be the one searched"
        );
    }

    #[test]
    fn test_edges_without_embedding() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision("mutex", "deadlock", "caused", None, 0.8, "rule")
            .unwrap();
        store
            .record_decision("channel", "fixed race", "caused", None, 0.9, "rule")
            .unwrap();

        let pending = store.edges_without_embedding(10).unwrap();
        assert_eq!(pending.len(), 2);
        // Text is "decision outcome" — the shape the record path embeds.
        assert!(pending[0].1.contains("mutex"));
        assert!(pending[0].1.contains("deadlock"));

        store.put_embedding(1, "test", &[1.0]).unwrap();
        let pending = store.edges_without_embedding(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, 2);

        // limit
        let pending = store.edges_without_embedding(1).unwrap();
        assert_eq!(pending.len(), 1);

        // Invalidated edges are excluded from backfill.
        store.invalidate_edge(2).unwrap();
        assert!(store.edges_without_embedding(10).unwrap().is_empty());
    }

    #[test]
    fn test_similar_decision_edges() {
        let store = CausalStore::open_in_memory().unwrap();
        // Distinct decision texts keep the contradiction short-circuit out of
        // the way; fresh in-memory DB → edge ids 1..=5 in insertion order.
        store
            .record_decision("used Redis mutex", "deadlock", "caused", None, 0.8, "rule")
            .unwrap();
        store
            .record_decision(
                "used Redis lock",
                "deadlock again",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "switched to channel",
                "fixed race",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "added cache TTL",
                "stampede stopped",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 5 gets no embedding — semantic paths must never return it.
        store
            .record_decision("no vector edge", "nothing", "caused", None, 0.5, "rule")
            .unwrap();

        store.put_embedding(1, "test", &[1.0, 0.0, 0.0]).unwrap(); // sim 1.0
        store.put_embedding(2, "test", &[0.9, 0.1, 0.0]).unwrap(); // sim ≈ 0.994
        store.put_embedding(3, "test", &[0.6, 0.8, 0.0]).unwrap(); // sim 0.6
        store.put_embedding(4, "test", &[0.0, 1.0, 0.0]).unwrap(); // sim 0.0

        // Threshold 0.5: edges 1-3, ranked by similarity descending.
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 10, 0.5)
            .unwrap();
        let ids: Vec<i64> = res.iter().map(|(e, _)| e.edge_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(res[0].1 > res[1].1 && res[1].1 > res[2].1);

        // A higher threshold filters out the mid-similarity edge.
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 10, 0.9)
            .unwrap();
        let ids: Vec<i64> = res.iter().map(|(e, _)| e.edge_id).collect();
        assert_eq!(ids, vec![1, 2]);

        // limit applies to the sorted list.
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 1, 0.5)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0.edge_id, 1);

        // Invalidated edges never seed interventions.
        store.invalidate_edge(2).unwrap();
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 10, 0.5)
            .unwrap();
        assert!(res.iter().all(|(e, _)| e.edge_id != 2));
    }

    #[test]
    fn test_invalidate_semantic_contradictions() {
        let store = CausalStore::open_in_memory().unwrap();
        // Edge 1: old lesson with a failure outcome, vector close to the query.
        store
            .record_decision(
                "用 Redis 加互斥锁",
                "死锁:持有者崩溃",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 2: vector equally close, but the outcome does NOT contradict.
        store
            .record_decision(
                "Redis mutex for stampede protection",
                "成功防止缓存击穿",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 3: contradicting outcome, but a distant (orthogonal) vector.
        store
            .record_decision(
                "switched to channel single-flight",
                "panic under load",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 4: exact same decision text as the new edge — that is the
        // exact-match path's job; the semantic path must skip it even with a
        // close vector and a contradicting outcome.
        store
            .record_decision(
                "used Redis with mutex lock",
                "deadlock occurred",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();

        store.put_embedding(1, "test", &[0.95, 0.05, 0.0]).unwrap();
        store.put_embedding(2, "test", &[0.95, 0.05, 0.0]).unwrap();
        store.put_embedding(3, "test", &[0.0, 1.0, 0.0]).unwrap();
        store.put_embedding(4, "test", &[1.0, 0.0, 0.0]).unwrap();

        // New edge: decision text differs from edges 1-3, and its success
        // outcome contradicts the failure outcomes of edges 1/3/4.
        let n = store
            .invalidate_semantic_contradictions(
                "used Redis with mutex lock",
                "成功修复,运行正常",
                None,
                &[1.0, 0.0, 0.0],
                0.85,
            )
            .unwrap();
        assert_eq!(n, 1, "only edge 1 (close vector + contradicting outcome)");

        assert!(store.get_edge(1).unwrap().unwrap().valid_to.is_some());
        let e2 = store.get_edge(2).unwrap().unwrap();
        assert!(e2.valid_to.is_none(), "no contradiction → kept");
        let e3 = store.get_edge(3).unwrap().unwrap();
        assert!(e3.valid_to.is_none(), "low similarity → kept");
        let e4 = store.get_edge(4).unwrap().unwrap();
        assert!(e4.valid_to.is_none(), "same text → exact-match path's job");

        // A query vector with no close neighbors invalidates nothing.
        let n = store
            .invalidate_semantic_contradictions(
                "another decision",
                "再次失败",
                None,
                &[0.0, 0.0, 1.0],
                0.85,
            )
            .unwrap();
        assert_eq!(n, 0);
        assert!(store.get_edge(2).unwrap().unwrap().valid_to.is_none());
        assert!(store.get_edge(3).unwrap().unwrap().valid_to.is_none());
        assert!(store.get_edge(4).unwrap().unwrap().valid_to.is_none());
    }

    #[test]
    fn test_record_with_polarity_and_cte_propagation() {
        let store = CausalStore::open_in_memory().unwrap();
        // Build A → B → C with the link helper (raw edges, polarity NULL).
        let edge_ab = link(&store, "action alpha", "state bravo", "caused", 0.9);
        let edge_bc = link(&store, "state bravo", "state charlie", "caused", 0.9);

        // Both edges lack a stored polarity → eligible for backfill.
        let pending = store.edges_without_polarity(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].0, edge_ab);
        assert!(pending[0].1.contains("action alpha"));
        assert!(pending[0].2.contains("state bravo"));

        store.set_outcome_polarity(edge_ab, "negative").unwrap();
        store.set_outcome_polarity(edge_bc, "mixed").unwrap();
        assert!(store.edges_without_polarity(10).unwrap().is_empty());
        // Out-of-enum values are rejected by the CHECK constraint.
        assert!(store.set_outcome_polarity(edge_ab, "bogus").is_err());

        // Forward CTE hops carry the stored polarity.
        let chains = store.trace_effect_chain("action alpha", 5, 0.1).unwrap();
        let full = chains.iter().find(|c| c.len() == 2).expect("2-hop chain");
        assert_eq!(full[0].outcome_polarity.as_deref(), Some("negative"));
        assert_eq!(full[1].outcome_polarity.as_deref(), Some("mixed"));

        // Backward CTE hops carry it too.
        let chains = store.trace_cause_chain("state charlie", 5, 0.1).unwrap();
        let full = chains.iter().find(|c| c.len() == 2).expect("2-hop chain");
        assert_eq!(full[0].outcome_polarity.as_deref(), Some("mixed"));
        assert_eq!(full[1].outcome_polarity.as_deref(), Some("negative"));

        // record_decision_full persists the polarity it is given; the plain
        // record_decision path stores NULL.
        store
            .record_decision_full(
                "used Redis mutex",
                "deadlock under load; fixed by switching to channels",
                "caused",
                None,
                0.8,
                "rule",
                1000,
                Some("mixed"),
            )
            .unwrap();
        store
            .record_decision("plain record", "nothing", "caused", None, 0.5, "rule")
            .unwrap();
        let pending = store.edges_without_polarity(10).unwrap();
        assert_eq!(pending.len(), 1, "only the NULL-polarity edge is pending");
        assert!(pending[0].1.contains("plain record"));
    }

    #[test]
    fn test_contradiction_stored_polarity() {
        let store = CausalStore::open_in_memory().unwrap();
        let record = |outcome: &str, polarity: Option<&str>| {
            store
                .record_decision_full(
                    "用方案A部署",
                    outcome,
                    "caused",
                    Some("deploy"),
                    0.8,
                    "rule",
                    1000,
                    polarity,
                )
                .unwrap();
        };

        // stored negative old + stored positive new → old invalidated, even
        // though both outcome TEXTS look neutral to the heuristic.
        record("rollout done", Some("negative"));
        record("rollout done again", Some("positive"));
        let valid = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].outcome_text, "rollout done again");

        // stored mixed old + stored positive new → NOT invalidated (mixed
        // never triggers on either side), even though the old outcome text
        // contains a failure signal the heuristic would latch onto.
        record("deadlock occurred; fixed later", Some("mixed"));
        let valid = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(valid.len(), 2, "mixed old edge must survive");

        // stored positive old + stored negative new → NOT invalidated
        // (conservative: only negative-old + positive-new invalidates).
        record("再次失败", Some("negative"));
        let valid = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(
            valid.len(),
            3,
            "nothing invalidated: positive edge + mixed edge + the new negative edge itself"
        );
    }

    #[test]
    fn test_semantic_contradiction_stored_polarity() {
        let store = CausalStore::open_in_memory().unwrap();
        // Edge 1: text looks like failure, but stored 'mixed' — the stored
        // value must win and protect it from invalidation.
        store
            .record_decision_full(
                "用 Redis 加互斥锁",
                "死锁:持有者崩溃",
                "caused",
                None,
                0.8,
                "rule",
                1000,
                Some("mixed"),
            )
            .unwrap();
        // Edge 2: stored 'negative' with a neutral-looking text — stored wins,
        // so a positive new edge invalidates it.
        store
            .record_decision_full(
                "Redis mutex for stampede protection",
                "rollout finished",
                "caused",
                None,
                0.8,
                "rule",
                1001,
                Some("negative"),
            )
            .unwrap();
        store.put_embedding(1, "test", &[1.0, 0.0]).unwrap();
        store.put_embedding(2, "test", &[1.0, 0.0]).unwrap();

        let n = store
            .invalidate_semantic_contradictions(
                "used Redis with mutex lock",
                "rollout completed",
                Some("positive"),
                &[1.0, 0.0],
                0.85,
            )
            .unwrap();
        assert_eq!(n, 1, "only the stored-negative edge is invalidated");
        assert!(
            store.get_edge(1).unwrap().unwrap().valid_to.is_none(),
            "mixed → kept"
        );
        assert!(store.get_edge(2).unwrap().unwrap().valid_to.is_some());
    }

    // ── search_causal_bm25 ───────────────────────────────────────────────

    /// Three caching edges + one unrelated edge, all valid.
    fn bm25_store() -> CausalStore {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "cache stampede protection with Redis",
                "stampede stopped, hit ratio recovered",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "used Redis mutex lock",
                "deadlock under load",
                "caused",
                Some("caching"),
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "added cache TTL to Redis",
                "memory grew bounded again",
                "caused",
                Some("caching"),
                0.85,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "rewrote parser in rust",
                "build success",
                "caused",
                Some("compiler"),
                0.95,
                "rule",
            )
            .unwrap();
        store
    }

    #[test]
    fn test_bm25_beats_like_on_word_order() {
        // The LoCoMo failure case: LIKE on the full question string can never
        // match a doc whose words appear in a different order ("Redis cache
        // stampede" vs "cache stampede protection with Redis"); BM25 does.
        let store = bm25_store();
        assert!(store
            .search_causal(None, Some("Redis cache stampede"))
            .unwrap()
            .is_empty());
        let res = store
            .search_causal_bm25(None, "Redis cache stampede", 10)
            .unwrap();
        assert!(!res.is_empty());
        assert_eq!(
            res[0].decision_text, "cache stampede protection with Redis",
            "the 3-term doc must outrank the 2-term docs"
        );
        // The unrelated compiler edge must not appear.
        assert!(res.iter().all(|e| e.task_tag.as_deref() == Some("caching")));
    }

    #[test]
    fn test_bm25_task_tag_filter_scopes_idf() {
        let store = bm25_store();
        let res = store
            .search_causal_bm25(Some("compiler"), "redis cache stampede build", 10)
            .unwrap();
        assert_eq!(res.len(), 1, "task filter must exclude caching edges");
        assert_eq!(res[0].task_tag.as_deref(), Some("compiler"));
        // An unknown tag → empty candidate set → empty result, not an error.
        assert!(store
            .search_causal_bm25(Some("nope"), "redis", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_bm25_limit_and_score_order() {
        let store = bm25_store();
        let res = store.search_causal_bm25(None, "redis", 2).unwrap();
        assert_eq!(res.len(), 2, "limit truncates the ranked list");
        let full = store.search_causal_bm25(None, "redis", 10).unwrap();
        assert!(full.len() >= 2);
        assert_eq!(
            res.iter().map(|e| e.edge_id).collect::<Vec<_>>(),
            full[..2].iter().map(|e| e.edge_id).collect::<Vec<_>>(),
            "limit must keep the top of the same ranking"
        );
    }

    #[test]
    fn test_bm25_excludes_invalidated_and_tracks_access() {
        let store = bm25_store();
        let hit = store
            .search_causal_bm25(None, "cache stampede", 10)
            .unwrap();
        assert!(!hit.is_empty());
        let top_id = hit[0].edge_id;

        // record_access: every returned edge gets access_count + 1.
        let before = store.get_edge(top_id).unwrap().unwrap().access_count;
        store
            .search_causal_bm25(None, "cache stampede", 10)
            .unwrap();
        let after = store.get_edge(top_id).unwrap().unwrap();
        assert_eq!(after.access_count, before + 1);
        assert!(after.last_accessed_at.is_some());

        // Invalidated edges no longer participate in the index.
        store.invalidate_edge(top_id).unwrap();
        let res = store
            .search_causal_bm25(None, "cache stampede", 10)
            .unwrap();
        assert!(res.iter().all(|e| e.edge_id != top_id));
    }

    #[test]
    fn test_bm25_oov_and_empty_query_fallback() {
        let store = bm25_store();
        // All query terms out-of-vocabulary → empty (not an error).
        assert!(store
            .search_causal_bm25(None, "zzzxqqq", 10)
            .unwrap()
            .is_empty());
        // Empty / stop-words-only query → plain task_tag listing fallback.
        let res = store.search_causal_bm25(Some("caching"), "", 10).unwrap();
        assert_eq!(res.len(), 3);
        let res = store
            .search_causal_bm25(Some("caching"), "the a an", 2)
            .unwrap();
        assert_eq!(res.len(), 2, "fallback respects limit");
    }

    #[test]
    fn test_bm25_chinese_bigrams() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "用Redis做缓存防止缓存击穿",
                "缓存命中率恢复成功",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "重写数据库连接池",
                "连接耗尽错误消失",
                "caused",
                Some("db"),
                0.8,
                "rule",
            )
            .unwrap();
        let res = store.search_causal_bm25(None, "缓存击穿", 10).unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].decision_text.contains("缓存击穿"));
    }
}
