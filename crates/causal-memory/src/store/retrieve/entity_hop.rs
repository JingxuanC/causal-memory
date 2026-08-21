//! Split from `retrieve.rs` — pure module split, no logic change.

use anyhow::{anyhow, Result};
use rusqlite::params;

use crate::store::{CausalStore, ENTRY_COLUMNS, entry_from_row};
use super::by_conf;

/// Lock the entity cache ignoring poisoning (cache writes can't panic, so a
/// poisoned guard only means some other thread panicked elsewhere).
fn poison_safe_lock<T>(
    m: &std::sync::Mutex<T>,
) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl CausalStore {
    /// Entity tokens for one edge, cache-or-compute (audit 2026-08 #2).
    /// Chunk texts are immutable, so the cache never goes stale; entries
    /// already loaded (e.g. by the semantic path) are tokenized exactly once
    /// and reused by every later query in the process.
    pub(crate) fn entity_tokens_for(
        &self,
        edge_id: i64,
        decision_text: &str,
        outcome_text: &str,
    ) -> std::sync::Arc<Vec<String>> {
        if let Some(hit) = poison_safe_lock(&self.entity_cache).get(&edge_id) {
            return hit.clone();
        }
        let mut ents = crate::patterns::entity_tokens(decision_text);
        ents.extend(crate::patterns::entity_tokens(outcome_text));
        let arc = std::sync::Arc::new(ents);
        poison_safe_lock(&self.entity_cache).insert(edge_id, arc.clone());
        arc
    }

    pub fn search_causal_entity(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::store::CausalEntry>> {
        let q_entities = crate::patterns::entity_tokens(query);
        if q_entities.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.acquire()?;
        // Score via the token cache: the candidate scan fetches only edge
        // ids (cheap index scan); texts are fetched just for edges not yet
        // cached — a warm query touches no chunk text at all. Ordering
        // contract preserved from the pre-cache implementation: overlap
        // desc, then edge id asc.
        let mut stmt = conn.prepare(
            "SELECT ce.id
             FROM causal_edges ce
             WHERE ce.valid_to IS NULL
             ORDER BY ce.id",
        )?;
        let all_ids: Vec<i64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;
        // Which ids need their texts (cache misses only)?
        let uncached: Vec<i64> = {
            let cache = poison_safe_lock(&self.entity_cache);
            all_ids.iter().copied().filter(|id| !cache.contains_key(id)).collect()
        };
        let mut texts: std::collections::HashMap<i64, (String, String)> =
            std::collections::HashMap::new();
        if !uncached.is_empty() {
            for chunk_ids in uncached.chunks(500) {
                let placeholders =
                    chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT ce.id, cf.text, ct.text
                     FROM causal_edges ce
                     JOIN chunks cf ON cf.id = ce.from_id
                     JOIN chunks ct ON ct.id = ce.to_id
                     WHERE ce.id IN ({placeholders})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let binds: Vec<&dyn rusqlite::ToSql> = chunk_ids
                    .iter()
                    .map(|id| id as &dyn rusqlite::ToSql)
                    .collect();
                let rows = stmt.query_map(binds.as_slice(), |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, dec, out) = row.map_err(|e| anyhow!("Query failed: {e}"))?;
                    texts.insert(id, (dec, out));
                }
            }
        }
        let mut scored: Vec<(usize, i64)> = Vec::new();
        for id in all_ids {
            let (dec_text, out_text) = match texts.get(&id) {
                Some(t) => (t.0.as_str(), t.1.as_str()),
                // Cache hit: tokens already computed, texts not needed.
                None => ("", ""),
            };
            let ents = self.entity_tokens_for(id, dec_text, out_text);
            let overlap = q_entities.iter().filter(|q| ents.contains(q)).count();
            if overlap > 0 {
                scored.push((overlap, id));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let top: Vec<i64> = scored.into_iter().take(limit).map(|(_, id)| id).collect();
        if top.is_empty() {
            return Ok(Vec::new());
        }
        // Hydrate only the output slice (limit is small; the full-table
        // entry fetch of the old implementation is the other half of the
        // per-query cost this cache removes).
        let placeholders = top.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.id IN ({placeholders})
             ORDER BY ce.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let ids: Vec<&dyn rusqlite::ToSql> =
            top.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(ids.as_slice(), entry_from_row)?;
        let mut by_id: std::collections::HashMap<i64, crate::store::CausalEntry> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?
            .into_iter()
            .map(|e| (e.edge_id, e))
            .collect();
        let entries: Vec<crate::store::CausalEntry> =
            top.into_iter().filter_map(|id| by_id.remove(&id)).collect();
        self.record_access(entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// Semantic + entity-boosted retrieval: cosine-rank like
    /// [`search_causal_semantic`], but edges sharing ≥1 query entity
    /// ([`crate::patterns::entity_tokens`]) get a multiplicative boost on
    /// their similarity — `score = sim × (1 + ENTITY_BOOST × overlap)`.
    ///
    /// The entity signal is a rank amplifier on top of a similarity backbone,
    /// never a standalone ranking: a bare entity list has no precision signal
    /// (every person-anchored edge ties at overlap 1) and its arbitrary
    /// ordering displaces lexical hits — measured regression on LoCoMo cat1
    /// when fused as a peer RRF list. A query with no entities gets boost
    /// 1.0, i.e. this degrades to plain semantic search.
    pub fn search_causal_hop(
        &self,
        query: &str,
        seed_edge_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<crate::store::CausalEntry>> {
        if seed_edge_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let q_tokens = crate::patterns::tokenize(query);
        let conn = self.acquire()?;

        let overlap = |entry: &crate::store::CausalEntry| -> usize {
            let toks = crate::patterns::tokenize(&format!(
                "{} {}",
                entry.decision_text, entry.outcome_text
            ));
            q_tokens.iter().filter(|t| toks.contains(t)).count()
        };

        // Fetch edges matching a built SQL with bound values.
        let run = |sql: String, binds: Vec<Box<dyn rusqlite::ToSql>>| -> Result<Vec<crate::store::CausalEntry>> {
            let mut stmt = conn.prepare(&sql)?;
            let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(bind_refs), entry_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow!("Query failed: {e}"))
        };

        // Seed endpoint chunks (deduped).
        let mut seed_chunks: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT from_id, to_id FROM causal_edges WHERE id = ?1 AND valid_to IS NULL",
            )?;
            for &sid in seed_edge_ids {
                let mut rows = stmt.query_map(params![sid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                while let Some((f, t)) = rows.next().transpose()? {
                    for c in [f, t] {
                        if !seed_chunks.contains(&c) {
                            seed_chunks.push(c);
                        }
                    }
                }
            }
        }
        if seed_chunks.is_empty() {
            return Ok(Vec::new());
        }

        let edge_ph = seed_edge_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let chunk_ph = seed_chunks.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        // 1-hop: valid edges sharing an endpoint chunk with a seed.
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = seed_edge_ids
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
            .collect();
        for c in seed_chunks.iter().chain(seed_chunks.iter()) {
            binds.push(Box::new(c.clone()));
        }
        let hop1 = run(
            format!(
                "SELECT {ENTRY_COLUMNS}
                 FROM causal_edges ce
                 JOIN chunks cf ON cf.id = ce.from_id
                 JOIN chunks ct ON ct.id = ce.to_id
                 WHERE ce.valid_to IS NULL
                   AND ce.id NOT IN ({edge_ph})
                   AND (ce.from_id IN ({chunk_ph}) OR ce.to_id IN ({chunk_ph}))"
            ),
            binds,
        )?;
        let mut hop1_chunks: Vec<String> = Vec::new();
        let mut hop1_ids: Vec<i64> = Vec::new();
        for e in &hop1 {
            hop1_ids.push(e.edge_id);
            for c in [&e.decision_id, &e.outcome_id] {
                if !hop1_chunks.contains(c) {
                    hop1_chunks.push(c.clone());
                }
            }
        }

        // 2-hop: distilled causal episodes touching 1-hop endpoints, gated by
        // shared query tokens (causal leaps are topically loose).
        let mut hop2: Vec<crate::store::CausalEntry> = Vec::new();
        if !hop1_chunks.is_empty() {
            let h1_chunk_ph = hop1_chunks.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let id_ph = seed_edge_ids
                .iter()
                .map(|_| "?")
                .chain(hop1_ids.iter().map(|_| "?"))
                .collect::<Vec<_>>()
                .join(",");
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = seed_edge_ids
                .iter()
                .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
                .collect();
            for id in &hop1_ids {
                binds.push(Box::new(*id));
            }
            for c in hop1_chunks.iter().chain(hop1_chunks.iter()) {
                binds.push(Box::new(c.clone()));
            }
            let candidates = run(
                format!(
                    "SELECT {ENTRY_COLUMNS}
                     FROM causal_edges ce
                     JOIN chunks cf ON cf.id = ce.from_id
                     JOIN chunks ct ON ct.id = ce.to_id
                     WHERE ce.valid_to IS NULL
                       AND ce.discovered_by = 'distill'
                       AND ce.id NOT IN ({id_ph})
                       AND (ce.from_id IN ({h1_chunk_ph}) OR ce.to_id IN ({h1_chunk_ph}))"
                ),
                binds,
            )?;
            hop2 = candidates.into_iter().filter(|e| overlap(e) > 0).collect();
        }

        // One ranked list: 1-hop first (decay by rank), each by query overlap
        // then confidence; truncate to the budget.
        let mut ranked: Vec<crate::store::CausalEntry> = Vec::with_capacity(hop1.len() + hop2.len());
        let mut scored: Vec<(usize, crate::store::CausalEntry)> =
            hop1.into_iter().map(|e| (overlap(&e), e)).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(by_conf(&a.1, &b.1)));
        ranked.extend(scored.into_iter().map(|(_, e)| e));
        let mut scored2: Vec<(usize, crate::store::CausalEntry)> =
            hop2.into_iter().map(|e| (overlap(&e), e)).collect();
        scored2.sort_by(|a, b| b.0.cmp(&a.0).then(by_conf(&a.1, &b.1)));
        ranked.extend(scored2.into_iter().map(|(_, e)| e));
        ranked.truncate(limit);
        self.record_access(ranked.iter().map(|e| e.edge_id))?;
        Ok(ranked)
    }

    /// Phase B (one-graph-convergence): valid edges with an endpoint in
    /// `chunk_ids` — materializes the display rows for chunk nodes the
    /// spreading-activation engine lit up. Ordered by confidence (edge id
    /// as tiebreaker); `task_tag` narrows like the single-layer searches.
    pub fn edges_touching_chunks(
        &self,
        chunk_ids: &[String],
        task_tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::store::CausalEntry>> {
        if chunk_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.acquire()?;

        let chunk_ph = vec!["?"; chunk_ids.len()].join(",");
        let mut sql = format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL
               AND (ce.from_id IN ({chunk_ph}) OR ce.to_id IN ({chunk_ph}))"
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = chunk_ids
            .iter()
            .chain(chunk_ids.iter())
            .map(|c| Box::new(c.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        if let Some(tag) = task_tag {
            sql.push_str(" AND ce.task_tag = ?");
            binds.push(Box::new(tag.to_string()));
        }
        sql.push_str(" ORDER BY ce.confidence DESC, ce.id LIMIT ?");
        binds.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), entry_from_row)?;
        let entries = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;
        self.record_access(entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }
}
