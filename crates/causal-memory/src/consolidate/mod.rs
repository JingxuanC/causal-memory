//! Phase 4: sleep consolidation — the offline "sleep" cycle.
//!
//! Inspired by the memory-consolidation literature (Schapiro et al. 2017
//! compressed replay; Diekelmann & Born 2010 sleep consolidation):
//!
//! 1. **Reactivation** — score every valid edge for replay priority
//!    (failures, user feedback, contradicted or recently accessed edges
//!    first). Replay here means *re-evaluation*, not playback (Schapiro
//!    2017): the scores feed stage 3, where high-priority edges are
//!    protected (halved decay, lenient GC), and replayed edges are marked
//!    (`last_accessed_at`) so the next cycle can see they were consolidated.
//! 2. **Generalization** — merge redundant duplicate edges, then run the
//!    Phase-3 pattern miner to distil meta edges (hippocampus → neocortex).
//! 3. **Downscaling** — synaptic homeostasis: exponential confidence decay by
//!    age, an access-based boost for recently used edges, and garbage
//!    collection (soft-invalidation) of edges that fell below threshold.
//!    `user_feedback` edges are never garbage-collected; replay-protected
//!    edges (stage 1) decay at half rate and use a lower GC threshold —
//!    retention ∝ priority × recency × confidence, not age alone.
//! 4. **REM integration** — cross-domain transfer: link meta edges whose
//!    patterns are similar but live in disjoint task tags.
//!
//! This is designed as a once-per-day offline job (`causal-memory sleep`).
//! It is NOT idempotent: running it twice in one day decays twice. The report
//! reflects exactly what was (or, with `dry_run`, would be) done.
//!
//! `now` is injected so tests can assert decay math precisely; the CLI passes
//! the system time.

use std::collections::HashSet;

use anyhow::Result;

use crate::llm::LlmConfig;
use crate::patterns::PatternMiner;
use crate::store::CausalStore;

mod stages;
mod types;

pub use types::{ConsolidateConfig, ConsolidateReport, ReactivationEntry, SupersessionAction};

use rusqlite::params;

use stages::{
    downscale, merge_redundant_edges, rem_integrate, replay_writeback, score_reactivation,
    snapshot_meta_edges,
};

/// Run one full sleep-consolidation cycle over `store`.
///
/// With `dry_run = true` every stage computes exactly as usual but no write
/// (merge, mine, decay/boost, GC, transfer) hits the database.
pub fn consolidate(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    now: i64,
) -> Result<ConsolidateReport> {
    let mut report = ConsolidateReport {
        dry_run,
        ..Default::default()
    };

    // ── Stage 0: P6 novelty gate — diverse recent experience? ────────────
    // Token-level Shannon entropy over the most recent chunks, normalized to
    // 0..1. Near-uniform recent text means there is nothing new to
    // consolidate: skip the whole cycle as a no-op (sleep --auto).
    report.diversity = recent_diversity(store, 64)?;
    eprintln!("[consolidate] stage 0 done (diversity {:.2})", report.diversity);
    if report.diversity < config.min_diversity {
        report.skipped_low_diversity = true;
        return Ok(report);
    }

    // ── Stage 1: Reactivation (score → protect in stage 3 → write back) ──
    let scored = score_reactivation(store, config, now)?;
    eprintln!("[consolidate] stage 1 done ({} edges scored)", scored.len());
    let protected: HashSet<i64> = scored
        .iter()
        .filter(|e| e.score >= config.replay_protect_score)
        .map(|e| e.edge_id)
        .collect();
    report.reactivated = scored.into_iter().take(20).collect();

    // ── Stage 1.5: Q-value reinforcement (Bellman) ───────────────────────
    // Replay-protected edges are the "useful" lessons: reward their endpoint
    // chunks (Q ← Q + α·(r + γ·max_next_Q − Q), r = 1.0) and persist to
    // chunks.q_value so the hippocampus seeding (0.5 + 0.5·Q) favors them in
    // the next session. The in-memory graph dies with the process otherwise.
    if !protected.is_empty() {
        if let Ok(mut graph) = crate::hippocampus::CausalGraph::from_store(store) {
            for &edge_id in &protected {
                let Ok(Some(entry)) = store.get_edge(edge_id) else {
                    continue;
                };
                if graph.update_q_value_by_chunk_id(
                    &entry.decision_id,
                    1.0,
                    config.q_alpha as f32,
                    config.q_gamma as f32,
                ) {
                    report.q_updates += 1;
                }
                graph.update_q_value_by_chunk_id(
                    &entry.outcome_id,
                    1.0,
                    config.q_alpha as f32,
                    config.q_gamma as f32,
                );
            }
            if !dry_run {
                graph.persist_q_values(store)?;
            }
        }
    }
    eprintln!("[consolidate] stage 1.5 done ({} q-updates)", report.q_updates);

    // ── Stage 1.7: C7 supersession resolution ───────────────────────────
    // Knowledge-update pass: lessons whose decision chunk was re-recorded
    // with a different outcome may be falsified. The LLM judge decides;
    // superseded edges are soft-invalidated (retired before decay/GC).
    // No LLM configured -> skipped (rule-based contradiction already ran on
    // the write path); failures keep the edge (conservative).
    resolve_supersessions(store, config, dry_run, &mut report)?;
    eprintln!("[consolidate] stage 1.7 done (supersessions resolved)");

    // ── Stage 2: Generalization ─────────────────────────────────────────
    report.merged_edges = merge_redundant_edges(store, dry_run, now)?;
    eprintln!("[consolidate] stage 2a done ({} merged)", report.merged_edges);
    let meta_before = snapshot_meta_edges(store)?;
    let miner = PatternMiner::new(store, config.miner);
    report.mine_report = if dry_run {
        miner.mine_dry_run()?
    } else {
        miner.mine()?
    };
    eprintln!("[consolidate] stage 2b done (mining: {report:?})", report = report.mine_report);

    // ── Stage 3: Downscaling (decay + access boost + GC) ────────────────
    downscale(store, config, dry_run, now, &protected, &mut report)?;
    eprintln!("[consolidate] stage 3 done ({} decayed, {} gc)", report.decayed, report.gc_invalidated);

    // ── Stage 1 write-back: mark replay-protected edges as replayed ─────
    // Runs AFTER downscale so this cycle's access-boost math still sees the
    // pre-replay `last_accessed_at`; the mark takes effect next cycle.
    report.replayed = replay_writeback(store, &protected, dry_run, now)?;

    // ── Stage 4: REM integration (cross-domain transfer) ────────────────
    report.rem_transfers = rem_integrate(store, config, dry_run, &meta_before)?;
    eprintln!("[consolidate] stage 4 done ({} rem transfers)", report.rem_transfers);

    Ok(report)
}

