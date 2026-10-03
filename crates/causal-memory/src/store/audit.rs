//! Recall audit (v13): one best-effort row per recall, powering
//! `/debug/recalls`. Audit writes MUST NEVER break retrieval — insert
//! failures surface as a metrics counter + a warn log only.

use anyhow::Result;
use rusqlite::params;
use std::sync::atomic::{AtomicU64, Ordering};

use super::CausalStore;

/// One recall audit record (JSON fields are pre-encoded by the caller).
pub struct RecallAuditRow<'a> {
    pub query: &'a str,
    pub task_tag: Option<&'a str>,
    pub server: &'a str,
    pub mode: &'a str,
    pub seeds_json: &'a str,
    pub activated_nodes: usize,
    pub max_hop: u8,
    pub results_json: &'a str,
    pub latency_ms: f64,
    pub result_count: usize,
}

/// One audit row as read back by `recent_recall_audits` (JSON fields stay
/// encoded — the debug endpoint passes them through).
#[derive(Debug, serde::Serialize)]
pub struct RecallAuditEntry {
    pub id: i64,
    pub created_at: i64,
    pub query: String,
    pub task_tag: Option<String>,
    pub server: String,
    pub mode: String,
    pub seeds: serde_json::Value,
    pub activated_nodes: i64,
    pub max_hop: i64,
    pub results: serde_json::Value,
    pub latency_ms: f64,
    pub result_count: i64,
}

/// Retention: on every 100th insert, drop rows older than 30 days or
/// beyond the newest 10k. Amortized so the read path stays clean.
const RETENTION_EVERY: u64 = 100;
const RETENTION_MAX_AGE_SECS: i64 = 30 * 24 * 3600;
const RETENTION_MAX_ROWS: i64 = 10_000;

static INSERTS_SINCE_SWEEP: AtomicU64 = AtomicU64::new(0);

impl CausalStore {
    /// Best-effort audit insert. Errors are returned to the caller (which
    /// logs + counts them); nothing here panics or blocks retrieval.
    pub fn insert_recall_audit(&self, row: &RecallAuditRow<'_>) -> Result<()> {
        let conn = self.acquire()?;
        conn.execute(
            "INSERT INTO recall_audit
             (created_at, query, task_tag, server, mode, seeds_json,
              activated_nodes, max_hop, results_json, latency_ms, result_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                chrono::Utc::now().timestamp(),
                row.query,
                row.task_tag,
                row.server,
                row.mode,
                row.seeds_json,
                row.activated_nodes as i64,
                i64::from(row.max_hop),
                row.results_json,
                row.latency_ms,
                row.result_count as i64,
            ],
        )?;
        if INSERTS_SINCE_SWEEP.fetch_add(1, Ordering::Relaxed) + 1 >= RETENTION_EVERY {
            INSERTS_SINCE_SWEEP.store(0, Ordering::Relaxed);
            self.sweep_recall_audit()?;
        }
        Ok(())
    }

    /// Retention sweep: delete rows older than 30 days or beyond the
    /// newest RETENTION_MAX_ROWS.
    fn sweep_recall_audit(&self) -> Result<()> {
        let conn = self.acquire()?;
        let cutoff = chrono::Utc::now().timestamp() - RETENTION_MAX_AGE_SECS;
        conn.execute(
            "DELETE FROM recall_audit WHERE created_at < ?1",
            params![cutoff],
        )?;
        conn.execute(
            "DELETE FROM recall_audit WHERE id NOT IN
             (SELECT id FROM recall_audit ORDER BY id DESC LIMIT ?1)",
            params![RETENTION_MAX_ROWS],
        )?;
        Ok(())
    }

    /// Newest-first audit rows for the /debug/recalls endpoint.
    pub fn recent_recall_audits(&self, limit: usize) -> Result<Vec<RecallAuditEntry>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT id, created_at, query, task_tag, server, mode, seeds_json,
                    activated_nodes, max_hop, results_json, latency_ms, result_count
             FROM recall_audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let seeds_raw: String = r.get(6)?;
            let results_raw: String = r.get(9)?;
            Ok(RecallAuditEntry {
                id: r.get(0)?,
                created_at: r.get(1)?,
                query: r.get(2)?,
                task_tag: r.get(3)?,
                server: r.get(4)?,
                mode: r.get(5)?,
                seeds: serde_json::from_str(&seeds_raw).unwrap_or(serde_json::Value::Null),
                activated_nodes: r.get(7)?,
                max_hop: r.get(8)?,
                results: serde_json::from_str(&results_raw).unwrap_or(serde_json::Value::Null),
                latency_ms: r.get(10)?,
                result_count: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
