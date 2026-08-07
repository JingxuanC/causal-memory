//! Reasoning-level decision extractor.
//!
//! v0.4 feature: extracts decisions from `assistant.content` text, not
//! just from `tool_calls`. This addresses the v0.3 finding that tool_call
//! events are mostly trivial operations (echo, cat, git status) while
//! the real lessons live in the agent's reasoning text.
//!
//! ## How it works
//!
//! 1. Scans `chat_history.jsonl` for `assistant` entries with non-trivial text content
//! 2. Sends each to an LLM with a focused prompt: "extract any decisions,
//!    judgments, or lessons worth remembering from this reasoning"
//! 3. If the LLM returns structured decisions, records them with `llm_inferred`
//!    confidence and a `reasoning` task_tag
//!
//! This is more expensive than tool_call extraction (one LLM call per
//! assistant message) but captures the high-value decisions that tool_call
//! extraction structurally misses.

use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

use crate::llm::{judge_causality, LlmConfig};
use crate::session::{GrokParser, ParsedSession, SessionParser, SessionSource};
use crate::store::CausalStore;

const REASONING_EXTRACT_PROMPT: &str = r#"You are extracting decisions from an AI agent's reasoning text.

Read the following assistant message and extract any DECISIONS or LESSONS that would be valuable to remember for future similar tasks. Focus on:
- Architecture/design choices ("I chose X over Y because...")
- Strategy decisions ("Let's do X first, then Y")
- Debugging insights ("The root cause was X, which means Y")
- Rejections of alternatives ("X won't work because...")
- Surprising findings ("Contrary to expectation, X caused Y")

DO NOT extract:
- Routine descriptions of what was done
- Factual observations without decision value
- Questions to the user

Respond with ONLY a JSON array (empty if no decisions worth remembering):
[{"decision": "<what was decided>", "reasoning": "<why it matters>"}]

Maximum 3 decisions per message. Be selective."#;

#[derive(Debug, Deserialize)]
struct ExtractedDecision {
    decision: String,
    #[serde(default)]
    reasoning: String,
}

#[derive(Debug, Default, Clone)]
pub struct ReasoningExtractionStats {
    pub messages_scanned: usize,
    pub messages_with_decisions: usize,
    pub decisions_extracted: usize,
    pub edges_inserted: usize,
    pub llm_calls: usize,
    pub llm_errors: usize,
}

pub struct ReasoningExtractor;

impl ReasoningExtractor {
    /// Extract reasoning-level decisions from chat_history using an LLM.
    ///
    /// This is expensive (one LLM call per non-trivial assistant message)
    /// but captures high-value decisions that tool_call extraction misses.
    /// Extract reasoning-level decisions from a grok session directory
    /// (backward-compatible entry).
    pub async fn extract_from_session(
        store: &CausalStore,
        session_dir: &Path,
        config: &LlmConfig,
        max_messages: usize,
    ) -> Result<ReasoningExtractionStats> {
        let parsed = GrokParser.parse(&SessionSource::dir(session_dir))?;
        Self::extract_from_parsed(store, &parsed, config, max_messages).await
    }

    /// Extract reasoning-level decisions from a format-agnostic parsed session.
    ///
    /// This is expensive (one LLM call per non-trivial assistant message)
    /// but captures high-value decisions that tool_call extraction misses.
    pub async fn extract_from_parsed(
        store: &CausalStore,
        parsed: &ParsedSession,
        config: &LlmConfig,
        max_messages: usize,
    ) -> Result<ReasoningExtractionStats> {
        // Phase 1: collect non-trivial assistant messages (limited)
        let messages: Vec<String> = parsed
            .assistant_texts
            .iter()
            .take(max_messages)
            .cloned()
            .collect();
        let mut stats = ReasoningExtractionStats {
            messages_scanned: messages.len(),
            ..Default::default()
        };

        let client = reqwest::Client::new();
        let url = format!("{}/chat/completions", config.api_base.trim_end_matches('/'));

        // Phase 2: for each message, ask LLM to extract decisions
        for msg_text in &messages {
            // Skip very short messages (likely just "ok" or status updates)
            if msg_text.len() < 100 {
                continue;
            }

            stats.llm_calls += 1;

            let decisions =
                match Self::extract_decisions_from_text(&client, config, &url, msg_text).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("LLM extraction failed: {e}");
                        stats.llm_errors += 1;
                        continue;
                    }
                };

            if decisions.is_empty() {
                continue;
            }
            stats.messages_with_decisions += 1;

            // Phase 3: record each decision
            for dec in decisions {
                stats.decisions_extracted += 1;

                // Use the LLM judge to assess confidence
                let (confidence, _reason) =
                    match judge_causality(config, &dec.decision, &dec.reasoning).await {
                        Ok((c, r)) => (c, r),
                        Err(_) => (0.5, String::new()), // fallback
                    };

                // Skip trivial decisions (LLM judge says < 0.3)
                if confidence < 0.3 {
                    continue;
                }

                match store.record_decision(
                    &dec.decision,
                    &dec.reasoning,
                    "caused",
                    Some("reasoning"),
                    confidence,
                    "llm_inferred",
                ) {
                    Ok(_) => stats.edges_inserted += 1,
                    Err(e) => tracing::warn!("Insert failed: {e}"),
                }
            }
        }

        Ok(stats)
    }

    async fn extract_decisions_from_text(
        client: &reqwest::Client,
        config: &LlmConfig,
        url: &str,
        text: &str,
    ) -> Result<Vec<ExtractedDecision>> {
        use serde::Serialize;

        #[derive(Serialize)]
        struct ChatRequest<'a> {
            model: &'a str,
            messages: Vec<serde_json::Value>,
            max_tokens: u32,
            temperature: f32,
        }

        // Truncate very long texts (char-safe, not byte-safe)
        let truncated = if text.chars().count() > 2000 {
            format!("{}...", text.chars().take(2000).collect::<String>())
        } else {
            text.to_string()
        };

        let req = ChatRequest {
            model: &config.model,
            messages: vec![
                serde_json::json!({"role": "system", "content": REASONING_EXTRACT_PROMPT}),
                serde_json::json!({"role": "user", "content": truncated}),
            ],
            max_tokens: 500,
            temperature: 0.0,
        };

        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {}", config.api_key))
            .json(&req)
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("LLM API status {}", resp.status());
        }

        let body: serde_json::Value = resp.json().await?;
        let content = body["choices"]
            .get(0)
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("[]");

        // Parse JSON from response (handle markdown wrapping)
        let json_str = content
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        let decisions: Vec<ExtractedDecision> = serde_json::from_str(json_str).unwrap_or_default();

        Ok(decisions)
    }
}
