//! LongMemEval benchmark harness for causal-memory.
//!
//! Ingests LongMemEval haystack sessions (longmemeval_s_cleaned.json) into a
//! single shared causal-memory SQLite DB, then answers and judges the QA set
//! with an OpenAI-compatible LLM (DeepSeek by default), following the official
//! LongMemEval evaluation protocol (src/evaluation/evaluate_qa.py):
//! per-question-type judge prompts, yes/no verdict, abstention questions
//! identified by `_abs` in the question_id.
//!
//! Usage:
//!   causal-memory-longmemeval run --data benches/longmemeval/data/longmemeval_s_cleaned.json [options]
//!
//! Env:
//!   DEEPSEEK_API_KEY        (required; or CAUSAL_MEMORY_LLM_KEY)
//!   LOCOMO_LLM_API          (default: https://api.deepseek.com/v1)
//!   LOCOMO_LLM_MODEL        (default: deepseek-chat, used for answer + judge)
//!
//! DB layout: unlike LoCoMo (10 conversations -> 10 DBs), LongMemEval has 500
//! questions with ~115k-token haystacks each, so per-question DBs would be
//! far too fragmented. All haystacks go into ONE shared DB instead:
//!   - chunk ids are prefixed `{question_id}::{session_id}::{turn}` so chunks
//!     of different questions never collide;
//!   - every causal edge carries `task_tag = question_id`, and retrieval goes
//!     through `search_causal_bm25(Some(question_id), ..)`, which filters on
//!     task_tag in SQL — this is the hard isolation boundary that prevents
//!     cross-question contamination.
//!
//! Ingest is one-time and idempotent per question (chunk-count match skips).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use causal_memory::distill::{Distiller, ItemKind};
use causal_memory::hippocampus::CausalGraph;
use causal_memory::store::{CausalEntry, CausalStore};
use chrono::NaiveDateTime;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

/// Synthetic base timestamp (2023-05-01T00:00:00Z) used when a session's
/// haystack_dates entry cannot be parsed. Sessions are spaced one day apart
/// so ordering is preserved even in the fallback path.
const SYNTH_BASE_TS: i64 = 1_682_899_200;

/// Edge metadata written between consecutive turns of opposite roles.
const TURN_EDGE_RELATION: &str = "caused";
const TURN_EDGE_CONFIDENCE: f64 = 0.4;
const TURN_EDGE_DISCOVERED_BY: &str = "temporal";

/// LLM settings (temperature 0, per the LongMemEval protocol).
const ANSWER_MAX_TOKENS: u32 = 300;
const JUDGE_MAX_TOKENS: u32 = 10;
const LLM_TEMPERATURE: f32 = 0.0;
const LLM_RETRIES: usize = 3;

/// Answer system prompt: the LoCoMo run-5 balanced-refusal prompt, adapted to
/// the user/assistant chat-history setting, with the same time-normalization
/// rule (resolve relative dates against the memory's session date).
const ANSWER_SYSTEM_PROMPT: &str = r#"You are a chat assistant answering questions about your past conversations with a user, using memory snippets retrieved from those conversations.

Rules:
- Base your answer ONLY on the memories provided below.
- Keep the answer short: a few words or one sentence.
- Each memory is prefixed with its session date, e.g. "[session_3 2023/05/30 (Tue) 14:23]". When the question asks WHEN something happened, resolve relative time expressions ("yesterday", "last week", "next month", "last year") against that date and the question's current date, and answer with an ABSOLUTE date or time period (e.g. "7 May 2023", "June 2023"), not the relative expression.
- When a memory DIRECTLY addresses the question, you MUST answer — a short partial answer grounded in a memory is always better than a refusal. Refuse ONLY when no memory states the requested fact: if the memories merely discuss the same person/object/topic without stating the answer, respond that the information was not mentioned in the conversation. Never infer, generalize, or guess specific details (meanings, inspirations, reasons, feelings) that are not explicitly stated."#;

/// Extra instruction appended for knowledge-update questions: the haystack
/// may contain several versions of a fact; the newest one wins.
const KNOWLEDGE_UPDATE_RULE: &str =
    "\n- If the requested information was updated over time, always answer with the most recent \
     value (latest session date), not an outdated one.";

/// P8: Extra instruction for multi-session questions. These are typically
/// "how many X?" or "list all Y" questions that need the model to scan ALL
/// provided memories and aggregate, not stop at the first match.
const MULTI_SESSION_RULE: &str = "\n- This question requires synthesizing information across \
     MULTIPLE sessions. Follow this procedure:\n\
     1. Scan EVERY memory line, not just the first few.\n\
     2. If the question asks \"how many\" or \"list all\", first WRITE OUT each matching item \
     you find (e.g. \"1. boots from Zara, 2. jacket from H&M, 3. shirt from Uniqlo\"), then \
     give the final count or list.\n\
     3. Do NOT stop after finding one or two matches — some items may be buried deep in the \
     memory list. Read ALL lines before answering.\n\
     4. If you found N items, the answer to \"how many\" is N, even if you suspect there \
     might be more — answer based only on what the memories state.";

/// P8: Extra instruction for single-session-preference questions. These ask
/// for a preference (favorite, preferred, likes) — answer with the specific
/// stated preference, not a vague summary.
const PREFERENCE_RULE: &str = "\n- This question asks about a user preference. Answer with the \
     specific item, brand, or choice the user stated they prefer. If multiple preferences were \
     mentioned, answer with the most recently stated one.";

/// Judge system prompt (shared preamble; the user message carries the
/// official per-type template from evaluate_qa.py).
const JUDGE_SYSTEM_PROMPT: &str =
    "You are an impartial judge. Answer the user's question with yes or no only.";

const DATA_HINT: &str = "data file not found: please download longmemeval_s_cleaned.json from \
     https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned \
     and place it at benches/longmemeval/data/longmemeval_s_cleaned.json";

