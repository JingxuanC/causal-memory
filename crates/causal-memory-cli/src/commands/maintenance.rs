//! Store maintenance subcommands (restore / judge / sleep / migrate / embed / polarity).

use causal_memory::extractor::DecisionExtractor;
use causal_memory::store::CausalStore;
use crate::get_db_path;
use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) fn run_restore(args: &[String]) -> anyhow::Result<()> {
    let mut edge_id: Option<i64> = None;
    let mut db: Option<&String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                db = args.get(i);
            }
            other => {
                if edge_id.is_some() {
                    anyhow::bail!("unexpected argument: {other}");
                }
                edge_id = Some(other.parse().map_err(|_| {
                    anyhow::anyhow!("edge id must be an integer, got: {other}")
                })?);
            }
        }
        i += 1;
    }
    let Some(edge_id) = edge_id else {
        anyhow::bail!("Usage: causal-memory restore <edge_id> [--db <PATH>]");
    };
    let db_path = db.map(PathBuf::from).unwrap_or_else(get_db_path);
    let store = CausalStore::open(&db_path)?;
    if store.restore_edge(edge_id)? {
        let entry = store
            .get_edge(edge_id)?
            .ok_or_else(|| anyhow::anyhow!("edge {edge_id} vanished after restore"))?;
        println!(
            "Restored edge {edge_id}: \"{}\" → \"{}\" ({}) · confidence {:.2}",
            entry.decision_text, entry.outcome_text, entry.relation, entry.confidence
        );
        println!("The old lesson is live again and will surface in searches.");
    } else {
        eprintln!("Edge {edge_id} was not superseded (or does not exist) — nothing to restore.");
    }
    Ok(())
}

/// Parse a session JSON file: `{"date": "YYYY-MM-DD", "turns": [...]}` where
/// turns are `[speaker, message]` pairs or `{speaker, message}` objects.
pub(crate) fn load_session(path: &std::path::Path) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let raw = std::fs::read_to_string(path)?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let date = v
        .get("date")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    if date.is_empty() {
        anyhow::bail!("missing 'date'");
    }
    let turns_v = v
        .get("turns")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing 'turns' array"))?;
    let mut turns = Vec::new();
    let mut skipped = 0usize;
    for t in turns_v {
        if let Some(arr) = t.as_array() {
            if arr.len() >= 2 {
                turns.push((
                    arr[0].as_str().unwrap_or("user").to_string(),
                    arr[1].as_str().unwrap_or("").to_string(),
                ));
            } else {
                skipped += 1;
            }
        } else if let Some(obj) = t.as_object() {
            turns.push((
                obj.get("speaker")
                    .and_then(|x| x.as_str())
                    .unwrap_or("user")
                    .to_string(),
                obj.get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            ));
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        eprintln!("⚠️ {}: skipped {skipped} malformed turn(s)", path.display());
    }
    Ok((date, turns))
}

