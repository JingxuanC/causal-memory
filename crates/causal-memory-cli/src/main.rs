//! Causal Memory MCP Server — entry point.
//!
//! Two modes:
//! - Default (no args): run as MCP server via stdio
//! - `extract <session-dir>`: one-shot extraction from grok-build session logs

use std::path::PathBuf;

use causal_memory::{extractor::DecisionExtractor, store::CausalStore};
use rusqlite::OptionalExtension;
mod bench;
mod bench_agent;
mod bench_tokens;
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

    // Subcommand: distill <session.json|dir> [--dry-run] — LLM distill into
    // all memory layers (facts → agent_facts, lessons/events → causal store)
    if args.len() >= 2 && args[1] == "distill" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_distill(&args[2..]));
    }

    // Subcommand: sleep [--db <PATH>] [--dry-run] — offline consolidation cycle
    if args.len() >= 2 && args[1] == "sleep" {
        return run_sleep(&args[2..]);
    }

    // Subcommand: restore <edge_id> [--db <PATH>] — reversible consolidation:
    // later evidence proved the old memory right, so roll back a supersession.
    if args.len() >= 2 && args[1] == "restore" {
        return run_restore(&args[2..]);
    }

    // Subcommand: novelty <decision> <actual> [--mode entropy|prediction_gap|hybrid]
    // — novelty gate with the Nemori FEP prediction-gap fallback (P5).
    if args.len() >= 2 && args[1] == "novelty" {
        return run_novelty(&args[2..]);
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

    // Subcommand: export <file.jsonl> — share causal memory across agents
    if args.len() >= 2 && args[1] == "export" {
        return run_export(&args[2..]);
    }

    // Subcommand: import <file.jsonl> — import shared causal memory
    if args.len() >= 2 && args[1] == "import" {
        return run_import(&args[2..]);
    }

    // Subcommand: bench-compaction — reproducible compaction-degradation bench
    if args.len() >= 2 && args[1] == "bench-compaction" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(bench::run(&args[2..]));
    }

    // Subcommand: bench-agent — end-to-end ablation with/without causal memory
    if args.len() >= 2 && args[1] == "bench-agent" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(bench_agent::run(&args[2..]));
    }

    // Subcommand: bench-tokens — token-efficiency benchmark (P6)
    if args.len() >= 2 && args[1] == "bench-tokens" {
        return bench_tokens::run(&args[2..]);
    }

    // Default: MCP server mode
    run_mcp_server()
}

/// LLM-distill session file(s) into all memory layers (unified-memory-design
/// Phase 3): one LLM call per session produces facts + lessons/events; facts
/// land in `agent_facts`, lessons/events take the existing record_distilled
/// path into the causal store.
async fn run_distill(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::distill::{Distiller, ItemKind};

    // v8 (P1): recurrence-gated distill — `--mode recurrence|batch` routes
    // to the RecMem flow (embeddings + recurrence gate); the plain call stays
    // the classic eager path below.
    if args.iter().any(|a| a == "--mode") {
        return run_distill_recurrence(args).await;
    }

    let dry_run = args.iter().any(|a| a == "--dry-run");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    // Fail loudly on anything we don't understand — silently ignoring a
    // misspelled flag (e.g. `--dryrun`) could write when the user asked not to.
    let unknown: Vec<&String> = args
        .iter()
        .filter(|a| a.starts_with("--") && a.as_str() != "--dry-run")
        .collect();
    if !unknown.is_empty() || positional.len() > 1 {
        eprintln!(
            "Unrecognized argument(s): {}",
            unknown
                .iter()
                .map(|s| s.as_str())
                .chain(positional.iter().skip(1).map(|s| s.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        std::process::exit(2);
    }
    if positional.is_empty() {
        eprintln!("Usage: causal-memory distill <session.json|session-dir> [--dry-run]");
        eprintln!(
            "  Session file: {{\"date\": \"YYYY-MM-DD\", \"turns\": [[speaker, message], ...]}}"
        );
        eprintln!("\nRequired env:");
        eprintln!("  CAUSAL_MEMORY_LLM_API   (e.g. https://api.deepseek.com/v1)");
        eprintln!("  CAUSAL_MEMORY_LLM_KEY   (or DEEPSEEK_API_KEY)");
        std::process::exit(1);
    }

    let distiller = match Distiller::from_env() {
        Some(d) => d,
        None => {
            eprintln!("No LLM configured. Set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY (or DEEPSEEK_API_KEY)");
            std::process::exit(1);
        }
    };

    // Collect session files: one JSON file, or every *.json in a directory.
    let path = PathBuf::from(positional[0]);
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(&path)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "json") {
                files.push(p);
            }
        }
        files.sort();
    } else {
        files.push(path.clone());
    }
    if files.is_empty() {
        eprintln!("No .json session files found at {}", path.display());
        std::process::exit(1);
    }

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;
    let embedder =
        causal_memory::embed::EmbedConfig::from_env().map(causal_memory::embed::Embedder::new);

    let mut total_facts = 0usize;
    let mut total_episodes = 0usize;
    let mut total_retired = 0usize;
    let mut sessions_distilled = 0usize;

    for file in &files {
        let (date, turns) = match load_session(file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("⚠️ {}: {e}", file.display());
                continue;
            }
        };
        let items = match distiller.distill_session(&date, &turns).await {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => {
                println!("{}: nothing worth remembering", file.display());
                continue;
            }
            Err(e) => {
                eprintln!("⚠️ distill failed for {}: {e}", file.display());
                continue;
            }
        };
        sessions_distilled += 1;
        println!(
            "{}: {} item(s){}",
            file.display(),
            items.len(),
            if dry_run { " (dry run)" } else { "" }
        );

        for item in &items {
            let kind = match item.kind {
                ItemKind::Fact => "fact",
                ItemKind::Preference => "preference",
                ItemKind::Lesson => "lesson",
                ItemKind::Event => "event",
                ItemKind::Causal => "causal",
            };
            println!("  [{kind}] {}", item.text);
        }
        if !dry_run {
            let (f, e, r) = write_distilled_items(&store, &items, embedder.as_ref()).await?;
            total_facts += f;
            total_episodes += e;
            total_retired += r;
        }
    }

    println!("\n=== Distill complete ===");
    println!("Sessions: {sessions_distilled}/{} distilled", files.len());
    println!("Facts recorded: {total_facts} (outdated retired: {total_retired})");
    println!("Episodes recorded: {total_episodes}");
    Ok(())
}