fn usage() {
    eprintln!("Usage: causal-memory-longmemeval run --data <longmemeval_s_cleaned.json> [options]");
    eprintln!();
    eprintln!("run options:");
    eprintln!("  --data PATH         LongMemEval dataset JSON (required)");
    eprintln!("  --limit N           max questions to run (cost guard)");
    eprintln!("  --offset M          skip the first M questions (default 0)");
    eprintln!("  --qtype TYPE        only this question_type");
    eprintln!("                      (single-session-user, single-session-assistant,");
    eprintln!("                       single-session-preference, multi-session,");
    eprintln!("                       temporal-reasoning, knowledge-update,");
    eprintln!("                       or `abstention` for _abs questions)");
    eprintln!("  --db-dir DIR        shared DB dir (default: benches/longmemeval/db)");
    eprintln!("  --out DIR           results dir (default: benches/longmemeval/results)");
    eprintln!("  --topk N            retrieved memories per question (default: 10)");
    eprintln!("  --concurrency N     parallel questions (default: 8)");
    eprintln!("  --ingest MODE       raw (default) | distill (raw + LLM-distilled facts/");
    eprintln!("                      episodes on top; separate longmemeval_distill.db)");
    eprintln!("  --ingest-only       ingest (+ distill) and exit; skip QA");
    eprintln!();
    eprintln!("Env: DEEPSEEK_API_KEY (required), LOCOMO_LLM_API, LOCOMO_LLM_MODEL");
    eprintln!();
    eprintln!("Smoke test: causal-memory-longmemeval run --data benches/longmemeval/data/longmemeval_s_cleaned.json --limit 5");
}

// ---------------------------------------------------------------------------
// Dataset model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LmeQuestion {
    question_id: String,
    question_type: String,
    question: String,
    /// Expected answer; for single-session-preference this is a rubric.
    /// Stringified defensively in case of non-string values.
    answer: serde_json::Value,
    question_date: String,
    #[serde(default)]
    haystack_session_ids: Vec<String>,
    #[serde(default)]
    haystack_dates: Vec<String>,
    #[serde(default)]
    haystack_sessions: Vec<Vec<Turn>>,
    #[serde(default)]
    answer_session_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Turn {
    role: String,
    content: String,
    /// Present (true) on turns containing the required evidence.
    #[serde(default)]
    has_answer: Option<bool>,
}

impl LmeQuestion {
    /// Abstention questions are identified by `_abs` in the question_id
    /// (official evaluate_qa.py: `abstention='_abs' in entry['question_id']`).
    fn is_abstention(&self) -> bool {
        self.question_id.contains("_abs")
    }
}

/// Stringify a gold answer that may be a JSON string or another scalar.
fn answer_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Date parsing
// ---------------------------------------------------------------------------

/// Parse LongMemEval timestamps into a unix timestamp (treated as UTC).
/// The released data uses "2023/05/30 (Tue) 14:23" (see
/// data/custom_history/sample_haystack_and_timestamp.py: "%Y/%m/%d (%a) %H:%M");
/// shorter fallbacks are accepted for robustness.
fn parse_lme_datetime(s: &str) -> Option<i64> {
    const FORMATS: &[&str] = &[
        "%Y/%m/%d (%a) %H:%M", // 2023/05/30 (Tue) 14:23
        "%Y/%m/%d %H:%M",      // 2023/05/30 14:23
    ];
    let trimmed = s.trim();
    FORMATS
        .iter()
        .find_map(|fmt| {
            NaiveDateTime::parse_from_str(trimmed, fmt)
                .ok()
                .map(|dt| dt.and_utc().timestamp())
        })
        .or_else(|| {
            // Date-only fallback: midnight UTC.
            chrono::NaiveDate::parse_from_str(trimmed, "%Y/%m/%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|dt| dt.and_utc().timestamp())
        })
}

/// Session base time; synthetic fallback keeps sessions one day apart.
fn session_base_time(session_idx: usize, date_raw: Option<&str>) -> i64 {
    match date_raw.and_then(parse_lme_datetime) {
        Some(ts) => ts,
        None => {
            eprintln!(
                "warn: session {session_idx} date {date_raw:?} unparsable, using synthetic timestamp"
            );
            SYNTH_BASE_TS + session_idx as i64 * 86_400
        }
    }
}

/// Chunk id for one turn: `{question_id}::{session_id}::{turn}` (1-based turn,
/// matching the official corpus-id convention `session_id + "_" + (i_turn+1)`).
fn chunk_id(question_id: &str, session_id: &str, turn_idx: usize) -> String {
    format!("{question_id}::{session_id}::{}", turn_idx + 1)
}

/// Chunk text for a single turn: "[session_i <raw date>] role: content".
/// `session_i` is the 1-based position in the haystack (the official reader
/// prompt numbers sessions the same way); the raw date string is kept
/// verbatim so the LLM sees exactly what the official prompts show.
fn turn_chunk_text(session_idx: usize, date_raw: &str, role: &str, content: &str) -> String {
    format!(
        "[session_{} {}] {}: {}",
        session_idx + 1,
        date_raw.trim(),
        role,
        content
    )
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/// Ingest one question's haystack into the shared store.
///
/// Each turn becomes one chunk keyed `{question_id}::{session_id}::{turn}`;
/// consecutive turns of opposite roles are linked with a low-confidence
/// `caused` edge (temporal discovery) tagged with `task_tag = question_id` —
/// that tag is what scopes later retrieval to this question's haystack.
///
/// Idempotent: if the store already holds exactly this question's expected
/// chunk count, ingestion is skipped; on a partial/stale state the question's
/// own chunks and edges are wiped and re-ingested (other questions untouched).
///
/// Returns (chunk count, evidence chunk ids) — evidence ids are the turns
/// flagged `has_answer: true`.
fn ingest_question(store: &CausalStore, q: &LmeQuestion) -> Result<(usize, Vec<String>)> {
    let prefix = format!("{}::", q.question_id);
    let expected_chunks: usize = q.haystack_sessions.iter().map(|s| s.len()).sum();

    // substr() instead of LIKE: question_ids contain '_', a LIKE wildcard.
    let existing: i64 = store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM chunks WHERE substr(id, 1, ?1) = ?2",
            rusqlite::params![prefix.len() as i64, &prefix],
            |r| r.get(0),
        )?)
    })?;
    let evidence: Vec<String> = q
        .haystack_session_ids
        .iter()
        .zip(q.haystack_sessions.iter())
        .flat_map(|(sid, session)| {
            let qid = q.question_id.clone();
            let sid = sid.clone();
            session
                .iter()
                .enumerate()
                .filter(|&(_, t)| t.has_answer == Some(true))
                .map(move |(i, _)| chunk_id(&qid, &sid, i))
        })
        .collect();
    if existing == expected_chunks as i64 && expected_chunks > 0 {
        return Ok((expected_chunks, evidence));
    }
    if existing > 0 {
        eprintln!(
            "warn: question {} has {existing} chunks, expected {expected_chunks}; re-ingesting",
            q.question_id
        );
        store.with_conn(|c| {
            c.execute(
                "DELETE FROM causal_edges WHERE task_tag = ?1",
                rusqlite::params![&q.question_id],
            )?;
            c.execute(
                "DELETE FROM chunks WHERE substr(id, 1, ?1) = ?2",
                rusqlite::params![prefix.len() as i64, &prefix],
            )?;
            Ok(())
        })?;
    }

    let mut written = 0usize;
    for (s_idx, session) in q.haystack_sessions.iter().enumerate() {
        let session_id = q
            .haystack_session_ids
            .get(s_idx)
            .cloned()
            .unwrap_or_else(|| format!("session_{}", s_idx + 1));
        let date_raw = q
            .haystack_dates
            .get(s_idx)
            .map(String::as_str)
            .unwrap_or("");
        let base = session_base_time(s_idx, q.haystack_dates.get(s_idx).map(String::as_str));

        for (t_idx, turn) in session.iter().enumerate() {
            let ts = base + t_idx as i64; // +1s per turn keeps intra-session order
            let id = chunk_id(&q.question_id, &session_id, t_idx);
            let text = turn_chunk_text(s_idx, date_raw, &turn.role, &turn.content);
            store.with_conn(|c| {
                c.execute(
                    "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![&id, &text, ts],
                )?;
                Ok(())
            })?;

            // Link each turn to the nearest preceding turn from the OTHER
            // role (the turn it responds to). The first turn of a session,
            // or a turn with no prior opposite-role turn, gets no edge.
            let prev_idx = session[..t_idx].iter().rposition(|t| t.role != turn.role);
            if let Some(prev_idx) = prev_idx {
                let prev_id = chunk_id(&q.question_id, &session_id, prev_idx);
                store.with_conn(|c| {
                    c.execute(
                        "INSERT INTO causal_edges
                         (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        rusqlite::params![
                            &prev_id,
                            &id,
                            TURN_EDGE_RELATION,
                            TURN_EDGE_CONFIDENCE,
                            TURN_EDGE_DISCOVERED_BY,
                            ts,
                            ts,
                            &q.question_id
                        ],
                    )?;
                    Ok(())
                })?;
            }
            written += 1;
        }
    }
    Ok((written, evidence))
}