pub(crate) async fn run_judge(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::llm;
    use causal_memory::session::{default_source_kind, parser_for, SessionSource};

    let (agent, session_dir) = crate::commands::parse_agent_path(args);
    if session_dir.as_os_str().is_empty() {
        eprintln!("Usage: causal-memory judge <session-dir|session-file> [--agent grok|claude]");
        eprintln!("  Extracts decisions and uses a real LLM to judge causal confidence.");
        eprintln!("\nRequired env:");
        eprintln!("  CAUSAL_MEMORY_LLM_API   (e.g. https://api.deepseek.com/v1)");
        eprintln!("  CAUSAL_MEMORY_LLM_KEY   (or DEEPSEEK_API_KEY)");
        eprintln!("  CAUSAL_MEMORY_LLM_MODEL (default: deepseek-chat)");
        std::process::exit(1);
    }

    let config = match llm::LlmConfig::from_env() {
        Some(c) => {
            println!("LLM: {} @ {}", c.model, c.api_base);
            c
        }
        None => {
            eprintln!("No LLM configured. Set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY");
            std::process::exit(1);
        }
    };

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    // First extract (rule-based), then re-judge the high-value ones with LLM
    let source = SessionSource {
        path: session_dir.clone(),
        kind: default_source_kind(agent),
    };
    let parsed = parser_for(agent).parse(&source)?;
    println!(
        "Extracting from: {} (agent={agent:?})",
        session_dir.display()
    );
    let stats = DecisionExtractor::extract_from_parsed(&store, &parsed)?;
    println!(
        "Extracted {} edges (rule-based). Now re-judging top entries with LLM...\n",
        stats.edges_inserted
    );

    // Get top edges by rule-based confidence (high-value first), re-judge with LLM
    let recent = store.top_decisions_by_confidence(20)?;
    println!(
        "LLM-judging {} highest-confidence decisions:\n",
        recent.len()
    );

    for (i, entry) in recent.iter().enumerate() {
        let decision = &entry.decision_snippet;
        let outcome = &entry.outcome_snippet;

        match llm::judge_causality(&config, decision, outcome).await {
            Ok((confidence, reasoning)) => {
                println!(
                    "{}. [{}] {:.0}% — {}",
                    i + 1,
                    entry.task_tag.as_deref().unwrap_or("?"),
                    confidence * 100.0,
                    decision
                );
                println!("   → {}", outcome);
                println!("   LLM: {} (\"{}\")", confidence, reasoning);
                match store.rejudge_decision(&entry.id, confidence, "llm_inferred") {
                    Ok(n) => println!("   ↳ wrote confidence back to {} edge(s)\n", n),
                    Err(e) => println!("   ↳ write-back failed: {}\n", e),
                }
            }
            Err(e) => {
                println!("{}. {} — LLM judge failed: {}\n", i + 1, decision, e);
            }
        }
    }

    Ok(())
}
/// C7 update-resolver: scan for lessons that new evidence may have falsified
/// and let the LLM judge each candidate. Without `--apply` this is a
/// read-only preview (candidates + LLM verdicts); with `--apply` the
/// superseded edges are soft-invalidated. No LLM configured (or a judge
/// failure) leaves the candidate untouched — rule-based behaviour is the
/// fallback.
pub(crate) async fn run_resolve_updates(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::llm::{judge_supersession, LlmConfig};

    let mut db: Option<PathBuf> = None;
    let mut limit: usize = 50;
    let mut apply = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--db requires a path\nUsage: causal-memory resolve-updates [--db <PATH>] [--limit N] [--apply]");
                };
                db = Some(PathBuf::from(p));
            }
            "--limit" => {
                i += 1;
                let Some(n) = args.get(i) else {
                    anyhow::bail!("--limit requires a number");
                };
                limit = n.parse().map_err(|_| anyhow::anyhow!("bad limit: {n}"))?;
            }
            "--apply" => apply = true,
            other => anyhow::bail!("unknown flag: {other}\nUsage: causal-memory resolve-updates [--db <PATH>] [--limit N] [--apply]"),
        }
        i += 1;
    }

    let store = CausalStore::open(db.unwrap_or_else(get_db_path))?;
    let llm = LlmConfig::from_env();
    let candidates = store.find_falsified_candidates(limit)?;
    if candidates.is_empty() {
        println!("No repeated-decision candidates found — nothing to resolve.");
        return Ok(());
    }
    println!("C7 update-resolver: {} candidate edge(s) with repeated decisions but different outcomes", candidates.len());
    if llm.is_none() {
        println!("No CAUSAL_MEMORY_LLM_API configured — set it to get LLM verdicts (rule-based fallback only).");
    }
    println!();

    let mut superseded = 0usize;
    // One old edge can pair with several newer records (each is a candidate):
    // once a verdict retires it, later pairs for the same edge are redundant
    // (no re-judge, no duplicate SUPERSEDED line, no second invalidate).
    let mut retired: HashSet<i64> = HashSet::new();
    for (edge_id, _new_edge_id, old_dec, old_out, new_dec, new_out) in &candidates {
        if retired.contains(edge_id) {
            continue;
        }
        let old_d: String = old_dec.chars().take(60).collect();
        let old_o: String = old_out.chars().take(60).collect();
        let new_o: String = new_out.chars().take(60).collect();
        print!("[edge {edge_id}] \"{old_d}\" → \"{old_o}\"\n            now: \"{new_o}\"");
        let verdict = match &llm {
            Some(config) => match judge_supersession(config, old_dec, old_out, new_dec, new_out).await {
                Ok(v) => v,
                Err(e) => {
                    println!("  ⚠ judge failed: {e} (kept, rule-based fallback)");
                    continue;
                }
            },
            None => {
                println!("  (no LLM — preview only)");
                continue;
            }
        };
        if verdict.supersedes {
            if apply {
                store.invalidate_edge(*edge_id)?;
                retired.insert(*edge_id);
                superseded += 1;
                println!("  ✗ SUPERSEDED{}", if !verdict.reasoning.is_empty() { format!(" — {}", verdict.reasoning) } else { String::new() });
            } else {
                println!("  ✗ would supersede{}", if !verdict.reasoning.is_empty() { format!(" — {}", verdict.reasoning) } else { String::new() });
            }
        } else {
            println!("  ✓ keep{}", if !verdict.reasoning.is_empty() { format!(" — {}", verdict.reasoning) } else { String::new() });
        }
    }
    println!();
    if apply {
        println!("Applied: {superseded} edge(s) invalidated.");
    } else {
        println!("Preview only (no changes). Re-run with --apply to invalidate the superseded edges.");
    }
    Ok(())
}

