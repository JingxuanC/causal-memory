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

/// Run graph-structural refutation on all edges in the causal store.
/// Each edge gets a grade A/B/C/D/F based on three tests:
/// confounder (neighbor Jaccard), corroboration (path redundancy),
/// placebo (activation specificity).
pub(crate) fn run_refute(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::hippocampus::CausalGraph;
    use causal_memory::refute::EdgeRefuter;
    use causal_memory::store::CausalStore;
    use crate::get_db_path;

    let db_path = get_db_path();
    let store = CausalStore::open(&db_path)?;
    let edge_count = store.count_edges().unwrap_or(0);
    eprintln!("Loading graph from {} ({edge_count} edges)...", db_path.display());

    let graph = CausalGraph::from_store(&store)?;
    eprintln!("Graph: {} nodes, {} edges ({} valid)",
        graph.num_nodes(), graph.num_edges(), graph.num_valid_edges());

    let refuter = EdgeRefuter::new(&graph);
    let report = refuter.refute_all();

    println!("\n=== Refutation Report ===");
    println!("  Total edges graded: {}", report.graded);

    // Distribution
    println!("\n  Grade distribution:");
    for grade in ['A', 'B', 'C', 'D', 'F'] {
        let count = report.distribution.get(&grade).copied().unwrap_or(0);
        let pct = if report.graded > 0 { 100.0 * count as f64 / report.graded as f64 } else { 0.0 };
        let bar = "█".repeat(count * 40 / report.graded.max(1));
        println!("    {}: {:>4} ({:>5.1}%) {}", grade, count, pct, bar);
    }

    // Sample edges by grade
    println!("\n  Sample edges by grade:");
    for grade in ['A', 'B', 'D', 'F'] {
        let sample: Vec<_> = report.results.iter()
            .filter(|(_, r)| r.grade == grade)
            .take(3)
            .collect();
        if sample.is_empty() { continue; }
        println!("\n    Grade {}:", grade);
        for (edge_idx, result) in sample {
            let from = graph.edge_source_node(*edge_idx);
            let to = graph.edge_target(*edge_idx);
            let from_text = graph.node_text(from as usize);
            let to_text = graph.node_text(to as usize);
            println!("      [{}] {} → {}",
                result.tests.iter()
                    .map(|t| match t.result {
                        causal_memory::refute::TestResult::Robust => "✓",
                        causal_memory::refute::TestResult::Inconclusive => "?",
                        causal_memory::refute::TestResult::Refuted => "✗",
                    })
                    .collect::<Vec<_>>()
                    .join(""),
                &from_text[..from_text.len().min(40)],
                &to_text[..to_text.len().min(40)]);
        }
    }

    // Option: store grades in DB
    if args.iter().any(|a| a == "--store") {
        eprintln!("\n  Storing grades in DB...");
        // Add refutation_grade column if not exists
        store.with_conn(|c| {
            c.execute_batch(
                "ALTER TABLE causal_edges ADD COLUMN refutation_grade TEXT;
                 ALTER TABLE causal_edges ADD COLUMN refutation_detail TEXT;"
            ).ok();
            Ok::<_, anyhow::Error>(())
        })?;

        // Map edge_idx to causal_edges.id — we need the edge IDs from the store.
        // The CausalGraph's CSR edge order matches the from_store loading order,
        // which sorts by event_time. We need to re-query to get the DB row IDs.
        let edge_ids: Vec<i64> = store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM causal_edges WHERE valid_to IS NULL ORDER BY event_time ASC"
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;

        for (edge_idx, result) in &report.results {
            if let Some(&edge_id) = edge_ids.get(*edge_idx) {
                let detail_json = serde_json::to_string(
                    &result.tests.iter().map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "result": match t.result {
                                causal_memory::refute::TestResult::Robust => "robust",
                                causal_memory::refute::TestResult::Inconclusive => "inconclusive",
                                causal_memory::refute::TestResult::Refuted => "refuted",
                            },
                            "score": t.score,
                            "detail": t.detail,
                        })
                    }).collect::<Vec<_>>()
                ).unwrap_or_default();

                store.with_conn(|c| {
                    c.execute(
                        "UPDATE causal_edges SET refutation_grade = ?1, refutation_detail = ?2 WHERE id = ?3",
                        rusqlite::params![result.grade.to_string(), detail_json, edge_id],
                    )?;
                    Ok::<_, anyhow::Error>(())
                })?;
            }
        }
        eprintln!("  Stored grades for {} edges", report.graded);
    }

    Ok(())
}