/// Write distilled items through the standard path shared by eager and
/// recurrence modes: facts/preferences → the fact layer (with supersedes
/// retirement + opportunistic embedding), lessons/events/causal →
/// `record_distilled`. Returns (facts, episodes, retired) — log-and-continue
/// on per-item failures so one bad record never aborts the batch.
async fn write_distilled_items(
    store: &CausalStore,
    items: &[causal_memory::distill::MemoryItem],
    embedder: Option<&causal_memory::embed::Embedder>,
) -> anyhow::Result<(usize, usize, usize)> {
    use causal_memory::distill::ItemKind;
    let mut total_facts = 0usize;
    let mut total_episodes = 0usize;
    let mut total_retired = 0usize;
    for item in items {
        let kind = match item.kind {
            ItemKind::Fact => "fact",
            ItemKind::Preference => "preference",
            ItemKind::Lesson => "lesson",
            ItemKind::Event => "event",
            ItemKind::Causal => "causal",
        };
        match item.kind {
            // Facts/preferences → the fact layer; supersedes retires the
            // outdated value it replaces (edge-layer threshold semantics).
            ItemKind::Fact | ItemKind::Preference => {
                let id = match store.record_fact(kind, &item.text, "user", "distill", 0.8) {
                    Ok(id) => id,
                    Err(e) => {
                        eprintln!("⚠️ record_fact failed ({}): {e}", item.text);
                        continue;
                    }
                };
                total_facts += 1;
                if let Some(hint) = item.supersedes.as_deref() {
                    // exclude the fact we just wrote: a transition fact
                    // ("switched from almond milk to oat milk") mentions the
                    // old value and would otherwise retire itself.
                    total_retired += store
                        .retire_facts_by_hint(kind, "user", hint, Some(id))
                        .unwrap_or(0);
                }
                // Opportunistic embedding (silent on failure).
                if let Some(e) = &embedder {
                    let text = format!("{} {}", kind.replace('_', " "), item.text);
                    if let Ok(vec) = e.embed(&text).await {
                        let _ = store.put_fact_embedding(id, e.model(), &vec);
                    }
                }
            }
            // Lessons/events → the causal store's distilled path (handles
            // its own supersedes-based soft-invalidation).
            ItemKind::Lesson | ItemKind::Event | ItemKind::Causal => {
                if let Err(e) = store.record_distilled(item, None) {
                    eprintln!("⚠️ record_distilled failed ({}): {e}", item.text);
                    continue;
                }
                total_episodes += 1;
            }
        }
    }
    Ok((total_facts, total_episodes, total_retired))
}

/// Deterministic positive session id from a file name (FNV-1a) — the same
/// session file always maps to the same session_logs group.
fn session_id_from_name(name: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash >> 1) as i64
}

