//! Claude Code session parser.
//!
//! Reads `~/.claude/projects/**/<session>.jsonl`. Each line is a JSON object
//! with `type` ∈ {`user`, `assistant`, `summary`, ...}. Tool calls live as
//! `tool_use` blocks inside `assistant.message.content[]`; results live as
//! `tool_result` blocks inside `user.message.content[]`, linked by
//! `tool_use_id`.
//!
//! There is no `events.jsonl` equivalent — outcome (success/error) must be
//! inferred from the tool_result content text, same as the v0.2 fallback path.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::{
    CandidateDecision, CandidateEvent, CandidateResult, ParsedSession, SessionParser,
    SessionSource, SourceKind,
};

/// Parser for Claude Code session jsonl files.
pub struct ClaudeParser;

#[derive(Debug, Deserialize)]
struct ClaudeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    message: Option<ClaudeMessage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    content: serde_json::Value,
}

impl SessionParser for ClaudeParser {
    fn parse(&self, source: &SessionSource) -> Result<ParsedSession> {
        if source.kind != SourceKind::File {
            anyhow::bail!(
                "ClaudeParser expects a .jsonl file, got {}",
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
            let entry: ClaudeEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match entry.entry_type.as_str() {
                "assistant" => {
                    let Some(msg) = &entry.message else { continue };
                    let content = msg.content.as_array();

                    if let Some(blocks) = content {
                        for block in blocks {
                            if let Some(btype) = block.get("type").and_then(|v| v.as_str()) {
                                match btype {
                                    "tool_use" => {
                                        let id = block
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let arguments = block
                                            .get("input")
                                            .map(|v| serde_json::to_string(v).unwrap_or_default())
                                            .unwrap_or_default();
                                        decisions.push(CandidateDecision {
                                            id,
                                            name,
                                            arguments,
                                        });
                                    }
                                    "text" => {
                                        if let Some(text) =
                                            block.get("text").and_then(|v| v.as_str())
                                        {
                                            if text.len() >= 100 {
                                                assistant_texts.push(text.to_string());
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    } else if let Some(text) = msg.content.as_str() {
                        if text.len() >= 100 {
                            assistant_texts.push(text.to_string());
                        }
                    }
                }
                "user" => {
                    let Some(msg) = &entry.message else { continue };

                    // Tool results are inside user.message.content[] as
                    // {type: "tool_result", tool_use_id, content}
                    if let Some(blocks) = msg.content.as_array() {
                        for block in blocks {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                let tool_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let content = block.get("content").cloned().unwrap_or_default();
                                results.insert(tool_id.clone(), CandidateResult { content });
                                // Synthesize an event for the outcome queue.
                                // Claude has no events.jsonl — infer from content.
                                let outcome = infer_outcome_from_result(&results[&tool_id].content);
                                let tool_name = decisions
                                    .iter()
                                    .rev()
                                    .find(|d| d.id == tool_id)
                                    .map(|d| d.name.clone())
                                    .unwrap_or_default();
                                events.push_back(CandidateEvent {
                                    tool_name,
                                    outcome,
                                    ts: None,
                                });
                            }
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

/// Infer success/error from the tool_result content text.
/// Claude Code doesn't have an explicit outcome field — we look for common
/// error patterns in the result text.
fn infer_outcome_from_result(content: &serde_json::Value) -> String {
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => serde_json::to_string(content).unwrap_or_default(),
    };
    let lower = text.to_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("panic")
        || lower.contains("denied")
    {
        "error".to_string()
    } else {
        "success".to_string()
    }
}

// Suppress unused import for Path (used in the trait impl via source.path)
#[allow(dead_code)]
fn _path_used(_p: &Path) {}