struct DbFlags {
    db: PathBuf,
    dry_run: bool,
}

fn parse_db_flags(args: &[String], usage: &str) -> anyhow::Result<DbFlags> {
    let mut db: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    anyhow::bail!("--db requires a path\n{usage}");
                };
                db = Some(PathBuf::from(path));
            }
            "--dry-run" => dry_run = true,
            other => anyhow::bail!("unknown flag: {other}\n{usage}"),
        }
        i += 1;
    }
    Ok(DbFlags {
        db: db.unwrap_or_else(get_db_path),
        dry_run,
    })
}

pub(crate) fn run_sleep(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::consolidate::{consolidate, ConsolidateConfig};

    let auto = args.iter().any(|a| a == "--auto");
    let immutable = args.iter().any(|a| a == "--immutable");

    // D2 (SWR 2.0 / Dreams-aligned): --restore swaps the live DB for a
    // consolidated snapshot (backing up the current DB first). Handled
    // before the shared flag parser — it takes its own positional path.
    if let Some(pos) = args.iter().position(|a| a == "--restore") {
        let Some(src) = args.get(pos + 1) else {
            anyhow::bail!("--restore requires a path\nUsage: causal-memory sleep --restore <consolidated.db> [--db <PATH>]");
        };
        let mut target = get_db_path();
        let mut i = pos + 2;
        while i < args.len() {
            if args[i] == "--db" {
                if let Some(p) = args.get(i + 1) {
                    target = std::path::PathBuf::from(p);
                    i += 2;
                    continue;
                }
            }
            anyhow::bail!("unexpected arg: {}\nUsage: causal-memory sleep --restore <consolidated.db> [--db <PATH>]", args[i]);
        }
        return restore_from_consolidated(std::path::Path::new(src), &target);
    }

    // --auto / --immutable are sleep-specific: strip them before the shared
    // flag parser.
    let filtered: Vec<String> = args
        .iter()
        .filter(|a| *a != "--auto" && *a != "--immutable")
        .cloned()
        .collect();
    let flags = parse_db_flags(
        &filtered,
        "Usage: causal-memory sleep [--db <PATH>] [--dry-run] [--auto] [--immutable] | sleep --restore <consolidated.db>",
    )?;
    if let Some(parent) = flags.db.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let now = chrono::Utc::now().timestamp();
    // --auto: P6 novelty gate — skip the cycle when recent experience is too
    // uniform to have consolidation material (min_diversity = 0.4).
    let config = if auto {
        ConsolidateConfig {
            min_diversity: 0.4,
            ..ConsolidateConfig::default()
        }
    } else {
        ConsolidateConfig::default()
    };

    // D2: --immutable produces a NEW store — the original file is never
    // mutated (Dreams alignment: produce a new store, keep the original
    // untouched and auditable). Preview via dry-run on the live DB, then
    // VACUUM INTO a timestamped copy and run the real consolidation there.
    if immutable {
        let preview_store = CausalStore::open(&flags.db)?;
        let preview = consolidate(&preview_store, &config, true, now)?;
        let stem = flags
            .db
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "causal".to_string());
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let new_path = flags.db.with_file_name(format!("{stem}.consolidated-{ts}"));
        if new_path.exists() {
            anyhow::bail!("target {} already exists — refusing to overwrite", new_path.display());
        }
        vacuum_into(&flags.db, &new_path)?;
        let new_store = CausalStore::open(&new_path)?;
        let report = consolidate(&new_store, &config, false, now)?;
        println!("=== Sleep Consolidation (IMMUTABLE — new store) ===");
        println!("  original untouched: {}", flags.db.display());
        println!("  consolidated store: {}", new_path.display());
        println!("  preview diversity: {:.2} ({} edges)", preview.diversity, report.reactivated.len());
        println!("  to activate: sleep --restore {} [--db {}]", new_path.display(), flags.db.display());
        println!();
        return print_consolidation_report(&report, flags.db.as_path(), Some(new_path.as_path()));
    }

    let store = CausalStore::open(&flags.db)?;
    eprintln!("[sleep] store opened, starting consolidation");
    let report = consolidate(&store, &config, flags.dry_run, now)?;
    eprintln!("[sleep] consolidation returned");

    if report.skipped_low_diversity {
        println!("=== Sleep Consolidation: SKIPPED (recent diversity {:.2} < 0.4) — nothing new to consolidate ===", report.diversity);
        return Ok(());
    }
    print_consolidation_report(&report, flags.db.as_path(), None)
}

