//! CE-3 `compact_with_causal`: ingest a transcript into the causal store
//! BEFORE the context it lives in is compacted away.
//!
//! Positioning (docs/compact-with-causal.md): in the context-rot era the
//! compressed summary is lossy and iteratively more lossy; causal edges live
//! OUTSIDE the context window, so whatever is recorded before compaction
//! survives it verbatim. Claude Code's PostCompact hook hands us the
//! transcript path right after compaction — the pre-compaction turns are
//! still in that file, and this command backfills them as decision→outcome
//! pairs before the next compaction eats more of them.
//!
//! Extraction is deliberately deterministic and LLM-free (same rule as
//! condition B of the LoCoMo compaction-survival bench): consecutive
//! non-empty turns pair as (decision = turn i, outcome = turn i+1). Retrieval
//! quality is left to the store's BM25/embedding search; a smarter extractor
//! can be layered on later without changing the ingest contract.
//!
//! Idempotency: PostCompact fires after EVERY compaction, and users re-run
//! this manually — so chunk ids are content-derived (SHA-256) and an edge
//! whose endpoints already exist is skipped, never duplicated.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use causal_memory::store::CausalStore;
use sha2::Digest;

/// Edge metadata: identical to the LoCoMo condition-B ingest path
/// (adjacent-turn pairing) so bench and product agree on what a
/// "compact-with-causal" edge is.
const EDGE_RELATION: &str = "caused";
const EDGE_CONFIDENCE: f64 = 0.4;
const EDGE_DISCOVERED_BY: &str = "compact-ingest";

/// Cap per side of an edge — transcripts contain huge tool outputs; the
/// causal table wants the decision text, not the payload.
const MAX_SIDE_CHARS: usize = 2000;

/// One conversational turn extracted from a transcript line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Turn {
    pub role: String,
    pub text: String,
    /// epoch seconds; 0 when the transcript line carries no timestamp.
    pub ts: i64,
}

/// Extract the human/assistant text of one transcript line (Claude Code
/// JSONL). Returns None for non-message lines (system, tool_result-only,
/// progress, sidechain, ...). Content may be a plain string or an array of
/// blocks — only `text` blocks are kept (tool_use/tool_result/thinking are
/// skipped: their payloads are not decision/outcome prose).
pub(crate) fn extract_turn(line: &str) -> Option<Turn> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let role = v.get("type")?.as_str()?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let msg = v.get("message")?;
    let text = match msg.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp())
        .unwrap_or(0);
    Some(Turn {
        role: role.to_string(),
        text,
        ts,
    })
}

/// Parse a whole transcript (JSONL) into ordered turns.
pub(crate) fn parse_transcript(content: &str) -> Vec<Turn> {
    content.lines().filter_map(extract_turn).collect()
}

fn content_id(prefix: &str, text: &str) -> String {
    let digest = sha2::Sha256::digest(text.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

/// Ingest (decision, outcome) pairs into the store, idempotently.
/// Returns (inserted, skipped_already_present).
pub(crate) fn ingest_pairs(
    store: &CausalStore,
    turns: &[Turn],
    task_tag: Option<&str>,
) -> Result<(usize, usize)> {
    let mut inserted = 0usize;
    let mut skipped = 0usize;
    for w in turns.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let decision: String = a.text.chars().take(MAX_SIDE_CHARS).collect();
        let outcome: String = b.text.chars().take(MAX_SIDE_CHARS).collect();
        let dec_id = content_id("cd", &decision);
        let out_id = content_id("co", &outcome);
        let event_time = if b.ts > 0 {
            b.ts
        } else {
            chrono::Utc::now().timestamp()
        };
        let tag = task_tag.map(|t| t.to_string());
        let new_edge = store.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![dec_id, decision, event_time],
            )?;
            c.execute(
                "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![out_id, outcome, event_time],
            )?;
            // Any edge between these endpoints (even an invalidated one)
            // means we recorded this pair before — don't duplicate.
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE from_id = ?1 AND to_id = ?2",
                rusqlite::params![dec_id, out_id],
                |r| r.get(0),
            )?;
            if n == 0 {
                c.execute(
                    "INSERT INTO causal_edges
                         (from_id, to_id, relation, confidence, discovered_by,
                          event_time, discovered_at, task_tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        dec_id,
                        out_id,
                        EDGE_RELATION,
                        EDGE_CONFIDENCE,
                        EDGE_DISCOVERED_BY,
                        event_time,
                        chrono::Utc::now().timestamp(),
                        tag,
                    ],
                )?;
            }
            Ok(n == 0)
        })?;
        if new_edge {
            inserted += 1;
        } else {
            skipped += 1;
        }
    }
    Ok((inserted, skipped))
}

pub(crate) struct IngestArgs {
    pub path: Option<PathBuf>,
    pub stdin: bool,
    pub task_tag: Option<String>,
    pub db: Option<PathBuf>,
    pub pin: bool,
}

