//! Split from `retrieve.rs` — pure module split, no logic change.

use anyhow::{anyhow, Result};


use crate::store::{CausalStore, ENTRY_COLUMNS, entry_from_row};

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

}
