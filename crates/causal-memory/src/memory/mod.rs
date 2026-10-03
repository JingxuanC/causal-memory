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
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use format::{format_activation_layered, provenance_tag, TokenBudget};

/// Rebuild policy (T0, enterprise-scaling doc): the periodic full
/// `from_store` rebuild is **amortized maintenance**, not the freshness
/// mechanism. Freshness comes from write-path patches (`patch_graph_new_edge`
/// / `patch_graph_new_fact` / retire — see ops.rs) plus the immediate
/// proof-of-staleness rebuild when a store-resolved seed maps to no node
/// (unified.rs). The generous ceilings below stop an active writer from
/// self-throttling on O(store) reloads: measured, a full rebuild is ~107 s at
/// 1M nodes / 4M edges (see docs/design/enterprise-scaling.md), so the old
/// 30 s cadence made the store spend most of its time rebuilding itself.
/// Periodic full rebuilds still run to (a) GC retired nodes, (b) flush the
/// co-activation buffer into cooccurrence_edges, (c) repair any drift a
/// patch missed.
const GRAPH_REBUILD_WRITES: usize = 512;
const GRAPH_REBUILD_SECS: i64 = 900;

/// Bounded retries for a write-path patch whose graph generation changed
/// underneath it (see `patch_graph_optimistic`). One retry is the normal
/// case; the cap only stops a pathological rebuild storm from spinning the
/// recorder.
const PATCH_REPLAY_ATTEMPTS: usize = 4;

/// Cosine floor for semantic seeding in intervention_query (recall-oriented).
pub(crate) const INTERVENTION_MIN_SIMILARITY: f64 = 0.5;

/// Phase-4 interface: task tags whose consequences live in deterministic
/// environments (builds, tests, configs, benches). counterfactual_query
/// routes these to an executable-replay plan — rerunning the alternative
/// in a sandbox beats any estimate when the world is code.
pub(crate) const CLOSED_WORLD_TAGS: [&str; 4] = ["build", "test", "config", "bench"];
/// Cosine floor for the semantic contradiction scan on record (precision-
/// oriented: only paraphrase-level duplicates of the same decision).
pub(crate) const SEMANTIC_CONTRADICTION_MIN_SIMILARITY: f64 = 0.85;

/// Reciprocal Rank Fusion constant (the RRF paper's standard value).
pub(crate) const RRF_K: f64 = 60.0;

pub mod format;
pub mod ops;
pub mod output;
mod unified;

#[cfg(test)]
mod tests;

/// State of the hippocampus graph accelerator (F2: lazy construction).
///
/// The graph is built on the first *graph-consuming* query, not at
/// construction, so a frontend that only writes (record_decision /
/// record_fact / remember) never pays the O(store) build. A plain `Option`
/// cannot carry this: `None` would have to mean both "not built yet, so
/// build it" and "the build failed, serve from the store" — and every
/// consumer that reads `None` as "no graph available" (the unified engine's
/// `guard.as_mut()?`, `ensure_fresh_for`) would silently leave the graph
/// unbuilt forever.
enum GraphSlot {
    /// Never built. Every graph entry point builds it via
    /// [`Memory::ensure_graph_built`]; a write-path patch only marks the
    /// slot dirty (the write is already in the store, so the build that
    /// does happen picks it up).
    Unbuilt,
    /// A live graph. May be stale — staleness is tracked separately
    /// (`graph_writes` / `patch_epoch`), not by this state.
    Ready(CausalGraph),
    /// The build errored. Queries degrade to the store-only paths (dual-pool
    /// RRF / SQL) instead of retrying an O(store) load per query.
    Failed,
}

impl GraphSlot {
    fn as_graph(&self) -> Option<&CausalGraph> {
        match self {
            GraphSlot::Ready(graph) => Some(graph),
            GraphSlot::Unbuilt | GraphSlot::Failed => None,
        }
    }

    fn as_graph_mut(&mut self) -> Option<&mut CausalGraph> {
        match self {
            GraphSlot::Ready(graph) => Some(graph),
            GraphSlot::Unbuilt | GraphSlot::Failed => None,
        }
    }
}

