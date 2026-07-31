//! Memora benchmark harness for causal-memory.
//!
//! Ingests Memora persona conversations (data/<scale>/<persona>/conversations/
//! session_NNNN.json) into one causal-memory SQLite DB per persona, then
//! answers the persona's evaluation questions with BM25 retrieval + an
//! OpenAI-compatible LLM (DeepSeek by default), and scores the answers with
//! the official Memora FAMA protocol (Forgetting-Aware Memory Accuracy,
//! paper §4.2), ported from the official evals:
//!   memora/evals/agent_eval/memory_to_answer.py
//!     - fama_score()                    -> fama_score()
//!     - _evaluate_with_single_judge()   -> judge prompts + parse_judge_output()
//!     - answer_question() metric split  -> memory_presence / forgetting_absence
//!     - _generate_report() aggregation  -> summary (mean of per-question FAMA)
//!
//! FAMA per question:  FAMA = max(0, MPA - lambda * (1 - FAA))
//!   MPA    = memory_presence_correct / memory_presence_total
//!   FAA    = forgetting_absence_correct / forgetting_absence_total
//!   lambda = N_forget / (N_presence + N_forget)
//! Aggregate FAMA = mean over questions * 100 (official report behavior).
//!
//! Each evaluation sub-question is a yes/no probe over the generated answer:
//!   evaluation_type = "memory_presence"     (expected "yes": the answer MUST
//!                                            still mention the live fact)
//!   evaluation_type = "forgetting_absence"  (expected "no": the answer MUST
//!                                            NOT mention the deleted/outdated
//!                                            item — this is the "forgetting"
//!                                            dimension; answering with stale
//!                                            info loses points here)
//!
//! Usage:
//!   causal-memory-memora run --memora-root /path/to/memora --scale weekly \
//!       [--persona NAME] [--limit N] [--db-dir DIR] [--out DIR] \
//!       [--topk 10] [--concurrency 8] [--ingest raw|distill]
//!
//! Env:
//!   DEEPSEEK_API_KEY        (required; or CAUSAL_MEMORY_LLM_KEY)
//!   LOCOMO_LLM_API          (default: https://api.deepseek.com/v1)
//!   LOCOMO_LLM_MODEL        (default: deepseek-chat, used for answer + judge)
//!
//! DB layout: one DB per (scale, persona) at <db-dir>/<scale>/<persona>.db.
//! Chunk ids are `{persona}::{session_id}::{turn}` and every causal edge
//! carries `task_tag = persona`; retrieval goes through
//! `search_causal_bm25(Some(persona), ..)`. Ingest is idempotent per persona
//! (chunk-count match skips).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use causal_memory::distill::{Distiller, ItemKind};
use causal_memory::store::{CausalEntry, CausalStore};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

/// Edge metadata written between consecutive turns of opposite speakers.
const TURN_EDGE_RELATION: &str = "caused";
const TURN_EDGE_CONFIDENCE: f64 = 0.4;
const TURN_EDGE_DISCOVERED_BY: &str = "temporal";

/// LLM settings (temperature 0, same as the other benches).
const ANSWER_MAX_TOKENS: u32 = 500;
const JUDGE_MAX_TOKENS: u32 = 300;
const LLM_TEMPERATURE: f32 = 0.0;
const LLM_RETRIES: usize = 3;

/// The three Memora task types (paper §3.5); keys in the questions file are
/// lowercase, reports use these capitalized names (official TASK_TYPES).
const TASK_TYPES: [&str; 3] = ["Remembering", "Reasoning", "Recommending"];

/// Answer system prompt: the balanced-refusal + time-normalization base from
/// the LoCoMo/LongMemEval runs, adapted to the Memora personal-memory setting.
/// The latest-wins rule is ALWAYS on here (not just for a subtype): Memora's
/// forgetting dimension is scored by whether the answer avoids deleted /
/// outdated items, so preferring the newest dated memory is load-bearing.
const ANSWER_SYSTEM_PROMPT: &str = r#"You are a helpful AI assistant with access to a user's personal memory system, answering questions based ONLY on the user's stored memories.

Rules:
- Use ONLY the information from the provided memories; never make up facts that are not in the memories.
- Be specific and reference actual items/preferences from memory. If the question asks for recommendations, suggest based on similar items the user likes in memory, and avoid items the user dislikes or has already consumed.
- Each memory is prefixed with its session date, e.g. "[session_12 2025-06-03]". When the question contains relative time expressions ("this week", "last month", "recently"), resolve them against the memory dates and the Current Date given below, and answer with ABSOLUTE dates or periods.
- When information was updated over time (items added and later removed, plans changed, preferences revised), always answer with the most recent state (latest session date). Do NOT mention items that, according to the memories, were deleted, cancelled, completed, or otherwise are no longer current — unless the user explicitly asks about history.
- Memories may RECORD a removal or change as an event ("Removed X from the to-do list", "Cancelled/superseded: X", "no longer likes X", "prefers Y over X"). These mean X is NOT current: never include X when listing the user's current tasks, preferences, plans, or document fields, and do not mention X at all unless the user explicitly asks what changed.
- When a memory DIRECTLY addresses the question, you MUST answer — a short partial answer grounded in a memory is better than a refusal. Refuse ONLY when no memory states the requested fact."#;

/// Judge prompts, ported 1:1 from the official
/// evals/agent_eval/memory_to_answer.py `_evaluate_with_single_judge`.
const JUDGE_SYSTEM_PROMPT: &str = r#"You are an expert evaluator assessing AI assistant responses. Your task is to answer a YES/NO evaluation question about a given response.

You must provide your answer in the following JSON format:
{
    "answer": "yes" or "no",
    "confidence": 0.0 to 1.0,
    "explanation": "Brief explanation of your reasoning"
}

Be objective and thorough in your evaluation."#;

fn judge_user_prompt(generated_answer: &str, eval_question: &str) -> String {
    format!(
        "Please evaluate the following AI response against the evaluation question.\n\n\
         AI RESPONSE TO EVALUATE:\n{generated_answer}\n\n\
         EVALUATION QUESTION:\n{eval_question}\n\n\
         Provide your evaluation in JSON format with answer (yes/no), confidence (0.0-1.0), and explanation."
    )
}

fn usage() {
    eprintln!(
        "Usage: causal-memory-memora run --memora-root <memora repo> --scale <weekly|monthly|quarterly> [options]"
    );
    eprintln!();
    eprintln!("run options:");
    eprintln!("  --memora-root PATH  Memora repo root (contains data/<scale>/<persona>/)");
    eprintln!("  --scale NAME        weekly | monthly | quarterly (required)");
    eprintln!("  --persona NAME      only this persona (default: all 10)");
    eprintln!("  --limit N           max questions per persona (cost guard)");
    eprintln!("  --db-dir DIR        DB dir (default: benches/memora/db)");
    eprintln!("  --out DIR           results dir (default: benches/memora/results)");
    eprintln!("  --topk N            retrieved memories per question (default: 10)");
    eprintln!("  --concurrency N     parallel questions (default: 8)");
    eprintln!("  --ingest MODE       raw (default) | distill (LLM-distilled memory items;");
    eprintln!("                      hybrid: light sessions distill-only, heavy sessions");
    eprintln!("                      raw+distill dual write, separate *_distill.db)");
    eprintln!();
    eprintln!("Env: DEEPSEEK_API_KEY (required), LOCOMO_LLM_API, LOCOMO_LLM_MODEL");
    eprintln!();
    eprintln!(
        "Smoke test: causal-memory-memora run --memora-root /path/to/memora --scale weekly --persona software_engineer --limit 5"
    );
}

// ---------------------------------------------------------------------------
// Dataset model
// ---------------------------------------------------------------------------

/// One session_NNNN.json file.
#[derive(Debug, Deserialize)]
struct MemoraSession {
    session_id: u32,
    /// "add" | "delete" | "update" | ... — what the session did to the
    /// persona's state; null for `no_memory` chit-chat sessions (519 of the
    /// weekly files). Kept for provenance, not shown to the LLM.
    #[serde(default)]
    #[allow(dead_code)]
    operation: Option<String>,
    /// "YYYY-MM-DD" — the session date, the only timestamp Memora provides.
    date: String,
    #[serde(default)]
    conversation: Vec<MemoraTurn>,
}

#[derive(Debug, Clone, Deserialize)]
struct MemoraTurn {
    turn: u32,
    /// "user_agent" | "ai_agent".
    speaker: String,
    message: String,
    /// True on turns carrying the session's memory payload (official flag;
    /// we ingest all turns like the other benches and keep this for future
    /// evidence-recall analysis).
    #[serde(default)]
    #[allow(dead_code)]
    share_memory: bool,
}