/// v8 (P1): recurrence-triggered distill (RecMem, arXiv:2605.16045).
///
///     causal-memory distill --mode recurrence <session.json|dir> [--db PATH] [--threshold F]
///     causal-memory distill --mode batch [--db PATH] [--threshold F]
///
/// Recurrence mode: each session is logged to `session_logs` WITH its
/// embedding, and distilled only when its topic semantically repeats a prior
/// distilled session (cosine ≥ threshold; token savings 50-87% per RecMem).
/// Non-recurrent sessions stay pending for the daily batch drain.
///
/// Batch mode: drain pending sessions — distill those that now recur
/// (merged with the matched session), eagerly distill embedding-less ones,
/// leave the rest pending.
async fn run_distill_recurrence(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::distill::{Distiller, distill_recurrence, distill_undistilled_batch};

    let mut mode = String::new();
    let mut db_override: Option<String> = None;
    let mut threshold = 0.6f32;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> anyhow::Result<&str> {
            *i += 1;
            args.get(*i)
                .map(String::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing value for {}", args[*i - 1]))
        };
        match args[i].as_str() {
            "--mode" => mode = take(&mut i)?.to_string(),
            "--db" => db_override = Some(take(&mut i)?.to_string()),
            "--threshold" => threshold = take(&mut i)?.parse()?,
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}"),
            other => positional.push(other),
        }
        i += 1;
    }
    if !matches!(mode.as_str(), "recurrence" | "batch") {
        anyhow::bail!(
            "--mode must be recurrence|batch (eager is the default mode), got: {mode}"
        );
    }

    let distiller = match Distiller::from_env() {
        Some(d) => d,
        None => {
            eprintln!("No LLM configured. Set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY (or DEEPSEEK_API_KEY)");
            std::process::exit(1);
        }
    };
    let db_path = db_override.map(PathBuf::from).unwrap_or_else(get_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;

    // ── Batch drain: no session files, only pending sessions ─────────────
    if mode == "batch" {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let out = distill_undistilled_batch(&store, &distiller, &today, 100, threshold).await?;
        println!("\n=== Distill batch drain (recurrence) ===");
        println!("distilled (topic recurred):   {}", out.distilled.len());
        for o in &out.distilled {
            println!(
                "  session {} ↔ matched #{} (sim {:.2}) → {} item(s)",
                o.session_id,
                o.matched_session.unwrap_or_default(),
                o.similarity.unwrap_or_default(),
                o.items.len()
            );
        }
        println!("eager fallback (no embedding): {}", out.eager_fallback.len());
        println!("still pending (no recurrence): {}", out.still_pending.len());
        return Ok(());
    }

    // ── Recurrence mode: session files, gated distill ────────────────────
    if positional.is_empty() {
        anyhow::bail!(
            "Usage: causal-memory distill --mode recurrence <session.json|session-dir> [--db PATH] [--threshold F]"
        );
    }
    let path = PathBuf::from(positional[0]);
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(&path)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "json") {
                files.push(p);
            }
        }
        files.sort();
    } else {
        files.push(path.clone());
    }
    if files.is_empty() {
        anyhow::bail!("No .json session files found at {}", path.display());
    }

    let embedder =
        causal_memory::embed::EmbedConfig::from_env().map(causal_memory::embed::Embedder::new);
    if embedder.is_none() {
        eprintln!(
            "⚠️ --mode recurrence needs an embedder for the semantic recurrence check."
        );
        eprintln!("   Set CAUSAL_MEMORY_EMBED_API, or build with --features local-embed.");
        eprintln!("   Falling back to EAGER distill (every session distilled).");
    }

    let mut distilled = 0usize;
    let mut pending = 0usize;
    for file in &files {
        let (date, turns) = match load_session(file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("⚠️ {}: {e}", file.display());
                continue;
            }
        };
        let Some(embedder) = &embedder else {
            // No embedder → the gate cannot run; eager fallback keeps nothing lost.
            let items = distiller.distill_session(&date, &turns).await?;
            let _ = write_distilled_items(&store, &items, None).await?;
            distilled += 1;
            println!("{}: {} item(s) (eager fallback — no embedder)", file.display(), items.len());
            continue;
        };
        let file_name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
        let session_id = session_id_from_name(&file_name);
        let session_text = turns.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
        let embedding = match embedder.embed(&session_text).await {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("⚠️ embed failed for {}: {e}", file.display());
                None
            }
        };
        let outcome = distill_recurrence(
            &store,
            &distiller,
            session_id,
            &turns,
            &date,
            embedding.as_deref(),
            threshold,
        )
        .await?;
        if outcome.distilled {
            let (f, e, r) = write_distilled_items(&store, &outcome.items, Some(embedder)).await?;
            distilled += 1;
            println!(
                "{}: DISTILLED (recurred with session #{}, sim {:.2}) → {} item(s) ({} facts, {} episodes, {} retired)",
                file.display(),
                outcome.matched_session.unwrap_or_default(),
                outcome.similarity.unwrap_or_default(),
                outcome.items.len(),
                f,
                e,
                r
            );
        } else {
            pending += 1;
            println!(
                "{}: PENDING (no recurrence) — stays in session_logs for `distill --mode batch`",
                file.display()
            );
        }
    }
    println!("\n=== Distill complete (recurrence mode) ===");
    println!("Sessions distilled: {distilled}/{}   pending: {pending}", files.len());
    Ok(())
}

/// (P5) World-model prompt for the prediction-gap novelty check (Nemori FEP):
/// given a decision, produce the single most likely concrete outcome.
const NOVELTY_PREDICT_SYSTEM: &str = "You are a world-model simulator. Given a decision or action, predict the single most likely concrete outcome in one short sentence. Output ONLY the predicted outcome — no preamble, no hedging.";

/// (P5) Novelty gate with the FEP prediction-gap fallback.
///
///     causal-memory novelty <decision> <actual> [--mode entropy|prediction_gap|hybrid] [--db <PATH>]
///
/// Entropy mode = the cheap word-frequency surprise (no LLM). PredictionGap
/// mode = LLM predicts the outcome of the decision, surprise is the semantic
/// gap between prediction and the actual outcome. Hybrid = entropy first,
/// only borderline cases (0.4..=0.7) pay for the LLM disambiguation.
///
/// Sync by design: the prediction closure calls `block_on` on its own
/// runtime, which is only legal outside an async context.
fn run_novelty(args: &[String]) -> anyhow::Result<()> {
    use causal_memory::hippocampus::{CausalGraph, NoveltyMode, needs_prediction_gap};

    let mut mode = NoveltyMode::default();
    let mut db: Option<&String> = None;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--mode needs a value"))?;
                mode = NoveltyMode::parse(raw);
            }
            "--db" => {
                i += 1;
                db = args.get(i);
            }
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}"),
            other => positional.push(other),
        }
        i += 1;
    }
    if positional.len() != 2 {
        anyhow::bail!(
            "Usage: causal-memory novelty <decision> <actual> [--mode entropy|prediction_gap|hybrid] [--db <PATH>]"
        );
    }
    let decision = positional[0];
    let actual = positional[1];

    let db_path = db.map(PathBuf::from).unwrap_or_else(get_db_path);
    let store = CausalStore::open(&db_path)?;
    let mut graph = CausalGraph::from_store(&store)?;

    let rt = tokio::runtime::Runtime::new()?;
    let mut predict = |decision_text: &str| -> Option<String> {
        let config = causal_memory::llm::LlmConfig::from_env()?;
        let user_msg = format!(
            "Decision/action: \"{decision_text}\"\nPredict the most likely outcome:"
        );
        rt.block_on(causal_memory::llm::chat(
            &config,
            NOVELTY_PREDICT_SYSTEM,
            &user_msg,
            80,
            0.0,
        ))
        .ok()
    };

    let report = graph.detect_novelty_with_mode(decision, actual, mode, &mut predict);

    println!(
        "=== Novelty gate ({}) ===",
        match mode {
            NoveltyMode::Entropy => "entropy",
            NoveltyMode::PredictionGap => "prediction_gap",
            NoveltyMode::Hybrid => "hybrid",
        }
    );
    println!("decision: \"{decision}\"");
    println!("actual:   \"{actual}\"");
    println!(
        "surprise: {:.2} {} — {}",
        report.surprise,
        if needs_prediction_gap(report.surprise) {
            "(borderline)"
        } else {
            ""
        },
        if report.should_record { "RECORD" } else { "skip" }
    );
    if !report.predicted_positive.is_empty() {
        println!("predicted: {}", report.predicted_positive.join(" | "));
    }
    if !report.predicted_negative.is_empty() {
        println!("warnings:  {}", report.predicted_negative.join(" | "));
    }
    Ok(())
}

