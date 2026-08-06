//! Misc subcommands (mcp / extract / reasoning / link).

use causal_memory::extractor::DecisionExtractor;
use crate::server::CausalMemoryServer;
use causal_memory::store::CausalStore;
use crate::get_db_path;
use std::path::PathBuf;

pub(crate) fn run_mcp_server() -> anyhow::Result<()> {
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

pub(crate) fn run_extract(args: &[String]) -> anyhow::Result<()> {
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

pub(crate) async fn run_reasoning(args: &[String]) -> anyhow::Result<()> {
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

pub(crate) fn run_link() -> anyhow::Result<()> {
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
