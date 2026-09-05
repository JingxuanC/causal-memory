//! The 16 memory operations, shared by all frontends. Each method mirrors
//! one MCP tool and returns the same text the tool would produce — agent
//! frameworks (MCP host, Python bindings) consume these strings directly.

use std::collections::HashMap;

use super::format::{
    format_entry_layered, format_fact_layered, format_lesson_layered, rrf_fuse_many,
    truncate_chars, TokenBudget,
};
use super::output::*;
use super::{
    block_on, Memory, CLOSED_WORLD_TAGS, INTERVENTION_MIN_SIMILARITY,
    SEMANTIC_CONTRADICTION_MIN_SIMILARITY,
};
use crate::store::{AgentFact, CausalEntry, ChainHop};

/// One structured retrieval hit — the machine-readable form of
/// `search_memory`'s output, so non-LLM frontends (the AMC leaderboard
/// contract: `{id, content, score, created_at}`) consume the same fused
/// result agents see as text. `score` is the RRF fused rank score; `rank`
/// is the 1-based fused position.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    /// Layer-namespaced key: `fact:{id}` or `causal:{edge_id}`.
    pub key: String,
    /// Human-readable content of the underlying memory.
    pub content: String,
    /// RRF fused score (higher = more relevant).
    pub score: f64,
    /// 1-based position in the fused ranking.
    pub rank: usize,
    pub created_at: Option<i64>,
}

/// Ranked retrieval results shared by both search paths: (rank, item)
/// pairs per layer (rank = fused position, or engine activation position
/// on the spread path) plus the mode tag, the explain map (Flip-path
/// marking: hit key → `[seed]` / `[spread hop=N via …]` tag; rendered only
/// when the caller passes explain=true, so default output is unchanged),
/// and the recall-audit metadata (seeds / hop summary) for the audit row
/// and the /debug/recall trace endpoint.
struct RankedHits {
    facts: Vec<(usize, AgentFact)>,
    causal: Vec<(usize, CausalEntry)>,
    mode: &'static str,
    explains: HashMap<String, String>,
    seeds: Vec<(String, &'static str)>,
    activated_nodes: usize,
    /// Hop distribution of the SURFACED hits (index = hop; surfaced hits
    /// only, not the whole lit set).
    hop_counts: Vec<usize>,
    max_hop: u8,
}

/// The dual-pool fallback's return shape (no provenance — every hit there
/// is a literal/semantic seed by construction).
type DualPoolHits = (
    Vec<(usize, AgentFact)>,
    Vec<(usize, CausalEntry)>,
    &'static str,
);

/// Multi-pass session-expansion budget (was a hardcoded 40).
const MULTI_PASS_SESSION_BUDGET: usize = 80;

/// Top-N density-weighted sessions kept by multi-pass expansion. 2 = the
/// LME-measured sweet spot (multi-session 133q: 55.6% → 59.4% at -40%
/// context); 0 disables the whitelist (all touched sessions share the
/// budget proportionally). Env-tunable for A/B:
/// CAUSAL_MEMORY_EXPAND_TOP_SESSIONS.
fn multi_pass_top_sessions() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CAUSAL_MEMORY_EXPAND_TOP_SESSIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2)
    })
}

