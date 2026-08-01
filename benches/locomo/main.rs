//! LoCoMo benchmark harness for causal-memory.
//!
//! Ingests LoCoMo conversations (locomo10.json) into per-conversation
//! causal-memory SQLite DBs, then answers and judges the QA set with an
//! OpenAI-compatible LLM (DeepSeek by default), following the LoCoMo
//! evaluation protocol (answer + judge, per-category accuracy).
//!
//! Subcommands:
//!   causal-memory-locomo run --data benches/locomo/data/locomo10.json [options]
//!   causal-memory-locomo compact --data benches/locomo/data/locomo10.json [options]
//!
//! `compact` is the "compressed LoCoMo" experiment (see compact.rs): text-only
//! memory after k iterative compactions (condition A) vs the same compressed
//! text plus causal edges over the ORIGINAL uncompressed turns (condition B).
//!
//! Env:
//!   DEEPSEEK_API_KEY        (required; or CAUSAL_MEMORY_LLM_KEY)
//!   LOCOMO_LLM_API          (default: https://api.deepseek.com/v1)
//!   LOCOMO_LLM_MODEL        (default: deepseek-chat, used for answer + judge)

mod compact;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use causal_memory::distill::{Distiller, ItemKind};
use causal_memory::embed::{cosine_similarity, blob_to_vec, EmbedConfig, Embedder};
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
const ANSWER_MAX_TOKENS_V2: u32 = 800; // 7-step reasoning needs budget; final answer still short
const JUDGE_MAX_TOKENS: u32 = 200;
const LLM_TEMPERATURE: f32 = 0.0;
const LLM_RETRIES: usize = 3;

const ANSWER_SYSTEM_PROMPT: &str = r#"You are answering questions about a conversation between two people, using memory snippets retrieved from that conversation.

Rules:
- Base your answer ONLY on the memories provided below.
- Keep the answer short: a few words or one sentence.
- Each memory is prefixed with the session date, e.g. "[session_3 2023-05-08 13:56]". When the question asks WHEN something happened, resolve relative time expressions ("yesterday", "last week", "next month", "last year") against that date and answer with an ABSOLUTE date or time period (e.g. "7 May 2023", "June 2023"), not the relative expression.
- When a memory DIRECTLY addresses the question, you MUST answer — a short partial answer grounded in a memory is always better than a refusal. Refuse ONLY when no memory states the requested fact: if the memories merely discuss the same person/object/topic without stating the answer, respond that the information was not mentioned in the conversation. Never infer, generalize, or guess specific details (meanings, inspirations, reasons, feelings) that are not explicitly stated."#;

// V1 prompt renamed for adversarial (cat5) questions — must keep refusal ability.
const ANSWER_SYSTEM_PROMPT_ADVERSARIAL: &str = ANSWER_SYSTEM_PROMPT;

// E1: V2 prompt — 7-step reasoning, ported from mem0's ANSWER_GENERATION_PROMPT.
// Only for cat1-4 (factual QA); cat5 uses ADVERSARIAL to preserve abstention.
const ANSWER_SYSTEM_PROMPT_V2: &str = r#"You are answering a question using retrieved memories from past conversations between two people. Follow these reasoning steps IN ORDER.

## Step 1: SCAN ALL MEMORIES
Read EVERY memory from first to last. Do NOT stop after finding the first relevant memory — important details are often scattered across the whole list, including near the end. Give equal weight to ALL memories regardless of position.

## Step 2: ENTITY VERIFICATION
Confirm each relevant memory is about the correct person/entity. If the question asks about Person A and a memory attributes something to Person B, do not use it for A — unless B is the other speaker in the same conversation, in which case it is still valid shared evidence, but check the attribution.

## Step 3: COMBINE AND CROSS-REFERENCE
- COMBINE facts from multiple memories about the same topic.
- For listing/counting questions, extract EVERY distinct item from ALL memories, then re-scan specifically for each category of answer.
- For counting questions ("how many times"), enumerate each distinct instance explicitly with its date or context BEFORE giving a final count. Do not estimate — list, then count.
- DECOMPOSE complex sentences: "an immersive X with Y" contains multiple distinct facts.

## Step 4: SELECT THE BEST ANSWER
- ALWAYS choose the MOST SPECIFIC detail available. A proper name, title, or number beats a generic description.
- Report what someone actually DID, not what was offered or available. "Has not tried X yet" means X was NOT done.
- Repetition of a generic fact across memories does NOT make it more correct than one memory with a more specific answer.

## Step 5: TEMPORAL GROUNDING
- Resolve all relative time expressions ("yesterday", "last week", "last year") against the date attached to each memory, and answer with an ABSOLUTE date or period (e.g. "7 May 2023", "June 2023").
- For "how long" questions, find explicit start and end dates, then compute. Do not guess.
- When MULTIPLE instances of similar events exist at different dates, enumerate them with dates before picking: past tense + "the" → the instance closest to (before) the conversation's latest date; future tense → the earliest planned date.

