//! Causal Memory MCP Server — entry point.
//!
//! Two modes:
//! - Default (no args): run as MCP server via stdio
//! - `extract <session-dir>`: one-shot extraction from grok-build session logs

use std::path::PathBuf;

use causal_memory::{extractor::DecisionExtractor, store::CausalStore};
mod server;
use server::CausalMemoryServer;

fn get_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("CAUSAL_MEMORY_DB") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".local/share/causal-memory")
        .join("causal.db")
}

fn main() -> anyhow::Result<()> {
    // Logging goes to stderr only (stdout is reserved for MCP protocol)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Subcommand: extract <session-dir>
    if args.len() >= 2 && args[1] == "extract" {
        return run_extract(&args[2..]);
    }

    // Subcommand: judge <session-dir> — extract + LLM judge
    if args.len() >= 2 && args[1] == "judge" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_judge(&args[2..]));
    }

    // Subcommand: reasoning <session-dir> — extract reasoning-level decisions via LLM
    if args.len() >= 2 && args[1] == "reasoning" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_reasoning(&args[2..]));
    }

    // Subcommand: link — connect flat decisions into multi-hop chains
    if args.len() >= 2 && args[1] == "link" {
        return run_link();
    }

    // Subcommand: sleep [--db <PATH>] [--dry-run] — offline consolidation cycle
    if args.len() >= 2 && args[1] == "sleep" {
        return run_sleep(&args[2..]);
    }

    // Subcommand: migrate [--db <PATH>] — explicit schema migration check
    if args.len() >= 2 && args[1] == "migrate" {
        return run_migrate(&args[2..]);
    }

    // Subcommand: embed [--db <PATH>] [--limit N] — backfill edge embeddings
    if args.len() >= 2 && args[1] == "embed" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_embed(&args[2..]));
    }

    // Subcommand: polarity [--db <PATH>] [--limit N] — backfill outcome polarity
    if args.len() >= 2 && args[1] == "polarity" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_polarity(&args[2..]));
    }

    // Default: MCP server mode
    run_mcp_server()
}

async fn run_judge(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::llm;

    if args.is_empty() {
        eprintln!("Usage: causal-memory judge <session-dir>");
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

    let session_dir = PathBuf::from(&args[0]);
    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    // First extract (rule-based), then re-judge the high-value ones with LLM
    println!("Extracting from: {}", session_dir.display());
    let stats = DecisionExtractor::extract_from_session(&store, &session_dir)?;
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

fn run_mcp_server() -> anyhow::Result<()> {
    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    tracing::info!("Opening causal memory DB at {}", db_path.display());
    let store = CausalStore::open(&db_path)?;
    let edge_count = store.count_edges().unwrap_or(0);
    tracing::info!("Causal memory ready: {} existing edges", edge_count);

    let server = CausalMemoryServer::new(store);

    use rmcp::ServiceExt;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let transport = (tokio::io::stdin(), tokio::io::stdout());
        let server = server
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
        tracing::info!("MCP server initialized; waiting for shutdown");
        let _ = server.waiting().await;
        tracing::info!("MCP server shut down");
        Ok(())
    })
}

fn run_extract(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: causal-memory extract <session-dir>");
        eprintln!("  session-dir = ~/.grok/sessions/<workspace>/<session-id>/");
        std::process::exit(1);
    }

    let session_dir = PathBuf::from(&args[0]);
    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    println!("Extracting decisions from: {}", session_dir.display());
    let stats = DecisionExtractor::extract_from_session(&store, &session_dir)?;

    println!("\n=== Extraction complete ===");
    println!("  Decisions found:      {}", stats.decisions_found);
    println!("  Results matched:      {}", stats.results_matched);
    println!("  Skipped (low-value):  {}", stats.skipped_low_value);
    println!("  Edges inserted:       {}", stats.edges_inserted);
    println!("\nTotal causal edges in DB: {}", store.count_edges()?);

    Ok(())
}

