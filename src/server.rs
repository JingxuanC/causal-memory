//! MCP server handler — exposes 3 tools for causal memory.
//!
//! Tools:
//! - record_decision: agent calls after completing an action, to log
//!   the decision and its outcome as a causal edge.
//! - search_causal: agent calls BEFORE a non-trivial decision, to check
//!   past lessons in the same task domain.
//! - trace_cause: agent calls when something fails, to find which past
//!   decision could have caused it.

use rmcp::{
    handler::server::wrapper::Parameters,
    schemars,
    ServerHandler,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::store::CausalStore;

pub struct CausalMemoryServer {
    store: CausalStore,
}

impl CausalMemoryServer {
    pub fn new(store: CausalStore) -> Self {
        Self { store }
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
    #[schemars(description = "Confidence source. Use: temporal, rule, llm_inferred, or user_feedback")]
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

// ─── Tool implementations ─────────────────────────────────────────────────

#[tool_router]
impl CausalMemoryServer {
    #[tool(name = "record_decision", description = "Record a decision and its observed outcome as a causal memory. Call this AFTER you've acted on a decision and observed the result, especially if the outcome was surprising or educational. This builds your experience base for future similar tasks.")]
    fn record_decision(
        &self,
        Parameters(params): Parameters<RecordDecisionParams>,
    ) -> String {
        let confidence = match params.confidence_source.as_deref() {
            Some("temporal") => 0.4,
            Some("rule") => 0.7,
            Some("user_feedback") => 0.95,
            _ => 0.6, // llm_inferred (default)
        };
        let source = params.confidence_source.as_deref().unwrap_or("llm_inferred");

        match self.store.record_decision(
            &params.decision,
            &params.outcome,
            &params.relation,
            Some(&params.task_tag),
            confidence,
            source,
        ) {
            Ok(id) => format!(
                "✅ Recorded: [{}] \"{}\" →({})→ \"{}\" (confidence: {:.2}, id: {})",
                params.task_tag,
                &params.decision[..params.decision.len().min(60)],
                params.relation,
                &params.outcome[..params.outcome.len().min(60)],
                confidence,
                id
            ),
            Err(e) => format!("❌ Failed to record: {e}"),
        }
    }

    #[tool(name = "search_causal", description = "Search past decisions and their outcomes for situations similar to your current task. Call this BEFORE attempting a non-trivial decision to learn from past experience. Filter by task_tag for domain-specific lessons, or use query text for broader search.")]
    fn search_causal(
        &self,
        Parameters(params): Parameters<SearchCausalParams>,
    ) -> String {
        let limit = params.limit.unwrap_or(5);

        let results = match self.store.search_causal(
            params.task_tag.as_deref(),
            params.query.as_deref(),
        ) {
            Ok(r) => r,
            Err(e) => return format!("❌ Search failed: {e}"),
        };

        if results.is_empty() {
            return "📭 No past causal episodes found matching your query.".to_string();
        }

        let count = results.len().min(limit);
        let mut out = format!("Found {} past episode(s) (showing {}):\n\n", results.len(), count);
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

    #[tool(name = "trace_cause", description = "When something went wrong, trace back which past decision could have caused it. Use for post-mortem analysis. Provide a description of the bad outcome.")]
    fn trace_cause(
        &self,
        Parameters(params): Parameters<TraceCauseParams>,
    ) -> String {
        match self.store.trace_cause(&params.outcome_description) {
            Ok(results) if results.is_empty() => {
                "📭 No past decisions found that match this outcome.".to_string()
            }
            Ok(results) => {
                let mut out = format!(
                    "Traced {} possible cause(s):\n\n",
                    results.len()
                );
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