## Step 6: INCLUSION CHECK (for lists and counts)
If you found items you are tempted to exclude — STOP. Include them unless you have STRONG evidence they are wrong. The most common mistake is dropping relevant items through overly strict filtering. More items is better than fewer when there is supporting evidence.

## Step 7: COMMIT AND ANSWER
Give a direct, specific answer after "ANSWER:". NEVER say "not specified", "not mentioned", or "the memories don't say" when ANY memory contains relevant information. No hedging.
- NEVER invent specific names, titles, places, or dates that do not appear in any memory. If no memory contains the requested detail, answer with what the memories DO contain.
- Keep the final answer short: a few words or one or two sentences."#;

const JUDGE_SYSTEM_PROMPT: &str = r#"You are an impartial judge evaluating whether a predicted answer correctly answers a question about a conversation.

Respond with ONLY a JSON object (no markdown, no extra text):
{"verdict": "correct" or "incorrect", "reason": "<one sentence>"}"#;

// E3: mem0-compatible judge — much more lenient than strict.
// Used to quantify the "judge caliber tax": how much of mem0's 91.6% is
// judge looseness vs genuine recall quality. Report as "J-score (mem0)".
const JUDGE_SYSTEM_PROMPT_MEM0: &str = r#"You are evaluating conversational AI memory recall. Label the predicted answer as CORRECT or WRONG.

Rules:
1. PARTIAL CREDIT: If the predicted answer includes AT LEAST ONE correct item from the gold answer's list, mark CORRECT. Only mark WRONG if NONE of the gold items appear.
2. PARAPHRASES COUNT: Same concept in different words is CORRECT. Emotions in the same positive/negative family count as paraphrases.
3. EXTRA DETAIL IS FINE: A longer answer that includes the gold answer's key facts plus more is CORRECT. Never penalize detail.
4. DATE TOLERANCE: Dates within 14 days are CORRECT. Durations within 50% are CORRECT. Converting relative dates to the correct absolute date is CORRECT.
5. SEMANTIC OVERLAP: Judge whether the answer addresses the same topic and captures the core idea. Different wording or specificity should not cause WRONG.
6. SAME REFERENT: If the answer references the same core entity/concept as the gold answer, mark CORRECT even with different descriptions.

ONLY mark WRONG if: the answer contains ZERO correct items from the gold answer, or addresses a completely different topic.

Respond with ONLY a JSON object: {"verdict": "correct" or "incorrect", "reason": "<one sentence>"}"#;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum JudgeStyle {
    Strict,
    Mem0,
}

impl JudgeStyle {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            JudgeStyle::Strict => "strict",
            JudgeStyle::Mem0 => "mem0",
        }
    }
    pub(crate) fn system_prompt(&self) -> &'static str {
        match self {
            JudgeStyle::Strict => JUDGE_SYSTEM_PROMPT,
            JudgeStyle::Mem0 => JUDGE_SYSTEM_PROMPT_MEM0,
        }
    }
}