// ---------------------------------------------------------------------------
// Distill-mode ingest
// ---------------------------------------------------------------------------

/// Ingest mode: `raw` (turn chunks + temporal edges only, the baseline) or
/// `distill` (raw PLUS an LLM distillation pass per haystack session:
/// facts/preferences → the fact layer scoped to the question_id, lessons/
/// events → distilled causal edges tagged question_id). Distill runs use a
/// separate `longmemeval_distill.db` so the raw baseline DB stays intact.
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

/// Statistics of one question's distillation pass (auditable in the summary,
/// same discipline as the Memora harness).
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

/// Haystack session date as "YYYY-MM-DD" for the distiller (the raw
/// haystack_dates entry parses to a full timestamp; distill items carry only
/// the date; unparseable dates fall back to the synthetic per-index spacing).
fn session_date_str(q: &LmeQuestion, s_idx: usize) -> String {
    let ts = session_base_time(s_idx, q.haystack_dates.get(s_idx).map(String::as_str));
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Distill every haystack session of one question into the store.
///
/// Bounded-concurrency LLM calls, recorded strictly in session order so a
/// later session's `supersedes` always sees the earlier items. Routing:
/// Fact/Preference → the fact layer (scope = question_id, so fact retrieval
/// is hard-scoped to this haystack exactly like the edge retrieval) with
/// supersedes-driven retirement via `retire_facts_by_hint`; Lesson/Event →
/// `record_distilled` tagged question_id. Raw turn chunks are always
/// ingested as well (dual write): 96% of haystack sessions are detail-heavy,
/// and the evidence-id protocol needs the raw chunks.
///
/// Idempotent at the question level via the `distill_done` marker table:
/// the marker is written only after ALL of a question's sessions were
/// recorded, so an interrupted question is redone cleanly on the next run
/// (item-level idempotency lives in record_distilled / record_fact's
/// upsert, so a redo is cheap and never double-writes).
async fn distill_question(
    store: &CausalStore,
    distiller: Option<&Distiller>,
    q: &LmeQuestion,
    concurrency: usize,
) -> Result<DistillStats> {
    let mut stats = DistillStats {
        sessions: q.haystack_sessions.len(),
        ..Default::default()
    };

    let existing: i64 = store.with_conn(|c| {
        c.execute(
            "CREATE TABLE IF NOT EXISTS distill_done (
                qid TEXT PRIMARY KEY,
                done_at INTEGER NOT NULL
            )",
            [],
        )?;
        Ok(c.query_row(
            "SELECT COUNT(*) FROM distill_done WHERE qid = ?1",
            rusqlite::params![&q.question_id],
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

    let futures = q
        .haystack_sessions
        .iter()
        .enumerate()
        .map(|(s_idx, session)| {
            let date = session_date_str(q, s_idx);
            let turns: Vec<(String, String)> = session
                .iter()
                .map(|t| (t.role.clone(), t.content.clone()))
                .collect();
            async move { distiller.distill_session(&date, &turns).await }
        });
    let results: Vec<Result<Vec<causal_memory::distill::MemoryItem>>> =
        futures::stream::iter(futures)
            .buffered(concurrency)
            .collect()
            .await;
    stats.llm_calls = results.len();

    // Record strictly in session order.
    let mut sessions_failed = 0usize;
    for (s_idx, result) in results.into_iter().enumerate() {
        let items = match result {
            Ok(items) => items,
            Err(e) => {
                // Raw chunks for this session are already ingested, so no
                // data is lost; the pass continues with the next session.
                eprintln!(
                    "warn: distill failed for {} session {} ({e}); raw chunks already cover it",
                    q.question_id,
                    s_idx + 1
                );
                sessions_failed += 1;
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
                    // v7 namespaced scope: hard-scopes fact retrieval to this
                    // question's haystack, mirroring the edge task_tag filter.
                    let fact_scope = format!("lme:{}", q.question_id);
                    // Retire BEFORE record: the new value often shares topic
                    // tokens with its own supersedes hint and
                    // retire_facts_by_hint has no self-exclusion — recording
                    // first can retire the fact we just wrote (found by
                    // review). NOTE: the 20260730/31 full distill runs
                    // predates this fix; any self-retires there remove facts
                    // (scores degrade toward the raw baseline), i.e. the
                    // reported +7.8pp is, if anything, conservative.
                    if let Some(hint) = item.supersedes.as_deref() {
                        match store.retire_facts_by_hint(kind, &fact_scope, hint) {
                            Ok(n) => stats.facts_retired += n,
                            Err(e) => eprintln!(
                                "warn: retire_facts_by_hint failed for {} ({e}); stale fact may stay live",
                                q.question_id
                            ),
                        }
                    }
                    store.record_fact(kind, &item.text, &fact_scope, "distill", 0.8)?;
                    stats.facts_recorded += 1;
                }
                ItemKind::Lesson | ItemKind::Event => {
                    let out = store.record_distilled(item, Some(&q.question_id))?;
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

    // Completion marker: only written after ALL sessions of this question
    // were processed. The skip check above keys on this table (not on "has
    // any distill edges"), so a question interrupted mid-record is redone
    // cleanly on the next run instead of being skipped half-written.
    // EXCEPTION: if EVERY session's LLM call failed (rate-limit burst, API
    // outage), the question produced nothing through no fault of its own —
    // writing the marker would freeze it as "successfully empty" (found the
    // hard way: 133 questions marked with zero data during a 429 storm).
    if sessions_failed == stats.sessions && stats.sessions > 0 {
        eprintln!(
            "warn: ALL {} sessions of {} failed; NOT marking done (retry next run)",
            stats.sessions, q.question_id
        );
        return Ok(stats);
    }
    store.with_conn(|c| {
        c.execute(
            "INSERT OR REPLACE INTO distill_done (qid, done_at) VALUES (?1, ?2)",
            rusqlite::params![&q.question_id, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    })?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

/// Retrieve candidate causal entries for a question (BM25), hard-scoped to
/// this question's haystack via the task_tag = question_id edge filter.
///
/// For multi-session questions, does **iterative retrieval**: extracts
/// content nouns from the question and runs additional BM25 queries per
/// noun, merging results by dedup on edge_id. This widens the evidence
/// net — a single top-k query misses fragments scattered across 40+
/// sessions, but per-noun queries catch them.
fn retrieve(store: &CausalStore, q: &LmeQuestion, topk: usize) -> Result<Vec<CausalEntry>> {
    let base = store.search_causal_bm25(Some(&q.question_id), &q.question, topk)?;

    // P7: multi-session retrieval boost.
    if q.question_type != "multi-session" || base.len() < 2 {
        return Ok(base);
    }

    // Extract content words from the question (skip stopwords, short words).
    let stopwords: HashSet<&str> = [
        "how", "many", "what", "which", "who", "whom", "whose", "where", "when",
        "why", "do", "did", "does", "is", "are", "was", "were", "have", "has",
        "had", "i", "you", "we", "they", "he", "she", "it", "the", "a", "an",
        "of", "in", "on", "at", "to", "for", "with", "from", "by", "and", "or",
        "but", "not", "this", "that", "these", "those", "my", "your", "me",
        "need", "pick", "up", "return", "list", "all", "items", "kind", "types",
        "led", "leading", "worked", "bought", "am", "currently",
    ]
    .into_iter()
    .collect();

    let mut seen_ids: HashSet<i64> = base.iter().map(|e| e.edge_id).collect();
    let mut merged = base;

    // Pull content nouns ≥4 chars that aren't stopwords.
    let nouns: Vec<String> = q
        .question
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| w.len() >= 4 && !stopwords.contains(w.as_str()))
        .collect();

    // Run one BM25 query per noun, merge new hits.
    for noun in &nouns {
        let hits = store.search_causal_bm25(Some(&q.question_id), noun, topk / 2)?;
        for entry in hits {
            if seen_ids.insert(entry.edge_id) {
                merged.push(entry);
            }
        }
    }

    Ok(merged)
}

/// P7+: Hippocampus spreading activation for multi-session questions.
///
/// Uses a pre-built graph (built once per run, not per question — building
/// from a 500-question DB is expensive). Runs spreading_activation on the
/// question text and collects texts of activated nodes not already in the
/// BM25 result set. These are "associative hits" — semantically related
/// memories that keyword search missed but spreading activation found
/// through edge traversal.
fn hippocampus_boost(
    graph: Option<&CausalGraph>,
    q: &LmeQuestion,
    existing_texts: &HashSet<String>,
) -> Vec<String> {
    if q.question_type != "multi-session" {
        return Vec::new();
    }
    let graph = match graph {
        Some(g) => g,
        None => return Vec::new(),
    };
    // Clone for read-only activation (avoids Hebbian side effects + lifetime).
    let mut g = graph.clone();
    let results = g.spreading_activation_opts(&q.question, None, false, false);
    let mut extra = Vec::new();
    for r in results.iter().take(20) {
        let snippet = if r.text.len() > 50 { &r.text[..50] } else { &r.text };
        if !existing_texts
            .iter()
            .any(|e| e.contains(snippet) || snippet.contains(e.as_str()))
        {
            extra.push(format!("- [spreading] {}", r.text));
        }
    }
    extra
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
// Answer & judge prompts (official LongMemEval protocol)
// ---------------------------------------------------------------------------

/// Answer system prompt for a question type: balanced-refusal base plus
/// type-specific rules where applicable.
fn answer_system_prompt(question_type: &str) -> String {
    let mut prompt = ANSWER_SYSTEM_PROMPT.to_string();
    match question_type {
        "knowledge-update" => prompt.push_str(KNOWLEDGE_UPDATE_RULE),
        "multi-session" => prompt.push_str(MULTI_SESSION_RULE),
        "single-session-preference" => prompt.push_str(PREFERENCE_RULE),
        _ => {}
    }
    prompt
}

/// Answer user prompt: memories + question_date as the "current time"
/// reference (mirrors the official reader template's `Current Date:` field).
fn answer_user_prompt(q: &LmeQuestion, memories: &str) -> String {
    let memories = if memories.is_empty() {
        "(no memories retrieved)"
    } else {
        memories
    };
    format!(
        "Current Date: {}\n\nMemories:\n{memories}\n\nQuestion: {}\nAnswer:",
        q.question_date, q.question
    )
}

/// Judge user prompt, ported 1:1 from the official
/// src/evaluation/evaluate_qa.py `get_anscheck_prompt` templates.
fn judge_user_prompt(q: &LmeQuestion, predicted: &str) -> String {
    let answer = answer_to_string(&q.answer);
    if q.is_abstention() {
        return format!(
            "I will give you an unanswerable question, an explanation, and a response from a \
             model. Please answer yes if the model correctly identifies the question as \
             unanswerable. The model could say that the information is incomplete, or some \
             other information is given but the asked information is not.\n\n\
             Question: {}\n\nExplanation: {answer}\n\nModel Response: {predicted}\n\n\
             Does the model correctly identify the question as unanswerable? \
             Answer yes or no only.",
            q.question
        );
    }
    match q.question_type.as_str() {
        "single-session-user" | "single-session-assistant" | "multi-session" => format!(
            "I will give you a question, a correct answer, and a response from a model. \
             Please answer yes if the response contains the correct answer. Otherwise, answer \
             no. If the response is equivalent to the correct answer or contains all the \
             intermediate steps to get the correct answer, you should also answer yes. If the \
             response only contains a subset of the information required by the answer, answer \
             no. \n\nQuestion: {}\n\nCorrect Answer: {answer}\n\nModel Response: {predicted}\n\n\
             Is the model response correct? Answer yes or no only.",
            q.question
        ),
        "temporal-reasoning" => format!(
            "I will give you a question, a correct answer, and a response from a model. \
             Please answer yes if the response contains the correct answer. Otherwise, answer \
             no. If the response is equivalent to the correct answer or contains all the \
             intermediate steps to get the correct answer, you should also answer yes. If the \
             response only contains a subset of the information required by the answer, answer \
             no. In addition, do not penalize off-by-one errors for the number of days. If the \
             question asks for the number of days/weeks/months, etc., and the model makes \
             off-by-one errors (e.g., predicting 19 days when the answer is 18), the model's \
             response is still correct. \n\nQuestion: {}\n\nCorrect Answer: {answer}\n\n\
             Model Response: {predicted}\n\nIs the model response correct? \
             Answer yes or no only.",
            q.question
        ),
        "knowledge-update" => format!(
            "I will give you a question, a correct answer, and a response from a model. \
             Please answer yes if the response contains the correct answer. Otherwise, answer \
             no. If the response contains some previous information along with an updated \
             answer, the response should be considered as correct as long as the updated \
             answer is the required answer.\n\nQuestion: {}\n\nCorrect Answer: {answer}\n\n\
             Model Response: {predicted}\n\nIs the model response correct? \
             Answer yes or no only.",
            q.question
        ),
        "single-session-preference" => format!(
            "I will give you a question, a rubric for desired personalized response, and a \
             response from a model. Please answer yes if the response satisfies the desired \
             response. Otherwise, answer no. The model does not need to reflect all the points \
             in the rubric. The response is correct as long as it recalls and utilizes the \
             user's personal information correctly.\n\nQuestion: {}\n\nRubric: {answer}\n\n\
             Model Response: {predicted}\n\nIs the model response correct? \
             Answer yes or no only.",
            q.question
        ),
        other => format!(
            "warn: unknown question_type {other:?}, using generic judge template\n\
             I will give you a question, a correct answer, and a response from a model. \
             Please answer yes if the response contains the correct answer. Otherwise, answer \
             no.\n\nQuestion: {}\n\nCorrect Answer: {answer}\n\nModel Response: {predicted}\n\n\
             Is the model response correct? Answer yes or no only.",
            q.question
        ),
    }
}

// ---------------------------------------------------------------------------
// Judge verdict parsing
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Verdict {
    Correct,
    Incorrect,
    /// Infrastructure failure (LLM unreachable after retries) or empty judge
    /// output — excluded from accuracy.
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

/// Parse the judge's yes/no reply. Official logic (evaluate_qa.py):
/// `label = 'yes' in eval_response.lower()` — anything not containing "yes"
/// is a "no". Empty output returns None (treated as Error by the caller).
fn parse_judge_output(raw: &str) -> Option<Verdict> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    Some(if s.contains("yes") {
        Verdict::Correct
    } else {
        Verdict::Incorrect
    })
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

struct Args {
    data: PathBuf,
    limit: Option<usize>,
    offset: usize,
    qtype: Option<String>,
    db_dir: PathBuf,
    out_dir: PathBuf,
    topk: usize,
    concurrency: usize,
    ingest: IngestMode,
    /// Ingest (+ distill) only; skip the QA phase. Used to warm the shared
    /// distill DB in offset/limit chunks before one full QA run.
    ingest_only: bool,
}

fn parse_args(argv: &[String]) -> Result<Option<Args>> {
    if argv.is_empty() || argv.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(None);
    }
    if argv[0] != "run" {
        anyhow::bail!("unknown subcommand {:?}; expected `run`", argv[0]);
    }
    let mut data: Option<PathBuf> = None;
    let mut limit = None;
    let mut offset = 0usize;
    let mut qtype: Option<String> = None;
    let mut db_dir = PathBuf::from("benches/longmemeval/db");
    let mut out_dir = PathBuf::from("benches/longmemeval/results");
    let mut topk = 10usize;
    let mut concurrency = 8usize;
    let mut ingest = IngestMode::Raw;
    let mut ingest_only = false;

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
            "--limit" => limit = Some(take(&mut i, "--limit")?.parse()?),
            "--offset" => offset = take(&mut i, "--offset")?.parse()?,
            "--qtype" => qtype = Some(take(&mut i, "--qtype")?),
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
            other => anyhow::bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    let data = data.ok_or_else(|| anyhow!("--data is required"))?;
    Ok(Some(Args {
        data,
        limit,
        offset,
        qtype,
        db_dir,
        out_dir,
        topk,
        concurrency,
        ingest,
        ingest_only,
    }))
}

#[derive(Serialize)]
struct ResultRow {
    question_id: String,
    question_type: String,
    abstention: bool,
    question: String,
    gold: String,
    predicted: String,
    verdict: String,
    judge_reason: String,
    retrieved_ids: Vec<String>,
    evidence_ids: Vec<String>,
    /// Session-level evidence (official answer_session_ids), for recall checks.
    answer_session_ids: Vec<String>,
    evidence_hit: bool,
}

#[derive(Serialize)]
struct TypeStats {
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
    /// causal-only protocol — that comparison is the point).
    ingest: String,
    /// Aggregated distillation-pass statistics (distill mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    distill_ingest: Option<DistillStats>,
    data: String,
    qtype_filter: Option<String>,
    total_questions: usize,
    correct: usize,
    incorrect: usize,
    error: usize,
    accuracy: f64,
    evidence_hit_rate: f64,
    per_question_type: BTreeMap<String, TypeStats>,
    abstention: TypeStats,
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
    fn stats(&self) -> TypeStats {
        TypeStats {
            total: self.total,
            correct: self.correct,
            incorrect: self.incorrect,
            error: self.error,
            accuracy: self.accuracy(),
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
    graph: &Option<CausalGraph>,
    q: &LmeQuestion,
    evidence_ids: Vec<String>,
    topk: usize,
    with_facts: bool,
) -> ResultRow {
    let retrieved = retrieve(store, q, topk).unwrap_or_default();
    let retrieved_ids = retrieved_chunk_ids(&retrieved);
    let evidence_hit = retrieved_ids.iter().any(|r| evidence_ids.contains(r));
    // Distill mode additionally queries the fact layer (BM25, scoped to this
    // question's haystack, same topk) and puts fact lines FIRST: they are
    // the high-precision layer for the factual-recall slice the causal-only
    // baseline conceded. Evidence-hit stays computed from causal entries
    // only (facts carry no chunk ids) — protocol unchanged.
    let memories = if with_facts {
        let fact_scope = format!("lme:{}", q.question_id);
        // P7: multi-session questions need ALL matching facts, not top-k.
        // "How many X?" questions require scanning every fact that mentions
        // the entity — top-k truncates and the count comes out wrong.
        // Use a very large top-k (500) to effectively list all matching facts.
        let fact_topk = if q.question_type == "multi-session" {
            500
        } else {
            topk
        };
        let facts = store
            .search_facts_bm25(&q.question, Some(&fact_scope), fact_topk)
            .unwrap_or_default();
        let mut lines: Vec<String> = facts.iter().map(|f| format!("- {}", f.value)).collect();

        // P7: for multi-session, also run per-noun fact queries and merge,
        // catching facts that the full-question BM25 missed (different
        // phrasing in distill vs question).
        if q.question_type == "multi-session" {
            let mut seen_values: HashSet<String> =
                facts.iter().map(|f| f.value.clone()).collect();
            let nouns: Vec<String> = q
                .question
                .split_whitespace()
                .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
                .filter(|w| w.len() >= 4)
                .filter(|w| !["what", "which", "how", "many", "have", "been", "does", "that", "this", "with", "from", "they", "them"].contains(&w.as_str()))
                .take(5)
                .collect();
            for noun in &nouns {
                let extra = store
                    .search_facts_bm25(noun, Some(&fact_scope), 50)
                    .unwrap_or_default();
                for f in extra {
                    if seen_values.insert(f.value.clone()) {
                        lines.push(format!("- {}", f.value));
                    }
                }
            }
        }

        let causal = memory_lines(&retrieved);
        if !causal.is_empty() {
            lines.push(causal);
        }

        // P7+: hippocampus spreading activation — finds associative hits
        // that BM25 missed (semantically related via edge traversal).
        // NOTE: in practice on LongMemEval, the BM25 + full-scan fact layer
        // already covers most evidence; hippocampus spreading finds few NEW
        // nodes. Its value is in the agent ablation bench (repeated exposure,
        // where associative recall accumulates). Kept wired but guarded by
        // CAUSAL_MEMORY_HIPPOCAMPUS_BENCH env var for controlled experiments.
        if std::env::var("CAUSAL_MEMORY_HIPPOCAMPUS_BENCH").is_ok() {
            let existing: HashSet<String> = lines
                .iter()
                .map(|l| l.trim_start_matches("- ").to_lowercase())
                .collect();
            let hippo_extra = hippocampus_boost(graph.as_ref(), q, &existing);
            if !hippo_extra.is_empty() {
                lines.push("[associative memory]".to_string());
                lines.extend(hippo_extra);
            }
        }

        lines.join("\n")
    } else {
        memory_lines(&retrieved)
    };

    let system = answer_system_prompt(&q.question_type);
    let answer_user = answer_user_prompt(q, &memories);
    let predicted = match chat(cfg, &system, &answer_user, ANSWER_MAX_TOKENS).await {
        Ok(s) => s,
        Err(e) => {
            return ResultRow {
                question_id: q.question_id.clone(),
                question_type: q.question_type.clone(),
                abstention: q.is_abstention(),
                question: q.question.clone(),
                gold: answer_to_string(&q.answer),
                predicted: String::new(),
                verdict: Verdict::Error.as_str().into(),
                judge_reason: format!("answer LLM failed: {e}"),
                retrieved_ids,
                evidence_ids,
                answer_session_ids: q.answer_session_ids.clone(),
                evidence_hit,
            }
        }
    };

    let judge_user = judge_user_prompt(q, &predicted);
    let (verdict, reason) =
        match chat(cfg, JUDGE_SYSTEM_PROMPT, &judge_user, JUDGE_MAX_TOKENS).await {
            Ok(raw) => match parse_judge_output(&raw) {
                Some(v) => (v, raw),
                None => (Verdict::Error, format!("empty judge output: {raw}")),
            },
            Err(e) => (Verdict::Error, format!("judge LLM failed: {e}")),
        };

    ResultRow {
        question_id: q.question_id.clone(),
        question_type: q.question_type.clone(),
        abstention: q.is_abstention(),
        question: q.question.clone(),
        gold: answer_to_string(&q.answer),
        predicted,
        verdict: verdict.as_str().into(),
        judge_reason: reason,
        retrieved_ids,
        evidence_ids,
        answer_session_ids: q.answer_session_ids.clone(),
        evidence_hit,
    }
}

async fn run(args: Args) -> Result<()> {
    // Graceful failure when the dataset has not been downloaded yet.
    if !args.data.is_file() {
        anyhow::bail!("{} (looked at {})", DATA_HINT, args.data.display());
    }
    let cfg = LlmConfig::from_env()?;
    eprintln!("LLM: {} @ {}", cfg.model, cfg.api_base);

    let raw = std::fs::read_to_string(&args.data)
        .with_context(|| format!("reading {}", args.data.display()))?;
    let questions: Vec<LmeQuestion> =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", args.data.display()))?;

    // Selection: qtype filter -> offset -> limit. `abstention` is a pseudo
    // type matching _abs questions (they keep their real question_type too).
    let selected: Vec<&LmeQuestion> = questions
        .iter()
        .filter(|q| match &args.qtype {
            Some(t) if t == "abstention" => q.is_abstention(),
            Some(t) => &q.question_type == t,
            None => true,
        })
        .skip(args.offset)
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();
    eprintln!(
        "dataset: {} questions, selected {} (offset {}, limit {:?}, qtype {:?})",
        questions.len(),
        selected.len(),
        args.offset,
        args.limit,
        args.qtype
    );
    if selected.is_empty() {
        eprintln!("nothing to do");
        return Ok(());
    }

    std::fs::create_dir_all(&args.db_dir)?;
    std::fs::create_dir_all(&args.out_dir)?;
    // Distill mode uses a separate DB so the raw baseline stays intact.
    let db_path = match args.ingest {
        IngestMode::Raw => args.db_dir.join("longmemeval.db"),
        IngestMode::Distill => args.db_dir.join("longmemeval_distill.db"),
    };
    let store =
        CausalStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    // Ingest phase (sequential, idempotent) before any answering. Distill
    // mode adds a per-question LLM distillation pass (also idempotent).
    let distiller = Distiller::from_env().map(Arc::new);
    let mut distill_totals = DistillStats::default();
    let mut pending_distill: Vec<&LmeQuestion> = Vec::new();
    let mut evidence_by_qid: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for q in &selected {
        let (n, evidence) = ingest_question(&store, q)
            .with_context(|| format!("ingesting question {}", q.question_id))?;
        eprintln!(
            "ingest {}: {n} chunks ({} sessions, {} evidence turns)",
            q.question_id,
            q.haystack_sessions.len(),
            evidence.len()
        );
        if args.ingest == IngestMode::Distill {
            pending_distill.push(*q);
        }
        evidence_by_qid.insert(q.question_id.clone(), evidence);
    }

    // Distill phase: cross-question parallelism. Within one question,
    // sessions are still distilled with bounded concurrency and recorded in
    // strict session order (supersedes semantics); ACROSS questions there is
    // no ordering dependency (separate task_tag / fact scopes), so questions
    // are pipelined DISTILL_OUTER at a time. Total in-flight LLM calls ≈
    // DISTILL_OUTER × per-question concurrency.
    const DISTILL_OUTER: usize = 8;
    if args.ingest == IngestMode::Distill && !pending_distill.is_empty() {
        let per_q = (args.concurrency / DISTILL_OUTER).max(4);
        let total_q = pending_distill.len();
        let done_q = Arc::new(AtomicUsize::new(0));
        let results: Vec<(String, Result<DistillStats>)> =
            futures::stream::iter(pending_distill.iter().map(|q| {
                let store = store.clone();
                let distiller = distiller.clone();
                let done_q = done_q.clone();
                async move {
                    let r = distill_question(&store, distiller.as_deref(), q, per_q).await;
                    let d = done_q.fetch_add(1, Ordering::Relaxed) + 1;
                    if d.is_multiple_of(10) || d == total_q {
                        eprintln!("distill progress: {d}/{total_q} questions");
                    }
                    (q.question_id.clone(), r)
                }
            }))
            .buffered(DISTILL_OUTER)
            .collect()
            .await;
        for (qid, r) in results {
            let stats = r.with_context(|| format!("distilling question {qid}"))?;
            distill_totals.sessions += stats.sessions;
            distill_totals.llm_calls += stats.llm_calls;
            distill_totals.facts_recorded += stats.facts_recorded;
            distill_totals.episodes_recorded += stats.episodes_recorded;
            distill_totals.episodes_duplicate += stats.episodes_duplicate;
            distill_totals.facts_retired += stats.facts_retired;
            distill_totals.superseded_invalidations += stats.superseded_invalidations;
        }
    }

    if args.ingest_only {
        eprintln!(
            "--ingest-only: {} questions ingested (distill: {} sessions, {} facts, {} episodes), no QA run",
            selected.len(),
            distill_totals.sessions,
            distill_totals.facts_recorded,
            distill_totals.episodes_recorded,
        );
        return Ok(());
    }

    // Answer + judge phase (parallel across questions).
    let run_id = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let done = Arc::new(AtomicUsize::new(0));
    let total = selected.len();
    let with_facts = args.ingest == IngestMode::Distill;

    // P7+: Build the hippocampus graph ONCE for the whole run (building it
    // per-question from a 500-question DB is too slow). Used for multi-session
    // spreading activation. None if graph build fails (benchmark continues).
    eprintln!("building hippocampus graph for spreading activation...");
    let graph: Arc<Option<CausalGraph>> = Arc::new(CausalGraph::from_store(&store).ok());
    if let Some(g) = graph.as_ref() {
        eprintln!("  graph: {} nodes, {} edges", g.num_nodes(), g.num_edges());
    }

    let rows: Vec<ResultRow> = futures::stream::iter(selected.iter().map(|q| {
        let cfg = cfg.clone();
        let store = store.clone();
        let done = done.clone();
        let graph = graph.clone();
        let evidence = evidence_by_qid
            .get(&q.question_id)
            .cloned()
            .unwrap_or_default();
        async move {
            let row = answer_question(&cfg, &store, &graph, q, evidence, args.topk, with_facts).await;
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d.is_multiple_of(25) || d == total {
                eprintln!("{d}/{total} questions done");
            }
            row
        }
    }))
    .buffer_unordered(args.concurrency)
    .collect()
    .await;

    let jsonl_path = args.out_dir.join(format!("run_{run_id}.jsonl"));
    let mut out = String::new();
    let mut overall = Acc::new();
    let mut per_type: BTreeMap<String, Acc> = BTreeMap::new();
    let mut abstention = Acc::new();
    for row in &rows {
        overall.add(row);
        per_type
            .entry(row.question_type.clone())
            .or_insert_with(Acc::new)
            .add(row);
        if row.abstention {
            abstention.add(row);
        }
        out.push_str(&serde_json::to_string(row)?);
        out.push('\n');
    }
    std::fs::write(&jsonl_path, out)?;
    eprintln!("wrote {}", jsonl_path.display());

    let summary = Summary {
        run_id: run_id.clone(),
        date: chrono::Utc::now().to_rfc3339(),
        git_commit: git_commit(),
        model: cfg.model.clone(),
        judge_model: cfg.model.clone(),
        temperature: LLM_TEMPERATURE,
        topk: args.topk,
        ingest: args.ingest.as_str().to_string(),
        distill_ingest: (args.ingest == IngestMode::Distill).then_some(distill_totals),
        data: args.data.display().to_string(),
        qtype_filter: args.qtype.clone(),
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
        per_question_type: per_type
            .iter()
            .map(|(t, a)| (t.clone(), a.stats()))
            .collect(),
        abstention: abstention.stats(),
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
    fn parse_lme_datetime_formats() {
        // Actual released format: "2023/05/30 (Tue) 14:23".
        let ts = parse_lme_datetime("2023/05/30 (Tue) 14:23").expect("should parse");
        assert_eq!(ts, 1_685_456_580); // 2023-05-30T14:23:00Z

        // Fallbacks.
        assert!(parse_lme_datetime("2023/05/30 14:23").is_some());
        let day = parse_lme_datetime("2023/05/30").expect("date-only fallback");
        assert_eq!(day, 1_685_404_800); // 2023-05-30T00:00:00Z

        assert!(parse_lme_datetime("not a date").is_none());
        assert!(parse_lme_datetime("05/30/2023").is_none());
    }

    #[test]
    fn synthetic_fallback_preserves_order() {
        let t0 = session_base_time(0, None);
        let t1 = session_base_time(1, Some("garbage"));
        assert_eq!(t1 - t0, 86_400, "sessions spaced one day apart");
    }

    fn tiny_question(qid: &str, qtype: &str) -> LmeQuestion {
        let raw = format!(
            r#"{{
                "question_id": "{qid}",
                "question_type": "{qtype}",
                "question": "What camera did the user buy?",
                "answer": "Nikon Z6",
                "question_date": "2023/06/01 (Thu) 10:00",
                "haystack_session_ids": ["{qid}_s1", "{qid}_s2"],
                "haystack_dates": ["2023/05/20 (Sat) 09:00", "2023/05/28 (Sun) 15:30"],
                "haystack_sessions": [
                    [
                        {{"role": "user", "content": "I want to buy a camera."}},
                        {{"role": "assistant", "content": "Any brand in mind?"}}
                    ],
                    [
                        {{"role": "user", "content": "I bought a Nikon Z6 yesterday.", "has_answer": true}},
                        {{"role": "assistant", "content": "Great choice!"}}
                    ]
                ],
                "answer_session_ids": ["{qid}_s2"]
            }}"#
        );
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn ingest_writes_chunks_edges_and_is_idempotent() {
        let q = tiny_question("q1", "single-session-user");
        let store = CausalStore::open_in_memory().unwrap();

        let (n, evidence) = ingest_question(&store, &q).unwrap();
        assert_eq!(n, 4);
        assert_eq!(evidence, vec!["q1::q1_s2::1".to_string()]);

        // Edges: each session's second turn links back to the first (opposite
        // role): 2 edges, both tagged with the question id.
        let edges = store.all_valid_edges().unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges
            .iter()
            .all(|e| e.decision_id.starts_with("q1::") && e.outcome_id.starts_with("q1::")));

        // Chunk text format: 1-based session number + raw date + role.
        let text: String = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT text FROM chunks WHERE id = 'q1::q1_s2::1'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            text,
            "[session_2 2023/05/28 (Sun) 15:30] user: I bought a Nikon Z6 yesterday."
        );

        // Session 2 turns are later than session 1 turns.
        let t = |id: &str| -> i64 {
            store
                .with_conn(|c| {
                    Ok(c.query_row(
                        "SELECT created_at FROM chunks WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get(0),
                    )?)
                })
                .unwrap()
        };
        assert!(t("q1::q1_s2::1") > t("q1::q1_s1::2"));
        assert_eq!(t("q1::q1_s1::2") - t("q1::q1_s1::1"), 1, "+1s per turn");

        // Idempotent: second run skips (chunk count unchanged, edges not
        // duplicated).
        let (n2, _) = ingest_question(&store, &q).unwrap();
        assert_eq!(n2, 4);
        assert_eq!(store.all_valid_edges().unwrap().len(), 2);
    }

    #[test]
    fn retrieval_is_isolated_by_question_id_prefix() {
        // Two questions sharing one store: retrieval scoped by task_tag must
        // never leak the other question's chunks, even for identical text.
        let q1 = tiny_question("q1", "single-session-user");
        let q2 = tiny_question("q2_abs", "single-session-user");
        let store = CausalStore::open_in_memory().unwrap();
        ingest_question(&store, &q1).unwrap();
        ingest_question(&store, &q2).unwrap();

        let res = retrieve(&store, &q1, 10).unwrap();
        assert!(!res.is_empty());
        let ids = retrieved_chunk_ids(&res);
        assert!(ids.iter().all(|id| id.starts_with("q1::")));
        assert!(ids.iter().any(|id| id == "q1::q1_s2::1"));

        let res2 = retrieve(&store, &q2, 10).unwrap();
        assert!(!res2.is_empty());
        assert!(retrieved_chunk_ids(&res2)
            .iter()
            .all(|id| id.starts_with("q2_abs::")));
    }

    #[test]
    fn evidence_hit_detects_answer_turn() {
        let q = tiny_question("q1", "single-session-user");
        let store = CausalStore::open_in_memory().unwrap();
        let (_, evidence) = ingest_question(&store, &q).unwrap();
        let res = retrieve(&store, &q, 10).unwrap();
        let ids = retrieved_chunk_ids(&res);
        assert!(ids.iter().any(|r| evidence.contains(r)));
    }

    #[test]
    fn knowledge_update_prompt_has_latest_value_rule() {
        let base = answer_system_prompt("single-session-user");
        assert!(!base.contains("most recent value"));

        let ku = answer_system_prompt("knowledge-update");
        assert!(ku.contains("most recent value"));
        assert!(ku.contains("latest session date"));
        assert!(ku.starts_with(ANSWER_SYSTEM_PROMPT));
    }

    #[test]
    fn answer_prompt_carries_question_date() {
        let q = tiny_question("q1", "single-session-user");
        let p = answer_user_prompt(&q, "- mem");
        assert!(p.contains("Current Date: 2023/06/01 (Thu) 10:00"));
        let p_empty = answer_user_prompt(&q, "");
        assert!(p_empty.contains("(no memories retrieved)"));
    }

    #[test]
    fn judge_prompts_match_official_templates() {
        let mut q = tiny_question("q1", "knowledge-update");
        let p = judge_user_prompt(&q, "Nikon Z6");
        assert!(p.contains("previous information along with an updated answer"));
        assert!(p.contains("Correct Answer: Nikon Z6"));
        assert!(p.ends_with("Answer yes or no only."));

        q.question_type = "temporal-reasoning".into();
        assert!(judge_user_prompt(&q, "x").contains("off-by-one"));

        q.question_type = "single-session-preference".into();
        assert!(judge_user_prompt(&q, "x").contains("Rubric: Nikon Z6"));

        // Abstention overrides the question_type template.
        q.question_id = "q1_abs".into();
        let p = judge_user_prompt(&q, "I don't know");
        assert!(p.contains("unanswerable question"));
        assert!(p.contains("Explanation: Nikon Z6"));
        assert!(q.is_abstention());
    }

    #[test]
    fn judge_verdict_parsing_follows_official_rule() {
        // Official: label = 'yes' in response.lower().
        assert_eq!(parse_judge_output("yes"), Some(Verdict::Correct));
        assert_eq!(parse_judge_output("Yes."), Some(Verdict::Correct));
        assert_eq!(parse_judge_output("no"), Some(Verdict::Incorrect));
        assert_eq!(parse_judge_output("NO"), Some(Verdict::Incorrect));
        // Anything without "yes" counts as no (official behavior).
        assert_eq!(parse_judge_output("maybe"), Some(Verdict::Incorrect));
        // Empty output is an infrastructure-level error, not a verdict.
        assert_eq!(parse_judge_output(""), None);
        assert_eq!(parse_judge_output("   "), None);
    }

    #[test]
    fn abstention_detected_by_abs_suffix() {
        let mut q = tiny_question("abc_abs", "single-session-user");
        assert!(q.is_abstention());
        q.question_id = "abc".into();
        assert!(!q.is_abstention());
    }
}
