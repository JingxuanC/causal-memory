//! Grok session parser — reads `chat_history.jsonl` + `events.jsonl` from a
//! session directory. This is the parsing half that used to live inside
//! `DecisionExtractor` / `ReasoningExtractor`, extracted behind [`SessionParser`].

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::{
    CandidateDecision, CandidateEvent, CandidateResult, ParsedSession, SessionParser,
    SessionSource, SourceKind,
};

/// Parser for grok-build session directories.
pub struct GrokParser;

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
#[allow(dead_code)]
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
    #[serde(default)]
    ts: Option<String>,
}

impl SessionParser for GrokParser {
    fn parse(&self, source: &SessionSource) -> Result<ParsedSession> {
        if source.kind != SourceKind::Dir {
            anyhow::bail!(
                "GrokParser expects a session directory, got {}",
                source.path.display()
            );
        }

        let chat_path = source.path.join("chat_history.jsonl");
        let events_path = source.path.join("events.jsonl");

        if !chat_path.exists() {
            return Err(anyhow!(
                "chat_history.jsonl not found in {}",
                source.path.display()
            ));
        }

        let (decisions, results) = Self::parse_chat_history(&chat_path)?;

        let events = if events_path.exists() {
            Self::parse_events_ordered(&events_path)?
        } else {
            VecDeque::new()
        };

        Ok(ParsedSession {
            decisions,
            results,
            events,
            assistant_texts: Self::collect_assistant_texts(&chat_path)?,
        })
    }
}

impl GrokParser {
    /// Parse `chat_history.jsonl` into candidate decisions + per-decision results.
    fn parse_chat_history(
        path: &Path,
    ) -> Result<(Vec<CandidateDecision>, HashMap<String, CandidateResult>)> {
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
                        decisions.push(CandidateDecision {
                            id: tc.id,
                            name: tc.name,
                            arguments: tc.arguments,
                        });
                    }
                }
                "tool_result" | "tool" => {
                    if let Some(id) = &entry.tool_call_id {
                        results.insert(
                            id.clone(),
                            CandidateResult {
                                content: entry.content,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        Ok((decisions, results))
    }

    /// Parse `events.jsonl` preserving order, as a consumable queue.
    fn parse_events_ordered(path: &Path) -> Result<VecDeque<CandidateEvent>> {
        let mut queue = VecDeque::new();
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
                    queue.push_back(CandidateEvent {
                        tool_name: name,
                        outcome,
                        ts: event.ts,
                    });
                }
            }
        }
        Ok(queue)
    }

    /// Collect assistant reasoning texts (feed `reasoning_extractor`).
    fn collect_assistant_texts(path: &Path) -> Result<Vec<String>> {
        let mut texts = Vec::new();
        for line in std::fs::read_to_string(path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: ChatEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.entry_type != "assistant" {
                continue;
            }
            let text = match &entry.content {
                serde_json::Value::String(s) => s.clone(),
                _ => continue,
            };
            if text.len() >= 100 {
                texts.push(text);
            }
        }
        Ok(texts)
    }
}
