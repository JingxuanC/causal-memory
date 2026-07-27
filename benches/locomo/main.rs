//! LoCoMo benchmark harness for causal-memory.
//!
//! Ingests LoCoMo conversations (locomo10.json) into per-conversation
//! causal-memory SQLite DBs, then answers and judges the QA set with an
//! OpenAI-compatible LLM (DeepSeek by default), following the LoCoMo
//! evaluation protocol (answer + judge, per-category accuracy).
//!
//! Subcommands:
//!   causal-memory-locomo run --data benches/locomo/data/locomo10.json [options]
//!
//! Env:
//!   DEEPSEEK_API_KEY        (required; or CAUSAL_MEMORY_LLM_KEY)
//!   LOCOMO_LLM_API          (default: https://api.deepseek.com/v1)
//!   LOCOMO_LLM_MODEL        (default: deepseek-chat, used for answer + judge)

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use causal_memory::store::{CausalEntry, CausalStore};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

/// Synthetic base timestamp (2023-05-01T00:00:00Z) used when a session's
/// date_time string cannot be parsed. Sessions are spaced one day apart so
/// ordering is preserved even in the fallback path.
const SYNTH_BASE_TS: i64 = 1_682_899_200;

/// Edge metadata written between consecutive turns of opposite speakers.
const TURN_EDGE_RELATION: &str = "caused";
const TURN_EDGE_CONFIDENCE: f64 = 0.4;
const TURN_EDGE_DISCOVERED_BY: &str = "temporal";

/// LLM answer generation settings (LoCoMo protocol).
const ANSWER_MAX_TOKENS: u32 = 200;
const JUDGE_MAX_TOKENS: u32 = 200;
const LLM_TEMPERATURE: f32 = 0.0;
const LLM_RETRIES: usize = 3;

const ANSWER_SYSTEM_PROMPT: &str = r#"You are answering questions about a conversation between two people, using memory snippets retrieved from that conversation.

Rules:
- Base your answer ONLY on the memories provided below.
- Keep the answer short: a few words or one sentence.
- Each memory is prefixed with the session date, e.g. "[session_3 2023-05-08 13:56]". When the question asks WHEN something happened, resolve relative time expressions ("yesterday", "last week", "next month", "last year") against that date and answer with an ABSOLUTE date or time period (e.g. "7 May 2023", "June 2023"), not the relative expression.
- If the memories do not contain the answer, say "I don't know" or state that the information was not mentioned in the conversation."#;

const JUDGE_SYSTEM_PROMPT: &str = r#"You are an impartial judge evaluating whether a predicted answer correctly answers a question about a conversation.

Respond with ONLY a JSON object (no markdown, no extra text):
{"verdict": "correct" or "incorrect", "reason": "<one sentence>"}"#;

fn usage() {
    eprintln!("Usage: causal-memory-locomo run --data <locomo10.json> [options]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --data PATH         LoCoMo dataset JSON (required)");
    eprintln!("  --conv N            run only conversation index N");
    eprintln!("  --all               run all conversations (default)");
    eprintln!("  --limit K           max questions per conversation (cost guard)");
    eprintln!("  --categories LIST   comma-separated categories, e.g. 1,2,3,4,5 (default: all)");
    eprintln!("  --db-dir DIR        per-conversation DBs (default: benches/locomo/db)");
    eprintln!("  --out DIR           results dir (default: benches/locomo/results)");
    eprintln!("  --topk N            retrieved memories per question (default: 10)");
    eprintln!("  --concurrency N     parallel questions (default: 8)");
    eprintln!();
    eprintln!("Env: DEEPSEEK_API_KEY (required), LOCOMO_LLM_API, LOCOMO_LLM_MODEL");
    eprintln!();
    eprintln!("Smoke test: causal-memory-locomo run --data benches/locomo/data/locomo10.json --conv 0 --limit 10");
}

// ---------------------------------------------------------------------------
// Dataset model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LocomoConversation {
    /// Mixed map: `session_N` -> turn array, `session_N_date_time` -> string.
    conversation: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    qa: Vec<Qa>,
}

