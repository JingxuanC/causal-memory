//! Decision extractor — extracts high-value decision→outcome pairs from
//! agent session logs into the causal store.
//!
//! v0.3 design change: only extract decisions that are WORTH remembering.
//! The old approach recorded every tool call → result, producing thousands
//! of low-value edges ("ran cargo build" → "exit: 0") that pollute retrieval.
//! The new approach uses a two-tier filter:
//!
//! Tier 1 (always record): failures — error, timeout, crash. These are the
//!   highest-value lessons for an agent.
//! Tier 2 (record if state-changing): write/edit operations that modify the
//!   codebase. These represent real decisions, not routine lookups.
//! Tier 3 (skip): read-only operations (grep, cat, ls, search, read, web fetch).
//!   These are information gathering, not decisions.
//!
//! The `DECISION_WORTHY_TOOLS` list is now a STATE-CHANGING tools list.
//! Read-only tools are explicitly excluded even if they appear in the session.

use std::collections::VecDeque;
use std::path::Path;

use anyhow::Result;

use crate::session::{CandidateEvent, GrokParser, ParsedSession, SessionParser, SessionSource};
use crate::store::CausalStore;

/// Tools that CHANGE the system state — only these produce decision-worthy
/// edges. Read-only tools (read/grep/ls/cat/search/webfetch) are explicitly
/// excluded because they represent information gathering, not decisions.
pub(crate) const STATE_CHANGING_TOOLS: &[&str] = &[
    // grok-build: write/edit
    "write",
    "search_replace",
    "image_gen",
    "image_edit",
    "spawn_subagent",
    "scheduler_create",
    "scheduler_delete",
    // claude-code: write/edit
    "Write",
    "Edit",
    "NotebookEdit",
    // kimi: write
    "write",
];

/// Read-only tools that should NEVER produce edges (information gathering).
const READ_ONLY_TOOLS: &[&str] = &[
    "run_terminal_command", // mostly read-only (grep, cat, ls) — handled separately
    "Bash",                 // claude code equivalent
    "exec",                 // kimi
    "exec_command",         // codex
    "read", "Read",         // file reads
    "WebSearch", "WebFetch",
    "update_goal", "update_plan",
    "kill_command_or_subagent",
    "list_dir", "List",
    "search_tool", "use_tool",
];

/// Result patterns that indicate a FAILURE (high-value edge).
const FAILURE_MARKERS: &[&str] = &[
    "error", "Error", "ERROR",
    "failed", "Failed", "FAILED",
    "panic", "Panic",
    "traceback", "Traceback",
    "exception", "Exception",
    "denied", "Permission denied",
    "not found", "No such file",
    "fatal", "FATAL",
    "exit: 1", "exit code: 1", "exit code 1",
    "cannot find", "cannot resolve",
    "unresolved", "compilation failed",
    "test failed", "tests failed", "FAILED (",
    "command not found",
    "timeout", "timed out",
    "conflict", "CONFLICT",
];

/// Alias for backward compatibility (pattern miner imports this name).
pub(crate) const DECISION_WORTHY_TOOLS: &[&str] = STATE_CHANGING_TOOLS;


#[derive(Debug, Default, Clone)]
pub struct ExtractionStats {
    pub decisions_found: usize,
    pub results_matched: usize,
    pub edges_inserted: usize,
    pub skipped_low_value: usize,
    pub errors_captured: usize,
    pub llm_inferred_count: usize,
}

pub struct DecisionExtractor;

impl DecisionExtractor {
    /// Extract decisions from a grok session directory (backward-compatible entry).
    pub fn extract_from_session(
        store: &CausalStore,
        session_dir: &Path,
    ) -> Result<ExtractionStats> {
        let parsed = GrokParser.parse(&SessionSource::dir(session_dir))?;
        Self::extract_from_parsed(store, &parsed)
    }

