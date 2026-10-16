//! Human-interface commands — `ask` / `record` / `status` / `forget`.
//!
//! The everyday face of causal-memory for a person in a terminal, as
//! opposed to the MCP tools that agents call. All four delegate to the
//! same `Memory` facade the MCP tools use, so behavior is identical
//! across surfaces (CLI, MCP, Python). This is the onboarding/demo and
//! trust surface: see a memory, write one by hand, delete one you don't
//! trust — without an agent in the loop.

use std::path::PathBuf;

use causal_memory::memory::Memory;
use causal_memory::store::CausalStore;

use crate::get_db_path;

/// Pull `--db <PATH>` out of args; returns (db, remaining args).
/// Every user-facing command accepts `--db` so people can point at a
/// specific store (e.g. a per-project DB) without env gymnastics.
fn take_db(args: &[String]) -> (Option<PathBuf>, Vec<String>) {
    let mut db = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            if let Some(p) = args.get(i + 1) {
                db = Some(PathBuf::from(p));
                i += 2;
                continue;
            }
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (db, rest)
}

fn open_memory(db: Option<PathBuf>) -> anyhow::Result<Memory> {
    let path = db.unwrap_or_else(get_db_path);
    Ok(Memory::open(path)?)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `ask "<question>" [--limit N] [--tag T] [--explain] [--db PATH]`
///
/// Natural-language recall across ALL memory layers (facts + causal
/// lessons, RRF-fused) — the terminal twin of the agent's
/// `search_memory` tool. With no question, lists your recent memory
/// directory instead.
pub(crate) fn run_ask(args: &[String]) -> anyhow::Result<()> {
    let (db, rest) = take_db(args);
    let mut query: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut tag: Option<String> = None;
    let mut explain = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--limit" => {
                let Some(v) = rest.get(i + 1).and_then(|v| v.parse().ok()) else {
                    anyhow::bail!("--limit requires a number");
                };
                limit = Some(v);
                i += 2;
            }
            "--tag" => {
                let Some(v) = rest.get(i + 1) else {
                    anyhow::bail!("--tag requires a value");
                };
                tag = Some(v.clone());
                i += 2;
            }
            "--explain" => {
                explain = true;
                i += 1;
            }
            s if s.starts_with('-') => {
                anyhow::bail!("unknown flag: {s}\nUsage: causal-memory ask \"<question>\" [--limit N] [--tag T] [--explain]")
            }
            s => {
                if query.is_none() {
                    query = Some(s.to_string());
                } else {
                    anyhow::bail!("unexpected extra argument: '{s}'\nUsage: causal-memory ask \"<question>\" [--limit N] [--tag T] [--explain]");
                }
                i += 1;
            }
        }
    }
    let mem = open_memory(db)?;
    match query {
        Some(q) => {
            let out = mem.search_memory(
                &q,
                tag.as_deref(),
                None,
                limit,
                Some("l2"),
                None,
                Some(explain),
            );
            println!("{out}");
        }
        None => {
            println!("{}", mem.causal_directory(Some(limit.unwrap_or(10))));
        }
    }
    Ok(())
}

/// `record "<decision>" "<outcome>" [--relation R] [--tag T]
///        [--confidence S] [--context C] [--db PATH]`
///
/// Manually write a decision → outcome lesson. Default relation is
/// `caused`; `--confidence` accepts temporal/rule/llm_inferred/
/// user_feedback (maps to the same confidence table the MCP tool uses).
pub(crate) fn run_record(args: &[String]) -> anyhow::Result<()> {
    let (db, rest) = take_db(args);
    let mut decision: Option<String> = None;
    let mut outcome: Option<String> = None;
    let mut relation = "caused".to_string();
    let mut tag = "general".to_string();
    let mut confidence: Option<String> = None;
    let mut context: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--relation" => {
                let Some(v) = rest.get(i + 1) else {
                    anyhow::bail!("--relation requires a value");
                };
                relation = v.clone();
                i += 2;
            }
            "--tag" => {
                let Some(v) = rest.get(i + 1) else {
                    anyhow::bail!("--tag requires a value");
                };
                tag = v.clone();
                i += 2;
            }
            "--confidence" => {
                let Some(v) = rest.get(i + 1) else {
                    anyhow::bail!("--confidence requires a value");
                };
                confidence = Some(v.clone());
                i += 2;
            }
            "--context" => {
                let Some(v) = rest.get(i + 1) else {
                    anyhow::bail!("--context requires a value");
                };
                context = Some(v.clone());
                i += 2;
            }
            s if s.starts_with('-') => {
                anyhow::bail!(
                    "unknown flag: {s}\nUsage: causal-memory record \"<decision>\" \"<outcome>\" [--relation R] [--tag T] [--confidence S] [--context C]"
                )
            }
            s => {
                if decision.is_none() {
                    decision = Some(s.to_string());
                } else if outcome.is_none() {
                    outcome = Some(s.to_string());
                } else {
                    anyhow::bail!(
                        "unexpected extra argument: '{s}'\nUsage: causal-memory record \"<decision>\" \"<outcome>\" [--relation R] [--tag T]"
                    );
                }
                i += 1;
            }
        }
    }
    let (Some(d), Some(o)) = (decision, outcome) else {
        anyhow::bail!(
            "usage: causal-memory record \"<decision>\" \"<outcome>\" [--relation caused|enabled|prevented|no_effect] [--tag <task>] [--confidence user_feedback|rule|temporal|llm_inferred] [--context <ctx>]"
        );
    };
    if !matches!(
        relation.as_str(),
        "caused" | "enabled" | "prevented" | "no_effect"
    ) {
        anyhow::bail!(
            "relation must be one of: caused, enabled, prevented, no_effect (got '{relation}')"
        );
    }
    let mem = open_memory(db)?;
    let out = mem.record_decision(
        &d,
        &o,
        &relation,
        &tag,
        confidence.as_deref(),
        context.as_deref(),
    );
    println!("{out}");
    Ok(())
}