/// The unified agent memory: one `CausalStore` plus a lazily-rebuilt
/// hippocampus graph accelerator and the Hebbian co-occurrence buffer.
///
/// All 15 operations are methods on this struct (see `ops`). Frontends:
/// the MCP server (`causal-memory-cli::server`) and the Python bindings
/// (`causal-memory-py`).
pub struct Memory {
    pub(crate) store: CausalStore,
    /// The lazy graph slot (F2). Readers must go through
    /// [`Memory::ensure_graph_built`] first — a graph entry point that
    /// forgets it silently serves the store-only fallback forever.
    graph: Mutex<GraphSlot>,
    /// Graph generation: +1 on every swap. A write-path patch records it
    /// before patching and re-reads it after, so a patch that a concurrent
    /// rebuild swapped out from under it can be replayed onto the new graph
    /// (see `patch_graph_optimistic`).
    graph_version: AtomicU64,
    /// Patch counter, bumped under the `graph` lock by every write-path
    /// patch. A rebuilder samples it before reading the store and refuses
    /// to install a graph whose build window overlapped a patch — that
    /// patch's write may postdate the snapshot (see `install_graph`).
    patch_epoch: AtomicU64,
    /// Pending writes since the last installed snapshot (monotonic).
    graph_writes: AtomicUsize,
    /// Unix ts of the read transaction backing the live graph — the
    /// snapshot's horizon, not the install time (a write can land while the
    /// snapshot is being built; stamping that as "rebuilt" would hide it).
    graph_last_rebuild: AtomicI64,
    /// Single-flight for graph rebuilds: whoever gets this lock rebuilds,
    /// everyone else serves the current graph. A rebuild is O(store), so
    /// queueing N concurrent queries behind N of them is strictly worse
    /// than answering one of them from a slightly stale graph.
    rebuild_flight: Mutex<()>,
    /// D1: co-activated chunk pairs buffered by retrieval, flushed to the
    /// cooccurrence_edges table when the graph rebuilds. Keeps Hebbian
    /// learning off the read path (batched, low-frequency writes).
    cooc_buffer: Mutex<Vec<(String, String)>>,
    /// Observability: which frontend this instance serves ("core" default;
    /// "mcp-stdio" / "mcp-http" / "amc" when the server sets it). Labels
    /// the recall_audit rows and request metrics.
    server_label: &'static str,
}

impl Memory {
    /// Wrap an existing store.
    pub fn new(store: CausalStore) -> Self {
        Self::new_with_label(store, "core")
    }

    /// Wrap an existing store with a server label (observability).
    pub fn new_with_label(store: CausalStore, server_label: &'static str) -> Self {
        // F2: the hippocampus graph is NOT loaded here. The first
        // graph-consuming query builds it (`ensure_graph_built`), so an
        // instance that only writes never pays the O(store) load — the
        // per-request instances the HTTP frontends construct only pay for
        // reads. Single-instance frontends (stdio, Python) prewarm in the
        // background instead, to keep their first query warm: see
        // `spawn_prewarm`.
        Self {
            store,
            graph: Mutex::new(GraphSlot::Unbuilt),
            graph_version: AtomicU64::new(0),
            patch_epoch: AtomicU64::new(0),
            graph_writes: AtomicUsize::new(0),
            // No snapshot installed yet; the first install stamps the real
            // horizon. Never consulted while the slot is not `Ready`.
            graph_last_rebuild: AtomicI64::new(0),
            rebuild_flight: Mutex::new(()),
            cooc_buffer: Mutex::new(Vec::new()),
            server_label,
        }
    }

