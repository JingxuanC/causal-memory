//! `compact` subcommand — the "compressed LoCoMo" experiment.
//!
//! Reproduces the compaction-degradation methodology of
//! agent-teardown/papers/02-compaction-degradation.md on the LoCoMo benchmark:
//! text memory decays under iterative LLM compaction, while causal edges —
//! which live OUTSIDE the compacted context — survive untouched.
//!
//! Two conditions per conversation:
//!
//!   A (text-only):        each session's turns are concatenated and compressed
//!                         k times by the LLM (the i-th compression takes the
//!                         (i-1)-th summary as input). The k-times compressed
//!                         summaries become the memory (one chunk per session).
//!
//!   B (text + causal):    the SAME compressed summaries, PLUS causal edges
//!                         extracted from the ORIGINAL, UNCOMPRESSED turns
//!                         (decision = turn i, outcome = turn i+1). The edge
//!                         text is never compressed — that is the architectural
//!                         fact under test ("the causal table is outside the
//!                         context window").
//!
//! Both conditions share the exact QA pipeline of `run` (BM25 retrieval,
//! answer prompt, judge), so any accuracy delta B - A is attributable to the
//! causal edges alone.
//!
//! Compression summaries are cached at <out>/cache/conv{N}_session{M}_k{K}.txt
//! so re-runs do not repeat the most expensive LLM calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use causal_memory::store::CausalStore;
use chrono::Utc;
use crate::PromptVersion;
use serde::Serialize;

use super::{
    answer_all, chat, format_ts, git_commit, session_base_time, turn_chunk_text, turn_event_time,
    Acc, CategoryStats, LlmConfig, LocomoConversation, Qa, ResultRow, Session,
    TURN_EDGE_CONFIDENCE, TURN_EDGE_DISCOVERED_BY, TURN_EDGE_RELATION,
};

/// Compaction prompt version, recorded in the report metadata.
///
/// The original methodology (papers/02 §4.6) used grok-build's production
/// 9-section structured compaction prompt
/// (`crates/common/xai-grok-compaction/src/code_compaction/templates/
/// full_replace_summary_prompt.txt`). That prompt's text is not available in
/// this repo and is coding-session specific (sections like "Files", "Errors
/// and Fixes"), so this experiment uses the semantically-equivalent fallback
/// from the experiment spec ("compress to at most 1/3 of the original length,
/// preserving key facts, decisions and dates"), phrased in English to match
/// the LoCoMo data. Iteration i compresses the summary of iteration i-1,
/// matching the paper's k-fold lossy compaction chain.
const COMPACT_PROMPT_VERSION: &str = "compact-v1-third";
const COMPACT_SYSTEM_PROMPT: &str =
    "You are a lossy conversation compactor. Compress the conversation text you are \
     given into a summary that is at most one third of its original length. Preserve \
     key facts, decisions, and dates. Output only the summary, no preamble.";
const COMPACT_MAX_TOKENS: u32 = 2048;

/// Retrieved memories per question — same default as `run`.
const COMPACT_TOPK: usize = 10;

const DEFAULT_COMPACT_K: usize = 5;

pub(crate) struct CompactArgs {
    data: PathBuf,
    conv: Option<usize>,
    compact_k: usize,
    limit: Option<usize>,
    concurrency: usize,
    out_dir: PathBuf,
}

pub(crate) fn parse_args(argv: &[String]) -> Result<CompactArgs> {
    // argv[0] == "compact" (dispatch guarantees this).
    let mut data: Option<PathBuf> = None;
    let mut conv: Option<usize> = None;
    let mut compact_k = DEFAULT_COMPACT_K;
    let mut limit = None;
    let mut concurrency = 8usize;
    let mut out_dir = PathBuf::from("benches/locomo/results");

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
            "--compact" => compact_k = take(&mut i, "--compact")?.parse()?,
            "--limit" => limit = Some(take(&mut i, "--limit")?.parse()?),
            "--concurrency" => concurrency = take(&mut i, "--concurrency")?.parse()?,
            "--out" => out_dir = PathBuf::from(take(&mut i, "--out")?),
            other => anyhow::bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    if compact_k == 0 {
        anyhow::bail!("--compact must be >= 1");
    }
    let data = data.ok_or_else(|| anyhow!("--data is required"))?;
    Ok(CompactArgs {
        data,
        conv,
        compact_k,
        limit,
        concurrency,
        out_dir,
    })
}

