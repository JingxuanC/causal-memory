//! Codex (OpenAI Codex CLI) session parser.
//!
//! Reads `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. Each line is a JSON
//! object with `type` ∈ {`session_meta`, `response_item`, `event_msg`, ...}.
//!
//! Tool calls live inside `response_item` entries:
//! - `{type: "response_item", payload: {type: "function_call", name, arguments,
//!   call_id}}` — the decision.
//! - `{type: "response_item", payload: {type: "function_call_output", call_id,
//!   output}}` — the result, linked by `call_id`.
//!
//! Assistant reasoning text lives in `response_item` entries with
//! `payload.type: "message"` and `payload.role: "assistant"`.
//!
//! There is no `events.jsonl` — outcome is inferred from the output text
//! (Codex includes exit codes in the output).

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::{
    CandidateDecision, CandidateEvent, CandidateResult, ParsedSession, SessionParser,
    SessionSource, SourceKind,
};

/// Parser for OpenAI Codex CLI session jsonl files.
pub struct CodexParser;

#[derive(Debug, Deserialize)]
struct CodexEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    payload: Option<CodexPayload>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    content: serde_json::Value,
}

impl SessionParser for CodexParser {
    fn parse(&self, source: &SessionSource) -> Result<ParsedSession> {
        if source.kind != SourceKind::File {
            anyhow::bail!(
                "CodexParser expects a .jsonl file, got {}",
                source.path.display()
            );
        }
        if !source.path.exists() {
            return Err(anyhow!("Session file not found: {}", source.path.display()));
        }

        let raw = std::fs::read_to_string(&source.path)?;
        let mut decisions = Vec::new();
        let mut results = HashMap::new();
        let mut events = std::collections::VecDeque::new();
        let mut assistant_texts = Vec::new();

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: CodexEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.entry_type != "response_item" {
                continue;
            }
            let Some(payload) = &entry.payload else {
                continue;
            };
            let pt = payload.payload_type.as_deref().unwrap_or("");
            let ts = entry.timestamp.clone();

            match pt {
                "function_call" => {
                    let id = payload.call_id.clone().unwrap_or_default();
                    let name = payload.name.clone().unwrap_or_default();
                    let arguments = payload.arguments.clone().unwrap_or_default();
                    decisions.push(CandidateDecision {
                        id,
                        name,
                        arguments,
                    });
                }
                "function_call_output" => {
                    let call_id = payload.call_id.clone().unwrap_or_default();
                    let output = payload.output.clone().unwrap_or_default();
                    results.insert(
                        call_id.clone(),
                        CandidateResult {
                            content: serde_json::Value::String(output.clone()),
                        },
                    );
                    // Find the function name for this call_id.
                    let tool_name = decisions
                        .iter()
                        .rev()
                        .find(|d| d.id == call_id)
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    let outcome = infer_outcome(&output);
                    events.push_back(CandidateEvent {
                        tool_name,
                        outcome,
                        ts,
                    });
                }
                "message" if payload.role.as_deref() == Some("assistant") => {
                    if let Some(blocks) = payload.content.as_array() {
                        for block in blocks {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                if text.len() >= 100 {
                                    assistant_texts.push(text.to_string());
                                }
                            }
                        }
                    } else if let Some(text) = payload.content.as_str() {
                        if text.len() >= 100 {
                            assistant_texts.push(text.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(ParsedSession {
            decisions,
            results,
            events,
            assistant_texts,
        })
    }
}

/// Infer success/error from Codex function_call_output text.
/// Codex includes "Process exited with code N" for shell commands.
fn infer_outcome(text: &str) -> String {
    if text.contains("Process exited with code 0") {
        return "success".to_string();
    }
    // Non-zero exit code or explicit error patterns
    if text.contains("exited with non-zero")
        || (text.contains("exited with code") && !text.contains("code 0"))
        || text.to_lowercase().contains("error")
        || text.to_lowercase().contains("traceback")
        || text.to_lowercase().contains("panic")
    {
        return "error".to_string();
    }
    "success".to_string()
}
