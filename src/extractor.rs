//! Decision extractor — watches grok-build's session logs and auto-extracts
//! decision→outcome pairs into the causal store.
//!
//! v0.2.1 changes:
//! - Fixed outcome-overwrite bug: each tool_call now consumes its own
//!   tool_completed event by ordered name matching, instead of looking
//!   up by tool_name in a HashMap (which got overwritten by same-name
//!   later calls — 7 real errors in the test session were lost).
//! - Added causal inference: confidence is now graded 0.3-0.8 based on
//!   content-relation analysis between decision and outcome, instead of
//!   the old binary temporal(0.4)/rule(0.7) split.

use std::path::Path;
use std::collections::VecDeque;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::store::CausalStore;

/// Minimum tool name to treat as a "decision worth recording".
const DECISION_WORTHY_TOOLS: &[&str] = &[
    "write", "search_replace", "run_terminal_command", "image_gen",
    "image_edit", "spawn_subagent", "kill_command_or_subagent",
    "scheduler_create", "scheduler_delete", "update_goal",
];

#[derive(Debug, Deserialize)]
struct ChatEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default)]
    tool_calls: Vec<ToolCallEntry>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolCallEntry {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ToolResultEntry {
    #[serde(rename = "type")]
    entry_type: String,
    tool_call_id: String,
    content: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct EventEntry {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
}

/// An outcome event waiting to be consumed by a matching tool_call.
#[derive(Debug, Clone)]
struct PendingOutcome {
    tool_name: String,
    outcome: String,
}

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
    pub fn extract_from_session(
        store: &CausalStore,
        session_dir: &Path,
    ) -> Result<ExtractionStats> {
        let chat_path = session_dir.join("chat_history.jsonl");
        let events_path = session_dir.join("events.jsonl");

        if !chat_path.exists() {
            return Err(anyhow!("chat_history.jsonl not found in {}", session_dir.display()));
        }

        let (decisions, results) = Self::parse_chat_history(&chat_path)?;

        // v0.2.1 fix: collect outcomes as an ordered queue, consumed
        // by matching tool_name — each tool_call gets its own outcome
        let mut outcome_queue: VecDeque<PendingOutcome> = if events_path.exists() {
            Self::parse_events_ordered(&events_path)?
        } else {
            VecDeque::new()
        };

        let mut stats = ExtractionStats {
            decisions_found: decisions.len(),
            ..Default::default()
        };

        for decision in &decisions {
            if !DECISION_WORTHY_TOOLS.iter().any(|t| decision.name.contains(t)) {
                stats.skipped_low_value += 1;
                continue;
            }

            let result = match results.get(&decision.id) {
                Some(r) => r,
                None => continue,
            };
            stats.results_matched += 1;

            let result_content = Self::extract_text(&result.content);
            let decision_text = format!(
                "{}({})",
                decision.name,
                Self::summarize_args(&decision.arguments)
            );

            // v0.2.1 fix: consume the next matching outcome from the queue
            let event_outcome = Self::consume_next_outcome(&mut outcome_queue, &decision.name);

            // v0.2.1: graded causal inference (replaces binary temporal/rule)
            let (relation, confidence, source) =
                Self::infer_causal_confidence(&decision_text, &result_content, event_outcome.as_deref());

            if source == "rule" && confidence >= 0.7 {
                stats.errors_captured += 1;
            }
            if source == "llm_inferred" {
                stats.llm_inferred_count += 1;
            }

            let task_tag = Self::infer_task_tag(&decision.name, &decision.arguments);

            match store.record_decision(
                &decision_text,
                &result_content.chars().take(300).collect::<String>(),
                relation,
                Some(&task_tag),
                confidence,
                source,
            ) {
                Ok(_) => stats.edges_inserted += 1,
                Err(e) => tracing::warn!("Failed to insert edge: {e}"),
            }
        }

        Ok(stats)
    }

    fn parse_chat_history(
        path: &Path,
    ) -> Result<(Vec<ToolCallEntry>, std::collections::HashMap<String, ToolResultEntry>)> {
        let mut decisions = Vec::new();
        let mut results = std::collections::HashMap::new();

        for line in std::fs::read_to_string(path)?.lines() {
            if line.trim().is_empty() { continue; }
            let entry: ChatEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match entry.entry_type.as_str() {
                "assistant" => {
                    for tc in entry.tool_calls {
                        decisions.push(tc);
                    }
                }
                "tool_result" | "tool" => {
                    if let Some(id) = &entry.tool_call_id {
                        results.insert(id.clone(), ToolResultEntry {
                            entry_type: entry.entry_type,
                            tool_call_id: id.clone(),
                            content: entry.content,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok((decisions, results))
    }

    /// v0.2.1: parse events preserving order, return as a consumable queue.
    /// Each tool_completed becomes a PendingOutcome waiting to be matched.
    fn parse_events_ordered(path: &Path) -> Result<VecDeque<PendingOutcome>> {
        let mut queue = VecDeque::new();
        for line in std::fs::read_to_string(path)?.lines() {
            if line.trim().is_empty() { continue; }
            let event: EventEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if event.event_type == "tool_completed" {
                if let (Some(name), Some(outcome)) = (event.tool_name, event.outcome) {
                    queue.push_back(PendingOutcome { tool_name: name, outcome });
                }
            }
        }
        Ok(queue)
    }

    /// v0.2.1: consume the next outcome matching `tool_name` from the queue.
    /// Scans from the front; the first match is removed and returned.
    /// This prevents same-name tools from overwriting each other.
    fn consume_next_outcome(
        queue: &mut VecDeque<PendingOutcome>,
        tool_name: &str,
    ) -> Option<String> {
        let pos = queue.iter().position(|p| p.tool_name == tool_name)?;
        let matched = queue.remove(pos)?;
        Some(matched.outcome)
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
        let is_failure_event = matches!(event_outcome, Some("error") | Some("failure") | Some("timeout"));
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
                let args = &dec_lower[start+1..end];
                // Check if any meaningful token from args appears in result
                for token in args.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.') {
                    let t = token.trim();
                    if t.len() >= 4 {  // skip short tokens
                        if res_lower.contains(t) {
                            return true;
                        }
                    }
                }
            }
        }

        // Check generic success patterns that reference the action type
        let action_patterns: &[(&str, &[&str])] = &[
            ("write", &["written", "created", "has been written", "file "]),
            ("search_replace", &["updated", "has been updated", "replaced"]),
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

    fn extract_text(content: &serde_json::Value) -> String {
        match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            _ => content.to_string(),
        }
    }

    fn summarize_args(args: &str) -> String {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
            if let Some(obj) = v.as_object() {
                for key in &["command", "file_path", "target_file", "target_directory", "prompt"] {
                    if let Some(val) = obj.get(*key) {
                        let s = val.as_str().map(String::from).unwrap_or_else(|| val.to_string());
                        if s.chars().count() > 50 {
                            return format!("{}...", s.chars().take(50).collect::<String>());
                        }
                        return s;
                    }
                }
                if let Some((_, v)) = obj.iter().next() {
                    let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
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
        let lower = text.to_lowercase();
        lower.contains("error") || lower.contains("failed") || lower.contains("panic")
            || lower.contains("exception") || lower.contains("traceback")
            || lower.contains("denied") || lower.contains("not found")
            || lower.contains("fatal") || lower.contains("reject")
    }

    fn infer_task_tag(tool_name: &str, args: &str) -> String {
        let combined = format!("{} {}", tool_name, args).to_lowercase();
        if combined.contains("replace") || combined.contains("edit") || combined.contains("write") {
            "code-edit".into()
        } else if combined.contains("test") {
            "testing".into()
        } else if combined.contains("build") || combined.contains("cargo") || combined.contains("compile") {
            "build".into()
        } else if combined.contains("git") || combined.contains("commit") || combined.contains("push") {
            "vcs".into()
        } else if combined.contains("deploy") || combined.contains("docker") {
            "deploy".into()
        } else if combined.contains("search") || combined.contains("grep") || combined.contains("find") {
            "search".into()
        } else {
            "general".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarize_args() {
        assert_eq!(DecisionExtractor::summarize_args(r#"{"command":"ls -la"}"#), "ls -la");
        assert_eq!(DecisionExtractor::summarize_args(r#"{"target_file":"src/main.rs"}"#), "src/main.rs");
    }

    #[test]
    fn test_looks_like_failure() {
        assert!(DecisionExtractor::looks_like_failure("error: compilation failed"));
        assert!(DecisionExtractor::looks_like_failure("Permission denied"));
        assert!(!DecisionExtractor::looks_like_failure("Build succeeded"));
    }

    #[test]
    fn test_infer_task_tag() {
        assert_eq!(DecisionExtractor::infer_task_tag("run_terminal_command", r#"{"command":"cargo test"}"#), "testing");
        assert_eq!(DecisionExtractor::infer_task_tag("run_terminal_command", r#"{"command":"cargo build"}"#), "build");
        assert_eq!(DecisionExtractor::infer_task_tag("search_replace", r#"{"file_path":"src/lib.rs"}"#), "code-edit");
        assert_eq!(DecisionExtractor::infer_task_tag("spawn_subagent", r#"{"prompt":"grep for patterns"}"#), "search");
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
        let (r, c, s) = DecisionExtractor::infer_causal_confidence(
            "run_terminal_command(cargo build)",
            "error: cargo build failed",
            Some("error"),
        );
        assert_eq!(s, "rule");
        assert!(c >= 0.7);
    }

    #[test]
    fn test_infer_causal_confidence_success_related() {
        let (r, c, s) = DecisionExtractor::infer_causal_confidence(
            "write(src/main.rs)",
            "The file src/main.rs has been written",
            Some("success"),
        );
        assert!(c >= 0.5);
    }

    #[test]
    fn test_infer_causal_confidence_generic_success() {
        // "ok" doesn't relate to search_replace specifically → temporal
        let (r, c, s) = DecisionExtractor::infer_causal_confidence(
            "search_replace(README.md)",
            "ok",
            Some("success"),
        );
        assert_eq!(s, "temporal");
        assert!(c < 0.5);
    }

    #[test]
    fn test_consume_next_outcome_no_overwrite() {
        // v0.2.1 fix: two same-name tools, first error second success
        // must NOT overwrite each other
        let mut queue = VecDeque::new();
        queue.push_back(PendingOutcome { tool_name: "run_terminal_command".into(), outcome: "error".into() });
        queue.push_back(PendingOutcome { tool_name: "run_terminal_command".into(), outcome: "success".into() });

        // First call should consume the error
        let first = DecisionExtractor::consume_next_outcome(&mut queue, "run_terminal_command");
        assert_eq!(first.as_deref(), Some("error"));

        // Second call should consume the success
        let second = DecisionExtractor::consume_next_outcome(&mut queue, "run_terminal_command");
        assert_eq!(second.as_deref(), Some("success"));

        // Queue now empty
        assert!(queue.is_empty());
    }
}
