//! Retrieval and tracing: search, BM25, semantic, entity-anchored, hop
//! expansion, multi-hop causal chains, meta edges, and RRF fusion.
//!
//! Module layout: [`bm25`] keyword retrieval · [`semantic`] cosine +
//! entity-boosted · [`entity_hop`] entity anchoring and graph hops ·
//! [`trace`] multi-hop causal chains · [`fusion`] shared RRF merge.

pub mod bm25;
pub mod entity_hop;
pub mod fusion;
pub mod semantic;
pub mod trace;

// Stable public path for the shared fusion (harnesses and servers import it).
pub use fusion::rrf_merge_many;

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension};

use super::{CausalStore, ENTRY_COLUMNS, entry_from_row, CausalEntry};

impl CausalStore {
    pub fn rejudge_decision(
        &self,
        from_id: &str,
        confidence: f64,
        discovered_by: &str,
    ) -> Result<usize> {
        let conn = self.acquire()?;
        let n = conn.execute(
            "UPDATE causal_edges SET confidence = ?1, discovered_by = ?2
             WHERE from_id = ?3 AND valid_to IS NULL",
            params![confidence.clamp(0.0, 1.0), discovered_by, from_id],
        )?;
        Ok(n)
    }

    /// Fetch a single edge by id, including its invalidation status and audit
    /// fields. Unlike the read paths, this does NOT filter on valid_to.
    pub fn get_edge(&self, edge_id: i64) -> Result<Option<super::CausalEntry>> {
        let conn = self.acquire()?;
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

    /// Fetch many edges in one query (C4: replaces per-chain get_edge loops,
    /// e.g. intervention_query's chain_stratum aggregation).
    pub fn get_edges_batch(&self, edge_ids: &[i64]) -> Result<Vec<super::CausalEntry>> {
        if edge_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.acquire()?;
        let ph = vec!["?"; edge_ids.len()].join(",");
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.id IN ({ph})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(edge_ids.iter()), entry_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Markov blanket subgraph around seed edges: the seeds themselves plus
    /// every valid edge sharing a `from_id` or `to_id` chunk with a seed
    /// (parents, children, and co-parents). Seeds come first (in input
    /// order), neighbors follow by confidence descending; the total is
    /// capped at `max_edges`. Used by reconstruct_lesson to bound the
    /// subgraph handed to the LLM.
    pub fn markov_blanket(
        &self,
        seed_edge_ids: &[i64],
        max_edges: usize,
    ) -> Result<Vec<super::CausalEntry>> {
        let conn = self.acquire()?;
        if seed_edge_ids.is_empty() {
            return Ok(Vec::new());
        }
        // C5: one IN query for all seeds instead of one query per seed.
        let seed_ph = vec!["?"; seed_edge_ids.len()].join(",");
        let seed_sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.id IN ({seed_ph})"
        );
        let mut seed_stmt = conn.prepare(&seed_sql)?;
        let seed_rows = seed_stmt.query_map(
            rusqlite::params_from_iter(seed_edge_ids.iter()),
            entry_from_row,
        )?;

        let mut seeds: Vec<super::CausalEntry> = Vec::new();
        let mut chunk_ids: Vec<String> = Vec::new();
        for e in seed_rows {
            let e = e?;
            chunk_ids.push(e.decision_id.clone());
            chunk_ids.push(e.outcome_id.clone());
            seeds.push(e);
        }
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        chunk_ids.sort();
        chunk_ids.dedup();

        let seed_ph = vec!["?"; seeds.len()].join(",");
        let chunk_ph = vec!["?"; chunk_ids.len()].join(",");
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL
               AND ce.id NOT IN ({seed_ph})
               AND (ce.from_id IN ({chunk_ph}) OR ce.to_id IN ({chunk_ph}))
             ORDER BY ce.confidence DESC"
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for e in &seeds {
            bind.push(Box::new(e.edge_id));
        }
        for c in &chunk_ids {
            bind.push(Box::new(c.clone()));
        }
        for c in &chunk_ids {
            bind.push(Box::new(c.clone()));
        }
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_refs.as_slice(), entry_from_row)?;
        let neighbors = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;

        let mut out = seeds;
        out.extend(neighbors);
        out.truncate(max_edges);
        Ok(out)
    }

