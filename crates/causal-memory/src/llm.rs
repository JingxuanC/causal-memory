//! LLM-enhanced causal inference.
//!
//! When CAUSAL_MEMORY_LLM_API is set, calls a real LLM to judge the causal
//! strength between a decision and its outcome. Otherwise returns None and
//! the caller falls back to the rule-based path.
//!
//! Compatible with any OpenAI-style /v1/chat/completions endpoint:
//! DeepSeek, OpenAI, Moonshot, GLM, Groq, Together, xAI, local Ollama, etc.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// HTTP timeout for the LLM endpoint. Default 8s: the record path calls this
/// synchronously inside an MCP tool handler (60s tool timeout), so 8s is long
/// enough for slow models, short enough that an unreachable endpoint fails
/// fast and the caller falls back instead of hanging. Long-running callers
/// (bench agent loops with growing transcripts) override it via
/// CAUSAL_MEMORY_HTTP_TIMEOUT_SECS.
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 8;

/// Pure parsing, split from env access so tests never mutate process env
/// (env writes race under `cargo test`'s parallel harness).
fn timeout_secs(env_value: Option<&str>) -> u64 {
    env_value
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_SECS)
}

fn http_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(timeout_secs(
        crate::config::get("CAUSAL_MEMORY_HTTP_TIMEOUT_SECS").as_deref(),
    ))
}

const CAUSAL_JUDGE_PROMPT: &str = r#"You are judging whether a decision-outcome pair is worth remembering as a LESSON for future similar tasks.

A lesson worth remembering has these properties:
- The outcome was SURPRISING (success or failure) — not the obvious/direct result
- A different decision could have led to a different outcome
- Future agents facing a similar choice would benefit from knowing this

Trivial pairs like "wrote a file → file was written" or "ran a command → got output" are NOT worth remembering. Everyone knows writing creates a file.

Interesting pairs look like:
- "chose mutex → deadlock" (surprising failure, alternative existed)
- "switched to channels → fixed race" (non-obvious solution)
- "used Redis 7.2 → stampede" (specific version caused specific failure)

Respond with ONLY a JSON object:
{"confidence": <0.0-1.0>, "reasoning": "<one sentence>"}

confidence guide:
- 0.8-1.0: High-value lesson, surprising outcome, worth remembering
- 0.4-0.6: Moderate lesson, some non-obvious insight
- 0.0-0.2: Trivial/obvious, NOT worth remembering (the default for routine operations)"#;

const POLARITY_JUDGE_PROMPT: &str = r#"You are judging the polarity of an OUTCOME as the direct result of a DECISION.

Judge the outcome as it happened to THIS decision — not whether the problem was eventually fixed later or by someone else.

Categories:
- "positive": the outcome is a success / things worked as intended
- "negative": the outcome is a failure / error / regression caused by this decision
- "mixed": the outcome contains BOTH a failure caused by this decision AND a fix or success in the same statement (e.g. "deadlock under load; fixed by switching to channels" — the deadlock is this decision's direct result, so it is NOT purely positive)
- "neutral": neither clearly good nor bad

Respond with ONLY a JSON object:
{"polarity": "positive|negative|mixed|neutral"}"#;

const SUPERSESSION_JUDGE_PROMPT: &str = r#"You are an update-resolver for an agent's causal memory.

An agent recorded an OLD lesson (decision -> outcome). Later it recorded a NEW decision with a different outcome. Determine whether the NEW evidence FALSIFIES / SUPERSEDES the OLD lesson — i.e. the old belief is now known to be wrong and should be retired — or whether they are separate lessons that should both stay.

Supersedes = TRUE when:
- the same underlying decision/approach was re-attempted and the new outcome contradicts the old one (e.g. "used X -> failed" then "used X -> worked")
- the new information explains why the old conclusion no longer holds
- the new decision is a correction/retraction of the old one

Supersedes = FALSE when:
- the decisions are genuinely different actions
- the outcomes are different but not contradictory (different facets)
- the new record is a refinement/extension, not a contradiction

Respond with ONLY a JSON object:
{"supersedes": <true|false>, "reasoning": "<one sentence>"}"#;

const RECONSTRUCT_PROMPT: &str = r#"You are reconstructing a LESSON from an agent's causal memory.

You are given a subgraph of causal edges (decision → outcome, with relation, confidence, and polarity) around a topic. Write one short, coherent narrative (3-6 sentences) that distils the lesson these edges teach.

Rules:
- Base every claim ONLY on the given edges — do not invent facts, decisions, or outcomes beyond them.
- When an edge is central to the lesson, mention its confidence (e.g. "high confidence").
- If edges conflict, say so instead of picking one silently.
- Write in the same language as the edges."#;

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct CausalJudgment {
    confidence: f64,
    #[serde(default)]
    reasoning: String,
}

#[derive(Debug, Deserialize)]
struct PolarityJudgment {
    polarity: String,
}

/// Configuration for the LLM judge. Read from environment variables.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

impl LlmConfig {
    /// Load from env / config file (`config::get`: process env wins, the
    /// JSON config file is the fallback). Returns None if not configured
    /// (caller falls back to rules).
    pub fn from_env() -> Option<Self> {
        let api_base = crate::config::get("CAUSAL_MEMORY_LLM_API")?;
        let api_key = crate::config::get("CAUSAL_MEMORY_LLM_KEY")
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())?;
        let model =
            crate::config::get("CAUSAL_MEMORY_LLM_MODEL").unwrap_or_else(|| "deepseek-chat".into());
        Some(Self {
            api_base,
            api_key,
            model,
        })
    }
}

