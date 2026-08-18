//! Embedding management: storing vectors and backfill helpers.

use anyhow::{anyhow, Result};
use rusqlite::params;

use super::CausalStore;

impl CausalStore {
    /// Store/replace the embedding of an edge.
    pub fn put_embedding(&self, edge_id: i64, model: &str, vector: &[f32]) -> Result<()> {
        let conn = self.acquire()?;
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

    /// Valid edges that have no embedding yet (for CLI backfill).
    /// Returns (edge_id, "decision outcome") pairs.
    pub fn edges_without_embedding(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let conn = self.acquire()?;
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
        let conn = self.acquire()?;
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
        let conn = self.acquire()?;
        conn.execute(
            "UPDATE causal_edges SET outcome_polarity = ?1 WHERE id = ?2",
            params![polarity, edge_id],
        )?;
        Ok(())
    }
}