/// `status [--db PATH]` — the human heartbeat view.
///
/// One-screen summary: what the store holds, when it last learned
/// something, the three most recent lessons, and what to do next.
/// (The `stats` command remains the exhaustive DB-technical view.)
pub(crate) fn run_status(args: &[String]) -> anyhow::Result<()> {
    let (db, rest) = take_db(args);
    if let Some(extra) = rest.first() {
        anyhow::bail!("unknown flag: {extra}\nUsage: causal-memory status [--db <PATH>]");
    }
    let path = db.unwrap_or_else(get_db_path);
    let store = CausalStore::open(&path)?;
    // Size after open: opening a missing store creates+migrates it, so a
    // brand-new DB should not report "0 B on disk".
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    store.with_conn(|c| {
        let q1 = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) };
        let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let edges_active = q1("SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NULL")?;
        let edges_dead = q1("SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NOT NULL")?;
        let facts_active = q1("SELECT COUNT(*) FROM agent_facts WHERE valid_to IS NULL")?;
        let chunks = q1("SELECT COUNT(*) FROM chunks")?;

        println!("causal-memory — {}", path.display());
        println!("  {} on disk · schema v{version}", human_bytes(bytes));
        println!();
        println!("  🧠 lessons:   {edges_active} active · {edges_dead} invalidated (audited)");
        println!("  📌 facts:     {facts_active} active");
        println!("  📦 chunks:    {chunks}");

        if edges_active > 0 {
            let mut stmt = c.prepare(
                "SELECT fc.text, tc.text, e.relation, e.confidence
                 FROM causal_edges e
                 JOIN chunks fc ON fc.id = e.from_id
                 JOIN chunks tc ON tc.id = e.to_id
                 WHERE e.valid_to IS NULL
                 ORDER BY e.discovered_at DESC LIMIT 3",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, f64>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !rows.is_empty() {
                println!();
                println!("  Recent lessons:");
                for (i, (d, o, rel, conf)) in rows.iter().enumerate() {
                    let clip = |s: &str| -> String {
                        let s = s.replace('\n', " ");
                        if s.chars().count() > 72 {
                            format!("{}…", s.chars().take(72).collect::<String>())
                        } else {
                            s
                        }
                    };
                    println!(
                        "    {}. {} →({rel})→ {}  (conf {:.0}%)",
                        i + 1,
                        clip(d),
                        clip(o),
                        conf * 100.0
                    );
                }
            }
        }

        let latest: Option<i64> = c
            .query_row("SELECT MAX(discovered_at) FROM causal_edges", [], |r| {
                r.get(0)
            })
            .ok();
        println!();
        match latest {
            Some(ts) => {
                let dt = chrono::DateTime::from_timestamp(ts, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| ts.to_string());
                println!("  Last lesson recorded: {dt}");
            }
            None => println!(
                "  No lessons yet — record your first with `causal-memory record \"...\" \"...\"`"
            ),
        }
        println!();
        println!("  Next:");
        println!("    causal-memory ask \"<question>\"     search all memory (facts + lessons)");
        println!("    causal-memory record ...            write a lesson by hand");
        println!("    causal-memory forget <id>           hide a lesson you no longer trust");
        println!("    causal-memory sleep                 offline consolidation cycle");
        Ok(())
    })?;
    Ok(())
}

