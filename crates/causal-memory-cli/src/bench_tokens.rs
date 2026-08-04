//! `bench-tokens`: token-efficiency benchmark (P6).
//!
//! Measures the estimated context cost of three retrieval strategies on a
//! real DB, per query:
//! - **raw top-k**: BM25 edges, full-detail (l2) formatting — the baseline.
//! - **rrf top-k**: BM25 edges ⊕ substring-LIKE edges, RRF-fused — the
//!   cross-view agreement floats to the top.
//! - **layered**: L0 directory (recent decisions) + L1 overview + L2 top-3 —
//!   the query-time construction; cheapest per query by design.
//!
//! Tokens are estimated with `estimate_tokens` (CJK-aware, deterministic,
//! dependency-free). Relative numbers are what matter: choosing a default
//! retrieval strategy, and measuring the cost of context loading per query.
//!
//! Usage:
//!   causal-memory bench-tokens --db <PATH> --queries <file> [--topk N]
//!   queries file: one query per line.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use causal_memory::store::{CausalEntry, CausalStore};

use causal_memory::token::estimate_tokens;
use crate::server::{format_entry_layered, rrf_fuse};

pub fn run(args: &[String]) -> Result<()> {
    let mut db_path: Option<&String> = None;
    let mut queries_path: Option<&String> = None;
    let mut topk = 10usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                db_path = args.get(i);
            }
            "--queries" => {
                i += 1;
                queries_path = args.get(i);
            }
            "--topk" => {
                i += 1;
                topk = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--topk needs a value"))?
                    .parse()?;
            }
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }
    let Some(queries_path) = queries_path else {
        anyhow::bail!(
            "Usage: causal-memory bench-tokens --db <PATH> --queries <file> [--topk N]"
        );
    };
    let queries: Vec<String> = std::fs::read_to_string(queries_path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if queries.is_empty() {
        anyhow::bail!("no queries in {queries_path}");
    }
    let db_path = db_path.map(PathBuf::from).unwrap_or_else(crate::get_db_path);
    let store = CausalStore::open(&db_path)?;

    let mut raw_tokens = 0usize;
    let mut rrf_tokens = 0usize;
    let mut layered_tokens = 0usize;

    println!("# bench-tokens (token efficiency, P6)\n");
    println!("db: {}   queries: {}   topk: {topk}\n", db_path.display(), queries.len());
    println!("| query | strategy | entries | est. tokens |");
    println!("|---|---|---|---|");

    for q in &queries {
        // ── raw top-k: BM25 edges, l2 (full detail) ──────────────────────
        let raw = store.search_causal_bm25(None, q, topk).unwrap_or_default();
        let mut raw_lines = String::new();
        for (rank, e) in raw.iter().take(topk).enumerate() {
            let (line, _) = format_entry_layered(e, rank + 1, "l2");
            raw_lines.push_str(&line);
        }
        let raw_n = estimate_tokens(&raw_lines);
        raw_tokens += raw_n;

        // ── rrf top-k: BM25 ⊕ substring-LIKE, RRF-fused ───────────────────
        let like = store.search_causal(None, Some(q)).unwrap_or_default();
        let raw_keys: Vec<String> = raw.iter().map(|e| e.edge_id.to_string()).collect();
        let like_keys: Vec<String> = like.iter().map(|e| e.edge_id.to_string()).collect();
        let fused = rrf_fuse(&raw_keys, &like_keys);
        let by_id: HashMap<i64, CausalEntry> = raw
            .iter()
            .cloned()
            .chain(like.iter().cloned())
            .map(|e| (e.edge_id, e))
            .collect();
        let rrf_hits: Vec<CausalEntry> = fused
            .iter()
            .filter_map(|(k, _)| k.parse::<i64>().ok())
            .filter_map(|id| by_id.get(&id).cloned())
            .take(topk)
            .collect();
        let mut rrf_lines = String::new();
        for (rank, e) in rrf_hits.iter().enumerate() {
            let (line, _) = format_entry_layered(e, rank + 1, "l2");
            rrf_lines.push_str(&line);
        }
        let rrf_n = estimate_tokens(&rrf_lines);
        rrf_tokens += rrf_n;

        // ── layered: L0 directory + L1 overview + L2 top-3 ───────────────
        let mut layered_lines = String::new();
        // L0: recent decisions — the pinned system-prompt directory.
        for (rank, d) in store
            .recent_decisions(3)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            layered_lines.push_str(&format!(
                "{}. [{}] {} →({})→ {}\n",
                rank + 1,
                d.task_tag.as_deref().unwrap_or("untagged"),
                d.decision_snippet,
                d.relation,
                d.outcome_snippet,
            ));
        }
        // L1: overview of the top-5 raw hits (one-line summaries).
        for (rank, e) in rrf_hits.iter().take(5).enumerate() {
            let (line, _) = format_entry_layered(e, rank + 1, "l1");
            layered_lines.push_str(&line);
        }
        // L2: top-3 full detail.
        for (rank, e) in rrf_hits.iter().take(3).enumerate() {
            let (line, _) = format_entry_layered(e, rank + 1, "l2");
            layered_lines.push_str(&line);
        }
        let layered_n = estimate_tokens(&layered_lines);
        layered_tokens += layered_n;

        println!("| {q} | raw | {} | {raw_n} |", raw.len().min(topk));
        println!("|   | rrf | {} | {rrf_n} |", rrf_hits.len());
        println!(
            "|   | layered | {} | {layered_n} |",
            rrf_hits.len().min(5) + rrf_hits.len().min(3) + 3
        );
    }

    let n = queries.len() as f64;
    println!("\n## Totals ({} queries)", queries.len());
    println!("| strategy | total tokens | avg tokens/query |");
    println!("|---|---|---|");
    println!(
        "| raw top-{topk} (l2)      | {raw_tokens} | {:.0} |",
        raw_tokens as f64 / n
    );
    println!(
        "| rrf top-{topk} (l2)      | {rrf_tokens} | {:.0} |",
        rrf_tokens as f64 / n
    );
    println!(
        "| layered (l0+l1+l2)      | {layered_tokens} | {:.0} |",
        layered_tokens as f64 / n
    );
    Ok(())
}
