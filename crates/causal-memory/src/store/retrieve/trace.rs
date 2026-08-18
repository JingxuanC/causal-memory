//! Split from `retrieve.rs` — pure module split, no logic change.

use anyhow::{anyhow, Result};
use rusqlite::{params};


use crate::store::{CausalStore, ENTRY_COLUMNS, entry_from_row};

impl CausalStore {
    pub fn trace_cause(&self, outcome_description: &str) -> Result<Vec<crate::store::CausalEntry>> {
        let conn = self.acquire()?;
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
        self.record_access(entries.iter().map(|e| e.edge_id))?;
        Ok(entries)
    }

    /// Multi-hop causal trace: follow causal chains backward from an outcome.
    pub fn trace_cause_chain(
        &self,
        outcome_description: &str,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<crate::store::ChainHop>>> {
        let conn = self.acquire()?;
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

        let mut chains: Vec<Vec<crate::store::ChainHop>> = Vec::new();
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

                chain.push(crate::store::ChainHop {
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
        self.record_access(chains.iter().flatten().map(|hop| hop.edge_id))?;
        Ok(chains)
    }

    /// Forward multi-hop: start from a decision text match and walk downstream.
    pub fn trace_effect_chain(
        &self,
        decision_description: &str,
        max_depth: usize,
        min_confidence: f64,
    ) -> Result<Vec<Vec<crate::store::ChainHop>>> {
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
    ) -> Result<Vec<Vec<crate::store::ChainHop>>> {
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
    ) -> Result<Vec<Vec<crate::store::ChainHop>>> {
        let conn = self.acquire()?;
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

        let mut chains: Vec<Vec<crate::store::ChainHop>> = Vec::new();
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

                chain.push(crate::store::ChainHop {
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
        self.record_access(chains.iter().flatten().map(|hop| hop.edge_id))?;
        Ok(chains)
    }

    /// Get recent decisions for L0 directory (system prompt injection).
    pub fn trace_cause_cross_session(
        &self,
        outcome_description: &str,
        max_depth: usize,
        min_confidence: f64,
        max_meta_bridges: usize,
    ) -> Result<Vec<crate::store::CrossSessionChain>> {
        let seeds = self.trace_cause(outcome_description)?;
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let mut results: Vec<crate::store::CrossSessionChain> = Vec::new();
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
                let seg1 = crate::store::SessionSegment {
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
                            let seg2 = crate::store::SessionSegment {
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

                            let chain = crate::store::CrossSessionChain {
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
                            results.push(crate::store::CrossSessionChain {
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
    ) -> Result<Vec<Vec<crate::store::ChainHop>>> {
        let conn = self.acquire()?;

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

        let mut chains: Vec<Vec<crate::store::ChainHop>> = Vec::new();
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

                chain.push(crate::store::ChainHop {
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
        self.record_access(chains.iter().flatten().map(|hop| hop.edge_id))?;
        Ok(chains)
    }

    /// Find valid meta-causal edges connected to a decision chunk id.
    fn meta_bridges_from_decision(
        &self,
        decision_id: &str,
        min_confidence: f64,
    ) -> Result<Vec<crate::store::MetaEdge>> {
        let conn = self.acquire()?;
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
            Ok(crate::store::MetaEdge {
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

}
