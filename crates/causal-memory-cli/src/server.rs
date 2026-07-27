//! MCP server handler — exposes 8 tools for causal memory.
//!
//! Tools:
//! - record_decision: agent calls after completing an action, to log
//!   the decision and its outcome as a causal edge.
//! - search_causal: agent calls BEFORE a non-trivial decision, to check
//!   past lessons in the same task domain.
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

use rmcp::{
    handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router, ServerHandler,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;

use causal_memory::embed::{EmbedConfig, Embedder};
use causal_memory::store::{outcome_polarity, CausalStore};

pub struct CausalMemoryServer {
    store: CausalStore,
}

impl CausalMemoryServer {
    pub fn new(store: CausalStore) -> Self {
        Self { store }
    }
}

/// Run an async embed call from a sync tool handler.
/// The MCP server runs inside a multi-thread tokio runtime (see main.rs), so
/// bridge with block_in_place; fall back to a throwaway runtime when no
/// runtime context exists (defensive — not expected in production).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime")
            .block_on(fut),
    }
}

// ─── Tool parameter types ─────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(description = "Parameters for recording a decision and its outcome")]
pub struct RecordDecisionParams {
    /// What you decided to do (e.g., "used Redis mutex for cache stampede protection")
    #[schemars(description = "What you decided to do")]
    pub decision: String,
    /// What actually happened (e.g., "deadlock because mutex holder crashed")
    #[schemars(description = "What actually happened as a result")]
    pub outcome: String,
    /// Did the decision cause / enable / prevent / not affect the outcome?
    #[schemars(description = "Relationship: caused, enabled, prevented, or no_effect")]
    pub relation: String,
    /// The type of task (e.g., "concurrency", "caching", "debugging")
    #[schemars(description = "Task category for future retrieval")]
    pub task_tag: String,
    /// How confident are you in this causal link? (temporal=0.4, rule=0.7, llm_inferred=0.6, user_feedback=0.95)
    #[schemars(
        description = "Confidence source. Use: temporal, rule, llm_inferred, or user_feedback"
    )]
    pub confidence_source: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SearchCausalParams {
    /// The type of task you're working on (e.g., "concurrency")
    #[schemars(description = "Task category to search within")]
    pub task_tag: Option<String>,
    /// Natural language description of what you're about to do
    #[schemars(description = "Text to match against past decisions and outcomes")]
    pub query: Option<String>,
    /// Max number of results (default 5)
    #[schemars(description = "Maximum results to return (default 5)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct TraceCauseParams {
    /// Description of the bad outcome
    #[schemars(description = "Description of what went wrong")]
    pub outcome_description: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct TraceCauseChainParams {
    /// Description of the bad outcome to trace backward from
    #[schemars(description = "Description of what went wrong")]
    pub outcome_description: String,
    /// Maximum chain depth (default 3)
    #[schemars(description = "Maximum hops to trace backward (default 3)")]
    pub max_depth: Option<usize>,
    /// Minimum confidence per edge (default 0.5)
    #[schemars(description = "Minimum confidence threshold for each hop (default 0.5)")]
    pub min_confidence: Option<f64>,
    /// Max chains to return (default 5)
    #[schemars(description = "Maximum causal chains to return (default 5)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct InvalidateDecisionParams {
    /// The edge_id of the causal edge to invalidate
    #[schemars(description = "ID of the causal edge to invalidate")]
    pub edge_id: i64,
    /// Why this lesson is wrong (echoed back for confirmation; not persisted)
    #[schemars(description = "Reason for invalidation (optional, for confirmation only)")]
    pub reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SearchPatternsParams {
    /// Text to match against pattern summaries or the decisions at either end
    #[schemars(description = "Text to match against patterns or endpoint decisions")]
    pub query: Option<String>,
    /// Only patterns where at least one endpoint decision belongs to this task
    #[schemars(description = "Task category filter (matches either endpoint)")]
    pub task_tag: Option<String>,
    /// Max number of results (default 10)
    #[schemars(description = "Maximum results to return (default 10)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct CausalDirectoryParams {
    /// Max directory entries (default 20)
    #[schemars(description = "Maximum directory entries to return (default 20)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct InterventionQueryParams {
    /// The action you are about to take
    #[schemars(description = "Description of the action you are about to take")]
    pub action: String,
    /// Maximum chain depth (default 3)
    #[schemars(description = "Maximum hops to trace forward (default 3)")]
    pub max_depth: Option<usize>,
    /// Max predicted-effect chains to return (default 5)
    #[schemars(description = "Maximum predicted-effect chains to return (default 5)")]
    pub limit: Option<usize>,
}

// ─── Tool implementations ─────────────────────────────────────────────────

#[tool_router]
impl CausalMemoryServer {
    #[tool(
        name = "record_decision",
        description = "Record a decision and its observed outcome as a causal memory. Call this AFTER you've acted on a decision and observed the result, especially if the outcome was surprising or educational. This builds your experience base for future similar tasks."
    )]
    fn record_decision(&self, Parameters(params): Parameters<RecordDecisionParams>) -> String {
        let confidence = match params.confidence_source.as_deref() {
            Some("temporal") => 0.4,
            Some("rule") => 0.7,
            Some("user_feedback") => 0.95,
            _ => 0.6, // llm_inferred (default)
        };
        let source = params
            .confidence_source
            .as_deref()
            .unwrap_or("llm_inferred");

        match self.store.record_decision(
            &params.decision,
            &params.outcome,
            &params.relation,
            Some(&params.task_tag),
            confidence,
            source,
        ) {
            Ok(id) => {
                // Phase 6: opportunistically embed the new edge so semantic
                // search finds it. Silent on any failure — embedding must never
                // block recording; the `causal-memory embed` CLI backfills
                // anything missed.
                if let Some(embedder) = EmbedConfig::from_env().map(Embedder::new) {
                    let text = format!("{} {}", params.decision, params.outcome);
                    if let Ok(vec) = block_on(embedder.embed(&text)) {
                        // record_decision returns the decision chunk id; resolve
                        // the edge id (chunk ids are unique per record).
                        let edge_id = self.store.with_conn(|conn| {
                            Ok(conn
                                .query_row(
                                    "SELECT id FROM causal_edges WHERE from_id = ?1
                                     ORDER BY id DESC LIMIT 1",
                                    rusqlite::params![&id],
                                    |r| r.get::<_, i64>(0),
                                )
                                .optional()?)
                        });
                        if let Ok(Some(eid)) = edge_id {
                            let _ = self.store.put_embedding(eid, embedder.model(), &vec);
                        }
                    }
                }
                format!(
                    "✅ Recorded: [{}] \"{}\" →({})→ \"{}\" (confidence: {:.2}, id: {})",
                    params.task_tag,
                    &params.decision[..params.decision.len().min(60)],
                    params.relation,
                    &params.outcome[..params.outcome.len().min(60)],
                    confidence,
                    id
                )
            }
            Err(e) => format!("❌ Failed to record: {e}"),
        }
    }

    #[tool(
        name = "search_causal",
        description = "Search past decisions and their outcomes for situations similar to your current task. Call this BEFORE attempting a non-trivial decision to learn from past experience. Filter by task_tag for domain-specific lessons, or use query text for broader search."
    )]
    fn search_causal(&self, Parameters(params): Parameters<SearchCausalParams>) -> String {
        let limit = params.limit.unwrap_or(5);
        // Semantic retrieval is meaningless for tag-only browsing (no query text).
        let query = params.query.as_deref().filter(|q| !q.trim().is_empty());

        // Semantic path: embed the query and cosine-rank edge embeddings.
        // Requires a configured embedding endpoint; any failure falls back to
        // the keyword path below (identical to the pre-Phase-6 behavior).
        if let Some(query) = query {
            if let Some(embedder) = EmbedConfig::from_env().map(Embedder::new) {
                let semantic = block_on(embedder.embed(query)).ok().and_then(|vec| {
                    self.store
                        .search_causal_semantic(&vec, params.task_tag.as_deref(), limit)
                        .ok()
                });
                if let Some(results) = semantic {
                    if results.is_empty() {
                        return "[semantic] 📭 No past causal episodes found matching your query."
                            .to_string();
                    }
                    let mut out =
                        format!("[semantic] Found {} past episode(s):\n\n", results.len());
                    for (i, (entry, sim)) in results.iter().enumerate() {
                        out.push_str(&format!(
                            "{}. [{}] \"{}\"\n   →({})→ \"{}\"\n   similarity: {:.0}%, confidence: {:.0}%\n\n",
                            i + 1,
                            entry.task_tag.as_deref().unwrap_or("untagged"),
                            entry.decision_text,
                            entry.relation,
                            entry.outcome_text,
                            sim * 100.0,
                            entry.confidence * 100.0,
                        ));
                    }
                    return out;
                }
                // embed or semantic search failed — fall through to keyword.
            }
        }

        // Keyword (LIKE) path — original behavior.
        let results = match self
            .store
            .search_causal(params.task_tag.as_deref(), params.query.as_deref())
        {
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
                "{}. [{}] \"{}\"\n   →({})→ \"{}\"\n   confidence: {:.0}%\n\n",
                i + 1,
                entry.task_tag.as_deref().unwrap_or("untagged"),
                entry.decision_text,
                entry.relation,
                entry.outcome_text,
                entry.confidence * 100.0,
            ));
        }
        out
    }

    #[tool(
        name = "trace_cause",
        description = "When something went wrong, trace back which past decision could have caused it. Use for post-mortem analysis. Provide a description of the bad outcome."
    )]
    fn trace_cause(&self, Parameters(params): Parameters<TraceCauseParams>) -> String {
        match self.store.trace_cause(&params.outcome_description) {
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

    #[tool(
        name = "trace_cause_chain",
        description = "Deep failure analysis: trace multi-hop causal chains backward from a bad outcome. Use when a single-hop trace doesn't reveal the root cause. E.g., 'service crashed' ← 'OOM' ← 'cache had no TTL' ← 'Redis configured without expiry'. Parameters: outcome_description, max_depth (default 3), min_confidence (default 0.5), limit (default 5)."
    )]
    fn trace_cause_chain(&self, Parameters(params): Parameters<TraceCauseChainParams>) -> String {
        let max_depth = params.max_depth.unwrap_or(3);
        let min_confidence = params.min_confidence.unwrap_or(0.5);
        let limit = params.limit.unwrap_or(5);

        let chains = match self.store.trace_cause_chain(
            &params.outcome_description,
            max_depth,
            min_confidence,
        ) {
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

    #[tool(
        name = "invalidate_decision",
        description = "Mark a past causal lesson as wrong (soft-invalidate). The edge is hidden from all future search/trace results but kept in the database for audit. Use when you or the user discover that a recorded decision→outcome link was incorrect."
    )]
    fn invalidate_decision(
        &self,
        Parameters(params): Parameters<InvalidateDecisionParams>,
    ) -> String {
        let edge = match self.store.get_edge(params.edge_id) {
            Ok(Some(e)) => e,
            Ok(None) => return format!("❌ Edge #{} not found.", params.edge_id),
            Err(e) => return format!("❌ Lookup failed: {e}"),
        };

        if edge.valid_to.is_some() {
            return format!(
                "❌ Edge #{} was already invalidated: \"{}\" →({})→ \"{}\"",
                params.edge_id, edge.decision_text, edge.relation, edge.outcome_text,
            );
        }

        match self.store.invalidate_edge(params.edge_id) {
            Ok(true) => {
                let reason = params
                    .reason
                    .as_deref()
                    .map(|r| format!(" (reason: {r})"))
                    .unwrap_or_default();
                format!(
                    "✅ Invalidated edge #{}: \"{}\" →({})→ \"{}\"{reason}. It will no longer appear in search/trace results, but is kept for audit.",
                    params.edge_id, edge.decision_text, edge.relation, edge.outcome_text,
                )
            }
            Ok(false) => format!("❌ Edge #{} could not be invalidated.", params.edge_id),
            Err(e) => format!("❌ Invalidate failed: {e}"),
        }
    }

    #[tool(
        name = "search_patterns",
        description = "Search mined cross-task patterns (meta-causal edges): decisions that are similar_to each other, repeated across tasks, contradicts each other, or refines an earlier failed attempt. Use this to recall abstracted lessons that span multiple task domains."
    )]
    fn search_patterns(&self, Parameters(params): Parameters<SearchPatternsParams>) -> String {
        let limit = params.limit.unwrap_or(10);

        let results = match self.store.search_patterns(
            params.query.as_deref(),
            params.task_tag.as_deref(),
            limit,
        ) {
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
                "{}. \"{}\" --[{label}]--> \"{}\"\n   {pattern}\n   confidence: {:.0}%\n\n",
                i + 1,
                edge.from_text,
                edge.to_text,
                edge.confidence * 100.0,
            ));
        }
        out
    }

    #[tool(
        name = "causal_directory",
        description = "L0 directory of your recent decisions and their outcomes — a compact pointer list meant to be pinned in the agent's system prompt so it always knows what past experience it holds. Entries are one-line pointers; use trace_cause / search_causal / intervention_query with the decision texts for full details."
    )]
    fn causal_directory(&self, Parameters(params): Parameters<CausalDirectoryParams>) -> String {
        let limit = params.limit.unwrap_or(20);
        let body = self.recent_decisions_directory(limit);
        if body.is_empty() {
            return "📭 No decisions recorded yet — the causal memory directory is empty."
                .to_string();
        }
        format!(
            "{body}\nUse trace_cause/search_causal/intervention_query with these decision texts for details.\n"
        )
    }

    #[tool(
        name = "intervention_query",
        description = "Pearl Rung-2 intervention: BEFORE taking an action, query what outcomes similar past actions caused. Returns predicted effects with causal paths and confidence, labeled safe/warning/danger."
    )]
    fn intervention_query(
        &self,
        Parameters(params): Parameters<InterventionQueryParams>,
    ) -> String {
        let max_depth = params.max_depth.unwrap_or(3);
        let limit = params.limit.unwrap_or(5);
        // Internal pruning floor: lower than trace_cause_chain's 0.5 default
        // because forward chains multiply confidence per hop and would prune
        // away realistic 2-3 hop predictions at 0.5.
        let min_confidence = 0.3;

        let chains = match self
            .store
            .trace_effect_chain(&params.action, max_depth, min_confidence)
        {
            Ok(c) => c,
            Err(e) => return format!("❌ Intervention query failed: {e}"),
        };

        if chains.is_empty() {
            return format!(
                "📭 No precedent found for \"{}\" — absence of evidence is not evidence of safety. Proceed with caution, and record the outcome afterward with record_decision.",
                params.action
            );
        }

        let show = chains.len().min(limit);
        let mut out = format!(
            "Predicted effect(s) of \"{}\" — {} chain(s) (showing {}, max_depth={}):\n\n",
            params.action,
            chains.len(),
            show,
            max_depth
        );

        for (i, chain) in chains.iter().take(limit).enumerate() {
            let terminal = chain.last();
            let has_prevented = chain.iter().any(|h| h.relation == "prevented");
            let label = match terminal.map(|h| outcome_polarity(&h.outcome_text)) {
                // A failure that a `prevented` edge along the path blocked
                // before: downgrade DANGER → UNKNOWN.
                Some(Some(false)) if has_prevented => {
                    "ℹ️ UNKNOWN (failure outcome, but a prevented edge on this path blocked it before)"
                }
                Some(Some(false)) => "⚠️ DANGER",
                Some(Some(true)) => "✅ SAFE",
                _ => "ℹ️ UNKNOWN",
            };
            out.push_str(&format!(
                "Chain {} {} (chain confidence: {:.0}%):\n",
                i + 1,
                label,
                terminal.map(|h| h.chain_confidence * 100.0).unwrap_or(0.0)
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

// ─── ServerHandler implementation ──────────────────────────────────────────
// The #[tool_handler] macro auto-generates get_info(), list_tools(), call_tool().

#[tool_handler]
impl ServerHandler for CausalMemoryServer {}
