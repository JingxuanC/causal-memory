//! MCP server handler — exposes 10 tools for causal memory.
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
//! - counterfactual_query: contrastive (empirical) counterfactual — agent
//!   calls when choosing between two concrete options, to compare recorded
//!   outcomes of each (NOT a Pearl Rung-3 SCM counterfactual).
//! - reconstruct_lesson: reconstructive retrieval — agent calls to get the
//!   distilled lesson of a past episode as an LLM narrative over the causal
//!   subgraph, instead of raw records.

use rmcp::{
    handler::server::wrapper::Parameters, schemars, tool, tool_handler, tool_router, ServerHandler,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;

use causal_memory::embed::{EmbedConfig, Embedder};
use causal_memory::store::{outcome_polarity, CausalStore, ChainHop};

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

/// Cosine floor for semantic seeding in intervention_query (recall-oriented).
const INTERVENTION_MIN_SIMILARITY: f64 = 0.5;
/// Cosine floor for the semantic contradiction scan on record (precision-
/// oriented: only paraphrase-level duplicates of the same decision).
const SEMANTIC_CONTRADICTION_MIN_SIMILARITY: f64 = 0.85;

/// Write-time outcome polarity: LLM judge when an LLM is configured, falling
/// back to the signal-word heuristic on any failure or when unconfigured
/// (Some(true)→positive, Some(false)→negative, None→neutral).
fn judge_outcome_polarity(decision: &str, outcome: &str) -> String {
    if let Some(config) = causal_memory::llm::LlmConfig::from_env() {
        if let Ok(pol) = block_on(causal_memory::llm::judge_polarity(
            &config, decision, outcome,
        )) {
            return pol;
        }
        // LLM failed — fall through to the heuristic.
    }
    match outcome_polarity(outcome) {
        Some(true) => "positive",
        Some(false) => "negative",
        None => "neutral",
    }
    .to_string()
}

/// Label a forward (intervention) chain by its terminal hop. Stored polarity
/// (v4) wins over the text heuristic; `mixed` gets its own WARNING label
/// instead of being forced into SAFE/DANGER. A failure outcome that a
/// `prevented` edge on the path blocked before downgrades DANGER → UNKNOWN.
fn chain_label(
    terminal_polarity: Option<&str>,
    terminal_text: &str,
    has_prevented: bool,
) -> &'static str {
    if terminal_polarity == Some("mixed") {
        return "⚠️ WARNING (mixed outcome)";
    }
    match causal_memory::store::effective_polarity(terminal_polarity, terminal_text) {
        Some(false) if has_prevented => {
            "ℹ️ UNKNOWN (failure outcome, but a prevented edge on this path blocked it before)"
        }
        Some(false) => "⚠️ DANGER",
        Some(true) => "✅ SAFE",
        _ => "ℹ️ UNKNOWN",
    }
}

/// Polarity bucket from (stored polarity, outcome text): stored wins, NULL
/// falls back to the signal-word heuristic.
fn polarity_bucket(stored: Option<&str>, text: &str) -> &'static str {
    match stored {
        Some("positive") => "positive",
        Some("negative") => "negative",
        Some("mixed") => "mixed",
        Some(_) => "neutral",
        None => match outcome_polarity(text) {
            Some(true) => "positive",
            Some(false) => "negative",
            None => "neutral",
        },
    }
}

/// Terminal-outcome bucket for stratified aggregation: stored polarity (v4)
/// wins; NULL falls back to the signal-word heuristic.
fn terminal_bucket(hop: &ChainHop) -> &'static str {
    polarity_bucket(hop.outcome_polarity.as_deref(), &hop.outcome_text)
}

/// Outcome distribution of one group of chains ("other" = mixed + neutral
/// terminal buckets).
#[derive(Default)]
struct StratumDist {
    positive: usize,
    negative: usize,
    other: usize,
}

impl StratumDist {
    fn add(&mut self, bucket: &str) {
        match bucket {
            "positive" => self.positive += 1,
            "negative" => self.negative += 1,
            _ => self.other += 1,
        }
    }
    fn total(&self) -> usize {
        self.positive + self.negative + self.other
    }
    /// Majority direction; "mixed" on a tie or an empty group.
    fn direction(&self) -> &'static str {
        if self.positive > self.negative {
            "positive"
        } else if self.negative > self.positive {
            "negative"
        } else {
            "mixed"
        }
    }
}