#[derive(Debug, Clone, Deserialize)]
struct Turn {
    speaker: String,
    dia_id: String,
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Qa {
    question: String,
    /// Absent for category 5 (adversarial); may be a string OR a number.
    #[serde(default)]
    answer: Option<serde_json::Value>,
    /// The hallucinated answer a fooled model would give (category 5 only).
    #[serde(default)]
    adversarial_answer: Option<serde_json::Value>,
    #[serde(default)]
    evidence: Vec<String>,
    category: u32,
}

/// One session of a conversation, flattened to turns with resolved times.
#[derive(Debug)]
struct Session {
    number: u32,
    date_time_raw: Option<String>,
    turns: Vec<Turn>,
}

impl LocomoConversation {
    /// Extract sessions ordered by session number.
    fn sessions(&self) -> Result<Vec<Session>> {
        let mut numbers: Vec<u32> = Vec::new();
        for key in self.conversation.keys() {
            if let Some(n) = key
                .strip_prefix("session_")
                .and_then(|s| s.parse::<u32>().ok())
            {
                numbers.push(n);
            }
        }
        numbers.sort_unstable();
        numbers.dedup();

        let mut sessions = Vec::new();
        for n in numbers {
            let turns: Vec<Turn> = match self.conversation.get(&format!("session_{n}")) {
                Some(v) => serde_json::from_value(v.clone())
                    .with_context(|| format!("bad turn list in session_{n}"))?,
                None => continue,
            };
            let date_time_raw = self
                .conversation
                .get(&format!("session_{n}_date_time"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            sessions.push(Session {
                number: n,
                date_time_raw,
                turns,
            });
        }
        Ok(sessions)
    }
}

/// Stringify a gold/adversarial answer that may be a JSON string or number.
fn answer_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Date parsing
// ---------------------------------------------------------------------------

/// Parse LoCoMo session timestamps like "1:56 pm on 8 May, 2023" into a unix
/// timestamp (treated as UTC). Returns None when no known format matches.
fn parse_session_datetime(s: &str) -> Option<i64> {
    const FORMATS: &[&str] = &[
        "%I:%M %p on %e %b, %Y", // 1:56 pm on 8 May, 2023
        "%I:%M %p on %d %b, %Y", // 1:56 pm on 08 May, 2023
        "%I:%M %p on %e %B, %Y", // 1:56 pm on 8 May(full month), 2023
        "%I:%M %p on %d %B, %Y",
    ];
    let trimmed = s.trim();
    FORMATS.iter().find_map(|fmt| {
        NaiveDateTime::parse_from_str(trimmed, fmt)
            .ok()
            .map(|dt| dt.and_utc().timestamp())
    })
}

/// Event time for a turn: parsed session time + turn offset (seconds) to keep
/// intra-session ordering; synthetic fallback keeps sessions one day apart.
fn turn_event_time(session_base: i64, turn_idx: usize) -> i64 {
    session_base + turn_idx as i64
}

fn session_base_time(session: &Session) -> i64 {
    match session
        .date_time_raw
        .as_deref()
        .and_then(parse_session_datetime)
    {
        Some(ts) => ts,
        None => {
            eprintln!(
                "warn: session_{} date_time {:?} unparsable, using synthetic timestamp",
                session.number, session.date_time_raw
            );
            SYNTH_BASE_TS + session.number as i64 * 86_400
        }
    }
}

fn format_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown-time".to_string())
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/// Ingest all sessions of a conversation into the store.
///
/// Each turn becomes one chunk keyed by its dia_id (e.g. "D1:3") so QA
/// evidence ids can be matched against retrieval results. Chunk text is
/// "[session_N <date>] speaker: text". Consecutive turns of opposite
/// speakers are linked with a low-confidence `caused` edge (temporal
/// discovery) to give the graph basic connectivity.
///
/// Idempotent: if the store already holds exactly the expected chunk count,
/// ingestion is skipped; on a partial/stale DB the tables are wiped and
/// re-ingested.
fn ingest_conversation(store: &CausalStore, conv: &LocomoConversation) -> Result<usize> {
    let sessions = conv.sessions()?;
    let expected_chunks: usize = sessions.iter().map(|s| s.turns.len()).sum();

    let existing: i64 =
        store.with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?))?;
    if existing == expected_chunks as i64 && expected_chunks > 0 {
        return Ok(expected_chunks);
    }
    if existing > 0 {
        eprintln!(
            "warn: DB has {existing} chunks, expected {expected_chunks}; re-ingesting from scratch"
        );
        store.with_conn(|c| {
            c.execute("DELETE FROM causal_edges", [])?;
            c.execute("DELETE FROM chunks", [])?;
            Ok(())
        })?;
    }

