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

/// Explain provenance of one surfaced hit (Flip-path marking): whether it
/// was a direct seed hit or lit by spreading — and if spread, at which hop
/// and through which edge. Display-only; never affects ranking.
#[derive(Debug, Clone)]
pub(crate) struct Provenance {
    pub hop: u8,
    pub via_relation: Option<&'static str>,
    pub via_from_text: Option<String>,
}

impl Provenance {
    /// Shared seed-marker instance (direct hit, no spread hop).
    const SEED: Provenance = Provenance {
        hop: 0,
        via_relation: None,
        via_from_text: None,
    };
    /// The explain tag appended to a hit when explain=true:
    /// `[seed]` or `[spread hop=2 via prevented←"skip tests"]`.
    pub(crate) fn tag(&self) -> String {
        super::format::provenance_tag(self.hop, self.via_relation, self.via_from_text.as_deref())
    }
}

/// Typed hits from the unified spreading-activation engine — facts and
/// causal entries in activation order (strongest first), with per-hit
/// provenance and recall-audit metadata.
pub(crate) struct UnifiedSpreadHits {
    pub facts: Vec<(AgentFact, Provenance)>,
    pub causal: Vec<(CausalEntry, Provenance)>,
    /// Seeds used, with their source ("bm25" | "semantic") — audit/metrics.
    pub seeds: Vec<(String, &'static str)>,
    /// Nodes lit above threshold in this run.
    pub activated_nodes: usize,
    /// Deepest hop any surfaced hit was lit at.
    pub max_hop: u8,
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
        let (seed_ids, seeds) = self.unified_seed_ids(query, task_tag, scope);
        self.ensure_fresh_for(&seed_ids);

        // Spread + typed split happen under one lock hold; the store
        // materialization below runs lock-free.
        let (fact_prov, chunk_activation, active_ids, activated_nodes, max_hop) = {
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

        let facts = self.materialize_facts(&fact_prov, scope, limit);
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
                let aw = a.0.event_time >= start && a.0.event_time <= end;
                let bw = b.0.event_time >= start && b.0.event_time <= end;
                bw.cmp(&aw).then(b.0.event_time.cmp(&a.0.event_time))
            });
        }

        if facts.is_empty() && causal.is_empty() {
            return None;
        }
        Some(UnifiedSpreadHits {
            facts,
            causal,
            seeds,
            activated_nodes,
            max_hop,
        })
    }

    /// Seed layer (unified, all node types): the persistent BM25 index
    /// first — it deliberately spans BOTH namespaces — then semantic
    /// seeds when an embedder is configured. Returns the deduped seed ids
    /// plus the per-seed source list (audit/metrics).
    fn unified_seed_ids(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
    ) -> (Vec<String>, Vec<(String, &'static str)>) {
        let mut seeds: Vec<(String, &'static str)> = Vec::new();
        if let Ok(bm25) = self.store.bm25_seed_ids(query, scope, UNIFIED_SEED_LIMIT) {
            crate::observability::metrics().record_recall_seeds("bm25", bm25.len());
            seeds.extend(bm25.into_iter().map(|id| (id, "bm25")));
        }
        if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(query)) {
            let mut n = 0usize;
            if let Ok(sem) = self
                .store
                .search_facts_semantic(&vec, scope, UNIFIED_SEED_LIMIT)
            {
                n += sem.len();
                seeds.extend(
                    sem.into_iter()
                        .map(|(f, _)| (format!("fact:{}", f.id), "semantic")),
                );
            }
            if let Ok(sem) = self.store.search_causal_semantic_entity_boosted(
                &vec,
                query,
                task_tag,
                UNIFIED_SEED_LIMIT,
            ) {
                n += sem.len();
                seeds.extend(sem.into_iter().map(|(e, _)| (e.decision_id, "semantic")));
            }
            crate::observability::metrics().record_recall_seeds("semantic", n);
        }
        let seed_ids: Vec<String> = seeds.iter().map(|(id, _)| id.clone()).collect();
        (seed_ids, seeds)
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

    /// Facts in activation order, scope-filtered, capped at `limit`;
    /// each carries its provenance.
    fn materialize_facts(
        &self,
        fact_prov: &[(i64, Provenance)],
        scope: Option<&str>,
        limit: usize,
    ) -> Vec<(AgentFact, Provenance)> {
        let ids: Vec<i64> = fact_prov.iter().map(|(id, _)| *id).collect();
        let by_id: HashMap<i64, AgentFact> = self
            .store
            .facts_by_ids(&ids)
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.id, f))
            .collect();
        fact_prov
            .iter()
            .filter_map(|(id, prov)| by_id.get(id).map(|f| (f.clone(), prov.clone())))
            .filter(|(f, _)| scope.is_none_or(|s| f.scope == s))
            .take(limit)
            .collect()
    }

    /// Edges touching the activated chunks, ranked by their strongest
    /// endpoint activation (activation carries the engine's ordering;
    /// confidence is only the SQL prefilter's tiebreak). Each entry carries
    /// the provenance of its winning endpoint.
    fn materialize_causal(
        &self,
        chunk_activation: &[(String, f32, Provenance)],
        task_tag: Option<&str>,
        limit: usize,
    ) -> Vec<(CausalEntry, Provenance)> {
        let act_pairs: Vec<(String, f32)> = chunk_activation
            .iter()
            .map(|(c, a, _)| (c.clone(), *a))
            .collect();
        let edges = self
            .store
            .edges_touching_chunks(&act_pairs, task_tag, limit.saturating_mul(2).max(10))
            .unwrap_or_default();
        rank_edges_by_activation(chunk_activation, edges, limit)
    }
}