impl Memory {
    /// `record_decision` — log a decision → outcome causal edge.
    /// `context` (v14, optional): short description of the situation the
    /// decision was made in — same task_tag+context becomes a comparable
    /// branch (fork) for counterfactual queries.
    pub fn record_decision(
        &self,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: &str,
        confidence_source: Option<&str>,
        context: Option<&str>,
    ) -> String {
        let confidence = match confidence_source {
            Some("temporal") => 0.4,
            Some("rule") => 0.7,
            Some("user_feedback") => 0.95,
            _ => 0.6, // llm_inferred (default)
        };
        let source = confidence_source.unwrap_or("llm_inferred");

        // Write-time outcome polarity (v4): LLM judge when configured,
        // otherwise the signal-word heuristic. Silent on any failure —
        // polarity must never block recording; legacy-style NULL would just
        // make readers fall back to the heuristic anyway.
        let polarity = judge_outcome_polarity(decision, outcome);

        let result = match self.store.record_decision_full(
            decision,
            outcome,
            relation,
            Some(task_tag),
            confidence,
            source,
            chrono::Utc::now().timestamp(),
            Some(&polarity),
            context,
        ) {
            Ok((_dec_id, edge_id)) => {
                // Phase C: patch the live graph so the new lesson is
                // visible to the very next query (no rebuild wait).
                if let Ok(Some(entry)) = self.store.get_edge(edge_id) {
                    self.patch_graph_new_edge(&entry);
                }
                // v14 prediction ledger: a recorded outcome for a decision
                // text that a pending counterfactual predicted resolves it.
                // Best-effort — resolution must never block recording.
                let resolved = self
                    .store
                    .resolve_predictions_for_decision(decision, Some(&polarity))
                    .unwrap_or(0);
                let ledger_note = if resolved > 0 {
                    format!(
                        "\n📐 Resolved {resolved} pending prediction(s) about this decision — see prediction_report."
                    )
                } else {
                    String::new()
                };
                // Opportunistically embed the new edge so semantic search
                // finds it. Silent on any failure — embedding must never
                // block recording; the `causal-memory embed` CLI backfills
                // anything missed. C3: record_decision_full now returns the
                // edge id directly (the old code re-queried it by from_id).
                let text = format!("{decision} {outcome}");
                if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(&text)) {
                    let _ = self.store.put_embedding(edge_id, "shared", &vec);
                    // Semantic contradiction scan: the exact-text path
                    // already ran inside record_decision; this catches
                    // paraphrased duplicates of the same decision.
                    // High threshold, silent on any error.
                    let _ = self.store.invalidate_semantic_contradictions(
                        decision,
                        outcome,
                        Some(&polarity),
                        &vec,
                        SEMANTIC_CONTRADICTION_MIN_SIMILARITY,
                    );
                }
                format!(
                    "✅ Recorded: [{}] {} →({})→ {} (confidence: {:.2}, id: {}){ledger_note}",
                    task_tag,
                    truncate_chars(decision, 60),
                    relation,
                    truncate_chars(outcome, 60),
                    confidence,
                    edge_id
                )
            }
            Err(e) => format!("❌ Failed to record: {e}"),
        };
        // After recording, rebuild the hippocampus graph so the new edge is
        // immediately available for spreading activation queries.
        self.mark_graph_dirty();
        result
    }

    /// `remember` — mem0-style auto-extraction. Agent feeds raw conversation
    /// text; the system's LLM automatically extracts facts, lessons, and
    /// causal edges (caused/enabled/prevented). This is the zero-friction
    /// alternative to `record_decision` — agent just dumps conversation text,
    /// system does the rest.
    pub fn remember(&self, messages: &str, date: Option<&str>) -> String {
        use crate::distill::{Distiller, ItemKind};

        // Parse the messages into turns
        let date = date.unwrap_or("");
        let date = if date.chars().count() >= 10 {
            date.chars().take(10).collect::<String>()
        } else {
            String::new()
        };

        // Split messages into turns — accept raw text with speaker: prefix,
        // or just treat as a single assistant message
        let turns: Vec<(String, String)> = messages
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                if let Some((speaker, rest)) = line.split_once(':') {
                    (speaker.trim().to_string(), rest.trim().to_string())
                } else {
                    ("user".to_string(), line.trim().to_string())
                }
            })
            .collect();

        if turns.is_empty() {
            return "❌ No messages to remember. Paste conversation text.".to_string();
        }

        // Try to run the distiller
        let distiller = match Distiller::from_env() {
            Some(d) => d,
            None => {
                // No LLM configured — fall back to raw recording
                let now = chrono::Utc::now().timestamp();
                let text = messages.chars().take(500).collect::<String>();
                let result = self.store.record_decision_at(
                    &text,
                    "remember (no LLM available, stored raw)",
                    "caused",
                    Some("remember"),
                    0.3,
                    "temporal",
                    now,
                );
                return match result {
                    Ok(_) => "✅ Stored raw (no LLM for extraction). Set CAUSAL_MEMORY_LLM_API for auto-extraction.".to_string(),
                    Err(e) => format!("❌ Failed: {e}"),
                };
            }
        };

        // Run distill synchronously (blocking the op call)
        let items = match block_on(distiller.distill_session(&date, &turns)) {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => return "ℹ️ Nothing worth remembering in this conversation.".to_string(),
            Err(e) => return format!("❌ Extraction failed: {e}"),
        };

        // Write extracted items to the store
        let mut facts = 0usize;
        let mut episodes = 0usize;
        let mut causal = 0usize;
        let mut summary = Vec::new();

        for item in &items {
            let kind_str = match item.kind {
                ItemKind::Fact => "fact",
                ItemKind::Preference => "preference",
                ItemKind::Lesson => "lesson",
                ItemKind::Event => "event",
                ItemKind::Causal => "causal",
            };
            summary.push(format!(
                "  [{kind_str}] {}",
                item.text.chars().take(80).collect::<String>()
            ));

            // Write to store based on kind
            if item.kind == ItemKind::Causal {
                let relation = item
                    .causal_relation
                    .as_ref()
                    .map(|r| r.as_str())
                    .unwrap_or("caused");
                let decision = item.decision.as_deref().unwrap_or("decision");
                let now = chrono::Utc::now().timestamp();
                let conf = match relation {
                    "caused" => 0.7,
                    "prevented" => 0.8,
                    "enabled" => 0.6,
                    _ => 0.5,
                };
                if self
                    .store
                    .record_decision_at(
                        decision,
                        &item.text,
                        relation,
                        Some("remember"),
                        conf,
                        "llm_inferred",
                        now,
                    )
                    .is_ok()
                {
                    causal += 1;
                }
            } else {
                // Fact/preference/lesson/event → agent_facts or causal_edges
                let key = match item.kind {
                    ItemKind::Fact => "fact",
                    ItemKind::Preference => "preference",
                    ItemKind::Lesson => "lesson",
                    _ => "event",
                };
                match self
                    .store
                    .record_fact(key, &item.text, "user", "remember", 0.8)
                {
                    Ok(_) => facts += 1,
                    Err(_) => {
                        // Fall back to causal edge with no_effect
                        let now = chrono::Utc::now().timestamp();
                        let _ = self.store.record_decision_at(
                            &item.text,
                            "(observed)",
                            "no_effect",
                            Some("remember"),
                            0.3,
                            "llm_inferred",
                            now,
                        );
                        episodes += 1;
                    }
                }
            }
        }

        // Reload graph
        self.mark_graph_dirty();

        format!(
            "✅ Extracted {} memories: {} facts, {} causal edges, {} episodes\n{}",
            items.len(),
            facts,
            causal,
            episodes,
            summary.join("\n")
        )
    }

    /// `search_causal` — BM25 + semantic retrieval of past causal episodes.
    /// `explain` (default false) appends a provenance tag per hit
    /// (Flip-path marking); default output is byte-identical to the
    /// pre-explain format.
    #[allow(clippy::too_many_arguments)]
    pub fn search_causal(
        &self,
        task_tag: Option<&str>,
        query: Option<&str>,
        limit: Option<usize>,
        detail_level: Option<&str>,
        max_tokens: Option<usize>,
        explain: Option<bool>,
    ) -> String {
        let limit = limit.unwrap_or(5);
        let detail_level = detail_level.unwrap_or("l2");
        let max_tokens = max_tokens.unwrap_or(0);
        let explain = explain.unwrap_or(false);
        let mut budget = TokenBudget::new(max_tokens);
        // Non-spread paths surface direct store hits — provenance-wise they
        // are seeds by definition.
        let seed_tag = if explain { "   ↳ [seed]\n" } else { "" };

        // ── Hippocampus path: spreading activation (联想检索) ──
        // The graph does associative retrieval: from seed matches, activation
        // spreads along causal edges to related memories that keyword search
        // would miss. Falls through to BM25/semantic if graph is unavailable
        // or finds nothing.
        if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
            if let Some(hippo_result) = self.hippocampus_search(
                query,
                task_tag,
                false,
                limit,
                detail_level,
                max_tokens,
                explain,
            ) {
                return hippo_result;
            }

            // ── Semantic path: embed + cosine ──
            // Requires a configured embedding endpoint; any failure falls back to
            // the BM25 path below.
            if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(query)) {
                let semantic = self
                    .store
                    .search_causal_semantic_entity_boosted(&vec, query, task_tag, limit)
                    .ok();
                if let Some(results) = semantic {
                    if results.is_empty() {
                        return "[semantic] 📭 No past causal episodes found matching your query."
                            .to_string();
                    }
                    let mut out = format!(
                        "[semantic/{detail_level}] Found {} past episode(s)",
                        results.len()
                    );
                    if max_tokens > 0 {
                        out.push_str(&format!(" (token budget: {max_tokens})"));
                    }
                    out.push_str(":\n\n");
                    for (i, (entry, _sim)) in results.iter().enumerate() {
                        let (line, cost) = format_entry_layered(entry, i + 1, detail_level);
                        if !budget.try_spend(cost) {
                            out.push_str(&format!(
                                "… {} more result(s) truncated (token budget)\n",
                                results.len() - i
                            ));
                            break;
                        }
                        if explain {
                            // Tag hugs its entry: the layered line ends in
                            // a blank separator, so trim + re-add it after
                            // the tag. (explain=false keeps the raw line.)
                            out.push_str(line.trim_end());
                            out.push('\n');
                            out.push_str(seed_tag);
                            out.push('\n');
                        } else {
                            out.push_str(&line);
                        }
                    }
                    return out;
                }
                // embed or semantic search failed — fall through to BM25.
            }

            // BM25 keyword path: query present but no usable embedder. Unlike
            // the old LIKE substring match, BM25 ranks by token overlap, so
            // word order and phrasing differences no longer zero out hits.
            let results = match self.store.search_causal_bm25(task_tag, query, limit) {
                Ok(r) => r,
                Err(e) => return format!("❌ Search failed: {e}"),
            };
            if results.is_empty() {
                return "[bm25] 📭 No past causal episodes found matching your query.".to_string();
            }
            let mut out = format!(
                "[bm25/{detail_level}] Found {} past episode(s)",
                results.len()
            );
            if max_tokens > 0 {
                out.push_str(&format!(" (token budget: {max_tokens})"));
            }
            out.push_str(":\n\n");
            for (i, entry) in results.iter().enumerate() {
                let (line, cost) = format_entry_layered(entry, i + 1, detail_level);
                if !budget.try_spend(cost) {
                    out.push_str(&format!(
                        "… {} more result(s) truncated (token budget)\n",
                        results.len() - i
                    ));
                    break;
                }
                if explain {
                    // Tag hugs its entry: the layered line ends in a blank
                    // separator, so trim + re-add it after the tag.
                    out.push_str(line.trim_end());
                    out.push('\n');
                    out.push_str(seed_tag);
                    out.push('\n');
                } else {
                    out.push_str(&line);
                }
            }
            return out;
        } // end hippocampus if let Some(query) block

        // Tag-only browsing (no query text) — original LIKE/listing path.
        let results = match self.store.search_causal(task_tag, query) {
            Ok(r) => r,
            Err(e) => return format!("❌ Search failed: {e}"),
        };

        if results.is_empty() {
            return "[keyword] 📭 No past causal episodes found matching your query.".to_string();
        }

        let count = results.len().min(limit);
        let mut out = format!(
            "[keyword] Found {} past episode(s) (showing {}):\n\n",
            results.len(),
            count
        );
        for (i, entry) in results.iter().take(limit).enumerate() {
            out.push_str(&format!(
                "{}. [{}] \"{}\"\n   →({})→ \"{}\"\n   confidence: {:.0}%\n",
                i + 1,
                entry.task_tag.as_deref().unwrap_or("untagged"),
                entry.decision_text,
                entry.relation,
                entry.outcome_text,
                entry.confidence * 100.0,
            ));
            if explain {
                out.push_str("   ↳ [seed]\n");
            }
            out.push('\n');
        }
        out
    }

    /// `record_fact` — record a flat fact (preference / tech stack / config).
    pub fn record_fact(
        &self,
        key: &str,
        value: &str,
        scope: Option<&str>,
        confidence: Option<f64>,
        replace_same_key: Option<bool>,
    ) -> String {
        let scope = scope.unwrap_or("user");
        if !matches!(scope, "user" | "session" | "agent") {
            return format!("❌ Invalid scope '{scope}' — use one of: user, session, agent");
        }
        let confidence = confidence.unwrap_or(0.8);

        // Optional same-key retirement runs atomically with the record
        // (single lock, single write batch) — no window where old and new
        // values are both valid.
        let (fact_id, retired) = if replace_same_key == Some(true) {
            match self
                .store
                .record_fact_replacing(key, value, scope, "agent", confidence)
            {
                Ok(v) => v,
                Err(e) => return format!("❌ Failed to record fact: {e}"),
            }
        } else {
            match self
                .store
                .record_fact(key, value, scope, "agent", confidence)
            {
                Ok(id) => (id, 0),
                Err(e) => return format!("❌ Failed to record fact: {e}"),
            }
        };

        // Phase C: patch the live graph (scope hub + fact node + entity
        // links); on replace, retire the superseded fact nodes too — the
        // old value stops seeding/surfacing immediately, not at the next
        // lazy rebuild.
        self.patch_graph_new_fact(fact_id, key, value, scope, confidence);
        if retired > 0 {
            self.patch_graph_retire_facts(fact_id);
        }

        // Opportunistic embedding (silent on any failure — must never block
        // recording; a CLI backfill path can catch up later).
        let text = format!("{} {}", key.replace('_', " "), value);
        if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(&text)) {
            let _ = self.store.put_fact_embedding(fact_id, "shared", &vec);
        }

        // Phase A: fact changes now reach the graph — mark it dirty so the
        // lazy rebuild picks the new fact node up (same contract as
        // record_decision / remember).
        self.mark_graph_dirty();

        let mut out = format!(
            "✅ Recorded fact: [{}] {} = \"{}\" (confidence: {:.2}, id: {})",
            scope,
            key,
            truncate_chars(value, 60),
            confidence.clamp(0.0, 1.0),
            fact_id
        );
        if retired > 0 {
            out.push_str(&format!(
                "\n🗑️ Retired {retired} outdated fact(s) under the same key."
            ));
        }
        out
    }

    /// `search_facts` — BM25 + semantic retrieval over the fact layer.
    pub fn search_facts(
        &self,
        query: Option<&str>,
        scope: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        let limit = limit.unwrap_or(5);
        if let Some(s) = scope {
            if !matches!(s, "user" | "session" | "agent") {
                return format!("❌ Invalid scope '{s}' — use one of: user, session, agent");
            }
        }

        if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
            // Semantic path: embed + cosine (requires a configured endpoint).
            if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(query)) {
                let semantic = self.store.search_facts_semantic(&vec, scope, limit).ok();
                // Only short-circuit on actual hits — an empty semantic
                // result (e.g. facts written without embeddings) falls
                // through to BM25 instead of falsely reporting "no facts".
                if let Some(results) = semantic.filter(|r| !r.is_empty()) {
                    let mut out = format!("[semantic] Found {} fact(s):\n\n", results.len());
                    for (i, (fact, sim)) in results.iter().enumerate() {
                        out.push_str(&format!(
                            "{}. [{}] {} = \"{}\"\n   similarity: {:.0}%, confidence: {:.0}%\n\n",
                            i + 1,
                            fact.scope,
                            fact.key,
                            fact.value,
                            sim * 100.0,
                            fact.confidence * 100.0,
                        ));
                    }
                    return out;
                }
                // embed failed or semantic found nothing — fall through to BM25.
            }

            // BM25 keyword path.
            let results = match self.store.search_facts_bm25(query, scope, limit) {
                Ok(r) => r,
                Err(e) => return format!("❌ Fact search failed: {e}"),
            };
            if results.is_empty() {
                return "[bm25] 📭 No facts found matching your query.".to_string();
            }
            let mut out = format!("[bm25] Found {} fact(s):\n\n", results.len());
            for (i, fact) in results.iter().enumerate() {
                out.push_str(&format!(
                    "{}. [{}] {} = \"{}\" (confidence: {:.0}%)\n",
                    i + 1,
                    fact.scope,
                    fact.key,
                    fact.value,
                    fact.confidence * 100.0,
                ));
            }
            return out;
        }

        // No query: most recently updated facts.
        let results = match self.store.list_facts(scope, limit) {
            Ok(r) => r,
            Err(e) => return format!("❌ Fact listing failed: {e}"),
        };
        if results.is_empty() {
            return "[list] 📭 No facts recorded yet.".to_string();
        }
        let mut out = format!("[list] {} most recent fact(s):\n\n", results.len());
        for (i, fact) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {} = \"{}\" (confidence: {:.0}%)\n",
                i + 1,
                fact.scope,
                fact.key,
                fact.value,
                fact.confidence * 100.0,
            ));
        }
        out
    }

    /// `remember_raw_turns` — pre-gatekeeping write path: raw conversation
    /// turns go straight into the retrieval pool (chunks) with adjacent-turn
    /// temporal edges. No LLM, no distillation — this is what `remember`
    /// does when no distiller is available, productized for backends (the
    /// AMC server's `--write-mode raw`) that need write-time LLM calls off
    /// the synchronous path. The full pipeline (`remember`) keeps
    /// write-time gatekeeping: LLM extraction is the sole path into the
    /// pool (0afb9f1) — choose deliberately.
    ///
    /// Returns the number of turns written.
    pub fn remember_raw_turns(&self, turns: &[(String, String)], session: &str) -> usize {
        let now = chrono::Utc::now().timestamp();
        let mut written = 0usize;
        for (idx, (speaker, text)) in turns.iter().enumerate() {
            let chunk_id = format!("raw:{session}:{idx}");
            let payload = format!("[{session}] {speaker}: {text}");
            let ok = self
                .store
                .with_conn(|c| {
                    use rusqlite::params;
                    c.execute(
                        "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                        params![&chunk_id, &payload, now],
                    )?;
                    if idx > 0 {
                        let prev_id = format!("raw:{session}:{}", idx - 1);
                        c.execute(
                            "INSERT OR IGNORE INTO causal_edges
                             (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                             VALUES (?1, ?2, 'no_effect', 0.4, 'temporal', ?3, ?3, NULL)",
                            params![&prev_id, &chunk_id, now],
                        )?;
                    }
                    Ok(())
                })
                .is_ok();
            if ok {
                written += 1;
            }
        }
        written
    }

    /// Structured core of `search_memory`: both layers (facts + causal),
    /// semantic/BM25 per-layer fallthrough, hop expansion, RRF fusion and
    /// top-`limit` truncation. The text tool and the AMC server both wrap
    /// this — one retrieval pipeline, two presentations. Display-only
    /// concerns (D4 intent routing) stay in the text wrapper. See
    /// [`MemoryHit`] for the hit shape.
    pub fn search_memory_entries(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: usize,
    ) -> (Vec<MemoryHit>, &'static str) {
        // Phase B: unified engine first — one seeding pass over ALL node
        // types, one spreading-activation run, typed hits. The dual-pool
        // RRF path is the fallback (and the regression control for A/B
        // comparison). Both paths produce the same ranked-pair shape, so
        // hit materialization is shared. Structured hits are explain-free
        // by contract (benchmarks consume content verbatim).
        let hits = self.ranked_hits(query, task_tag, scope, limit);
        let mode = hits.mode;
        (hits_from_ranked(&hits.facts, &hits.causal), mode)
    }

    /// Observability (/debug/recall): run one recall and return the full
    /// trace as JSON — seeds with sources, hop summary, and per-result
    /// provenance tags. Read-only; the audit row is written as for any
    /// recall.
    pub fn recall_trace(&self, query: &str, limit: usize) -> serde_json::Value {
        let t0 = std::time::Instant::now();
        let hits = self.ranked_hits(query, None, None, limit);
        let structured = hits_from_ranked(&hits.facts, &hits.causal);
        serde_json::json!({
            "query": query,
            "mode": hits.mode,
            "latency_ms": t0.elapsed().as_secs_f64() * 1000.0,
            "seeds": hits.seeds.iter().map(|(id, s)| serde_json::json!({"id": id, "source": s})).collect::<Vec<_>>(),
            "activated_nodes": hits.activated_nodes,
            "max_hop": hits.max_hop,
            "hop_counts_surfaced": hits.hop_counts,
            "results": structured.iter().map(|h| serde_json::json!({
                "key": h.key,
                "rank": h.rank,
                "explain": hits.explains.get(&h.key),
                "content": h.content,
            })).collect::<Vec<_>>(),
        })
    }

    /// `search_memory` — unified retrieval: facts + causal lessons fused by
    /// Reciprocal Rank Fusion (RRF) in one call. `detail_level` (l0/l1/l2,
    /// default l2) picks the per-item verbosity; `max_tokens` (default 0 =
    /// unlimited) truncates the rendered pool against a shared token budget;
    /// `explain` (default false) appends a provenance tag per hit — default
    /// output is byte-identical to the pre-explain format.
    #[allow(clippy::too_many_arguments)]
    pub fn search_memory(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: Option<usize>,
        detail_level: Option<&str>,
        max_tokens: Option<usize>,
        explain: Option<bool>,
    ) -> String {
        let limit = limit.unwrap_or(10);
        if let Some(s) = scope {
            if !matches!(s, "user" | "session" | "agent") {
                return format!("❌ Invalid scope '{s}' — use one of: user, session, agent");
            }
        }
        let detail_level = detail_level.unwrap_or("l2");
        if !matches!(detail_level, "l0" | "l1" | "l2") {
            return format!("❌ Invalid detail_level '{detail_level}' — use one of: l0, l1, l2");
        }

        // Phase B: unified engine first; the dual-pool RRF path stays as
        // the fallback and the regression control for A/B comparison.
        // Both produce the same ranked-pair shape, so D4 routing and the
        // grouped display are shared.
        let hits = self.ranked_hits(query, task_tag, scope, limit);
        render_unified(
            query,
            &hits.facts,
            &hits.causal,
            hits.mode,
            detail_level,
            max_tokens.unwrap_or(0),
            if explain.unwrap_or(false) {
                Some(&hits.explains)
            } else {
                None
            },
        )
    }

    /// One retrieval story, two presentations: the unified spread engine
    /// when it can serve the query, the dual-pool RRF path otherwise.
    /// Returns facts and causal entries as (rank, item) pairs — facts in
    /// activation/fused order, causal likewise — plus the mode tag and the
    /// per-hit explain map. Every call is a recall: record metrics and a
    /// best-effort audit row (audit failures never break retrieval).
    fn ranked_hits(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: usize,
    ) -> RankedHits {
        let t0 = std::time::Instant::now();
        let out = if let Some(spread) = self.unified_spread_hits(query, task_tag, scope, limit) {
            let mut explains = HashMap::new();
            let mut hop_counts = vec![0usize; spread.max_hop as usize + 1];
            let base = spread.facts.len();
            let facts: Vec<(usize, AgentFact)> = spread
                .facts
                .into_iter()
                .enumerate()
                .map(|(i, (f, p))| {
                    let key = format!("fact:{}", f.id);
                    explains.insert(key, p.tag());
                    hop_counts[p.hop as usize] += 1;
                    let source = if p.hop == 0 { "seed" } else { "spread" };
                    crate::observability::metrics().record_recall_result("facts", source);
                    (i + 1, f)
                })
                .collect();
            let causal: Vec<(usize, CausalEntry)> = spread
                .causal
                .into_iter()
                .enumerate()
                .map(|(i, (e, p))| {
                    let key = format!("causal:{}", e.edge_id);
                    explains.insert(key, p.tag());
                    hop_counts[p.hop as usize] += 1;
                    let source = if p.hop == 0 { "seed" } else { "spread" };
                    crate::observability::metrics().record_recall_result("causal", source);
                    (base + i + 1, e)
                })
                .collect();
            crate::observability::metrics().record_activated_nodes(spread.activated_nodes);
            RankedHits {
                facts,
                causal,
                mode: "spread",
                explains,
                seeds: spread.seeds,
                activated_nodes: spread.activated_nodes,
                hop_counts,
                max_hop: spread.max_hop,
            }
        } else {
            let (facts, causal, mode) = self.dual_pool_fused(query, task_tag, scope, limit);
            // Direct store hits: every result is a literal/semantic seed.
            let mut explains = HashMap::new();
            for (_, f) in &facts {
                explains.insert(format!("fact:{}", f.id), "[seed]".to_string());
                crate::observability::metrics().record_recall_result("facts", "seed");
            }
            for (_, e) in &causal {
                explains.insert(format!("causal:{}", e.edge_id), "[seed]".to_string());
                crate::observability::metrics().record_recall_result("causal", "seed");
            }
            let n = facts.len() + causal.len();
            RankedHits {
                facts,
                causal,
                mode,
                explains,
                seeds: Vec::new(),
                activated_nodes: 0,
                hop_counts: vec![n],
                max_hop: 0,
            }
        };
        self.write_recall_audit(
            query,
            task_tag,
            out.mode,
            &out.seeds,
            out.activated_nodes,
            out.max_hop,
            &out.explains,
            out.facts.len() + out.causal.len(),
            t0.elapsed().as_secs_f64() * 1000.0,
        );
        out
    }

    /// Best-effort recall audit (v13 `recall_audit` table): one row per
    /// recall with seeds, hop summary and per-result explain tags. A write
    /// failure increments a metrics counter and logs a warning — retrieval
    /// is NEVER affected.
    #[allow(clippy::too_many_arguments)]
    fn write_recall_audit(
        &self,
        query: &str,
        task_tag: Option<&str>,
        mode: &str,
        seeds: &[(String, &'static str)],
        activated_nodes: usize,
        max_hop: u8,
        explains: &HashMap<String, String>,
        result_count: usize,
        latency_ms: f64,
    ) {
        let seeds_json = serde_json::to_string(
            &seeds
                .iter()
                .map(|(id, s)| serde_json::json!({ "id": id, "source": s }))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".to_string());
        let mut result_keys: Vec<String> = explains.keys().cloned().collect();
        result_keys.sort();
        let results_json = serde_json::to_string(
            &result_keys
                .iter()
                .map(|k| serde_json::json!({ "key": k, "explain": explains[k] }))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".to_string());
        let row = crate::store::RecallAuditRow {
            query,
            task_tag,
            server: self.server_label(),
            mode,
            seeds_json: &seeds_json,
            activated_nodes,
            max_hop,
            results_json: &results_json,
            latency_ms,
            result_count,
        };
        if let Err(e) = self.store.insert_recall_audit(&row) {
            crate::observability::metrics().record_audit_error();
            tracing::warn!("recall audit write failed (recall unaffected): {e}");
        }
    }

    /// The dual-pool RRF fallback: per-layer retrieval (semantic → BM25
    /// fallthrough per layer), A2 hop expansion, RRF fusion, fused
    /// top-`limit` as ranked pairs — facts and causal entries each carry
    /// their fused rank.
    fn dual_pool_fused(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: usize,
    ) -> DualPoolHits {
        // Pull more than needed per layer so the fusion has real candidates.
        let per_layer = limit.saturating_mul(2).max(10);

        // Same retrieval discipline as the single-layer tools: semantic when
        // an embedder is configured, BM25 otherwise. One query embedding
        // serves both layers. Per-layer fallthrough: an empty/failed
        // semantic result (e.g. records stored without embeddings) degrades
        // that layer to BM25 instead of silently missing hits.
        let query_vec = block_on(crate::embed::embed_shared(query)).and_then(|r| r.ok());

        let mut used_semantic = false;
        let facts: Vec<AgentFact> = match &query_vec {
            Some(v) => {
                let sem: Vec<AgentFact> = self
                    .store
                    .search_facts_semantic(v, scope, per_layer)
                    .map(|hits| hits.into_iter().map(|(f, _)| f).collect())
                    .unwrap_or_default();
                if sem.is_empty() {
                    self.store
                        .search_facts_bm25(query, scope, per_layer)
                        .unwrap_or_default()
                } else {
                    used_semantic = true;
                    sem
                }
            }
            None => self
                .store
                .search_facts_bm25(query, scope, per_layer)
                .unwrap_or_default(),
        };
        let causal: Vec<CausalEntry> = match &query_vec {
            Some(v) => {
                let sem: Vec<CausalEntry> = self
                    .store
                    .search_causal_semantic_entity_boosted(v, query, task_tag, per_layer)
                    .map(|hits| hits.into_iter().map(|(e, _)| e).collect())
                    .unwrap_or_default();
                if sem.is_empty() {
                    self.store
                        .search_causal_bm25(task_tag, query, per_layer)
                        .unwrap_or_default()
                } else {
                    used_semantic = true;
                    sem
                }
            }
            None => self
                .store
                .search_causal_bm25(task_tag, query, per_layer)
                .unwrap_or_default(),
        };
        let mode = if used_semantic { "semantic" } else { "bm25" };

        if facts.is_empty() && causal.is_empty() {
            return (Vec::new(), Vec::new(), mode);
        }

        // A2: hop expansion from the causal seeds — 1-hop adjacency + 2-hop
        // distilled causal jumps. System-side, deterministic, LLM-free
        // multi-hop recall (new edges join the fused pool).
        let mut causal_by_id: HashMap<i64, CausalEntry> = HashMap::new();
        for e in &causal {
            causal_by_id.insert(e.edge_id, e.clone());
        }
        let seed_ids: Vec<i64> = causal.iter().map(|e| e.edge_id).collect();
        let hop = self
            .store
            .search_causal_hop(query, &seed_ids, per_layer)
            .unwrap_or_default();
        let mut hop_keys: Vec<String> = Vec::new();
        for e in &hop {
            if let std::collections::hash_map::Entry::Vacant(entry) = causal_by_id.entry(e.edge_id)
            {
                entry.insert(e.clone());
                hop_keys.push(format!("causal:{}", e.edge_id));
            }
        }

        // RRF fusion over layer-prefixed keys. Keys are namespaced per
        // layer (a fact and a causal edge never share a key), so fusion is
        // rank-interleaving: each item scores by its own layer's rank.
        let fact_keys: Vec<String> = facts.iter().map(|f| format!("fact:{}", f.id)).collect();
        let causal_keys: Vec<String> = causal
            .iter()
            .map(|e| format!("causal:{}", e.edge_id))
            .collect();
        let fused = rrf_fuse_many(&[&fact_keys, &causal_keys, &hop_keys]);
        let rank_of: HashMap<&str, usize> = fused
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (k.as_str(), i + 1))
            .collect();

        // Keep only items inside the fused top-`limit`, as ranked pairs.
        let keep = |key: &str| rank_of.get(key).is_some_and(|r| *r <= limit);
        let facts_kept: Vec<(usize, AgentFact)> = facts
            .into_iter()
            .filter(|f| keep(&format!("fact:{}", f.id)))
            .map(|f| (rank_of[format!("fact:{}", f.id).as_str()], f))
            .collect();
        // Materialize from the merged causal pool (primary + hop
        // neighbors), in fused-rank order.
        let mut causal_kept: Vec<(usize, CausalEntry)> = causal_by_id
            .into_values()
            .filter(|e| keep(&format!("causal:{}", e.edge_id)))
            .map(|e| (rank_of[format!("causal:{}", e.edge_id).as_str()], e))
            .collect();
        causal_kept.sort_by_key(|(r, _)| *r);

        (facts_kept, causal_kept, mode)
    }

    /// Step A (multi-session retrieval design, docs/design/): multi-pass
    /// retrieval path for cross-session questions — query decomposition
    /// (content entities + temporal anchor), one BM25 per entity, time-window
    /// weighting, and full-coverage session expansion for aggregation shapes.
    /// Deterministic and LLM-free; the verification loop lives at callers.
    /// This is the lib-level capability behind the harness's type-agnostic
    /// retrieve(); default-off behind its own method (search_memory keeps the
    /// single-pass contract).
    pub fn search_memory_multi_pass(
        &self,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        let _ = scope;
        let limit = limit.unwrap_or(10);
        let per_layer = limit.saturating_mul(2).max(10);
        let plan = crate::retrieval::plan_query(query, chrono::Utc::now().timestamp());
        let entries = match crate::retrieval::retrieve_multi_pass(
            &self.store,
            task_tag,
            query,
            &plan,
            per_layer,
        ) {
            Ok(e) => e,
            Err(err) => return format!("❌ Multi-pass retrieval failed: {err}"),
        };
        // Distill episodes are BM25-favored paraphrases that crowd original
        // turns out of top-k (retrieval-scoring.md §4): cap them at a third
        // of the budget so primary evidence keeps its slots.
        let entries = crate::retrieval::apply_episode_quota(entries, per_layer / 3);
        if entries.is_empty() {
            return "[multi-pass] 📭 No memories found matching your query.".to_string();
        }
        let mut out = format!(
            "[multi-pass] {} causal edge(s){}:\n",
            entries.len(),
            if plan.time_window.is_some() {
                " (time-anchored)"
            } else {
                ""
            }
        );
        for (i, e) in entries.iter().enumerate() {
            out.push_str(&format!(
                "  {}. \"{}\" →({})→ \"{}\" (confidence: {:.0}%)\n",
                i + 1,
                truncate_chars(&e.decision_text, 50),
                e.relation,
                truncate_chars(&e.outcome_text, 50),
                e.confidence * 100.0,
            ));
        }
        // Aggregation shapes: append full session context (chunks not already
        // covered by a retrieved edge) so a complete evidence set is visible.
        // Density-weighted top-N whitelist (the LME dilution cut, +3.8pp /
        // -40% tokens measured on multi-session 133q): only the highest-
        // value sessions expand at all, budget split proportionally by
        // hit_count × query-token overlap. Top-2 default from the measured
        // sweet spot; 0 disables (all touched sessions, proportional).
        if plan.aggregation {
            let ids: Vec<String> = entries
                .iter()
                .flat_map(|e| [e.decision_id.clone(), e.outcome_id.clone()])
                .collect();
            if let Ok(chunks) = crate::retrieval::expand_session_chunks_weighted(
                &self.store,
                &ids,
                query,
                MULTI_PASS_SESSION_BUDGET,
                multi_pass_top_sessions(),
            ) {
                let covered: std::collections::HashSet<String> = ids.into_iter().collect();
                let extra: Vec<&(String, String)> = chunks
                    .iter()
                    .filter(|(id, _)| !covered.contains(id))
                    .collect();
                if !extra.is_empty() {
                    out.push_str(&format!(
                        "\nFull session context ({} additional turn(s)):\n",
                        extra.len()
                    ));
                    for (_, text) in extra.iter().take(20) {
                        out.push_str(&format!("  - {}\n", truncate_chars(text, 100)));
                    }
                }
            }
        }
        out
    }

    /// `trace_cause` — single-hop reverse: which decision caused this outcome.
    pub fn trace_cause(&self, outcome_description: &str) -> String {
        // ── Hippocampus path: reverse spreading activation ──
        // Walk backward from the outcome through the causal graph to find
        // which decisions could have caused it. Activation spreads along
        // reverse causal edges, surfacing decisions that keyword search
        // would miss (e.g., a decision phrased differently from the query).
        if let Some(hippo_result) =
            self.hippocampus_search(outcome_description, None, true, 5, "l1", 0, false)
        {
            return hippo_result;
        }

        // ── SQL fallback: single-hop reverse lookup ──
        match self.store.trace_cause(outcome_description) {
            Ok(results) if results.is_empty() => {
                "📭 No past decisions found that match this outcome.".to_string()
            }
            Ok(results) => {
                let mut out = format!("Traced {} possible cause(s):\n\n", results.len());
                for (i, entry) in results.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. \"{}\"\n   →({})→ \"{}\"\n   confidence: {:.0}%\n\n",
                        i + 1,
                        entry.decision_text,
                        entry.relation,
                        entry.outcome_text,
                        entry.confidence * 100.0,
                    ));
                }
                out
            }
            Err(e) => format!("❌ Trace failed: {e}"),
        }
    }

    /// `trace_cause_chain` — multi-hop backward traversal through the graph.
    pub fn trace_cause_chain(
        &self,
        outcome_description: &str,
        max_depth: Option<usize>,
        min_confidence: Option<f64>,
        limit: Option<usize>,
    ) -> String {
        let max_depth = max_depth.unwrap_or(3);
        let min_confidence = min_confidence.unwrap_or(0.5);
        let limit = limit.unwrap_or(5);

        let chains =
            match self
                .store
                .trace_cause_chain(outcome_description, max_depth, min_confidence)
            {
                Ok(c) => c,
                Err(e) => return format!("❌ Chain trace failed: {e}"),
            };

        if chains.is_empty() {
            return "📭 No multi-hop causal chains found. Try widening max_depth or lowering min_confidence.".to_string();
        }

        let show = chains.len().min(limit);
        let mut out = format!(
            "Found {} causal chain(s) (showing {}, max_depth={}, min_conf={}):\n\n",
            chains.len(),
            show,
            max_depth,
            min_confidence
        );

        for (i, chain) in chains.iter().take(limit).enumerate() {
            out.push_str(&format!(
                "Chain {} (chain confidence: {:.0}%):\n",
                i + 1,
                chain
                    .last()
                    .map(|h| h.chain_confidence * 100.0)
                    .unwrap_or(0.0)
            ));
            for hop in chain {
                out.push_str(&format!(
                    "  hop {}: \"{}\"\n         →({})→ \"{}\"\n         edge confidence: {:.0}%\n",
                    hop.hop,
                    hop.decision_text,
                    hop.relation,
                    hop.outcome_text,
                    hop.confidence * 100.0,
                ));
            }
            out.push('\n');
        }
        out
    }

    /// `resolve_updates` — C7 knowledge-update pass: scan repeated decisions
    /// whose later outcomes diverged, LLM-judge whether the new evidence
    /// falsifies the old lesson, and supersede accordingly (annotate = mark
    /// `superseded_by`, old lesson stays retrievable with its correction).
    ///
    /// Preview by default (`apply: false` counts without writing) — same
    /// discipline as the `resolve-updates` CLI. This is the same pipeline
    /// sleep runs as stage 1.7; exposing it as a tool means an agent can
    /// trigger a knowledge-update pass right after recording contradicting
    /// evidence instead of waiting for the next sleep cycle.
    pub fn resolve_updates(&self, limit: Option<usize>, apply: bool) -> String {
        let Some(llm) = crate::llm::LlmConfig::from_env() else {
            return "⚠ No LLM configured (set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY). \
                    Only the rule-based contradiction pass on the write path runs; \
                    there is nothing for this tool to judge."
                .into();
        };
        let mut config = crate::consolidate::ConsolidateConfig::default();
        if let Some(l) = limit {
            config.supersession_limit = l;
        }
        let mut report = crate::consolidate::ConsolidateReport::default();
        match crate::consolidate::resolve_supersessions_with(
            &self.store,
            &config,
            !apply, // dry_run = preview mode
            &mut report,
            Some(&llm),
            config.supersession_action,
        ) {
            Ok(()) => {
                if report.superseded_lessons == 0 {
                    return format!(
                        "✓ No falsifications found: scanned repeated-decision pairs \
                         (limit {}) with the LLM judge; every old lesson held up. \
                         Nothing {}.",
                        config.supersession_limit,
                        if apply { "was applied" } else { "to apply" },
                    );
                }
                format!(
                    "{} {} lesson(s){} — superseded edges {}.\n\
                     Re-run with apply=true to write, or `causal-memory sleep` folds \
                     this into the next consolidation cycle.",
                    if apply {
                        "✓ Superseded"
                    } else {
                        "👀 Would supersede"
                    },
                    report.superseded_lessons,
                    if apply {
                        ""
                    } else {
                        " (preview — nothing written)"
                    },
                    match config.supersession_action {
                        crate::consolidate::SupersessionAction::Retire => {
                            "exit retrieval (valid_to set)"
                        }
                        crate::consolidate::SupersessionAction::Annotate => {
                            "stay retrievable, annotated with their correction"
                        }
                    },
                )
            }
            Err(e) => format!("❌ resolve_updates failed: {e}"),
        }
    }

    /// `invalidate_decision` — soft-invalidate a wrong lesson (kept for audit).
    pub fn invalidate_decision(&self, edge_id: i64, reason: Option<&str>) -> String {
        let edge = match self.store.get_edge(edge_id) {
            Ok(Some(e)) => e,
            Ok(None) => return format!("❌ Edge #{edge_id} not found."),
            Err(e) => return format!("❌ Lookup failed: {e}"),
        };

        if edge.valid_to.is_some() {
            return format!(
                "❌ Edge #{} was already invalidated: \"{}\" →({})→ \"{}\"",
                edge_id, edge.decision_text, edge.relation, edge.outcome_text,
            );
        }

        match self.store.invalidate_edge(edge_id) {
            Ok(true) => {
                // Phase C: the falsified lesson stops spreading immediately
                // (O(deg) flip) instead of at the next lazy rebuild.
                if let Ok(mut guard) = self.graph.lock() {
                    if let Some(graph) = guard.as_mut() {
                        graph.invalidate_edges_between(&edge.decision_id, &edge.outcome_id);
                    }
                }
                let reason = reason
                    .map(|r| format!(" (reason: {r})"))
                    .unwrap_or_default();
                format!(
                    "✅ Invalidated edge #{}: \"{}\" →({})→ \"{}\"{reason}. It will no longer appear in search/trace results, but is kept for audit.",
                    edge_id, edge.decision_text, edge.relation, edge.outcome_text,
                )
            }
            Ok(false) => format!("❌ Edge #{edge_id} could not be invalidated."),
            Err(e) => format!("❌ Invalidate failed: {e}"),
        }
    }

    /// `invalidate_pattern` — soft-invalidate a mined cross-task pattern
    /// (meta-causal edge, the id shown as `#N` in `search_patterns`).
    /// Meta edges are mine-able during sleep consolidation; this is the
    /// revoking half (roadmap). Idempotent.
    pub fn invalidate_pattern(&self, edge_id: i64, reason: Option<&str>) -> String {
        use rusqlite::OptionalExtension;
        struct MetaRow {
            from_id: String,
            to_id: String,
            relation: String,
            from_text: String,
            to_text: String,
            valid_to: Option<i64>,
        }
        let meta = self.store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT m.from_id, m.to_id, m.relation, cf.text, ct.text, m.valid_to
                     FROM meta_causal_edges m
                     JOIN chunks cf ON cf.id = m.from_id
                     JOIN chunks ct ON ct.id = m.to_id
                     WHERE m.id = ?1",
                rusqlite::params![edge_id],
                |r| {
                    Ok(MetaRow {
                        from_id: r.get(0)?,
                        to_id: r.get(1)?,
                        relation: r.get(2)?,
                        from_text: r.get(3)?,
                        to_text: r.get(4)?,
                        valid_to: r.get(5)?,
                    })
                },
            )
            .optional()?)
        });
        let meta = match meta {
            Ok(Some(m)) => m,
            Ok(None) => return format!("❌ Pattern edge #{edge_id} not found."),
            Err(e) => return format!("❌ Lookup failed: {e}"),
        };
        if meta.valid_to.is_some() {
            return format!(
                "❌ Pattern edge #{} was already invalidated: \"{}\" --[{}]--> \"{}\"",
                edge_id, meta.from_text, meta.relation, meta.to_text,
            );
        }

        match self.store.invalidate_meta_edge(edge_id) {
            Ok(true) => {
                // The revoked pattern stops spreading immediately (O(deg)
                // flip) instead of at the next lazy rebuild — same
                // contract as invalidate_decision.
                if let Ok(mut guard) = self.graph.lock() {
                    if let Some(graph) = guard.as_mut() {
                        graph.invalidate_edges_between(&meta.from_id, &meta.to_id);
                    }
                }
                let reason = reason
                    .map(|r| format!(" (reason: {r})"))
                    .unwrap_or_default();
                format!(
                    "✅ Invalidated pattern edge #{}: \"{}\" --[{}]--> \"{}\"{reason}. It will no longer appear in search_patterns or spreading activation, but is kept for audit.",
                    edge_id, meta.from_text, meta.relation, meta.to_text,
                )
            }
            Ok(false) => format!("❌ Pattern edge #{edge_id} could not be invalidated."),
            Err(e) => format!("❌ Invalidate failed: {e}"),
        }
    }

    /// `search_patterns` — mined cross-task meta edges (similar_to /
    /// repeated / contradicts / refines).
    pub fn search_patterns(
        &self,
        query: Option<&str>,
        task_tag: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        let limit = limit.unwrap_or(10);

        let results = match self.store.search_patterns(query, task_tag, limit) {
            Ok(r) => r,
            Err(e) => return format!("❌ Pattern search failed: {e}"),
        };

        if results.is_empty() {
            return "📭 No cross-task patterns found matching your query.".to_string();
        }

        let mut out = format!("Found {} cross-task pattern(s):\n\n", results.len());
        for (i, edge) in results.iter().enumerate() {
            let label = match edge.relation.as_str() {
                "similar_to" => "🔗 similar_to",
                "repeated" => "🔁 repeated",
                "contradicts" => "⚡ contradicts",
                "refines" => "🔧 refines",
                other => other,
            };
            let pattern = edge.pattern.as_deref().unwrap_or("");
            out.push_str(&format!(
                "{}. \"{}\" --[{label}]--> \"{}\" (#{})\n   {pattern}\n   confidence: {:.0}%\n",
                i + 1,
                edge.from_text,
                edge.to_text,
                edge.id,
                edge.confidence * 100.0,
            ));
            // v5 stratified-replication verdicts (NULL = untested → no note).
            if edge.confounded == Some(true) {
                let strata = edge
                    .strata
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                    .map(|v| v.join(", "))
                    .unwrap_or_else(|| "?".into());
                out.push_str(&format!(
                    "   ⚠️ confounded: 仅见于 task_tag={strata}，可能是该领域特有\n"
                ));
            }
            if edge.simpson == Some(true) {
                out.push_str(
                    "   ⚠️ Simpson warning: 该模式在不同 task_tag 分层中 outcome 方向相反\n",
                );
            }
            out.push('\n');
        }
        out
    }

    /// `causal_directory` — L0 compact pointer list of recent decisions.
    pub fn causal_directory(&self, limit: Option<usize>) -> String {
        let limit = limit.unwrap_or(20);
        let body = self.recent_decisions_directory(limit);
        if body.is_empty() {
            return "📭 No decisions recorded yet — the causal memory directory is empty."
                .to_string();
        }
        format!(
            "{body}\nUse trace_cause/search_causal/intervention_query with these decision texts for details.\n"
        )
    }

    /// `intervention_query` — Pearl Rung-2 forward prediction with
    /// stratified summaries and prevented-edge warnings.
    pub fn intervention_query(
        &self,
        action: &str,
        task_tag: Option<&str>,
        max_depth: Option<usize>,
        limit: Option<usize>,
    ) -> String {
        let max_depth = max_depth.unwrap_or(3);
        let limit = limit.unwrap_or(5);
        // Internal pruning floor: lower than trace_cause_chain's 0.5 default
        // because forward chains multiply confidence per hop and would prune
        // away realistic 2-3 hop predictions at 0.5.
        let min_confidence = 0.3;

        // Semantic seed path: embed the action, find similar past decisions by
        // cosine, walk forward chains from them. Failure chain: BM25 ranks
        // similar decisions by token overlap and seeds chains from them; the
        // LIKE anchor is the last resort (pre-embedding behavior).
        let mut tag = "[keyword]";
        let chains = match self.semantic_effect_chains(action, max_depth, min_confidence, limit) {
            Some(c) => {
                tag = "[semantic]";
                c
            }
            None => {
                let bm25_seeds = self
                    .store
                    .search_causal_bm25(None, action, limit)
                    .ok()
                    .filter(|r| !r.is_empty());
                match bm25_seeds {
                    Some(entries) => {
                        tag = "[bm25]";
                        let ids: Vec<String> =
                            entries.iter().map(|e| e.decision_id.clone()).collect();
                        match self.store.trace_effect_chain_from_ids(
                            &ids,
                            max_depth,
                            min_confidence,
                        ) {
                            Ok(c) => c,
                            Err(e) => return format!("❌ Intervention query failed: {e}"),
                        }
                    }
                    None => match self
                        .store
                        .trace_effect_chain(action, max_depth, min_confidence)
                    {
                        Ok(c) => c,
                        Err(e) => return format!("❌ Intervention query failed: {e}"),
                    },
                }
            }
        };

        if chains.is_empty() {
            return format!(
                "{tag} 📭 No precedent found for \"{}\" — absence of evidence is not evidence of safety. Proceed with caution, and record the outcome afterward with record_decision.",
                action
            );
        }

        // Stratified adjustment (engineering backdoor check): tag each chain
        // with its anchor edge's task_tag and its terminal outcome bucket,
        // then compare the reference stratum against the pooled evidence.
        // The optional task_tag param pins the reference stratum AND filters
        // the displayed chain list to it; the summary always sees all chains.
        // Chain traversal above is untouched — this is aggregation only.
        // C4: resolve every chain's anchor edge in ONE batch query instead
        // of a get_edge per chain.
        let anchor_ids: Vec<i64> = chains
            .iter()
            .filter_map(|c| c.first().map(|h| h.edge_id))
            .collect();
        let anchors = self.store.get_edges_batch(&anchor_ids).unwrap_or_default();
        let tag_of: std::collections::HashMap<i64, String> = anchors
            .into_iter()
            .filter_map(|e| e.task_tag.map(|t| (e.edge_id, t)))
            .collect();
        let pooled: Vec<(Option<String>, &'static str)> = chains
            .iter()
            .map(|c| {
                (
                    c.first().and_then(|h| tag_of.get(&h.edge_id).cloned()),
                    c.last().map(terminal_bucket).unwrap_or("neutral"),
                )
            })
            .collect();
        let reference = task_tag
            .map(str::to_string)
            .or_else(|| modal_stratum(&pooled));
        let summary = stratified_summary(&pooled, reference.as_deref());
        let display: Vec<&Vec<ChainHop>> = match task_tag {
            Some(t) => chains
                .iter()
                .zip(&pooled)
                .filter(|(_, (tag, _))| tag.as_deref() == Some(t))
                .map(|(c, _)| c)
                .collect(),
            None => chains.iter().collect(),
        };

        let show = display.len().min(limit);
        let mut out = format!(
            "{tag} Predicted effect(s) of \"{}\" — {} chain(s) (showing {}, max_depth={}):\n\n",
            action,
            display.len(),
            show,
            max_depth
        );
        if display.is_empty() {
            out.push_str(&format!(
                "(no chains within task_tag={} — see the stratified summary below)\n\n",
                task_tag.unwrap_or("?")
            ));
        }

        for (i, chain) in display.iter().take(limit).enumerate() {
            let terminal = chain.last();
            let has_prevented = chain.iter().any(|h| h.relation == "prevented");
            let label = chain_label(
                terminal.and_then(|h| h.outcome_polarity.as_deref()),
                terminal.map(|h| h.outcome_text.as_str()).unwrap_or(""),
                has_prevented,
            );
            out.push_str(&format!(
                "Chain {} {} (chain confidence: {:.0}%):\n",
                i + 1,
                label,
                terminal.map(|h| h.chain_confidence * 100.0).unwrap_or(0.0)
            ));
            for hop in chain.iter() {
                out.push_str(&format!(
                    "  hop {}: \"{}\"\n         →({})→ \"{}\"\n         edge confidence: {:.0}%\n",
                    hop.hop,
                    hop.decision_text,
                    hop.relation,
                    hop.outcome_text,
                    hop.confidence * 100.0,
                ));
            }
            out.push('\n');
        }
        out.push_str(&summary);
        out
    }

    /// Semantic seed path for intervention_query: embed the action, find
    /// similar past decisions (cosine >= INTERVENTION_MIN_SIMILARITY), then
    /// walk forward effect chains from those decisions. Returns None when
    /// embeddings are unavailable/fail or no similar decision exists — the
    /// caller then falls back to the LIKE path.
    fn semantic_effect_chains(
        &self,
        action: &str,
        max_depth: usize,
        min_confidence: f64,
        limit: usize,
    ) -> Option<Vec<Vec<ChainHop>>> {
        let Some(Ok(vec)) = block_on(crate::embed::embed_shared(action)) else {
            return None;
        };
        let seeds = self
            .store
            .similar_decision_edges(&vec, limit, INTERVENTION_MIN_SIMILARITY)
            .ok()?;
        if seeds.is_empty() {
            return None;
        }
        let decision_ids: Vec<String> = seeds.iter().map(|(e, _)| e.decision_id.clone()).collect();
        self.store
            .trace_effect_chain_from_ids(&decision_ids, max_depth, min_confidence)
            .ok()
    }

    /// `counterfactual_query` — contrastive (empirical) counterfactual over
    /// recorded alternatives (NOT a Pearl Rung-3 SCM counterfactual).
    pub fn counterfactual_query(
        &self,
        decision: &str,
        alternative: &str,
        task_tag: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        self.counterfactual_inner(decision, alternative, task_tag, limit.unwrap_or(5))
    }

    /// `prediction_report` (v14) — the prediction-ledger calibration
    /// dashboard: resolved/correct/ambiguous/pending overall, per method and
    /// per task_tag, plus the newest pending predictions. This is how the
    /// system's counterfactual claims stay falsifiable instead of rhetorical.
    pub fn prediction_report(&self) -> String {
        let Ok(stats) = self.store.prediction_stats() else {
            return "❌ Prediction report failed".to_string();
        };
        if stats.resolved == 0 && stats.pending == 0 {
            return "📐 Prediction ledger is empty — counterfactual_query logs a \
                    prediction every time it issues a verdict; predictions resolve \
                    automatically when either option is later recorded."
                .to_string();
        }
        let judged = stats.resolved - stats.ambiguous;
        let mut out = format!(
            "📐 Prediction ledger: {} resolved / {} pending\n",
            stats.resolved, stats.pending
        );
        if stats.resolved > 0 {
            out.push_str(&format!(
                "   accuracy {}/{} ({:.0}%{}), {} ambiguous excluded\n",
                stats.correct,
                judged,
                if judged > 0 {
                    stats.correct as f64 / judged as f64 * 100.0
                } else {
                    0.0
                },
                if judged > 0 {
                    ""
                } else {
                    ", no judged predictions yet"
                },
                stats.ambiguous
            ));
        }
        let fmt_entry = |label: &str, e: &crate::store::PredictionStatsEntry| {
            let denom = e.resolved - e.ambiguous;
            format!(
                "   {label}: {}/{} correct ({} ambiguous)\n",
                e.correct, denom, e.ambiguous
            )
        };
        for (method, e) in &stats.by_method {
            out.push_str(&fmt_entry(&format!("method={method}"), e));
        }
        for (tag, e) in &stats.by_task_tag {
            out.push_str(&fmt_entry(&format!("task_tag={tag}"), e));
        }
        if let Ok(pending) = self.store.pending_predictions(5) {
            for p in pending {
                let tag = if p.task_tag.is_empty() {
                    "(none)"
                } else {
                    &p.task_tag
                };
                out.push_str(&format!(
                    "   pending #{} \"{}\" vs \"{}\" ({}, {})\n",
                    p.id,
                    truncate_chars(&p.option_a, 40),
                    truncate_chars(&p.option_b, 40),
                    tag,
                    p.method
                ));
            }
        }
        out
    }

    /// Counterfactual comparison with the embedder injected (None = keyword
    /// path, identical to unconfigured — keeps tests hermetic).
    pub fn counterfactual_inner(
        &self,
        decision: &str,
        alternative: &str,
        task_tag: Option<&str>,
        limit: usize,
    ) -> String {
        // C2: run both sides' embedding + retrieval concurrently (the two
        // retrieval calls used to serialize two HTTP round-trips).
        let (a, b) = block_on(async {
            let fa = self.side_entries(decision, task_tag, limit);
            let fb = self.side_entries(alternative, task_tag, limit);
            tokio::join!(fa, fb)
        });
        let (entries_a, tag_a) = a;
        let (entries_b, tag_b) = b;
        let tag = if tag_a == "semantic" && tag_b == "semantic" {
            "[semantic]"
        } else {
            "[bm25]"
        };
        // Competitive separation before aggregation (v14.1): shared
        // vocabulary must not pool both sides into both distributions.
        let (entries_a, entries_b) = Self::separate_sides(&entries_a, &entries_b);
        let (dist_a, reps_a) = Self::dist_and_reps(&entries_a);
        let (dist_b, reps_b) = Self::dist_and_reps(&entries_b);
        let ids_a: Vec<i64> = entries_a.iter().map(|e| e.edge_id).collect();
        let ids_b: Vec<i64> = entries_b.iter().map(|e| e.edge_id).collect();

        // v14 fork section: same-context siblings of any retrieved edge are
        // natural experiments — evidence about ONE world state rather than a
        // cross-context aggregate. Best-effort: lookup failures just skip
        // the section (output stays distribution-only).
        let mut ids = ids_a.clone();
        ids.extend_from_slice(&ids_b);
        let forks = self.store.fork_siblings_for_edges(&ids).unwrap_or_default();

        let mut out = String::from(
            "⚠️ contrastive/empirical counterfactual over recorded alternatives — not a Pearl Rung-3 SCM counterfactual\n",
        );
        out.push_str(&format!("{tag} Comparing recorded evidence:\n\n"));
        for (label, text, dist, reps) in [
            ("A", decision, &dist_a, &reps_a),
            ("B", alternative, &dist_b, &reps_b),
        ] {
            out.push_str(&format!(
                "{label}. \"{}\" (n={}): {} positive / {} negative / {} mixed / {} neutral\n",
                truncate_chars(text, 60),
                dist.total(),
                dist.positive,
                dist.negative,
                dist.mixed,
                dist.neutral
            ));
            for r in reps {
                out.push_str(&format!("   → {r}\n"));
            }
        }
        if !forks.is_empty() {
            out.push_str(&format!(
                "\n🔀 Same-context branches (natural experiments, {} pair(s)):\n",
                forks.len()
            ));
            for f in &forks {
                // Which query side each endpoint matched (if any): label
                // only when the endpoint was retrieved BY that side.
                let side = |id: i64, decision_ids: &[i64], label: &'static str| {
                    if decision_ids.contains(&id) {
                        label
                    } else {
                        ""
                    }
                };
                let sa = side(f.edge_id_a, &ids_a, "A");
                let sb = side(f.edge_id_b, &ids_b, "B");
                fn pol(p: &Option<String>) -> &str {
                    p.as_deref().unwrap_or("?")
                }
                out.push_str(&format!(
                    "   [{}] {}{} →({})→ \"{}\" [{}]  vs  {}{} →({})→ \"{}\" [{}]\n",
                    truncate_chars(&f.fingerprint, 48),
                    sa,
                    truncate_chars(&f.a_decision, 40),
                    f.a_relation,
                    truncate_chars(&f.a_outcome, 40),
                    pol(&f.a_polarity),
                    sb,
                    truncate_chars(&f.b_decision, 40),
                    f.b_relation,
                    truncate_chars(&f.b_outcome, 40),
                    pol(&f.b_polarity),
                ));
            }
        }
        // Conclusion: paired (same-context) evidence outranks the pooled
        // distributions when both exist — a fork pair is evidence about one
        // world; distributions mix many.
        let paired = paired_verdict(&forks, &ids_a, &ids_b);
        out.push_str(&format!(
            "\nConclusion: {}\n",
            match &paired {
                Some(p) => format!("{p} (outranks the pooled distribution)"),
                None => counterfactual_verdict(&dist_a, &dist_b),
            }
        ));
        // v14 prediction ledger: every verdict becomes a falsifiable
        // prediction, auto-resolved when either option is later recorded.
        // Verdict codes come from the shared VERDICT_* constants — the
        // formatters and this matcher read the same strings by construction
        // (the "favor"/"favors" mismatch bug this replaces was exactly a
        // hand-copied-contract failure).
        let verdict_code = match &paired {
            Some(p) if p.contains(VERDICT_FAVORS_A) => "prefer_a",
            Some(p) if p.contains(VERDICT_FAVORS_B) => "prefer_b",
            Some(_) => "no_difference", // tied contrasting pairs
            None => match counterfactual_verdict(&dist_a, &dist_b) {
                v if v.contains(VERDICT_FAVORS_A) => "prefer_a",
                v if v.contains(VERDICT_FAVORS_B) => "prefer_b",
                _ if dist_a.total() > 0 && dist_b.total() > 0 => "no_difference",
                _ => "",
            },
        };
        if !verdict_code.is_empty() {
            let strength = ((dist_a.score() - dist_b.score()).abs() / 4.0).clamp(0.1, 1.0);
            if let Ok(id) = self.store.log_prediction(
                decision,
                alternative,
                task_tag,
                verdict_code,
                "contrastive",
                strength,
                None,
            ) {
                out.push_str(&format!(
                    "📐 Prediction #{id} logged — resolved automatically when either option is recorded.\n"
                ));
                // Phase-4 interface (executable replay routing): closed-world
                // tags route to a replay plan instead of only an estimate.
                // The engine (stepback-style trace + dirty-set rerun) is future
                // work; this defines the routing + the ledger feedback path so
                // method='executable' predictions can exist from day one.
                if let Some(tag) = task_tag {
                    if CLOSED_WORLD_TAGS.contains(&tag.to_lowercase().as_str()) {
                        out.push_str(&format!(
                            "🧪 Closed-world decision (task_tag={tag}): instead of estimating — replay it. \
                             Apply the alternative in a sandbox, execute, then record the outcome with \
                             record_decision(…, context=<same as the original edge>). The prediction \
                             resolves automatically; over time prediction_report separates replayed facts \
                             from estimates.\n"
                        ));
                    }
                }
            }
        }
        out
    }

    /// One side of the counterfactual: retrieve similar past decision edges
    /// (semantic with BM25 fallback, same pattern as search_causal) and
    /// aggregate their outcome distribution + representative outcomes.
    /// v14.1: returns the raw ranked entries — competitive separation (an
    /// edge retrieved by BOTH queries stays only on the side that ranks it
    /// higher) runs in counterfactual_inner AFTER both retrievals land,
    /// then distribution/reps are computed from the separated pools.
    async fn side_entries(
        &self,
        query: &str,
        task_tag: Option<&str>,
        limit: usize,
    ) -> (Vec<crate::store::CausalEntry>, &'static str) {
        let semantic = crate::embed::embed_shared(query).await.and_then(|r| {
            let vec = r.ok()?;
            let hits = self
                .store
                .similar_decision_edges(&vec, limit * 2, INTERVENTION_MIN_SIMILARITY)
                .ok()?;
            let entries: Vec<_> = hits
                .into_iter()
                .map(|(e, _)| e)
                .filter(|e| task_tag.is_none() || e.task_tag.as_deref() == task_tag)
                .take(limit)
                .collect();
            (!entries.is_empty()).then_some(entries)
        });
        match semantic {
            Some(e) => (e, "semantic"),
            None => {
                let entries = self
                    .store
                    .search_causal_bm25(task_tag, query, limit)
                    .unwrap_or_default();
                (entries, "bm25")
            }
        }
    }

    /// Distribution + representative outcomes over (already separated)
    /// side entries.
    fn dist_and_reps(entries: &[crate::store::CausalEntry]) -> (CfDist, Vec<String>) {
        let mut dist = CfDist::default();
        for e in entries {
            dist.add(polarity_bucket(
                e.outcome_polarity.as_deref(),
                &e.outcome_text,
            ));
        }
        let reps: Vec<String> = entries
            .iter()
            .take(3)
            .map(|e| {
                format!(
                    "\"{}\" (conf {:.0}%, {})",
                    truncate_chars(&e.outcome_text, 60),
                    e.confidence * 100.0,
                    polarity_bucket(e.outcome_polarity.as_deref(), &e.outcome_text)
                )
            })
            .collect();
        (dist, reps)
    }

    /// Competitive separation (v14.1, the cross-side-contamination fix):
    /// retrieval matches decision AND outcome text, so two options that
    /// share vocabulary pull each other's episodes into both pools,
    /// flattening the contrast toward a tie. An edge retrieved by BOTH
    /// queries stays only on the side that ranks it earlier (both paths
    /// return rank-ordered entries; earlier = stronger match) and is
    /// dropped from the other. Returns the separated pools.
    fn separate_sides(
        a: &[crate::store::CausalEntry],
        b: &[crate::store::CausalEntry],
    ) -> (
        Vec<crate::store::CausalEntry>,
        Vec<crate::store::CausalEntry>,
    ) {
        let rank_of = |id: i64, entries: &[crate::store::CausalEntry]| {
            entries.iter().position(|e| e.edge_id == id)
        };
        let mut a_kept = Vec::with_capacity(a.len());
        for (ia, e) in a.iter().enumerate() {
            match rank_of(e.edge_id, b) {
                Some(ib) if ib < ia => {} // B ranks it stronger → B keeps it
                _ => a_kept.push(e.clone()),
            }
        }
        let mut b_kept = Vec::with_capacity(b.len());
        for (ib, e) in b.iter().enumerate() {
            match rank_of(e.edge_id, a) {
                Some(ia) if ia <= ib => {} // A ranks it stronger (or tie → A) → A keeps it
                _ => b_kept.push(e.clone()),
            }
        }
        (a_kept, b_kept)
    }

    /// `reconstruct_lesson` — reconstructive retrieval: Markov-blanket
    /// subgraph → optional LLM narrative, with optional N-way calibration.
    pub fn reconstruct_lesson(
        &self,
        query: &str,
        max_edges: Option<usize>,
        calibrate: Option<usize>,
    ) -> String {
        let llm = crate::llm::LlmConfig::from_env();
        self.reconstruct_lesson_inner(
            query,
            max_edges.unwrap_or(20),
            calibrate.unwrap_or(0),
            llm.as_ref(),
        )
    }

    /// Reconstruct pipeline with embedder/LLM injected (None, None = local
    /// subgraph only — keeps tests hermetic and honors zero-intrusion).
    pub fn reconstruct_lesson_inner(
        &self,
        query: &str,
        max_edges: usize,
        calibrate: usize,
        llm: Option<&crate::llm::LlmConfig>,
    ) -> String {
        // 1. Subgraph layer (always local): seed via semantic/BM25, then the
        //    Markov blanket around the seeds, serialized as compact stubs.
        let mut tag = "[bm25]";
        let mut seeds: Vec<CausalEntry> = Vec::new();
        if let Some(Ok(vec)) = block_on(crate::embed::embed_shared(query)) {
            if let Ok(hits) =
                self.store
                    .similar_decision_edges(&vec, 3, INTERVENTION_MIN_SIMILARITY)
            {
                if !hits.is_empty() {
                    tag = "[semantic]";
                    seeds = hits.into_iter().map(|(e, _)| e).collect();
                }
            }
        }
        if seeds.is_empty() {
            seeds = self
                .store
                .search_causal_bm25(None, query, 3)
                .unwrap_or_default();
        }
        if seeds.is_empty() {
            return format!("{tag} 📭 No recorded causal context found for \"{query}\".");
        }
        let seed_ids: Vec<i64> = seeds.iter().map(|e| e.edge_id).collect();
        let blanket = match self.store.markov_blanket(&seed_ids, max_edges) {
            Ok(b) => b,
            Err(e) => return format!("❌ Subgraph query failed: {e}"),
        };
        let stubs: Vec<String> = blanket.iter().map(edge_stub).collect();

        let mut out = format!(
            "{tag} Causal subgraph for \"{query}\" ({} edge(s), max {max_edges}):\n",
            blanket.len()
        );
        for s in &stubs {
            out.push_str(s);
            out.push('\n');
        }

        // 2. Narrative layer (LLM configured): reconstruct the lesson.
        let Some(config) = llm else {
            out.push_str("\n(configure CAUSAL_MEMORY_LLM_* for narrative reconstruction)\n");
            return out;
        };
        let stubs_text = stubs.join("\n");
        match block_on(crate::llm::reconstruct_narrative(
            config,
            query,
            &stubs_text,
            0.0,
        )) {
            Ok(n) => out.push_str(&format!("\nReconstructed lesson:\n{}\n", n.trim())),
            Err(_) => {
                out.push_str("\n(narrative reconstruction failed — subgraph above is complete)\n");
                return out;
            }
        }

        // 3. Calibration layer: k independent reconstructions, agreement check.
        if calibrate >= 2 {
            let mut samples = Vec::new();
            for _ in 0..calibrate {
                if let Ok(t) = block_on(crate::llm::reconstruct_narrative(
                    config,
                    query,
                    &stubs_text,
                    0.7,
                )) {
                    samples.push(t);
                }
            }
            if samples.len() >= 2 {
                let agreement = reconstruction_agreement(&samples);
                out.push_str(&format!(
                    "\nreconstruction agreement ({} samples): {:.0}%\n",
                    samples.len(),
                    agreement * 100.0
                ));
                if agreement < 0.5 {
                    out.push_str(
                        "⚠️ low reconstruction agreement — underlying memories may be unreliable\n",
                    );
                }
            }
        }
        out
    }

    /// Get recent decisions for system prompt (L0 directory, per insights/13 §1.2).
    pub fn recent_decisions_directory(&self, limit: usize) -> String {
        match self.store.recent_decisions(limit) {
            Ok(entries) if entries.is_empty() => String::new(),
            Ok(entries) => {
                let mut out = String::from("# Your recent decisions (causal memory)\n\n");
                for e in entries {
                    out.push_str(&format!(
                        "- [{}] {} →({})→ {}\n",
                        e.task_tag.as_deref().unwrap_or("?"),
                        e.decision_snippet,
                        e.relation,
                        e.outcome_snippet,
                    ));
                }
                out
            }
            Err(_) => String::new(),
        }
    }
}

