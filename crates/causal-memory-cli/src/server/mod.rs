//! MCP server handler — exposes 13 tools for unified agent memory.
//!
//! Tools:
//! - record_decision: agent calls after completing an action, to log
//!   the decision and its outcome as a causal edge.
//! - search_causal: agent calls BEFORE a non-trivial decision, to check
//!   past lessons in the same task domain.
//! - record_fact: agent calls to record a flat fact (preference / tech
//!   stack / config); idempotent, optional same-key retirement.
//! - search_facts: agent calls to retrieve flat facts (semantic/BM25/list).
//! - search_memory: unified retrieval — facts + causal lessons fused by
//!   Reciprocal Rank Fusion (RRF) in one call.
//! - trace_cause: agent calls when something fails, to find which past
//!   decision could have caused it (single-hop reverse lookup).
//! - trace_cause_chain: agent calls for deep failure analysis, to follow
//!   multi-hop causal chains backward through the decision graph.
//! - invalidate_decision: agent/user calls to soft-invalidate a wrong lesson
//!   (sets valid_to; the edge stays in the DB for audit).
//! - search_patterns: agent calls to query mined cross-task patterns
//!   (meta-causal edges: similar_to / repeated / contradicts / refines).
//! - causal_directory: L0 compact directory of recent decisions, intended
//!   to be pinned in the agent system prompt (insights/13 §1.2).
//! - intervention_query: Pearl Rung-2 intervention — agent calls BEFORE
//!   acting, to predict what similar past actions caused (forward multi-hop).
//! - counterfactual_query: contrastive (empirical) counterfactual — agent
//!   calls when choosing between two concrete options, to compare recorded
//!   outcomes of each (NOT a Pearl Rung-3 SCM counterfactual).
//! - reconstruct_lesson: reconstructive retrieval — agent calls to get the
//!   distilled lesson of a past episode as an LLM narrative over the causal
//!   subgraph, instead of raw records.


use causal_memory::hippocampus::CausalGraph;
use causal_memory::store::CausalStore;
use std::sync::Mutex;

pub struct CausalMemoryServer {
    store: CausalStore,
    graph: Mutex<Option<CausalGraph>>,
}

impl CausalMemoryServer {
    pub fn new(store: CausalStore) -> Self {
        // Load the hippocampus graph from the store on startup.
        let graph = CausalGraph::from_store(&store).ok();
        Self {
            store,
            graph: Mutex::new(graph),
        }
    }

    /// Reload the hippocampus graph from the store (after new data is written).
    fn reload_graph(&self) {
        if let Ok(g) = CausalGraph::from_store(&self.store) {
            if let Ok(mut guard) = self.graph.lock() {
                *guard = Some(g);
            }
        }
    }

    /// Try spreading activation search on the hippocampus graph.
    /// Returns None if graph is empty, missing, or finds nothing.
    fn hippocampus_search(
        &self,
        query: &str,
        task_tag: Option<&str>,
        reverse: bool,
        limit: usize,
    ) -> Option<String> {
        let mut guard = self.graph.lock().ok()?;
        let graph = guard.as_mut()?;
        if graph.num_nodes() == 0 {
            return None;
        }

        let results = graph.spreading_activation(query, task_tag, reverse);
        if results.is_empty() {
            return None;
        }

        let count = results.len().min(limit);
        let direction = if reverse { "reverse" } else { "forward" };
        let mut out = format!(
            "[hippocampus/{direction}] Activated {}/{} nodes via spreading activation:\n\n",
            count,
            results.len()
        );
        for (i, r) in results.iter().take(limit).enumerate() {
            let sign = if r.activation > 0.0 { "+" } else { "-" };
            out.push_str(&format!(
                "{}. [{:.0}%{}] \"{}\"\n",
                i + 1,
                r.activation.abs() * 100.0,
                sign,
                truncate_chars(&r.text, 80),
            ));
        }
        Some(out)
    }
}

/// Run an async embed call from a sync tool handler.
/// The MCP server runs inside a multi-thread tokio runtime (see main.rs), so
/// bridge with block_in_place; fall back to a throwaway runtime when no
/// runtime context exists (defensive — not expected in production).
#[allow(
    clippy::expect_used,
    reason = "fallback runtime; failure is unrecoverable"
)]
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime")
            .block_on(fut),
    }
}

/// Cosine floor for semantic seeding in intervention_query (recall-oriented).
pub(crate) const INTERVENTION_MIN_SIMILARITY: f64 = 0.5;
/// Cosine floor for the semantic contradiction scan on record (precision-
/// oriented: only paraphrase-level duplicates of the same decision).
pub(crate) const SEMANTIC_CONTRADICTION_MIN_SIMILARITY: f64 = 0.85;

/// Reciprocal Rank Fusion constant (the RRF paper's standard value).
pub(crate) const RRF_K: f64 = 60.0;

/// P5: Layered loading — format a causal entry at L0/L1/L2 detail.
pub mod format;
pub mod output;
pub mod tools;

#[cfg(test)]
mod tests;

// Stable public paths for cross-module consumers (bench_tokens, tests).
pub(crate) use format::{format_entry_layered, rrf_fuse, truncate_chars};