/// evaluation_questions_<persona>.json.
#[derive(Debug, Deserialize)]
struct QuestionsFile {
    /// Persona name as recorded in the file (parsed for completeness; the
    /// directory name is the authoritative persona key).
    #[allow(dead_code)]
    persona: String,
    /// { "remembering": [...], "reasoning": [...], "recommending": [...] }
    questions: BTreeMap<String, Vec<MemoraQuestion>>,
}

#[derive(Debug, Clone, Deserialize)]
struct MemoraQuestion {
    question_id: String,
    question: String,
    /// "YYYY-MM-DD" — the "current date" for relative-time resolution.
    #[serde(default)]
    question_date: String,
    #[serde(default)]
    evaluation: EvaluationBlock,
    /// Stamped from the section key during loading (official
    /// _normalize_evaluation_questions does the same).
    #[serde(default)]
    task_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct EvaluationBlock {
    #[serde(default)]
    evaluation_questions: Vec<EvalQuestion>,
}

#[derive(Debug, Clone, Deserialize)]
struct EvalQuestion {
    evaluation_question_id: String,
    evaluation_question: String,
    /// "yes" | "no".
    expected_answer: String,
    /// "memory_presence" | "forgetting_absence".
    evaluation_type: String,
}

// ---------------------------------------------------------------------------
// Date handling
// ---------------------------------------------------------------------------

/// Parse a Memora "YYYY-MM-DD" date into a unix timestamp (midnight UTC).
fn parse_memora_date(s: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
}

/// Session base time; sessions without a parsable date fall back to a
/// synthetic epoch that keeps session-id ordering (one day apart).
fn session_base_time(session_id: u32, date: &str) -> i64 {
    const SYNTH_BASE_TS: i64 = 1_735_689_600; // 2025-01-01T00:00:00Z
    match parse_memora_date(date) {
        Some(ts) => ts,
        None => {
            eprintln!(
                "warn: session {session_id} date {date:?} unparsable, using synthetic timestamp"
            );
            SYNTH_BASE_TS + session_id as i64 * 86_400
        }
    }
}

/// Chunk id for one turn: `{persona}::{session_id}::{turn}` (turn as given in
/// the file, 1-based).
fn chunk_id(persona: &str, session_id: u32, turn: u32) -> String {
    format!("{persona}::{session_id:04}::{turn}")
}

/// Chunk text for a single turn: "[session_N <date>] speaker: message".
/// Speakers are normalized to user/assistant for readability.
fn turn_chunk_text(session_id: u32, date: &str, speaker: &str, message: &str) -> String {
    let role = match speaker {
        "user_agent" => "user",
        "ai_agent" => "assistant",
        other => other,
    };
    format!(
        "[session_{session_id} {}] {role}: {}",
        date.trim(),
        message.trim()
    )
}

// ---------------------------------------------------------------------------
// Loading data from the Memora repo layout
// ---------------------------------------------------------------------------

fn conversations_dir(root: &Path, scale: &str, persona: &str) -> PathBuf {
    root.join("data")
        .join(scale)
        .join(persona)
        .join("conversations")
}

fn questions_path(root: &Path, scale: &str, persona: &str) -> PathBuf {
    root.join("data")
        .join(scale)
        .join(persona)
        .join(format!("evaluation_questions_{persona}.json"))
}

/// Load all sessions of a persona, sorted by session_id.
fn load_sessions(root: &Path, scale: &str, persona: &str) -> Result<Vec<MemoraSession>> {
    let dir = conversations_dir(root, scale, persona);
    let mut sessions = Vec::new();
    let entries = std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name.starts_with("session_") && name.ends_with(".json")) {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let session: MemoraSession =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        sessions.push(session);
    }
    sessions.sort_by_key(|s| s.session_id);
    if sessions.is_empty() {
        anyhow::bail!("no session_*.json found in {}", dir.display());
    }
    Ok(sessions)
}

/// Load and normalize the questions file: flattens the three task sections
/// into one list with `task_type` stamped (official normalize step), keeping
/// the file's per-section order (remembering, reasoning, recommending as they
/// appear; BTreeMap iterates alphabetically, so we order by TASK_TYPES).
fn load_questions(root: &Path, scale: &str, persona: &str) -> Result<Vec<MemoraQuestion>> {
    let path = questions_path(root, scale, persona);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: QuestionsFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let mut out = Vec::new();
    for task in TASK_TYPES {
        if let Some(qs) = file.questions.get(&task.to_lowercase()) {
            for q in qs {
                let mut q = q.clone();
                q.task_type = task.to_string();
                out.push(q);
            }
        }
    }
    // Any unexpected section keys still get included (defensive).
    for (key, qs) in &file.questions {
        if !TASK_TYPES.iter().any(|t| t.eq_ignore_ascii_case(key)) {
            for q in qs {
                let mut q = q.clone();
                q.task_type = key.clone();
                out.push(q);
            }
        }
    }
    Ok(out)
}

/// List all personas available under data/<scale>/ (dirs that contain both
/// conversations/ and an evaluation_questions file).
fn list_personas(root: &Path, scale: &str) -> Result<Vec<String>> {
    let dir = root.join("data").join(scale);
    let mut personas = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if conversations_dir(root, scale, name).is_dir()
            && questions_path(root, scale, name).is_file()
        {
            personas.push(name.to_string());
        }
    }
    personas.sort();
    Ok(personas)
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/// Ingest one persona's sessions into its dedicated store.
///
/// Each turn becomes one chunk keyed `{persona}::{session_id:04}::{turn}`;
/// consecutive turns of opposite speakers are linked with a low-confidence
/// `caused` edge (temporal discovery) tagged `task_tag = persona`.
///
/// Idempotent: if the store already holds exactly the expected chunk count,
/// ingestion is skipped; on a partial/stale state the persona's own chunks
/// and edges are wiped and re-ingested.
fn ingest_persona(store: &CausalStore, persona: &str, sessions: &[MemoraSession]) -> Result<usize> {
    let prefix = format!("{persona}::");
    let expected_chunks: usize = sessions.iter().map(|s| s.conversation.len()).sum();

    // substr() instead of LIKE: persona names contain '_', a LIKE wildcard.
    let existing: i64 = store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM chunks WHERE substr(id, 1, ?1) = ?2",
            rusqlite::params![prefix.len() as i64, &prefix],
            |r| r.get(0),
        )?)
    })?;
    if existing == expected_chunks as i64 && expected_chunks > 0 {
        return Ok(expected_chunks);
    }
    if existing > 0 {
        eprintln!(
            "warn: persona {persona} has {existing} chunks, expected {expected_chunks}; re-ingesting"
        );
        store.with_conn(|c| {
            c.execute(
                "DELETE FROM causal_edges WHERE task_tag = ?1",
                rusqlite::params![persona],
            )?;
            c.execute(
                "DELETE FROM chunks WHERE substr(id, 1, ?1) = ?2",
                rusqlite::params![prefix.len() as i64, &prefix],
            )?;
            Ok(())
        })?;
    }

    let mut written = 0usize;
    for session in sessions {
        written += ingest_session_raw(store, persona, session)?;
    }
    Ok(written)
}

/// Raw-ingest one session: each turn becomes one chunk keyed
/// `{persona}::{session_id:04}::{turn}`; consecutive turns of opposite
/// speakers are linked with a low-confidence `caused` edge (temporal
/// discovery) tagged `task_tag = persona`. Returns chunks written.
fn ingest_session_raw(
    store: &CausalStore,
    persona: &str,
    session: &MemoraSession,
) -> Result<usize> {
    let mut written = 0usize;
    let base = session_base_time(session.session_id, &session.date);
    for (t_idx, turn) in session.conversation.iter().enumerate() {
        let ts = base + t_idx as i64; // +1s per turn keeps intra-session order
        let id = chunk_id(persona, session.session_id, turn.turn);
        let text = turn_chunk_text(
            session.session_id,
            &session.date,
            &turn.speaker,
            &turn.message,
        );
        // INSERT OR IGNORE returns 1 only when the chunk was newly written;
        // on a redo of an interrupted persona (distill_done marker absent)
        // existing chunks are skipped and their turn edges must NOT be
        // duplicated, or BM25 evidence would double-count.
        let inserted = store.with_conn(|c| {
            Ok(c.execute(
                "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![&id, &text, ts],
            )?)
        })?;
        if inserted == 0 {
            continue;
        }

        // Link each turn to the nearest preceding turn from the OTHER
        // speaker (the turn it responds to); first turn gets no edge.
        let prev_idx = session.conversation[..t_idx]
            .iter()
            .rposition(|t| t.speaker != turn.speaker);
        if let Some(prev_idx) = prev_idx {
            let prev_id = chunk_id(
                persona,
                session.session_id,
                session.conversation[prev_idx].turn,
            );
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
                        persona
                    ],
                )?;
                Ok(())
            })?;
        }
        written += 1;
    }
    Ok(written)
}

