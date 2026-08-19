//! Causal Memory MCP Server — entry point.
//!
//! Two modes:
//! - Default (no args): run as MCP server via stdio
//! - `extract <session-dir>`: one-shot extraction from grok-build session logs

use std::path::PathBuf;

mod bench;
mod bench_agent;
mod bench_tokens;
mod commands;
mod server;
use commands::distill::{run_distill, run_novelty};
use commands::io::{run_export, run_import};
use commands::maintenance::{run_embed, run_judge, run_migrate, run_polarity, run_resolve_updates, run_restore, run_sleep};
use commands::misc::{run_extract, run_http_server, run_link, run_mcp_server, run_reasoning};
use commands::wiki::run_wiki;
use commands::misc::run_refute;

pub(crate) fn get_db_path() -> PathBuf {
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

    // Subcommand: resolve-updates — C7 LLM update-resolver (falsified lessons)
    if args.len() >= 2 && args[1] == "resolve-updates" {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(run_resolve_updates(&args[2..]));
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

    // Subcommand: wiki [--out <dir>] [--format obsidian|html] — export causal
    // memory as an Obsidian markdown vault or standalone interactive HTML graph.
    if args.len() >= 2 && args[1] == "wiki" {
        return run_wiki(&args[2..]);
    }

    // Subcommand: refute — run graph-structural refutation on all edges
    if args.len() >= 2 && args[1] == "refute" {
        return run_refute(&args[2..]);
    }

    // Default: MCP server mode (stdio)
    // --http: HTTP transport mode (remote agents, multi-agent shared memory)
    if args.len() >= 2 && args[1] == "http" {
        return run_http_server(&args[2..]);
    }

    run_mcp_server()
}

/// LLM-distill session file(s) into all memory layers (unified-memory-design
/// Phase 3): one LLM call per session produces facts + lessons/events; facts
/// land in `agent_facts`, lessons/events take the existing record_distilled
/// path into the causal store.
#[cfg(test)]
mod tests {
    use crate::commands::io::{export_jsonl, import_jsonl, redact, ExportFilters};
    use crate::commands::maintenance::load_session;
    use causal_memory::store::CausalStore;

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