/// Most frequent non-None stratum (ties → first seen); None when every chain
/// is untagged.
fn modal_stratum(chains: &[(Option<String>, &str)]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for (tag, _) in chains {
        if let Some(t) = tag {
            match counts.iter_mut().find(|(k, _)| *k == t.as_str()) {
                Some((_, n)) => *n += 1,
                None => counts.push((t.as_str(), 1)),
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(t, _)| t.to_string())
}

/// Stratified summary block appended to intervention_query output: pooled vs
/// reference-stratum terminal-outcome distribution, with a Simpson warning
/// when the pooled majority and the stratum majority point in opposite
/// directions (the pooled estimate is then likely confounded by task_tag).
/// Returns an empty string when there are no chains.
fn stratified_summary(chains: &[(Option<String>, &str)], reference: Option<&str>) -> String {
    if chains.is_empty() {
        return String::new();
    }
    let mut pooled = StratumDist::default();
    let mut within = StratumDist::default();
    let mut across = StratumDist::default();
    for (tag, bucket) in chains {
        pooled.add(bucket);
        match reference {
            Some(r) if tag.as_deref() == Some(r) => within.add(bucket),
            _ => across.add(bucket),
        }
    }

    let mut out = String::from("Stratified by task_tag (terminal outcomes):\n");
    out.push_str(&format!(
        "  pooled (n={}): {} positive / {} negative / {} other → {}\n",
        pooled.total(),
        pooled.positive,
        pooled.negative,
        pooled.other,
        pooled.direction()
    ));
    if let Some(r) = reference {
        out.push_str(&format!(
            "  task_tag={r} (n={}): {} positive / {} negative / {} other → {}\n",
            within.total(),
            within.positive,
            within.negative,
            within.other,
            within.direction()
        ));
        out.push_str(&format!(
            "  other strata (n={}): {} positive / {} negative / {} other → {}\n",
            across.total(),
            across.positive,
            across.negative,
            across.other,
            across.direction()
        ));
        let (p, w) = (pooled.direction(), within.direction());
        if within.total() > 0 && across.total() > 0 && p != "mixed" && w != "mixed" && p != w {
            out.push_str(&format!(
                "  ⚠️ Simpson's paradox: pooled result is {p} but within task_tag={r} it is {w} — pooled estimate likely confounded\n"
            ));
        }
    }
    out
}

/// Outcome distribution of one side of a counterfactual comparison.
#[derive(Default)]
struct CfDist {
    positive: usize,
    negative: usize,
    mixed: usize,
    neutral: usize,
}

impl CfDist {
    fn add(&mut self, bucket: &str) {
        match bucket {
            "positive" => self.positive += 1,
            "negative" => self.negative += 1,
            "mixed" => self.mixed += 1,
            _ => self.neutral += 1,
        }
    }
    fn total(&self) -> usize {
        self.positive + self.negative + self.mixed + self.neutral
    }
    /// Net evidence score: positive counts +1, negative -1, mixed -0.5.
    fn score(&self) -> f64 {
        self.positive as f64 - self.negative as f64 - 0.5 * self.mixed as f64
    }
}

/// Conclusion of a counterfactual comparison between two outcome
/// distributions. Deterministic: the side with the higher net evidence score
/// wins; equal scores (or missing data) are honestly "insufficient".
fn counterfactual_verdict(a: &CfDist, b: &CfDist) -> String {
    match (a.total() == 0, b.total() == 0) {
        (true, true) => "📭 insufficient evidence: no recorded episodes for either option — record outcomes with record_decision to build it.".to_string(),
        (true, false) => "insufficient evidence: no recorded episodes matching option A.".to_string(),
        (false, true) => "insufficient evidence: no recorded episodes matching option B.".to_string(),
        (false, false) => {
            let (sa, sb) = (a.score(), b.score());
            if sa > sb {
                format!("recorded evidence favors A (net {sa:+.1} vs {sb:+.1})")
            } else if sb > sa {
                format!("recorded evidence favors B (net {sb:+.1} vs {sa:+.1})")
            } else {
                format!("insufficient evidence to distinguish (both net {sa:+.1})")
            }
        }
    }
}

/// Char-safe truncation to at most `n` chars, appending "…" when cut.
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars().take(n.saturating_sub(1)).collect::<String>()
    )
}