/// v8 (P3): reversible consolidation rollback.
///
///     causal-memory restore <edge_id> [--db <PATH>]
///
/// Later evidence proved the old memory right — clear the supersession
/// (`valid_to` + `superseded_by`) and the lesson is live again.
fn run_restore(args: &[String]) -> anyhow::Result<()> {
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
fn load_session(path: &std::path::Path) -> anyhow::Result<(String, Vec<(String, String)>)> {
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
    println!(
        "  → {} edge(s) replay-protected & marked (half decay, lenient GC)",
        report.replayed
    );

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

// ─── export / import: cross-agent causal sharing (insights/11 §8.5) ────────

/// FNV-1a 64-bit hash of a text, as an `imp…` chunk id. Hand-rolled (not
/// std's DefaultHasher) so ids are stable across Rust versions — import
/// dedup relies on "same text → same chunk id" holding forever.
fn fnv1a(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("imp{h:016x}")
}

/// Best-effort secret redaction (no regex dep; documented as best-effort in
/// --help): sk-… tokens, Bearer tokens, password-style assignments, and
/// private-key headers are replaced with [REDACTED]. Returns the redacted
/// text and the number of redactions applied.
fn redact(text: &str) -> (String, usize) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut redacted_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut count = 0;

    for (i, w) in words.iter().enumerate() {
        // sk-<secret> (API keys)
        if let Some(rest) = w.strip_prefix("sk-") {
            let key: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if key.len() >= 8 {
                redacted_idx.insert(i);
                count += 1;
                continue;
            }
        }
        // Bearer <token>
        if *w == "Bearer" {
            if let Some(next) = words.get(i + 1) {
                if next.chars().filter(|c| c.is_alphanumeric()).count() >= 4 {
                    redacted_idx.insert(i + 1);
                    count += 1;
                }
            }
            continue;
        }
        // password=… / password: … (case-insensitive, also passwd). When the
        // value is in the NEXT word ("password: hunter2"), redact it too.
        let lower = w.to_lowercase();
        for prefix in ["password", "passwd"] {
            if let Some(pos) = lower.find(prefix) {
                let after = &w[pos + prefix.len()..];
                if after.starts_with('=') || after.starts_with(':') {
                    redacted_idx.insert(i);
                    count += 1;
                    if after.len() == 1 {
                        if let Some(next) = words.get(i + 1) {
                            if !next.is_empty() {
                                redacted_idx.insert(i + 1);
                            }
                        }
                    }
                }
                break;
            }
        }
        // -----BEGIN … PRIVATE KEY----- (split on whitespace, so the closing
        // word is just "…KEY-----")
        if *w == "-----BEGIN" {
            for j in i + 1..=(i + 3).min(words.len().saturating_sub(1)) {
                if j < words.len() && words[j].ends_with("KEY-----") {
                    for k in i..=j {
                        redacted_idx.insert(k);
                    }
                    count += 1;
                    break;
                }
            }
        }
    }

    if count == 0 {
        return (text.to_string(), 0);
    }
    let mut out_words: Vec<&str> = Vec::with_capacity(words.len());
    for (i, w) in words.iter().enumerate() {
        if redacted_idx.contains(&i) {
            out_words.push("[REDACTED]");
        } else {
            out_words.push(w);
        }
    }
    (out_words.join(" "), count)
}

struct ExportFilters {
    task_tag: Option<String>,
    min_confidence: f64,
    since: i64,
    include_invalidated: bool,
    redact: bool,
}

#[derive(Default, Debug)]
struct ExportStats {
    chunks: usize,
    edges: usize,
    meta_edges: usize,
    redacted: usize,
}

/// Serialize the store to JSONL lines (header first, then chunks, edges,
/// meta edges). Pure read path; fully testable without a file.
fn export_jsonl(
    store: &CausalStore,
    f: &ExportFilters,
) -> anyhow::Result<(Vec<String>, ExportStats)> {
    let mut stats = ExportStats::default();
    let mut lines = Vec::new();
    let mut chunks: std::collections::HashMap<String, (String, i64)> =
        std::collections::HashMap::new();

    lines.push(
        serde_json::json!({
            "type": "header",
            "format_version": 1,
            "exported_at": chrono::Utc::now().timestamp(),
            "source": "causal-memory",
        })
        .to_string(),
    );

    // ── causal edges (filtered) ──
    let mut sql = String::from(
        "SELECT cf.id, cf.text, cf.created_at, ct.id, ct.text, ct.created_at,
                ce.relation, ce.confidence, ce.task_tag, ce.event_time,
                ce.discovered_at, ce.valid_to, ce.discovered_by, ce.outcome_polarity
         FROM causal_edges ce
         JOIN chunks cf ON cf.id = ce.from_id
         JOIN chunks ct ON ct.id = ce.to_id
         WHERE ce.confidence >= ?1 AND ce.event_time >= ?2",
    );
    let mut bind: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(f.min_confidence), Box::new(f.since)];
    if !f.include_invalidated {
        sql.push_str(" AND ce.valid_to IS NULL");
    }
    if let Some(t) = &f.task_tag {
        sql.push_str(" AND ce.task_tag = ?3");
        bind.push(Box::new(t.clone()));
    }
    sql.push_str(" ORDER BY ce.id");

    store.with_conn(|conn| {
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, f64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
            ))
        })?;
        for row in rows {
            let (fid, ftext, fcat, tid, ttext, tcat, rel, conf, tag, et, dat, vto, dby, pol) = row?;
            chunks.entry(fid.clone()).or_insert((ftext, fcat));
            chunks.entry(tid.clone()).or_insert((ttext, tcat));
            lines.push(
                serde_json::json!({
                    "type": "edge",
                    "from_id": fid, "to_id": tid,
                    "relation": rel, "confidence": conf, "task_tag": tag,
                    "event_time": et, "discovered_at": dat, "valid_to": vto,
                    "discovered_by": dby, "outcome_polarity": pol,
                })
                .to_string(),
            );
            stats.edges += 1;
        }

        // ── meta edges (only the invalidated filter applies) ──
        let meta_sql = if f.include_invalidated {
            "SELECT m.from_id, cf.text, cf.created_at, m.to_id, ct.text, ct.created_at,
                    m.relation, m.pattern, m.confidence, m.discovered_at, m.valid_to,
                    m.strata_count, m.strata, m.confounded, m.simpson
             FROM meta_causal_edges m
             JOIN chunks cf ON cf.id = m.from_id
             JOIN chunks ct ON ct.id = m.to_id
             ORDER BY m.id"
        } else {
            "SELECT m.from_id, cf.text, cf.created_at, m.to_id, ct.text, ct.created_at,
                    m.relation, m.pattern, m.confidence, m.discovered_at, m.valid_to,
                    m.strata_count, m.strata, m.confounded, m.simpson
             FROM meta_causal_edges m
             JOIN chunks cf ON cf.id = m.from_id
             JOIN chunks ct ON ct.id = m.to_id
             WHERE m.valid_to IS NULL
             ORDER BY m.id"
        };
        let mut stmt = conn.prepare(meta_sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, f64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<bool>>(13)?,
                row.get::<_, Option<bool>>(14)?,
            ))
        })?;
        for row in rows {
            let (fid, ftext, fcat, tid, ttext, tcat, rel, pat, conf, dat, vto, sc, s, cfd, sim) =
                row?;
            chunks.entry(fid.clone()).or_insert((ftext, fcat));
            chunks.entry(tid.clone()).or_insert((ttext, tcat));
            lines.push(
                serde_json::json!({
                    "type": "meta_edge",
                    "from_id": fid, "to_id": tid,
                    "relation": rel, "pattern": pat, "confidence": conf,
                    "discovered_at": dat, "valid_to": vto,
                    "strata_count": sc, "strata": s,
                    "confounded": cfd, "simpson": sim,
                })
                .to_string(),
            );
            stats.meta_edges += 1;
        }
        Ok(())
    })?;

    // ── chunks (deduped, redacted) — emitted before edges reference them? ──
    // Edges reference chunk ids only, so order is free; chunks go right after
    // the header for readability. Insert at position 1.
    let mut chunk_lines = Vec::new();
    let mut sorted: Vec<_> = chunks.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, (text, created_at)) in sorted {
        let (text, n) = if f.redact { redact(&text) } else { (text, 0) };
        stats.redacted += n;
        chunk_lines.push(
            serde_json::json!({
                "type": "chunk", "id": id, "text": text, "created_at": created_at,
            })
            .to_string(),
        );
        stats.chunks += 1;
    }
    let header = lines.remove(0);
    let mut out = vec![header];
    out.extend(chunk_lines);
    out.extend(lines);
    Ok((out, stats))
}