/// `forget <edge_id> [--reason <why>] [--db PATH]`
///
/// Soft-invalidate a causal lesson: hidden from every future recall
/// (search, spread, trace) but kept in the DB for audit — the memory
/// equivalent of "I was wrong", not "it never happened".
pub(crate) fn run_forget(args: &[String]) -> anyhow::Result<()> {
    let (db, rest) = take_db(args);
    let mut edge_id: Option<i64> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--reason" => {
                let Some(v) = rest.get(i + 1) else {
                    anyhow::bail!("--reason requires a value");
                };
                reason = Some(v.clone());
                i += 2;
            }
            s if s.starts_with('-') => {
                anyhow::bail!(
                    "unknown flag: {s}\nUsage: causal-memory forget <edge_id> [--reason <why>]"
                )
            }
            s => {
                if edge_id.is_none() {
                    edge_id = s.parse().ok();
                    if edge_id.is_none() {
                        anyhow::bail!("edge_id must be a number (got '{s}')");
                    }
                } else {
                    anyhow::bail!(
                        "unexpected extra argument: '{s}'\nUsage: causal-memory forget <edge_id> [--reason <why>]"
                    );
                }
                i += 1;
            }
        }
    }
    let Some(id) = edge_id else {
        anyhow::bail!("usage: causal-memory forget <edge_id> [--reason <why>]");
    };
    let mem = open_memory(db)?;
    let out = mem.invalidate_decision(id, reason.as_deref());
    println!("{out}");
    println!("(soft-invalidate: hidden from recall, kept in the DB for audit)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_db(name: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        std::fs::create_dir_all(std::env::temp_dir().join("cm-chat-tests")).unwrap();
        std::env::temp_dir().join("cm-chat-tests").join(format!(
            "{name}-{}-{}.db",
            std::process::id(),
            ts
        ))
    }

    #[test]
    fn test_record_via_cli_args_writes_lesson() {
        let db = tmp_db("record");
        let args = vec![
            "pushed to prod on Friday".to_string(),
            "outage — rollback needed".to_string(),
            "--relation".to_string(),
            "caused".to_string(),
            "--tag".to_string(),
            "deploy".to_string(),
            "--confidence".to_string(),
            "user_feedback".to_string(),
            "--db".to_string(),
            db.display().to_string(),
        ];
        run_record(&args).unwrap();
        let mem = Memory::open(&db).unwrap();
        let out = mem.search_memory("prod outage", None, None, Some(5), Some("l2"), None, None);
        assert!(
            out.contains("outage"),
            "recall should surface the recorded lesson:\n{out}"
        );
    }

    #[test]
    fn test_forget_hides_lesson_but_keeps_audit_row() {
        let db = tmp_db("forget");
        // Record two lessons; forget the first by its edge id (edge #1).
        let mem = Memory::open(&db).unwrap();
        mem.record_decision(
            "used Redis mutex",
            "deadlock — holder crashed",
            "caused",
            "concurrency",
            Some("rule"),
            None,
        );
        mem.record_decision(
            "switched to channel ownership",
            "race fixed",
            "caused",
            "concurrency",
            Some("user_feedback"),
            None,
        );
        let out = mem.search_memory("mutex", None, None, Some(5), Some("l2"), None, None);
        assert!(
            out.contains("deadlock"),
            "lesson visible before forget:\n{out}"
        );

        let args = vec![
            "1".to_string(),
            "--reason".to_string(),
            "wrong attribution".to_string(),
            "--db".to_string(),
            db.display().to_string(),
        ];
        run_forget(&args).unwrap();

        let mem2 = Memory::open(&db).unwrap();
        let out2 = mem2.search_memory("mutex", None, None, Some(5), Some("l2"), None, None);
        assert!(
            !out2.contains("deadlock"),
            "forgotten lesson must not surface:\n{out2}"
        );
        // Audit row survives (soft delete).
        let n: i64 = mem2
            .store()
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NOT NULL",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(n, 1, "invalidation must keep the audit row");
    }

    #[test]
    fn test_record_rejects_bad_relation() {
        let db = tmp_db("badrel");
        let args = vec![
            "a".to_string(),
            "b".to_string(),
            "--relation".to_string(),
            "because".to_string(),
            "--db".to_string(),
            db.display().to_string(),
        ];
        assert!(run_record(&args).is_err());
    }

    #[test]
    fn test_parser_fails_loud_on_extra_args_and_missing_values() {
        let db = tmp_db("strict");
        let db_arg = vec!["--db".to_string(), db.display().to_string()];

        // Extra positional argument → error, never silent ignore.
        let mut a = vec!["q1".to_string(), "q2".to_string()];
        a.extend(db_arg.clone());
        assert!(run_ask(&a).is_err());

        // Missing value after a value-taking flag → error.
        let mut a2 = vec!["q".to_string(), "--tag".to_string()];
        a2.extend(db_arg.clone());
        assert!(run_ask(&a2).is_err());

        let mut r = vec!["d".to_string(), "o".to_string(), "--confidence".to_string()];
        r.extend(db_arg.clone());
        assert!(run_record(&r).is_err());

        let mut f = vec!["1".to_string(), "--reason".to_string()];
        f.extend(db_arg.clone());
        assert!(run_forget(&f).is_err());

        // Unknown flag on status (which takes no positional args) → error.
        let mut s = vec!["--bogus".to_string()];
        s.extend(db_arg);
        assert!(run_status(&s).is_err());
    }

    #[test]
    fn test_status_and_ask_empty_db_are_ok() {
        let db = tmp_db("empty");
        let args = vec!["--db".to_string(), db.display().to_string()];
        assert!(run_status(&args).is_ok());
        let ask = vec![
            "anything".to_string(),
            "--db".to_string(),
            db.display().to_string(),
        ];
        assert!(run_ask(&ask).is_ok());
        // No query → directory listing path is also fine.
        let dir = vec!["--db".to_string(), db.display().to_string()];
        assert!(run_ask(&dir).is_ok());
    }
}