/// Run the MCP server in HTTP (Streamable HTTP) mode instead of stdio.
/// This enables remote agents and multi-agent shared memory.
pub(crate) fn run_http_server(args: &[String]) -> anyhow::Result<()> {
    let mut port: u16 = 9938;
    let mut host = "0.0.0.0".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(port);
            }
            "--host" => {
                i += 1;
                host = args.get(i).cloned().unwrap_or(host);
            }
            _ => {}
        }
        i += 1;
    }

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    eprintln!("Opening causal memory DB at {}", db_path.display());
    let store = CausalStore::open(&db_path)?;
    let edge_count = store.count_edges().unwrap_or(0);
    eprintln!("Causal memory ready: {} existing edges", edge_count);
    eprintln!("Starting MCP HTTP server on {host}:{port}/mcp");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
        };
        use std::sync::Arc;

        // Each connection gets a fresh server instance backed by the same store.
        let store_db_path = db_path.clone();
        let config = StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true);
        let service = StreamableHttpService::new(
            move || {
                let store = CausalStore::open(&store_db_path)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                Ok(CausalMemoryServer::new(store))
            },
            Arc::new(rmcp::transport::streamable_http_server::session::never::NeverSessionManager::default()),
            config,
        );

        let app = axum::Router::new()
            .route_service("/mcp", service)
            .route("/health", axum::routing::get(|| async { "ok" }));

        let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind {host}:{port}: {e}"))?;
        eprintln!("Listening on http://{host}:{port}/mcp");
        eprintln!("Health check: http://{host}:{port}/health");

        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))?;
        Ok(())
    })
}