// ---------------------------------------------------------------------------
// Compression chain + cache
// ---------------------------------------------------------------------------

fn cache_path(cache_dir: &Path, conv: usize, session: u32, k: usize) -> PathBuf {
    cache_dir.join(format!("conv{conv}_session{session}_k{k}.txt"))
}

/// Longest cached prefix of the compression chain. Returns (next_k, text):
/// the first compression level that still needs an LLM call, and the text to
/// feed into it (the cached summary at next_k - 1, or the source when no
/// cache exists).
fn cached_prefix(
    cache_dir: &Path,
    conv: usize,
    session: u32,
    k: usize,
    source: &str,
) -> (usize, String) {
    let mut current = source.to_string();
    let mut next = 1;
    for i in 1..=k {
        match std::fs::read_to_string(cache_path(cache_dir, conv, session, i)) {
            Ok(cached) => {
                current = cached;
                next = i + 1;
            }
            Err(_) => break,
        }
    }
    (next, current)
}

fn write_cache(cache_dir: &Path, conv: usize, session: u32, k: usize, text: &str) -> Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let path = cache_path(cache_dir, conv, session, k);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// Iteratively compress `source` k times, resuming from the cache. The LLM
/// call itself already retries (chat(), 3 attempts); a failure here marks the
/// session as erroneous and the run continues without it.
async fn compress_session(
    cfg: &LlmConfig,
    cache_dir: &Path,
    conv: usize,
    session: u32,
    k: usize,
    source: &str,
) -> Result<String> {
    let (next, mut current) = cached_prefix(cache_dir, conv, session, k, source);
    for i in next..=k {
        current = chat(cfg, COMPACT_SYSTEM_PROMPT, &current, COMPACT_MAX_TOKENS)
            .await
            .with_context(|| {
                format!("compression failed at k={i} (conv {conv} session {session})")
            })?;
        write_cache(cache_dir, conv, session, i, &current)?;
    }
    Ok(current)
}

/// Plain-text form of a session's turns — the input of the first compression.
fn session_source_text(session: &Session, base: i64) -> String {
    session
        .turns
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            turn_chunk_text(
                session.number,
                turn_event_time(base, idx),
                &t.speaker,
                &t.text,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Store construction (conditions A and B)
// ---------------------------------------------------------------------------

/// A k-times compressed session summary, ready for ingestion.
struct SessionSummary {
    session: u32,
    base_time: i64,
    text: String,
}

fn summary_chunk_id(session: u32) -> String {
    format!("compact_s{session}")
}

/// Condition A store: one chunk per session holding the k-times compressed
/// summary.
///
/// Consecutive session summaries are chained with a low-confidence `caused`
/// edge — NOT because there is a causal claim between sessions, but because
/// the shared retriever (`search_causal_bm25`) only scans causal_edges, so
/// standalone chunks would be invisible to QA. The chain carries no extra
/// information: both endpoints are the compressed text itself.
fn ingest_compressed_summaries(store: &CausalStore, summaries: &[SessionSummary]) -> Result<usize> {
    for (i, s) in summaries.iter().enumerate() {
        let id = summary_chunk_id(s.session);
        let text = format!(
            "[session_{} {}] {}",
            s.session,
            format_ts(s.base_time),
            s.text
        );
        store.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, text, s.base_time],
            )?;
            causal_memory::store::CausalStore::index_chunk(&c, &id, &text)?;
            Ok(())
        })?;
        if i > 0 {
            let prev = summary_chunk_id(summaries[i - 1].session);
            store.with_conn(|c| {
                c.execute(
                    "INSERT INTO causal_edges
                     (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                    rusqlite::params![
                        prev,
                        id,
                        TURN_EDGE_RELATION,
                        TURN_EDGE_CONFIDENCE,
                        TURN_EDGE_DISCOVERED_BY,
                        s.base_time,
                        s.base_time
                    ],
                )?;
                Ok(())
            })?;
        }
    }
    Ok(summaries.len())
}

