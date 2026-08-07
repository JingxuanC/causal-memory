//! Kimi (OpenClaw) session parser.
//!
//! Reads `~/.openclaw/agents/main/sessions/<session-id>.jsonl`. Each line is a
//! JSON object with `type` ∈ {`session`, `message`, `model_change`, ...}.
//!
//! Tool calls live as `toolCall` blocks inside assistant messages:
//! `{type: "message", message: {role: "assistant", content: [{type: "toolCall",
//! id, name, arguments}]}}`.
//!
//! Tool results appear as separate messages with `role: "toolResult"`:
//! `{type: "message", message: {role: "toolResult", content: [{type: "text",
//! text: "..."}]}}`. The link between a toolCall and its result is positional
//! (the toolResult message immediately follows the assistant message that
//! contained the toolCall), not by ID.
//!
//! There is no `events.jsonl` — outcome is inferred from the result text.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::{
    CandidateDecision, CandidateEvent, CandidateResult, ParsedSession, SessionParser,
    SessionSource, SourceKind,
};

/// Parser for Kimi / OpenClaw session jsonl files.
pub struct KimiParser;

#[derive(Debug, Deserialize)]
struct KimiEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    message: Option<KimiMessage>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KimiMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: serde_json::Value,
}

impl SessionParser for KimiParser {
    fn parse(&self, source: &SessionSource) -> Result<ParsedSession> {
        if source.kind != SourceKind::File {
            anyhow::bail!(
                "KimiParser expects a .jsonl file, got {}",
                source.path.display()
            );
        }
        if !source.path.exists() {
            return Err(anyhow!(
                "Session file not found: {}",
                source.path.display()
            ));
        }

        let raw = std::fs::read_to_string(&source.path)?;
        let mut decisions = Vec::new();
        let mut results = HashMap::new();
        let mut events = std::collections::VecDeque::new();
        let mut assistant_texts = Vec::new();

        // Track pending tool calls (not yet matched to a result). Kimi
        // delivers tool results as role="toolResult" messages that follow
        // the assistant message — we match by order.
        let mut pending_tool_calls: Vec<CandidateDecision> = Vec::new();

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: KimiEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.entry_type != "message" {
                continue;
            }
            let Some(msg) = &entry.message else { continue };
            let role = msg.role.as_deref().unwrap_or("");
            let ts = entry.timestamp.clone();

            match role {
                "assistant" => {
                    if let Some(blocks) = msg.content.as_array() {
                        let mut new_tool_calls = Vec::new();
                        for block in blocks {
                            if let Some(btype) = block.get("type").and_then(|v| v.as_str()) {
                                match btype {
                                    "toolCall" => {
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
                                            .get("arguments")
                                            .map(|v| serde_json::to_string(v).unwrap_or_default())
                                            .unwrap_or_default();
                                        let tc = CandidateDecision { id: id.clone(), name: name.clone(), arguments };
                                        decisions.push(tc.clone());
                                        new_tool_calls.push(tc);
                                    }
                                    "text" => {
                                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                            if text.len() >= 100 {
                                                assistant_texts.push(text.to_string());
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if !new_tool_calls.is_empty() {
                            pending_tool_calls = new_tool_calls;
                        }
                    } else if let Some(text) = msg.content.as_str() {
                        if text.len() >= 100 {
                            assistant_texts.push(text.to_string());
                        }
                    }
                }
                "toolResult" => {
                    // Match tool results to pending tool calls by order.
                    // Kimi returns one toolResult message per tool call, in
                    // the same order as the calls in the preceding assistant
                    // message.
                    let result_text = extract_text_from_content(&msg.content);

                    if let Some(tc) = pending_tool_calls.first() {
                        results.insert(
                            tc.id.clone(),
                            CandidateResult {
                                content: serde_json::Value::String(result_text.clone()),
                            },
                        );
                        let outcome = infer_outcome(&result_text);
                        events.push_back(CandidateEvent {
                            tool_name: tc.name.clone(),
                            outcome,
                            ts,
                        });
                        pending_tool_calls.remove(0);
                    } else if !decisions.is_empty() {
                        // Fallback: match to last unmatched decision
                        let tc = decisions.last().unwrap();
                        if !results.contains_key(&tc.id) {
                            results.insert(
                                tc.id.clone(),
                                CandidateResult {
                                    content: serde_json::Value::String(result_text.clone()),
                                },
                            );
                            let outcome = infer_outcome(&result_text);
                            events.push_back(CandidateEvent {
                                tool_name: tc.name.clone(),
                                outcome,
                                ts,
                            });
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

/// Extract text from Kimi content (array of {type, text} blocks, or a string).
fn extract_text_from_content(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    b.get("text").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect();
        return texts.join("\n");
    }
    serde_json::to_string(content).unwrap_or_default()
}

/// Infer success/error from tool result text (same heuristic as Claude parser).
fn infer_outcome(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("denied")
        || lower.contains("command not found")
    {
        "error".to_string()
    } else {
        "success".to_string()
    }
}