pub(crate) fn parse_args(argv: &[String]) -> Result<IngestArgs> {
    let mut args = IngestArgs {
        path: None,
        stdin: false,
        task_tag: None,
        db: None,
        pin: false,
    };
    let mut i = 0;
    let take = |i: &mut usize, flag: &str| -> Result<String> {
        *i += 1;
        argv.get(*i)
            .cloned()
            .ok_or_else(|| anyhow!("missing value for {flag}"))
    };
    while i < argv.len() {
        match argv[i].as_str() {
            "--stdin" => args.stdin = true,
            "--task-tag" => args.task_tag = Some(take(&mut i, "--task-tag")?),
            "--db" => args.db = Some(PathBuf::from(take(&mut i, "--db")?)),
            "--pin" => args.pin = true,
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}"),
            other => {
                if args.path.is_some() {
                    anyhow::bail!("unexpected extra argument: {other}");
                }
                args.path = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    if args.path.is_none() && !args.stdin {
        anyhow::bail!(
            "usage: causal-memory compact-ingest <transcript.jsonl>|--stdin \
             [--task-tag TAG] [--db PATH] [--pin]"
        );
    }
    Ok(args)
}

/// `causal-memory compact-ingest <transcript.jsonl>|--stdin [flags]`
pub fn run(argv: &[String]) -> Result<()> {
    let args = parse_args(argv)?;

    let content = if args.stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading transcript from stdin")?;
        buf
    } else {
        // parse_args guarantees path is set when --stdin is absent.
        let path = args.path.as_ref().ok_or_else(|| {
            anyhow!("usage: causal-memory compact-ingest <transcript.jsonl>|--stdin")
        })?;
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    };

    let turns = parse_transcript(&content);
    if turns.len() < 2 {
        println!(
            "📭 {} turn(s) extracted — nothing to record (need at least 2).",
            turns.len()
        );
        return Ok(());
    }

    let db_path = args.db.clone().unwrap_or_else(crate::get_db_path);
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let store =
        CausalStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let (inserted, skipped) = ingest_pairs(&store, &turns, args.task_tag.as_deref())?;
    println!(
        "✅ compact-ingest: {} turns → {inserted} causal edge(s) recorded, \
         {skipped} already present (skipped).",
        turns.len()
    );
    println!(
        "   These edges live OUTSIDE the context window — compaction cannot \
         touch them. Query with trace_cause / search_causal / intervention_query."
    );

    if args.pin {
        let memory = causal_memory::memory::Memory::new(store);
        let pin_path = crate::server::tools::default_pin_path();
        if let Some(parent) = pin_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let content = memory.build_pin_file(20, 200);
        std::fs::write(&pin_path, &content)?;
        println!(
            "📌 refreshed {} ({} lines).",
            pin_path.display(),
            content.lines().count()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = concat!(
        r#"{"type":"system","subtype":"init","content":"ignored"}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-10-14T10:00:00Z","message":{"role":"user","content":"decided to switch the session cache to Redis with jittered TTL"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-10-14T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"switched to Redis; the stampede during deploy stopped"}]}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-10-14T10:02:00Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"},{"type":"text","text":"verified latency dropped"}]}}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-10-14T10:03:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"ok"},{"type":"text","text":"great, no more deploy failures"}]}}"#,
        "\n",
    );

    #[test]
    fn parse_extracts_text_blocks_only_in_order() {
        let turns = parse_transcript(TRANSCRIPT);
        assert_eq!(turns.len(), 4, "system line dropped, 4 text turns kept");
        assert_eq!(turns[0].role, "user");
        assert!(turns[0].text.contains("Redis"));
        assert!(turns[1].text.contains("stampede"));
        // tool_use block is not text; the companion text block is.
        assert!(turns[2].text.contains("verified latency"));
        assert!(!turns[2].text.contains("tool_use"));
        // tool_result block skipped, its text sibling kept.
        assert!(turns[3].text.contains("no more deploy failures"));
        assert!(!turns[3].text.contains("tool_result"));
        // Timestamps parsed (RFC 3339 → epoch, ordered).
        assert!(turns[0].ts > 0);
        assert!(turns[1].ts > turns[0].ts);
    }

    #[test]
    fn ingest_is_idempotent() {
        let store = CausalStore::open_in_memory().unwrap();
        let turns = parse_transcript(TRANSCRIPT);

        let (i1, s1) = ingest_pairs(&store, &turns, Some("deploy")).unwrap();
        assert_eq!(i1, 3, "4 turns → 3 consecutive pairs");
        assert_eq!(s1, 0);

        // Second run: every pair already recorded → all skipped, no dup edges.
        let (i2, s2) = ingest_pairs(&store, &turns, Some("deploy")).unwrap();
        assert_eq!(i2, 0);
        assert_eq!(s2, 3);
        assert_eq!(store.all_valid_edges().unwrap().len(), 3);

        // And the recorded text is retrievable.
        let hits = store
            .search_causal_bm25(None, "why did the deploy fail", 10)
            .unwrap();
        assert!(
            hits.iter().any(|h| h.decision_text.contains("Redis")),
            "ingested decision text must be retrievable: {hits:?}"
        );
    }

    #[test]
    fn parse_args_accepts_positional_and_flags() {
        let a = parse_args(&[
            "t.jsonl".to_string(),
            "--task-tag".to_string(),
            "x".to_string(),
            "--pin".to_string(),
        ])
        .unwrap();
        assert_eq!(a.path.unwrap().to_str().unwrap(), "t.jsonl");
        assert_eq!(a.task_tag.as_deref(), Some("x"));
        assert!(a.pin && !a.stdin);

        assert!(parse_args(&[]).is_err(), "path or --stdin required");
        let s = parse_args(&["--stdin".to_string()]).unwrap();
        assert!(s.stdin);
    }
}