fn run_export(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "Usage: causal-memory export <file.jsonl> [--db <PATH>] [--task-tag X] [--min-confidence 0.5] [--since <unix_ts>] [--include-invalidated] [--no-redact]
  Text is best-effort redacted (sk-…, Bearer, password=…, private-key headers) unless --no-redact.";
    let mut file: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut f = ExportFilters {
        task_tag: None,
        min_confidence: 0.0,
        since: 0,
        include_invalidated: false,
        redact: true,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--db requires a path\n{USAGE}")
                };
                db = Some(PathBuf::from(p));
            }
            "--task-tag" => {
                i += 1;
                let Some(t) = args.get(i) else {
                    anyhow::bail!("--task-tag requires a value\n{USAGE}")
                };
                f.task_tag = Some(t.clone());
            }
            "--min-confidence" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    anyhow::bail!("--min-confidence requires a number\n{USAGE}")
                };
                f.min_confidence = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--min-confidence must be a float, got: {v}"))?;
            }
            "--since" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    anyhow::bail!("--since requires a unix timestamp\n{USAGE}")
                };
                f.since = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--since must be an integer, got: {v}"))?;
            }
            "--include-invalidated" => f.include_invalidated = true,
            "--no-redact" => f.redact = false,
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}\n{USAGE}"),
            positional => {
                if file.is_some() {
                    anyhow::bail!("unexpected extra argument: {positional}\n{USAGE}");
                }
                file = Some(PathBuf::from(positional));
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("{USAGE}");
        std::process::exit(1);
    };

    let db_path = db.unwrap_or_else(get_db_path);
    let store = CausalStore::open(&db_path)?;
    let (lines, stats) = export_jsonl(&store, &f)?;
    std::fs::write(&file, lines.join("\n") + "\n")?;

    println!("=== Export complete ===");
    println!("DB:   {}", db_path.display());
    println!("File: {}", file.display());
    println!("  chunks:      {}", stats.chunks);
    println!("  edges:       {}", stats.edges);
    println!("  meta edges:  {}", stats.meta_edges);
    println!(
        "  redacted:    {}{}",
        stats.redacted,
        if f.redact {
            ""
        } else {
            " (redaction disabled)"
        }
    );
    Ok(())
}

