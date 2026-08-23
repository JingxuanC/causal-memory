//! Shared benchmark harness library (mem0/memory-benchmarks `benchmarks/common`
//! analogue). Every bench keeps its dataset protocol, ingest specifics and
//! scoring rules; the undifferentiated plumbing lives here:
//!
//! - [`LlmConfig`]/[`chat`] — OpenAI-compatible chat client with retries
//!   (DEEPSEEK_API_KEY / LOCOMO_LLM_API / LOCOMO_LLM_MODEL convention)
//! - [`Judge`] — binary yes/no judge parsing with malformed-output tolerance
//! - [`run_dir`] / [`summary_path`] — timestamped results-dir convention
//! - [`estimate_tokens`] — the ~4-chars-per-token budget estimator
//!
//! Consumed by benches/longmemeval (first); locomo/memora migrate as they
//! touch this code next (their copies diverged in judge details — migrating
//! them is a behavior-change review, not a copy).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

pub const LLM_TEMPERATURE: f32 = 0.0;
pub const LLM_RETRIES: u32 = 4;

/// OpenAI-compatible chat configuration (env convention shared by all
/// benches since the LoCoMo harness introduced it).
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

impl LlmConfig {
    /// DEEPSEEK_API_KEY (or CAUSAL_MEMORY_LLM_KEY); LOCOMO_LLM_API
    /// (default <https://api.deepseek.com/v1>); LOCOMO_LLM_MODEL
    /// (default deepseek-chat).
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .or_else(|_| std::env::var("CAUSAL_MEMORY_LLM_KEY"))
            .map_err(|_| anyhow!("DEEPSEEK_API_KEY (or CAUSAL_MEMORY_LLM_KEY) not set"))?;
        Ok(Self {
            api_base: std::env::var("LOCOMO_LLM_API")
                .unwrap_or_else(|_| "https://api.deepseek.com/v1".into()),
            api_key,
            model: std::env::var("LOCOMO_LLM_MODEL").unwrap_or_else(|_| "deepseek-chat".into()),
        })
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize, Deserialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatOwnedMessage,
}

#[derive(Deserialize)]
struct ChatOwnedMessage {
    content: String,
}

async fn chat_once(
    client: &reqwest::Client,
    cfg: &LlmConfig,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<String> {
    let req = ChatRequest {
        model: &cfg.model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: user,
            },
        ],
        max_tokens,
        temperature: LLM_TEMPERATURE,
    };
    let url = format!("{}/chat/completions", cfg.api_base.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .json(&req)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // 4xx other than 429 is not retryable.
        if status.is_client_error() && status.as_u16() != 429 {
            anyhow::bail!(
                "LLM API {status} (not retryable): {}",
                &body[..body.len().min(300)]
            );
        }
        anyhow::bail!("LLM API {status}: {}", &body[..body.len().min(300)]);
    }
    let chat: ChatResponse = resp.json().await?;
    chat.choices
        .first()
        .map(|c| c.message.content.trim().to_string())
        .ok_or_else(|| anyhow!("no choices in LLM response"))
}

/// Chat completion with exponential-backoff retries (1s, 2s, 4s, ...).
pub async fn chat(cfg: &LlmConfig, system: &str, user: &str, max_tokens: u32) -> Result<String> {
    let client = reqwest::Client::new();
    let mut last_err = anyhow!("no attempt made");
    for attempt in 0..=LLM_RETRIES {
        match chat_once(&client, cfg, system, user, max_tokens).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                let retryable = !e.to_string().contains("not retryable");
                last_err = e;
                if !retryable || attempt == LLM_RETRIES {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
            }
        }
    }
    Err(last_err)
}

/// Binary judge verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Yes,
    No,
    Error,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Yes => "correct",
            Verdict::No => "incorrect",
            Verdict::Error => "error",
        }
    }
}

/// Judge: binary LLM correctness call. Malformed output counts as Error
/// (never a silent pass); the bench's scoring treats Error as not-correct.
pub async fn judge(
    cfg: &LlmConfig,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> (Verdict, String) {
    match chat(cfg, system, user, max_tokens).await {
        Ok(raw) => (parse_verdict(&raw), raw),
        Err(e) => (Verdict::Error, format!("judge LLM failed: {e}")),
    }
}

/// Parse a yes/no judge reply: first word wins, tolerant of prefixes like
/// "Yes." / "**yes**" / "yes — because...".
pub fn parse_verdict(raw: &str) -> Verdict {
    let first = raw.trim().trim_start_matches('*').to_lowercase();
    let first = first.split_whitespace().next().unwrap_or_default();
    let first = first.trim_matches(|c: char| !c.is_ascii_alphabetic());
    match first {
        "yes" => Verdict::Yes,
        "no" => Verdict::No,
        _ => Verdict::Error,
    }
}

/// Timestamped run directory under a bench's results root:
/// `run_YYYYMMDD_HHMMSS` (the convention every existing bench writes).
pub fn run_dir(results_root: &std::path::Path) -> std::path::PathBuf {
    let id = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    results_root.join(format!("run_{id}"))
}

/// Companion paths for a run dir: `<dir>.jsonl` rows + `<dir>_summary.json`.
pub fn summary_path(run: &std::path::Path) -> std::path::PathBuf {
    run.with_extension("summary.json")
}

/// ~4-chars-per-token estimator shared by benches (same as
/// causal_memory::token::estimate_tokens; re-exported path keeps benches
/// single-sourced on the lib constant).
pub fn estimate_tokens(text: &str) -> usize {
    causal_memory::token::estimate_tokens(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_parsing_tolerates_common_wrappers() {
        assert_eq!(parse_verdict("yes"), Verdict::Yes);
        assert_eq!(parse_verdict("Yes."), Verdict::Yes);
        assert_eq!(parse_verdict("**yes** because gold says 3"), Verdict::Yes);
        assert_eq!(parse_verdict("no"), Verdict::No);
        assert_eq!(parse_verdict("NO — the answer is 2"), Verdict::No);
        assert_eq!(parse_verdict(""), Verdict::Error);
        assert_eq!(parse_verdict("maybe"), Verdict::Error);
    }
}