fn usage() {
    eprintln!("Usage: causal-memory-locomo run --data <locomo10.json> [options]");
    eprintln!("       causal-memory-locomo compact --data <locomo10.json> [options]");
    eprintln!();
    eprintln!("run options:");
    eprintln!("  --data PATH         LoCoMo dataset JSON (required)");
    eprintln!("  --conv N            run only conversation index N");
    eprintln!("  --all               run all conversations (default)");
    eprintln!("  --limit K           max questions per conversation (cost guard)");
    eprintln!("  --categories LIST   comma-separated categories, e.g. 1,2,3,4,5 (default: all)");
    eprintln!("  --db-dir DIR        per-conversation DBs (default: benches/locomo/db)");
    eprintln!("  --out DIR           results dir (default: benches/locomo/results)");
    eprintln!("  --topk N            retrieved memories per question (default: 10)");
    eprintln!("  --concurrency N     parallel questions (default: 8)");
    eprintln!("  --ingest MODE       raw (default) | distill (raw + LLM-distilled facts/");
    eprintln!("                      episodes on top; separate *_distill.db)");
    eprintln!("  --ingest-only       ingest (+ distill) and exit; skip QA");
    eprintln!("  --prompt-version VER  v1 (legacy) | v2 (7-step reasoning, default)");
    eprintln!("  --judge-style STYLE  strict (default) | mem0 (lenient: partial credit, ±14d dates)");
    eprintln!("  rejudge --input results/<run>.jsonl --judge-style mem0  (re-judge without re-answering)");
    eprintln!();
    eprintln!("compact options: --data PATH (required) [--conv N] [--compact K] (default 5)");
    eprintln!("  [--limit Q] [--concurrency N] [--out DIR] (default benches/locomo/results)");
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

/// Chunk text for a single turn: "[session_N <date>] speaker: text". Shared by
/// the plain ingest path and the compact experiment's uncompressed edges.
fn turn_chunk_text(session_number: u32, ts: i64, speaker: &str, text: &str) -> String {
    format!(
        "[session_{} {}] {}: {}",
        session_number,
        format_ts(ts),
        speaker,
        text
    )
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
        store.with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM chunks WHERE id GLOB 'D[0-9]*'", [], |r| r.get(0))?))?;
    // Count only raw turn chunks (D-prefixed dia_ids from the dataset).
    // record_decision / record_distilled create extra d{id}/o{id} chunks
    // as causal-edge endpoints — those are expected and must not trigger
    // a re-ingest cycle.
    if existing == expected_chunks as i64 && expected_chunks > 0 {
        return Ok(existing as usize);
    }
    if existing > 0 {
        eprintln!(
            "warn: DB has {existing} D-prefixed chunks, expected {expected_chunks}; re-ingesting from scratch"
        );
        store.with_conn(|c| {
            // Re-ingest: delete all edges first (FK constraint requires edges
            // gone before chunks), then chunks. Distill edges are lost and
            // will be re-created by the distill pass (idempotent via upsert).
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
            let chunk_text = turn_chunk_text(session.number, ts, &turn.speaker, &turn.text);
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
fn retrieve(store: &CausalStore, question: &str, topk: usize, query_vec: Option<&[f32]>) -> Result<Vec<CausalEntry>> {
    let bm25_results = store.search_causal_bm25(None, question, topk)?;

    // Semantic + BM25 RRF fusion (query_vec pre-computed in async context)
    if let Some(qv) = query_vec {
        let semantic = semantic_search(store, qv, topk * 2).unwrap_or_default();
        if !semantic.is_empty() {
            return Ok(rrf_merge(&bm25_results, &semantic, topk));
        }
    }

    Ok(bm25_results)
}

/// Brute-force cosine search over edge_embeddings (mirrors store::search_causal_semantic
/// but works from the harness without going through the MCP server).
fn semantic_search(store: &CausalStore, query_vec: &[f32], limit: usize) -> Result<Vec<CausalEntry>> {
    store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT ee.edge_id, ee.vector, cf.text, ct.text, e.relation, e.confidence, e.task_tag
             FROM edge_embeddings ee
             JOIN causal_edges e ON e.id = ee.edge_id
             JOIN chunks cf ON cf.id = e.from_id
             JOIN chunks ct ON ct.id = e.to_id
             WHERE e.valid_to IS NULL"
        )?;
        let mut scored: Vec<(CausalEntry, f64)> = Vec::new();
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let row_data: Vec<(i64, Vec<u8>, String, String, String, f64, Option<String>)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (edge_id, blob, dec_text, out_text, relation, confidence, task_tag) in row_data {
            if let Ok(vec) = blob_to_vec(&blob) {
                let sim = cosine_similarity(query_vec, &vec);
                scored.push((CausalEntry {
                    edge_id,
                    decision_id: String::new(),
                    decision_text: dec_text,
                    outcome_id: String::new(),
                    outcome_text: out_text,
                    relation,
                    confidence,
                    task_tag,
                    event_time: 0,
                    valid_to: None,
                    access_count: 0,
                    last_accessed_at: None,
                    discovered_by: String::new(),
                    discovered_at: 0,
                    outcome_polarity: None,
                }, sim));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(e, _)| e).collect())
    })
}

/// RRF merge of two ranked lists (same logic as the MCP server's search_memory).
fn rrf_merge(bm25: &[CausalEntry], semantic: &[CausalEntry], limit: usize) -> Vec<CausalEntry> {
    use std::collections::HashMap;
    let k = 60.0;
    let mut scores: HashMap<i64, (f64, usize)> = HashMap::new(); // edge_id → (rrf_score, bm25_index)

    // Score from BM25 list
    for (i, entry) in bm25.iter().enumerate() {
        let s = 1.0 / (k + i as f64 + 1.0);
        scores.insert(entry.edge_id, (s, i));
    }
    // Score from semantic list
    for (i, entry) in semantic.iter().enumerate() {
        let s = 1.0 / (k + i as f64 + 1.0);
        match scores.get_mut(&entry.edge_id) {
            Some((acc, _)) => *acc += s,
            None => { scores.insert(entry.edge_id, (s, usize::MAX)); } // not in BM25
        }
    }

    // Build merged result: sort by score desc, take top-k
    let mut ranked: Vec<(i64, f64)> = scores.into_iter().map(|(id, (s, _))| (id, s)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Materialize: prefer BM25 entry (has full chunk ids), fall back to semantic
    let mut result = Vec::new();
    for (edge_id, _) in ranked.into_iter().take(limit) {
        if let Some(entry) = bm25.iter().find(|e| e.edge_id == edge_id) {
            result.push(entry.clone());
        } else if let Some(entry) = semantic.iter().find(|e| e.edge_id == edge_id) {
            result.push(entry.clone());
        }
    }
    result
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
    /// Reasoning models (deepseek-v4-pro, o1-style) put their chain-of-thought
    /// here and sometimes leave `content` empty. Fall back to this when content
    /// is blank so we don't lose the answer.
    #[serde(default)]
    reasoning_content: String,
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
        .map(|c| {
            let content = c.message.content.trim();
            if content.is_empty() {
                // Reasoning model put everything in reasoning_content
                c.message.reasoning_content.trim().to_string()
            } else {
                content.to_string()
            }
        })
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
// Distill-mode ingest
// ---------------------------------------------------------------------------

/// Ingest mode: `raw` (turn chunks + temporal edges only, the frozen-protocol
/// baseline) or `distill` (raw PLUS an LLM distillation pass: facts → the
/// fact layer, lessons/events → distilled causal edges). Distill runs use
/// separate `*_distill.db` files so the raw baseline DBs stay intact.
#[derive(Debug, Clone, Copy, PartialEq)]
enum IngestMode {
    Raw,
    Distill,
}

impl IngestMode {
    fn as_str(&self) -> &'static str {
        match self {
            IngestMode::Raw => "raw",
            IngestMode::Distill => "distill",
        }
    }
}

/// Statistics of one conversation's distillation pass (auditable in the run
/// summary, same discipline as the Memora harness).
#[derive(Debug, Default, Clone, Serialize)]
struct DistillStats {
    sessions: usize,
    llm_calls: usize,
    facts_recorded: usize,
    episodes_recorded: usize,
    episodes_duplicate: usize,
    facts_retired: usize,
    superseded_invalidations: usize,
    /// True when a pre-existing distill pass was detected and skipped.
    skipped_existing: bool,
}

/// Session date as "YYYY-MM-DD" for the distiller (the raw LoCoMo
/// `session_N_date_time` parses to a full timestamp; distill items carry
/// only the date).
fn session_date_str(session: &Session) -> String {
    let base = session_base_time(session);
    DateTime::from_timestamp(base, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Distill every session of the conversation into the store.
///
/// Bounded-concurrency LLM calls, recorded strictly in session order so a
/// later session's `supersedes` always sees the earlier items. Routing:
/// Fact/Preference → the fact layer (`record_fact`, scope "user" — the DB is
/// already per-conversation) with supersedes-driven retirement via
/// `retire_facts_by_hint`; Lesson/Event → `record_distilled` (its own
/// supersedes machinery soft-invalidates outdated edges). Raw turn chunks
/// are always ingested as well (dual write): LoCoMo conversations are all
/// detail-heavy, and the evidence-id protocol needs the raw chunks.
///
/// Idempotent at the conversation level: pre-existing distill edges skip the
/// pass entirely (item-level idempotency lives in record_distilled /
/// record_fact's upsert).
async fn distill_conversation(
    store: &CausalStore,
    distiller: Option<&Distiller>,
    conv: &LocomoConversation,
    concurrency: usize,
) -> Result<DistillStats> {
    let sessions = conv.sessions()?;
    let mut stats = DistillStats {
        sessions: sessions.len(),
        ..Default::default()
    };

    let existing: i64 = store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM causal_edges WHERE discovered_by = 'distill'",
            [],
            |r| r.get(0),
        )?)
    })?;
    if existing > 0 {
        stats.skipped_existing = true;
        return Ok(stats);
    }

    let Some(distiller) = distiller else {
        eprintln!("warn: no Distiller configured (DEEPSEEK_API_KEY unset); skipping distill pass");
        return Ok(stats);
    };

    let futures = sessions.iter().map(|session| {
        let turns: Vec<(String, String)> = session
            .turns
            .iter()
            .map(|t| (t.speaker.clone(), t.text.clone()))
            .collect();
        let date = session_date_str(session);
        async move { distiller.distill_session(&date, &turns).await }
    });
    let results: Vec<Result<Vec<causal_memory::distill::MemoryItem>>> =
        futures::stream::iter(futures)
            .buffered(concurrency)
            .collect()
            .await;
    stats.llm_calls = results.len();

    // Record strictly in session order.
    for (session, result) in sessions.iter().zip(results) {
        let items = match result {
            Ok(items) => items,
            Err(e) => {
                // Raw chunks for this session are already ingested, so no
                // data is lost; the pass continues with the next session.
                eprintln!(
                    "warn: distill failed for session {} ({e}); raw chunks already cover it",
                    session.number
                );
                continue;
            }
        };
        for item in &items {
            match item.kind {
                ItemKind::Fact | ItemKind::Preference => {
                    let kind = match item.kind {
                        ItemKind::Fact => "fact",
                        ItemKind::Preference => "preference",
                        _ => unreachable!(),
                    };
                    // Retire BEFORE record: the new value often shares topic
                    // tokens with its own supersedes hint and
                    // retire_facts_by_hint has no self-exclusion — recording
                    // first can retire the fact we just wrote (found by
                    // review). The 20260730 full distill run predates this
                    // fix; any self-retires there remove facts, so the
                    // reported +5.4pp is, if anything, conservative.
                    if let Some(hint) = item.supersedes.as_deref() {
                        match store.retire_facts_by_hint(kind, "user", hint) {
                            Ok(n) => stats.facts_retired += n,
                            Err(e) => eprintln!(
                                "warn: retire_facts_by_hint failed ({e}); stale fact may stay live"
                            ),
                        }
                    }
                    store.record_fact(kind, &item.text, "user", "distill", 0.8)?;
                    stats.facts_recorded += 1;
                }
                ItemKind::Lesson | ItemKind::Event => {
                    let out = store.record_distilled(item, None)?;
                    if out.duplicate {
                        stats.episodes_duplicate += 1;
                    } else {
                        stats.episodes_recorded += 1;
                    }
                    stats.superseded_invalidations += out.invalidated_edge_ids.len();
                }
            }
        }
    }
    Ok(stats)
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
    ingest: IngestMode,
    /// Ingest (+ distill) only; skip the QA phase. Used to warm the
    /// per-conversation distill DBs in cheap chunks before one full QA run.
    ingest_only: bool,
    /// E1: answer prompt version. v1 = legacy one-paragraph; v2 = 7-step
    /// reasoning (mem0-aligned). Default v2.
    prompt_version: PromptVersion,
    /// E3: judge style. strict = exact match; mem0 = lenient (partial credit,
    /// date tolerance ±14d). Default strict.
    judge_style: JudgeStyle,
    /// E2: top-k cutoffs for retrieval budget experiment (e.g. [10, 20, 50]).
    /// When non-empty, one retrieval at max(cutoffs) is sliced to each cutoff
    /// and answered+judged independently. Mutually exclusive with plain --topk.
    topk_cutoffs: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PromptVersion {
    V1,
    V2,
}

impl PromptVersion {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            PromptVersion::V1 => "v1",
            PromptVersion::V2 => "v2",
        }
    }
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
    let mut ingest = IngestMode::Raw;
    let mut ingest_only = false;
    let mut prompt_version = PromptVersion::V2;
    let mut judge_style = JudgeStyle::Strict;
    let mut topk_cutoffs: Vec<usize> = Vec::new();

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
            "--ingest" => {
                ingest = match take(&mut i, "--ingest")?.as_str() {
                    "raw" => IngestMode::Raw,
                    "distill" => IngestMode::Distill,
                    other => anyhow::bail!("bad --ingest {other:?}; expected raw|distill"),
                }
            }
            "--ingest-only" => ingest_only = true,
            "--prompt-version" => {
                prompt_version = match take(&mut i, "--prompt-version")?.as_str() {
                    "v1" => PromptVersion::V1,
                    "v2" => PromptVersion::V2,
                    other => anyhow::bail!("bad --prompt-version {other:?}; expected v1|v2"),
                }
            }
            "--judge-style" => {
                judge_style = match take(&mut i, "--judge-style")?.as_str() {
                    "strict" => JudgeStyle::Strict,
                    "mem0" => JudgeStyle::Mem0,
                    other => anyhow::bail!("bad --judge-style {other:?}; expected strict|mem0"),
                }
            }
            "--topk-cutoffs" => {
                let raw = take(&mut i, "--topk-cutoffs")?;
                topk_cutoffs = raw
                    .split(',')
                    .map(|s| s.trim().parse::<usize>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| anyhow!("bad --topk-cutoffs: {e}"))?;
                if topk_cutoffs.len() < 2 {
                    anyhow::bail!("--topk-cutoffs needs at least 2 values, e.g. 10,20,50");
                }
            }
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
        ingest,
        ingest_only,
        prompt_version,
        judge_style,
        topk_cutoffs,
    }))
}

