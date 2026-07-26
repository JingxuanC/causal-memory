//! Causal chain linker — connects flat decisions into multi-hop chains.
//!
//! The problem: extractors produce flat edges (decision → outcome), but
//! real debugging is chain-shaped:
//!   test_failed → edit_fix → test_passed → deploy → deploy_failed
//!
//! Each adjacent pair (outcome_i → decision_j) is a potential causal link.
//! This module detects those links and creates bridge edges so that
//! trace_cause_chain can walk multi-hop paths.
//!
//! ## Three linking strategies (from cheap to expensive)
//!
//! 1. **Temporal** (free): if decision_j happened right after outcome_i,
//!    and outcome_i looks like a failure, create a bridge with low confidence.
//! 2. **Text overlap** (free): if outcome_i and decision_j share keywords,
//!    create a bridge with medium confidence.
//! 3. **LLM** (costs $): ask LLM "did decision B happen because of outcome A?"
//!
//! All three write the same kind of bridge edge:
//!   from_id = outcome_i's chunk, to_id = decision_j's chunk
//!   relation = "caused" (the outcome caused / triggered the next decision)
//!
//! This is the missing piece that makes trace_cause_chain actually find
//! multi-hop chains instead of stopping at 1 hop.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

use crate::store::CausalStore;

#[derive(Debug, Default, Clone)]
pub struct LinkStats {
    pub edges_scanned: usize,
    pub temporal_links: usize,
    pub text_links: usize,
    pub llm_links: usize,
    pub bridge_edges_created: usize,
    pub skipped_self: usize,
}

/// One flat edge loaded from the store, enriched with text for matching.
#[derive(Debug, Clone)]
struct FlatEdge {
    edge_id: i64,
    from_id: String,
    to_id: String,
    dec_text: String,
    out_text: String,
    task_tag: Option<String>,
    confidence: f64,
    event_time: i64,
}

#[derive(Debug, Deserialize)]
struct ChatEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default)]
    tool_calls: Vec<ToolCallEntry>,
}

#[derive(Debug, Deserialize)]
struct ToolCallEntry {
    id: String,
    name: String,
    arguments: String,
}

pub struct ChainLinker;

impl ChainLinker {
    /// Link flat decisions into multi-hop chains.
    ///
    /// Scans existing causal_edges, finds outcome→decision connections,
    /// and creates bridge edges. After this, trace_cause_chain will
    /// find multi-hop paths.
    pub fn link_chains(store: &CausalStore) -> Result<LinkStats> {
        let edges = Self::load_all_edges(store)?;
        let mut stats = LinkStats {
            edges_scanned: edges.len(),
            ..Default::default()
        };

        // Strategy 1 + 2 combined: for each pair (i, j) where i happened
        // before j, check temporal + text overlap
        for (i, edge_i) in edges.iter().enumerate() {
            for edge_j in edges.iter().skip(i + 1) {
                // Skip self-loops
                if edge_i.to_id == edge_j.from_id {
                    stats.skipped_self += 1;
                    continue;
                }

                // Check if outcome_i and decision_j are related
                let (link_type, confidence) = Self::check_link(edge_i, edge_j);

                if confidence < 0.3 {
                    continue;
                }

                // Create bridge edge: outcome_i chunk → decision_j chunk
                // This connects the chain: ...→outcome_i→decision_j→...
                let bridge_confidence = confidence;
                let source = match link_type {
                    "temporal" => {
                        stats.temporal_links += 1;
                        "temporal"
                    }
                    "text" => {
                        stats.text_links += 1;
                        "rule"
                    }
                    _ => "temporal",
                };

                // Check if bridge already exists (avoid duplicates)
                if Self::bridge_exists(store, &edge_i.to_id, &edge_j.from_id)? {
                    continue;
                }

                // Create the bridge edge directly in causal_edges
                // from_id = outcome chunk (edge_i.to_id)
                // to_id = decision chunk (edge_j.from_id)
                // relation = "caused" (outcome caused the next decision)
                match Self::create_bridge_edge(
                    store,
                    &edge_i.to_id,
                    &edge_j.from_id,
                    bridge_confidence,
                    source,
                    &format!("chain-link:{}", link_type),
                ) {
                    Ok(_) => stats.bridge_edges_created += 1,
                    Err(e) => tracing::debug!("Bridge creation failed: {e}"),
                }
            }
        }

        Ok(stats)
    }

