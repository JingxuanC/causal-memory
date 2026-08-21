//! High-level memory facade — the 15 memory operations shared by every
//! frontend (MCP server, Python bindings, …).
//!
//! The orchestration logic (write-time polarity judging, opportunistic
//! embedding, semantic contradiction scan, hippocampus spreading activation,
//! RRF fusion, stratified intervention summaries, distill ingest) lives here
//! in the library; frontends only parse parameters and format/transport the
//! resulting text. Methods return the same human/agent-readable strings the
//! MCP tools produce — agent frameworks consume these directly as tool
//! outputs.

use crate::hippocampus::{CausalGraph, NodeData, Relation};
use crate::store::CausalStore;
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;

use format::truncate_chars;

/// C7: rebuild the hippocampus graph only after enough writes accumulated
/// (or enough time passed) — a full from_store per write is O(store) and
/// dominates the write path as the store grows. The graph is a retrieval
/// accelerator: briefly serving the previous version while writes batch is
/// fine (keyword/semantic paths below still see the fresh store).
const GRAPH_REBUILD_WRITES: usize = 5;
const GRAPH_REBUILD_SECS: i64 = 30;

/// Cosine floor for semantic seeding in intervention_query (recall-oriented).
pub(crate) const INTERVENTION_MIN_SIMILARITY: f64 = 0.5;
/// Cosine floor for the semantic contradiction scan on record (precision-
/// oriented: only paraphrase-level duplicates of the same decision).
pub(crate) const SEMANTIC_CONTRADICTION_MIN_SIMILARITY: f64 = 0.85;

/// Reciprocal Rank Fusion constant (the RRF paper's standard value).
pub(crate) const RRF_K: f64 = 60.0;

pub mod format;
pub mod ops;
mod unified;
pub mod output;

#[cfg(test)]
mod tests;

/// The unified agent memory: one `CausalStore` plus a lazily-rebuilt
/// hippocampus graph accelerator and the Hebbian co-occurrence buffer.
///
/// All 15 operations are methods on this struct (see `ops`). Frontends:
/// the MCP server (`causal-memory-cli::server`) and the Python bindings
/// (`causal-memory-py`).
pub struct Memory {
    pub(crate) store: CausalStore,
    graph: Mutex<Option<CausalGraph>>,
    /// Pending writes since the last rebuild (monotonic counter).
    graph_writes: AtomicUsize,
    /// Unix ts of the last rebuild.
    graph_last_rebuild: AtomicI64,
    /// D1: co-activated chunk pairs buffered by retrieval, flushed to the
    /// cooccurrence_edges table when the graph rebuilds. Keeps Hebbian
    /// learning off the read path (batched, low-frequency writes).
    cooc_buffer: Mutex<Vec<(String, String)>>,
}

impl Memory {
    /// Wrap an existing store.
    pub fn new(store: CausalStore) -> Self {
        // Load the hippocampus graph from the store on startup.
        let graph = CausalGraph::from_store(&store).ok();
        Self {
            store,
            graph: Mutex::new(graph),
            graph_writes: AtomicUsize::new(0),
            graph_last_rebuild: AtomicI64::new(chrono::Utc::now().timestamp()),
            cooc_buffer: Mutex::new(Vec::new()),
        }
    }

