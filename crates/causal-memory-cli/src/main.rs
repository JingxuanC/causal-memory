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
    println!("Extracted {} edges (rule-based). Now re-judging top entries with LLM...\n", stats.edges_inserted);

    // Get top edges by rule-based confidence (high-value first), re-judge with LLM
    let recent = store.top_decisions_by_confidence(20)?;
    println!("LLM-judging {} highest-confidence decisions:\n", recent.len());

    for (i, entry) in recent.iter().enumerate() {
        let decision = &entry.decision_snippet;
        let outcome = &entry.outcome_snippet;

        match llm::judge_causality(&config, decision, outcome).await {
            Ok((confidence, reasoning)) => {
                println!("{}. [{}] {:.0}% — {}", i + 1, entry.task_tag.as_deref().unwrap_or("?"), confidence * 100.0, decision);
                println!("   → {}", outcome);
                println!("   LLM: {} (\"{}\")\n", confidence, reasoning);
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
        server
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
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
        eprintln!("  This is the v0.4 feature — captures decisions that tool_call extraction misses.");
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
    let max_messages = args.get(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(30);

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    println!("Extracting reasoning-level decisions from: {}", session_dir.display());
    println!("Max messages to scan: {}\n", max_messages);

    let stats = ReasoningExtractor::extract_from_session(
        &store, &session_dir, &config, max_messages,
    ).await?;

    println!("\n=== Reasoning extraction complete ===");
    println!("  Messages scanned:        {}", stats.messages_scanned);
    println!("  Messages with decisions: {}", stats.messages_with_decisions);
    println!("  Decisions extracted:     {}", stats.decisions_extracted);
    println!("  Edges inserted:          {}", stats.edges_inserted);
    println!("  LLM calls:               {}", stats.llm_calls);
    println!("  LLM errors:              {}", stats.llm_errors);
    println!("\nTotal causal edges: {}", store.count_edges()?);

    Ok(())
}
