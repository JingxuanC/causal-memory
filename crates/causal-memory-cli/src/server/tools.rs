//! The 14 MCP tools — thin rmcp handlers over the shared library facade
//! (`causal_memory::memory::Memory`). All orchestration logic lives in the
//! library; this file only declares parameter schemas and delegates.

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;

use super::CausalMemoryServer;

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
        self.memory.record_decision(
            &params.decision,
            &params.outcome,
            &params.relation,
            &params.task_tag,
            params.confidence_source.as_deref(),
        )
    }

    #[tool(
        name = "remember",
        description = "Extract and store memories from conversation text. The system automatically identifies facts, lessons, and causal relationships (caused/enabled/prevented) using LLM analysis. Call this after any meaningful conversation exchange — just paste the conversation text, no need to manually identify decisions or outcomes."
    )]
    fn remember(&self, Parameters(params): Parameters<RememberParams>) -> String {
        self.memory.remember(&params.messages, params.date.as_deref())
    }

    #[tool(
        name = "search_causal",
        description = "Search past decisions and their outcomes for situations similar to your current task. Call this BEFORE attempting a non-trivial decision to learn from past experience. Filter by task_tag for domain-specific lessons, or use query text for broader search."
    )]
    fn search_causal(&self, Parameters(params): Parameters<SearchCausalParams>) -> String {
        self.memory.search_causal(
            params.task_tag.as_deref(),
            params.query.as_deref(),
            params.limit,
            params.detail_level.as_deref(),
            params.max_tokens,
        )
    }

    #[tool(
        name = "record_fact",
        description = "Record a flat fact for future retrieval: user preferences, tech stack, configuration, project facts. Use this for stable 'what is' information. Do NOT use it for causal relationships (decision → outcome lessons belong in record_decision). Re-recording the same fact is idempotent."
    )]
    fn record_fact(&self, Parameters(params): Parameters<RecordFactParams>) -> String {
        self.memory.record_fact(
            &params.key,
            &params.value,
            params.scope.as_deref(),
            params.confidence,
            params.replace_same_key,
        )
    }

    #[tool(
        name = "search_facts",
        description = "Search flat facts: user preferences, tech stack, configuration, project facts. Call this when you need 'what is' information. For causal lessons (decision → outcome), use search_causal instead. Without a query, lists the most recently updated facts."
    )]
    fn search_facts(&self, Parameters(params): Parameters<SearchFactsParams>) -> String {
        self.memory.search_facts(
            params.query.as_deref(),
            params.scope.as_deref(),
            params.limit,
        )
    }

    #[tool(
        name = "search_memory",
        description = "Search ALL memory types at once: flat facts (preferences, tech stack, config) AND causal lessons (decision → outcome). Use this when you're not sure whether what you need is a fact or a lesson — results from every layer are fused by Reciprocal Rank Fusion (RRF) into one ranked list. For a deep dive into one layer, use search_facts or search_causal directly."
    )]
    fn search_memory(&self, Parameters(params): Parameters<SearchMemoryParams>) -> String {
        self.memory.search_memory(
            &params.query,
            params.task_tag.as_deref(),
            params.scope.as_deref(),
            params.limit,
        )
    }

    #[tool(
        name = "trace_cause",
        description = "When something went wrong, trace back which past decision could have caused it. Use for post-mortem analysis. Provide a description of the bad outcome."
    )]
    fn trace_cause(&self, Parameters(params): Parameters<TraceCauseParams>) -> String {
        self.memory.trace_cause(&params.outcome_description)
    }

    #[tool(
        name = "trace_cause_chain",
        description = "Deep failure analysis: trace multi-hop causal chains backward from a bad outcome. Use when a single-hop trace doesn't reveal the root cause. E.g., 'service crashed' ← 'OOM' ← 'cache had no TTL' ← 'Redis configured without expiry'. Parameters: outcome_description, max_depth (default 3), min_confidence (default 0.5), limit (default 5)."
    )]
    fn trace_cause_chain(&self, Parameters(params): Parameters<TraceCauseChainParams>) -> String {
        self.memory.trace_cause_chain(
            &params.outcome_description,
            params.max_depth,
            params.min_confidence,
            params.limit,
        )
    }

    #[tool(
        name = "invalidate_decision",
        description = "Mark a past causal lesson as wrong (soft-invalidate). The edge is hidden from all future search/trace results but kept in the database for audit. Use when you or the user discover that a recorded decision→outcome link was incorrect."
    )]
    fn invalidate_decision(
        &self,
        Parameters(params): Parameters<InvalidateDecisionParams>,
    ) -> String {
        self.memory
            .invalidate_decision(params.edge_id, params.reason.as_deref())
    }

    #[tool(
        name = "search_patterns",
        description = "Search mined cross-task patterns (meta-causal edges): decisions that are similar_to each other, repeated across tasks, contradicts each other, or refines an earlier failed attempt. Use this to recall abstracted lessons that span multiple task domains."
    )]
    fn search_patterns(&self, Parameters(params): Parameters<SearchPatternsParams>) -> String {
        self.memory.search_patterns(
            params.query.as_deref(),
            params.task_tag.as_deref(),
            params.limit,
        )
    }

    #[tool(
        name = "causal_directory",
        description = "L0 directory of your recent decisions and their outcomes — a compact pointer list meant to be pinned in the agent's system prompt so it always knows what past experience it holds. Entries are one-line pointers; use trace_cause / search_causal / intervention_query with the decision texts for full details."
    )]
    fn causal_directory(&self, Parameters(params): Parameters<CausalDirectoryParams>) -> String {
        self.memory.causal_directory(params.limit)
    }

    #[tool(
        name = "intervention_query",
        description = "Pearl Rung-2 intervention: BEFORE taking an action, query what outcomes similar past actions caused. Returns predicted effects with causal paths and confidence, labeled safe/warning/danger."
    )]
    fn intervention_query(
        &self,
        Parameters(params): Parameters<InterventionQueryParams>,
    ) -> String {
        self.memory.intervention_query(
            &params.action,
            params.task_tag.as_deref(),
            params.max_depth,
            params.limit,
        )
    }

    #[tool(
        name = "counterfactual_query",
        description = "Contrastive (empirical) counterfactual: compare the recorded outcomes of a decision vs an alternative in similar past situations. Call when choosing between two concrete options BEFORE acting. Reports recorded evidence only — this is NOT a Pearl Rung-3 SCM counterfactual."
    )]
    fn counterfactual_query(&self, Parameters(params): Parameters<CounterfactualParams>) -> String {
        self.memory.counterfactual_query(
            &params.decision,
            &params.alternative,
            params.task_tag.as_deref(),
            params.limit,
        )
    }

    #[tool(
        name = "reconstruct_lesson",
        description = "Reconstructive retrieval (Schacter 2007): fetch the Markov-blanket causal subgraph around a topic and, when an LLM is configured, reconstruct a coherent lesson narrative from it instead of returning raw records. Call when you want the distilled lesson of a past episode rather than individual edges. Optional calibrate=N (>= 2) generates N independent reconstructions and warns when they disagree — disagreement marks unreliable memories."
    )]
    fn reconstruct_lesson(
        &self,
        Parameters(params): Parameters<ReconstructLessonParams>,
    ) -> String {
        self.memory
            .reconstruct_lesson(&params.query, params.max_edges, params.calibrate)
    }
}

// ─── ServerHandler implementation ──────────────────────────────────────────
// The #[tool_handler] macro auto-generates get_info(), list_tools(), call_tool().

// The handler macro must live beside the router (it references the
// macro-generated private router fn).
#[tool_handler]
impl ServerHandler for CausalMemoryServer {}
