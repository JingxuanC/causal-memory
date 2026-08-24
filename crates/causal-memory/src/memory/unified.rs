//! Phase B (one-graph-convergence): the unified retrieval engine — one
//! seeding pass over ALL node types, one spreading-activation run over
//! the whole typed graph, typed results. [`Memory::search_memory`] and
//! `search_memory_entries` consume it; the dual-pool RRF path in `ops`
//! stays as the fallback and the A/B regression control.

use std::collections::HashMap;

use super::{block_on, Memory};
use crate::hippocampus::{ActivationResult, CausalGraph};
use crate::store::{AgentFact, CausalEntry};

/// Cap on store-resolved seeds — spread cost scales with the seed set,
/// and beyond this the tail seeds contribute almost nothing.
const UNIFIED_SEED_LIMIT: usize = 16;

/// Typed hits from the unified spreading-activation engine — facts and
/// causal entries in activation order (strongest first).
pub(crate) struct UnifiedSpreadHits {
    pub facts: Vec<AgentFact>,
    pub causal: Vec<CausalEntry>,
}

impl Memory {
    /// The unified engine. Returns None when it can't serve the query
    /// (no graph / no seeds / nothing activated / nothing materializable)
    /// — callers fall back to the dual-pool RRF path.
    pub(crate) fn unified_spread_hits(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: usize,
    ) -> Option<UnifiedSpreadHits> {
        self.maybe_rebuild_graph();
        let seed_ids = self.unified_seed_ids(query, task_tag, scope);
        self.ensure_fresh_for(&seed_ids);

        // Spread + typed split happen under one lock hold; the store
        // materialization below runs lock-free.
        let (fact_ids, chunk_activation, active_ids) = {
            let mut guard = self.graph.lock().ok()?;
            let graph = guard.as_mut()?;
            if graph.num_nodes() == 0 {
                return None;
            }
            let results = graph.spreading_activation_seeded(query, &seed_ids, task_tag, true);
            if results.is_empty() {
                return None;
            }
            split_typed(&results, graph)
        };
        // Hebbian co-activation buffering — same contract as the
        // hippocampus path (buffered pairs flush at the next rebuild).
        self.buffer_cooccurrences(&active_ids);

        let facts = self.materialize_facts(&fact_ids, scope, limit);
        let mut causal = self.materialize_causal(&chunk_activation, task_tag, limit);

        // Query-plan awareness (the plan_query port from the multi-pass
        // harness path): temporal anchors float in-window evidence to the
        // top of the causal list — the anchor rules (N unit ago, last
        // <weekday>, past-N-unit windows) run against each entry's
        // event_time. Date-math questions were already carved out of
        // aggregation upstream (looks_aggregation); this adds the
        // positive half for the engine path.
        let plan = crate::retrieval::plan_query(query, chrono::Utc::now().timestamp());
        if let Some((start, end)) = plan.time_window {
            causal.sort_by(|a, b| {
                let aw = a.event_time >= start && a.event_time <= end;
                let bw = b.event_time >= start && b.event_time <= end;
                bw.cmp(&aw).then(b.event_time.cmp(&a.event_time))
            });
        }

        if facts.is_empty() && causal.is_empty() {
            return None;
        }
        Some(UnifiedSpreadHits { facts, causal })
    }