/// Stage 1.7: C7 supersession resolution — LLM-judge repeated-decision
/// candidates and retire the ones the new evidence falsifies.
///
/// Degradation discipline (project-wide): no LLM configured -> no-op;
/// a judge failure keeps the edge (rule-based fallback). `dry_run` counts
/// without writing. LLM calls run on a short-lived tokio runtime — this is
/// an offline sleep cycle, latency is not a concern.
fn resolve_supersessions(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    report: &mut ConsolidateReport,
) -> Result<()> {
    resolve_supersessions_with(
        store,
        config,
        dry_run,
        report,
        LlmConfig::from_env().as_ref(),
        config.supersession_action,
    )
}

/// The LLM judge is injected (`Option`) so tests exercise both the no-LLM
/// and judge-failure paths WITHOUT mutating process env — env writes race
/// under `cargo test`'s parallel harness, and worse, `EmbedConfig::from_env`
/// falls back to the LLM env vars, so a test that sets them can poison the
/// process-global embedder slot (`embed::SLOT`) for every other test.
///
/// Public so the eval harness and the MCP `resolve_updates` tool drive the
/// same pipeline sleep uses — one detection layer, three entry points.
pub fn resolve_supersessions_with(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    report: &mut ConsolidateReport,
    llm: Option<&LlmConfig>,
    action: SupersessionAction,
) -> Result<()> {
    // No LLM configured: the rule-based contradiction pass on the write path
    // is all we have — skip silently (the CLI sleep report notes it when run
    // with --llm expected).
    let Some(llm) = llm else {
        return Ok(());
    };
    let candidates = store.find_falsified_candidates(config.supersession_limit)?;
    if candidates.is_empty() {
        return Ok(());
    }
    let mut superseded = 0usize;
    // One old edge can pair with several newer records: once a verdict
    // retires it, skip the remaining pairs (no re-judge, no duplicate count).
    let mut decided: HashSet<i64> = HashSet::new();
    for (edge_id, new_edge_id, old_d, old_o, new_d, new_o) in &candidates {
        if decided.contains(edge_id) {
            continue;
        }
        // memory::block_on is async-context-safe (block_in_place on an
        // existing runtime handle, fresh runtime otherwise): this stage is
        // reachable from the MCP server's async handlers, where a nested
        // `Runtime::new().block_on` would panic.
        let verdict = crate::memory::block_on(crate::llm::judge_supersession(
            llm, old_d, old_o, new_d, new_o,
        ));
        match verdict {
            Ok(v) if v.supersedes => {
                decided.insert(*edge_id);
                superseded += 1;
                if !dry_run {
                    match action {
                        SupersessionAction::Retire => {
                            store.invalidate_edge(*edge_id)?;
                        }
                        SupersessionAction::Annotate => {
                            store.annotate_superseded(*edge_id, *new_edge_id)?;
                        }
                    }
                }
            }
            _ => {} // keep, or judge failure -> keep (conservative)
        }
    }
    report.superseded_lessons = superseded;
    Ok(())
}

#[cfg(test)]
mod tests;

/// P6: token-level diversity over the `n` most recent chunks — Shannon
/// entropy of the token distribution, normalized by ln(unique tokens) so a
/// uniform distribution scores 1.0. The production novelty signal: high
/// diversity = the store accumulated genuinely new material worth
/// consolidating; near-uniform recent text = nothing new (skip as no-op).
pub fn recent_diversity(store: &CausalStore, n: usize) -> Result<f64> {
    store.with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT text FROM chunks ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![n as i64], |r| r.get::<_, String>(0))?;
        let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut total = 0usize;
        for text in rows {
            for tok in crate::patterns::tokenize(&text?) {
                *freq.entry(tok).or_insert(0) += 1;
                total += 1;
            }
        }
        if total == 0 {
            return Ok(0.0);
        }
        let mut entropy = 0.0;
        for &c in freq.values() {
            let p = c as f64 / total as f64;
            entropy -= p * p.ln();
        }
        let unique = freq.len() as f64;
        Ok(if unique > 1.0 { entropy / unique.ln() } else { 0.0 })
    })
}