    /// Extract decisions from a format-agnostic parsed session.
    ///
    /// v0.3: Two-tier filter — only record failures (always) and state-changing
    /// operations (when they produce meaningful results). Skip read-only ops.
    pub fn extract_from_parsed(
        store: &CausalStore,
        parsed: &ParsedSession,
    ) -> Result<ExtractionStats> {
        let mut outcome_queue: VecDeque<CandidateEvent> = parsed.events.clone();

        let mut stats = ExtractionStats {
            decisions_found: parsed.decisions.len(),
            ..Default::default()
        };

        for decision in &parsed.decisions {
            // Tier 3: Skip read-only tools entirely (only record if CLEAR failure)
            let is_read_only = READ_ONLY_TOOLS.iter().any(|t| decision.name.contains(t));
            let is_state_changing = STATE_CHANGING_TOOLS.iter().any(|t| decision.name.contains(t));

            if is_read_only {
                // For read-only tools, ONLY record if it's a clear failure
                let result = match parsed.results.get(&decision.id) {
                    Some(r) => r,
                    None => {
                        stats.skipped_low_value += 1;
                        continue;
                    }
                };
                let result_text = Self::extract_text(&result.content);
                // Strict failure check: must have a strong failure signal
                // (not just "error" substring which can appear in file paths)
                if !Self::is_clear_failure(&result_text) {
                    stats.skipped_low_value += 1;
                    continue;
                }
                // Fall through to record the failure
            } else if !is_state_changing {
                stats.skipped_low_value += 1;
                continue;
            }

            let result = match parsed.results.get(&decision.id) {
                Some(r) => r,
                None => continue,
            };

            let result_content = Self::extract_text(&result.content);

            // Tier 2: For state-changing tools, skip ONLY if result is a
            // trivial bash success (e.g. "exit: 0" from cargo build).
            // Edit/Write operations are always worth recording even if
            // their result is short ("successfully updated").
            if is_read_only && Self::is_trivial_success(&result_content) {
                // read-only tool with trivial success → skip (already handled above
                // for non-failures, but this is the fallback for edge cases)
            } else if !is_read_only && is_state_changing {
                // State-changing tools: always record (the decision to edit
                // a file IS the valuable memory, regardless of result length)
            }

            stats.results_matched += 1;

            let decision_text = format!(
                "{}({})",
                decision.name,
                Self::summarize_args(&decision.arguments)
            );

            let event_outcome = Self::consume_next_outcome(&mut outcome_queue, &decision.name);

            let event_ts = event_outcome
                .as_ref()
                .and_then(|o| o.ts.as_ref())
                .and_then(|ts| Self::parse_event_ts(ts))
                .unwrap_or_else(|| chrono::Utc::now().timestamp());

            let (relation, confidence, source) = Self::infer_causal_confidence(
                &decision_text,
                &result_content,
                event_outcome.as_ref().map(|o| o.outcome.as_str()),
            );

            if relation == "caused" && confidence >= 0.7 {
                stats.errors_captured += 1;
            }
            if source == "llm_inferred" {
                stats.llm_inferred_count += 1;
            }

            let task_tag = Self::infer_task_tag(&decision.name, &decision.arguments);

            match store.record_decision_at(
                &decision_text,
                &result_content.chars().take(200).collect::<String>(),
                relation,
                Some(&task_tag),
                confidence,
                source,
                event_ts,
            ) {
                Ok(_) => stats.edges_inserted += 1,
                Err(e) => tracing::warn!("Failed to insert edge: {e}"),
            }
        }

        Ok(stats)
    }

    /// v0.2.1: consume the next outcome matching `tool_name` from the queue.
    /// Returns the full event (including timestamp).
    fn consume_next_outcome(
        queue: &mut VecDeque<CandidateEvent>,
        tool_name: &str,
    ) -> Option<CandidateEvent> {
        let pos = queue.iter().position(|p| p.tool_name == tool_name)?;
        queue.remove(pos)
    }

    /// v0.2.1: graded causal confidence inference.
    ///
    /// Combines three signals:
    /// 1. event outcome (from events.jsonl) — success/error/timeout
    /// 2. content relation — does the result text reference the decision's action?
    /// 3. failure markers in result text
    ///
    /// Returns (relation, confidence 0.3-0.8, source).
    /// The old v0.2 binary (0.4 temporal / 0.7 rule) is replaced by a gradient.
    fn infer_causal_confidence(
        decision: &str,
        result: &str,
        event_outcome: Option<&str>,
    ) -> (&'static str, f64, &'static str) {
        let is_failure_event = matches!(
            event_outcome,
            Some("error") | Some("failure") | Some("timeout")
        );
        let is_success_event = matches!(event_outcome, Some("success"));
        let result_looks_like_failure = Self::looks_like_failure(result);
        let content_relates = Self::content_relates_to_decision(decision, result);

        // Failure cases — high-value lessons
        if is_failure_event || result_looks_like_failure {
            if content_relates {
                // Error AND result references the decision — strong causal link
                return ("caused", 0.8, "rule");
            }
            // Error but result doesn't clearly reference decision
            return ("caused", 0.65, "rule");
        }

        // Success cases — weaker causal claim (success doesn't teach much)
        if is_success_event {
            if content_relates {
                // Success AND result clearly references the action (e.g. "file X updated")
                return ("caused", 0.55, "llm_inferred");
            }
            // Generic success ("updated successfully") — weak causal
            return ("caused", 0.4, "temporal");
        }

        // No event data — infer from content alone
        if content_relates {
            return ("caused", 0.5, "llm_inferred");
        }
        ("caused", 0.3, "temporal")
    }