// ─── Unified search presentation (shared by both retrieval paths) ────

/// Layer-namespaced key + content of a fact hit.
fn fact_hit(f: &AgentFact) -> MemoryHit {
    MemoryHit {
        key: format!("fact:{}", f.id),
        content: format!("[{}] {} = \"{}\"", f.scope, f.key, f.value),
        score: 0.0, // filled by hits_from_ranked
        rank: 0,
        created_at: Some(f.updated_at),
    }
}

/// Layer-namespaced key + content of a causal hit.
fn causal_hit(e: &CausalEntry) -> MemoryHit {
    MemoryHit {
        key: format!("causal:{}", e.edge_id),
        content: format!(
            "\"{}\" →({})→ \"{}\"",
            e.decision_text, e.relation, e.outcome_text
        ),
        score: 0.0, // filled by hits_from_ranked
        rank: 0,
        created_at: Some(e.event_time),
    }
}

/// Materialize ranked pairs as `MemoryHit`s, globally rank-sorted (the
/// same order the fused/spread ranking produced). Score uses the RRF
/// formula on BOTH paths so the field keeps one semantics.
fn hits_from_ranked(
    facts: &[(usize, AgentFact)],
    causal: &[(usize, CausalEntry)],
) -> Vec<MemoryHit> {
    let mut hits: Vec<MemoryHit> = facts
        .iter()
        .map(|(rank, f)| {
            let mut h = fact_hit(f);
            h.rank = *rank;
            h.score = 1.0 / (super::RRF_K + *rank as f64);
            h
        })
        .chain(causal.iter().map(|(rank, e)| {
            let mut h = causal_hit(e);
            h.rank = *rank;
            h.score = 1.0 / (super::RRF_K + *rank as f64);
            h
        }))
        .collect();
    hits.sort_by_key(|h| h.rank);
    hits
}

