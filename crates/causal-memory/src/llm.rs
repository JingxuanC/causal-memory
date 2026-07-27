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
    /// Load from env. Returns None if not configured (caller falls back to rules).
    pub fn from_env() -> Option<Self> {
        let api_base = std::env::var("CAUSAL_MEMORY_LLM_API").ok()?;
        let api_key = std::env::var("CAUSAL_MEMORY_LLM_KEY")
            .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
            .ok()?;
        let model =
            std::env::var("CAUSAL_MEMORY_LLM_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
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

    let content = chat(config, CAUSAL_JUDGE_PROMPT, &user_msg).await?;
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

    let content = chat(config, POLARITY_JUDGE_PROMPT, &user_msg).await?;
    let judgment: PolarityJudgment = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse LLM polarity: {e}\nRaw: {content}"))?;

    match judgment.polarity.as_str() {
        "positive" | "negative" | "mixed" | "neutral" => Ok(judgment.polarity),
        other => anyhow::bail!("LLM returned unknown polarity: {other}"),
    }
}

/// Shared chat-completions call: POSTs system+user messages, returns the
/// reply content with any markdown code fence stripped.
async fn chat(config: &LlmConfig, system_prompt: &str, user_msg: &str) -> Result<String> {
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
        max_tokens: 100,
        temperature: 0.0,
    };

    let client = reqwest::Client::new();
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
