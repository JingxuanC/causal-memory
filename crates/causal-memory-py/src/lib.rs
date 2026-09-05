//! Python bindings for causal-memory — a thin PyO3 shell over the shared
//! library facade `causal_memory::memory::Memory`. Every method mirrors one
//! MCP tool and returns the same text; agent frameworks (LangChain,
//! LlamaIndex, Hermes, …) consume these strings directly as tool outputs.

use cm::memory::Memory;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// Unified agent memory: facts, temporal state, and decision→outcome causal
/// edges on one SQLite store, with a hippocampus-style retrieval engine.
///
/// Example:
///     >>> from causal_memory import CausalMemory
///     >>> mem = CausalMemory("~/.local/share/causal-memory/causal.db")
///     >>> mem.record_decision("used Redis mutex", "deadlock under load",
///     ...                     "caused", "concurrency")
///     >>> print(mem.search_causal(query="cache stampede protection"))
///
/// Optional embeddings / LLM features are configured through the same
/// environment variables as the MCP server (CAUSAL_MEMORY_EMBED_API,
/// CAUSAL_MEMORY_EMBED_KEY, CAUSAL_MEMORY_LLM_API, …); without them the
/// memory degrades gracefully to BM25-only retrieval.
#[pyclass(name = "CausalMemory")]
struct PyCausalMemory {
    inner: Memory,
}

/// Expand a leading `~` (Python users expect it; Rust does not).
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

