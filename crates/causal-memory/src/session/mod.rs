//! Uniform session parsing abstraction.
//!
//! Each agent (grok, claude-code, ...) produces session artifacts in its own
//! format. [`SessionParser`] abstracts "parse agent session → format-agnostic
//! intermediate representation" so that the decision/reasoning extraction
//! logic in `extractor` / `reasoning_extractor` never touches agent-specific
//! JSON. Adding support for a new agent = one new parser behind the trait.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use anyhow::Result;

pub mod claude;
pub mod codex;
pub mod grok;
pub mod kimi;

pub use claude::ClaudeParser;
pub use codex::CodexParser;
pub use grok::GrokParser;
pub use kimi::KimiParser;

/// Where a session lives and how it should be read.
#[derive(Debug, Clone)]
pub struct SessionSource {
    pub path: PathBuf,
    pub kind: SourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A directory (e.g. grok's `~/.grok/sessions/<ws>/<id>/`).
    Dir,
    /// A single file (e.g. claude-code's `<session>.jsonl`).
    File,
}

impl SessionSource {
    pub fn dir(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            kind: SourceKind::Dir,
        }
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            kind: SourceKind::File,
        }
    }
}

/// Format-agnostic parsed session.
///
/// `extractor::DecisionExtractor` and `reasoning_extractor::ReasoningExtractor`
/// consume only this — a parser for any new agent just needs to fill these fields.
#[derive(Debug, Default)]
pub struct ParsedSession {
    /// Tool-call style decisions (candidate decision→outcome pairs).
    pub decisions: Vec<CandidateDecision>,
    /// Per-decision result, keyed by decision id.
    pub results: HashMap<String, CandidateResult>,
    /// Ordered outcome events (success/error/timeout) with timestamps.
    pub events: VecDeque<CandidateEvent>,
    /// Assistant reasoning texts (feed `reasoning_extractor`).
    pub assistant_texts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CandidateDecision {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments; consumed by `DecisionExtractor::summarize_args`.
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct CandidateResult {
    pub content: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CandidateEvent {
    pub tool_name: String,
    pub outcome: String,
    pub ts: Option<String>,
}

/// A parser for one agent's session format.
pub trait SessionParser: Send + Sync {
    fn parse(&self, source: &SessionSource) -> Result<ParsedSession>;
}

/// Known agent kinds. Add a variant + adapter when a new agent is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Grok,
    ClaudeCode,
    Kimi,
    Codex,
}

/// Resolve an agent kind from its CLI string.
pub fn agent_kind_from_str(s: &str) -> Option<AgentKind> {
    match s.to_ascii_lowercase().as_str() {
        "grok" | "grok-build" => Some(AgentKind::Grok),
        "claude" | "claude-code" | "claude_code" => Some(AgentKind::ClaudeCode),
        "kimi" | "openclaw" | "kimi-claw" => Some(AgentKind::Kimi),
        "codex" | "openai-codex" | "codex-cli" => Some(AgentKind::Codex),
        _ => None,
    }
}

/// Default source kind for a given agent.
pub fn default_source_kind(kind: AgentKind) -> SourceKind {
    match kind {
        AgentKind::Grok => SourceKind::Dir,
        AgentKind::ClaudeCode => SourceKind::File,
        AgentKind::Kimi => SourceKind::File,
        AgentKind::Codex => SourceKind::File,
    }
}

/// Dispatch to the parser implementation for an agent kind.
pub fn parser_for(kind: AgentKind) -> Box<dyn SessionParser> {
    match kind {
        AgentKind::Grok => Box::new(GrokParser),
        AgentKind::ClaudeCode => Box::new(ClaudeParser),
        AgentKind::Kimi => Box::new(KimiParser),
        AgentKind::Codex => Box::new(CodexParser),
    }
}