    /// Check if result content references the decision's key action.
    /// E.g. decision="write(src/main.rs)" → result should mention the file or "written/updated/created".
    fn content_relates_to_decision(decision: &str, result: &str) -> bool {
        let dec_lower = decision.to_lowercase();
        let res_lower = result.to_lowercase();

        // Extract the argument (inside parens) — usually a file path or command
        if let Some(start) = dec_lower.find('(') {
            if let Some(end) = dec_lower.rfind(')') {
                let args = &dec_lower[start + 1..end];
                // Check if any meaningful token from args appears in result
                for token in
                    args.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
                {
                    let t = token.trim();
                    if t.len() >= 4 {
                        // skip short tokens
                        if res_lower.contains(t) {
                            return true;
                        }
                    }
                }
            }
        }

        // Check generic success patterns that reference the action type
        let action_patterns: &[(&str, &[&str])] = &[
            (
                "write",
                &["written", "created", "has been written", "file "],
            ),
            (
                "search_replace",
                &["updated", "has been updated", "replaced"],
            ),
            ("run_terminal_command", &["exit:", "stdout", "stderr"]),
            ("spawn_subagent", &["subagent", "completed", "returned"]),
        ];

        for (action, patterns) in action_patterns {
            if dec_lower.contains(action) {
                for p in *patterns {
                    if res_lower.contains(p) {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Parse an ISO timestamp from events.jsonl into Unix epoch seconds.
    /// Example: "2026-07-26T06:03:11.008Z" → epoch_seconds
    fn parse_event_ts(ts: &str) -> Option<i64> {
        // Try chrono's parser for ISO 8601
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|dt| dt.timestamp())
    }

    fn extract_text(content: &serde_json::Value) -> String {
        match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
                .join(" "),
            _ => content.to_string(),
        }
    }

    fn summarize_args(args: &str) -> String {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
            if let Some(obj) = v.as_object() {
                for key in &[
                    "command",
                    "file_path",
                    "target_file",
                    "target_directory",
                    "prompt",
                ] {
                    if let Some(val) = obj.get(*key) {
                        let s = val
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| val.to_string());
                        if s.chars().count() > 50 {
                            return format!("{}...", s.chars().take(50).collect::<String>());
                        }
                        return s;
                    }
                }
                if let Some((_, v)) = obj.iter().next() {
                    let s = v
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string());
                    if s.chars().count() > 50 {
                        return format!("{}...", s.chars().take(50).collect::<String>());
                    }
                    return s;
                }
            }
        }
        if args.chars().count() > 50 {
            format!("{}...", args.chars().take(50).collect::<String>())
        } else {
            args.to_string()
        }
    }

    fn looks_like_failure(text: &str) -> bool {
        Self::is_failure(text)
    }

    /// Check if the result indicates a failure (error, crash, test failure).
    fn is_failure(text: &str) -> bool {
        FAILURE_MARKERS.iter().any(|m| text.contains(m))
    }