#[pymethods]
impl PyCausalMemory {
    /// Open (or create) a memory database at `db_path`, running migrations.
    #[new]
    fn new(db_path: &str) -> PyResult<Self> {
        let path = expand_tilde(db_path);
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PyRuntimeError::new_err(format!("failed to create db dir: {e}")))?;
        }
        let inner = Memory::open(&path)
            .map_err(|e| PyRuntimeError::new_err(format!("failed to open memory db: {e}")))?;
        Ok(Self { inner })
    }

    /// An ephemeral in-memory memory (tests, scratch use).
    #[staticmethod]
    fn in_memory() -> PyResult<Self> {
        let inner = Memory::open_in_memory()
            .map_err(|e| PyRuntimeError::new_err(format!("failed to open memory db: {e}")))?;
        Ok(Self { inner })
    }

    /// Record a decision and its observed outcome as a causal memory.
    /// Call AFTER acting on a decision and observing the result.
    /// relation: caused / enabled / prevented / no_effect.
    /// confidence_source: temporal / rule / llm_inferred / user_feedback.
    /// context: short situation description — same task_tag+context
    /// becomes a comparable branch (fork) for counterfactual queries.
    #[pyo3(signature = (decision, outcome, relation, task_tag, confidence_source=None, context=None))]
    fn record_decision(
        &self,
        py: Python<'_>,
        decision: &str,
        outcome: &str,
        relation: &str,
        task_tag: &str,
        confidence_source: Option<&str>,
        context: Option<&str>,
    ) -> String {
        py.allow_threads(|| {
            self.inner.record_decision(
                decision,
                outcome,
                relation,
                task_tag,
                confidence_source,
                context,
            )
        })
    }

    /// Extract and store memories from conversation text (LLM auto-extracts
    /// facts, lessons, and causal edges). Zero-friction alternative to
    /// record_decision. date: YYYY-MM-DD, defaults to today.
    #[pyo3(signature = (messages, date=None))]
    fn remember(&self, py: Python<'_>, messages: &str, date: Option<&str>) -> String {
        py.allow_threads(|| self.inner.remember(messages, date))
    }

    /// Search past decisions and their outcomes for situations similar to
    /// your current task. Call BEFORE a non-trivial decision.
    /// detail_level: l0 (summary), l1 (overview), l2 (full, default).
    /// explain=True appends a provenance tag per hit ([seed] / [spread …]).
    #[pyo3(signature = (task_tag=None, query=None, limit=None, detail_level=None, max_tokens=None, explain=None))]
    fn search_causal(
        &self,
        py: Python<'_>,
        task_tag: Option<&str>,
        query: Option<&str>,
        limit: Option<usize>,
        detail_level: Option<&str>,
        max_tokens: Option<usize>,
        explain: Option<bool>,
    ) -> String {
        py.allow_threads(|| {
            self.inner
                .search_causal(task_tag, query, limit, detail_level, max_tokens, explain)
        })
    }

    /// Record a flat fact (preference / tech_stack / config / project).
    /// Idempotent; set replace_same_key=True to retire outdated values under
    /// the same key+scope. scope: user (default) / session / agent.
    #[pyo3(signature = (key, value, scope=None, confidence=None, replace_same_key=None))]
    fn record_fact(
        &self,
        py: Python<'_>,
        key: &str,
        value: &str,
        scope: Option<&str>,
        confidence: Option<f64>,
        replace_same_key: Option<bool>,
    ) -> String {
        py.allow_threads(|| {
            self.inner
                .record_fact(key, value, scope, confidence, replace_same_key)
        })
    }

    /// Search flat facts ('what is' information). Without a query, lists the
    /// most recently updated facts.
    #[pyo3(signature = (query=None, scope=None, limit=None))]
    fn search_facts(
        &self,
        py: Python<'_>,
        query: Option<&str>,
        scope: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        py.allow_threads(|| self.inner.search_facts(query, scope, limit))
    }

    /// Search ALL memory types at once — facts AND causal lessons fused by
    /// Reciprocal Rank Fusion into one ranked list. detail_level: l0
    /// (pointer) / l1 (overview) / l2 (full, default); max_tokens 0 =
    /// unlimited; explain=True appends a provenance tag per hit.
    #[allow(
        clippy::too_many_arguments,
        reason = "tool surface mirrors the MCP schema; kwargs-style from Python"
    )]
    #[pyo3(signature = (query, task_tag=None, scope=None, limit=None, detail_level=None, max_tokens=None, explain=None))]
    fn search_memory(
        &self,
        py: Python<'_>,
        query: &str,
        task_tag: Option<&str>,
        scope: Option<&str>,
        limit: Option<usize>,
        detail_level: Option<&str>,
        max_tokens: Option<usize>,
        explain: Option<bool>,
    ) -> String {
        py.allow_threads(|| {
            self.inner.search_memory(
                query,
                task_tag,
                scope,
                limit,
                detail_level,
                max_tokens,
                explain,
            )
        })
    }

    /// When something went wrong, trace back which past decision could have
    /// caused it (single-hop reverse lookup).
    fn trace_cause(&self, py: Python<'_>, outcome_description: &str) -> String {
        py.allow_threads(|| self.inner.trace_cause(outcome_description))
    }

    /// Deep failure analysis: trace multi-hop causal chains backward from a
    /// bad outcome. Defaults: max_depth=3, min_confidence=0.5, limit=5.
    #[pyo3(signature = (outcome_description, max_depth=None, min_confidence=None, limit=None))]
    fn trace_cause_chain(
        &self,
        py: Python<'_>,
        outcome_description: &str,
        max_depth: Option<usize>,
        min_confidence: Option<f64>,
        limit: Option<usize>,
    ) -> String {
        py.allow_threads(|| {
            self.inner
                .trace_cause_chain(outcome_description, max_depth, min_confidence, limit)
        })
    }

    /// Mark a past causal lesson as wrong (soft-invalidate; hidden from
    /// future search/trace results, kept in the DB for audit).
    #[pyo3(signature = (edge_id, reason=None))]
    fn invalidate_decision(&self, py: Python<'_>, edge_id: i64, reason: Option<&str>) -> String {
        py.allow_threads(|| self.inner.invalidate_decision(edge_id, reason))
    }

    /// Knowledge-update pass: LLM-judge repeated decisions whose outcomes
    /// diverged and supersede the falsified old lessons (preview by default).
    #[pyo3(signature = (limit=None, apply=false))]
    fn resolve_updates(&self, py: Python<'_>, limit: Option<usize>, apply: bool) -> String {
        py.allow_threads(|| self.inner.resolve_updates(limit, apply))
    }

    /// Search mined cross-task patterns (meta edges: similar_to / repeated /
    /// contradicts / refines).
    #[pyo3(signature = (query=None, task_tag=None, limit=None))]
    fn search_patterns(
        &self,
        py: Python<'_>,
        query: Option<&str>,
        task_tag: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        py.allow_threads(|| self.inner.search_patterns(query, task_tag, limit))
    }

    /// Mark a mined cross-task pattern (meta edge, the #N id shown by
    /// search_patterns) as wrong (soft-invalidate; hidden from
    /// search_patterns and spreading activation, kept for audit).
    #[pyo3(signature = (edge_id, reason=None))]
    fn invalidate_pattern(&self, py: Python<'_>, edge_id: i64, reason: Option<&str>) -> String {
        py.allow_threads(|| self.inner.invalidate_pattern(edge_id, reason))
    }

    /// L0 directory of recent decisions and their outcomes — a compact
    /// pointer list meant to be pinned in an agent's system prompt.
    #[pyo3(signature = (limit=None))]
    fn causal_directory(&self, py: Python<'_>, limit: Option<usize>) -> String {
        py.allow_threads(|| self.inner.causal_directory(limit))
    }

    /// Pearl Rung-2 intervention: BEFORE taking an action, query what
    /// outcomes similar past actions caused (safe / warning / danger).
    #[pyo3(signature = (action, task_tag=None, max_depth=None, limit=None))]
    fn intervention_query(
        &self,
        py: Python<'_>,
        action: &str,
        task_tag: Option<&str>,
        max_depth: Option<usize>,
        limit: Option<usize>,
    ) -> String {
        py.allow_threads(|| {
            self.inner
                .intervention_query(action, task_tag, max_depth, limit)
        })
    }

    /// Contrastive (empirical) counterfactual: compare the recorded outcomes
    /// of a decision vs an alternative. NOT a Pearl Rung-3 SCM counterfactual.
    /// Every verdict is logged as a falsifiable prediction (see
    /// prediction_report); same-context branches (forks) render when present.
    #[pyo3(signature = (decision, alternative, task_tag=None, limit=None))]
    fn counterfactual_query(
        &self,
        py: Python<'_>,
        decision: &str,
        alternative: &str,
        task_tag: Option<&str>,
        limit: Option<usize>,
    ) -> String {
        py.allow_threads(|| {
            self.inner
                .counterfactual_query(decision, alternative, task_tag, limit)
        })
    }

    /// Prediction-ledger calibration dashboard: accuracy of past
    /// counterfactual verdicts (overall / per method / per task_tag) plus
    /// pending predictions that will resolve when either option is recorded.
    fn prediction_report(&self, py: Python<'_>) -> String {
        py.allow_threads(|| self.inner.prediction_report())
    }

    /// Reconstructive retrieval: Markov-blanket causal subgraph around a
    /// topic, plus an LLM narrative when CAUSAL_MEMORY_LLM_* is configured.
    /// calibrate>=2 generates N independent reconstructions and reports
    /// their agreement.
    #[pyo3(signature = (query, max_edges=None, calibrate=None))]
    fn reconstruct_lesson(
        &self,
        py: Python<'_>,
        query: &str,
        max_edges: Option<usize>,
        calibrate: Option<usize>,
    ) -> String {
        py.allow_threads(|| self.inner.reconstruct_lesson(query, max_edges, calibrate))
    }
}

/// Console-script entry point (`causal-memory` command): forwards to the
/// CLI library dispatcher — pip users get the FULL command surface (MCP
/// stdio server by default, plus stats/sleep/setconfig/…). Reads Python's
/// sys.argv (Rust's own argv also carries the interpreter path, so there
/// is no fixed skip count); the returned int becomes the exit code via
/// `sys.exit`.
#[pyfunction]
fn _main(py: Python<'_>) -> PyResult<i32> {
    let argv: Vec<String> = py.import("sys")?.getattr("argv")?.extract()?;
    Ok(cm_cli::run(&argv[1..]))
}

/// causal-memory: an agent memory system with a causal core.
#[pymodule]
fn causal_memory(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyCausalMemory>()?;
    m.add_function(wrap_pyfunction!(_main, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