/// Print the shared consolidation report body. `extra_store` is Some when
/// the report was produced on a brand-new immutable store (D2).
fn print_consolidation_report(
    report: &causal_memory::consolidate::ConsolidateReport,
    db_path: &std::path::Path,
    extra_store: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    println!(
        "=== Sleep Consolidation Report{} ===",
        if report.dry_run { " (DRY RUN)" } else { "" }
    );
    println!("DB: {} (recent diversity {:.2})", db_path.display(), report.diversity);
    if let Some(p) = extra_store {
        println!("Consolidated store (original untouched): {}", p.display());
    }
    println!();

    println!("① Reactivation (replay priority, top {}):", report.reactivated.len().min(10));
    for (i, entry) in report.reactivated.iter().take(10).enumerate() {
        let snippet: String = entry.decision_text.chars().take(60).collect();
        println!("  {}. [edge {}] score {:.2} — {}", i + 1, entry.edge_id, entry.score, snippet);
        println!("     ({})", entry.reasons.join(", "));
    }
    if report.reactivated.is_empty() {
        println!("  (no valid edges)");
    }
    println!("  → {} edge(s) replay-protected & marked (half decay, lenient GC)", report.replayed);

    println!("\n①.7 C7 supersession (LLM judge): {} lesson(s) retired", report.superseded_lessons);

    println!("\n② Generalization:");
    println!("  redundant edges merged: {}", report.merged_edges);
    println!(
        "  patterns mined: similar_to={} repeated={} contradicts={} refines={}",
        report.mine_report.similar_to,
        report.mine_report.repeated,
        report.mine_report.contradicts,
        report.mine_report.refines
    );
    println!(
        "  pruned: trivial/self={} too-short={} capped={}",
        report.mine_report.skipped_self,
        report.mine_report.skipped_short,
        report.mine_report.capped
    );

    println!("\n③ Downscaling:");
    println!("  decayed:        {}", report.decayed);
    println!("  access-boosted: {}", report.boosted);
    println!("  GC invalidated: {}", report.gc_invalidated);
    println!(
        "  facts decayed:  {} (GC'd: {})",
        report.facts_decayed, report.facts_gc
    );

    println!("\n④ REM integration:");
    println!("  cross-domain transfers: {}", report.rem_transfers);
    println!("⑤ Q-value reinforcement (Bellman): {} chunk(s) updated", report.q_updates);

    if report.dry_run {
        println!("\n(dry run — no changes were written)");
    }
    Ok(())
}

/// Copy the current DB into a brand-new file via SQLite's VACUUM INTO
/// (the Dreams-aligned "produce a new store" primitive). The target must
/// not exist; the copy is a complete, consistent snapshot.
fn vacuum_into(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    let conn = rusqlite::Connection::open(src)?;
    let dst_str = dst.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{dst_str}'"))?;
    Ok(())
}

/// Swap the live DB for a consolidated snapshot (D2): back up the current
/// file, then copy the snapshot over it. The snapshot is a complete DB, so
/// a plain file copy is enough; the original backup allows rollback.
fn restore_from_consolidated(src: &std::path::Path, target: &std::path::Path) -> anyhow::Result<()> {
    if !src.exists() {
        anyhow::bail!("consolidated store not found: {}", src.display());
    }
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "causal".to_string());
    let backup = target.with_file_name(format!("{stem}.bak.{ts}"));
    if target.exists() {
        std::fs::copy(target, &backup)?;
        println!("Backed up current store: {}", backup.display());
    }
    std::fs::copy(src, target)?;
    println!("Restored consolidated store: {} → {}", src.display(), target.display());
    println!("Backup (rollback): {}", backup.display());
    Ok(())
}