    /// Strict failure check for read-only tools: the failure signal must be
    /// prominent (start of line / first 200 chars), not buried in a file path.
    /// This prevents false positives like `ls error_handler.rs` → contains "error".
    fn is_clear_failure(text: &str) -> bool {
        let first_200: String = text.chars().take(200).collect();
        let lines: Vec<&str> = first_200.lines().take(5).collect();
        let strong_markers = [
            "error[", "error:", "Error:", "ERROR:",
            "panic:", "thread 'main' panicked",
            "Traceback (most recent call last)",
            "exit: 1", "exit code: 1", "Process exited with non-zero",
            "test failed", "tests failed", "FAILED ",
            "command not found",
            "Permission denied",
            "fatal:",
            "compilation failed", "could not compile",
        ];
        // Check first 5 lines for strong markers
        for line in &lines {
            for marker in &strong_markers {
                if line.contains(marker) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if the result is a trivially successful operation that carries
    /// no decision value. E.g. "exit: 0" from a routine build.
    fn is_trivial_success(text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.len() < 30 { return true; }
        let trivial_starts = ["exit: 0\n", "exit: 0\r", "exit: 0 "];
        for t in &trivial_starts {
            if trimmed.starts_with(t) && trimmed.len() < 80 { return true; }
        }
        matches!(trimmed, "success" | "ok" | "done")
    }

    fn infer_task_tag(tool_name: &str, args: &str) -> String {
        let combined = format!("{} {}", tool_name, args).to_lowercase();
        if combined.contains("replace") || combined.contains("edit") || combined.contains("write") {
            "code-edit".into()
        } else if combined.contains("test") {
            "testing".into()
        } else if combined.contains("build")
            || combined.contains("cargo")
            || combined.contains("compile")
        {
            "build".into()
        } else if combined.contains("git")
            || combined.contains("commit")
            || combined.contains("push")
        {
            "vcs".into()
        } else if combined.contains("deploy") || combined.contains("docker") {
            "deploy".into()
        } else if combined.contains("search")
            || combined.contains("grep")
            || combined.contains("find")
        {
            "search".into()
        } else {
            "general".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::CandidateEvent;

    #[test]
    fn test_summarize_args() {
        assert_eq!(
            DecisionExtractor::summarize_args(r#"{"command":"ls -la"}"#),
            "ls -la"
        );
        assert_eq!(
            DecisionExtractor::summarize_args(r#"{"target_file":"src/main.rs"}"#),
            "src/main.rs"
        );
    }

    #[test]
    fn test_looks_like_failure() {
        assert!(DecisionExtractor::looks_like_failure(
            "error: compilation failed"
        ));
        assert!(DecisionExtractor::looks_like_failure("Permission denied"));
        assert!(!DecisionExtractor::looks_like_failure("Build succeeded"));
    }

    #[test]
    fn test_infer_task_tag() {
        assert_eq!(
            DecisionExtractor::infer_task_tag(
                "run_terminal_command",
                r#"{"command":"cargo test"}"#
            ),
            "testing"
        );
        assert_eq!(
            DecisionExtractor::infer_task_tag(
                "run_terminal_command",
                r#"{"command":"cargo build"}"#
            ),
            "build"
        );
        assert_eq!(
            DecisionExtractor::infer_task_tag("search_replace", r#"{"file_path":"src/lib.rs"}"#),
            "code-edit"
        );
        assert_eq!(
            DecisionExtractor::infer_task_tag(
                "spawn_subagent",
                r#"{"prompt":"grep for patterns"}"#
            ),
            "search"
        );
    }

    #[test]
    fn test_content_relates_to_decision() {
        // File path in args appears in result
        assert!(DecisionExtractor::content_relates_to_decision(
            "write(src/main.rs)",
            "The file src/main.rs has been written"
        ));
        // Command in args appears in result
        assert!(DecisionExtractor::content_relates_to_decision(
            "run_terminal_command(cargo build)",
            "cargo build finished"
        ));
        // Unrelated result
        assert!(!DecisionExtractor::content_relates_to_decision(
            "write(src/main.rs)",
            "ok"
        ));
    }

    #[test]
    fn test_infer_causal_confidence_failure() {
        // Failure + content relation → high confidence rule
        let (_, c, s) = DecisionExtractor::infer_causal_confidence(
            "run_terminal_command(cargo build)",
            "error: cargo build failed",
            Some("error"),
        );
        assert_eq!(s, "rule");
        assert!(c >= 0.7);
    }

    #[test]
    fn test_infer_causal_confidence_success_related() {
        let (_, c, _) = DecisionExtractor::infer_causal_confidence(
            "write(src/main.rs)",
            "The file src/main.rs has been written",
            Some("success"),
        );
        assert!(c >= 0.5);
    }

    #[test]
    fn test_infer_causal_confidence_generic_success() {
        // "ok" doesn't relate to search_replace specifically → temporal
        let (_, c, s) = DecisionExtractor::infer_causal_confidence(
            "search_replace(README.md)",
            "ok",
            Some("success"),
        );
        assert_eq!(s, "temporal");
        assert!(c < 0.5);
    }

    #[test]
    fn test_consume_next_outcome_no_overwrite() {
        let mut queue = VecDeque::new();
        queue.push_back(CandidateEvent {
            tool_name: "run_terminal_command".into(),
            outcome: "error".into(),
            ts: None,
        });
        queue.push_back(CandidateEvent {
            tool_name: "run_terminal_command".into(),
            outcome: "success".into(),
            ts: None,
        });

        let first = DecisionExtractor::consume_next_outcome(&mut queue, "run_terminal_command");
        assert_eq!(first.as_ref().map(|o| o.outcome.as_str()), Some("error"));

        let second = DecisionExtractor::consume_next_outcome(&mut queue, "run_terminal_command");
        assert_eq!(second.as_ref().map(|o| o.outcome.as_str()), Some("success"));

        assert!(queue.is_empty());
    }
}