/// Ingest statistics for the distill pipeline (written into the run summary
/// so the cost of distillation is auditable).
#[derive(Debug, Default, Clone, Serialize)]
struct DistillIngestStats {
    sessions: usize,
    sessions_distilled: usize,
    /// Heavy sessions dual-written: raw turns AND distilled items.
    sessions_dual_write: usize,
    /// Light sessions the distiller judged memory-free; nothing was written
    /// for them (round-1 dumped their raw turns into the DB, diluting BM25).
    sessions_light_empty: usize,
    /// Sessions where the LLM call failed; these were raw-ingested instead
    /// so no data is lost.
    sessions_fallback_raw: usize,
    items_recorded: usize,
    items_duplicate: usize,
    /// Fact/Preference items routed to the fact layer (scope = "user" in this
    /// persona's own DB), mirroring the LongMemEval harness.
    facts_recorded: usize,
    /// Older same-key facts retired via supersedes-hint matching.
    facts_retired: usize,
    /// Old edges soft-invalidated via supersedes matching.
    superseded_invalidations: usize,
    raw_chunks_written: usize,
    llm_calls: usize,
    /// True when a pre-existing distill DB was detected and ingest skipped.
    skipped_existing: bool,
}

/// Hybrid-ingest thresholds (round-2 fix, both tunable):
/// - a session whose WHOLE conversation is shorter than this many chars is
///   "light" (chit-chat): distill-only, and an empty distillation writes
///   NOTHING — round 1 raw-fell-back 79 such sessions (1,301 noise chunks)
///   and BM25 for the 125 real distilled items drowned in them.
const LIGHT_SESSION_TOTAL_CHARS: usize = 1500;
/// - ...or whose average turn is shorter than this many chars. Catches
///   many-turn small talk that clears the total-length bar.
const LIGHT_SESSION_AVG_TURN_CHARS: usize = 80;

/// Light (chit-chat) vs heavy (content) session classification for hybrid
/// ingest. Light => distill only; heavy => raw + distill dual write (raw
/// keeps the quantitative detail distillation would compress away, distill
/// keeps a clean retrieval entry point).
fn is_light_session(session: &MemoraSession) -> bool {
    let total: usize = session.conversation.iter().map(|t| t.message.len()).sum();
    if total < LIGHT_SESSION_TOTAL_CHARS {
        return true;
    }
    let avg = total
        .checked_div(session.conversation.len().max(1))
        .unwrap_or(0);
    avg < LIGHT_SESSION_AVG_TURN_CHARS
}