/// The shared display tail of `search_memory`: D4 intent routing (prefer
/// the dominant layer in the DISPLAY when the classifier is confident and
/// that layer has hits — never hide evidence), then the grouped
/// fact/causal rendering with per-item ranks. `detail_level` picks the
/// per-item verbosity (l2 = the historical format, byte-identical);
/// `max_tokens` > 0 truncates both sections against one shared budget.
/// `explain` (None by default → byte-identical output) appends a
/// provenance tag per hit (Flip-path marking).
#[allow(clippy::too_many_arguments)]
fn render_unified(
    query: &str,
    facts: &[(usize, AgentFact)],
    causal: &[(usize, CausalEntry)],
    mode: &str,
    detail_level: &str,
    max_tokens: usize,
    explain: Option<&HashMap<String, String>>,
) -> String {
    if facts.is_empty() && causal.is_empty() {
        return format!("[unified/{mode}] 📭 No memories found matching your query in any layer.");
    }
    let intent = crate::query_router::classify_query(query);
    let dominant_is_causal = matches!(
        intent,
        crate::query_router::QueryIntent::Causal | crate::query_router::QueryIntent::Chain
    );
    let dominant_is_fact = matches!(intent, crate::query_router::QueryIntent::Fact);
    let route_causal = dominant_is_causal && !causal.is_empty();
    let route_fact = dominant_is_fact && !facts.is_empty();
    let facts: &[(usize, AgentFact)] = if route_causal { &[] } else { facts };
    let causal: &[(usize, CausalEntry)] = if route_fact { &[] } else { causal };

    let layers = usize::from(!facts.is_empty()) + usize::from(!causal.is_empty());
    let total = facts.len() + causal.len();
    let mut out = format!("[unified/{mode}] Found {total} memories across {layers} layer(s):\n\n");
    let mut budget = TokenBudget::new(max_tokens);
    let mut truncated = 0usize;
    if !facts.is_empty() {
        out.push_str(&format!("📊 Facts ({}):\n", facts.len()));
        for (rank, fact) in facts {
            let (line, cost) = format_fact_layered(fact, *rank, detail_level);
            if !budget.try_spend(cost) {
                truncated += 1;
                continue;
            }
            out.push_str(&line);
            if let Some(tag) = explain.and_then(|m| m.get(&format!("fact:{}", fact.id))) {
                out.push_str(&format!("    ↳ {tag}\n"));
            }
        }
        out.push('\n');
    }
    if !causal.is_empty() {
        out.push_str(&format!("🔗 Causal lessons ({}):\n", causal.len()));
        for (rank, entry) in causal {
            let (line, cost) = format_lesson_layered(entry, *rank, detail_level);
            if !budget.try_spend(cost) {
                truncated += 1;
                continue;
            }
            out.push_str(&line);
            if let Some(tag) = explain.and_then(|m| m.get(&format!("causal:{}", entry.edge_id))) {
                out.push_str(&format!("    ↳ {tag}\n"));
            }
        }
    }
    if truncated > 0 {
        out.push_str(&format!(
            "… {truncated} more result(s) truncated (token budget)\n"
        ));
    }
    out
}