    /// Search past causal episodes by task tag and/or text similarity.
    /// Returns entries ordered by confidence descending.
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<super::DecisionDirectoryEntry>> {
        let conn = self.acquire()?;
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
            Ok(super::DecisionDirectoryEntry {
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

    /// Get top decisions by confidence (high-value lessons first).
    pub fn top_decisions_by_confidence(
        &self,
        limit: usize,
    ) -> Result<Vec<super::DecisionDirectoryEntry>> {
        let conn = self.acquire()?;
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
            Ok(super::DecisionDirectoryEntry {
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
    pub fn all_valid_edges(&self) -> Result<Vec<super::CausalEntry>> {
        let conn = self.acquire()?;
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
    pub fn upsert_meta_edge(
        &self,
        from_id: &str,
        to_id: &str,
        relation: &str,
        pattern: &str,
        confidence: f64,
    ) -> Result<i64> {
        self.upsert_meta_edge_stratified(
            from_id, to_id, relation, pattern, confidence, None, None, None,
        )
    }

    /// `upsert_meta_edge` plus the v5 stratified-replication results.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_meta_edge_stratified(
        &self,
        from_id: &str,
        to_id: &str,
        relation: &str,
        pattern: &str,
        confidence: f64,
        strata: Option<&[String]>,
        confounded: Option<bool>,
        simpson: Option<bool>,
    ) -> Result<i64> {
        let conn = self.acquire()?;
        let now = chrono::Utc::now().timestamp();
        let strata_json = strata
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| anyhow!("strata encode: {e}"))?;
        let strata_count = strata.map(|s| s.len() as i64);
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
                     SET confidence = ?1, pattern = ?2, discovered_at = ?3,
                         strata_count = ?4, strata = ?5, confounded = ?6, simpson = ?7
                     WHERE id = ?8",
                    params![
                        confidence,
                        pattern,
                        now,
                        strata_count,
                        strata_json,
                        confounded,
                        simpson,
                        id
                    ],
                )?;
                Ok(id)
            }
            None => {
                conn.execute(
                    "INSERT INTO meta_causal_edges
                         (from_id, to_id, relation, pattern, confidence, discovered_at, valid_from,
                          strata_count, strata, confounded, simpson)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        from_id,
                        to_id,
                        relation,
                        pattern,
                        confidence,
                        now,
                        now,
                        strata_count,
                        strata_json,
                        confounded,
                        simpson
                    ],
                )?;
                Ok(conn.last_insert_rowid())
            }
        }
    }

    /// Search mined cross-task patterns (meta-causal edges).
    pub fn search_patterns(
        &self,
        query: Option<&str>,
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<super::MetaEdge>> {
        let conn = self.acquire()?;
        let mut sql = String::from(
            "SELECT m.id, m.from_id, m.to_id, m.relation, m.pattern, m.confidence,
                    m.discovered_at, m.valid_to, cf.text, ct.text,
                    m.strata_count, m.strata, m.confounded, m.simpson
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
            Ok(super::MetaEdge {
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
                strata_count: row.get(10)?,
                strata: row.get(11)?,
                confounded: row.get(12)?,
                simpson: row.get(13)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))
    }

    /// Count causal edges (for diagnostics).
    pub fn count_edges(&self) -> Result<i64> {
        let conn = self.acquire()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM causal_edges", [], |row| row.get(0))?;
        Ok(n)
    }

    // ─── Internal helpers ──────────────────────────────────────────────────

    /// Bump access counters for edges returned by a read-path query.
    ///
    /// Architecture hardening A4: this no longer writes to the DB per
    /// query. Counts accumulate in an in-memory buffer and are flushed in
    /// bulk by the next connection checkout (CausalStore::acquire), so a
    /// search no longer costs a write transaction.
    fn record_access(&self, edge_ids: impl Iterator<Item = i64>) -> Result<()> {
        let mut buf = self
            .access_buffer
            .lock()
            .map_err(|e| anyhow!("access buffer lock: {e}"))?;
        // Same semantics as the old immediate-UPDATE path: dedup within a
        // single call (an edge appearing in several chains counts once),
        // and each buffered id bumps the counter exactly once at flush.
        for id in edge_ids {
            buf.insert(id);
        }
        Ok(())
    }

    /// Flush buffered access counts to the DB (one UPDATE per edge).
    /// Idempotent; a no-op when the buffer is empty. Failures are
    /// swallowed — access tracking must never block a memory operation.
    pub(crate) fn flush_access_buffer(&self, conn: &rusqlite::Connection) {
        let mut buf = match self.access_buffer.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if buf.is_empty() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        for &id in buf.iter() {
            let _ = conn.execute(
                "UPDATE causal_edges SET access_count = access_count + 1, last_accessed_at = ?1 WHERE id = ?2",
                rusqlite::params![now, id],
            );
        }
        buf.clear();
    }

    fn resolve_chunk_pair(
        conn: &rusqlite::Connection,
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

    // ─── Cross-session causal tracing ──────────────────────────────────────

    /// Cross-session causal tracing: find causal explanations that span multiple
    /// task sessions by using meta-causal edges (pattern-miner bridges) to jump
    /// between sessions.
    ///
    /// Algorithm:
    /// 1. Find seeds via `trace_cause`.
    /// 2. For each seed, run a backward causal chain within its own session.
    /// 3. For the root decision of each chain, look up meta-causal edges that
    ///    connect to decisions in *other* sessions.
    /// 4. For each meta-edge bridge, run another backward chain from the bridged
    ///    decision in its session.
    /// 5. Combine segments into `CrossSessionChain` results.
    fn task_tag_for_chunk(&self, chunk_id: &str) -> Result<Option<String>> {
        let conn = self.acquire()?;
        let tag: Option<String> = conn
            .query_row(
                "SELECT task_tag FROM causal_edges
                 WHERE (from_id = ?1 OR to_id = ?1) AND valid_to IS NULL
                 ORDER BY event_time DESC LIMIT 1",
                params![chunk_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(tag)
    }

    /// Fetch all chunks whose id starts with `prefix`, ordered by id (which
    /// sorts by session then turn). Used by P8 session expansion: given a
    /// session prefix like `{question_id}::{session_id}::`, pull every turn
    /// in that session so the answerer gets full context, not just the 2
    /// turns BM25 happened to hit.
    pub fn chunks_by_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let conn = self.acquire()?;
        let mut stmt =
            conn.prepare("SELECT id, text FROM chunks WHERE id LIKE ?1 ORDER BY id")?;
        let pattern = format!("{prefix}%");
        let rows = stmt.query_map(params![pattern], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Order two entries by confidence, descending (higher confidence first).
fn by_conf(a: &CausalEntry, b: &CausalEntry) -> std::cmp::Ordering {
    b.confidence
        .partial_cmp(&a.confidence)
        .unwrap_or(std::cmp::Ordering::Equal)
}
