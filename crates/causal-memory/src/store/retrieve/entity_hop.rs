//! Split from `retrieve.rs` — pure module split, no logic change.

use anyhow::{anyhow, Result};
use rusqlite::params;

use crate::store::{CausalStore, ENTRY_COLUMNS, entry_from_row};
use super::by_conf;

impl CausalStore {
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
        let mut stmt = conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS}
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL
             ORDER BY ce.id"
        ))?;
        let rows = stmt.query_map([], entry_from_row)?;
        let candidates: Vec<crate::store::CausalEntry> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow!("Query failed: {e}"))?;
        let mut scored: Vec<(usize, crate::store::CausalEntry)> = Vec::new();
        for entry in candidates {
            let mut ents = crate::patterns::entity_tokens(&entry.decision_text);
            ents.extend(crate::patterns::entity_tokens(&entry.outcome_text));
            let overlap = q_entities.iter().filter(|q| ents.contains(q)).count();
            if overlap > 0 {
                scored.push((overlap, entry));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.edge_id.cmp(&b.1.edge_id)));
        let entries: Vec<crate::store::CausalEntry> =
            scored.into_iter().take(limit).map(|(_, e)| e).collect();
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

}