    let mut written = 0usize;
    for session in &sessions {
        let base = session_base_time(session);

        for (idx, turn) in session.turns.iter().enumerate() {
            let ts = turn_event_time(base, idx);
            let chunk_text = format!(
                "[session_{} {}] {}: {}",
                session.number,
                format_ts(ts),
                turn.speaker,
                turn.text
            );
            store.with_conn(|c| {
                c.execute(
                    "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![&turn.dia_id, &chunk_text, ts],
                )?;
                Ok(())
            })?;

            // Link each turn to the nearest preceding turn from the OTHER
            // speaker (the turn it responds to). The first turn of a session,
            // or a turn with no prior opposite-speaker turn, gets no edge.
            let prev_other = session.turns[..idx]
                .iter()
                .rev()
                .find(|t| t.speaker != turn.speaker);
            if let Some(prev) = prev_other {
                store.with_conn(|c| {
                    c.execute(
                        "INSERT INTO causal_edges
                         (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                        rusqlite::params![
                            &prev.dia_id,
                            &turn.dia_id,
                            TURN_EDGE_RELATION,
                            TURN_EDGE_CONFIDENCE,
                            TURN_EDGE_DISCOVERED_BY,
                            ts,
                            ts
                        ],
                    )?;
                    Ok(())
                })?;
            }
            written += 1;
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

/// Retrieve candidate causal entries for a question (run 3: BM25).
///
/// Single path: `search_causal_bm25(None, question, topk)` — the question is
/// tokenized and BM25-ranked against all valid edges (decision + outcome
/// text). This replaces the run-2 logic (whole-question LIKE, then a keyword
/// fan-out of per-word LIKE ranked by hit count): natural-language questions
/// almost never appear verbatim in turn text, and the fan-out had no IDF or
/// length normalization — BM25 fixes both while staying dependency-free.
fn retrieve(store: &CausalStore, question: &str, topk: usize) -> Result<Vec<CausalEntry>> {
    store.search_causal_bm25(None, question, topk)
}

/// Chunk ids covered by the retrieval result (decision + outcome endpoints).
fn retrieved_chunk_ids(entries: &[CausalEntry]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in entries {
        for id in [&e.decision_id, &e.outcome_id] {
            if seen.insert(id.clone()) {
                out.push(id.clone());
            }
        }
    }
    out
}

/// Memory lines for the answer prompt, deduplicated by chunk id.
fn memory_lines(entries: &[CausalEntry]) -> String {
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for e in entries {
        for (id, text) in [
            (&e.decision_id, &e.decision_text),
            (&e.outcome_id, &e.outcome_text),
        ] {
            if seen.insert(id.clone()) {
                lines.push(format!("- {text}"));
            }
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// LLM client (OpenAI-compatible, retry with exponential backoff)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LlmConfig {
    api_base: String,
    api_key: String,
    model: String,
}

impl LlmConfig {
    fn from_env() -> Result<Self> {
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

/// Single chat completion attempt.
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

/// Chat with up to LLM_RETRIES retries and exponential backoff (1s, 2s, 4s).
async fn chat(cfg: &LlmConfig, system: &str, user: &str, max_tokens: u32) -> Result<String> {
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
                let delay = std::time::Duration::from_secs(1 << attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
    Err(last_err)
}

// ---------------------------------------------------------------------------
// Judge verdict parsing
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Verdict {
    Correct,
    Incorrect,
    /// Infrastructure failure (LLM unreachable after retries) or unparseable
    /// judge output — excluded from accuracy.
    Error,
}

impl Verdict {
    fn as_str(&self) -> &'static str {
        match self {
            Verdict::Correct => "correct",
            Verdict::Incorrect => "incorrect",
            Verdict::Error => "error",
        }
    }
}

/// Parse the judge's JSON verdict, tolerating ```json fences and surrounding
/// prose. Returns None when no parseable verdict object is present.
fn parse_judge_output(raw: &str) -> Option<(Verdict, String)> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("```") {
        s = rest.strip_prefix("json").unwrap_or(rest).trim();
        s = s.strip_suffix("```").unwrap_or(s).trim();
    }
    // Tolerate prose around the JSON object: take the outermost {...}.
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&s[start..=end]).ok()?;
    let verdict = match v.get("verdict")?.as_str()?.to_ascii_lowercase().as_str() {
        "correct" => Verdict::Correct,
        "incorrect" => Verdict::Incorrect,
        _ => return None,
    };
    let reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    Some((verdict, reason))
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

struct Args {
    data: PathBuf,
    conv: Option<usize>,
    limit: Option<usize>,
    categories: HashSet<u32>,
    db_dir: PathBuf,
    out_dir: PathBuf,
    topk: usize,
    concurrency: usize,
}

fn parse_args(argv: &[String]) -> Result<Option<Args>> {
    if argv.is_empty() || argv.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(None);
    }
    if argv[0] != "run" {
        anyhow::bail!("unknown subcommand {:?}; expected `run`", argv[0]);
    }
    let mut data: Option<PathBuf> = None;
    let mut conv: Option<usize> = None;
    let mut all = false;
    let mut limit = None;
    let mut categories: Option<HashSet<u32>> = None;
    let mut db_dir = PathBuf::from("benches/locomo/db");
    let mut out_dir = PathBuf::from("benches/locomo/results");
    let mut topk = 10usize;
    let mut concurrency = 8usize;

    let mut i = 1;
    let take = |i: &mut usize, flag: &str| -> Result<String> {
        *i += 1;
        argv.get(*i)
            .cloned()
            .ok_or_else(|| anyhow!("missing value for {flag}"))
    };
    while i < argv.len() {
        match argv[i].as_str() {
            "--data" => data = Some(PathBuf::from(take(&mut i, "--data")?)),
            "--conv" => conv = Some(take(&mut i, "--conv")?.parse()?),
            "--all" => all = true,
            "--limit" => limit = Some(take(&mut i, "--limit")?.parse()?),
            "--categories" => {
                let raw = take(&mut i, "--categories")?;
                let set: Result<HashSet<u32>, _> =
                    raw.split(',').map(|s| s.trim().parse::<u32>()).collect();
                categories = Some(set.map_err(|e| anyhow!("bad --categories: {e}"))?);
            }
            "--db-dir" => db_dir = PathBuf::from(take(&mut i, "--db-dir")?),
            "--out" => out_dir = PathBuf::from(take(&mut i, "--out")?),
            "--topk" => topk = take(&mut i, "--topk")?.parse()?,
            "--concurrency" => concurrency = take(&mut i, "--concurrency")?.parse()?,
            other => anyhow::bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    if conv.is_some() && all {
        anyhow::bail!("--conv and --all are mutually exclusive");
    }
    let data = data.ok_or_else(|| anyhow!("--data is required"))?;
    Ok(Some(Args {
        data,
        conv,
        limit,
        categories: categories.unwrap_or_else(|| (1..=5).collect()),
        db_dir,
        out_dir,
        topk,
        concurrency,
    }))
}

#[derive(Serialize)]
struct ResultRow {
    conv: usize,
    category: u32,
    question: String,
    gold: Option<String>,
    predicted: String,
    verdict: String,
    judge_reason: String,
    retrieved_ids: Vec<String>,
    evidence_ids: Vec<String>,
    evidence_hit: bool,
}

#[derive(Serialize)]
struct CategoryStats {
    total: usize,
    correct: usize,
    incorrect: usize,
    error: usize,
    accuracy: f64,
}

#[derive(Serialize)]
struct Summary {
    run_id: String,
    date: String,
    git_commit: String,
    model: String,
    judge_model: String,
    temperature: f32,
    topk: usize,
    data: String,
    conversations: Vec<usize>,
    total_questions: usize,
    correct: usize,
    incorrect: usize,
    error: usize,
    accuracy: f64,
    evidence_hit_rate: f64,
    per_category: BTreeMap<String, CategoryStats>,
}

struct Acc {
    total: usize,
    correct: usize,
    incorrect: usize,
    error: usize,
    hits: usize,
}

impl Acc {
    fn new() -> Self {
        Self {
            total: 0,
            correct: 0,
            incorrect: 0,
            error: 0,
            hits: 0,
        }
    }
    fn add(&mut self, row: &ResultRow) {
        self.total += 1;
        match row.verdict.as_str() {
            "correct" => self.correct += 1,
            "incorrect" => self.incorrect += 1,
            _ => self.error += 1,
        }
        if row.evidence_hit {
            self.hits += 1;
        }
    }
    fn accuracy(&self) -> f64 {
        let graded = self.correct + self.incorrect;
        if graded == 0 {
            0.0
        } else {
            self.correct as f64 / graded as f64
        }
    }
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

async fn answer_question(
    cfg: &LlmConfig,
    store: &CausalStore,
    conv_idx: usize,
    qa: &Qa,
    topk: usize,
) -> ResultRow {
    let retrieved = retrieve(store, &qa.question, topk).unwrap_or_default();
    let retrieved_ids = retrieved_chunk_ids(&retrieved);
    let evidence_hit = qa
        .evidence
        .iter()
        .any(|e| retrieved_ids.iter().any(|r| r == e));
    let memories = memory_lines(&retrieved);
    let memories = if memories.is_empty() {
        "(no memories retrieved)".to_string()
    } else {
        memories
    };

    let answer_user = format!(
        "Memories:\n{memories}\n\nQuestion: {}\nAnswer:",
        qa.question
    );
    let predicted = match chat(cfg, ANSWER_SYSTEM_PROMPT, &answer_user, ANSWER_MAX_TOKENS).await {
        Ok(s) => s,
        Err(e) => {
            return ResultRow {
                conv: conv_idx,
                category: qa.category,
                question: qa.question.clone(),
                gold: qa.answer.as_ref().map(answer_to_string),
                predicted: String::new(),
                verdict: Verdict::Error.as_str().into(),
                judge_reason: format!("answer LLM failed: {e}"),
                retrieved_ids,
                evidence_ids: qa.evidence.clone(),
                evidence_hit,
            }
        }
    };

    let judge_user = if qa.category == 5 {
        format!(
            "Question: {}\n\
             This is an adversarial question: the information it asks about was NOT mentioned \
             in the conversation. The correct behavior is to state that the information is \
             unknown / was not mentioned. A common WRONG (hallucinated) answer is: {}\n\
             Predicted answer: {}\n\n\
             Verdict is \"correct\" if the prediction says the information is unknown or not \
             mentioned; \"incorrect\" if it fabricates an answer.",
            qa.question,
            qa.adversarial_answer
                .as_ref()
                .map(answer_to_string)
                .unwrap_or_else(|| "(unspecified)".into()),
            predicted
        )
    } else {
        format!(
            "Question: {}\nGold answer: {}\nPredicted answer: {}\n\n\
             The prediction is \"correct\" if it conveys the same information as the gold \
             answer (wording may differ); otherwise \"incorrect\".",
            qa.question,
            qa.answer
                .as_ref()
                .map(answer_to_string)
                .unwrap_or_else(|| "(missing gold)".into()),
            predicted
        )
    };
    let (verdict, reason) =
        match chat(cfg, JUDGE_SYSTEM_PROMPT, &judge_user, JUDGE_MAX_TOKENS).await {
            Ok(raw) => parse_judge_output(&raw)
                .unwrap_or((Verdict::Error, format!("unparseable judge output: {raw}"))),
            Err(e) => (Verdict::Error, format!("judge LLM failed: {e}")),
        };

    ResultRow {
        conv: conv_idx,
        category: qa.category,
        question: qa.question.clone(),
        gold: qa.answer.as_ref().map(answer_to_string),
        predicted,
        verdict: verdict.as_str().into(),
        judge_reason: reason,
        retrieved_ids,
        evidence_ids: qa.evidence.clone(),
        evidence_hit,
    }
}

async fn run(args: Args) -> Result<()> {
    let cfg = LlmConfig::from_env()?;
    eprintln!("LLM: {} @ {}", cfg.model, cfg.api_base);

    let raw = std::fs::read_to_string(&args.data)
        .with_context(|| format!("reading {}", args.data.display()))?;
    let conversations: Vec<LocomoConversation> =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", args.data.display()))?;

    let conv_indices: Vec<usize> = match args.conv {
        Some(n) => {
            if n >= conversations.len() {
                anyhow::bail!(
                    "--conv {n} out of range (dataset has {} conversations)",
                    conversations.len()
                );
            }
            vec![n]
        }
        None => (0..conversations.len()).collect(),
    };

    std::fs::create_dir_all(&args.db_dir)?;
    std::fs::create_dir_all(&args.out_dir)?;
    let run_id = Utc::now().format("%Y%m%d_%H%M%S").to_string();

    let mut overall = Acc::new();
    let mut per_cat: BTreeMap<u32, Acc> = BTreeMap::new();
    let mut ran_convs = Vec::new();

    for conv_idx in conv_indices {
        let conv = &conversations[conv_idx];
        let db_path = args.db_dir.join(format!("conv_{conv_idx}.db"));
        let store = CausalStore::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        let n = ingest_conversation(&store, conv)
            .with_context(|| format!("ingesting conversation {conv_idx}"))?;
        eprintln!("conv {conv_idx}: {n} chunks in {}", db_path.display());

        let mut qas: Vec<Qa> = conv
            .qa
            .iter()
            .filter(|q| args.categories.contains(&q.category))
            .cloned()
            .collect();
        if let Some(k) = args.limit {
            qas.truncate(k);
        }
        eprintln!("conv {conv_idx}: {} questions", qas.len());

        let done = Arc::new(AtomicUsize::new(0));
        let rows: Vec<ResultRow> = futures::stream::iter(qas.into_iter().map(|qa| {
            let cfg = cfg.clone();
            let store = store.clone();
            let done = done.clone();
            let topk = args.topk;
            async move {
                let row = answer_question(&cfg, &store, conv_idx, &qa, topk).await;
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                if d.is_multiple_of(50) {
                    eprintln!("conv {conv_idx}: {d} questions done");
                }
                row
            }
        }))
        .buffer_unordered(args.concurrency)
        .collect()
        .await;

        let jsonl_path = args
            .out_dir
            .join(format!("run_{run_id}_conv{conv_idx}.jsonl"));
        let mut out = String::new();
        for row in &rows {
            overall.add(row);
            per_cat
                .entry(row.category)
                .or_insert_with(Acc::new)
                .add(row);
            out.push_str(&serde_json::to_string(row)?);
            out.push('\n');
        }
        std::fs::write(&jsonl_path, out)?;
        eprintln!("conv {conv_idx}: wrote {}", jsonl_path.display());
        ran_convs.push(conv_idx);
    }

    let summary = Summary {
        run_id: run_id.clone(),
        date: Utc::now().to_rfc3339(),
        git_commit: git_commit(),
        model: cfg.model.clone(),
        judge_model: cfg.model.clone(),
        temperature: LLM_TEMPERATURE,
        topk: args.topk,
        data: args.data.display().to_string(),
        conversations: ran_convs,
        total_questions: overall.total,
        correct: overall.correct,
        incorrect: overall.incorrect,
        error: overall.error,
        accuracy: overall.accuracy(),
        evidence_hit_rate: if overall.total == 0 {
            0.0
        } else {
            overall.hits as f64 / overall.total as f64
        },
        per_category: per_cat
            .iter()
            .map(|(cat, a)| {
                (
                    cat.to_string(),
                    CategoryStats {
                        total: a.total,
                        correct: a.correct,
                        incorrect: a.incorrect,
                        error: a.error,
                        accuracy: a.accuracy(),
                    },
                )
            })
            .collect(),
    };
    let summary_path = args.out_dir.join(format!("run_{run_id}_summary.json"));
    let summary_json = serde_json::to_string_pretty(&summary)?;
    std::fs::write(&summary_path, &summary_json)?;
    println!("{summary_json}");
    eprintln!("wrote {}", summary_path.display());
    Ok(())
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&argv)? {
        None => {
            usage();
            Ok(())
        }
        Some(args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run(args))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (no network access)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_locomo_datetime_formats() {
        let ts = parse_session_datetime("1:56 pm on 8 May, 2023").expect("should parse");
        assert_eq!(format_ts(ts), "2023-05-08 13:56");

        let ts2 = parse_session_datetime("11:02 am on 25 May, 2023").expect("should parse");
        assert_eq!(format_ts(ts2), "2023-05-25 11:02");

        // Zero-padded day also accepted.
        let ts3 = parse_session_datetime("9:05 AM on 08 May, 2023").expect("should parse");
        assert_eq!(format_ts(ts3), "2023-05-08 09:05");

        assert!(parse_session_datetime("not a date").is_none());
    }

    #[test]
    fn synthetic_fallback_preserves_order() {
        let mk = |n: u32, dt: Option<&str>| Session {
            number: n,
            date_time_raw: dt.map(|s| s.to_string()),
            turns: vec![],
        };
        let s1 = mk(1, None);
        let s2 = mk(2, Some("garbage"));
        assert!(session_base_time(&s2) > session_base_time(&s1));
        assert_eq!(
            session_base_time(&s2) - session_base_time(&s1),
            86_400,
            "sessions spaced one day apart"
        );
    }

    fn tiny_conversation() -> LocomoConversation {
        let raw = r#"{
            "conversation": {
                "session_1": [
                    {"speaker": "Alice", "dia_id": "D1:1", "text": "I went hiking yesterday."},
                    {"speaker": "Bob", "dia_id": "D1:2", "text": "Nice, where?"},
                    {"speaker": "Alice", "dia_id": "D1:3", "text": "Up in the mountains."}
                ],
                "session_1_date_time": "1:56 pm on 8 May, 2023",
                "session_2": [
                    {"speaker": "Bob", "dia_id": "D2:1", "text": "Did you train for the race?"},
                    {"speaker": "Alice", "dia_id": "D2:2", "text": "Yes, every morning."},
                    {"speaker": "Bob", "dia_id": "D2:3", "text": "Impressive!"}
                ],
                "session_2_date_time": "3:00 pm on 9 May, 2023"
            },
            "qa": [
                {"question": "Where did Alice hike?", "answer": "mountains",
                 "evidence": ["D1:3"], "category": 1}
            ]
        }"#;
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn ingest_writes_chunks_edges_and_times() {
        let conv = tiny_conversation();
        let store = CausalStore::open_in_memory().unwrap();

        let n = ingest_conversation(&store, &conv).unwrap();
        assert_eq!(n, 6);

        // Chunk count and dia_id linkage.
        let count: i64 = store
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 6);

        // Edges: turns 2..6 each link back to the previous opposite-speaker
        // turn (first turn of each session has none): 2 + 2 = 4 edges.
        let edges = store.all_valid_edges().unwrap();
        assert_eq!(edges.len(), 4);
        let endpoints: HashSet<&str> = edges
            .iter()
            .flat_map(|e| [e.decision_id.as_str(), e.outcome_id.as_str()])
            .collect();
        assert!(endpoints.contains("D1:1"));
        assert!(endpoints.contains("D2:3"));
        // First turn of a session is never an edge target... actually D1:1
        // IS a source for D1:2's edge; check D2:1 links from D1:3? No —
        // prev_other resets per session, so D2:1 (Bob) has no prior Bob turn
        // in session 2 and gets no incoming edge.
        assert!(edges.iter().all(|e| e.outcome_id != "D1:1"));
        assert!(edges.iter().all(|e| e.outcome_id != "D2:1"));

        // event_time strictly increasing across sessions (session 2 > session 1).
        let t_d1: i64 = store
            .with_conn(|c| {
                Ok(
                    c.query_row("SELECT created_at FROM chunks WHERE id = 'D1:3'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        let t_d2: i64 = store
            .with_conn(|c| {
                Ok(
                    c.query_row("SELECT created_at FROM chunks WHERE id = 'D2:1'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert!(t_d2 > t_d1, "session 2 turns must be later than session 1");

        // Intra-session ordering: +1s per turn.
        let t_d11: i64 = store
            .with_conn(|c| {
                Ok(
                    c.query_row("SELECT created_at FROM chunks WHERE id = 'D1:1'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(t_d1 - t_d11, 2);

        // Chunk text format.
        let text: String = store
            .with_conn(|c| {
                Ok(
                    c.query_row("SELECT text FROM chunks WHERE id = 'D1:2'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(text, "[session_1 2023-05-08 13:56] Bob: Nice, where?");

        // Idempotent: second run skips.
        let n2 = ingest_conversation(&store, &conv).unwrap();
        assert_eq!(n2, 6);
        assert_eq!(store.all_valid_edges().unwrap().len(), 4);
    }

    #[test]
    fn judge_output_parsing_tolerates_mess() {
        let (v, r) =
            parse_judge_output(r#"{"verdict": "correct", "reason": "same info"}"#).unwrap();
        assert_eq!(v, Verdict::Correct);
        assert_eq!(r, "same info");

        let (v, _) =
            parse_judge_output("```json\n{\"verdict\": \"incorrect\", \"reason\": \"wrong\"}\n```")
                .unwrap();
        assert_eq!(v, Verdict::Incorrect);

        let (v, _) = parse_judge_output(
            "Sure! Here is my judgment:\n{\"verdict\": \"correct\"}\nHope that helps.",
        )
        .unwrap();
        assert_eq!(v, Verdict::Correct);

        assert!(parse_judge_output("no json here").is_none());
        assert!(parse_judge_output(r#"{"verdict": "maybe"}"#).is_none());
    }

    #[test]
    fn numeric_answers_stringify() {
        let v = serde_json::json!(2022);
        assert_eq!(answer_to_string(&v), "2022");
        let v = serde_json::json!("7 May 2023");
        assert_eq!(answer_to_string(&v), "7 May 2023");
    }

    #[test]
    fn retrieve_uses_bm25_word_order_invariant() {
        // Regression for the LoCoMo failure mode: the same content words in a
        // different order must still retrieve the edge (LIKE could not).
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision_at(
                "[session_1 2023-05-08 13:56] caroline: I went to the LGBTQ support group",
                "[session_1 2023-05-08 13:56] melanie: that is wonderful",
                "caused",
                None,
                0.4,
                "temporal",
                1000,
            )
            .unwrap();
        let res = retrieve(
            &store,
            "When did Caroline go to the LGBTQ support group?",
            10,
        )
        .unwrap();
        assert_eq!(res.len(), 1, "BM25 must rank the evidence edge");
        assert!(res[0].decision_text.contains("support group"));
    }
}