#[derive(Default, Debug)]
struct ImportStats {
    imported: usize,
    skipped_duplicate: usize,
    skipped_invalid: usize,
}

/// Parse and import JSONL produced by export_jsonl. With `dry_run`, reads and
/// dedup checks run against `store` but nothing is written. Bad lines are
/// counted and skipped, never fatal. Dedup key for edges:
/// (from_text, to_text, relation, event_time); meta edges:
/// (from_text, to_text, relation); chunks: FNV-1a(text) id, INSERT OR IGNORE.
fn import_jsonl(
    store: &CausalStore,
    content: &str,
    task_tag_override: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<ImportStats> {
    let mut stats = ImportStats::default();
    // source chunk id → text (chunk lines precede their referrers in exports)
    let mut chunk_texts: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            stats.skipped_invalid += 1;
            continue;
        };
        match v["type"].as_str() {
            Some("header") => {
                if v["format_version"].as_i64() != Some(1) {
                    anyhow::bail!(
                        "unsupported format_version {:?} (expected 1)",
                        v["format_version"]
                    );
                }
            }
            Some("chunk") => {
                let (Some(id), Some(text)) = (v["id"].as_str(), v["text"].as_str()) else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                chunk_texts.insert(id.to_string(), text.to_string());
                if !dry_run {
                    let created_at = v["created_at"].as_i64().unwrap_or(0);
                    store.with_conn(|conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(text), text, created_at],
                        )?;
                        Ok(())
                    })?;
                }
            }
            Some("edge") => {
                let (Some(fid), Some(tid), Some(rel)) = (
                    v["from_id"].as_str(),
                    v["to_id"].as_str(),
                    v["relation"].as_str(),
                ) else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                let (Some(ftext), Some(ttext)) = (chunk_texts.get(fid), chunk_texts.get(tid))
                else {
                    stats.skipped_invalid += 1; // chunk line missing
                    continue;
                };
                let event_time = v["event_time"].as_i64().unwrap_or(0);
                let tag = task_tag_override
                    .map(String::from)
                    .or_else(|| v["task_tag"].as_str().map(String::from));

                // Dedup: (from_text, to_text, relation, event_time).
                let dup = store.with_conn(|conn| {
                    Ok(conn
                        .query_row(
                            "SELECT ce.id FROM causal_edges ce
                             JOIN chunks cf ON cf.id = ce.from_id
                             JOIN chunks ct ON ct.id = ce.to_id
                             WHERE cf.text = ?1 AND ct.text = ?2 AND ce.relation = ?3 AND ce.event_time = ?4
                             LIMIT 1",
                            rusqlite::params![ftext, ttext, rel, event_time],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?
                        .is_some())
                })?;
                if dup {
                    stats.skipped_duplicate += 1;
                    continue;
                }
                if !dry_run {
                    let confidence = v["confidence"].as_f64().unwrap_or(0.5);
                    let discovered_at = v["discovered_at"]
                        .as_i64()
                        .unwrap_or_else(|| chrono::Utc::now().timestamp());
                    let valid_to = v["valid_to"].as_i64();
                    let discovered_by = v["discovered_by"].as_str().unwrap_or("llm_inferred");
                    let polarity = v["outcome_polarity"].as_str();
                    store.with_conn(|conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ftext), ftext, event_time],
                        )?;
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ttext), ttext, event_time],
                        )?;
                        conn.execute(
                            "INSERT INTO causal_edges
                                 (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, valid_to, task_tag, outcome_polarity)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                            rusqlite::params![
                                fnv1a(ftext), fnv1a(ttext), rel, confidence,
                                discovered_by, event_time, discovered_at, valid_to, tag, polarity
                            ],
                        )?;
                        Ok(())
                    })?;
                }
                stats.imported += 1;
            }
            Some("meta_edge") => {
                let (Some(fid), Some(tid), Some(rel)) = (
                    v["from_id"].as_str(),
                    v["to_id"].as_str(),
                    v["relation"].as_str(),
                ) else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                let (Some(ftext), Some(ttext)) = (chunk_texts.get(fid), chunk_texts.get(tid))
                else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                let dup = store.with_conn(|conn| {
                    Ok(conn
                        .query_row(
                            "SELECT m.id FROM meta_causal_edges m
                             JOIN chunks cf ON cf.id = m.from_id
                             JOIN chunks ct ON ct.id = m.to_id
                             WHERE cf.text = ?1 AND ct.text = ?2 AND m.relation = ?3
                             LIMIT 1",
                            rusqlite::params![ftext, ttext, rel],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?
                        .is_some())
                })?;
                if dup {
                    stats.skipped_duplicate += 1;
                    continue;
                }
                if !dry_run {
                    let confidence = v["confidence"].as_f64().unwrap_or(0.5);
                    let discovered_at = v["discovered_at"]
                        .as_i64()
                        .unwrap_or_else(|| chrono::Utc::now().timestamp());
                    let pattern = v["pattern"].as_str();
                    let valid_to = v["valid_to"].as_i64();
                    store.with_conn(|conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ftext), ftext, discovered_at],
                        )?;
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ttext), ttext, discovered_at],
                        )?;
                        conn.execute(
                            "INSERT INTO meta_causal_edges
                                 (from_id, to_id, relation, pattern, confidence, discovered_at, valid_from, valid_to,
                                  strata_count, strata, confounded, simpson)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                            rusqlite::params![
                                fnv1a(ftext), fnv1a(ttext), rel, pattern, confidence,
                                discovered_at, discovered_at, valid_to,
                                v["strata_count"].as_i64(),
                                v["strata"].as_str(),
                                v["confounded"].as_bool(),
                                v["simpson"].as_bool(),
                            ],
                        )?;
                        Ok(())
                    })?;
                }
                stats.imported += 1;
            }
            _ => stats.skipped_invalid += 1,
        }
    }
    Ok(stats)
}