/// Typed split of the activation results: fact node ids with provenance,
/// chunk nodes with activation + provenance, and all active node ids
/// (Hebbian buffering input), plus the audit summary (activated count,
/// max hop). Scope hubs are skipped.
#[allow(clippy::type_complexity)]
fn split_typed(
    results: &[ActivationResult],
    graph: &CausalGraph,
) -> (
    Vec<(i64, Provenance)>,
    Vec<(String, f32, Provenance)>,
    Vec<String>,
    usize,
    u8,
) {
    let mut fact_prov: Vec<(i64, Provenance)> = Vec::new();
    let mut chunk_activation: Vec<(String, f32, Provenance)> = Vec::new();
    let mut active_ids: Vec<String> = Vec::new();
    let mut activated = 0usize;
    let mut max_hop = 0u8;
    // Dedup sets — the old `contains`/`any` linear scans are O(n²) over the
    // same thousands-of-nodes spread results (see rank_edges_by_activation).
    let mut seen_facts: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut seen_chunks: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in results {
        if r.activation.abs() < graph.threshold() {
            continue;
        }
        activated += 1;
        max_hop = max_hop.max(r.hop);
        let prov = Provenance {
            hop: r.hop,
            via_relation: r.via.map(|v| v.relation.as_str()),
            via_from_text: r.via.map(|v| graph.node_text(v.from as usize).to_string()),
        };
        let id = graph.node_id(r.node_idx as usize);
        active_ids.push(id.to_string());
        if let Some(fid) = id.strip_prefix("fact:") {
            if let Ok(n) = fid.parse::<i64>() {
                if seen_facts.insert(n) {
                    fact_prov.push((n, prov));
                }
            }
        } else if !id.starts_with("scope:") && seen_chunks.insert(id.to_string()) {
            chunk_activation.push((id.to_string(), r.activation, prov));
        }
    }
    (fact_prov, chunk_activation, active_ids, activated, max_hop)
}

/// Rank edges by the strongest activation among their endpoint chunks;
/// each winner carries that endpoint's provenance.
fn rank_edges_by_activation(
    chunk_activation: &[(String, f32, Provenance)],
    edges: Vec<CausalEntry>,
    limit: usize,
) -> Vec<(CausalEntry, Provenance)> {
    // HashMap lookup, not a linear scan per endpoint: a wide spread lights
    // up thousands of chunks and `edges_touching_chunks` returns thousands
    // of candidate edges — the old O(E×C) String comparisons pegged a CPU
    // core for minutes per query on the 32万-node LongMemEval store
    // (measured: ablation harness 60s+/question with zero I/O).
    let act: HashMap<&str, (f32, &Provenance)> = chunk_activation
        .iter()
        .map(|(c, a, p)| (c.as_str(), (a.abs(), p)))
        .collect();
    let mut scored: Vec<(f32, CausalEntry, Provenance)> = edges
        .into_iter()
        .map(|e| {
            let (strength, prov) = match (
                act.get(e.decision_id.as_str()),
                act.get(e.outcome_id.as_str()),
            ) {
                (Some(&(a, p)), Some(&(b, q))) => {
                    if a >= b {
                        (a, p)
                    } else {
                        (b, q)
                    }
                }
                (Some(&(a, p)), None) => (a, p),
                (None, Some(&(b, q))) => (b, q),
                (None, None) => (0.0, &Provenance::SEED),
            };
            (strength, e, prov.clone())
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .map(|(_, e, p)| (e, p))
        .take(limit)
        .collect()
}