    /// The observability label of this instance ("core" unless a server
    /// set one).
    pub(crate) fn server_label(&self) -> &'static str {
        self.server_label
    }

    /// Open (or create) a memory database at `path`, running migrations.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::new(CausalStore::open(path)?))
    }

    /// An in-memory memory — for tests and ephemeral use.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self::new(CausalStore::open_in_memory()?))
    }

    /// F2: build the graph on a background thread, without blocking startup.
    ///
    /// A single-instance frontend (the stdio MCP server, the Python
    /// bindings) keeps one `Memory` alive for the whole process, so without
    /// this its first query would pay the full build that construction used
    /// to pay. Per-request instances (HTTP) must NOT call this: their query
    /// builds on demand anyway, and prewarming would rebuild the graph once
    /// per request — the exact cost F2 removed.
    ///
    /// Takes `&Arc<Self>` because it outlives the caller. Failure is not
    /// fatal and never blocks: the slot lands `Failed`, the thread exits,
    /// and queries serve the store-only paths.
    pub fn spawn_prewarm(self: &Arc<Self>) {
        let this = Arc::clone(self);
        if let Err(e) = std::thread::Builder::new()
            .name("memory-prewarm".into())
            .spawn(move || {
                this.ensure_graph_built();
                tracing::debug!(
                    server = this.server_label,
                    nodes = this.graph_node_count(),
                    "graph prewarm finished"
                );
            })
        {
            // Losing the prewarm only costs a cold first query.
            tracing::warn!(error = %e, server = self.server_label, "graph prewarm thread failed");
        }
    }

    /// Nodes in the live graph (0 when unbuilt or failed) — observability
    /// for the prewarm log line.
    fn graph_node_count(&self) -> usize {
        self.graph
            .lock()
            .map(|guard| guard.as_graph().map_or(0, CausalGraph::num_nodes))
            .unwrap_or(0)
    }

    /// Access the underlying store (escape hatch for frontends that need
    /// raw queries; prefer the high-level ops).
    pub fn store(&self) -> &CausalStore {
        &self.store
    }

    /// Ablation switch: disable spreading activation on the live graph —
    /// retrieval keeps the seeding layer (BM25/semantic direct hits) but
    /// no activation propagates along edges. No-op if the graph failed to
    /// build (retrieval already runs the dual-pool RRF fallback in that
    /// case). Irreversible for this `Memory` instance; reopen to undo.
    /// Used by the no-spread ablation arm (benches/ablation).
    ///
    /// Builds the graph if it is not built yet: flipping a slot that does
    /// not exist would be silently forgotten by the build that follows, and
    /// the ablation arm would run with spreading activation still on.
    pub fn disable_spread(&self) {
        self.ensure_graph_built();
        if let Ok(mut guard) = self.graph.lock() {
            if let Some(graph) = guard.as_graph_mut() {
                graph.disable_spread();
            }
        }
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
        self.patch_graph_optimistic(|graph| {
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
        });
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
        self.patch_graph_optimistic(|graph| {
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
            // Organizational edge (NoEffect): the scope hub must not
            // propagate activation — see from_store's counterpart.
            graph.add_patch_edge(scope_idx, fact_idx, Relation::NoEffect, confidence as f32);
            graph.link_fact_node(fact_idx);
        });
    }

    /// Phase C: retire graph nodes for facts superseded by `new_fact_id`
    /// (the store sets `superseded_by` on replace). Retired fact nodes
    /// neither seed nor surface until the next full rebuild drops them.
    fn patch_graph_retire_facts(&self, new_fact_id: i64) {
        let superseded: Vec<i64> = self
            .store
            .with_conn(|conn| {
                let mut stmt =
                    conn.prepare("SELECT id FROM agent_facts WHERE superseded_by = ?1")?;
                let rows = stmt.query_map(rusqlite::params![new_fact_id], |r| r.get(0))?;
                let ids: std::result::Result<Vec<i64>, rusqlite::Error> = rows.collect();
                Ok(ids?)
            })
            .unwrap_or_default();
        if superseded.is_empty() {
            return;
        }
        self.patch_graph_optimistic(|graph| {
            for id in &superseded {
                graph.retire_node(&format!("fact:{id}"));
            }
        });
    }

    /// Apply a write-path patch to the live graph under a generation
    /// double-check: sample the graph generation, patch under the lock,
    /// re-read the generation — a swap in between means the patch landed on
    /// a graph that is already gone, so replay it onto the new one. Without
    /// this the patch (and the write it reports) disappears with the old
    /// graph. Patches must be idempotent for replay — `append_node` dedups
    /// by id, `add_patch_edge` upserts, retire/invalidate are set flips.
    ///
    /// A missing graph is not an error: the write is already in the store,
    /// so whatever builds the graph next picks it up.
    fn patch_graph_optimistic(&self, patch: impl Fn(&mut CausalGraph)) {
        for _ in 0..PATCH_REPLAY_ATTEMPTS {
            let version = self.graph_version.load(Ordering::Acquire);
            let Ok(mut guard) = self.graph.lock() else {
                return;
            };
            // F2: nothing to patch yet (or the build failed) — the write is
            // already committed, and building the graph here would make
            // every write-only request pay the O(store) load this batch
            // removes, so leave the slot as it is and let the build a query
            // triggers pick the write up. Not bumping `patch_epoch` on this
            // path is deliberate: the epoch is what refuses a snapshot that
            // predates a patch, and a first build must not be refused (a
            // recording writer would then keep it unbuilt forever). The
            // write stays counted in `graph_writes`, and `ensure_fresh_for`
            // re-checks any seed the landed snapshot misses.
            let graph = match guard.as_graph_mut() {
                Some(graph) => graph,
                None => return,
            };
            patch(graph);
            self.patch_epoch.fetch_add(1, Ordering::Release);
            drop(guard);
            if self.graph_version.load(Ordering::Acquire) == version {
                return;
            }
        }
    }

    /// F2 entry gate: build the graph if it has never been built.
    ///
    /// Every graph entry point calls this before touching the slot —
    /// `hippocampus_search` (which `search_causal` and `trace_cause` route
    /// through), `unified_spread_hits`, `disable_spread`. A consumer that
    /// skips it reads `GraphSlot::Unbuilt` as "no graph" and silently serves
    /// the store-only fallback forever, which is the failure mode the old
    /// `Option` slot made undetectable.
    ///
    /// Single-flight, exactly like the periodic rebuild: whoever wins
    /// builds, everyone else serves the store-only path until it lands.
    fn ensure_graph_built(&self) {
        if !self.graph_is_unbuilt() {
            return;
        }
        let Ok(_flight) = self.rebuild_flight.try_lock() else {
            return;
        };
        // Re-check under the flight lock: the winner may have just built.
        if self.graph_is_unbuilt() {
            self.rebuild_graph_now();
        }
    }

    /// Is the slot still waiting for its first build?
    fn graph_is_unbuilt(&self) -> bool {
        let Ok(guard) = self.graph.lock() else {
            return false;
        };
        matches!(*guard, GraphSlot::Unbuilt)
    }

    /// Is a live graph installed?
    fn graph_is_ready(&self) -> bool {
        let Ok(guard) = self.graph.lock() else {
            return false;
        };
        matches!(*guard, GraphSlot::Ready(_))
    }

    /// Rebuild the graph when enough writes have accumulated or enough time
    /// passed. Called at the top of every graph query.
    fn maybe_rebuild_graph(&self) {
        if !self.should_rebuild() {
            return;
        }
        // Single-flight: rebuild now, or serve the current graph if someone
        // else already is.
        let Ok(_flight) = self.rebuild_flight.try_lock() else {
            return;
        };
        // Re-check under the flight lock: the winner may have just rebuilt,
        // and the check above ran outside it.
        if self.should_rebuild() {
            self.rebuild_graph_now();
        }
    }

    /// Is the live graph behind the store? `graph_writes` counts writes
    /// recorded since the snapshot currently installed.
    fn should_rebuild(&self) -> bool {
        // F2: an unbuilt slot is built by `ensure_graph_built`, and a failed
        // one must not be retried per query (an O(store) load that just
        // failed again). Neither has a snapshot this policy could age out.
        if !self.graph_is_ready() {
            return false;
        }
        let writes = self.graph_writes.load(Ordering::Relaxed);
        if writes == 0 {
            return false;
        }
        let age = chrono::Utc::now().timestamp() - self.graph_last_rebuild.load(Ordering::Relaxed);
        writes >= GRAPH_REBUILD_WRITES || age >= GRAPH_REBUILD_SECS
    }

    /// Build a graph snapshot from the store — the long, lock-free part of a
    /// rebuild. Also returns the snapshot's read-transaction time and the
    /// patch epoch sampled *before* the store read; both are handed back to
    /// [`Self::install_graph`].
    fn build_graph_snapshot(&self) -> Result<(CausalGraph, i64, u64)> {
        let epoch = self.patch_epoch.load(Ordering::Acquire);
        let (graph, snapshot_ts) = CausalGraph::from_store_snapshot(&self.store)?;
        Ok((graph, snapshot_ts, epoch))
    }

    /// Install a snapshot built by [`Self::build_graph_snapshot`] — unless a
    /// write-path patch landed while it was building. Such a patch's write
    /// may postdate the snapshot (the writer commits, then patches), and
    /// installing would drop it from the graph with nothing left to repair
    /// it. Refusing keeps the current, patched graph and leaves those writes
    /// pending, so a later rebuild retries. Returns whether it installed.
    ///
    /// F2: this needs no special case for an unbuilt slot. `patch_epoch` is
    /// only bumped after a patch lands on a *live* graph (see
    /// `patch_graph_optimistic`), and the slot never returns to Unbuilt — so
    /// while the slot is unbuilt the epoch cannot move, and a first build
    /// always installs. A write landing during that build is not dropped
    /// silently: it stays counted in `graph_writes` for the next rebuild,
    /// and `ensure_fresh_for` rebuilds on the first query that seeds on it.
    fn install_graph(&self, graph: CausalGraph, snapshot_ts: i64, built_at_epoch: u64) -> bool {
        let Ok(mut guard) = self.graph.lock() else {
            return false;
        };
        if self.patch_epoch.load(Ordering::Acquire) != built_at_epoch {
            return false;
        }
        *guard = GraphSlot::Ready(graph);
        self.graph_version.fetch_add(1, Ordering::Release);
        drop(guard);
        self.graph_last_rebuild
            .store(snapshot_ts, Ordering::Relaxed);
        true
    }

    /// Phase B: rebuild the graph from the store and install it. Same cost
    /// as the lazy trigger; called when a query proves the graph is stale (a
    /// store-resolved seed maps to no node) so the unified engine is never
    /// weaker than the store paths it replaced. Callers hold
    /// `rebuild_flight`.
    fn rebuild_graph_now(&self) {
        // Writes this rebuild intends to absorb.
        let pending = self.graph_writes.load(Ordering::Relaxed);
        match self.build_graph_snapshot() {
            Ok((graph, snapshot_ts, epoch)) => {
                if self.install_graph(graph, snapshot_ts, epoch) {
                    // Saturating: writes are counted *after* their patch, so
                    // a write that landed during the build can push the
                    // counter past `pending` — and an unconditional store(0)
                    // (the old behavior) would wipe exactly those.
                    let _ =
                        self.graph_writes
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
                                Some(w.saturating_sub(pending))
                            });
                }
            }
            Err(e) => {
                // F2: a first build that cannot read the store leaves the
                // slot Failed, so queries keep serving the store-only paths
                // instead of retrying an O(store) load each time. A failed
                // *refresh* of a live graph keeps the stale-but-usable one.
                tracing::warn!(error = %e, server = self.server_label, "graph build failed");
                self.mark_graph_failed();
            }
        }
        // D1: flush buffered co-activation pairs alongside the rebuild.
        self.flush_cooccurrences();
    }

    /// Move an unbuilt slot to `Failed`. A live graph is never downgraded —
    /// a failed refresh keeps the stale-but-usable graph.
    fn mark_graph_failed(&self) {
        if let Ok(mut guard) = self.graph.lock() {
            if matches!(*guard, GraphSlot::Unbuilt) {
                *guard = GraphSlot::Failed;
            }
        }
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
    /// Honors the same detail_level/max_tokens contract as the BM25 and
    /// semantic paths — these were dead parameters on this path until the
    /// budget was threaded through here. `explain` appends a provenance
    /// tag per hit (Flip-path marking); false = historical output.
    #[allow(clippy::too_many_arguments)]
    fn hippocampus_search(
        &self,
        query: &str,
        task_tag: Option<&str>,
        reverse: bool,
        limit: usize,
        detail_level: &str,
        max_tokens: usize,
        explain: bool,
    ) -> Option<String> {
        self.ensure_graph_built();
        self.maybe_rebuild_graph();
        let mut guard = self.graph.lock().ok()?;
        let graph = guard.as_graph_mut()?;
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
            "[hippocampus/{direction}/{detail_level}] Activated {}/{} nodes via spreading activation",
            count,
            results.len()
        );
        if max_tokens > 0 {
            out.push_str(&format!(" (token budget: {max_tokens})"));
        }
        out.push_str(":\n\n");
        let mut budget = TokenBudget::new(max_tokens);
        for (i, r) in results.iter().take(limit).enumerate() {
            let (line, cost) =
                format_activation_layered(&r.text, r.activation, i + 1, detail_level);
            if !budget.try_spend(cost) {
                out.push_str(&format!(
                    "… {} more result(s) truncated (token budget)\n",
                    count - i
                ));
                break;
            }
            out.push_str(&line);
            if explain {
                let tag = provenance_tag(
                    r.hop,
                    r.via.map(|v| v.relation.as_str()),
                    r.via.map(|v| graph.node_text(v.from as usize)),
                );
                out.push_str(&format!("   ↳ {tag}\n"));
            }
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