fn run_import(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str =
        "Usage: causal-memory import <file.jsonl> [--db <PATH>] [--dry-run] [--task-tag Y]
  --task-tag Y tags all imported edges (e.g. the source agent's name).";
    let mut file: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut tag: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--db requires a path\n{USAGE}")
                };
                db = Some(PathBuf::from(p));
            }
            "--dry-run" => dry_run = true,
            "--task-tag" => {
                i += 1;
                let Some(t) = args.get(i) else {
                    anyhow::bail!("--task-tag requires a value\n{USAGE}")
                };
                tag = Some(t.clone());
            }
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}\n{USAGE}"),
            positional => {
                if file.is_some() {
                    anyhow::bail!("unexpected extra argument: {positional}\n{USAGE}");
                }
                file = Some(PathBuf::from(positional));
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("{USAGE}");
        std::process::exit(1);
    };

    let content = std::fs::read_to_string(&file)?;
    let db_path = db.unwrap_or_else(get_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;
    let stats = import_jsonl(&store, &content, tag.as_deref(), dry_run)?;

    println!(
        "=== Import complete{} ===",
        if dry_run { " (DRY RUN)" } else { "" }
    );
    println!("  imported:          {}", stats.imported);
    println!("  skipped_duplicate: {}", stats.skipped_duplicate);
    println!("  skipped_invalid:   {}", stats.skipped_invalid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export_store() -> CausalStore {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision_full(
                "used Redis with mutex lock",
                "deadlock — holder crashed",
                "caused",
                Some("concurrency"),
                0.8,
                "rule",
                1000,
                Some("negative"),
            )
            .unwrap();
        store
            .record_decision_full(
                "switched to channel ownership",
                "race fixed, all tests pass",
                "caused",
                Some("concurrency"),
                0.9,
                "user_feedback",
                2000,
                Some("positive"),
            )
            .unwrap();
        // A meta edge over the two real decision chunks.
        let edges = store.all_valid_edges().unwrap();
        let (d1, d2) = (&edges[0].decision_id, &edges[1].decision_id);
        store
            .upsert_meta_edge_stratified(
                d1,
                d2,
                "contradicts",
                "test pattern",
                0.6,
                Some(&["concurrency".to_string()]),
                Some(false),
                Some(true),
            )
            .unwrap();
        store
    }

    fn default_filters() -> ExportFilters {
        ExportFilters {
            task_tag: None,
            min_confidence: 0.0,
            since: 0,
            include_invalidated: false,
            redact: true,
        }
    }

    #[test]
    fn test_redact_patterns() {
        let (out, n) = redact("use api key sk-1234567890abcdef for this");
        assert!(out.contains("[REDACTED]") && !out.contains("1234567890abcdef"));
        assert_eq!(n, 1);

        let (out, n) = redact("call with Bearer abcdef123456 token");
        assert!(out.contains("Bearer [REDACTED]"));
        assert_eq!(n, 1);

        let (out, n) = redact("set Password= hunter2 in config");
        assert!(!out.contains("hunter2"));
        assert_eq!(n, 1);

        let (out, n) = redact("-----BEGIN RSA PRIVATE KEY----- followed by body");
        assert!(!out.contains("RSA"));
        assert_eq!(n, 1);

        let (out, n) = redact("nothing secret here");
        assert_eq!(out, "nothing secret here");
        assert_eq!(n, 0);
    }

    #[test]
    fn test_export_import_roundtrip_and_idempotency() {
        let src = export_store();
        let (lines, stats) = export_jsonl(&src, &default_filters()).unwrap();
        assert_eq!(stats.edges, 2);
        assert_eq!(stats.chunks, 4);
        assert_eq!(stats.meta_edges, 1);
        assert!(lines[0].contains("\"format_version\":1"));
        let content = lines.join("\n") + "\n";

        // Round-trip into a fresh DB: 2 causal edges + 1 meta edge.
        let dst = CausalStore::open_in_memory().unwrap();
        let s = import_jsonl(&dst, &content, None, false).unwrap();
        assert_eq!(s.imported, 3, "2 edges + 1 meta edge: {s:?}");
        let hits = dst.search_causal(Some("concurrency"), None).unwrap();
        assert_eq!(hits.len(), 2);
        let pol: Vec<Option<String>> = hits.iter().map(|e| e.outcome_polarity.clone()).collect();
        assert!(pol.contains(&Some("negative".to_string())));
        assert!(pol.contains(&Some("positive".to_string())));
        // Meta edge round-trips with its stratification fields.
        let metas = dst.search_patterns(Some("test pattern"), None, 10).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].relation, "contradicts");
        assert_eq!(metas[0].simpson, Some(true));

        // Importing again → everything is a duplicate.
        let s2 = import_jsonl(&dst, &content, None, false).unwrap();
        assert_eq!(s2.imported, 0);
        assert_eq!(s2.skipped_duplicate, 3);
        assert_eq!(dst.count_edges().unwrap(), 2);

        // --task-tag override retags imported edges.
        let dst2 = CausalStore::open_in_memory().unwrap();
        let s3 = import_jsonl(&dst2, &content, Some("agent-b"), false).unwrap();
        assert_eq!(s3.imported, 3);
        let hits = dst2.search_causal(Some("agent-b"), None).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_import_tolerates_bad_lines_and_dry_run() {
        let src = export_store();
        let (lines, _) = export_jsonl(&src, &default_filters()).unwrap();
        let mut content = lines.join("\n");
        content.push_str("\nthis is not json\n{\"type\":\"mystery\"}\n{\"type\":\"edge\"}\n");

        // dry-run: counts but writes nothing.
        let dst = CausalStore::open_in_memory().unwrap();
        let s = import_jsonl(&dst, &content, None, true).unwrap();
        assert_eq!(s.imported, 3);
        assert_eq!(s.skipped_invalid, 3);
        assert_eq!(dst.count_edges().unwrap(), 0, "dry-run writes nothing");

        // bad version header is fatal.
        let err = import_jsonl(
            &dst,
            "{\"type\":\"header\",\"format_version\":99}\n",
            None,
            true,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_export_redacts_and_filters() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision_full(
                "used token sk-1234567890abcdef in header",
                "worked fine",
                "caused",
                Some("security"),
                0.8,
                "rule",
                1000,
                Some("positive"),
            )
            .unwrap();
        store
            .record_decision_full(
                "low confidence note",
                "nothing",
                "caused",
                Some("security"),
                0.3,
                "rule",
                1000,
                Some("neutral"),
            )
            .unwrap();

        // Redaction on by default: secret never leaves the DB.
        let (lines, stats) = export_jsonl(&store, &default_filters()).unwrap();
        let content = lines.join("\n");
        assert!(!content.contains("sk-1234567890abcdef"));
        assert!(content.contains("[REDACTED]"));
        assert!(stats.redacted >= 1);

        // min-confidence filter drops the 0.3 edge.
        let f = ExportFilters {
            min_confidence: 0.5,
            ..default_filters()
        };
        let (_, stats) = export_jsonl(&store, &f).unwrap();
        assert_eq!(stats.edges, 1);

        // since filter drops event_time=1000 edges.
        let f = ExportFilters {
            since: 2000,
            ..default_filters()
        };
        let (_, stats) = export_jsonl(&store, &f).unwrap();
        assert_eq!(stats.edges, 0);

        // task_tag filter misses everything.
        let f = ExportFilters {
            task_tag: Some("other".into()),
            ..default_filters()
        };
        let (_, stats) = export_jsonl(&store, &f).unwrap();
        assert_eq!(stats.edges, 0);

        // invalidated edges excluded by default, included on demand.
        store.invalidate_edge(1).unwrap();
        let (_, stats) = export_jsonl(&store, &default_filters()).unwrap();
        assert_eq!(stats.edges, 1);
        let f = ExportFilters {
            include_invalidated: true,
            ..default_filters()
        };
        let (_, stats) = export_jsonl(&store, &f).unwrap();
        assert_eq!(stats.edges, 2);
    }

    // ─── distill CLI helpers (Phase 3) ────────────────────────────────────

    #[test]
    fn test_load_session_pair_and_object_turns() {
        let dir = std::env::temp_dir().join(format!("cm-distill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Pair form: [[speaker, message], ...]
        let f1 = dir.join("a.json");
        std::fs::write(
            &f1,
            r#"{"date": "2026-07-31", "turns": [["user", "hi"], ["assistant", "hello"]]}"#,
        )
        .unwrap();
        let (date, turns) = load_session(&f1).unwrap();
        assert_eq!(date, "2026-07-31");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0], ("user".to_string(), "hi".to_string()));

        // Object form: [{speaker, message}, ...]
        let f2 = dir.join("b.json");
        std::fs::write(
            &f2,
            r#"{"date": "2026-07-31", "turns": [{"speaker": "user", "message": "ping"}]}"#,
        )
        .unwrap();
        let (_, turns) = load_session(&f2).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].1, "ping");

        // Missing date / missing turns are errors, not panics.
        let f3 = dir.join("c.json");
        std::fs::write(&f3, r#"{"turns": []}"#).unwrap();
        assert!(load_session(&f3).is_err());
        let f4 = dir.join("d.json");
        std::fs::write(&f4, r#"{"date": "2026-07-31"}"#).unwrap();
        assert!(load_session(&f4).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_retire_superseded_facts() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_fact(
                "preference",
                "user likes Bonobo coffee beans",
                "user",
                "distill",
                0.8,
            )
            .unwrap();
        store
            .record_fact("tech_stack", "Redis 7.2", "user", "distill", 0.8)
            .unwrap();

        // A supersedes hint naming the old preference retires it — the
        // unrelated fact is untouched.
        let retired = store
            .retire_facts_by_hint("preference", "user", "Bonobo coffee beans", None)
            .unwrap();
        assert_eq!(retired, 1);
        let facts = store.list_facts(None, 10).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "tech_stack");

        // Key isolation: a hint matching a fact under a DIFFERENT key
        // retires nothing.
        store
            .record_fact(
                "preference",
                "user likes PostgreSQL 16",
                "user",
                "distill",
                0.8,
            )
            .unwrap();
        let retired = store
            .retire_facts_by_hint("config", "user", "PostgreSQL 16", None)
            .unwrap();
        assert_eq!(retired, 0);

        // One-token or weak hints never nuke anything.
        let retired = store
            .retire_facts_by_hint("preference", "user", "PostgreSQL", None)
            .unwrap();
        assert_eq!(retired, 0);

        // Scope isolation: session-scoped retire does not touch user facts.
        let retired = store
            .retire_facts_by_hint("preference", "session", "PostgreSQL 16", None)
            .unwrap();
        assert_eq!(retired, 0);
    }
}
