//! The 14 MCP tools. `#[tool_router]` must stay on a single impl block,
//! so all tool methods live here; helpers live in `super` modules.

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use std::collections::HashMap;

use super::format::{format_entry_layered, rrf_fuse_many, truncate_chars, TokenBudget};
use super::output::*;
use super::{block_on, CausalMemoryServer, INTERVENTION_MIN_SIMILARITY, SEMANTIC_CONTRADICTION_MIN_SIMILARITY};
use causal_memory::embed;
use causal_memory::store::{AgentFact, CausalEntry, ChainHop};
use rusqlite::OptionalExtension;

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

/// Parameters for the `remember` tool — mem0-style auto-extraction.
/// Agent feeds raw conversation text; the system's LLM automatically extracts
/// facts, lessons, and causal edges (caused/enabled/prevented).
#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(description = "Parameters for remember — auto-extract memories from conversation text")]
pub struct RememberParams {
    /// The conversation text to extract memories from. Can be multiple messages
    /// joined by newlines, or a single user/assistant exchange.
    #[schemars(description = "Conversation text to extract memories from")]
    pub messages: String,
    /// The date of the conversation (YYYY-MM-DD). Defaults to today.
    #[schemars(description = "Session date (YYYY-MM-DD), defaults to today")]
    pub date: Option<String>,
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
    /// Detail level: l0 = one-line summary (~50 tok); l1 = overview (~200 tok);
    /// l2 = full text + confidence (default, max detail). Saves tokens.
    #[schemars(description = "Detail level: l0 (summary), l1 (overview), l2 (full, default)")]
    pub detail_level: Option<String>,
    /// Maximum total tokens to return (0 = unlimited, default 0).
    #[schemars(description = "Max output tokens (0 = unlimited, default 0)")]
    pub max_tokens: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(description = "Parameters for recording a flat fact")]
pub struct RecordFactParams {
    /// Category of the fact (e.g., "preference", "tech_stack", "config", "project")
    #[schemars(
        description = "Fact category: 'preference', 'tech_stack', 'config', 'project', ..."
    )]
    pub key: String,
    /// The fact content (e.g., "TypeScript", "Redis 7.2", "/api/v1/users")
    #[schemars(description = "The fact itself")]
    pub value: String,
    /// Who this fact belongs to: user (default), session, or agent
    #[schemars(description = "Scope: user (default), session, or agent")]
    pub scope: Option<String>,
    /// Confidence in this fact (default 0.8)
    #[schemars(description = "Confidence 0.0-1.0 (default 0.8)")]
    pub confidence: Option<f64>,
    /// Retire other valid values under the same key+scope (e.g. user switched
    /// package managers: record the new one, set this true to invalidate the old)
    #[schemars(description = "If true, invalidate other valid facts with the same key and scope")]
    pub replace_same_key: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SearchFactsParams {
    /// Natural language query (e.g., "programming language preference")
    #[schemars(description = "Text to match against fact keys and values")]
    pub query: Option<String>,
    /// Scope filter: user, session, or agent
    #[schemars(description = "Only return facts of this scope")]
    pub scope: Option<String>,
    /// Max number of results (default 5)
    #[schemars(description = "Maximum results to return (default 5)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SearchMemoryParams {
    /// Natural language query (e.g., "Redis caching")
    #[schemars(description = "Text matched against ALL memory layers at once")]
    pub query: String,
    /// Optional task filter for the causal layer (e.g., "concurrency")
    #[schemars(description = "Task category filter applied to causal episodes")]
    pub task_tag: Option<String>,
    /// Optional scope filter for the fact layer: user, session, or agent
    #[schemars(description = "Scope filter applied to facts")]
    pub scope: Option<String>,
    /// Max number of results (default 10)
    #[schemars(description = "Maximum fused results to return (default 10)")]
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
    /// Only show chains from this task category (the stratified summary still compares against all chains)
    #[schemars(
        description = "Task category to restrict reported chains to (stratified summary still covers all chains)"
    )]
    pub task_tag: Option<String>,
    /// Maximum chain depth (default 3)
    #[schemars(description = "Maximum hops to trace forward (default 3)")]
    pub max_depth: Option<usize>,
    /// Max predicted-effect chains to return (default 5)
    #[schemars(description = "Maximum predicted-effect chains to return (default 5)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(description = "Parameters for a contrastive (empirical) counterfactual comparison")]
