//! Split from `retrieve.rs` — pure module split, no logic change.

use anyhow::{anyhow, Result};


use crate::store::{CausalStore, ENTRY_COLUMNS, entry_from_row};

impl CausalStore {
    pub fn search_causal_semantic(
        &self,
        query_vec: &[f32],
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(crate::store::CausalEntry, f64)>> {
        let conn = self.acquire()?;

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
            Ok((entry_from_row(row)?, row.get::<_, Vec<u8>>(16)?))
        })?;

        let mut scored: Vec<(crate::store::CausalEntry, f64)> = Vec::new();
        for row in rows {
            let (entry, blob) = row.map_err(|e| anyhow!("Query failed: {e}"))?;
            let Ok(vec) = crate::embed::blob_to_vec(&blob) else {
                continue;
            };
            let sim = crate::embed::cosine_similarity(query_vec, &vec);
            scored.push((entry, sim));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        self.record_access(scored.iter().map(|(e, _)| e.edge_id))?;
        Ok(scored)
    }

    /// Entity-anchored retrieval: rank valid edges by how many of the query's
    /// named entities ([`crate::patterns::entity_tokens`]) appear in the
    /// edge's endpoint chunk text. This is the multi-hop binder — a
    /// person-anchored question ("Where has Melanie camped?") surfaces every
    /// chunk mentioning Melanie across sessions, which single-pass lexical
    /// and semantic search miss when the evidence wording diverges.
    ///
    /// Returns edges with full chunk ids (entity hits count toward
    /// evidence-level metrics, unlike semantic hits which callers must map
    /// back). No-op (empty result) when the query carries no entities.
    pub fn search_causal_semantic_entity_boosted(
        &self,
        query_vec: &[f32],
        query: &str,
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(crate::store::CausalEntry, f64)>> {
        // CAUSAL_MEMORY_ENTITY_BOOST overrides the default 0.5 (param sweep harness).
        let entity_boost = std::env::var("CAUSAL_MEMORY_ENTITY_BOOST")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.5);
        let q_entities = crate::patterns::entity_tokens(query);
        let conn = self.acquire()?;

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
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_refs), |row| {
            Ok((entry_from_row(row)?, row.get::<_, Vec<u8>>(16)?))
        })?;
        let mut scored: Vec<(crate::store::CausalEntry, f64)> = Vec::new();
        for row in rows {
            let (entry, blob) = row.map_err(|e| anyhow!("Query failed: {e}"))?;
            let Ok(vec) = crate::embed::blob_to_vec(&blob) else {
                continue;
            };
            let sim = crate::embed::cosine_similarity(query_vec, &vec);
            let ents = self.entity_tokens_for(entry.edge_id, &entry.decision_text, &entry.outcome_text);
            let overlap = q_entities.iter().filter(|q| ents.contains(q)).count();
            scored.push((entry, sim * (1.0 + entity_boost * overlap as f64)));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        self.record_access(scored.iter().map(|(e, _)| e.edge_id))?;
        Ok(scored)
    }

    /// Hop expansion for multi-hop retrieval (A2): starting from seed edges,
    /// pull 1-hop neighbors (any valid edge sharing an endpoint chunk — turn
    /// adjacency and causal links) then 2-hop neighbors reached only through
    /// distilled causal episodes (decision→outcome jumps). Seeds excluded.
    ///
    /// Returns ONE ranked list: 1-hop before 2-hop (hop decay by rank — RRF
    /// consumes ranks, not scores), each ordered by shared-token overlap with
    /// the query (desc) then confidence (desc), capped at `limit`. The 2-hop
    /// set is precision-gated: a distilled jump must share ≥1 query token,
    /// because causal leaps are topically loose; 1-hop adjacency is anchored
    /// to the seeds and is not gated.
    pub fn similar_decision_edges(
        &self,
        query_embedding: &[f32],
        limit: usize,
        min_similarity: f64,
    ) -> Result<Vec<(crate::store::CausalEntry, f64)>> {
        let mut scored = self.search_causal_semantic(query_embedding, None, limit)?;
        scored.retain(|(_, sim)| *sim >= min_similarity);
        Ok(scored)
    }

}