/// Condition B addition: causal edges over the ORIGINAL, UNCOMPRESSED turns
/// (decision = turn i, outcome = turn i + 1, within each session). Edge
/// relation/confidence/discovered_by match the plain ingest path; the
/// decision/outcome text keeps the pre-compression original — this is the
/// core contrast of the experiment.
///
/// "Informative turn" rule: every turn with non-empty text is recorded. This
/// is deliberately simple and deterministic (no extra LLM extraction call);
/// retrieval quality is left to BM25.
fn record_original_turn_edges(store: &CausalStore, sessions: &[Session]) -> Result<usize> {
    let mut written = 0usize;
    for session in sessions {
        let base = session_base_time(session);
        for idx in 0..session.turns.len().saturating_sub(1) {
            let t0 = &session.turns[idx];
            let t1 = &session.turns[idx + 1];
            if t0.text.trim().is_empty() || t1.text.trim().is_empty() {
                continue;
            }
            let ts_out = turn_event_time(base, idx + 1);
            let decision = turn_chunk_text(
                session.number,
                turn_event_time(base, idx),
                &t0.speaker,
                &t0.text,
            );
            let outcome = turn_chunk_text(session.number, ts_out, &t1.speaker, &t1.text);
            store.record_decision_at(
                &decision,
                &outcome,
                TURN_EDGE_RELATION,
                None,
                TURN_EDGE_CONFIDENCE,
                TURN_EDGE_DISCOVERED_BY,
                ts_out,
            )?;
            written += 1;
        }
    }
    Ok(written)
}

/// Open a fresh per-condition DB (derived artifact: any previous file is
/// removed so runs with different k never see stale state).
fn open_fresh_store(db_dir: &Path, name: &str) -> Result<CausalStore> {
    std::fs::create_dir_all(db_dir)?;
    let path = db_dir.join(format!("{name}.db"));
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale {}", path.display()))?;
    }
    CausalStore::open(&path).with_context(|| format!("opening {}", path.display()))
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ConditionReport {
    label: String,
    chunks: usize,
    edges: usize,
    total_questions: usize,
    correct: usize,
    incorrect: usize,
    error: usize,
    accuracy: f64,
    per_category: BTreeMap<String, CategoryStats>,
}

#[derive(Serialize)]
struct CompactSummary {
    run_id: String,
    date: String,
    git_commit: String,
    model: String,
    temperature: f32,
    compact_prompt_version: String,
    compact_system_prompt: String,
    compact_k: usize,
    topk: usize,
    data: String,
    conversations: Vec<usize>,
    cache_dir: String,
    compression_errors: Vec<String>,
    conditions: BTreeMap<String, ConditionReport>,
    /// B accuracy minus A accuracy, per category and overall: the causal
    /// table's rescue margin.
    delta_per_category: BTreeMap<String, f64>,
    delta_overall: f64,
}

fn condition_report(
    label: &str,
    chunks: usize,
    edges: usize,
    rows: &[ResultRow],
) -> ConditionReport {
    let mut overall = Acc::new();
    let mut per_cat: BTreeMap<u32, Acc> = BTreeMap::new();
    for row in rows {
        overall.add(row);
        per_cat
            .entry(row.category)
            .or_insert_with(Acc::new)
            .add(row);
    }
    ConditionReport {
        label: label.to_string(),
        chunks,
        edges,
        total_questions: overall.total,
        correct: overall.correct,
        incorrect: overall.incorrect,
        error: overall.error,
        accuracy: overall.accuracy(),
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
    }
}

