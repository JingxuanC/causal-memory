//! Split from `retrieve.rs` — pure module split, no logic change.

use anyhow::{anyhow, Result};

use crate::store::{entry_from_row, CausalStore, ENTRY_COLUMNS};

impl CausalStore {
    pub fn search_causal(
        &self,
        task_tag: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<crate::store::CausalEntry>> {
        let conn = self.acquire()?;

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
        self.record_access(entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// BM25 keyword retrieval over valid edges (`valid_to IS NULL`).
    pub fn search_causal_bm25(
        &self,
        task_tag: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::store::CausalEntry>> {
        let query_tokens = crate::patterns::tokenize(query);
        if query_tokens.is_empty() {
            let mut entries = self.search_causal(task_tag, None)?;
            entries.truncate(limit);
            return Ok(entries);
        }

        let conn = self.acquire()?;

        // B2: resolve candidate chunk ids from the persistent inverted index.
        // Scored ranks stay identical to the old full-table path — the index
        // only narrows the candidate set. Empty result (or a missing index,
        // e.g. a pre-v10 DB that somehow skipped migration) falls back to
        // the full scan so retrieval never silently loses results.
        let chunk_ph = vec!["?"; query_tokens.len()].join(",");
        let mut chunk_stmt = conn.prepare(&format!(
            "SELECT DISTINCT chunk_id FROM bm25_index
             WHERE chunk_id NOT LIKE 'fact:%' AND token IN ({chunk_ph})"
        ))?;
        let chunk_rows = chunk_stmt
            .query_map(rusqlite::params_from_iter(query_tokens.iter()), |r| {
                r.get::<_, String>(0)
            })?;
        let chunk_ids: Vec<String> = chunk_rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("index query failed: {e}"))?;

        // B2 recall guard: the persistent index only covers chunks written
        // through the store API. Harnesses that insert chunks directly
        // (locomo turn ingest) leave them unindexed, so a sparse candidate
        // set would silently miss their evidence. When the index yields
        // fewer than a few candidates, fall back to the full scan — the
        // same result set the pre-index code produced.
        //
        // Upper bound (index-size guard): the index lookup is NOT scoped
        // by task_tag (that filter applies to causal_edges below), so on a
        // large shared store a few common tokens match tens of thousands
        // of chunk ids — the `IN (...)` list would exceed SQLite's host
        // variable limit and fail to prepare. Oversized candidate sets are
        // also useless as a narrowing step; both ends fall back to the
        // full scan, which the task_tag filter already bounds.
        const MAX_INDEX_CANDIDATES: usize = 900; // < SQLite's 999-variable floor
        let use_index = (3..=MAX_INDEX_CANDIDATES).contains(&chunk_ids.len());
        let mut sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL"
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if use_index {
            let ph = vec!["?"; chunk_ids.len()].join(",");
            sql.push_str(&format!(
                " AND (ce.from_id IN ({ph}) OR ce.to_id IN ({ph}))"
            ));
            for cid in chunk_ids.iter().chain(chunk_ids.iter()) {
                bind.push(Box::new(cid.clone()));
            }
        }
        if let Some(tag) = task_tag {
            sql.push_str(" AND ce.task_tag = ?");
            bind.push(Box::new(tag.to_string()));
        }
        sql.push_str(" ORDER BY ce.id");

        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), entry_from_row)?;
        let candidates: Vec<crate::store::CausalEntry> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;

        let index = crate::bm25::Bm25Index::build(candidates.iter().map(|e| {
            (
                e.edge_id.to_string(),
                crate::patterns::tokenize(&format!("{} {}", e.decision_text, e.outcome_text)),
            )
        }));
        let scored = index.search(&query_tokens, limit);

        let by_id: std::collections::HashMap<i64, crate::store::CausalEntry> =
            candidates.into_iter().map(|e| (e.edge_id, e)).collect();
        let entries: Vec<crate::store::CausalEntry> = scored
            .iter()
            .filter_map(|(key, _)| key.parse::<i64>().ok())
            .filter_map(|id| by_id.get(&id).cloned())
            .collect();
        self.record_access(entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// Phase B (one-graph-convergence): unified seed resolver for the
    /// spreading-activation engine. One query, ALL node types: returns
    /// `fact:{id}` and chunk ids ranked by distinct shared tokens — the
    /// same persistent index both single-layer BM25 paths narrow against,
    /// but without their namespace filters. `scope`, when set, drops fact
    /// seeds outside that scope (chunk seeds are scope-free). The dual-pool
    /// searches keep their `LIKE 'fact:%'` / `NOT LIKE` split; this is the
    /// one place that deliberately ignores it.
    pub fn bm25_seed_ids(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        let query_tokens = crate::patterns::tokenize(query);
        if query_tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.acquire()?;

        let token_ph = vec!["?"; query_tokens.len()].join(",");
        // Validity always applies to fact rows; the scope filter binds
        // only when set (same conditional-SQL discipline as the other
        // retrieval queries in this module).
        //
        // The fact-validity LEFT JOIN must key on af.id = CAST(substr(...)),
        // NOT ('fact:' || af.id) = chunk_id: the expression form rewrites
        // the probe side and forces a full agent_facts scan PER bm25 row —
        // measured 3 minutes per query on the 21.6M-row LongMemEval index
        // (the unified engine's seed resolver was effectively dead in
        // production; the harness paths never touch it, which is why this
        // survived every bench). substr after 'fact:' keeps the same
        // semantics: non-fact rows don't match, invalid facts drop out.
        let mut sql = format!(
            "SELECT b.chunk_id, COUNT(DISTINCT b.token) AS overlap
             FROM bm25_index b
             LEFT JOIN agent_facts af
               ON b.chunk_id LIKE 'fact:%'
              AND af.id = CAST(substr(b.chunk_id, 6) AS INTEGER)
             WHERE b.token IN ({token_ph})
               AND (af.id IS NULL OR af.valid_to IS NULL)"
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = query_tokens
            .iter()
            .map(|t| Box::new(t.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        if let Some(s) = scope {
            sql.push_str(" AND (af.id IS NULL OR af.scope = ?)");
            binds.push(Box::new(s.to_string()));
        }
        sql.push_str(" GROUP BY b.chunk_id ORDER BY overlap DESC, b.chunk_id LIMIT ?");
        binds.push(Box::new(limit as i64));
        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_refs.as_slice(), |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("seed query failed: {e}"))
    }
}