pub(crate) fn run_migrate(args: &[String]) -> anyhow::Result<()> {
    let flags = parse_db_flags(args, "Usage: causal-memory migrate [--db <PATH>]")?;
    if let Some(parent) = flags.db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Open a raw connection so we can show user_version before/after the
    // migration that CausalStore::open would otherwise run silently.
    let conn = rusqlite::Connection::open(&flags.db)?;
    let before: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    causal_memory::migrate::migrate(&conn)?;
    let after: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    println!("=== Schema migration ===");
    println!("DB: {}", flags.db.display());
    println!("user_version before: {before}");
    println!("user_version after:  {after}");
    if before == after {
        println!("(already up to date)");
    }
    Ok(())
}

/// Backfill embeddings for valid edges that don't have one yet.
pub(crate) async fn run_embed(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::embed::{EmbedConfig, Embedder};    let mut db: Option<PathBuf> = None;
    let mut limit: usize = 0; // 0 = all pending
    let mut also_facts = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    anyhow::bail!("--db requires a path\nUsage: causal-memory embed [--db <PATH>] [--limit N] [--facts]");
                };
                db = Some(PathBuf::from(path));
            }
            "--limit" => {
                i += 1;
                let Some(n) = args.get(i) else {
                    anyhow::bail!("--limit requires a number\nUsage: causal-memory embed [--db <PATH>] [--limit N] [--facts]");
                };
                limit = n
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--limit must be a positive integer, got: {n}"))?;
            }
            "--facts" => also_facts = true,
            other => {
                anyhow::bail!(
                    "unknown flag: {other}\nUsage: causal-memory embed [--db <PATH>] [--limit N] [--facts]"
                )
            }
        }
        i += 1;
    }

    // Try HTTP embedding config first, then local ONNX.
    let mut embedder: causal_memory::embed::UnifiedEmbedder =
        if let Some(config) = EmbedConfig::from_env() {
            println!("Embedder: {} @ {}", config.model, config.api_base);
            causal_memory::embed::UnifiedEmbedder::Http(Embedder::new(config))
        } else {
            #[cfg(feature = "local-embed")]
            {
                match causal_memory::embed::LocalEmbedder::new() {
                    Ok(e) => {
                        println!("Embedder: {} (local ONNX)", e.model());
                        causal_memory::embed::UnifiedEmbedder::Local(e)
                    }
                    Err(e) => {
                        eprintln!("No HTTP embedding configured and local ONNX failed: {e}");
                        eprintln!("Set CAUSAL_MEMORY_EMBED_API + CAUSAL_MEMORY_EMBED_KEY, or build with --features local-embed");
                        std::process::exit(1);
                    }
                }
            }
            #[cfg(not(feature = "local-embed"))]
            {
                eprintln!("Embedding not configured. Set:");
                eprintln!("  CAUSAL_MEMORY_EMBED_API   (default: CAUSAL_MEMORY_LLM_API)");
                eprintln!("  CAUSAL_MEMORY_EMBED_KEY   (default: CAUSAL_MEMORY_LLM_KEY)");
                eprintln!("  CAUSAL_MEMORY_EMBED_MODEL (default: text-embedding-3-small)");
                eprintln!("  Or rebuild with --features local-embed for offline ONNX embedding.");
                std::process::exit(1);
            }
        };

    let db_path = db.unwrap_or_else(get_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;

    let model = embedder.model().to_string();
    let mut total_success = 0usize;
    let mut total_failed = 0usize;

    let pending = store.edges_without_embedding(limit)?;
    if pending.is_empty() {
        println!("No valid edges missing embeddings.");
    } else {
        println!("Embedding {} edge(s)...", pending.len());
        for (idx, (edge_id, text)) in pending.iter().enumerate() {
            match embedder.embed(text).await {
                Ok(vec) => match store.put_embedding(*edge_id, &model, &vec) {
                    Ok(()) => {
                        total_success += 1;
                        println!("  [{}/{}] edge {} ✓", idx + 1, pending.len(), edge_id);
                    }
                    Err(e) => {
                        total_failed += 1;
                        println!("  [{}/{}] edge {} DB write failed: {e}", idx + 1, pending.len(), edge_id);
                    }
                },
                Err(e) => {
                    total_failed += 1;
                    println!("  [{}/{}] edge {} embed failed: {e}", idx + 1, pending.len(), edge_id);
                }
            }
        }
    }

    if also_facts {
        let facts = store.facts_without_embedding(limit)?;
        if facts.is_empty() {
            println!("No valid facts missing embeddings.");
        } else {
            println!("\nEmbedding {} fact(s)...", facts.len());
            for (idx, (fid, text)) in facts.iter().enumerate() {
                match embedder.embed(text).await {
                    Ok(vec) => match store.put_fact_embedding(*fid, &model, &vec) {
                        Ok(()) => {
                            total_success += 1;
                            println!("  [{}/{}] fact {} ✓", idx + 1, facts.len(), fid);
                        }
                        Err(e) => {
                            total_failed += 1;
                            println!("  [{}/{}] fact {} DB write failed: {e}", idx + 1, facts.len(), fid);
                        }
                    },
                    Err(e) => {
                        total_failed += 1;
                        println!("  [{}/{}] fact {} embed failed: {e}", idx + 1, facts.len(), fid);
                    }
                }
            }
        }
    }

    println!("\n=== Embed backfill complete ===");
    println!("  success: {total_success}");
    println!("  failed:  {total_failed}");
    Ok(())
}