    /// Open (or create) a memory database at `path`, running migrations.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::new(CausalStore::open(path)?))
    }

    /// An in-memory memory — for tests and ephemeral use.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self::new(CausalStore::open_in_memory()?))
    }

    /// Access the underlying store (escape hatch for frontends that need
    /// raw queries; prefer the high-level ops).
    pub fn store(&self) -> &CausalStore {
        &self.store
    }

    /// Mark the in-memory graph as stale after a write. Cheap; the actual
    /// rebuild happens lazily in maybe_rebuild_graph on the next
    /// hippocampus query.
    fn mark_graph_dirty(&self) {
        self.graph_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// Phase C (one-graph-convergence): patch a freshly recorded causal
    /// edge into the live graph — append the endpoint nodes (no-op on
    /// chunk reuse), add the edge as an overlay patch. The new lesson is
    /// visible to the very next query without waiting for the lazy
    /// rebuild; `mark_graph_dirty` still runs so the periodic full rebuild
    /// bounds drift.
    fn patch_graph_new_edge(&self, entry: &crate::store::CausalEntry) {
        if let Ok(mut guard) = self.graph.lock() {
            if let Some(graph) = guard.as_mut() {
                let node = |id: &str, text: &str, task_tag: Option<String>| NodeData {
                    id: id.to_string(),
                    text: text.to_string(),
                    event_time: entry.event_time,
                    q_value: 0.5,
                    replay_count: 0,
                    last_activated: 0,
                    task_tag,
                    scope: None,
                };
                let from = graph.append_node(node(
                    &entry.decision_id,
                    &entry.decision_text,
                    entry.task_tag.clone(),
                ));
                let to = graph.append_node(node(&entry.outcome_id, &entry.outcome_text, None));
                graph.add_patch_edge(
                    from,
                    to,
                    Relation::from_str_lossy(&entry.relation),
                    entry.confidence as f32,
                );
            }
        }
    }

    /// Phase C: patch a freshly recorded fact into the live graph —
    /// scope hub + fact node + scope→fact edge, then entity-link the fact
    /// against the chunk nodes (same thresholds as the rebuild-time
    /// linker).
    fn patch_graph_new_fact(
        &self,
        fact_id: i64,
        key: &str,
        value: &str,
        scope: &str,
        confidence: f64,
    ) {
        if let Ok(mut guard) = self.graph.lock() {
            if let Some(graph) = guard.as_mut() {
                let scope_idx = graph.append_node(NodeData {
                    id: format!("scope:{scope}"),
                    text: format!("[{scope} scope]"),
                    event_time: 0,
                    q_value: 0.5,
                    replay_count: 0,
                    last_activated: 0,
                    task_tag: None,
                    scope: Some(scope.to_string()),
                });
                let fact_idx = graph.append_node(NodeData {
                    id: format!("fact:{fact_id}"),
                    text: format!("{key}: {value}"),
                    event_time: 0,
                    q_value: confidence as f32,
                    replay_count: 0,
                    last_activated: 0,
                    task_tag: Some(key.to_string()),
                    scope: Some(scope.to_string()),
                });
                graph.add_patch_edge(scope_idx, fact_idx, Relation::Fact, confidence as f32);
                graph.link_fact_node(fact_idx);
            }
        }
    }

    /// Phase C: retire graph nodes for facts superseded by `new_fact_id`
    /// (the store sets `superseded_by` on replace). Retired fact nodes
    /// neither seed nor surface until the next full rebuild drops them.
    fn patch_graph_retire_facts(&self, new_fact_id: i64) {
        let superseded: Vec<i64> = self
            .store
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT id FROM agent_facts WHERE superseded_by = ?1")?;
                let rows = stmt.query_map(rusqlite::params![new_fact_id], |r| r.get(0))?;
                let ids: std::result::Result<Vec<i64>, rusqlite::Error> = rows.collect();
                Ok(ids?)
            })
            .unwrap_or_default();
        if let Ok(mut guard) = self.graph.lock() {
            if let Some(graph) = guard.as_mut() {
                for id in superseded {
                    graph.retire_node(&format!("fact:{id}"));
                }
            }
        }
    }

    /// Rebuild the graph when enough writes have accumulated or enough time
    /// passed. Called at the top of every hippocampus search.
    fn maybe_rebuild_graph(&self) {
        let writes = self.graph_writes.load(Ordering::Relaxed);
        if writes == 0 {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        let last = self.graph_last_rebuild.load(Ordering::Relaxed);
        if writes >= GRAPH_REBUILD_WRITES || now - last >= GRAPH_REBUILD_SECS {
            self.rebuild_graph_now();
        }
    }

    /// Phase B: rebuild the graph from the store unconditionally and reset
    /// the lazy-rebuild bookkeeping. Same cost as the lazy trigger; called
    /// when a query proves the graph is stale (a store-resolved seed maps
    /// to no node) so the unified engine is never weaker than the store
    /// paths it replaced.
    fn rebuild_graph_now(&self) {
        if let Ok(g) = CausalGraph::from_store(&self.store) {
            if let Ok(mut guard) = self.graph.lock() {
                *guard = Some(g);
            }
        }
        self.graph_writes.store(0, Ordering::Relaxed);
        self.graph_last_rebuild
            .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        // D1: flush buffered co-activation pairs alongside the rebuild.
        self.flush_cooccurrences();
    }

    /// Record every unordered pair of co-activated chunks (D1). Retrieval
    /// results are typically small (<10 nodes -> <45 pairs), so this is
    /// cheap; the pairs are flushed to the DB only at graph-rebuild time.
    fn buffer_cooccurrences(&self, active_ids: &[String]) {
        if active_ids.len() < 2 {
            return;
        }
        let mut pairs = Vec::with_capacity(active_ids.len() * active_ids.len() / 2);
        for i in 0..active_ids.len() {
            for j in (i + 1)..active_ids.len() {
                pairs.push((active_ids[i].clone(), active_ids[j].clone()));
            }
        }
        if let Ok(mut buf) = self.cooc_buffer.lock() {
            buf.extend(pairs);
            // Hard cap so a pathological store never grows unbounded.
            if buf.len() > 4000 {
                buf.truncate(4000);
            }
        }
    }

    fn flush_cooccurrences(&self) {
        let pairs: Vec<(String, String)> = self
            .cooc_buffer
            .lock()
            .map(|mut b| std::mem::take(&mut *b))
            .unwrap_or_default();
        if !pairs.is_empty() {
            let _ = self.store.bump_cooccurrences(&pairs);
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
        self.maybe_rebuild_graph();
        let mut guard = self.graph.lock().ok()?;
        let graph = guard.as_mut()?;
        if graph.num_nodes() == 0 {
            return None;
        }

        let results = graph.spreading_activation(query, task_tag, reverse);
        if results.is_empty() {
            return None;
        }

        // D1: co-activated chunks (above threshold) wire together — buffer
        // the pairs for the Hebbian co-occurrence table (flushed at graph
        // rebuild). Only nodes that actually lit up participate.
        let active_ids: Vec<String> = results
            .iter()
            .filter(|r| r.activation.abs() >= graph.threshold())
            .map(|r| graph.node_id(r.node_idx as usize).to_string())
            .collect();
        self.buffer_cooccurrences(&active_ids);

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

/// Run an async embed/LLM call from a sync memory op.
/// When called inside a multi-thread tokio runtime (the MCP server), bridge
/// with block_in_place; otherwise (Python bindings, CLI one-shots) drive the
/// future on a throwaway runtime.
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