pub(crate) fn run_extract(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::session::{default_source_kind, parser_for, SessionSource};
    use causal_memory::distill::{Distiller, ItemKind};

    let (agent, session_dir) = crate::commands::parse_agent_path(args);
    if session_dir.as_os_str().is_empty() {
        eprintln!("Usage: causal-memory extract <session-dir|session-file> [--agent grok|claude|kimi|codex]");
        eprintln!("  Extracts causal memories from agent reasoning text using LLM distill.");
        eprintln!();
        eprintln!("  grok:   session-dir = ~/.grok/sessions/<workspace>/<session-id>/");
        eprintln!("  claude: session-file = ~/.claude/projects/<project>/<session>.jsonl");
        eprintln!("  kimi:   session-file = ~/.openclaw/agents/*/sessions/*.jsonl");
        eprintln!("  codex:  session-file = ~/.codex/sessions/**/*.jsonl");
        std::process::exit(1);
    }

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    // Parse the session
    let source = SessionSource {
        path: session_dir.clone(),
        kind: default_source_kind(agent),
    };
    let parsed = parser_for(agent).parse(&source)?;

    println!(
        "Extracting memories from: {} (agent={agent:?})",
        session_dir.display()
    );
    println!("  {} assistant messages, {} tool calls",
        parsed.assistant_texts.len(), parsed.decisions.len());

    // Build conversation turns from assistant reasoning texts
    // (not tool calls — we extract from the agent's THINKING, not its ACTIONS)
    let now = chrono::Utc::now();
    let date = now.format("%Y-%m-%d").to_string();
    let embedder = causal_memory::embed::EmbedConfig::from_env()
        .map(causal_memory::embed::Embedder::new);

    let distiller = match Distiller::from_env() {
        Some(d) => {
            println!("LLM: {} @ {}", d.model(), distiller_api_base(&d));
            d
        }
        None => {
            eprintln!("No LLM configured. Set DEEPSEEK_API_KEY or CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY");
            std::process::exit(1);
        }
    };

    // Group assistant texts into sessions (batch every 15 messages to stay
    // within LLM context limits)
    let batch_size = 15;
    let mut total_facts = 0usize;
    let mut total_episodes = 0usize;
    let mut total_causal = 0usize;
    let mut batches = 0;

    let texts: Vec<&String> = parsed.assistant_texts.iter()
        .filter(|t| t.len() >= 100) // skip trivial messages
        .collect();

    println!("  {} non-trivial messages to distill", texts.len());

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        for chunk in texts.chunks(batch_size) {
            // Join messages into a single "conversation" for the distiller
            let turns: Vec<(String, String)> = chunk
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    let speaker = if i % 2 == 0 { "assistant" } else { "user" };
                    (speaker.to_string(), (*text).clone())
                })
                .collect();

            if turns.is_empty() {
                continue;
            }

            batches += 1;
            let items = match distiller.distill_session(&date, &turns).await {
                Ok(items) if !items.is_empty() => items,
                Ok(_) => {
                    println!("  batch {}: nothing worth remembering", batches);
                    continue;
                }
                Err(e) => {
                    eprintln!("  batch {} distill failed: {e}", batches);
                    continue;
                }
            };

            print!("  batch {}: {} items:", batches, items.len());
            for item in &items {
                let kind_str: String = match item.kind {
                    ItemKind::Fact => "fact".into(),
                    ItemKind::Preference => "pref".into(),
                    ItemKind::Lesson => "lesson".into(),
                    ItemKind::Event => "event".into(),
                    ItemKind::Causal => {
                        let rel = item.causal_relation
                            .map(|r| r.as_str())
                            .unwrap_or("caused");
                        total_causal += 1;
                        // Write causal edge
                        let now_ts = chrono::Utc::now().timestamp();
                        let decision = item.decision.as_deref().unwrap_or("decision");
                        let conf = match rel {
                            "caused" => 0.7,
                            "prevented" => 0.8,
                            "enabled" => 0.6,
                            _ => 0.5,
                        };
                        let _ = store.record_decision_at(
                            decision, &item.text, rel,
                            Some("reasoning"), conf, "llm_inferred", now_ts,
                        );
                        format!("causal({})", rel)
                    }
                    _ => "?".to_string(),
                };

                if item.kind != ItemKind::Causal {
                    let key = match item.kind {
                        ItemKind::Fact => { total_facts += 1; "fact" }
                        ItemKind::Preference => { total_facts += 1; "preference" }
                        ItemKind::Lesson => { total_facts += 1; "lesson" }
                        _ => { total_episodes += 1; "event" }
                    };
                    let _ = store.record_fact(key, &item.text, "user", "reasoning", 0.8);
                }

                print!(" [{}]", kind_str);
            }
            println!();
        }
        Ok::<_, anyhow::Error>(())
    })?;

    println!("\n=== Extraction complete ===");
    println!("  Batches distilled:     {}", batches);
    println!("  Facts/preferences:     {}", total_facts);
    println!("  Causal edges:          {}", total_causal);
    println!("  Other episodes:        {}", total_episodes);
    println!("  Total memories:        {}", total_facts + total_causal + total_episodes);
    println!("\nTotal causal edges in DB: {}", store.count_edges()?);

    Ok(())
}

/// Get the API base from a Distiller (for logging).
fn distiller_api_base(_d: &causal_memory::distill::Distiller) -> String {
    // Distiller doesn't expose api_base directly, but we know it's DeepSeek
    "https://api.deepseek.com/v1".to_string()
}

pub(crate) async fn run_reasoning(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::llm;
    use causal_memory::reasoning_extractor::ReasoningExtractor;
    use causal_memory::session::{default_source_kind, parser_for, SessionSource};

    let (agent, session_dir) = crate::commands::parse_agent_path(args);
    if session_dir.as_os_str().is_empty() {
        eprintln!("Usage: causal-memory reasoning <session-dir|session-file> [max_messages] [--agent grok|claude]");
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

    let max_messages = args
        .iter()
        .find_map(|s| s.parse::<usize>().ok())
        .unwrap_or(30);

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let store = CausalStore::open(&db_path)?;

    let source = SessionSource {
        path: session_dir.clone(),
        kind: default_source_kind(agent),
    };
    let parsed = parser_for(agent).parse(&source)?;

    println!(
        "Extracting reasoning-level decisions from: {} (agent={agent:?})",
        session_dir.display()
    );
    println!("Max messages to scan: {}\n", max_messages);

    let stats =
        ReasoningExtractor::extract_from_parsed(&store, &parsed, &config, max_messages).await?;

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
