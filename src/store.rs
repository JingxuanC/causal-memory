//! Causal store — the core data layer.
//!
//! Ported from spike/grok-causal-memory/src/lib.rs, adapted for async MCP use.
//! SQLite operations are sync; we use tokio::task::spawn_blocking to avoid
//! blocking the async runtime.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use rusqlite::{params, Connection};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// SQL schema for causal tables. Stays compatible with grok-build's
/// existing chunks table (additive, not replacing).
pub const CAUSAL_SCHEMA_SQL: &str = r#"
-- Minimal chunks table (host agent writes decisions/outcomes as chunks)
CREATE TABLE IF NOT EXISTS chunks (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Causal edges: decision → outcome
CREATE TABLE IF NOT EXISTS causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK(relation IN ('caused','enabled','prevented','no_effect')),
    confidence REAL NOT NULL DEFAULT 0.5,
    discovered_by TEXT NOT NULL DEFAULT 'llm_inferred',
    discovered_at INTEGER NOT NULL,
    task_tag TEXT,
    FOREIGN KEY (from_id) REFERENCES chunks(id),
    FOREIGN KEY (to_id) REFERENCES chunks(id)
);
CREATE INDEX IF NOT EXISTS idx_causal_from ON causal_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_causal_to ON causal_edges(to_id);
CREATE INDEX IF NOT EXISTS idx_causal_task ON causal_edges(task_tag);

-- Meta causal edges: decision → decision (cross-task patterns)
CREATE TABLE IF NOT EXISTS meta_causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK(relation IN ('similar_to','repeated','contradicts','refines')),
    pattern TEXT,
    confidence REAL NOT NULL DEFAULT 0.5
);
CREATE INDEX IF NOT EXISTS idx_meta_from ON meta_causal_edges(from_id);
"#;

/// Thread-safe causal store backed by SQLite.
#[derive(Clone)]
pub struct CausalStore {
    conn: Arc<Mutex<Connection>>,
}

/// A causal retrieval result returned to the agent.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CausalEntry {
    pub decision_id: String,
    pub decision_text: String,
    pub outcome_id: String,
    pub outcome_text: String,
    pub relation: String,
    pub confidence: f64,
    pub task_tag: Option<String>,
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

impl CausalStore {
    /// Open an in-memory store (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(CAUSAL_SCHEMA_SQL)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open a file-backed store.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(CAUSAL_SCHEMA_SQL)?;
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
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dec_id = format!("d{}{}", now, seq);
        let out_id = format!("o{}{}", now, seq);

        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&dec_id, decision, now],
        )?;
        conn.execute(
            "INSERT INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
            params![&out_id, outcome, now],
        )?;
        conn.execute(
            "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, discovered_at, task_tag)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![&dec_id, &out_id, relation, confidence, discovered_by, now, task_tag],
        )?;
        Ok(dec_id)
    }

    /// Search past causal episodes by task tag and/or text similarity.
    /// Returns entries ordered by confidence descending.
    pub fn search_causal(&self, task_tag: Option<&str>, query: Option<&str>) -> Result<Vec<CausalEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;

        let sql = match (task_tag, query) {
            (Some(tag), Some(q)) => format!(
                "SELECT cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence, ce.task_tag
                 FROM causal_edges ce
                 JOIN chunks cf ON cf.id = ce.from_id
                 JOIN chunks ct ON ct.id = ce.to_id
                 WHERE ce.task_tag = '{}' AND (cf.text LIKE '%{}%' OR ct.text LIKE '%{}%')
                 ORDER BY ce.confidence DESC",
                tag.replace('\'', "''"),
                q.replace('\'', "''"),
                q.replace('\'', "''")
            ),
            (Some(tag), None) => format!(
                "SELECT cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence, ce.task_tag
                 FROM causal_edges ce
                 JOIN chunks cf ON cf.id = ce.from_id
                 JOIN chunks ct ON ct.id = ce.to_id
                 WHERE ce.task_tag = '{}'
                 ORDER BY ce.confidence DESC",
                tag.replace('\'', "''")
            ),
            (None, Some(q)) => format!(
                "SELECT cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence, ce.task_tag
                 FROM causal_edges ce
                 JOIN chunks cf ON cf.id = ce.from_id
                 JOIN chunks ct ON ct.id = ce.to_id
                 WHERE cf.text LIKE '%{}%' OR ct.text LIKE '%{}%'
                 ORDER BY ce.confidence DESC",
                q.replace('\'', "''"),
                q.replace('\'', "''")
            ),
            (None, None) => format!(
                "SELECT cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence, ce.task_tag
                 FROM causal_edges ce
                 JOIN chunks cf ON cf.id = ce.from_id
                 JOIN chunks ct ON ct.id = ce.to_id
                 ORDER BY ce.confidence DESC
                 LIMIT 20"
            ),
        };

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(CausalEntry {
                decision_id: row.get(0)?,
                decision_text: row.get(1)?,
                outcome_id: row.get(2)?,
                outcome_text: row.get(3)?,
                relation: row.get(4)?,
                confidence: row.get(5)?,
                task_tag: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Trace which decisions could have caused a given outcome (reverse lookup).
    pub fn trace_cause(&self, outcome_description: &str) -> Result<Vec<CausalEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let safe = outcome_description.replace('\'', "''");
        let mut stmt = conn.prepare(
            "SELECT cf.id, cf.text, ct.id, ct.text, ce.relation, ce.confidence, ce.task_tag
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ct.text LIKE ?
             ORDER BY ce.confidence DESC"
        )?;
        let rows = stmt.query_map(params![format!("%{safe}%")], |row| {
            Ok(CausalEntry {
                decision_id: row.get(0)?,
                decision_text: row.get(1)?,
                outcome_id: row.get(2)?,
                outcome_text: row.get(3)?,
                relation: row.get(4)?,
                confidence: row.get(5)?,
                task_tag: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Get recent decisions for L0 directory (system prompt injection).
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<DecisionDirectoryEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT cf.id, ce.task_tag, cf.text, ct.text, ce.relation
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             ORDER BY ce.discovered_at DESC
             LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let dec_text: String = row.get(2)?;
            let out_text: String = row.get(3)?;
            Ok(DecisionDirectoryEntry {
                id: row.get(0)?,
                task_tag: row.get(1)?,
                decision_snippet: if dec_text.len() > 80 { format!("{}...", &dec_text[..80]) } else { dec_text },
                outcome_snippet: if out_text.len() > 80 { format!("{}...", &out_text[..80]) } else { out_text },
                relation: row.get(4)?,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_search() {
        let store = CausalStore::open_in_memory().unwrap();
        store.record_decision(
            "used Redis with mutex lock",
            "deadlock — holder crashed without releasing",
            "caused",
            Some("concurrency"),
            0.85,
            "llm_inferred",
        ).unwrap();
        store.record_decision(
            "switched to channel/single-flight",
            "successfully fixed race condition",
            "caused",
            Some("concurrency"),
            0.95,
            "user_feedback",
        ).unwrap();

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
}
