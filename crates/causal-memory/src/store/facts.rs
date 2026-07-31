//! Agent-fact layer (v6, unified-memory-design Phase 1).

use anyhow::{anyhow, Result};
use rusqlite::params;

use super::{CausalStore, SUPERSEDES_MIN_SHARED_TOKENS, SUPERSEDES_SIM_THRESHOLD};

impl CausalStore {
    /// Record a flat fact ("user prefers TypeScript"). Idempotent on
    /// (key, value, scope): re-recording an existing valid fact refreshes
    /// `updated_at` and `confidence`; re-recording a previously invalidated
    /// fact revives it (valid_to back to NULL — the fact is true again).
    /// Returns the fact id (new or existing).
    pub fn record_fact(
        &self,
        key: &str,
        value: &str,
        scope: &str,
        source: &str,
        confidence: f64,
    ) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO agent_facts (key, value, scope, source, confidence, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(key, value, scope) DO UPDATE SET
                 updated_at = excluded.updated_at,
                 confidence = excluded.confidence,
                 source = excluded.source,
                 valid_to = NULL",
            params![key, value, scope, source, confidence.clamp(0.0, 1.0), now],
        )?;
        let id = conn.query_row(
            "SELECT id FROM agent_facts WHERE key = ?1 AND value = ?2 AND scope = ?3",
            params![key, value, scope],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Soft-invalidate a fact: set valid_to = now. Returns true if a row was
    /// actually invalidated; false if missing or already invalid (no-op).
    pub fn invalidate_fact(&self, fact_id: i64) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let n = conn.execute(
            "UPDATE agent_facts SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![now, fact_id],
        )?;
        Ok(n > 0)
    }

    /// Retire valid facts under (key, scope) whose value matches a supersedes
    /// hint. Fact-layer port of the edge layer's supersedes machinery
    /// (record_distilled), with its guards:
    /// - thresholds: containment ≥ SUPERSEDES_SIM_THRESHOLD AND ≥
    ///   SUPERSEDES_MIN_SHARED_TOKENS shared tokens, computed on DEDUPLICATED
    ///   token sets
    /// - retraction records are never retirement TARGETS (a fact whose text
    ///   announces a retraction, e.g. "no longer likes X", must not be killed
    ///   by a later hint sharing that vocabulary — double-negation
    ///   resurrection)
    ///
    /// Returns the number retired.
    pub fn retire_facts_by_hint(&self, key: &str, scope: &str, hint: &str) -> Result<usize> {
        let hint_tokens: std::collections::HashSet<String> =
            crate::patterns::tokenize(hint).into_iter().collect();
        if hint_tokens.len() < SUPERSEDES_MIN_SHARED_TOKENS {
            return Ok(0);
        }
        let candidates = self.search_facts_bm25(hint, Some(scope), 10)?;
        let mut retired = 0;
        for fact in candidates {
            if fact.key != key {
                continue;
            }
            // Guard: retraction records are never targets (edge-layer parity).
            let lower = fact.value.to_lowercase();
            if super::RETRACTION_MARKERS.iter().any(|m| lower.contains(m)) {
                continue;
            }
            let cand_tokens: std::collections::HashSet<String> =
                crate::patterns::tokenize(&fact.value).into_iter().collect();
            let shared = hint_tokens.intersection(&cand_tokens).count();
            if shared < SUPERSEDES_MIN_SHARED_TOKENS {
                continue;
            }
            let denom = hint_tokens.len().min(cand_tokens.len());
            if denom > 0
                && shared as f64 / denom as f64 >= SUPERSEDES_SIM_THRESHOLD
                && self.invalidate_fact(fact.id)?
            {
                retired += 1;
            }
        }
        Ok(retired)
    }