async fn run_reasoning(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::llm;
    use causal_memory::reasoning_extractor::ReasoningExtractor;

    if args.is_empty() {
        eprintln!("Usage: causal-memory reasoning <session-dir> [max_messages]");
        eprintln!("  Extracts high-value decisions from assistant reasoning text using LLM.");
        eprintln!(
            "  This is the v0.4 feature — captures decisions that tool_call extraction misses."
        );
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

    let session_dir = PathBuf::from(&args[0]);
    let max_messages = args
        .get(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(30);

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    println!(
        "Extracting reasoning-level decisions from: {}",
        session_dir.display()
    );
    println!("Max messages to scan: {}\n", max_messages);

    let stats =
        ReasoningExtractor::extract_from_session(&store, &session_dir, &config, max_messages)
            .await?;

    println!("\n=== Reasoning extraction complete ===");
    println!("  Messages scanned:        {}", stats.messages_scanned);
    println!(
        "  Messages with decisions: {}",
        stats.messages_with_decisions
    );
    println!("  Decisions extracted:     {}", stats.decisions_extracted);
    println!("  Edges inserted:          {}", stats.edges_inserted);
    println!("  LLM calls:               {}", stats.llm_calls);
    println!("  LLM errors:              {}", stats.llm_errors);
    println!("\nTotal causal edges: {}", store.count_edges()?);

    Ok(())
}

fn run_link() -> anyhow::Result<()> {
    use causal_memory::chain_linker::ChainLinker;

    let db_path = get_db_path();
    let store = CausalStore::open(&db_path)?;

    let edge_count = store.count_edges()?;
    println!("=== Causal Chain Linker ===");
    println!(
        "DB: {} ({} edges before linking)\n",
        db_path.display(),
        edge_count
    );

    if edge_count == 0 {
        anyhow::bail!("No edges. Run `extract` or `reasoning` first.");
    }

    let stats = ChainLinker::link_chains(&store)?;

    println!("=== Linking complete ===");
    println!("  Edges scanned:          {}", stats.edges_scanned);
    println!("  Temporal links found:   {}", stats.temporal_links);
    println!("  Text-overlap links:     {}", stats.text_links);
    println!("  Self-loops skipped:     {}", stats.skipped_self);
    println!("  Bridge edges created:   {}", stats.bridge_edges_created);
    println!("\nTotal edges after linking: {}", store.count_edges()?);

    // Verify: check if multi-hop chains now exist
    println!("\n=== Multi-hop chain check ===");
    let multi = store.trace_cause_chain("error", 5, 0.15)?;
    if multi.is_empty() {
        println!("No multi-hop chains found yet.");
        println!("(This is expected if the session didn't have failure→fix→test sequences.)");
    } else {
        println!("Found {} chains! Sample:", multi.len());
        for (i, chain) in multi.iter().take(3).enumerate() {
            println!("  Chain {}: {} hops", i + 1, chain.len());
            for hop in chain.iter().take(3) {
                println!(
                    "    hop {}: {}",
                    hop.hop,
                    &hop.decision_text[..hop.decision_text.len().min(50)]
                );
            }
        }
    }

    Ok(())
}

/// Parse `--db <PATH>` / `--dry-run` style flags for the sleep/migrate subcommands.
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

fn run_sleep(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::consolidate::{consolidate, ConsolidateConfig};

    let flags = parse_db_flags(args, "Usage: causal-memory sleep [--db <PATH>] [--dry-run]")?;
    if let Some(parent) = flags.db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&flags.db)?;
    let now = chrono::Utc::now().timestamp();
    let report = consolidate(&store, &ConsolidateConfig::default(), flags.dry_run, now)?;

    println!(
        "=== Sleep Consolidation Report{} ===",
        if report.dry_run { " (DRY RUN)" } else { "" }
    );
    println!("DB: {}\n", flags.db.display());

    println!(
        "① Reactivation (replay priority, top {}):",
        report.reactivated.len().min(10)
    );
    for (i, entry) in report.reactivated.iter().take(10).enumerate() {
        let snippet: String = entry.decision_text.chars().take(60).collect();
        println!(
            "  {}. [edge {}] score {:.2} — {}",
            i + 1,
            entry.edge_id,
            entry.score,
            snippet
        );
        println!("     ({})", entry.reasons.join(", "));
    }
    if report.reactivated.is_empty() {
        println!("  (no valid edges)");
    }

    println!("\n② Generalization:");
    println!("  redundant edges merged: {}", report.merged_edges);
    println!(
        "  patterns mined: similar_to={} repeated={} contradicts={} refines={}",
        report.mine_report.similar_to,
        report.mine_report.repeated,
        report.mine_report.contradicts,
        report.mine_report.refines
    );

    println!("\n③ Downscaling:");
    println!("  decayed:        {}", report.decayed);
    println!("  access-boosted: {}", report.boosted);
    println!("  GC invalidated: {}", report.gc_invalidated);

    println!("\n④ REM integration:");
    println!("  cross-domain transfers: {}", report.rem_transfers);

    if report.dry_run {
        println!("\n(dry run — no changes were written)");
    }
    Ok(())
}

fn run_migrate(args: &[String]) -> anyhow::Result<()> {
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
async fn run_embed(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::embed::{EmbedConfig, Embedder};

    let mut db: Option<PathBuf> = None;
    let mut limit: usize = 100;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    anyhow::bail!("--db requires a path\nUsage: causal-memory embed [--db <PATH>] [--limit N]");
                };
                db = Some(PathBuf::from(path));
            }
            "--limit" => {
                i += 1;
                let Some(n) = args.get(i) else {
                    anyhow::bail!("--limit requires a number\nUsage: causal-memory embed [--db <PATH>] [--limit N]");
                };
                limit = n
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--limit must be a positive integer, got: {n}"))?;
            }
            other => {
                anyhow::bail!(
                    "unknown flag: {other}\nUsage: causal-memory embed [--db <PATH>] [--limit N]"
                )
            }
        }
        i += 1;
    }

    let config = match EmbedConfig::from_env() {
        Some(c) => c,
        None => {
            eprintln!("Embedding not configured. Set:");
            eprintln!("  CAUSAL_MEMORY_EMBED_API   (default: CAUSAL_MEMORY_LLM_API)");
            eprintln!("  CAUSAL_MEMORY_EMBED_KEY   (default: CAUSAL_MEMORY_LLM_KEY)");
            eprintln!("  CAUSAL_MEMORY_EMBED_MODEL (default: text-embedding-3-small)");
            std::process::exit(1);
        }
    };
    println!("Embedder: {} @ {}", config.model, config.api_base);
    let embedder = Embedder::new(config);

    let db_path = db.unwrap_or_else(get_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;

    let pending = store.edges_without_embedding(limit)?;
    if pending.is_empty() {
        println!("No valid edges missing embeddings. Nothing to do.");
        return Ok(());
    }
    println!("Embedding {} edge(s)...\n", pending.len());

    let total = pending.len();
    let mut success = 0usize;
    let mut failed = 0usize;
    for (idx, (edge_id, text)) in pending.iter().enumerate() {
        match embedder.embed(text).await {
            Ok(vec) => match store.put_embedding(*edge_id, embedder.model(), &vec) {
                Ok(()) => {
                    success += 1;
                    println!("[{}/{}] edge {} ✓", idx + 1, total, edge_id);
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
            },
            Err(e) => {
                failed += 1;
                println!("[{}/{}] edge {} embed failed: {e}", idx + 1, total, edge_id);
            }
        }
    }

    println!("\n=== Embed backfill complete ===");
    println!("  success: {success}");
    println!("  failed:  {failed}");
    Ok(())
}

/// Backfill outcome polarity (v4) for valid edges that don't have one yet.
/// Uses the LLM judge when configured, otherwise the signal-word heuristic.
async fn run_polarity(args: &[String]) -> anyhow::Result<()> {
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