/// Distill-mode ingest: every session is distilled by the LLM into dated
/// memory items (concurrency-limited, but recorded strictly in session
/// order so a later session's `supersedes` always sees the earlier items).
///
/// Hybrid routing (round-2): LIGHT sessions (chit-chat) are distill-only —
/// an empty distillation is the correct outcome and writes nothing, so raw
/// noise no longer dilutes BM25. HEAVY sessions (long/detailed) are
/// dual-written: raw turns (numbers and lists survive verbatim) plus the
/// distilled items (clean retrieval entry points). LLM FAILURE always falls
/// back to raw for that session — no conversation data is ever dropped.
/// When no Distiller is configured at all, the whole persona falls back to
/// raw.
///
/// Idempotent at the bench level: any pre-existing distill edges for this
/// persona skip ingest entirely (item-level idempotency lives in
/// `record_distilled`).
async fn ingest_persona_distill(
    store: &CausalStore,
    distiller: Option<&Distiller>,
    persona: &str,
    sessions: &[MemoraSession],
    concurrency: usize,
) -> Result<DistillIngestStats> {
    let mut stats = DistillIngestStats {
        sessions: sessions.len(),
        ..Default::default()
    };

    // Idempotent at the persona level via the `distill_done` marker table,
    // mirroring the LongMemEval harness. The old check ("has any distill
    // edge") broke once Fact/Preference items route to the fact layer: a
    // persona whose items are all facts writes ZERO distill edges and would
    // be re-distilled forever. A persona interrupted mid-record has no
    // marker and is redone cleanly (item-level idempotency lives in
    // record_distilled / record_fact).
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
            rusqlite::params![persona],
            |r| r.get(0),
        )?)
    })?;
    if existing > 0 {
        stats.skipped_existing = true;
        return Ok(stats);
    }

    let Some(distiller) = distiller else {
        eprintln!(
            "warn: no Distiller configured (DEEPSEEK_API_KEY unset); raw-ingesting {persona}"
        );
        stats.raw_chunks_written = ingest_persona(store, persona, sessions)?;
        stats.sessions_fallback_raw = sessions.len();
        return Ok(stats);
    };

    // Distill sessions with bounded concurrency; `buffered` keeps results in
    // session order for the sequential record phase below.
    let futures = sessions.iter().map(|session| {
        let turns: Vec<(String, String)> = session
            .conversation
            .iter()
            .map(|t| {
                let role = match t.speaker.as_str() {
                    "user_agent" => "user",
                    "ai_agent" => "assistant",
                    other => other,
                };
                (role.to_string(), t.message.clone())
            })
            .collect();
        let date = session.date.clone();
        async move { (distiller.distill_session(&date, &turns).await, 1usize) }
    });
    let results: Vec<(Result<Vec<causal_memory::distill::MemoryItem>>, usize)> =
        futures::stream::iter(futures)
            .buffered(concurrency)
            .collect()
            .await;
    stats.llm_calls = results.iter().map(|(_, n)| n).sum();

    // Record strictly in session order.
    for (session, (result, _)) in sessions.iter().zip(results) {
        let light = is_light_session(session);
        match result {
            Ok(items) if !items.is_empty() => {
                if !light {
                    // Heavy session: dual write. Raw turns preserve the
                    // quantitative detail (totals, prices, lists) that
                    // distillation compresses away; distilled items stay
                    // the clean retrieval entry points.
                    stats.sessions_dual_write += 1;
                    stats.raw_chunks_written += ingest_session_raw(store, persona, session)?;
                }
                stats.sessions_distilled += 1;
                for it in &items {
                    match it.kind {
                        // Fact/Preference → the fact layer. Scope "user"
                        // suffices: each persona gets its OWN distill DB
                        // (see db_name in main), so per-persona isolation is
                        // physical, not scope-based like the LME harness.
                        ItemKind::Fact | ItemKind::Preference => {
                            let kind = match it.kind {
                                ItemKind::Fact => "fact",
                                ItemKind::Preference => "preference",
                                _ => unreachable!(),
                            };
                            // Retire BEFORE record: the new value often
                            // shares topic tokens with its own supersedes
                            // hint ("now prefers classical over jazz" vs
                            // hint "likes jazz") and retire_facts_by_hint
                            // has no self-exclusion — recording first can
                            // retire the fact we just wrote (found by review;
                            // masked in small corpora where shared tokens get
                            // IDF 0). Retiring first removes that window;
                            // record_fact's upsert still revives an identical
                            // re-recorded value afterwards.
                            if let Some(hint) = it.supersedes.as_deref() {
                                match store.retire_facts_by_hint(kind, "user", hint) {
                                    Ok(n) => stats.facts_retired += n,
                                    Err(e) => eprintln!(
                                        "warn: retire_facts_by_hint failed for {persona} ({e}); stale fact may stay live"
                                    ),
                                }
                            }
                            store.record_fact(kind, &it.text, "user", "distill", 0.8)?;
                            stats.facts_recorded += 1;
                        }
                        ItemKind::Lesson | ItemKind::Event => {
                            let out = store.record_distilled(it, Some(persona))?;
                            if out.duplicate {
                                stats.items_duplicate += 1;
                            } else {
                                stats.items_recorded += 1;
                            }
                            stats.superseded_invalidations += out.invalidated_edge_ids.len();
                        }
                    }
                }
            }
            Ok(_) if light => {
                // Chit-chat the distiller judged memory-free: that IS the
                // correct outcome. Writing the raw turns anyway (round-1
                // behavior) dumped ~1.3k noise chunks into the DB and
                // diluted BM25 for the real items.
                stats.sessions_light_empty += 1;
            }
            Ok(_) => {
                // Heavy session with an empty distillation: keep the raw
                // turns (evaluation questions can hinge on details the
                // distiller dropped).
                stats.raw_chunks_written += ingest_session_raw(store, persona, session)?;
            }
            Err(e) => {
                eprintln!(
                    "warn: distill failed for session {} ({e}); raw-ingesting it",
                    session.session_id
                );
                stats.sessions_fallback_raw += 1;
                stats.raw_chunks_written += ingest_session_raw(store, persona, session)?;
            }
        }
    }

    // Completion marker: only written after ALL sessions were processed.
    // EXCEPTION mirroring the LME harness: if EVERY session's LLM call
    // failed (rate-limit burst, API outage, balance exhausted), the persona
    // produced nothing through no fault of its own — writing the marker
    // would freeze it as "successfully empty" (found the hard way on LME:
    // 133 questions marked with zero data during a 429 storm).
    if stats.sessions_fallback_raw == stats.sessions && stats.sessions > 0 {
        eprintln!(
            "warn: ALL {} sessions of {persona} failed; NOT marking done (retry next run)",
            stats.sessions
        );
        return Ok(stats);
    }
    store.with_conn(|c| {
        c.execute(
            "INSERT OR REPLACE INTO distill_done (qid, done_at) VALUES (?1, ?2)",
            rusqlite::params![persona, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    })?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

/// Retrieve candidate causal entries for a question (BM25), scoped to this
/// persona via the task_tag edge filter.
fn retrieve(
    store: &CausalStore,
    persona: &str,
    question: &str,
    topk: usize,
) -> Result<Vec<CausalEntry>> {
    store.search_causal_bm25(Some(persona), question, topk)
}

/// History-intent keywords: when the question explicitly asks about the
/// past or about changes, retraction records ("Removed X", "no longer
/// likes X", "Cancelled/superseded: X") are exactly the evidence needed
/// and must stay in the prompt.
const HISTORY_INTENT_MARKERS: [&str; 11] = [
    "what changed",
    "history",
    "previously",
    "used to",
    "removed",
    "deleted",
    "cancelled",
    "canceled",
    "anymore",
    "no longer",
    "before",
];

/// True when the question asks about history/changes rather than current
/// state.
fn asks_about_history(question: &str) -> bool {
    let lower = question.to_lowercase();
    HISTORY_INTENT_MARKERS.iter().any(|m| lower.contains(m))
}

/// Memory lines for the answer prompt, deduplicated by chunk id.
///
/// Round-2c FAA fix: for current-state questions, retraction RECORDS
/// ("Removed X from the list", "User no longer likes X", negation
/// memories) are filtered out. The Memora forgetting judge counts ANY
/// mention of a deleted item — even "X was removed on 06-03" — as a
/// failure, and deepseek kept volunteering those records whenever they
/// were in context. They remain stored and retrievable; they are only
/// withheld from the answer prompt unless the question asks about history.
fn memory_lines(entries: &[CausalEntry], question: &str) -> String {
    let history = asks_about_history(question);
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for e in entries {
        for (id, text) in [
            (&e.decision_id, &e.decision_text),
            (&e.outcome_id, &e.outcome_text),
        ] {
            if seen.insert(id.clone())
                && (history || !causal_memory::store::is_retraction_record(text))
            {
                lines.push(format!("- {text}"));
            }
        }
    }
    lines.join("\n")
}

/// Answer user prompt: memories + question_date as the "current time"
/// reference (mirrors the other benches' Current Date field).
fn answer_user_prompt(q: &MemoraQuestion, memories: &str) -> String {
    let memories = if memories.is_empty() {
        "(no memories retrieved)"
    } else {
        memories
    };
    format!(
        "Current Date: {}\n\nUser's Relevant Memories:\n{memories}\n\nUser's Question: {}\n\nPlease provide a helpful answer based on these memories.",
        q.question_date, q.question
    )
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

#[derive(Serialize)]
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
// FAMA scoring (ported from official memory_to_answer.py)
// ---------------------------------------------------------------------------

/// Forgetting-Aware Memory Accuracy (paper §4.2), ported 1:1 from the
/// official `fama_score` in evals/agent_eval/memory_to_answer.py:
///
///   FAMA = max(0, MPA - lambda * (1 - FAA))
///   MPA  = memory_presence_correct / memory_presence_total
///   FAA  = forgetting_absence_correct / forgetting_absence_total
///   lambda = N_forget / (N_presence + N_forget)
///
/// Per-question FAMA in [0, 1].
fn fama_score(
    memory_presence_correct: usize,
    memory_presence_total: usize,
    forgetting_absence_correct: usize,
    forgetting_absence_total: usize,
) -> f64 {
    let n_p = memory_presence_total as f64;
    let n_f = forgetting_absence_total as f64;
    if n_p == 0.0 && n_f == 0.0 {
        return 0.0;
    }
    let mpa = if n_p > 0.0 {
        memory_presence_correct as f64 / n_p
    } else {
        0.0
    };
    let faa = if n_f > 0.0 {
        forgetting_absence_correct as f64 / n_f
    } else {
        1.0
    };
    let lam = if (n_p + n_f) > 0.0 {
        n_f / (n_p + n_f)
    } else {
        0.0
    };
    (mpa - lam * (1.0 - faa)).max(0.0)
}

/// Parsed judge output for one evaluation sub-question.
#[derive(Debug, Clone, PartialEq)]
struct JudgeVerdict {
    /// "yes" | "no" | "unclear" | "error".
    llm_answer: String,
    /// Official rule: `is_correct = (llm_answer == expected_answer)`.
    is_correct: bool,
    explanation: String,
}

/// Parse the judge's JSON reply, ported from the official
/// `_evaluate_with_single_judge`: strip markdown fences, json.loads, normalize
/// the answer to yes/no (falling back to scanning the explanation), and on a
/// JSON decode error fall back to scanning the raw text for yes/no.
fn parse_judge_output(raw: &str, expected_answer: &str) -> JudgeVerdict {
    let expected = expected_answer.to_lowercase();
    let infer_from_text = |text: &str| -> String {
        let t = text.to_lowercase();
        let has_yes = t.contains("yes");
        let has_no = t.contains("no");
        match (has_yes, has_no) {
            (true, false) => "yes".into(),
            (false, true) => "no".into(),
            _ => "unclear".into(),
        }
    };

    // Remove markdown code fences if present (official behavior).
    let mut text = raw.trim().to_string();
    if text.starts_with("```") {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > 2 {
            text = lines[1..lines.len() - 1].join("\n");
        }
        text = text
            .replace("```json", "")
            .replace("```", "")
            .trim()
            .to_string();
    }

    let (llm_answer, explanation) = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => {
            let answer = v
                .get("answer")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_lowercase()
                .trim()
                .to_string();
            let explanation = v
                .get("explanation")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string();
            let answer = match answer.as_str() {
                "yes" | "no" => answer,
                // Official fallback: infer from the explanation text.
                _ => infer_from_text(&explanation),
            };
            (answer, explanation)
        }
        // Official JSONDecodeError fallback: scan the raw reply text.
        Err(_) => (
            infer_from_text(raw),
            format!(
                "parse error, inferred from text: {}",
                &raw[..raw.len().min(200)]
            ),
        ),
    };

    let is_correct = llm_answer == expected;
    JudgeVerdict {
        llm_answer,
        is_correct,
        explanation,
    }
}

// ---------------------------------------------------------------------------
// Per-question evaluation
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct EvalResultRow {
    evaluation_question_id: String,
    evaluation_question: String,
    expected_answer: String,
    evaluation_type: String,
    llm_answer: String,
    is_correct: bool,
    explanation: String,
}

#[derive(Serialize)]
struct ResultRow {
    question_id: String,
    task_type: String,
    question: String,
    question_date: String,
    model_response: String,
    memories_retrieved: usize,
    evaluation_results: Vec<EvalResultRow>,
    fama: f64,
    memory_presence_correct: usize,
    memory_presence_total: usize,
    memory_presence_accuracy: Option<f64>,
    forgetting_absence_correct: usize,
    forgetting_absence_total: usize,
    forgetting_absence_accuracy: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Answer one question and judge every evaluation sub-question.
async fn answer_question(
    cfg: &LlmConfig,
    store: &CausalStore,
    persona: &str,
    q: &MemoraQuestion,
    topk: usize,
    with_facts: bool,
) -> ResultRow {
    let retrieved = retrieve(store, persona, &q.question, topk).unwrap_or_default();
    // Distill mode additionally queries the fact layer (BM25, scope "user" —
    // this persona's own DB, same topk) and puts fact lines FIRST: they are
    // the high-precision layer for the factual-recall slice. Retired facts
    // (superseded) are excluded by search_facts_bm25, which is exactly the
    // semantics the forgetting (FAA) evaluation wants: the model never sees
    // the outdated value. Mirrors the LongMemEval harness.
    let memories = if with_facts {
        let facts = store
            .search_facts_bm25(&q.question, Some("user"), topk)
            .unwrap_or_default();
        let mut lines: Vec<String> = facts.iter().map(|f| format!("- {}", f.value)).collect();
        let causal = memory_lines(&retrieved, &q.question);
        if !causal.is_empty() {
            lines.push(causal);
        }
        lines.join("\n")
    } else {
        memory_lines(&retrieved, &q.question)
    };
    let memories_retrieved = retrieved.len();

    let base_row = |model_response: String, error: Option<String>| ResultRow {
        question_id: q.question_id.clone(),
        task_type: q.task_type.clone(),
        question: q.question.clone(),
        question_date: q.question_date.clone(),
        model_response,
        memories_retrieved,
        evaluation_results: Vec::new(),
        fama: 0.0,
        memory_presence_correct: 0,
        memory_presence_total: 0,
        memory_presence_accuracy: None,
        forgetting_absence_correct: 0,
        forgetting_absence_total: 0,
        forgetting_absence_accuracy: None,
        error,
    };

    let answer_user = answer_user_prompt(q, &memories);
    let predicted = match chat(cfg, ANSWER_SYSTEM_PROMPT, &answer_user, ANSWER_MAX_TOKENS).await {
        Ok(s) => s,
        Err(e) => return base_row(String::new(), Some(format!("answer LLM failed: {e}"))),
    };

    // Judge each evaluation sub-question (official: one judge call per eval
    // question; single judge here instead of the official 3-judge majority —
    // see summary metadata note).
    let mut eval_rows = Vec::new();
    for eq in &q.evaluation.evaluation_questions {
        let judge_user = judge_user_prompt(&predicted, &eq.evaluation_question);
        let verdict = match chat(cfg, JUDGE_SYSTEM_PROMPT, &judge_user, JUDGE_MAX_TOKENS).await {
            Ok(raw) => parse_judge_output(&raw, &eq.expected_answer),
            Err(e) => JudgeVerdict {
                llm_answer: "error".into(),
                is_correct: false,
                explanation: format!("judge LLM failed: {e}"),
            },
        };
        eval_rows.push(EvalResultRow {
            evaluation_question_id: eq.evaluation_question_id.clone(),
            evaluation_question: eq.evaluation_question.clone(),
            expected_answer: eq.expected_answer.clone(),
            evaluation_type: eq.evaluation_type.clone(),
            llm_answer: verdict.llm_answer,
            is_correct: verdict.is_correct,
            explanation: verdict.explanation,
        });
    }

    // Official metric split (answer_question in memory_to_answer.py).
    let mut mp_total = 0usize;
    let mut mp_correct = 0usize;
    let mut fa_total = 0usize;
    let mut fa_correct = 0usize;
    for row in &eval_rows {
        match row.evaluation_type.as_str() {
            "memory_presence" => {
                mp_total += 1;
                if row.is_correct {
                    mp_correct += 1;
                }
            }
            "forgetting_absence" => {
                fa_total += 1;
                if row.is_correct {
                    fa_correct += 1;
                }
            }
            other => eprintln!(
                "warn: unknown evaluation_type {other:?} in {}",
                q.question_id
            ),
        }
    }

    ResultRow {
        fama: fama_score(mp_correct, mp_total, fa_correct, fa_total),
        memory_presence_correct: mp_correct,
        memory_presence_total: mp_total,
        memory_presence_accuracy: (mp_total > 0).then_some(mp_correct as f64 / mp_total as f64),
        forgetting_absence_correct: fa_correct,
        forgetting_absence_total: fa_total,
        forgetting_absence_accuracy: (fa_total > 0).then_some(fa_correct as f64 / fa_total as f64),
        evaluation_results: eval_rows,
        ..base_row(predicted, None)
    }
}

// ---------------------------------------------------------------------------
// Summary (aggregation ported from official _generate_report)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TaskTypeStats {
    total_questions: usize,
    total_eval_questions: usize,
    correct_count: usize,
    accuracy: f64,
    memory_presence_total: usize,
    memory_presence_correct: usize,
    memory_presence_accuracy: Option<f64>,
    forgetting_absence_total: usize,
    forgetting_absence_correct: usize,
    forgetting_absence_accuracy: Option<f64>,
    /// Mean of per-question FAMA * 100 (official report behavior).
    fama: f64,
}

#[derive(Serialize)]
struct PersonaSummary {
    persona: String,
    total_questions: usize,
    total_eval_questions: usize,
    overall_accuracy: f64,
    fama: f64,
    memory_presence_total: usize,
    memory_presence_correct: usize,
    memory_presence_accuracy: Option<f64>,
    forgetting_absence_total: usize,
    forgetting_absence_correct: usize,
    forgetting_absence_accuracy: Option<f64>,
    by_task_type: BTreeMap<String, TaskTypeStats>,
    /// Distill-mode ingest statistics (None for raw ingest).
    #[serde(skip_serializing_if = "Option::is_none")]
    distill_ingest: Option<DistillIngestStats>,
}

#[derive(Serialize)]
struct Summary {
    run_id: String,
    date: String,
    git_commit: String,
    scale: String,
    model: String,
    judge_model: String,
    temperature: f32,
    topk: usize,
    /// "raw" | "distill" — how sessions were ingested.
    ingest: String,
    memora_root: String,
    /// Protocol note: the published Table 3 numbers use a 3-judge majority
    /// vote (openai/gpt-4.1, anthropic/claude-haiku-4.5, google/gemini-2.5-flash
    /// via OpenRouter); this run uses a single deepseek-chat judge and is
    /// therefore NOT directly comparable to Table 3.
    judge_protocol: String,
    personas: Vec<PersonaSummary>,
}

fn aggregate_persona(persona: &str, rows: &[ResultRow]) -> PersonaSummary {
    let mut mp_total = 0usize;
    let mut mp_correct = 0usize;
    let mut fa_total = 0usize;
    let mut fa_correct = 0usize;
    let mut famas = Vec::new();
    let mut by_task: BTreeMap<String, Vec<&ResultRow>> = BTreeMap::new();

    for row in rows {
        mp_total += row.memory_presence_total;
        mp_correct += row.memory_presence_correct;
        fa_total += row.forgetting_absence_total;
        fa_correct += row.forgetting_absence_correct;
        famas.push(row.fama);
        by_task.entry(row.task_type.clone()).or_default().push(row);
    }

    let task_stats = |rows: &[&ResultRow]| -> TaskTypeStats {
        let t_mp_total: usize = rows.iter().map(|r| r.memory_presence_total).sum();
        let t_mp_correct: usize = rows.iter().map(|r| r.memory_presence_correct).sum();
        let t_fa_total: usize = rows.iter().map(|r| r.forgetting_absence_total).sum();
        let t_fa_correct: usize = rows.iter().map(|r| r.forgetting_absence_correct).sum();
        let total_eval: usize = rows.iter().map(|r| r.evaluation_results.len()).sum();
        let correct: usize = t_mp_correct + t_fa_correct;
        let fama = if rows.is_empty() {
            0.0
        } else {
            rows.iter().map(|r| r.fama).sum::<f64>() / rows.len() as f64 * 100.0
        };
        TaskTypeStats {
            total_questions: rows.len(),
            total_eval_questions: total_eval,
            correct_count: correct,
            accuracy: if total_eval > 0 {
                correct as f64 / total_eval as f64
            } else {
                0.0
            },
            memory_presence_total: t_mp_total,
            memory_presence_correct: t_mp_correct,
            memory_presence_accuracy: (t_mp_total > 0)
                .then_some(t_mp_correct as f64 / t_mp_total as f64),
            forgetting_absence_total: t_fa_total,
            forgetting_absence_correct: t_fa_correct,
            forgetting_absence_accuracy: (t_fa_total > 0)
                .then_some(t_fa_correct as f64 / t_fa_total as f64),
            fama,
        }
    };

    let total_eval = mp_total + fa_total;
    let total_correct = mp_correct + fa_correct;
    PersonaSummary {
        persona: persona.to_string(),
        total_questions: rows.len(),
        total_eval_questions: total_eval,
        overall_accuracy: if total_eval > 0 {
            total_correct as f64 / total_eval as f64
        } else {
            0.0
        },
        fama: if famas.is_empty() {
            0.0
        } else {
            famas.iter().sum::<f64>() / famas.len() as f64 * 100.0
        },
        memory_presence_total: mp_total,
        memory_presence_correct: mp_correct,
        memory_presence_accuracy: (mp_total > 0).then_some(mp_correct as f64 / mp_total as f64),
        forgetting_absence_total: fa_total,
        forgetting_absence_correct: fa_correct,
        forgetting_absence_accuracy: (fa_total > 0).then_some(fa_correct as f64 / fa_total as f64),
        by_task_type: by_task
            .iter()
            .map(|(t, rs)| (t.clone(), task_stats(rs)))
            .collect(),
        distill_ingest: None,
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

// ---------------------------------------------------------------------------
// CLI / run
// ---------------------------------------------------------------------------

struct Args {
    memora_root: PathBuf,
    scale: String,
    persona: Option<String>,
    limit: Option<usize>,
    db_dir: PathBuf,
    out_dir: PathBuf,
    topk: usize,
    concurrency: usize,
    /// "raw" (default) = every turn becomes a chunk; "distill" = LLM
    /// distillation per session via `causal_memory::distill`.
    ingest: String,
}

fn parse_args(argv: &[String]) -> Result<Option<Args>> {
    if argv.is_empty() || argv.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(None);
    }
    if argv[0] != "run" {
        anyhow::bail!("unknown subcommand {:?}; expected `run`", argv[0]);
    }
    let mut memora_root: Option<PathBuf> = None;
    let mut scale: Option<String> = None;
    let mut persona: Option<String> = None;
    let mut limit = None;
    let mut db_dir = PathBuf::from("benches/memora/db");
    let mut out_dir = PathBuf::from("benches/memora/results");
    let mut topk = 10usize;
    let mut concurrency = 8usize;
    let mut ingest = "raw".to_string();

    let mut i = 1;
    let take = |i: &mut usize, flag: &str| -> Result<String> {
        *i += 1;
        argv.get(*i)
            .cloned()
            .ok_or_else(|| anyhow!("missing value for {flag}"))
    };
    while i < argv.len() {
        match argv[i].as_str() {
            "--memora-root" => memora_root = Some(PathBuf::from(take(&mut i, "--memora-root")?)),
            "--scale" => scale = Some(take(&mut i, "--scale")?),
            "--persona" => persona = Some(take(&mut i, "--persona")?),
            "--limit" => limit = Some(take(&mut i, "--limit")?.parse()?),
            "--db-dir" => db_dir = PathBuf::from(take(&mut i, "--db-dir")?),
            "--out" => out_dir = PathBuf::from(take(&mut i, "--out")?),
            "--topk" => topk = take(&mut i, "--topk")?.parse()?,
            "--concurrency" => concurrency = take(&mut i, "--concurrency")?.parse()?,
            "--ingest" => ingest = take(&mut i, "--ingest")?,
            other => anyhow::bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    let memora_root = memora_root.ok_or_else(|| anyhow!("--memora-root is required"))?;
    let scale = scale.ok_or_else(|| anyhow!("--scale is required"))?;
    if !["weekly", "monthly", "quarterly"].contains(&scale.as_str()) {
        anyhow::bail!("--scale must be weekly|monthly|quarterly, got {scale:?}");
    }
    if !["raw", "distill"].contains(&ingest.as_str()) {
        anyhow::bail!("--ingest must be raw|distill, got {ingest:?}");
    }
    Ok(Some(Args {
        memora_root,
        scale,
        persona,
        limit,
        db_dir,
        out_dir,
        topk,
        concurrency,
        ingest,
    }))
}

async fn run_persona(
    cfg: &LlmConfig,
    distiller: Option<&Distiller>,
    args: &Args,
    persona: &str,
    run_id: &str,
) -> Result<PersonaSummary> {
    let sessions = load_sessions(&args.memora_root, &args.scale, persona)?;
    let questions = load_questions(&args.memora_root, &args.scale, persona)?;
    let selected: Vec<&MemoraQuestion> = questions
        .iter()
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();
    eprintln!(
        "persona {persona}: {} sessions, {} questions (selected {})",
        sessions.len(),
        questions.len(),
        selected.len()
    );

    // Distill runs get their own DB file so the raw baseline DB is never
    // polluted and the two modes can be compared side by side.
    let db_name = match args.ingest.as_str() {
        "distill" => format!("{persona}_distill.db"),
        _ => format!("{persona}.db"),
    };
    let db_path = args.db_dir.join(&args.scale).join(db_name);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store =
        CausalStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let mut distill_stats = None;
    match args.ingest.as_str() {
        "distill" => {
            let stats =
                ingest_persona_distill(&store, distiller, persona, &sessions, args.concurrency)
                    .await
                    .with_context(|| format!("distill-ingesting persona {persona}"))?;
            eprintln!(
                "ingest {persona} (distill): {}/{} sessions distilled ({} dual-write, \
                 {} light-empty dropped), {} fallback-raw, \
                 {} items (+{} dup), {} facts ({} retired), {} superseded, {} LLM calls{}",
                stats.sessions_distilled,
                stats.sessions,
                stats.sessions_dual_write,
                stats.sessions_light_empty,
                stats.sessions_fallback_raw,
                stats.items_recorded,
                stats.items_duplicate,
                stats.facts_recorded,
                stats.facts_retired,
                stats.superseded_invalidations,
                stats.llm_calls,
                if stats.skipped_existing {
                    " [skipped: existing distill DB]"
                } else {
                    ""
                },
            );
            distill_stats = Some(stats);
        }
        _ => {
            let n = ingest_persona(&store, persona, &sessions)
                .with_context(|| format!("ingesting persona {persona}"))?;
            eprintln!("ingest {persona}: {n} chunks");
        }
    }

    let done = Arc::new(AtomicUsize::new(0));
    let total = selected.len();
    let with_facts = args.ingest == "distill";
    let rows: Vec<ResultRow> = futures::stream::iter(selected.iter().map(|q| {
        let cfg = cfg.clone();
        let store = store.clone();
        let done = done.clone();
        async move {
            let row = answer_question(&cfg, &store, persona, q, args.topk, with_facts).await;
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "[{persona} {d}/{total}] {} ({}) FAMA={:.2} MPA={}/{} FAA={}/{}",
                row.question_id,
                row.task_type,
                row.fama,
                row.memory_presence_correct,
                row.memory_presence_total,
                row.forgetting_absence_correct,
                row.forgetting_absence_total,
            );
            row
        }
    }))
    .buffer_unordered(args.concurrency)
    .collect()
    .await;

    // Per-question detail JSONL.
    std::fs::create_dir_all(&args.out_dir)?;
    let jsonl_path = args
        .out_dir
        .join(format!("run_{run_id}_{}_{persona}.jsonl", args.scale));
    let mut out = String::new();
    for row in &rows {
        out.push_str(&serde_json::to_string(row)?);
        out.push('\n');
    }
    std::fs::write(&jsonl_path, out)?;
    eprintln!("wrote {}", jsonl_path.display());

    let mut summary = aggregate_persona(persona, &rows);
    summary.distill_ingest = distill_stats;
    Ok(summary)
}

async fn run(args: Args) -> Result<()> {
    let cfg = LlmConfig::from_env()?;
    eprintln!("LLM: {} @ {}", cfg.model, cfg.api_base);

    // Distill mode: one extra LLM call per session, reusing the core
    // `causal_memory::distill::Distiller` (its own env config; absent →
    // per-session raw fallback inside ingest_persona_distill).
    let distiller = match args.ingest.as_str() {
        "distill" => {
            let d = Distiller::from_env();
            if d.is_none() {
                eprintln!(
                    "warn: Distiller::from_env() found no API key; falling back to raw ingest"
                );
            }
            d
        }
        _ => None,
    };

    let personas = match &args.persona {
        Some(p) => vec![p.clone()],
        None => list_personas(&args.memora_root, &args.scale)?,
    };
    if personas.is_empty() {
        eprintln!("no personas found under data/{}", args.scale);
        return Ok(());
    }
    eprintln!("scale {}: personas {:?}", args.scale, personas);

    let run_id = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let mut persona_summaries = Vec::new();
    for persona in &personas {
        let summary = run_persona(&cfg, distiller.as_ref(), &args, persona, &run_id).await?;
        persona_summaries.push(summary);
    }

    let summary = Summary {
        run_id: run_id.clone(),
        date: chrono::Utc::now().to_rfc3339(),
        git_commit: git_commit(),
        scale: args.scale.clone(),
        model: cfg.model.clone(),
        judge_model: cfg.model.clone(),
        temperature: LLM_TEMPERATURE,
        topk: args.topk,
        ingest: args.ingest.clone(),
        memora_root: args.memora_root.display().to_string(),
        judge_protocol: "single-judge (deepseek-chat, temp=0); official Table 3 uses \
                         3-judge majority vote via OpenRouter — NOT directly comparable"
            .into(),
        personas: persona_summaries,
    };
    std::fs::create_dir_all(&args.out_dir)?;
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
            // Distilling a whole session needs more than the core default
            // 8s HTTP timeout (which is tuned for the synchronous MCP path).
            if args.ingest == "distill" && std::env::var("CAUSAL_MEMORY_HTTP_TIMEOUT_SECS").is_err()
            {
                std::env::set_var("CAUSAL_MEMORY_HTTP_TIMEOUT_SECS", "60");
            }
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

    // -- FAMA (values cross-checked against the official Python fama_score) --

    #[test]
    fn fama_matches_official_formula() {
        // Perfect remembering, no forgetting probes: FAMA = MPA.
        assert_eq!(fama_score(2, 2, 0, 0), 1.0);
        // Both empty -> 0 (official guard).
        assert_eq!(fama_score(0, 0, 0, 0), 0.0);
        // MPA=1, FAA=1 -> 1.
        assert_eq!(fama_score(2, 2, 3, 3), 1.0);
        // MPA=0.5 (1/2), FAA=1 -> 0.5 (lambda term vanishes).
        assert_eq!(fama_score(1, 2, 4, 4), 0.5);
        // MPA=1 (2/2), FAA=0 (0/2), lambda=2/4=0.5 -> max(0, 1 - 0.5) = 0.5.
        assert_eq!(fama_score(2, 2, 0, 2), 0.5);
        // MPA=0.5 (1/2), FAA=0.5 (1/2), lambda=0.5 -> 0.5 - 0.5*0.5 = 0.25.
        assert_eq!(fama_score(1, 2, 1, 2), 0.25);
        // MPA=0, FAA=0, lambda=0.5 -> max(0, -0.5) = 0 (clamped).
        assert_eq!(fama_score(0, 2, 0, 2), 0.0);
        // No presence probes: MPA=0, FAA=1 -> 0 - 1*0 = 0 (official: a pure
        // forgetting question with all forgotten items absent scores 0).
        assert_eq!(fama_score(0, 0, 3, 3), 0.0);
        // No forgetting probes: FAA defaults to 1, FAMA = MPA = 2/3.
        let f = fama_score(2, 3, 0, 0);
        assert!((f - 2.0 / 3.0).abs() < 1e-12);
    }

    // -- session parsing / date handling --

    fn tiny_session_json() -> &'static str {
        r#"{
            "session_id": 3,
            "session_type": "activity",
            "operation": "delete",
            "operation_details": {"item": {"description": "Buy groceries"}, "category": "todo_list"},
            "date": "2025-06-03",
            "persona": "software_engineer",
            "conversation": [
                {"turn": 1, "speaker": "user_agent", "message": "Hi!", "share_memory": false},
                {"turn": 2, "speaker": "ai_agent", "message": "Hello, how can I help?", "share_memory": false},
                {"turn": 3, "speaker": "user_agent", "message": "Please remove Buy groceries from my todo list.", "share_memory": true},
                {"turn": 4, "speaker": "ai_agent", "message": "Done, I removed it.", "share_memory": false}
            ]
        }"#
    }

    #[test]
    fn session_json_parses() {
        let s: MemoraSession = serde_json::from_str(tiny_session_json()).unwrap();
        assert_eq!(s.session_id, 3);
        assert_eq!(s.date, "2025-06-03");
        assert_eq!(s.operation.as_deref(), Some("delete"));
        assert_eq!(s.conversation.len(), 4);
        assert!(s.conversation[2].share_memory);
        assert_eq!(s.conversation[0].speaker, "user_agent");
    }

    #[test]
    fn session_with_null_operation_parses() {
        // `no_memory` chit-chat sessions carry "operation": null (519 files in
        // the weekly scale alone).
        let raw = r#"{
            "session_id": 73, "session_type": "no_memory", "operation": null,
            "operation_details": {}, "date": "2025-06-04",
            "persona": "software_engineer",
            "conversation": [{"turn": 1, "speaker": "user_agent", "message": "Hi", "share_memory": false}]
        }"#;
        let s: MemoraSession = serde_json::from_str(raw).unwrap();
        assert_eq!(s.operation, None);
        assert_eq!(s.conversation.len(), 1);
    }

    #[test]
    fn parse_memora_date_works() {
        assert_eq!(parse_memora_date("2025-06-01"), Some(1_748_736_000)); // 2025-06-01T00:00:00Z
        assert_eq!(parse_memora_date(" 2025-06-07 "), Some(1_749_254_400));
        assert!(parse_memora_date("06/01/2025").is_none());
        assert!(parse_memora_date("garbage").is_none());
        assert!(parse_memora_date("").is_none());
    }

    #[test]
    fn synthetic_date_fallback_preserves_session_order() {
        let t1 = session_base_time(1, "garbage");
        let t2 = session_base_time(2, "also garbage");
        assert_eq!(t2 - t1, 86_400, "sessions spaced one day apart");
    }

    #[test]
    fn chunk_text_format() {
        let text = turn_chunk_text(12, "2025-06-03", "user_agent", " hello ");
        assert_eq!(text, "[session_12 2025-06-03] user: hello");
        let text = turn_chunk_text(1, "2025-06-01", "ai_agent", "hi");
        assert_eq!(text, "[session_1 2025-06-01] assistant: hi");
    }

    // -- ingest --

    #[test]
    fn light_session_classification() {
        // Short chit-chat: light (total < 1500 chars).
        let light: MemoraSession = serde_json::from_str(tiny_session_json()).unwrap();
        assert!(is_light_session(&light));

        // Long content: heavy even though turns are few.
        let heavy_json = r#"{
            "session_id": 9, "operation": "add", "date": "2025-06-09",
            "conversation": [
                {"turn": 1, "speaker": "user_agent", "message": "LONG"},
                {"turn": 2, "speaker": "ai_agent", "message": "LONG"}
            ]
        }"#;
        let long_msg = "x".repeat(1200);
        let heavy_text = heavy_json.replace("\"LONG\"", &format!("\"{long_msg}\""));
        let heavy: MemoraSession = serde_json::from_str(&heavy_text).unwrap();
        assert!(!is_light_session(&heavy), "2400 chars total => heavy");

        // Many turns clearing the total bar but tiny on average: light.
        let mut turns = String::new();
        for i in 1..=20 {
            if i > 1 {
                turns.push(',');
            }
            // Exactly 79 chars: 20 turns x 79 = 1580 >= 1500 total, avg 79 < 80.
            let msg = format!("{:79}", format!("small talk {i}"));
            turns.push_str(&format!(
                "{{\"turn\": {i}, \"speaker\": \"user_agent\", \"message\": \"{msg}\"}}"
            ));
        }
        let many_json = format!(
            "{{\"session_id\": 10, \"operation\": null, \"date\": \"2025-06-10\", \"conversation\": [{turns}]}}"
        );
        let many: MemoraSession = serde_json::from_str(&many_json).unwrap();
        let total: usize = many.conversation.iter().map(|t| t.message.len()).sum();
        assert!(
            total >= LIGHT_SESSION_TOTAL_CHARS,
            "fixture must clear total"
        );
        assert!(is_light_session(&many), "avg < 80 chars => light");

        // Empty conversation: light (nothing to preserve anyway).
        let empty: MemoraSession = serde_json::from_str(
            r#"{"session_id": 11, "operation": null, "date": "2025-06-11", "conversation": []}"#,
        )
        .unwrap();
        assert!(is_light_session(&empty));
    }

    #[test]
    fn memory_lines_filter_retraction_records_unless_history_question() {
        use causal_memory::distill::{ItemKind, MemoryItem};
        let rec = |text: &str, date: &str| MemoryItem {
            kind: ItemKind::Preference,
            text: text.into(),
            date: Some(date.into()),
            supersedes: None,
        };
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_distilled(
                &rec(
                    "User likes music from the 2010s, especially electronic pop.",
                    "2025-06-05",
                ),
                Some("p1"),
            )
            .unwrap();
        // Auto-supersedes (no hint): kills the outdated item, spawns a
        // negation memory — both retraction records.
        store
            .record_distilled(
                &rec(
                    "User no longer likes music from the 2010s as of 2025-06-05.",
                    "2025-06-05",
                ),
                Some("p1"),
            )
            .unwrap();
        store
            .record_distilled(
                &rec("User now prefers upbeat electronic music.", "2025-06-06"),
                Some("p1"),
            )
            .unwrap();

        let entries = retrieve(&store, "p1", "music preference", 10).unwrap();

        // Current-state question: retraction records withheld, current
        // preference kept.
        let lines = memory_lines(&entries, "What kind of music do I like?");
        assert!(lines.contains("upbeat electronic"), "{lines}");
        assert!(!lines.contains("no longer"), "{lines}");
        assert!(!lines.contains("Cancelled/superseded"), "{lines}");

        // History question: retraction records are the evidence — kept.
        let lines = memory_lines(&entries, "What changed in my music taste?");
        assert!(
            lines.contains("no longer") || lines.contains("Cancelled/superseded"),
            "{lines}"
        );
    }

    #[test]
    fn history_intent_detection() {
        assert!(!asks_about_history(
            "What tasks remain on my todo list this week?"
        ));
        assert!(!asks_about_history("Can you suggest me a movie?"));
        assert!(asks_about_history("Which tasks were removed from my list?"));
        assert!(asks_about_history("What changed in my music taste?"));
        assert!(asks_about_history("What music did I previously like?"));
    }

    #[test]
    fn distill_ingest_without_distiller_falls_back_to_raw() {
        let session: MemoraSession = serde_json::from_str(tiny_session_json()).unwrap();
        let store = CausalStore::open_in_memory().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let stats = rt
            .block_on(ingest_persona_distill(
                &store,
                None,
                "p1",
                std::slice::from_ref(&session),
                4,
            ))
            .unwrap();
        assert_eq!(stats.sessions_fallback_raw, 1);
        assert_eq!(stats.raw_chunks_written, 4);
        assert_eq!(stats.llm_calls, 0);
        // Raw fallback produces the same turn edges as raw ingest.
        assert_eq!(store.all_valid_edges().unwrap().len(), 3);
    }

    #[test]
    fn ingest_writes_chunks_edges_and_is_idempotent() {
        let session: MemoraSession = serde_json::from_str(tiny_session_json()).unwrap();
        let store = CausalStore::open_in_memory().unwrap();

        let n = ingest_persona(&store, "p1", std::slice::from_ref(&session)).unwrap();
        assert_eq!(n, 4);

        // Edges: turns 2->1, 3->2, 4->3 (alternating speakers), tagged p1.
        let edges = store.all_valid_edges().unwrap();
        assert_eq!(edges.len(), 3);
        assert!(edges
            .iter()
            .all(|e| e.decision_id.starts_with("p1::") && e.outcome_id.starts_with("p1::")));

        // Chunk text + event time.
        let (text, ts): (String, i64) = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT text, created_at FROM chunks WHERE id = 'p1::0003::3'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(
            text,
            "[session_3 2025-06-03] user: Please remove Buy groceries from my todo list."
        );
        // session date midnight UTC + 2s (0-based turn index 2).
        assert_eq!(ts, 1_748_908_800 + 2);

        // Idempotent: second run skips, edges not duplicated.
        let n2 = ingest_persona(&store, "p1", &[session]).unwrap();
        assert_eq!(n2, 4);
        assert_eq!(store.all_valid_edges().unwrap().len(), 3);
    }

    #[test]
    fn retrieval_is_scoped_to_persona() {
        let session: MemoraSession = serde_json::from_str(tiny_session_json()).unwrap();
        let store = CausalStore::open_in_memory().unwrap();
        ingest_persona(&store, "p1", std::slice::from_ref(&session)).unwrap();
        ingest_persona(&store, "p2", std::slice::from_ref(&session)).unwrap();

        let res = retrieve(&store, "p1", "todo list groceries", 10).unwrap();
        assert!(!res.is_empty());
        assert!(res
            .iter()
            .all(|e| e.decision_id.starts_with("p1::") && e.outcome_id.starts_with("p1::")));
    }

    #[test]
    fn fact_layer_routing_and_supersedes_retirement() {
        // Mirrors the ingest_persona_distill routing: Fact/Preference items
        // go to the fact layer (scope "user"); a supersedes hint retires the
        // outdated value so retrieval only surfaces the live fact — the
        // semantics the forgetting (FAA) evaluation relies on.
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_fact("preference", "User likes jazz music.", "user", "distill", 0.8)
            .unwrap();
        store
            .record_fact(
                "preference",
                "User now prefers classical music over jazz.",
                "user",
                "distill",
                0.8,
            )
            .unwrap();
        let retired = store
            .retire_facts_by_hint("preference", "user", "likes jazz music")
            .unwrap();
        assert_eq!(retired, 1);
        let hits = store
            .search_facts_bm25("what music does the user like", Some("user"), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].value.contains("classical"));
    }

    #[test]
    fn raw_ingest_redo_does_not_duplicate_turn_edges() {
        // distill_done-marker redo path: an interrupted persona is re-ingested
        // on the next run. Chunks dedupe via INSERT OR IGNORE; turn edges
        // must not be inserted again for pre-existing chunks.
        let session: MemoraSession = serde_json::from_str(tiny_session_json()).unwrap();
        let store = CausalStore::open_in_memory().unwrap();
        ingest_session_raw(&store, "p1", &session).unwrap();
        ingest_session_raw(&store, "p1", &session).unwrap();
        let (chunks, edges) = store
            .with_conn(|c| {
                let chunks: i64 = c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
                let edges: i64 = c.query_row(
                    "SELECT COUNT(*) FROM causal_edges WHERE task_tag = 'p1'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((chunks, edges))
            })
            .unwrap();
        assert_eq!(chunks, 4);
        // 4 alternating-speaker turns: turns 2/3/4 link back → exactly 3.
        assert_eq!(edges, 3);
    }

    // -- judge output parsing (official _evaluate_with_single_judge logic) --

    #[test]
    fn judge_parsing_happy_path() {
        let v = parse_judge_output(
            r#"{"answer": "yes", "confidence": 0.9, "explanation": "mentioned"}"#,
            "yes",
        );
        assert_eq!(v.llm_answer, "yes");
        assert!(v.is_correct);

        // expected "no" probes (forgetting): judge saying "no" is correct.
        let v = parse_judge_output(
            r#"{"answer": "no", "confidence": 0.8, "explanation": "not mentioned"}"#,
            "no",
        );
        assert!(v.is_correct);

        // Judge answer != expected -> incorrect.
        let v = parse_judge_output(r#"{"answer": "yes", "explanation": "x"}"#, "no");
        assert!(!v.is_correct);
    }

    #[test]
    fn judge_parsing_strips_markdown_fences() {
        let raw =
            "```json\n{\"answer\": \"no\", \"confidence\": 0.7, \"explanation\": \"absent\"}\n```";
        let v = parse_judge_output(raw, "no");
        assert_eq!(v.llm_answer, "no");
        assert!(v.is_correct);
    }

    #[test]
    fn judge_parsing_fallbacks_match_official() {
        // Invalid JSON -> scan raw text: contains "yes" but not "no".
        let v = parse_judge_output("yes, it does", "yes");
        assert_eq!(v.llm_answer, "yes");
        assert!(v.is_correct);

        // Contains both yes and no -> unclear -> incorrect either way.
        let v = parse_judge_output("well yes but actually no", "yes");
        assert_eq!(v.llm_answer, "unclear");
        assert!(!v.is_correct);

        // Valid JSON, invalid answer field -> infer from explanation.
        let v = parse_judge_output(
            r#"{"answer": "maybe", "explanation": "yes the item is mentioned"}"#,
            "yes",
        );
        assert_eq!(v.llm_answer, "yes");
        assert!(v.is_correct);
    }

    // -- questions file normalization --

    #[test]
    fn questions_file_normalization() {
        let raw = r#"{
            "persona": "p1",
            "date_range": {"start": "2025-06-01", "end": "2025-06-07"},
            "questions": {
                "remembering": [{"question_id": "r1", "question": "q1", "question_date": "2025-06-07",
                    "evaluation": {"evaluation_questions": [
                        {"evaluation_question_id": "e1", "evaluation_question": "Does it mention X?",
                         "expected_answer": "yes", "evaluation_type": "memory_presence"}]}}],
                "reasoning": [{"question_id": "r2", "question": "q2"}],
                "recommending": [{"question_id": "r3", "question": "q3"}]
            }
        }"#;
        let file: QuestionsFile = serde_json::from_str(raw).unwrap();
        assert_eq!(file.persona, "p1");
        assert_eq!(file.questions.len(), 3);
        let q = &file.questions["remembering"][0];
        assert_eq!(q.evaluation.evaluation_questions.len(), 1);
        assert_eq!(
            q.evaluation.evaluation_questions[0].evaluation_type,
            "memory_presence"
        );
    }

    // -- aggregation --

    #[test]
    fn aggregate_persona_means_per_question_fama() {
        let row = |task: &str, fama: f64, mp_c, mp_t, fa_c, fa_t| ResultRow {
            question_id: "q".into(),
            task_type: task.into(),
            question: "q".into(),
            question_date: "2025-06-07".into(),
            model_response: "a".into(),
            memories_retrieved: 3,
            evaluation_results: vec![
                EvalResultRow {
                    evaluation_question_id: "e".into(),
                    evaluation_question: "e".into(),
                    expected_answer: "yes".into(),
                    evaluation_type: "memory_presence".into(),
                    llm_answer: "yes".into(),
                    is_correct: true,
                    explanation: String::new(),
                };
                mp_t + fa_t
            ],
            fama,
            memory_presence_correct: mp_c,
            memory_presence_total: mp_t,
            memory_presence_accuracy: None,
            forgetting_absence_correct: fa_c,
            forgetting_absence_total: fa_t,
            forgetting_absence_accuracy: None,
            error: None,
        };
        let rows = vec![
            row("Remembering", 1.0, 2, 2, 2, 2),
            row("Remembering", 0.0, 0, 1, 0, 1),
            row("Reasoning", 0.5, 1, 1, 0, 1),
        ];
        let s = aggregate_persona("p1", &rows);
        // Overall FAMA = mean(1.0, 0.0, 0.5) * 100 = 50.
        assert!((s.fama - 50.0).abs() < 1e-9);
        assert_eq!(s.total_questions, 3);
        assert_eq!(s.memory_presence_correct, 3);
        assert_eq!(s.memory_presence_total, 4);
        assert_eq!(s.forgetting_absence_correct, 2);
        assert_eq!(s.forgetting_absence_total, 4);
        let rem = &s.by_task_type["Remembering"];
        assert!((rem.fama - 50.0).abs() < 1e-9); // mean(1.0, 0.0)*100
        let rea = &s.by_task_type["Reasoning"];
        assert!((rea.fama - 50.0).abs() < 1e-9); // 0.5*100
    }
}