    /// Seed layer (unified, all node types): the persistent BM25 index
    /// first — it deliberately spans BOTH namespaces — then semantic
    /// seeds when an embedder is configured.
    fn unified_seed_ids(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
    ) -> Vec<String> {
        let mut seed_ids: Vec<String> = self
            .store
            .bm25_seed_ids(query, scope, UNIFIED_SEED_LIMIT)
            .unwrap_or_default();
        if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(query)) {
            if let Ok(sem) = self
                .store
                .search_facts_semantic(&vec, scope, UNIFIED_SEED_LIMIT)
            {
                seed_ids.extend(sem.into_iter().map(|(f, _)| format!("fact:{}", f.id)));
            }
            if let Ok(sem) = self.store.search_causal_semantic_entity_boosted(
                &vec,
                query,
                task_tag,
                UNIFIED_SEED_LIMIT,
            ) {
                seed_ids.extend(sem.into_iter().map(|(e, _)| e.decision_id));
            }
        }
        seed_ids
    }

    /// Freshness (Phase C preview): a store-resolved seed that maps to no
    /// graph node means writes landed after the last (lazy) rebuild.
    /// Serving the query from the stale graph would silently drop that
    /// seed — weaker than the store-direct RRF path this engine replaces.
    /// Rebuild once and let the caller continue; the cost is the same
    /// from_store the lazy trigger would eventually pay anyway. An empty
    /// graph with resolvable seeds is the same staleness (the store grew
    /// after startup), not a reason to skip.
    fn ensure_fresh_for(&self, seed_ids: &[String]) {
        let stale = {
            let Ok(guard) = self.graph.lock() else {
                return;
            };
            match guard.as_ref() {
                Some(graph) => seed_ids.iter().any(|id| !graph.has_node(id)),
                None => false,
            }
        };
        if stale {
            self.rebuild_graph_now();
        }
    }

    /// Facts in activation order, scope-filtered, capped at `limit`.
    fn materialize_facts(
        &self,
        fact_ids: &[i64],
        scope: Option<&str>,
        limit: usize,
    ) -> Vec<AgentFact> {
        let by_id: HashMap<i64, AgentFact> = self
            .store
            .facts_by_ids(fact_ids)
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.id, f))
            .collect();
        fact_ids
            .iter()
            .filter_map(|id| by_id.get(id))
            .filter(|f| scope.is_none_or(|s| f.scope == s))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Edges touching the activated chunks, ranked by their strongest
    /// endpoint activation (activation carries the engine's ordering;
    /// confidence is only the SQL prefilter's tiebreak).
    fn materialize_causal(
        &self,
        chunk_activation: &[(String, f32)],
        task_tag: Option<&str>,
        limit: usize,
    ) -> Vec<CausalEntry> {
        let edges = self
            .store
            .edges_touching_chunks(chunk_activation, task_tag, limit.saturating_mul(2).max(10))
            .unwrap_or_default();
        rank_edges_by_activation(chunk_activation, edges, limit)
    }
}

/// Typed split of the activation results: fact node ids, chunk nodes with
/// their activation, and all active node ids (Hebbian buffering input).
/// Scope hubs are skipped.
fn split_typed(
    results: &[ActivationResult],
    graph: &CausalGraph,
) -> (Vec<i64>, Vec<(String, f32)>, Vec<String>) {
    let mut fact_ids: Vec<i64> = Vec::new();
    let mut chunk_activation: Vec<(String, f32)> = Vec::new();
    let mut active_ids: Vec<String> = Vec::new();
    // Dedup sets — the old `contains`/`any` linear scans are O(n²) over the
    // same thousands-of-nodes spread results (see rank_edges_by_activation).
    let mut seen_facts: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut seen_chunks: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in results {
        if r.activation.abs() < graph.threshold() {
            continue;
        }
        let id = graph.node_id(r.node_idx as usize);
        active_ids.push(id.to_string());
        if let Some(fid) = id.strip_prefix("fact:") {
            if let Ok(n) = fid.parse::<i64>() {
                if seen_facts.insert(n) {
                    fact_ids.push(n);
                }
            }
        } else if !id.starts_with("scope:") && seen_chunks.insert(id.to_string()) {
            chunk_activation.push((id.to_string(), r.activation));
        }
    }
    (fact_ids, chunk_activation, active_ids)
}

/// Rank edges by the strongest activation among their endpoint chunks.
fn rank_edges_by_activation(
    chunk_activation: &[(String, f32)],
    edges: Vec<CausalEntry>,
    limit: usize,
) -> Vec<CausalEntry> {
    // HashMap lookup, not a linear scan per endpoint: a wide spread lights
    // up thousands of chunks and `edges_touching_chunks` returns thousands
    // of candidate edges — the old O(E×C) String comparisons pegged a CPU
    // core for minutes per query on the 32万-node LongMemEval store
    // (measured: ablation harness 60s+/question with zero I/O).
    let act: HashMap<&str, f32> = chunk_activation
        .iter()
        .map(|(c, a)| (c.as_str(), a.abs()))
        .collect();
    let mut scored: Vec<(f32, CausalEntry)> = edges
        .into_iter()
        .map(|e| {
            let strength = act
                .get(e.decision_id.as_str())
                .copied()
                .unwrap_or(0.0)
                .max(act.get(e.outcome_id.as_str()).copied().unwrap_or(0.0));
            (strength, e)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().map(|(_, e)| e).take(limit).collect()
}