pub struct CounterfactualParams {
    /// The decision that was (or would be) taken
    #[schemars(description = "The decision that was (or would be) taken")]
    pub decision: String,
    /// The alternative option to compare against
    #[schemars(description = "The alternative option to compare against")]
    pub alternative: String,
    /// Restrict both sides to this task category
    #[schemars(description = "Task category to restrict both sides to")]
    pub task_tag: Option<String>,
    /// Max recorded episodes per side (default 5)
    #[schemars(description = "Maximum recorded episodes per side (default 5)")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[schemars(description = "Parameters for reconstructive lesson retrieval")]
pub struct ReconstructLessonParams {
    /// The topic or past decision to reconstruct the lesson for
    #[schemars(description = "Topic or past decision text to reconstruct the lesson for")]
    pub query: String,
    /// Max edges in the causal subgraph (default 20)
    #[schemars(description = "Maximum edges in the causal subgraph (default 20)")]
    pub max_edges: Option<usize>,
    /// Generate N independent reconstructions and report their agreement (default 0 = off; needs >= 2)
    #[schemars(
        description = "Generate N independent reconstructions and report their agreement (default 0 = off; needs >= 2)"
    )]
    pub calibrate: Option<usize>,
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

        // Write-time outcome polarity (v4): LLM judge when configured,
        // otherwise the signal-word heuristic. Silent on any failure —
        // polarity must never block recording; legacy-style NULL would just
        // make readers fall back to the heuristic anyway.
        let polarity = judge_outcome_polarity(&params.decision, &params.outcome);

        let result = match self.store.record_decision_full(
            &params.decision,
            &params.outcome,
            &params.relation,
            Some(&params.task_tag),
            confidence,
            source,
            chrono::Utc::now().timestamp(),
            Some(&polarity),
        ) {
            Ok(id) => {
                // Phase 6: opportunistically embed the new edge so semantic
                // search finds it. Silent on any failure — embedding must never
                // block recording; the `causal-memory embed` CLI backfills
                // anything missed.
                let text = format!("{} {}", params.decision, params.outcome);
                if let Some(Ok(vec)) = block_on(causal_memory::embed::embed_shared(&text)) {
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
                        let _ = self.store.put_embedding(eid, "shared", &vec);
                        // Semantic contradiction scan: the exact-text path
                        // already ran inside record_decision; this catches
                        // paraphrased duplicates of the same decision.
                        // High threshold, silent on any error.
                        let _ = self.store.invalidate_semantic_contradictions(
                            &params.decision,
                            &params.outcome,
                            Some(&polarity),
                            &vec,
                            SEMANTIC_CONTRADICTION_MIN_SIMILARITY,
                        );
                    }
                }
                format!(
                    "✅ Recorded: [{}] {} →({})→ {} (confidence: {:.2}, id: {})",
                    params.task_tag,
                    truncate_chars(&params.decision, 60),
                    params.relation,
                    truncate_chars(&params.outcome, 60),
                    confidence,
                    id
                )
            }
            Err(e) => format!("❌ Failed to record: {e}"),
        };
        // After recording, rebuild the hippocampus graph so the new edge is
        // immediately available for spreading activation queries.
        self.reload_graph();
        result
    }

    /// `remember` — mem0-style auto-extraction. Agent feeds raw conversation
    /// text; the system's LLM automatically extracts facts, lessons, and
    /// causal edges (caused/enabled/prevented). This is the zero-friction
    /// alternative to `record_decision` — agent just dumps conversation text,
    /// system does the rest.
    #[tool(
        name = "remember",
        description = "Extract and store memories from conversation text. The system automatically identifies facts, lessons, and causal relationships (caused/enabled/prevented) using LLM analysis. Call this after any meaningful conversation exchange — just paste the conversation text, no need to manually identify decisions or outcomes."
    )]
    fn remember(
        &self,
        Parameters(params): Parameters<RememberParams>,
    ) -> String {
        use causal_memory::distill::{Distiller, ItemKind};

        // Parse the messages into turns
        let date = params.date.as_deref().unwrap_or("");
        let date = if date.len() >= 10 { &date[..10] } else { "" };

        // Split messages into turns — accept raw text with speaker: prefix,
        // or just treat as a single assistant message
        let turns: Vec<(String, String)> = params
            .messages
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
                let text = params.messages.chars().take(500).collect::<String>();
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

        // Run distill synchronously (blocking the tool call)
        let items = match block_on(distiller.distill_session(date, &turns)) {
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
            summary.push(format!("  [{kind_str}] {}", &item.text[..item.text.len().min(80)]));

            // Write to store based on kind
            if item.kind == ItemKind::Causal {
                let relation = item.causal_relation
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
                match self.store.record_fact(key, &item.text, "user", "remember", 0.8) {
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
        self.reload_graph();

        format!(
            "✅ Extracted {} memories: {} facts, {} causal edges, {} episodes\n{}",
            items.len(),
            facts,
            causal,
            episodes,
            summary.join("\n")
        )
    }

    #[tool(
        name = "search_causal",
        description = "Search past decisions and their outcomes for situations similar to your current task. Call this BEFORE attempting a non-trivial decision to learn from past experience. Filter by task_tag for domain-specific lessons, or use query text for broader search."
    )]
    fn search_causal(&self, Parameters(params): Parameters<SearchCausalParams>) -> String {
        let limit = params.limit.unwrap_or(5);
        let detail_level = params.detail_level.as_deref().unwrap_or("l2");
        let max_tokens = params.max_tokens.unwrap_or(0);
        let mut budget = TokenBudget::new(max_tokens);

        // ── Hippocampus path: spreading activation (联想检索) ──
        // The graph does associative retrieval: from seed matches, activation
        // spreads along causal edges to related memories that keyword search
        // would miss. Falls through to BM25/semantic if graph is unavailable
        // or finds nothing.
        if let Some(query) = params.query.as_deref().filter(|q| !q.trim().is_empty()) {
            if let Some(hippo_result) =
                self.hippocampus_search(query, params.task_tag.as_deref(), false, limit)
            {
                return hippo_result;
            }

            // ── Semantic path: embed + cosine ──
            // Requires a configured embedding endpoint; any failure falls back to
            // the BM25 path below.
            if let Some(Ok(vec)) = block_on(causal_memory::embed::embed_shared(query)) {
                let semantic = self
                    .store
                    .search_causal_semantic_entity_boosted(
                        &vec,
                        query,
                        params.task_tag.as_deref(),
                        limit,
                    )
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
                        let (line, cost) =
                            format_entry_layered(entry, i + 1, detail_level);
                        if !budget.try_spend(cost) {
                            out.push_str(&format!(
                                "… {} more result(s) truncated (token budget)\n",
                                results.len() - i
                            ));
                            break;
                        }
                        out.push_str(&line);
                    }
                    return out;
                }
                // embed or semantic search failed — fall through to BM25.
            }

            // BM25 keyword path: query present but no usable embedder. Unlike
            // the old LIKE substring match, BM25 ranks by token overlap, so
            // word order and phrasing differences no longer zero out hits.
            let results =
                match self
                    .store
                    .search_causal_bm25(params.task_tag.as_deref(), query, limit)
                {
                    Ok(r) => r,
                    Err(e) => return format!("❌ Search failed: {e}"),
                };
            if results.is_empty() {
                return "[bm25] 📭 No past causal episodes found matching your query.".to_string();
            }
            let mut out = format!("[bm25/{detail_level}] Found {} past episode(s)", results.len());
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
                out.push_str(&line);
            }
            return out;
        } // end hippocampus if let Some(query) block

        // Tag-only browsing (no query text) — original LIKE/listing path.
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
        name = "record_fact",
        description = "Record a flat fact for future retrieval: user preferences, tech stack, configuration, project facts. Use this for stable 'what is' information. Do NOT use it for causal relationships (decision → outcome lessons belong in record_decision). Re-recording the same fact is idempotent."
    )]
    fn record_fact(&self, Parameters(params): Parameters<RecordFactParams>) -> String {
        let scope = params.scope.as_deref().unwrap_or("user");
        if !matches!(scope, "user" | "session" | "agent") {
            return format!("❌ Invalid scope '{scope}' — use one of: user, session, agent");
        }
        let confidence = params.confidence.unwrap_or(0.8);

        // Optional same-key retirement runs atomically with the record
        // (single lock, single write batch) — no window where old and new
        // values are both valid.
        let (fact_id, retired) = if params.replace_same_key == Some(true) {
            match self.store.record_fact_replacing(
                &params.key,
                &params.value,
                scope,
                "agent",
                confidence,
            ) {
                Ok(v) => v,
                Err(e) => return format!("❌ Failed to record fact: {e}"),
            }
        } else {
            match self
                .store
                .record_fact(&params.key, &params.value, scope, "agent", confidence)
            {
                Ok(id) => (id, 0),
                Err(e) => return format!("❌ Failed to record fact: {e}"),
            }
        };

        // Opportunistic embedding (silent on any failure — must never block
        // recording; a CLI backfill path can catch up later).
        let text = format!("{} {}", params.key.replace('_', " "), params.value);
        if let Some(Ok(vec)) = block_on(causal_memory::embed::embed_shared(&text)) {
            let _ = self.store.put_fact_embedding(fact_id, "shared", &vec);
        }

        let mut out = format!(
            "✅ Recorded fact: [{}] {} = \"{}\" (confidence: {:.2}, id: {})",
            scope,
            params.key,
            truncate_chars(&params.value, 60),
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

    #[tool(
        name = "search_facts",
        description = "Search flat facts: user preferences, tech stack, configuration, project facts. Call this when you need 'what is' information. For causal lessons (decision → outcome), use search_causal instead. Without a query, lists the most recently updated facts."
    )]
    fn search_facts(&self, Parameters(params): Parameters<SearchFactsParams>) -> String {
        let limit = params.limit.unwrap_or(5);
        let scope = params.scope.as_deref();
        if let Some(s) = scope {
            if !matches!(s, "user" | "session" | "agent") {
                return format!("❌ Invalid scope '{s}' — use one of: user, session, agent");
            }
        }

        if let Some(query) = params.query.as_deref().filter(|q| !q.trim().is_empty()) {
            // Semantic path: embed + cosine (requires a configured endpoint).
            if let Some(Ok(vec)) = block_on(causal_memory::embed::embed_shared(query)) {
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

    #[tool(
        name = "search_memory",
        description = "Search ALL memory types at once: flat facts (preferences, tech stack, config) AND causal lessons (decision → outcome). Use this when you're not sure whether what you need is a fact or a lesson — results from every layer are fused by Reciprocal Rank Fusion (RRF) into one ranked list. For a deep dive into one layer, use search_facts or search_causal directly."
    )]
    fn search_memory(&self, Parameters(params): Parameters<SearchMemoryParams>) -> String {
        let limit = params.limit.unwrap_or(10);
        let scope = params.scope.as_deref();
        if let Some(s) = scope {
            if !matches!(s, "user" | "session" | "agent") {
                return format!("❌ Invalid scope '{s}' — use one of: user, session, agent");
            }
        }
        // Pull more than needed per layer so the fusion has real candidates.
        let per_layer = limit.saturating_mul(2).max(10);

        // Same retrieval discipline as the single-layer tools: semantic when
        // an embedder is configured, BM25 otherwise. One query embedding
        // serves both layers. Per-layer fallthrough: an empty/failed
        // semantic result (e.g. records stored without embeddings) degrades
        // that layer to BM25 instead of silently missing hits.
        let query_vec = block_on(causal_memory::embed::embed_shared(&params.query))
            .and_then(|r| r.ok());

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
                        .search_facts_bm25(&params.query, scope, per_layer)
                        .unwrap_or_default()
                } else {
                    used_semantic = true;
                    sem
                }
            }
            None => self
                .store
                .search_facts_bm25(&params.query, scope, per_layer)
                .unwrap_or_default(),
        };
        let causal: Vec<CausalEntry> = match &query_vec {
            Some(v) => {
                let sem: Vec<CausalEntry> = self
                    .store
                    .search_causal_semantic_entity_boosted(
                        v,
                        &params.query,
                        params.task_tag.as_deref(),
                        per_layer,
                    )
                    .map(|hits| hits.into_iter().map(|(e, _)| e).collect())
                    .unwrap_or_default();
                if sem.is_empty() {
                    self.store
                        .search_causal_bm25(params.task_tag.as_deref(), &params.query, per_layer)
                        .unwrap_or_default()
                } else {
                    used_semantic = true;
                    sem
                }
            }
            None => self
                .store
                .search_causal_bm25(params.task_tag.as_deref(), &params.query, per_layer)
                .unwrap_or_default(),
        };
        let mode = if used_semantic { "semantic" } else { "bm25" };

        if facts.is_empty() && causal.is_empty() {
            return format!(
                "[unified/{mode}] 📭 No memories found matching your query in any layer."
            );
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
            .search_causal_hop(&params.query, &seed_ids, per_layer)
            .unwrap_or_default();
        let mut hop_keys: Vec<String> = Vec::new();
        for e in &hop {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                causal_by_id.entry(e.edge_id)
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

        // Keep only items inside the fused top-`limit`, then group by layer
        // for display (each item annotated with its fused rank).
        let keep = |key: &str| rank_of.get(key).is_some_and(|r| *r <= limit);
        let facts_kept: Vec<&AgentFact> = facts
            .iter()
            .filter(|f| keep(&format!("fact:{}", f.id)))
            .collect();
        // Materialize from the merged causal pool (primary + hop neighbors),
        // displayed in fused-rank order.
        let mut causal_kept: Vec<&CausalEntry> = causal_by_id
            .iter()
            .filter(|(id, _)| keep(&format!("causal:{id}")))
            .map(|(_, e)| e)
            .collect();
        causal_kept.sort_by_key(|e| rank_of[format!("causal:{}", e.edge_id).as_str()]);

        let layers = usize::from(!facts_kept.is_empty()) + usize::from(!causal_kept.is_empty());
        let total = facts_kept.len() + causal_kept.len();
        let mut out =
            format!("[unified/{mode}] Found {total} memories across {layers} layer(s):\n\n");
        if !facts_kept.is_empty() {
            out.push_str(&format!("📊 Facts ({}):\n", facts_kept.len()));
            for fact in &facts_kept {
                let rank = rank_of[format!("fact:{}", fact.id).as_str()];
                out.push_str(&format!(
                    "  #{rank} [{}] {} = \"{}\" (confidence: {:.0}%)\n",
                    fact.scope,
                    fact.key,
                    truncate_chars(&fact.value, 60),
                    fact.confidence * 100.0,
                ));
            }
            out.push('\n');
        }
        if !causal_kept.is_empty() {
            out.push_str(&format!("🔗 Causal lessons ({}):\n", causal_kept.len()));
            for entry in &causal_kept {
                let rank = rank_of[format!("causal:{}", entry.edge_id).as_str()];
                out.push_str(&format!(
                    "  #{rank} [{}] \"{}\" →({})→ \"{}\" (confidence: {:.0}%)\n",
                    entry.task_tag.as_deref().unwrap_or("untagged"),
                    truncate_chars(&entry.decision_text, 50),
                    entry.relation,
                    truncate_chars(&entry.outcome_text, 50),
                    entry.confidence * 100.0,
                ));
            }
        }
        out
    }

    #[tool(
        name = "trace_cause",
        description = "When something went wrong, trace back which past decision could have caused it. Use for post-mortem analysis. Provide a description of the bad outcome."
    )]
    fn trace_cause(&self, Parameters(params): Parameters<TraceCauseParams>) -> String {
        // ── Hippocampus path: reverse spreading activation ──
        // Walk backward from the outcome through the causal graph to find
        // which decisions could have caused it. Activation spreads along
        // reverse causal edges, surfacing decisions that keyword search
        // would miss (e.g., a decision phrased differently from the query).
        if let Some(hippo_result) =
            self.hippocampus_search(&params.outcome_description, None, true, 5)
        {
            return hippo_result;
        }

        // ── SQL fallback: single-hop reverse lookup ──
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
                "{}. \"{}\" --[{label}]--> \"{}\"\n   {pattern}\n   confidence: {:.0}%\n",
                i + 1,
                edge.from_text,
                edge.to_text,
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

        // Semantic seed path: embed the action, find similar past decisions by
        // cosine, walk forward chains from them. Failure chain: BM25 ranks
        // similar decisions by token overlap and seeds chains from them; the
        // LIKE anchor is the last resort (pre-embedding behavior).
        let mut tag = "[keyword]";
        let chains =
            match self.semantic_effect_chains(&params.action, max_depth, min_confidence, limit) {
                Some(c) => {
                    tag = "[semantic]";
                    c
                }
                None => {
                    let bm25_seeds = self
                        .store
                        .search_causal_bm25(None, &params.action, limit)
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
                        None => match self.store.trace_effect_chain(
                            &params.action,
                            max_depth,
                            min_confidence,
                        ) {
                            Ok(c) => c,
                            Err(e) => return format!("❌ Intervention query failed: {e}"),
                        },
                    }
                }
            };

        if chains.is_empty() {
            return format!(
                "{tag} 📭 No precedent found for \"{}\" — absence of evidence is not evidence of safety. Proceed with caution, and record the outcome afterward with record_decision.",
                params.action
            );
        }

        // Stratified adjustment (engineering backdoor check): tag each chain
        // with its anchor edge's task_tag and its terminal outcome bucket,
        // then compare the reference stratum against the pooled evidence.
        // The optional task_tag param pins the reference stratum AND filters
        // the displayed chain list to it; the summary always sees all chains.
        // Chain traversal above is untouched — this is aggregation only.
        let pooled: Vec<(Option<String>, &'static str)> = chains
            .iter()
            .map(|c| {
                (
                    self.chain_stratum(c),
                    c.last().map(terminal_bucket).unwrap_or("neutral"),
                )
            })
            .collect();
        let reference = params.task_tag.clone().or_else(|| modal_stratum(&pooled));
        let summary = stratified_summary(&pooled, reference.as_deref());
        let display: Vec<&Vec<ChainHop>> = match params.task_tag.as_deref() {
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
            params.action,
            display.len(),
            show,
            max_depth
        );
        if display.is_empty() {
            out.push_str(&format!(
                "(no chains within task_tag={} — see the stratified summary below)\n\n",
                params.task_tag.as_deref().unwrap_or("?")
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

    /// The stratum (task_tag) of a chain's anchor edge — the similar past
    /// action this chain started from. None when untagged or lookup fails.
    fn chain_stratum(&self, chain: &[ChainHop]) -> Option<String> {
        chain
            .first()
            .and_then(|h| self.store.get_edge(h.edge_id).ok().flatten())
            .and_then(|e| e.task_tag)
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
        let Some(Ok(vec)) = block_on(causal_memory::embed::embed_shared(action)) else {
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

    #[tool(
        name = "counterfactual_query",
        description = "Contrastive (empirical) counterfactual: compare the recorded outcomes of a decision vs an alternative in similar past situations. Call when choosing between two concrete options BEFORE acting. Reports recorded evidence only — this is NOT a Pearl Rung-3 SCM counterfactual."
    )]
    fn counterfactual_query(&self, Parameters(params): Parameters<CounterfactualParams>) -> String {
        self.counterfactual_inner(
            &params.decision,
            &params.alternative,
            params.task_tag.as_deref(),
            params.limit.unwrap_or(5),
        )
    }

    /// Counterfactual comparison with the embedder injected (None = keyword
    /// path, identical to unconfigured — keeps tests hermetic).
    pub(crate) fn counterfactual_inner(
        &self,
        decision: &str,
        alternative: &str,
        task_tag: Option<&str>,
        limit: usize,
    ) -> String {
        let (dist_a, reps_a, tag_a) = self.side_evidence(decision, task_tag, limit);
        let (dist_b, reps_b, tag_b) = self.side_evidence(alternative, task_tag, limit);
        let tag = if tag_a == "semantic" && tag_b == "semantic" {
            "[semantic]"
        } else {
            "[bm25]"
        };

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
        out.push_str(&format!(
            "\nConclusion: {}\n",
            counterfactual_verdict(&dist_a, &dist_b)
        ));
        out
    }

    /// One side of the counterfactual: retrieve similar past decision edges
    /// (semantic with BM25 fallback, same pattern as search_causal) and
    /// aggregate their outcome distribution + representative outcomes.
    fn side_evidence(
        &self,
        query: &str,
        task_tag: Option<&str>,
        limit: usize,
    ) -> (CfDist, Vec<String>, &'static str) {
        let semantic = block_on(causal_memory::embed::embed_shared(query)).and_then(|r| {
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
        let (entries, path) = match semantic {
            Some(e) => (e, "semantic"),
            None => {
                let entries = self
                    .store
                    .search_causal_bm25(task_tag, query, limit)
                    .unwrap_or_default();
                (entries, "bm25")
            }
        };

        let mut dist = CfDist::default();
        for e in &entries {
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
        (dist, reps, path)
    }

    #[tool(
        name = "reconstruct_lesson",
        description = "Reconstructive retrieval (Schacter 2007): fetch the Markov-blanket causal subgraph around a topic and, when an LLM is configured, reconstruct a coherent lesson narrative from it instead of returning raw records. Call when you want the distilled lesson of a past episode rather than individual edges. Optional calibrate=N (>= 2) generates N independent reconstructions and warns when they disagree — disagreement marks unreliable memories."
    )]
    fn reconstruct_lesson(
        &self,
        Parameters(params): Parameters<ReconstructLessonParams>,
    ) -> String {
        let llm = causal_memory::llm::LlmConfig::from_env();
        self.reconstruct_lesson_inner(
            &params.query,
            params.max_edges.unwrap_or(20),
            params.calibrate.unwrap_or(0),
            llm.as_ref(),
        )
    }

    /// Reconstruct pipeline with embedder/LLM injected (None, None = local
    /// subgraph only — keeps tests hermetic and honors zero-intrusion).
    pub(crate) fn reconstruct_lesson_inner(
        &self,
        query: &str,
        max_edges: usize,
        calibrate: usize,
        llm: Option<&causal_memory::llm::LlmConfig>,
    ) -> String {
        // 1. Subgraph layer (always local): seed via semantic/BM25, then the
        //    Markov blanket around the seeds, serialized as compact stubs.
        let mut tag = "[bm25]";
        let mut seeds: Vec<causal_memory::store::CausalEntry> = Vec::new();
        if let Some(Ok(vec)) = block_on(causal_memory::embed::embed_shared(query)) {
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
        match block_on(causal_memory::llm::reconstruct_narrative(
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
                if let Ok(t) = block_on(causal_memory::llm::reconstruct_narrative(
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

// ─── ServerHandler implementation ──────────────────────────────────────────
// The #[tool_handler] macro auto-generates get_info(), list_tools(), call_tool().

// The handler macro must live beside the router (it references the
// macro-generated private router fn).
#[tool_handler]
impl ServerHandler for CausalMemoryServer {}