    /// Check if two edges should be linked.
    /// Returns (link_type, confidence).
    fn check_link(edge_i: &FlatEdge, edge_j: &FlatEdge) -> (&'static str, f64) {
        // i must happen before j (temporal ordering)
        if edge_i.event_time >= edge_j.event_time {
            return ("none", 0.0);
        }

        // Time gap: closer = higher confidence
        let gap = edge_j.event_time - edge_i.event_time;
        let temporal_proximity = if gap < 60 {
            0.8
        } else if gap < 300 {
            0.6
        } else if gap < 600 {
            0.4
        } else {
            0.2
        };

        // Strategy 1: Temporal + failure — failure outcome triggers next decision (strong signal)
        let out_is_failure = Self::looks_like_failure(&edge_i.out_text);
        if out_is_failure && temporal_proximity >= 0.4 {
            return ("temporal", temporal_proximity * 0.7); // 0.28-0.56
        }

        // Strategy 2: Text overlap — outcome_i and decision_j share keywords
        let overlap = Self::text_overlap(&edge_i.out_text, &edge_j.dec_text);
        if overlap >= 2 && temporal_proximity >= 0.4 {
            let conf = (overlap as f64 * 0.15) * temporal_proximity;
            return ("text", conf.min(0.7));
        }

        // Strategy 3: Pure temporal proximity — adjacent actions are likely related
        // (lower confidence than failure-triggered, but enables multi-hop chains
        // through sequences of successful operations like build → test → deploy)
        if temporal_proximity >= 0.6 && gap < 120 {
            return ("temporal_adjacent", temporal_proximity * 0.3); // 0.18-0.24
        }

        ("none", 0.0)
    }

    /// Count keyword overlap between two texts.
    fn text_overlap(a: &str, b: &str) -> usize {
        let a_lower = a.to_lowercase();
        let b_lower = b.to_lowercase();

        // Extract meaningful tokens (>= 4 chars, alphanumeric)
        let a_tokens: std::collections::HashSet<&str> = a_lower
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
            .filter(|t| t.len() >= 4)
            .collect();

        let b_tokens: std::collections::HashSet<&str> = b_lower
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
            .filter(|t| t.len() >= 4)
            .collect();

        a_tokens.intersection(&b_tokens).count()
    }

    fn looks_like_failure(text: &str) -> bool {
        let lower = text.to_lowercase();
        lower.contains("error")
            || lower.contains("failed")
            || lower.contains("panic")
            || lower.contains("denied")
            || lower.contains("not found")
            || lower.contains("fatal")
    }

    fn load_all_edges(store: &CausalStore) -> Result<Vec<FlatEdge>> {
        store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ce.rowid, ce.from_id, ce.to_id, cf.text, ct.text,
                        ce.task_tag, ce.confidence, ce.event_time
                 FROM causal_edges ce
                 JOIN chunks cf ON cf.id = ce.from_id
                 JOIN chunks ct ON ct.id = ce.to_id
                 WHERE ce.valid_to IS NULL
                 ORDER BY ce.event_time ASC",
            )?;

            let rows = stmt.query_map([], |row| {
                Ok(FlatEdge {
                    edge_id: row.get(0)?,
                    from_id: row.get(1)?,
                    to_id: row.get(2)?,
                    dec_text: row.get(3)?,
                    out_text: row.get(4)?,
                    task_tag: row.get(5)?,
                    confidence: row.get(6)?,
                    event_time: row.get(7)?,
                })
            })?;

            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| anyhow::anyhow!("Query failed: {e}"))
        })
    }

    fn bridge_exists(store: &CausalStore, from_id: &str, to_id: &str) -> Result<bool> {
        store.with_conn(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE from_id = ?1 AND to_id = ?2",
                rusqlite::params![from_id, to_id],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
    }

    fn create_bridge_edge(
        store: &CausalStore,
        from_id: &str,
        to_id: &str,
        confidence: f64,
        source: &str,
        task_tag: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        store.with_conn(|conn| {
            conn.execute(
                "INSERT INTO causal_edges
                    (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                 VALUES (?1, ?2, 'caused', ?3, ?4, ?5, ?5, ?6)",
                rusqlite::params![from_id, to_id, confidence, source, now, task_tag],
            )?;
            Ok(())
        })
    }
}