fn print_comparison(summary: &CompactSummary) {
    let a = &summary.conditions["A_text_only"];
    let b = &summary.conditions["B_text_plus_causal"];
    println!(
        "\n=== compressed LoCoMo (k={}) — A vs B accuracy ===",
        summary.compact_k
    );
    println!("{:<10} {:>8} {:>8} {:>8}", "category", "A", "B", "B-A");
    let mut cats: Vec<&String> = a.per_category.keys().collect();
    cats.extend(b.per_category.keys());
    cats.sort();
    cats.dedup();
    for cat in cats {
        let aa = a.per_category.get(cat).map(|s| s.accuracy);
        let bb = b.per_category.get(cat).map(|s| s.accuracy);
        println!(
            "{:<10} {:>8} {:>8} {:>8}",
            cat,
            aa.map(|v| format!("{v:.3}")).unwrap_or_else(|| "-".into()),
            bb.map(|v| format!("{v:.3}")).unwrap_or_else(|| "-".into()),
            match (aa, bb) {
                (Some(x), Some(y)) => format!("{:+.3}", y - x),
                _ => "-".into(),
            }
        );
    }
    println!(
        "{:<10} {:>8.3} {:>8.3} {:>+8.3}",
        "overall", a.accuracy, b.accuracy, summary.delta_overall
    );
    if !summary.compression_errors.is_empty() {
        println!("compression errors: {}", summary.compression_errors.len());
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

pub(crate) async fn run(args: CompactArgs) -> Result<()> {
    let cfg = LlmConfig::from_env()?;
    let embedder: crate::SharedEmbedder =
        std::sync::Arc::new(tokio::sync::Mutex::new(causal_memory::embed::init_embedder()));
    eprintln!("LLM: {} @ {}", cfg.model, cfg.api_base);
    eprintln!(
        "compact: k={} prompt={} (topk={})",
        args.compact_k, COMPACT_PROMPT_VERSION, COMPACT_TOPK
    );

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

    let cache_dir = args.out_dir.join("cache");
    let db_dir = args.out_dir.join("db");
    std::fs::create_dir_all(&args.out_dir)?;
    let run_id = Utc::now().format("%Y%m%d_%H%M%S").to_string();

    let mut all_rows_a: Vec<ResultRow> = Vec::new();
    let mut all_rows_b: Vec<ResultRow> = Vec::new();
    let mut chunks_a = 0usize;
    let mut edges_a = 0usize;
    let mut chunks_b = 0usize;
    let mut edges_b = 0usize;
    let mut compression_errors: Vec<String> = Vec::new();
    let mut ran_convs = Vec::new();

    for conv_idx in conv_indices {
        let conv = &conversations[conv_idx];
        let sessions = conv.sessions()?;

        // --- compression phase (cached; failures mark the session, not the run)
        let mut summaries: Vec<SessionSummary> = Vec::new();
        for session in &sessions {
            let base = session_base_time(session);
            if session.turns.is_empty() {
                continue;
            }
            let source = session_source_text(session, base);
            match compress_session(
                &cfg,
                &cache_dir,
                conv_idx,
                session.number,
                args.compact_k,
                &source,
            )
            .await
            {
                Ok(text) => summaries.push(SessionSummary {
                    session: session.number,
                    base_time: base,
                    text,
                }),
                Err(e) => {
                    let msg = format!("conv {conv_idx} session {}: {e:#}", session.number);
                    eprintln!("error: {msg}");
                    compression_errors.push(msg);
                }
            }
        }
        eprintln!(
            "conv {conv_idx}: {}/{} sessions compressed (k={})",
            summaries.len(),
            sessions.len(),
            args.compact_k
        );

        let mut qas: Vec<Qa> = conv.qa.to_vec();
        if let Some(k) = args.limit {
            qas.truncate(k);
        }

        // --- condition A: text-only, k-times compressed
        let store_a = open_fresh_store(&db_dir, &format!("compact_conv{conv_idx}_A"))?;
        let ca = ingest_compressed_summaries(&store_a, &summaries)?;
        let ea = store_a.all_valid_edges()?.len();
        let rows_a = answer_all(
            &cfg,
            &store_a,
            &embedder,
            conv_idx,
            qas.clone(),
            COMPACT_TOPK,
            args.concurrency,
            false, // compact experiment is causal/text-only; no fact layer
            PromptVersion::V1, // compact experiment uses legacy prompt
            crate::JudgeStyle::Strict,
            false, // search_only: compact experiment always answers+judges
        )
        .await;
        write_rows(&args.out_dir, &run_id, conv_idx, "A", &rows_a)?;
        eprintln!(
            "conv {conv_idx} A: {ca} chunks, {ea} edges, {} questions",
            rows_a.len()
        );
        chunks_a += ca;
        edges_a += ea;
        all_rows_a.extend(rows_a);

        // --- condition B: same compressed text + causal edges over ORIGINAL turns
        let store_b = open_fresh_store(&db_dir, &format!("compact_conv{conv_idx}_B"))?;
        let cb = ingest_compressed_summaries(&store_b, &summaries)?;
        let causal_edges = record_original_turn_edges(&store_b, &sessions)?;
        let eb = store_b.all_valid_edges()?.len();
        let rows_b = answer_all(
            &cfg,
            &store_b,
            &embedder,
            conv_idx,
            qas,
            COMPACT_TOPK,
            args.concurrency,
            false, // compact experiment is causal/text-only; no fact layer
            PromptVersion::V1, // compact experiment uses legacy prompt
            crate::JudgeStyle::Strict,
            false, // search_only: compact experiment always answers+judges
        )
        .await;
        write_rows(&args.out_dir, &run_id, conv_idx, "B", &rows_b)?;
        eprintln!(
            "conv {conv_idx} B: {cb} chunks, {eb} edges ({causal_edges} uncompressed causal), {} questions",
            rows_b.len()
        );
        chunks_b += cb;
        edges_b += eb;
        all_rows_b.extend(rows_b);

        ran_convs.push(conv_idx);
    }

    let report_a = condition_report(
        "text-only: k-times compressed session summaries",
        chunks_a,
        edges_a,
        &all_rows_a,
    );
    let report_b = condition_report(
        "k-times compressed text + causal edges over original uncompressed turns",
        chunks_b,
        edges_b,
        &all_rows_b,
    );

    let mut delta_per_category: BTreeMap<String, f64> = BTreeMap::new();
    for (cat, bs) in &report_b.per_category {
        let av = report_a
            .per_category
            .get(cat)
            .map(|s| s.accuracy)
            .unwrap_or(0.0);
        delta_per_category.insert(cat.clone(), bs.accuracy - av);
    }
    let delta_overall = report_b.accuracy - report_a.accuracy;

    let mut conditions = BTreeMap::new();
    conditions.insert("A_text_only".to_string(), report_a);
    conditions.insert("B_text_plus_causal".to_string(), report_b);

    let summary = CompactSummary {
        run_id: run_id.clone(),
        date: Utc::now().to_rfc3339(),
        git_commit: git_commit(),
        model: cfg.model.clone(),
        temperature: super::LLM_TEMPERATURE,
        compact_prompt_version: COMPACT_PROMPT_VERSION.to_string(),
        compact_system_prompt: COMPACT_SYSTEM_PROMPT.to_string(),
        compact_k: args.compact_k,
        topk: COMPACT_TOPK,
        data: args.data.display().to_string(),
        conversations: ran_convs,
        cache_dir: cache_dir.display().to_string(),
        compression_errors,
        conditions,
        delta_per_category,
        delta_overall,
    };
    let summary_path = args.out_dir.join(format!("compact_{run_id}_summary.json"));
    let summary_json = serde_json::to_string_pretty(&summary)?;
    std::fs::write(&summary_path, &summary_json)?;
    print_comparison(&summary);
    eprintln!("wrote {}", summary_path.display());
    Ok(())
}

fn write_rows(
    out_dir: &Path,
    run_id: &str,
    conv_idx: usize,
    condition: &str,
    rows: &[ResultRow],
) -> Result<PathBuf> {
    let path = out_dir.join(format!("compact_{run_id}_conv{conv_idx}_{condition}.jsonl"));
    let mut out = String::new();
    for row in rows {
        out.push_str(&serde_json::to_string(row)?);
        out.push('\n');
    }
    std::fs::write(&path, out)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Tests (no network access)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Turn;

    fn mk_session(number: u32, date: &str, texts: &[&str]) -> Session {
        Session {
            number,
            date_time_raw: Some(date.to_string()),
            turns: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Turn {
                    speaker: if i % 2 == 0 { "Alice" } else { "Bob" }.to_string(),
                    dia_id: format!("D{number}:{}", i + 1),
                    text: t.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn cache_roundtrip_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();

        // No cache: resume from k=1 with the source text.
        let (next, text) = cached_prefix(cache, 0, 1, 5, "SOURCE");
        assert_eq!(next, 1);
        assert_eq!(text, "SOURCE");

        // Write k=1 and k=2; resume must skip both and continue at k=3 with
        // the k=2 summary — the expensive prefix is not recomputed.
        write_cache(cache, 0, 1, 1, "summary-k1").unwrap();
        write_cache(cache, 0, 1, 2, "summary-k2").unwrap();
        let (next, text) = cached_prefix(cache, 0, 1, 5, "SOURCE");
        assert_eq!(next, 3);
        assert_eq!(text, "summary-k2");

        // A gap in the chain (k=2 missing) resumes from the last contiguous hit.
        let dir2 = tempfile::tempdir().unwrap();
        write_cache(dir2.path(), 0, 1, 1, "s1").unwrap();
        write_cache(dir2.path(), 0, 1, 3, "s3").unwrap();
        let (next, text) = cached_prefix(dir2.path(), 0, 1, 5, "SOURCE");
        assert_eq!(next, 2);
        assert_eq!(text, "s1");

        // Asking for a smaller k than cached hits the cache exactly.
        let (next, text) = cached_prefix(cache, 0, 1, 2, "SOURCE");
        assert_eq!(next, 3, "k=2 fully cached, nothing to do");
        assert_eq!(text, "summary-k2");
    }

    #[test]
    fn condition_a_store_layout() {
        let store = CausalStore::open_in_memory().unwrap();
        let summaries = vec![
            SessionSummary {
                session: 1,
                base_time: 1_000,
                text: "compressed summary one".into(),
            },
            SessionSummary {
                session: 2,
                base_time: 2_000,
                text: "compressed summary two".into(),
            },
        ];
        let n = ingest_compressed_summaries(&store, &summaries).unwrap();
        assert_eq!(n, 2, "one chunk per session");

        let chunk_count: i64 = store
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(chunk_count, 2);

        // One chain edge between the two summaries (retrieval connectivity).
        let edges = store.all_valid_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].decision_text,
            "[session_1 1970-01-01 00:16] compressed summary one"
        );
        assert_eq!(
            edges[0].outcome_text,
            "[session_2 1970-01-01 00:33] compressed summary two"
        );
    }

    #[test]
    fn condition_b_edges_keep_original_uncompressed_text() {
        let store = CausalStore::open_in_memory().unwrap();
        let sessions = vec![mk_session(
            1,
            "1:56 pm on 8 May, 2023",
            &[
                "I adopted a retired racing greyhound named Biscuit last week.",
                "Wow, how is he settling in?",
                "He sleeps twenty hours a day.",
            ],
        )];
        let n = record_original_turn_edges(&store, &sessions).unwrap();
        assert_eq!(n, 2, "3 turns -> 2 consecutive-pair edges");

        let edges = store.all_valid_edges().unwrap();
        assert_eq!(edges.len(), 2);
        // The edge text is the ORIGINAL turn text (with the session date
        // prefix), never touched by the compressor.
        assert_eq!(
            edges[0].decision_text,
            "[session_1 2023-05-08 13:56] Alice: I adopted a retired racing greyhound named Biscuit last week."
        );
        assert_eq!(
            edges[0].outcome_text,
            "[session_1 2023-05-08 13:56] Bob: Wow, how is he settling in?"
        );
        assert!(edges.iter().all(|e| e.relation == TURN_EDGE_RELATION));
        assert!(edges
            .iter()
            .all(|e| e.discovered_by == TURN_EDGE_DISCOVERED_BY));

        // And the uncompressed text is actually retrievable via BM25.
        let hits = store
            .search_causal_bm25(None, "What kind of dog did Alice adopt?", 10)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits.iter()
                .any(|h| h.decision_text.contains("greyhound named Biscuit")),
            "the edge carrying the original turn text must be BM25-retrievable"
        );
    }

    #[test]
    fn condition_b_combines_compressed_chunks_and_original_edges() {
        let store = CausalStore::open_in_memory().unwrap();
        let sessions = vec![mk_session(
            1,
            "1:56 pm on 8 May, 2023",
            &["first turn", "second turn", "third turn"],
        )];
        let summaries = vec![SessionSummary {
            session: 1,
            base_time: session_base_time(&sessions[0]),
            text: "tiny compressed summary".into(),
        }];
        ingest_compressed_summaries(&store, &summaries).unwrap();
        let edge_n = record_original_turn_edges(&store, &sessions).unwrap();
        assert_eq!(edge_n, 2);

        // Chunks: 1 compressed summary + 3 distinct turns. The middle turn
        // ("second turn") is BOTH edge 1's outcome and edge 2's decision —
        // v9 text reuse keeps it ONE node (previously 2 duplicate chunks).
        let chunk_count: i64 = store
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(chunk_count, 4, "shared turn text must be a single node");
        assert_eq!(store.all_valid_edges().unwrap().len(), 2);
    }
}
