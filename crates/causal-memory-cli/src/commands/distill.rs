//! Distill pipeline subcommands (distill / recurrence / novelty).

use causal_memory::store::CausalStore;
use crate::get_db_path;
use super::maintenance::load_session;
use std::path::PathBuf;

pub(crate) async fn run_distill(args: &[String]) -> anyhow::Result<()> {
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
pub(crate) async fn run_distill_recurrence(args: &[String]) -> anyhow::Result<()> {
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
pub(crate) fn run_novelty(args: &[String]) -> anyhow::Result<()> {
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