/// Judge the causal strength between a decision and its outcome using an LLM.
///
/// Returns (confidence, reasoning). Falls back to None on any error
/// (caller should handle by using the rule-based path).
pub async fn judge_causality(
    config: &LlmConfig,
    decision: &str,
    outcome: &str,
) -> Result<(f64, String)> {
    let user_msg = format!(
        "Decision: {}\nOutcome: {}\n\nJudge the causal confidence.",
        decision,
        outcome.chars().take(500).collect::<String>()
    );

    let content = chat(config, CAUSAL_JUDGE_PROMPT, &user_msg, 100, 0.0).await?;
    let judgment: CausalJudgment = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse LLM judgment: {e}\nRaw: {content}"))?;

    Ok((judgment.confidence, judgment.reasoning))
}

/// Judge the polarity of an outcome as the direct result of a decision
/// (write-time, v4). Returns one of "positive" / "negative" / "mixed" /
/// "neutral"; any parse failure or out-of-enum value is an Err — the caller
/// falls back to the signal-word heuristic.
pub async fn judge_polarity(config: &LlmConfig, decision: &str, outcome: &str) -> Result<String> {
    let user_msg = format!(
        "Decision: {}\nOutcome: {}\n\nJudge the outcome polarity.",
        decision,
        outcome.chars().take(500).collect::<String>()
    );

    let content = chat(config, POLARITY_JUDGE_PROMPT, &user_msg, 100, 0.0).await?;
    let judgment: PolarityJudgment = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse LLM polarity: {e}\nRaw: {content}"))?;

    match judgment.polarity.as_str() {
        "positive" | "negative" | "mixed" | "neutral" => Ok(judgment.polarity),
        other => anyhow::bail!("LLM returned unknown polarity: {other}"),
    }
}

/// Verdict of the update-resolver (C7): does the new evidence falsify the
/// old lesson?
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SupersessionVerdict {
    pub supersedes: bool,
    #[serde(default)]
    pub reasoning: String,
}

/// Judge whether NEW evidence supersedes (falsifies) an OLD recorded lesson
/// (C7 update-resolver). Returns the verdict; any failure is an Err so the
/// caller can keep the rule-based behaviour as the fallback.
pub async fn judge_supersession(
    config: &LlmConfig,
    old_decision: &str,
    old_outcome: &str,
    new_decision: &str,
    new_outcome: &str,
) -> Result<SupersessionVerdict> {
    let user_msg = format!(
        "OLD lesson:\n  decision: {}\n  outcome: {}\n\nNEW evidence:\n  decision: {}\n  outcome: {}\n\nDoes the new evidence supersede the old lesson?",
        old_decision.chars().take(300).collect::<String>(),
        old_outcome.chars().take(300).collect::<String>(),
        new_decision.chars().take(300).collect::<String>(),
        new_outcome.chars().take(300).collect::<String>(),
    );

    let content = chat(config, SUPERSESSION_JUDGE_PROMPT, &user_msg, 120, 0.0).await?;
    serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse supersession verdict: {e}\nRaw: {content}"))
}

/// Reconstruct a lesson narrative from a causal subgraph (reconstructive
/// retrieval, Schacter 2007): the caller supplies compact edge stubs and the
/// LLM retells the lesson instead of the raw records being returned.
/// `temperature` > 0 is used by the calibration path (multiple independent
/// reconstructions); the base narrative passes 0.0.
pub async fn reconstruct_narrative(
    config: &LlmConfig,
    query: &str,
    stubs: &str,
    temperature: f32,
) -> Result<String> {
    let user_msg = format!("Topic: {query}\n\nCausal edges:\n{stubs}");
    chat(config, RECONSTRUCT_PROMPT, &user_msg, 400, temperature).await
}

/// Shared chat-completions call: POSTs system+user messages, returns the
/// reply content with any markdown code fence stripped.
pub async fn chat(
    config: &LlmConfig,
    system_prompt: &str,
    user_msg: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    let req = ChatRequest {
        model: config.model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt.into(),
            },
            ChatMessage {
                role: "user".into(),
                content: user_msg.into(),
            },
        ],
        max_tokens,
        temperature,
    };

    let client = reqwest::Client::builder()
        .timeout(http_timeout())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = format!("{}/chat/completions", config.api_base.trim_end_matches('/'));

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("LLM API returned status {}", resp.status());
    }

    let chat_resp: ChatResponse = resp.json().await?;
    let content = chat_resp
        .choices
        .first()
        .ok_or_else(|| anyhow::anyhow!("No choices in LLM response"))?
        .message
        .content
        .trim();

    // Parse JSON from response (LLMs sometimes wrap in markdown)
    let json_str = if content.starts_with("```") {
        content
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        content
    };

    Ok(json_str.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_secs() {
        assert_eq!(timeout_secs(None), 8, "default keeps the MCP-path behavior");
        assert_eq!(timeout_secs(Some("60")), 60);
        assert_eq!(timeout_secs(Some("0")), 8, "zero is invalid → default");
        assert_eq!(timeout_secs(Some("abc")), 8, "unparseable → default");
    }
}