/// Serialize one edge as a compact stub for LLM context (≤ 120 chars).
fn edge_stub(e: &causal_memory::store::CausalEntry) -> String {
    let pol = e.outcome_polarity.as_deref().unwrap_or("?");
    let base = format!(
        "#{} {} conf={:.2} pol={pol}",
        e.edge_id, e.relation, e.confidence
    );
    let overhead = base.chars().count() + " | \"\" → \"\"".chars().count();
    let budget = 120usize.saturating_sub(overhead).max(2);
    let half = budget / 2;
    let d = truncate_chars(&e.decision_text, half);
    let o = truncate_chars(&e.outcome_text, budget - half);
    format!("{base} | \"{d}\" → \"{o}\"")
}

/// Mean pairwise Jaccard similarity over the token sets of independent
/// reconstructions (multi-sample calibration). 1.0 = perfect agreement;
/// below the caller's threshold the underlying memories are flagged as
/// potentially unreliable. Fewer than 2 texts → 1.0 (nothing to compare).
fn reconstruction_agreement(texts: &[String]) -> f64 {
    if texts.len() < 2 {
        return 1.0;
    }
    let sets: Vec<std::collections::HashSet<String>> = texts
        .iter()
        .map(|t| causal_memory::patterns::tokenize(t).into_iter().collect())
        .collect();
    let mut sum = 0.0;
    let mut pairs = 0usize;
    for i in 0..sets.len() {
        for j in i + 1..sets.len() {
            let union = sets[i].union(&sets[j]).count();
            let sim = if union == 0 {
                1.0 // two empty texts agree vacuously
            } else {
                sets[i].intersection(&sets[j]).count() as f64 / union as f64
            };
            sum += sim;
            pairs += 1;
        }
    }
    sum / pairs as f64
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

        match self.store.record_decision_full(
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
        // the BM25 path below.
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
            let mut out = format!("[bm25] Found {} past episode(s):\n\n", results.len());
            for (i, entry) in results.iter().enumerate() {
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
            return out;
        }

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
        let embedder = EmbedConfig::from_env().map(Embedder::new)?;
        let vec = block_on(embedder.embed(action)).ok()?;
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
        let embedder = EmbedConfig::from_env().map(Embedder::new);
        self.counterfactual_inner(
            &params.decision,
            &params.alternative,
            params.task_tag.as_deref(),
            params.limit.unwrap_or(5),
            embedder.as_ref(),
        )
    }

    /// Counterfactual comparison with the embedder injected (None = keyword
    /// path, identical to unconfigured — keeps tests hermetic).
    fn counterfactual_inner(
        &self,
        decision: &str,
        alternative: &str,
        task_tag: Option<&str>,
        limit: usize,
        embedder: Option<&Embedder>,
    ) -> String {
        let (dist_a, reps_a, tag_a) = self.side_evidence(decision, task_tag, limit, embedder);
        let (dist_b, reps_b, tag_b) = self.side_evidence(alternative, task_tag, limit, embedder);
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
        embedder: Option<&Embedder>,
    ) -> (CfDist, Vec<String>, &'static str) {
        let semantic = embedder.and_then(|emb| {
            let vec = block_on(emb.embed(query)).ok()?;
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
        let embedder = EmbedConfig::from_env().map(Embedder::new);
        let llm = causal_memory::llm::LlmConfig::from_env();
        self.reconstruct_lesson_inner(
            &params.query,
            params.max_edges.unwrap_or(20),
            params.calibrate.unwrap_or(0),
            embedder.as_ref(),
            llm.as_ref(),
        )
    }

    /// Reconstruct pipeline with embedder/LLM injected (None, None = local
    /// subgraph only — keeps tests hermetic and honors zero-intrusion).
    fn reconstruct_lesson_inner(
        &self,
        query: &str,
        max_edges: usize,
        calibrate: usize,
        embedder: Option<&Embedder>,
        llm: Option<&causal_memory::llm::LlmConfig>,
    ) -> String {
        // 1. Subgraph layer (always local): seed via semantic/BM25, then the
        //    Markov blanket around the seeds, serialized as compact stubs.
        let mut tag = "[bm25]";
        let mut seeds: Vec<causal_memory::store::CausalEntry> = Vec::new();
        if let Some(emb) = embedder {
            if let Ok(vec) = block_on(emb.embed(query)) {
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

#[tool_handler]
impl ServerHandler for CausalMemoryServer {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chain_label_stored_polarity() {
        // Stored polarity wins over the text, whatever the text says.
        assert_eq!(
            chain_label(Some("negative"), "一切正常", false),
            "⚠️ DANGER"
        );
        assert_eq!(
            chain_label(Some("positive"), "deadlock occurred", false),
            "✅ SAFE"
        );
        // mixed gets its own WARNING, never forced into SAFE/DANGER — even
        // when the text heuristic would call it a success.
        assert_eq!(
            chain_label(
                Some("mixed"),
                "deadlock under load; fixed by switching to channels",
                false
            ),
            "⚠️ WARNING (mixed outcome)"
        );
        assert_eq!(
            chain_label(Some("mixed"), "deadlock under load; fixed later", true),
            "⚠️ WARNING (mixed outcome)"
        );
        // neutral is never SAFE/DANGER.
        assert_eq!(
            chain_label(Some("neutral"), "deploy finished", false),
            "ℹ️ UNKNOWN"
        );
    }

    #[test]
    fn test_chain_label_prevented_downgrade() {
        let downgraded =
            "ℹ️ UNKNOWN (failure outcome, but a prevented edge on this path blocked it before)";
        // Stored negative + a prevented edge on the path → UNKNOWN.
        assert_eq!(chain_label(Some("negative"), "x", true), downgraded);
        // Heuristic failure + prevented → UNKNOWN (pre-v4 behavior preserved).
        assert_eq!(chain_label(None, "service crashed", true), downgraded);
        // positive/neutral are unaffected by prevented.
        assert_eq!(chain_label(Some("positive"), "x", true), "✅ SAFE");
        assert_eq!(chain_label(Some("neutral"), "x", true), "ℹ️ UNKNOWN");
    }

    #[test]
    fn test_chain_label_heuristic_fallback() {
        // NULL stored polarity → identical to the pre-v4 text heuristic.
        assert_eq!(
            chain_label(None, "deadlock — holder crashed", false),
            "⚠️ DANGER"
        );
        assert_eq!(
            chain_label(None, "successfully fixed race", false),
            "✅ SAFE"
        );
        assert_eq!(chain_label(None, "deploy finished", false), "ℹ️ UNKNOWN");
        // The documented heuristic quirk is preserved on the fallback path:
        // failure+success in one text counts as success (use chain_label with
        // a stored 'mixed' to get the WARNING instead).
        assert_eq!(
            chain_label(
                None,
                "Deadlock under concurrent load; fixed by switching to channel-based ownership",
                false
            ),
            "✅ SAFE"
        );
    }

    #[test]
    fn test_stratified_summary_simpson_warning() {
        // Same action family, opposite outcomes per stratum: 2 negative in
        // caching, 3 positive in auth → pooled leans positive while the
        // caching stratum is purely negative → Simpson warning.
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (Some("caching".to_string()), "negative"),
            (Some("auth".to_string()), "positive"),
            (Some("auth".to_string()), "positive"),
            (Some("auth".to_string()), "positive"),
        ];
        let s = stratified_summary(&chains, Some("caching"));
        assert!(s.contains("pooled (n=5): 3 positive / 2 negative / 0 other → positive"));
        assert!(s.contains("task_tag=caching (n=2): 0 positive / 2 negative / 0 other → negative"));
        assert!(s.contains("other strata (n=3)"));
        assert!(
            s.contains(
                "Simpson's paradox: pooled result is positive but within task_tag=caching it is negative"
            ),
            "warning must name both directions: {s}"
        );
    }

    #[test]
    fn test_stratified_summary_single_stratum_no_warning() {
        // All chains in one stratum → nothing to confound, no warning.
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (Some("caching".to_string()), "negative"),
        ];
        let s = stratified_summary(&chains, Some("caching"));
        assert!(s.contains("task_tag=caching (n=2)"));
        assert!(s.contains("other strata (n=0)"));
        assert!(!s.contains("Simpson"));

        // Same-direction strata → no warning either.
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (Some("auth".to_string()), "negative"),
        ];
        let s = stratified_summary(&chains, Some("caching"));
        assert!(!s.contains("Simpson"));

        // Empty input → empty block.
        assert!(stratified_summary(&[], Some("caching")).is_empty());
    }

    #[test]
    fn test_modal_stratum() {
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (None, "positive"),
            (Some("auth".to_string()), "positive"),
            (Some("caching".to_string()), "positive"),
        ];
        assert_eq!(modal_stratum(&chains).as_deref(), Some("caching"));
        // All untagged → no reference stratum.
        let chains = vec![(None, "positive"), (None, "negative")];
        assert_eq!(modal_stratum(&chains), None);
    }

    #[test]
    fn test_intervention_params_task_tag_parsing() {
        let p: InterventionQueryParams =
            serde_json::from_str(r#"{"action":"use redis mutex","task_tag":"caching"}"#).unwrap();
        assert_eq!(p.action, "use redis mutex");
        assert_eq!(p.task_tag.as_deref(), Some("caching"));
        assert_eq!(p.max_depth, None);
        // Optional and absent by default.
        let p: InterventionQueryParams =
            serde_json::from_str(r#"{"action":"use redis mutex"}"#).unwrap();
        assert_eq!(p.task_tag, None);
    }

    // ── counterfactual_query ─────────────────────────────────────────────

    #[test]
    fn test_counterfactual_verdict() {
        let fav_a = (
            CfDist {
                positive: 2,
                ..Default::default()
            },
            CfDist {
                negative: 2,
                ..Default::default()
            },
        );
        assert!(counterfactual_verdict(&fav_a.0, &fav_a.1).contains("favors A"));
        assert!(counterfactual_verdict(&fav_a.1, &fav_a.0).contains("favors B"));
        // Both empty / one empty → insufficient.
        let empty = CfDist::default();
        assert!(counterfactual_verdict(&empty, &empty).contains("no recorded episodes for either"));
        assert!(counterfactual_verdict(&empty, &fav_a.0).contains("option A"));
        assert!(counterfactual_verdict(&fav_a.0, &empty).contains("option B"));
        // Equal net scores → honestly indistinguishable. mixed counts -0.5.
        let tie_a = CfDist {
            positive: 1,
            mixed: 2,
            ..Default::default()
        }; // 1 - 1 = 0
        let tie_b = CfDist {
            positive: 1,
            negative: 1,
            ..Default::default()
        }; // 0
        assert!(
            counterfactual_verdict(&tie_a, &tie_b).contains("insufficient evidence to distinguish")
        );
    }

    /// Build a server over an in-memory store with two edge families:
    /// "redis mutex …" edges (stored negative) and "channel …" (positive).
    fn counterfactual_server() -> CausalMemoryServer {
        let store = causal_memory::store::CausalStore::open_in_memory().unwrap();
        for (i, (dec, out, pol)) in [
            (
                "used redis mutex for cache",
                "deadlock under load",
                "negative",
            ),
            ("used redis mutex for queue", "deadlock again", "negative"),
            (
                "switched to channel ownership",
                "race fixed, all tests pass",
                "positive",
            ),
        ]
        .iter()
        .enumerate()
        {
            store
                .record_decision_full(
                    dec,
                    out,
                    "caused",
                    Some("concurrency"),
                    0.8,
                    "rule",
                    1000 + i as i64,
                    Some(pol),
                )
                .unwrap();
        }
        CausalMemoryServer::new(store)
    }

    #[test]
    fn test_counterfactual_inner_bm25_path() {
        let server = counterfactual_server();
        let out = server.counterfactual_inner("redis mutex", "channel", None, 5, None);
        assert!(out.starts_with(
            "⚠️ contrastive/empirical counterfactual over recorded alternatives — not a Pearl Rung-3 SCM counterfactual"
        ));
        assert!(out.contains("[bm25]"), "no embedder → BM25 tag: {out}");
        assert!(
            out.contains("A. \"redis mutex\" (n=2): 0 positive / 2 negative"),
            "{out}"
        );
        assert!(
            out.contains("B. \"channel\" (n=1): 1 positive / 0 negative"),
            "{out}"
        );
        assert!(out.contains("recorded evidence favors B"), "{out}");

        // One side without recorded episodes → insufficient.
        let out = server.counterfactual_inner("redis mutex", "nonexistent option", None, 5, None);
        assert!(
            out.contains("insufficient evidence: no recorded episodes matching option B"),
            "{out}"
        );

        // task_tag filter excludes everything → both sides empty.
        let out = server.counterfactual_inner("redis mutex", "channel", Some("other-tag"), 5, None);
        assert!(
            out.contains("no recorded episodes for either option"),
            "{out}"
        );
    }

    #[test]
    fn test_counterfactual_params_parsing() {
        let p: CounterfactualParams = serde_json::from_str(
            r#"{"decision":"use mutex","alternative":"use channel","task_tag":"concurrency","limit":3}"#,
        )
        .unwrap();
        assert_eq!(p.decision, "use mutex");
        assert_eq!(p.alternative, "use channel");
        assert_eq!(p.task_tag.as_deref(), Some("concurrency"));
        assert_eq!(p.limit, Some(3));
        let p: CounterfactualParams =
            serde_json::from_str(r#"{"decision":"a","alternative":"b"}"#).unwrap();
        assert_eq!(p.task_tag, None);
        assert_eq!(p.limit, None);
    }

    // ── reconstruct_lesson ───────────────────────────────────────────────

    #[test]
    fn test_edge_stub_format_and_cap() {
        let store = causal_memory::store::CausalStore::open_in_memory().unwrap();
        let long = "x".repeat(500);
        store
            .record_decision_full(
                &long,
                &long,
                "caused",
                None,
                0.8,
                "rule",
                1000,
                Some("mixed"),
            )
            .unwrap();
        let edge = store.get_edge(1).unwrap().unwrap();
        let stub = edge_stub(&edge);
        assert!(
            stub.chars().count() <= 120,
            "stub over budget: {}",
            stub.chars().count()
        );
        assert!(
            stub.starts_with("#1 caused conf=0.80 pol=mixed | \""),
            "{stub}"
        );
        assert!(stub.contains('…'), "long texts are truncated: {stub}");
    }

    #[test]
    fn test_reconstruction_agreement() {
        let a = "use channels to avoid deadlock in shared state".to_string();
        assert_eq!(reconstruction_agreement(&[a.clone(), a.clone()]), 1.0);
        // Completely disjoint token sets → 0.0.
        let b = "xyz qrs".to_string();
        assert_eq!(reconstruction_agreement(&[a.clone(), b]), 0.0);
        // Partial overlap lands strictly between.
        let c = "use channels to fix the parser".to_string();
        let mid = reconstruction_agreement(&[a.clone(), c]);
        assert!(mid > 0.0 && mid < 1.0, "got {mid}");
        // Fewer than two samples → vacuous 1.0.
        assert_eq!(reconstruction_agreement(&[a]), 1.0);
        assert_eq!(reconstruction_agreement(&[]), 1.0);
    }

    #[test]
    fn test_reconstruct_lesson_degraded_without_llm() {
        let server = counterfactual_server();
        let out = server.reconstruct_lesson_inner("redis mutex", 20, 0, None, None);
        assert!(
            out.contains("[bm25] Causal subgraph for \"redis mutex\""),
            "{out}"
        );
        // Stubs of the seeded edges are present, capped format.
        assert!(out.contains("conf=0.80 pol=negative"), "{out}");
        assert!(
            out.contains("(configure CAUSAL_MEMORY_LLM_* for narrative reconstruction)"),
            "{out}"
        );
        assert!(
            !out.contains("Reconstructed lesson"),
            "no LLM → no narrative: {out}"
        );

        // No matching history at all.
        let out = server.reconstruct_lesson_inner("totally unknown topic", 20, 0, None, None);
        assert!(out.contains("📭 No recorded causal context"), "{out}");
    }

    #[test]
    fn test_reconstruct_params_parsing() {
        let p: ReconstructLessonParams =
            serde_json::from_str(r#"{"query":"redis","max_edges":10,"calibrate":3}"#).unwrap();
        assert_eq!(p.query, "redis");
        assert_eq!(p.max_edges, Some(10));
        assert_eq!(p.calibrate, Some(3));
        let p: ReconstructLessonParams = serde_json::from_str(r#"{"query":"redis"}"#).unwrap();
        assert_eq!(p.max_edges, None);
        assert_eq!(p.calibrate, None);
    }
}