    /// Record a fact AND retire conflicting values under the same
    /// (key, scope) atomically — one lock, one write batch. The
    /// "user switched to pnpm" flow: callers get the new fact id plus the
    /// number of outdated facts retired, with no window where old and new
    /// values are both valid.
    pub fn record_fact_replacing(
        &self,
        key: &str,
        value: &str,
        scope: &str,
        source: &str,
        confidence: f64,
    ) -> Result<(i64, usize)> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO agent_facts (key, value, scope, source, confidence, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(key, value, scope) DO UPDATE SET
                 updated_at = excluded.updated_at,
                 confidence = excluded.confidence,
                 source = excluded.source,
                 valid_to = NULL",
            params![key, value, scope, source, confidence.clamp(0.0, 1.0), now],
        )?;
        let id = conn.query_row(
            "SELECT id FROM agent_facts WHERE key = ?1 AND value = ?2 AND scope = ?3",
            params![key, value, scope],
            |r| r.get(0),
        )?;
        let retired = conn.execute(
            "UPDATE agent_facts SET valid_to = ?1
             WHERE key = ?2 AND scope = ?3 AND value != ?4 AND valid_to IS NULL",
            params![now, key, scope, value],
        )?;
        Ok((id, retired))
    }

    /// Retire conflicting values for the same (key, scope): soft-invalidate
    /// every valid fact under this key whose value differs from
    /// `keep_value`. The "user switched to pnpm" path — record the new fact
    /// first, then call this to retire the old value in the same write flow.
    /// Returns the number of facts invalidated.
    pub fn invalidate_other_facts_for_key(
        &self,
        key: &str,
        scope: &str,
        keep_value: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let n = conn.execute(
            "UPDATE agent_facts SET valid_to = ?1
             WHERE key = ?2 AND scope = ?3 AND value != ?4 AND valid_to IS NULL",
            params![now, key, scope, keep_value],
        )?;
        Ok(n)
    }

    /// List valid facts, optionally filtered by scope, newest first.
    pub fn list_facts(&self, scope: Option<&str>, limit: usize) -> Result<Vec<super::AgentFact>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut sql = String::from(
            "SELECT id, key, value, scope, source, confidence, created_at, updated_at
             FROM agent_facts WHERE valid_to IS NULL",
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = scope {
            sql.push_str(" AND scope = ?");
            bind.push(Box::new(s.to_string()));
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ?");
        bind.push(Box::new(limit as i64));
        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), super::fact_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// BM25 search over valid facts (tokens from "key value"), optional scope
    /// filter. Same ranking discipline as search_causal_bm25: token overlap,
    /// not substring, so phrasing differences don't zero out hits. An empty
    /// query degrades to `list_facts`.
    pub fn search_facts_bm25(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<super::AgentFact>> {
        let query_tokens = crate::patterns::tokenize(query);
        if query_tokens.is_empty() {
            return self.list_facts(scope, limit);
        }
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut sql = String::from(
            "SELECT id, key, value, scope, source, confidence, created_at, updated_at
             FROM agent_facts WHERE valid_to IS NULL",
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = scope {
            sql.push_str(" AND scope = ?");
            bind.push(Box::new(s.to_string()));
        }
        sql.push_str(" ORDER BY id");
        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), super::fact_from_row)?;
        let candidates: Vec<super::AgentFact> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        let index = crate::bm25::Bm25Index::build(candidates.iter().map(|f| {
            (
                f.id.to_string(),
                crate::patterns::tokenize(&f.search_text()),
            )
        }));
        let scored = index.search(&query_tokens, limit);
        let by_id: std::collections::HashMap<i64, super::AgentFact> =
            candidates.into_iter().map(|f| (f.id, f)).collect();
        Ok(scored
            .iter()
            .filter_map(|(key, _)| key.parse::<i64>().ok())
            .filter_map(|id| by_id.get(&id).cloned())
            .collect())
    }

    /// Store/replace the embedding of a fact (mirrors put_embedding for edges).
    pub fn put_fact_embedding(&self, fact_id: i64, model: &str, vector: &[f32]) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        conn.execute(
            "INSERT INTO agent_facts_embeddings (fact_id, model, vector, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(fact_id) DO UPDATE SET
                 model = excluded.model,
                 vector = excluded.vector,
                 created_at = excluded.created_at",
            params![
                fact_id,
                model,
                crate::embed::vec_to_blob(vector),
                chrono::Utc::now().timestamp()
            ],
        )?;
        // Track which model produced the stored embedding (version management).
        conn.execute(
            "UPDATE agent_facts SET embedding_model = ?2 WHERE id = ?1",
            params![fact_id, model],
        )?;
        Ok(())
    }

    /// Semantic fact search: cosine-rank `query_vec` against embeddings of
    /// valid facts, optional scope filter. Brute-force scan — fact counts are
    /// in the hundreds-to-thousands range, same argument as edge embeddings.
    pub fn search_facts_semantic(
        &self,
        query_vec: &[f32],
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(super::AgentFact, f64)>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut sql = String::from(
            "SELECT f.id, f.key, f.value, f.scope, f.source, f.confidence,
                    f.created_at, f.updated_at, e.vector
             FROM agent_facts f
             JOIN agent_facts_embeddings e ON e.fact_id = f.id
             WHERE f.valid_to IS NULL",
        );
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = scope {
            sql.push_str(" AND f.scope = ?");
            bind.push(Box::new(s.to_string()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), |r| {
            Ok((super::fact_from_row(r)?, r.get::<_, Vec<u8>>(8)?))
        })?;
        let mut scored: Vec<(super::AgentFact, f64)> = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(fact, blob)| {
                let vec = crate::embed::blob_to_vec(&blob).ok()?;
                let sim = crate::embed::cosine_similarity(query_vec, &vec);
                Some((fact, sim))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }
}
