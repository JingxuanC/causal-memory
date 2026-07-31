//! Retrieval and tracing: search, BM25, semantic, multi-hop causal chains, meta edges.

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension};

use super::{CausalStore, ENTRY_COLUMNS, entry_from_row};

impl CausalStore {
    /// Persist an LLM re-judged confidence on all valid edges originating
    /// from a decision chunk. Returns the number of edges updated.
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
    pub fn get_edge(&self, edge_id: i64) -> Result<Option<super::CausalEntry>> {
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
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let seed_sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.id = ?1"
        );

        let mut seeds: Vec<super::CausalEntry> = Vec::new();
        let mut chunk_ids: Vec<String> = Vec::new();
        for &id in seed_edge_ids {
            if let Some(e) = conn
                .query_row(&seed_sql, params![id], entry_from_row)
                .optional()?
            {
                chunk_ids.push(e.decision_id.clone());
                chunk_ids.push(e.outcome_id.clone());
                seeds.push(e);
            }
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
    pub fn search_causal(
        &self,
        task_tag: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<super::CausalEntry>> {
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
    pub fn search_causal_bm25(
        &self,
        task_tag: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<super::CausalEntry>> {
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
        let candidates: Vec<super::CausalEntry> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;

        let index = crate::bm25::Bm25Index::build(candidates.iter().map(|e| {
            (
                e.edge_id.to_string(),
                crate::patterns::tokenize(&format!("{} {}", e.decision_text, e.outcome_text)),
            )
        }));
        let scored = index.search(&query_tokens, limit);

        let by_id: std::collections::HashMap<i64, super::CausalEntry> =
            candidates.into_iter().map(|e| (e.edge_id, e)).collect();
        let entries: Vec<super::CausalEntry> = scored
            .iter()
            .filter_map(|(key, _)| key.parse::<i64>().ok())
            .filter_map(|id| by_id.get(&id).cloned())
            .collect();
        Self::record_access(&conn, entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// Semantic search: cosine-rank `query_vec` against the embeddings of all
    /// valid edges, optionally filtered by task_tag. Returns the top `limit`
    /// entries with their similarity, descending. Access tracking is recorded.
    pub fn search_causal_semantic(
        &self,
        query_vec: &[f32],
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(super::CausalEntry, f64)>> {
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
            Ok((entry_from_row(row)?, row.get::<_, Vec<u8>>(15)?))
        })?;

        let mut scored: Vec<(super::CausalEntry, f64)> = Vec::new();
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
        Self::record_access(&conn, scored.iter().map(|(e, _)| e.edge_id))?;
        Ok(scored)
    }

    /// Semantic seed lookup for intervention queries: cosine-rank
    /// `query_embedding` against valid edge embeddings and keep only edges at
    /// or above `min_similarity`.
    pub fn similar_decision_edges(
        &self,
        query_embedding: &[f32],
        limit: usize,
        min_similarity: f64,
    ) -> Result<Vec<(super::CausalEntry, f64)>> {
        let mut scored = self.search_causal_semantic(query_embedding, None, limit)?;
        scored.retain(|(_, sim)| *sim >= min_similarity);
        Ok(scored)
    }

    /// Trace which decisions could have caused a given outcome (reverse lookup).
    pub fn trace_cause(&self, outcome_description: &str) -> Result<Vec<super::CausalEntry>> {
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
    pub fn trace_cause_chain(
        &self,
        outcome_description: &str,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<super::ChainHop>>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let pattern = format!("%{}%", outcome_description);

        let sql = r#"
            WITH RECURSIVE chain(node_id, path_json, depth, chain_confidence) AS (
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

        let mut chains: Vec<Vec<super::ChainHop>> = Vec::new();
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

                chain.push(super::ChainHop {
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

    /// Forward multi-hop: start from a decision text match and walk downstream.
    pub fn trace_effect_chain(
        &self,
        decision_description: &str,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<super::ChainHop>>> {
        let pattern = format!("%{}%", decision_description);
        self.trace_effect_chain_impl(
            "JOIN chunks c ON c.id = ce.from_id WHERE c.text LIKE ?1",
            &[Box::new(pattern)],
            max_depth,
            min_confidence,
        )
    }

    /// Forward multi-hop variant anchored on explicit decision chunk ids.
    pub fn trace_effect_chain_from_ids(
        &self,
        decision_ids: &[String],
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<super::ChainHop>>> {
        if decision_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (1..=decision_ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let anchor = format!("WHERE ce.from_id IN ({placeholders})");
        let binds: Vec<Box<dyn rusqlite::ToSql>> = decision_ids
            .iter()
            .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        self.trace_effect_chain_impl(&anchor, &binds, max_depth, min_confidence)
    }

    /// Shared forward-walk implementation.
    fn trace_effect_chain_impl(
        &self,
        anchor: &str,
        anchor_binds: &[Box<dyn rusqlite::ToSql>],
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<super::ChainHop>>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let conf_p = anchor_binds.len() + 1;
        let depth_p = anchor_binds.len() + 2;

        let sql = format!(
            r#"
            WITH RECURSIVE chain(node_id, path_json, depth, chain_confidence) AS (
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

        let mut chains: Vec<Vec<super::ChainHop>> = Vec::new();
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

                chain.push(super::ChainHop {
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

    /// Get recent decisions for L0 directory (system prompt injection).
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<super::DecisionDirectoryEntry>> {
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
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
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
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
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
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM causal_edges", [], |row| row.get(0))?;
        Ok(n)
    }

    // ─── Internal helpers ──────────────────────────────────────────────────

    /// Bump access counters for edges returned by a read-path query.
    fn record_access(conn: &rusqlite::Connection, edge_ids: impl Iterator<Item = i64>) -> Result<()> {
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
    pub fn trace_cause_cross_session(
        &self,
        outcome_description: &str,
        max_depth: usize,
        min_confidence: f64,
        max_meta_bridges: usize,
    ) -> Result<Vec<super::CrossSessionChain>> {
        let seeds = self.trace_cause(outcome_description)?;
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let mut results: Vec<super::CrossSessionChain> = Vec::new();
        let mut seen_keys: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for seed in seeds.iter().take(20) {
            // Session 1: backward chain from the seed's outcome within its session.
            let session1_chains = self.trace_cause_chain_session(
                &seed.outcome_id,
                seed.task_tag.as_deref(),
                max_depth,
                min_confidence,
            )?;

            for chain1 in session1_chains {
                let seg1 = super::SessionSegment {
                    task_tag: seed.task_tag.clone(),
                    hops: chain1,
                };

                // Try meta bridges from the root of this chain.
                let root_id = seg1.hops.last().map(|h| h.decision_id.clone());
                if let Some(ref root) = root_id {
                    let bridges =
                        self.meta_bridges_from_decision(root, min_confidence)?;

                    let mut bridged = false;
                    for bridge in bridges.iter().take(max_meta_bridges) {
                        let other_id = if bridge.from_id == *root {
                            &bridge.to_id
                        } else {
                            &bridge.from_id
                        };

                        // Skip same-session bridges.
                        let other_tag = self.task_tag_for_chunk(other_id)?;
                        if other_tag.as_deref() == seed.task_tag.as_deref() {
                            continue;
                        }
                        if other_tag.is_none() {
                            continue;
                        }
                        #[allow(clippy::unwrap_used, reason = "checked is_none above")]
                        let other_tag = other_tag.unwrap();

                        // Session 2: backward chain from the bridged decision.
                        let session2_chains = self.trace_cause_chain_session(
                            other_id,
                            Some(&other_tag),
                            max_depth,
                            min_confidence,
                        )?;

                        for chain2 in session2_chains {
                            let seg2 = super::SessionSegment {
                                task_tag: Some(other_tag.clone()),
                                hops: chain2,
                            };

                            let conf1 = seg1
                                .hops
                                .iter()
                                .map(|h| h.confidence)
                                .fold(1.0, |a, b| a * b);
                            let conf2 = seg2
                                .hops
                                .iter()
                                .map(|h| h.confidence)
                                .fold(1.0, |a, b| a * b);
                            let overall_conf = conf1 * bridge.confidence * conf2;

                            let chain = super::CrossSessionChain {
                                segments: vec![seg1.clone(), seg2],
                                overall_confidence: overall_conf,
                            };

                            let key = format!(
                                "{}|{}",
                                seg1.hops.first().map(|h| h.edge_id).unwrap_or(0),
                                chain.segments.get(1).and_then(|s| s.hops.first().map(|h| h.edge_id)).unwrap_or(0)
                            );
                            if seen_keys.insert(key) {
                                results.push(chain);
                                bridged = true;
                            }
                        }
                    }

                    // Keep single-session chains even when no bridge fires.
                    if !bridged {
                        let overall_conf = seg1
                            .hops
                            .iter()
                            .map(|h| h.confidence)
                            .fold(1.0, |a, b| a * b);
                        let key = format!(
                            "single|{}",
                            seg1.hops.first().map(|h| h.edge_id).unwrap_or(0)
                        );
                        if seen_keys.insert(key) {
                            results.push(super::CrossSessionChain {
                                segments: vec![seg1],
                                overall_confidence: overall_conf,
                            });
                        }
                    }
                }
            }
        }

        results.sort_by(|a, b| {
            b.overall_confidence
                .partial_cmp(&a.overall_confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(20);
        Ok(results)
    }

    /// Session-scoped backward causal chain from a specific outcome chunk id.
    fn trace_cause_chain_session(
        &self,
        outcome_chunk_id: &str,
        task_tag: Option<&str>,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<super::ChainHop>>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;

        let mut sql = r#"
            WITH RECURSIVE chain(node_id, path_json, depth, chain_confidence) AS (
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
                WHERE ce.to_id = ?1
                  AND ce.confidence >= ?2
                  AND ce.valid_to IS NULL
            "#
        .to_string();

        if task_tag.is_some() {
            sql.push_str(" AND ce.task_tag = ?3");
        }

        sql.push_str(
            r#"
                UNION ALL

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
                WHERE ch.depth < ?4
                  AND ce2.confidence >= ?2
                  AND ch.chain_confidence * ce2.confidence >= ?2
                  AND ce2.valid_to IS NULL
            "#,
        );

        if task_tag.is_some() {
            sql.push_str(" AND ce2.task_tag = ?3");
        }

        sql.push_str(
            r#"
            )
            SELECT path_json FROM chain
            WHERE depth >= 1
            ORDER BY depth DESC, chain_confidence DESC
            LIMIT 50
            "#,
        );

        let max_depth_i = max_depth as i64;
        let mut stmt = conn.prepare(&sql)?;
        let paths_json: Vec<String> = if let Some(tag) = task_tag {
            let rows = stmt.query_map(
                params![outcome_chunk_id, min_confidence, tag, max_depth_i],
                |row| {
                    let path_json: String = row.get(0)?;
                    Ok(path_json)
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow!("CTE query failed: {e}"))?
        } else {
            let rows = stmt.query_map(
                params![outcome_chunk_id, min_confidence, max_depth_i],
                |row| {
                    let path_json: String = row.get(0)?;
                    Ok(path_json)
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow!("CTE query failed: {e}"))?
        };

        let mut chains: Vec<Vec<super::ChainHop>> = Vec::new();
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

                chain.push(super::ChainHop {
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

    /// Find valid meta-causal edges connected to a decision chunk id.
    fn meta_bridges_from_decision(
        &self,
        decision_id: &str,
        min_confidence: f64,
    ) -> Result<Vec<super::MetaEdge>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let sql = r#"
            SELECT m.id, m.from_id, m.to_id, m.relation, m.pattern, m.confidence,
                   m.discovered_at, m.valid_to, cf.text, ct.text,
                   m.strata_count, m.strata, m.confounded, m.simpson
            FROM meta_causal_edges m
            JOIN chunks cf ON cf.id = m.from_id
            JOIN chunks ct ON ct.id = m.to_id
            WHERE m.valid_to IS NULL
              AND m.confidence >= ?1
              AND (m.from_id = ?2 OR m.to_id = ?2)
            ORDER BY m.confidence DESC
            LIMIT 20
        "#;
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![min_confidence, decision_id], |row| {
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

    /// Look up the task_tag for a chunk id (from the most recent causal edge).
    fn task_tag_for_chunk(&self, chunk_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
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
        let conn = self.conn.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let mut stmt =
            conn.prepare("SELECT id, text FROM chunks WHERE id LIKE ?1 ORDER BY id")?;
        let pattern = format!("{prefix}%");
        let rows = stmt.query_map(params![pattern], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