/// Backfill outcome polarity (v4) for valid edges that don't have one yet.
/// Uses the LLM judge when configured, otherwise the signal-word heuristic.
pub(crate) async fn run_polarity(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::llm::{judge_polarity, LlmConfig};
    use causal_memory::store::outcome_polarity;

    let mut db: Option<PathBuf> = None;
    let mut limit: usize = 100;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    anyhow::bail!("--db requires a path\nUsage: causal-memory polarity [--db <PATH>] [--limit N]");
                };
                db = Some(PathBuf::from(path));
            }
            "--limit" => {
                i += 1;
                let Some(n) = args.get(i) else {
                    anyhow::bail!("--limit requires a number\nUsage: causal-memory polarity [--db <PATH>] [--limit N]");
                };
                limit = n
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--limit must be a positive integer, got: {n}"))?;
            }
            other => {
                anyhow::bail!(
                    "unknown flag: {other}\nUsage: causal-memory polarity [--db <PATH>] [--limit N]"
                )
            }
        }
        i += 1;
    }

    // LLM optional: unconfigured (or per-edge failure) falls back to the
    // signal-word heuristic, same as the record path.
    let config = LlmConfig::from_env();
    match &config {
        Some(c) => println!("LLM: {} @ {}", c.model, c.api_base),
        None => println!("No LLM configured — using the signal-word heuristic."),
    }

    let db_path = db.unwrap_or_else(get_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;

    let pending = store.edges_without_polarity(limit)?;
    if pending.is_empty() {
        println!("No valid edges missing outcome polarity. Nothing to do.");
        return Ok(());
    }
    println!("Judging polarity of {} edge(s)...\n", pending.len());

    let total = pending.len();
    let mut updated = 0usize;
    let mut llm_failed = 0usize;
    let mut failed = 0usize;
    for (idx, (edge_id, decision, outcome)) in pending.iter().enumerate() {
        let mut polarity = None;
        if let Some(c) = &config {
            match judge_polarity(c, decision, outcome).await {
                Ok(pol) => polarity = Some(pol),
                Err(e) => {
                    llm_failed += 1;
                    println!(
                        "[{}/{}] edge {} LLM judge failed: {e} (heuristic fallback)",
                        idx + 1,
                        total,
                        edge_id
                    );
                }
            }
        }
        let polarity = polarity.unwrap_or_else(|| {
            match outcome_polarity(outcome) {
                Some(true) => "positive",
                Some(false) => "negative",
                None => "neutral",
            }
            .to_string()
        });
        match store.set_outcome_polarity(*edge_id, &polarity) {
            Ok(()) => {
                updated += 1;
                println!("[{}/{}] edge {} → {polarity}", idx + 1, total, edge_id);
            }
            Err(e) => {
                failed += 1;
                println!(
                    "[{}/{}] edge {} DB write failed: {e}",
                    idx + 1,
                    total,
                    edge_id
                );
            }
        }
    }

    println!("\n=== Polarity backfill complete ===");
    println!("  processed:  {total}");
    println!("  updated:    {updated}");
    println!("  llm failed: {llm_failed}");
    println!("  failed:     {failed}");
    Ok(())
}

// ─── export / import: cross-agent causal sharing (insights/11 §8.5) ────────