#[derive(Serialize, Deserialize)]
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
    /// Ingest mode of this run ("raw" | "distill"); distill runs also query
    /// the fact layer in the answer prompt (documented deviation from the
    /// causal-only frozen protocol — that comparison is the point).
    ingest: String,
    /// E1: answer prompt version ("v1" | "v2").
    prompt_version: String,
    /// E3: judge style ("strict" | "mem0").
    judge_style: String,
    /// Aggregated distillation-pass statistics (distill mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    distill_ingest: Option<DistillStats>,
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
    with_facts: bool,
    prompt_version: PromptVersion,
    judge_style: JudgeStyle,
) -> ResultRow {
    // Pre-compute query embedding in async context (avoids block_in_place deadlock)
    let query_vec = if let Some(config) = EmbedConfig::from_env() {
        let embedder = Embedder::new(config);
        embedder.embed(&qa.question).await.ok()
    } else {
        None
    };
    let mut retrieved = retrieve(store, &qa.question, topk, query_vec.as_deref()).unwrap_or_default();
    let retrieved_ids = retrieved_chunk_ids(&retrieved);
    let evidence_hit = qa
        .evidence
        .iter()
        .any(|e| retrieved_ids.iter().any(|r| r == e));

    // E1: V2 presents memories in chronological order (event_time ascending)
    // to prevent lost-in-the-middle and narrative drift. Facts stay first.
    // F4 fix: only sort for cat1-4 (factual QA). cat5 (adversarial) uses V1
    // prompt and should also get V1's BM25-ranked memory order — sorting
    // by time is a variable leak that contributed to cat5's -3.6pp regression.
    if prompt_version == PromptVersion::V2 && qa.category != 5 {
        retrieved.sort_by_key(|e| e.event_time);
    }

    // Distill mode additionally queries the fact layer (BM25, same topk) and
    // puts fact lines FIRST: they are the high-precision layer for the
    // factual QA slice the causal-only baseline conceded. Evidence-hit stays
    // computed from causal entries only (facts carry no dia_ids) — protocol
    // unchanged.
    let memories = if with_facts {
        let facts = store
            .search_facts_bm25(&qa.question, None, topk)
            .unwrap_or_default();
        let mut lines: Vec<String> = facts.iter().map(|f| format!("- {}", f.value)).collect();
        let causal = memory_lines(&retrieved);
        if !causal.is_empty() {
            lines.push(causal);
        }
        lines.join("\n")
    } else {
        memory_lines(&retrieved)
    };
    let memories = if memories.is_empty() {
        "(no memories retrieved)".to_string()
    } else {
        memories
    };

    // E1: select prompt by version and question category.
    // cat5 (adversarial) always uses the abstention-capable prompt.
    let (system_prompt, max_tokens) = if qa.category == 5 {
        (ANSWER_SYSTEM_PROMPT_ADVERSARIAL, ANSWER_MAX_TOKENS)
    } else {
        match prompt_version {
            PromptVersion::V1 => (ANSWER_SYSTEM_PROMPT, ANSWER_MAX_TOKENS),
            PromptVersion::V2 => (ANSWER_SYSTEM_PROMPT_V2, ANSWER_MAX_TOKENS_V2),
        }
    };

    let answer_user = format!(
        "Memories:\n{memories}\n\nQuestion: {}\nAnswer:",
        qa.question
    );
    let raw_predicted = match chat(cfg, system_prompt, &answer_user, max_tokens).await {
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

    // E1: V2 prompt outputs "ANSWER: <final answer>" after reasoning steps.
    // Extract only the part after the last "ANSWER:" marker (mem0 run.py:468).
    let predicted = if prompt_version == PromptVersion::V2 && qa.category != 5 {
        raw_predicted
            .rsplit("ANSWER:")
            .next()
            .unwrap_or(&raw_predicted)
            .trim()
            .to_string()
    } else {
        raw_predicted
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
        match chat(cfg, judge_style.system_prompt(), &judge_user, JUDGE_MAX_TOKENS).await {
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

/// Answer + judge a batch of questions against one store, in parallel.
/// Shared by `run` and the compact experiment's QA phase.
#[allow(clippy::too_many_arguments, reason = "benchmark harness; params are independent")]
pub(crate) async fn answer_all(
    cfg: &LlmConfig,
    store: &CausalStore,
    conv_idx: usize,
    qas: Vec<Qa>,
    topk: usize,
    concurrency: usize,
    with_facts: bool,
    prompt_version: PromptVersion,
    judge_style: JudgeStyle,
) -> Vec<ResultRow> {
    let done = Arc::new(AtomicUsize::new(0));
    futures::stream::iter(qas.into_iter().map(|qa| {
        let cfg = cfg.clone();
        let store = store.clone();
        let done = done.clone();
        async move {
            let row = answer_question(&cfg, &store, conv_idx, &qa, topk, with_facts, prompt_version, judge_style).await;
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d.is_multiple_of(50) {
                eprintln!("conv {conv_idx}: {d} questions done");
            }
            row
        }
    }))
    .buffer_unordered(concurrency)
    .collect()
    .await
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
    let mut distill_totals = DistillStats::default();

    for conv_idx in conv_indices {
        let conv = &conversations[conv_idx];
        // Distill mode uses separate DB files so the raw baseline DBs (and
        // their frozen-protocol results) stay intact and reproducible.
        let db_path = match args.ingest {
            IngestMode::Raw => args.db_dir.join(format!("conv_{conv_idx}.db")),
            IngestMode::Distill => args.db_dir.join(format!("conv_{conv_idx}_distill.db")),
        };
        let store = CausalStore::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        let n = ingest_conversation(&store, conv)
            .with_context(|| format!("ingesting conversation {conv_idx}"))?;
        eprintln!("conv {conv_idx}: {n} chunks in {}", db_path.display());

        let mut distill_stats = None;
        if args.ingest == IngestMode::Distill {
            let distiller = Distiller::from_env();
            let stats = distill_conversation(&store, distiller.as_ref(), conv, args.concurrency)
                .await
                .with_context(|| format!("distilling conversation {conv_idx}"))?;
            eprintln!(
                "conv {conv_idx}: distill — {} sessions, {} facts ({} retired), {} episodes{}{}",
                stats.sessions,
                stats.facts_recorded,
                stats.facts_retired,
                stats.episodes_recorded,
                if stats.skipped_existing {
                    " (skipped: existing)"
                } else {
                    ""
                },
                if stats.superseded_invalidations > 0 {
                    format!(", {} superseded", stats.superseded_invalidations)
                } else {
                    String::new()
                },
            );
            distill_stats = Some(stats);
        }

        if args.ingest_only {
            eprintln!("conv {conv_idx}: --ingest-only, skipping QA");
            continue;
        }

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

        let with_facts = args.ingest == IngestMode::Distill;
        let rows: Vec<ResultRow> = answer_all(
            &cfg,
            &store,
            conv_idx,
            qas,
            args.topk,
            args.concurrency,
            with_facts,
            args.prompt_version,
            args.judge_style,
        )
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
        if let Some(s) = distill_stats {
            distill_totals.sessions += s.sessions;
            distill_totals.llm_calls += s.llm_calls;
            distill_totals.facts_recorded += s.facts_recorded;
            distill_totals.episodes_recorded += s.episodes_recorded;
            distill_totals.episodes_duplicate += s.episodes_duplicate;
            distill_totals.facts_retired += s.facts_retired;
            distill_totals.superseded_invalidations += s.superseded_invalidations;
        }
    }

    if args.ingest_only {
        eprintln!("--ingest-only: all conversations ingested, no QA run");
        return Ok(());
    }

    let summary = Summary {
        run_id: run_id.clone(),
        date: Utc::now().to_rfc3339(),
        git_commit: git_commit(),
        model: cfg.model.clone(),
        judge_model: cfg.model.clone(),
        temperature: LLM_TEMPERATURE,
        topk: args.topk,
        ingest: args.ingest.as_str().to_string(),
        prompt_version: args.prompt_version.as_str().to_string(),
        judge_style: args.judge_style.as_str().to_string(),
        distill_ingest: (args.ingest == IngestMode::Distill).then_some(distill_totals),
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

/// E3: re-judge existing results with a different judge style (no re-answering).
/// Reads a JSONL results file, re-runs only the judge LLM call per row with
/// the specified style, writes a new JSONL + summary. Cost: ~1 judge call/q.
/// E3/F1: re-judge existing results with a different judge style.
/// Accepts a DIRECTORY (processes all *.jsonl, excluding *_rejudged_* files)
/// or a single .jsonl file. Output goes to a `rejudged_<style>/` subdirectory.
async fn rejudge(argv: &[String]) -> Result<()> {
    let mut input: Option<PathBuf> = None;
    let mut judge_style = JudgeStyle::Mem0;
    let mut i = 0;
    let take = |i: &mut usize, flag: &str| -> Result<String> {
        *i += 1;
        argv.get(*i)
            .cloned()
            .ok_or_else(|| anyhow!("missing value for {flag}"))
    };
    while i < argv.len() {
        match argv[i].as_str() {
            "--input" => input = Some(PathBuf::from(take(&mut i, "--input")?)),
            "--judge-style" => {
                judge_style = match take(&mut i, "--judge-style")?.as_str() {
                    "strict" => JudgeStyle::Strict,
                    "mem0" => JudgeStyle::Mem0,
                    other => anyhow::bail!("bad --judge-style {other:?}"),
                }
            }
            "--help" | "-h" => {
                eprintln!("Usage: causal-memory-locomo rejudge --input <dir-or-file> [--judge-style strict|mem0]");
                eprintln!("  If --input is a directory, all *.jsonl files are processed (excluding *_rejudged_*).");
                eprintln!("  Output goes to <input>/rejudged_<style>/");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    let input_path = input.ok_or_else(|| anyhow!("--input is required (path to a .jsonl file or results directory)"))?;

    // Collect input files: directory → all *.jsonl excluding _rejudged_; file → just that file.
    let input_files: Vec<PathBuf> = if input_path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&input_path)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|ext| ext == "jsonl")
                    // F1 bug fix: exclude any file that is itself a rejudged output
                    && !p.to_string_lossy().contains("_rejudged_")
            })
            .collect();
        files.sort();
        files
    } else {
        vec![input_path.clone()]
    };

    if input_files.is_empty() {
        anyhow::bail!("no .jsonl files found in {} (excluding _rejudged_)", input_path.display());
    }

    eprintln!("re-judging {} file(s) with {} style...", input_files.len(), judge_style.as_str());

    // Output directory: sibling to input, named rejudged_<style>/.
    let out_dir = if input_path.is_dir() {
        input_path.join(format!("rejudged_{}", judge_style.as_str()))
    } else {
        input_path.parent().unwrap_or(&PathBuf::from("."))
            .join(format!("rejudged_{}", judge_style.as_str()))
    };
    std::fs::create_dir_all(&out_dir)?;

    let cfg = LlmConfig::from_env()?;

    // Aggregate across all files.
    let mut grand_per_cat: BTreeMap<u32, Acc> = BTreeMap::new();
    let mut grand_overall = Acc::new();

    for file_path in &input_files {
        let raw = std::fs::read_to_string(file_path)
            .with_context(|| format!("reading {}", file_path.display()))?;
        let rows: Vec<ResultRow> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).with_context(|| "parsing row"))
            .collect::<Result<Vec<_>>>()?;

        eprintln!("  {} ({} rows)", file_path.file_name().unwrap().to_string_lossy(), rows.len());

        let done = Arc::new(AtomicUsize::new(0));
        let total = rows.len();
        let rejudged: Vec<ResultRow> = futures::stream::iter(rows.into_iter())
            .map(|mut row| {
                let cfg = cfg.clone();
                let done = done.clone();
                async move {
                    if row.verdict == Verdict::Error.as_str() || row.predicted.is_empty() {
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d.is_multiple_of(200) { eprintln!("    {d}/{total}"); }
                        return row;
                    }
                    let gold = row.gold.as_deref().unwrap_or("");
                    let judge_user = if row.category == 5 {
                        format!(
                            "Question: {}\nThis is an adversarial question: the information it asks about was NOT mentioned in the conversation. The correct behavior is to state that the information is not mentioned.\n\nPredicted Answer: {}\n\nDid the model correctly refuse to answer? Answer yes or no only.",
                            row.question, row.predicted
                        )
                    } else {
                        format!(
                            "Question: {}\nCorrect Answer: {gold}\n\nPredicted Answer: {}\n\nIs the predicted answer correct? Answer yes or no only.",
                            row.question, row.predicted
                        )
                    };
                    match chat(&cfg, judge_style.system_prompt(), &judge_user, JUDGE_MAX_TOKENS).await {
                        Ok(raw_judge) => {
                            let (verdict, reason) = parse_judge_output(&raw_judge)
                                .unwrap_or((Verdict::Incorrect, raw_judge.clone()));
                            row.verdict = verdict.as_str().into();
                            row.judge_reason = reason;
                        }
                        Err(e) => {
                            row.judge_reason = format!("re-judge LLM failed: {e}");
                        }
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d.is_multiple_of(200) { eprintln!("    {d}/{total}"); }
                    row
                }
            })
            .buffered(8)
            .collect()
            .await;

        // Write re-judged JSONL to the output subdirectory (same filename).
        let out_path = out_dir.join(file_path.file_name().unwrap());
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&out_path)?);
        for row in &rejudged {
            serde_json::to_writer(&mut file, row)?;
            writeln!(file)?;
        }

        // Aggregate.
        for row in &rejudged {
            let acc = grand_per_cat.entry(row.category).or_insert_with(Acc::new);
            acc.total += 1;
            grand_overall.total += 1;
            if row.verdict == "correct" {
                acc.correct += 1;
                grand_overall.correct += 1;
            } else if row.verdict == "incorrect" {
                acc.incorrect += 1;
                grand_overall.incorrect += 1;
            } else {
                acc.error += 1;
                grand_overall.error += 1;
            }
        }
    }

    eprintln!("\n=== re-judge ({}) aggregate results ===", judge_style.as_str());
    eprintln!("overall: {:.1}% ({}/{})",
        grand_overall.correct as f64 / grand_overall.total as f64 * 100.0,
        grand_overall.correct, grand_overall.total);
    // Category names: 1=multi-hop, 2=temporal, 3=open-domain, 4=single-hop, 5=adversarial
    let cat_names = [(1u32, "multi-hop"), (2, "temporal"), (3, "open-domain"), (4, "single-hop"), (5, "adversarial")];
    for (cat, name) in &cat_names {
        if let Some(acc) = grand_per_cat.get(cat) {
            eprintln!("  cat{cat} ({name}): {:.1}% ({}/{})",
                acc.correct as f64 / acc.total as f64 * 100.0, acc.correct, acc.total);
        }
    }
    eprintln!("output dir: {}", out_dir.display());
    Ok(())
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("compact") {
        let args = compact::parse_args(&argv)?;
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(compact::run(args));
    }
    if argv.first().map(String::as_str) == Some("rejudge") {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(rejudge(&argv[1..]));
    }
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
