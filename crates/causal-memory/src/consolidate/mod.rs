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

use crate::patterns::PatternMiner;
use crate::store::CausalStore;

mod stages;
mod types;

pub use types::{ConsolidateConfig, ConsolidateReport, ReactivationEntry};

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

    // ── Stage 1: Reactivation (score → protect in stage 3 → write back) ──
    let scored = score_reactivation(store, config, now)?;
    let protected: HashSet<i64> = scored
        .iter()
        .filter(|e| e.score >= config.replay_protect_score)
        .map(|e| e.edge_id)
        .collect();
    report.reactivated = scored.into_iter().take(20).collect();

    // ── Stage 2: Generalization ─────────────────────────────────────────
    report.merged_edges = merge_redundant_edges(store, dry_run, now)?;
    let meta_before = snapshot_meta_edges(store)?;
    let miner = PatternMiner::new(store, config.miner);
    report.mine_report = if dry_run {
        miner.mine_dry_run()?
    } else {
        miner.mine()?
    };

    // ── Stage 3: Downscaling (decay + access boost + GC) ────────────────
    downscale(store, config, dry_run, now, &protected, &mut report)?;

    // ── Stage 1 write-back: mark replay-protected edges as replayed ─────
    // Runs AFTER downscale so this cycle's access-boost math still sees the
    // pre-replay `last_accessed_at`; the mark takes effect next cycle.
    report.replayed = replay_writeback(store, &protected, dry_run, now)?;

    // ── Stage 4: REM integration (cross-domain transfer) ────────────────
    report.rem_transfers = rem_integrate(store, config, dry_run, &meta_before)?;

    Ok(report)
}

#[cfg(test)]
mod tests;
