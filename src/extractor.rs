//! Decision extractor — watches grok-build's session logs and auto-extracts
//! decision→outcome pairs into the causal store.
//!
//! This is the v0.2 feature: agents no longer need to manually call
//! `record_decision`. The watcher reads `chat_history.jsonl` from the
//! session directory and automatically populates the causal table.
//!
//! ## Data sources (grok-build session layout)
//!
//! ```text
//! ~/.grok/sessions/<workspace-hash>/<session-id>/
//!   ├── chat_history.jsonl    ← we read this
//!   │     - assistant entries have `tool_calls[].{id, name, arguments}`
//!   │     - tool_result entries have `tool_call_id, content`
//!   ├── events.jsonl          ← we read this for outcome (success/failure)
//!   │     - tool_completed events have `tool_name, outcome`
//!   └── summary.json
//! ```
//!
//! ## Extraction logic
//!
//! 1. Scan chat_history for `assistant` entries with `tool_calls`.
//! 2. Each tool_call is a **decision** (the agent decided to call tool X with args Y).
//! 3. Match each tool_call to its `tool_result` via `tool_call_id`.
//! 4. The result content is the **outcome**.
//! 5. Cross-reference events.jsonl for outcome status (success/failure/timeout).
//! 6. Confidence:
//!    - `rule` (0.7) if outcome was failure — failures are high-value lessons
//!    - `temporal` (0.4) for success — time-adjacent, weak causal claim
//!    - `llm_inferred` (0.6) is not used by the rule-based extractor (v0.3)

use std::path::{Path, PathBuf};
use std::collections::HashMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::store::CausalStore;

/// Minimum tool name to treat as a "decision worth recording".
/// Read-only tools (list_dir, read_file, search) are low-value —
/// they don't change state, so their causal impact is weak.
const DECISION_WORTHY_TOOLS: &[&str] = &[
    "write", "search_replace", "run_terminal_command", "image_gen",
    "image_edit", "spawn_subagent", "kill_command_or_subagent",
    "scheduler_create", "scheduler_delete", "update_goal",
    "search_replace",  // file edits
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
struct EventEntry {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    ts: String,
}

/// Result of a single extraction pass.
#[derive(Debug, Default, Clone)]
pub struct ExtractionStats {
    pub decisions_found: usize,
    pub results_matched: usize,
    pub edges_inserted: usize,
    pub skipped_low_value: usize,
}

/// The watcher/extractor. Stateless — call `extract_from_session` on demand.
pub struct DecisionExtractor;

impl DecisionExtractor {
    /// Extract decisions from a grok-build session directory.
    ///
    /// Reads chat_history.jsonl and events.jsonl, matches tool_calls to
    /// tool_results, and writes causal edges to the store.
    pub fn extract_from_session(
        store: &CausalStore,
        session_dir: &Path,
    ) -> Result<ExtractionStats> {
        let chat_path = session_dir.join("chat_history.jsonl");
        let events_path = session_dir.join("events.jsonl");

        if !chat_path.exists() {
            return Err(anyhow!("chat_history.jsonl not found in {}", session_dir.display()));
        }

        // Phase 1: parse chat_history into decisions + results
        let (decisions, results) = Self::parse_chat_history(&chat_path)?;

        // Phase 2: parse events for outcome status
        let outcomes = if events_path.exists() {
            Self::parse_events(&events_path)?
        } else {
            HashMap::new()
        };

        // Phase 3: match and insert
        let mut stats = ExtractionStats {
            decisions_found: decisions.len(),
            ..Default::default()
        };

        for decision in &decisions {
            // Skip low-value tools (reads, searches)
            if !DECISION_WORTHY_TOOLS.iter().any(|t| decision.name.contains(t)) {
                stats.skipped_low_value += 1;
                continue;
            }

            // Match to result
            let result = results.get(&decision.id);
            if result.is_none() {
                continue;
            }
            stats.results_matched += 1;

            let result_content = Self::extract_text(&result.unwrap().content);
            let decision_text = format!(
                "{}({})",
                decision.name,
                Self::summarize_args(&decision.arguments)
            );

            // Determine relation and confidence
            let outcome_key = format!("{}-{}", decision.name, "");
            let event_outcome = outcomes.get(&decision.name);
            let (relation, confidence, source) = match event_outcome.map(|s| s.as_str()) {
                Some("failure") | Some("error") | Some("timeout") => {
                    ("caused", 0.7, "rule") // failures are high-value
                }
                Some("success") => {
                    ("caused", 0.4, "temporal") // success is weak causal
                }
                _ => {
                    // No event data; infer from result content
                    if Self::looks_like_failure(&result_content) {
                        ("caused", 0.6, "rule")
                    } else {
                        ("caused", 0.4, "temporal")
                    }
                }
            };

            // Infer task_tag from tool name + args
            let task_tag = Self::infer_task_tag(&decision.name, &decision.arguments);

            // Insert into store
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
    ) -> Result<(Vec<ToolCallEntry>, HashMap<String, ToolResultEntry>)> {
        let mut decisions = Vec::new();
        let mut results = HashMap::new();

        for line in std::fs::read_to_string(path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
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
                    } else {
                        // Try parsing as standalone tool_result
                        if let Ok(tr) = serde_json::from_str::<ToolResultEntry>(line) {
                            results.insert(tr.tool_call_id.clone(), tr);
                        }
                    }
                }
                _ => {}
            }
        }

        Ok((decisions, results))
    }

    fn parse_events(path: &Path) -> Result<HashMap<String, String>> {
        // Returns map: tool_name → last known outcome
        let mut outcomes = HashMap::new();
        for line in std::fs::read_to_string(path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let event: EventEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if event.event_type == "tool_completed" {
                if let (Some(name), Some(outcome)) = (event.tool_name, event.outcome) {
                    outcomes.insert(name, outcome);
                }
            }
        }
        Ok(outcomes)
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
        // Try to parse and extract the most relevant field
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
            if let Some(obj) = v.as_object() {
                // Look for common fields: command, file_path, target_file, etc.
                for key in &["command", "file_path", "target_file", "target_directory", "prompt"] {
                    if let Some(val) = obj.get(*key) {
                        let s = val.as_str().map(String::from).unwrap_or_else(|| val.to_string());
                        if s.len() > 50 {
                            return format!("{}...", s.chars().take(50).collect::<String>());
                        }
                        return s;
                    }
                }
                // Fallback: first field's value
                if let Some((_, v)) = obj.iter().next() {
                    let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
                    if s.len() > 50 {
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
    }

    fn infer_task_tag(tool_name: &str, args: &str) -> String {
        let combined = format!("{} {}", tool_name, args).to_lowercase();
        // Check edit/write BEFORE search (search_replace contains "search")
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
}
