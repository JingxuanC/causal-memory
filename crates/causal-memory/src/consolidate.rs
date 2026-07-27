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

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::patterns::{jaccard, tokenize, MineReport, MinerConfig, PatternMiner};
use crate::store::{outcome_polarity, outcomes_contradict, CausalStore};

/// Seconds per day, for age/window math.
const SECS_PER_DAY: f64 = 86_400.0;

/// Consolidation tuning knobs.
#[derive(Debug, Clone, Copy)]
pub struct ConsolidateConfig {
    /// Per-day multiplicative confidence decay (stage 3).
    pub decay_per_day: f64,
    /// Additive confidence boost for edges accessed within the window (stage 3).
    pub access_boost: f64,
    /// Recency window (days) for the access boost (stage 3).
    pub access_boost_window_days: u32,
    /// Hard cap on confidence after boosting (stage 3).
    pub confidence_cap: f64,
    /// Soft-invalidate edges below this confidence after decay+boost (stage 3).
    pub gc_threshold: f64,
    /// Replay-priority score at/above which an edge is protected (stage 1→3).
    /// Default 1.0: reached by failure lessons (conf ≥ 0.5 + 0.5), most
    /// user_feedback edges, and high-confidence contradicted edges.
    pub replay_protect_score: f64,
    /// Decay-days divisor for replay-protected edges (stage 3): 2.0 = half-rate
    /// decay.
    pub replay_decay_divisor: f64,
    /// GC threshold for replay-protected edges (stage 3), more lenient than
    /// `gc_threshold`.
    pub replay_gc_threshold: f64,
    /// Pattern-miner configuration, reused for stages 2 and 4.
    pub miner: MinerConfig,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            decay_per_day: 0.99,
            access_boost: 0.05,
            access_boost_window_days: 7,
            confidence_cap: 0.95,
            gc_threshold: 0.2,
            replay_protect_score: 1.0,
            replay_decay_divisor: 2.0,
            replay_gc_threshold: 0.1,
            miner: MinerConfig::default(),
        }
    }
}

/// One scored edge from the reactivation (replay-priority) pass.
#[derive(Debug, Clone)]
pub struct ReactivationEntry {
    pub edge_id: i64,
    /// Decision text, for human-readable reports.
    pub decision_text: String,
    pub score: f64,
    /// Why this score: e.g. "base confidence", "outcome failed (+0.5)".
    pub reasons: Vec<String>,
}

/// What one consolidation cycle did (or would do, when `dry_run`).
#[derive(Debug, Default)]
pub struct ConsolidateReport {
    /// Stage 1: replay-priority queue, score-descending, top 20.
    pub reactivated: Vec<ReactivationEntry>,
    /// Stage 1 write-back: replay-protected edges marked with
    /// `last_accessed_at = now` (decay halved + lenient GC this cycle, and
    /// visible as "replayed" to the next cycle).
    pub replayed: usize,
    /// Stage 2a: redundant duplicate edges merged away.
    pub merged_edges: usize,
    /// Stage 2b: pattern-miner result.
    pub mine_report: MineReport,
    /// Stage 3: edges whose confidence actually decayed (age ≥ 1 day).
    pub decayed: usize,
    /// Stage 3: edges that received the access boost.
    pub boosted: usize,
    /// Stage 3: edges soft-invalidated by garbage collection.
    pub gc_invalidated: usize,
    /// Stage 4: cross-domain transfer meta edges written.
    pub rem_transfers: usize,
    pub dry_run: bool,
}

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

/// Stage 1: replay-priority score for every valid edge.
///
/// score = confidence
///       + 0.5 if the outcome is a clear failure (emotional salience)
///       + 0.3 if discovered by user feedback (high reward)
///       + 0.2 if a similar decision elsewhere has a contradicting outcome
///       + 0.2 if recently accessed (replayed by read paths or a previous
///         sleep cycle — the consolidation feedback loop)
///
/// Returns ALL edges, score-descending (ties broken by edge id); the caller
/// truncates for the report and derives the protected set (score ≥
/// `config.replay_protect_score`) for stage 3.
fn score_reactivation(
    store: &CausalStore,
    config: &ConsolidateConfig,
    now: i64,
) -> Result<Vec<ReactivationEntry>> {
    let edges = store.all_valid_edges()?;
    let tokens: Vec<Vec<String>> = edges.iter().map(|e| tokenize(&e.decision_text)).collect();
    let window_secs = i64::from(config.access_boost_window_days) * SECS_PER_DAY as i64;

    // Flag edges that participate in a contradiction pair.
    let mut contradicted = vec![false; edges.len()];
    for i in 0..edges.len() {
        for j in i + 1..edges.len() {
            if contradicted[i] && contradicted[j] {
                continue;
            }
            if jaccard(&tokens[i], &tokens[j]) >= config.miner.similarity_threshold
                && outcomes_contradict(&edges[i].outcome_text, &edges[j].outcome_text)
            {
                contradicted[i] = true;
                contradicted[j] = true;
            }
        }
    }

    let mut entries: Vec<ReactivationEntry> = edges
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut score = e.confidence;
            let mut reasons = vec![format!("base confidence {:.2}", e.confidence)];
            if outcome_polarity(&e.outcome_text) == Some(false) {
                score += 0.5;
                reasons.push("outcome failed (+0.5)".to_string());
            }
            if e.discovered_by == "user_feedback" {
                score += 0.3;
                reasons.push("user feedback (+0.3)".to_string());
            }
            if contradicted[i] {
                score += 0.2;
                reasons.push("contradicted elsewhere (+0.2)".to_string());
            }
            if e.last_accessed_at
                .is_some_and(|last| now - last <= window_secs)
            {
                score += 0.2;
                reasons.push("recently accessed (+0.2)".to_string());
            }
            if score >= config.replay_protect_score {
                reasons.push("replay-protected (half decay, lenient GC)".to_string());
            }
            ReactivationEntry {
                edge_id: e.edge_id,
                decision_text: e.decision_text.clone(),
                score,
                reasons,
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.edge_id.cmp(&b.edge_id))
    });
    Ok(entries)
}

/// Stage 2a: merge duplicate valid edges sharing (from_id, to_id, relation).
///
/// The survivor is the edge with the highest confidence (tie → latest
/// event_time, then latest edge id); all others are soft-invalidated.
/// The survivor's confidence is left unchanged. Returns the number merged.
fn merge_redundant_edges(store: &CausalStore, dry_run: bool, now: i64) -> Result<usize> {
    let edges = store.all_valid_edges()?;
    let mut groups: HashMap<(String, String, String), Vec<_>> = HashMap::new();
    for e in edges {
        groups
            .entry((
                e.decision_id.clone(),
                e.outcome_id.clone(),
                e.relation.clone(),
            ))
            .or_default()
            .push(e);
    }

    let mut merged = 0;
    for (_key, mut group) in groups {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.event_time.cmp(&a.event_time))
                .then(b.edge_id.cmp(&a.edge_id))
        });
        for loser in group.iter().skip(1) {
            merged += 1;
            if !dry_run {
                store.with_conn(|conn| {
                    conn.execute(
                        "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
                        rusqlite::params![now, loser.edge_id],
                    )?;
                    Ok(())
                })?;
            }
        }
    }
    Ok(merged)
}

/// Stage 3: per-edge decay, access boost, and garbage collection.
///
/// `protected` is stage 1's replay-priority set: those edges decay at half
/// rate (`replay_decay_divisor`) and are GC'd only below the more lenient
/// `replay_gc_threshold` — replay is re-evaluation, so a lesson the cycle
/// just replayed is harder to forget.
fn downscale(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    now: i64,
    protected: &HashSet<i64>,
    report: &mut ConsolidateReport,
) -> Result<()> {
    // Re-fetch: stage 2a may have invalidated some edges.
    let edges = store.all_valid_edges()?;
    let window_secs = i64::from(config.access_boost_window_days) * SECS_PER_DAY as i64;

    for e in &edges {
        let is_protected = protected.contains(&e.edge_id);
        let mut new_conf = e.confidence;
        let mut changed = false;

        // Decay only edges at least one full day old; same-day edges are untouched.
        let days = (now - e.discovered_at) as f64 / SECS_PER_DAY;
        if days >= 1.0 {
            let effective_days = if is_protected {
                days / config.replay_decay_divisor
            } else {
                days
            };
            new_conf *= config.decay_per_day.powf(effective_days);
            report.decayed += 1;
            changed = true;
        }

        // Access boost for recently-read edges, applied after decay, capped.
        if let Some(last) = e.last_accessed_at {
            if now - last <= window_secs {
                new_conf = (new_conf + config.access_boost).min(config.confidence_cap);
                report.boosted += 1;
                changed = true;
            }
        }

        // GC: user_feedback edges are pinned and never collected; replay-
        // protected edges use the more lenient threshold.
        let threshold = if is_protected {
            config.replay_gc_threshold
        } else {
            config.gc_threshold
        };
        let collect = new_conf < threshold && e.discovered_by != "user_feedback";
        if collect {
            report.gc_invalidated += 1;
        }

        if dry_run || (!changed && !collect) {
            continue;
        }
        store.with_conn(|conn| {
            if collect {
                conn.execute(
                    "UPDATE causal_edges SET confidence = ?1, valid_to = ?2 WHERE id = ?3",
                    rusqlite::params![new_conf, now, e.edge_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE causal_edges SET confidence = ?1 WHERE id = ?2",
                    rusqlite::params![new_conf, e.edge_id],
                )?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Stage 1 write-back: mark replay-protected edges with
/// `last_accessed_at = now`, so "was replayed" is visible to the next sleep
/// cycle (replayed → recently accessed → higher priority → more likely to
/// survive). Returns the number of edges marked.
fn replay_writeback(
    store: &CausalStore,
    protected: &HashSet<i64>,
    dry_run: bool,
    now: i64,
) -> Result<usize> {
    if dry_run || protected.is_empty() {
        return Ok(0);
    }
    let mut marked = 0;
    for &edge_id in protected {
        // Edges merged away in stage 2a or GC'd in stage 3 are skipped by the
        // valid_to guard.
        let n = store.with_conn(|conn| {
            Ok(conn.execute(
                "UPDATE causal_edges SET last_accessed_at = ?1 WHERE id = ?2 AND valid_to IS NULL",
                rusqlite::params![now, edge_id],
            )?)
        })?;
        marked += n;
    }
    Ok(marked)
}

/// Snapshot of valid meta edges: id → discovered_at. Used to tell which meta
/// edges stage 2b created or refreshed this round.
fn snapshot_meta_edges(store: &CausalStore) -> Result<HashMap<i64, i64>> {
    store.with_conn(|conn| {
        let mut stmt =
            conn.prepare("SELECT id, discovered_at FROM meta_causal_edges WHERE valid_to IS NULL")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (id, ts): (i64, i64) = row?;
            map.insert(id, ts);
        }
        Ok(map)
    })
}

/// One valid meta edge plus the task tags its endpoint decisions live in.
struct MetaNode {
    id: i64,
    from_id: String,
    /// Text of the central decision (from endpoint), for readable patterns.
    from_text: String,
    /// discovered_at after stage 2b — compared against the pre-mine snapshot
    /// to tell which meta edges this round created or refreshed.
    discovered_at: i64,
    /// from_text + to_text, tokenized once for similarity.
    tokens: Vec<String>,
    task_tags: HashSet<String>,
}

/// Stage 4: cross-domain transfer.
///
/// Compare this round's new/refreshed meta edges against all valid meta
/// edges; when two have similar patterns (Jaccard over their endpoint
/// decision texts ≥ miner threshold) but disjoint task tags, link their
/// central decisions with a `similar_to` meta edge marked as a cross-domain
/// transfer. Only new-vs-existing pairs are compared to avoid an all-pairs
/// blowup on every run.
fn rem_integrate(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    meta_before: &HashMap<i64, i64>,
) -> Result<usize> {
    let meta = store.search_patterns(None, None, 10_000)?;
    if meta.len() < 2 {
        return Ok(0);
    }

    // Build nodes with task tags and tokens.
    let mut nodes: Vec<MetaNode> = Vec::with_capacity(meta.len());
    for m in &meta {
        let task_tags = store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT task_tag FROM causal_edges
                 WHERE (from_id = ?1 OR from_id = ?2) AND task_tag IS NOT NULL",
            )?;
            let rows = stmt.query_map(rusqlite::params![m.from_id, m.to_id], |row| {
                row.get::<_, String>(0)
            })?;
            let mut tags = HashSet::new();
            for row in rows {
                tags.insert(row?);
            }
            Ok(tags)
        })?;
        nodes.push(MetaNode {
            id: m.id,
            from_id: m.from_id.clone(),
            from_text: m.from_text.clone(),
            discovered_at: m.discovered_at,
            tokens: tokenize(&format!("{} {}", m.from_text, m.to_text)),
            task_tags,
        });
    }

    // In dry-run nothing was written this round, so treat every meta edge as
    // "new" — the comparison set is the same either way (new × all).
    let is_new = |n: &MetaNode| {
        dry_run
            || meta_before
                .get(&n.id)
                .is_none_or(|&ts| ts != n.discovered_at)
    };

    // Existing (from,to,relation) pairs, to skip pairs already linked either
    // way — avoids clobbering miner-written similar_to edges.
    let existing: HashSet<(String, String)> = meta
        .iter()
        .filter(|m| m.relation == "similar_to")
        .map(|m| (m.from_id.clone(), m.to_id.clone()))
        .collect();

    let mut transfers = 0;
    for (i, a) in nodes.iter().enumerate() {
        if !is_new(a) {
            continue;
        }
        for b in nodes.iter().skip(i + 1) {
            if a.task_tags.is_empty()
                || b.task_tags.is_empty()
                || !a.task_tags.is_disjoint(&b.task_tags)
            {
                continue;
            }
            let sim = jaccard(&a.tokens, &b.tokens);
            if sim < config.miner.similarity_threshold {
                continue;
            }
            let pair_f = (a.from_id.clone(), b.from_id.clone());
            let pair_r = (b.from_id.clone(), a.from_id.clone());
            if existing.contains(&pair_f) || existing.contains(&pair_r) {
                continue;
            }
            transfers += 1;
            if !dry_run {
                let pattern = format!(
                    "cross-domain transfer: \"{}\" ↔ \"{}\" (相似模式跨任务迁移)",
                    a.from_text, b.from_text
                );
                store.upsert_meta_edge(&a.from_id, &b.from_id, "similar_to", &pattern, sim)?;
            }
        }
    }
    Ok(transfers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;

    /// Insert an edge with full control over audit fields. Returns edge id.
    #[allow(clippy::too_many_arguments)]
    fn insert_edge(
        store: &CausalStore,
        decision: &str,
        outcome: &str,
        confidence: f64,
        discovered_by: &str,
        task_tag: Option<&str>,
        discovered_at: i64,
        last_accessed_at: Option<i64>,
    ) -> i64 {
        store
            .record_decision_at(
                decision,
                outcome,
                "caused",
                task_tag,
                confidence,
                discovered_by,
                discovered_at,
            )
            .unwrap();
        let edge = store.all_valid_edges().unwrap();
        let edge = edge
            .iter()
            .find(|e| e.decision_text == decision)
            .unwrap_or_else(|| panic!("edge for {decision} not found"));
        let id = edge.edge_id;
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE causal_edges SET discovered_at = ?1, last_accessed_at = ?2 WHERE id = ?3",
                    rusqlite::params![discovered_at, last_accessed_at, id],
                )?;
                Ok(())
            })
            .unwrap();
        id
    }

    fn edge_conf(store: &CausalStore, edge_id: i64) -> f64 {
        store.get_edge(edge_id).unwrap().unwrap().confidence
    }

    fn edge_valid(store: &CausalStore, edge_id: i64) -> bool {
        store.get_edge(edge_id).unwrap().unwrap().valid_to.is_none()
    }

    fn default_config() -> ConsolidateConfig {
        ConsolidateConfig::default()
    }

    // ── Stage 3: decay math ──────────────────────────────────────────────

    #[test]
    fn test_decay_math_ten_days() {
        let store = CausalStore::open_in_memory().unwrap();
        let id = insert_edge(
            &store,
            "use connection pool",
            "successfully fixed exhaustion",
            0.8,
            "rule",
            Some("db"),
            NOW - 10 * DAY,
            None,
        );
        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        let expected = 0.8 * 0.99_f64.powi(10);
        assert!(
            (edge_conf(&store, id) - expected).abs() < 1e-9,
            "got {}, expected {expected}",
            edge_conf(&store, id)
        );
        assert_eq!(report.decayed, 1);
        assert_eq!(report.boosted, 0);
    }

    #[test]
    fn test_same_day_edge_not_decayed() {
        let store = CausalStore::open_in_memory().unwrap();
        let id = insert_edge(
            &store,
            "add retry loop",
            "deploy success",
            0.7,
            "rule",
            Some("deploy"),
            NOW - 3600, // one hour ago
            None,
        );
        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert!((edge_conf(&store, id) - 0.7).abs() < 1e-12);
        assert_eq!(report.decayed, 0);
    }

    // ── Stage 3: access boost + cap ──────────────────────────────────────

    #[test]
    fn test_access_boost_and_cap() {
        let store = CausalStore::open_in_memory().unwrap();
        // Accessed yesterday, discovered today → +0.05, no decay.
        let boosted_id = insert_edge(
            &store,
            "cache config lookup",
            "resolved quickly",
            0.7,
            "rule",
            Some("cache"),
            NOW - 3600,
            Some(NOW - DAY),
        );
        // High confidence + boost must cap at 0.95.
        let capped_id = insert_edge(
            &store,
            "pin dependency version",
            "build success",
            0.93,
            "rule",
            Some("build"),
            NOW - 3600,
            Some(NOW - DAY),
        );
        // Accessed 30 days ago → outside the 7-day window, no boost.
        let stale_id = insert_edge(
            &store,
            "old refactor attempt",
            "no visible change",
            0.6,
            "rule",
            Some("misc"),
            NOW - 3600,
            Some(NOW - 30 * DAY),
        );
        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert!((edge_conf(&store, boosted_id) - 0.75).abs() < 1e-9);
        assert!((edge_conf(&store, capped_id) - 0.95).abs() < 1e-9);
        assert!((edge_conf(&store, stale_id) - 0.6).abs() < 1e-12);
        assert_eq!(report.boosted, 2);
        assert_eq!(report.decayed, 0);
    }

    // ── Stage 3: GC ──────────────────────────────────────────────────────

    #[test]
    fn test_gc_invalidates_low_confidence_but_pins_user_feedback() {
        let store = CausalStore::open_in_memory().unwrap();
        // 0.21 decayed 10 days → ~0.19 < 0.2 → collected.
        let gc_id = insert_edge(
            &store,
            "speculative micro-optimization",
            "no measurable effect",
            0.21,
            "llm_inferred",
            Some("perf"),
            NOW - 10 * DAY,
            None,
        );
        // Same low confidence, but user feedback is pinned forever.
        let pinned_id = insert_edge(
            &store,
            "user said keep this workaround",
            "user confirmed it helps",
            0.1,
            "user_feedback",
            Some("perf"),
            NOW - 10 * DAY,
            None,
        );
        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert!(
            !edge_valid(&store, gc_id),
            "low-confidence edge must be GC'd"
        );
        assert!(
            edge_valid(&store, pinned_id),
            "user_feedback edge is pinned"
        );
        assert_eq!(report.gc_invalidated, 1);
        // The pinned edge still decays (it just can't be collected).
        assert!((edge_conf(&store, pinned_id) - 0.1 * 0.99_f64.powi(10)).abs() < 1e-9);
    }

    // ── Stage 2a: redundant merge ────────────────────────────────────────

    #[test]
    fn test_merge_redundant_edges_keeps_highest_confidence() {
        let store = CausalStore::open_in_memory().unwrap();
        // Three edges over the same chunk pair + relation, different confidence.
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO chunks (id, text, created_at) VALUES ('dA', 'use global lock', 0)",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO chunks (id, text, created_at) VALUES ('oA', 'deadlock under load', 0)",
                    [],
                )?;
                for (conf, et) in [(0.5_f64, 100_i64), (0.9, 200), (0.7, 300)] {
                    conn.execute(
                        "INSERT INTO causal_edges
                             (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
                         VALUES ('dA', 'oA', 'caused', ?1, 'rule', ?2, ?3)",
                        rusqlite::params![conf, et, NOW],
                    )?;
                }
                Ok(())
            })
            .unwrap();

        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert_eq!(report.merged_edges, 2);

        let valid = store.all_valid_edges().unwrap();
        assert_eq!(valid.len(), 1);
        assert!((valid[0].confidence - 0.9).abs() < 1e-9);
        // Survivor confidence is unchanged by the merge itself (same-day, no decay).
        let losers: Vec<_> = (1..=3_i64)
            .map(|id| store.get_edge(id).unwrap().unwrap())
            .filter(|e| e.valid_to.is_some())
            .collect();
        assert_eq!(losers.len(), 2);
    }

    // ── Stage 4: REM cross-domain transfer ───────────────────────────────

    #[test]
    fn test_rem_cross_domain_transfer() {
        let store = CausalStore::open_in_memory().unwrap();
        // Two pattern pairs with identical shape but fully disjoint task tags:
        // (A,B) mine into meta edge M1 over tags {t1,t2};
        // (C,D) mine into meta edge M2 over tags {t3,t4}.
        insert_edge(
            &store,
            "use redis for cache",
            "deploy success",
            0.8,
            "rule",
            Some("t1"),
            NOW,
            None,
        );
        insert_edge(
            &store,
            "use redis for session",
            "rollout success",
            0.8,
            "rule",
            Some("t2"),
            NOW,
            None,
        );
        insert_edge(
            &store,
            "use redis for cache",
            "deploy success",
            0.8,
            "rule",
            Some("t3"),
            NOW,
            None,
        );
        insert_edge(
            &store,
            "use redis for session",
            "rollout success",
            0.8,
            "rule",
            Some("t4"),
            NOW,
            None,
        );

        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert!(
            report.rem_transfers >= 1,
            "expected at least one cross-domain transfer, got {report:?}"
        );
        let transfer = store
            .search_patterns(Some("cross-domain transfer"), None, 10)
            .unwrap();
        assert!(!transfer.is_empty());
        assert_eq!(transfer[0].relation, "similar_to");
    }

    // ── dry run ──────────────────────────────────────────────────────────

    #[test]
    fn test_dry_run_writes_nothing_but_counts() {
        let store = CausalStore::open_in_memory().unwrap();
        // Would decay + GC.
        let gc_id = insert_edge(
            &store,
            "weak guess",
            "unclear outcome",
            0.21,
            "llm_inferred",
            Some("x"),
            NOW - 10 * DAY,
            None,
        );
        // Would decay + boost.
        let boost_id = insert_edge(
            &store,
            "hot path cache",
            "success",
            0.7,
            "rule",
            Some("y"),
            NOW - 5 * DAY,
            Some(NOW - DAY),
        );
        // Duplicate pair that would merge.
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO chunks (id, text, created_at) VALUES ('dD', 'dup decision', 0)",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO chunks (id, text, created_at) VALUES ('oD', 'dup outcome', 0)",
                    [],
                )?;
                for conf in [0.5_f64, 0.9] {
                    conn.execute(
                        "INSERT INTO causal_edges
                             (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
                         VALUES ('dD', 'oD', 'caused', ?1, 'rule', 0, ?2)",
                        rusqlite::params![conf, NOW],
                    )?;
                }
                Ok(())
            })
            .unwrap();

        let edges_before = store.all_valid_edges().unwrap();
        let conf_before: HashMap<i64, f64> = edges_before
            .iter()
            .map(|e| (e.edge_id, e.confidence))
            .collect();
        let count_before = edges_before.len();
        let meta_before = store.search_patterns(None, None, 100).unwrap().len();

        let report = consolidate(&store, &default_config(), true, NOW).unwrap();

        assert!(report.dry_run);
        assert_eq!(report.decayed, 2, "both old edges would decay");
        assert_eq!(report.boosted, 1);
        assert_eq!(report.gc_invalidated, 1);
        assert_eq!(report.merged_edges, 1);

        // Zero change in the DB.
        let edges_after = store.all_valid_edges().unwrap();
        assert_eq!(edges_after.len(), count_before);
        for e in &edges_after {
            assert_eq!(e.confidence, conf_before[&e.edge_id]);
            assert!(e.valid_to.is_none());
        }
        assert!(edge_valid(&store, gc_id));
        assert_eq!(
            store.search_patterns(None, None, 100).unwrap().len(),
            meta_before
        );
        assert!(edge_conf(&store, boost_id) == conf_before[&boost_id]);
    }

    // ── Stage 1: reactivation ordering ───────────────────────────────────

    #[test]
    fn test_reactivation_failure_outranks_success_and_sorted() {
        let store = CausalStore::open_in_memory().unwrap();
        // High-confidence success: score 0.9.
        insert_edge(
            &store,
            "add index to users table",
            "query success fast",
            0.9,
            "rule",
            Some("db"),
            NOW,
            None,
        );
        // Lower-confidence failure: 0.5 + 0.5 = 1.0 → must rank first.
        let fail_id = insert_edge(
            &store,
            "skip migration backup",
            "data loss error",
            0.5,
            "rule",
            Some("db"),
            NOW,
            None,
        );
        // User feedback success: 0.6 + 0.3 = 0.9 (ties the first edge; edge id
        // breaks the tie, and the failure still outranks both).
        let feedback_id = insert_edge(
            &store,
            "user approved workaround",
            "works fine",
            0.6,
            "user_feedback",
            Some("db"),
            NOW,
            None,
        );

        let report = consolidate(&store, &default_config(), true, NOW).unwrap();
        let r = &report.reactivated;
        assert_eq!(r.len(), 3);
        assert!(
            r.windows(2).all(|w| w[0].score >= w[1].score),
            "sorted desc"
        );
        assert_eq!(r[0].edge_id, fail_id, "failure replayed before successes");
        assert!(r[0].reasons.iter().any(|s| s.contains("outcome failed")));
        let fb = r.iter().find(|e| e.edge_id == feedback_id).unwrap();
        assert!(fb.reasons.iter().any(|s| s.contains("user feedback")));
    }

    #[test]
    fn test_reactivation_contradiction_bonus() {
        let store = CausalStore::open_in_memory().unwrap();
        // Similar decisions, opposite outcomes → both get +0.2.
        let a = insert_edge(
            &store,
            "use global lock for cache",
            "deadlock error under load",
            0.6,
            "rule",
            Some("locking"),
            NOW,
            None,
        );
        let b = insert_edge(
            &store,
            "use global lock for queue",
            "successfully fixed contention",
            0.6,
            "rule",
            Some("queue"),
            NOW,
            None,
        );
        let report = consolidate(&store, &default_config(), true, NOW).unwrap();
        for id in [a, b] {
            let entry = report.reactivated.iter().find(|e| e.edge_id == id).unwrap();
            assert!(
                entry.reasons.iter().any(|s| s.contains("contradicted")),
                "edge {id} should carry the contradiction reason: {entry:?}"
            );
        }
    }

    // ── Stage 1→3: replay protection & write-back ────────────────────────

    #[test]
    fn test_replay_protected_edges_decay_at_half_rate() {
        let store = CausalStore::open_in_memory().unwrap();
        // Failure lesson: score 0.5 + 0.5 = 1.0 → replay-protected.
        let protected_id = insert_edge(
            &store,
            "skip migration backup",
            "data loss error",
            0.5,
            "rule",
            Some("db"),
            NOW - 10 * DAY,
            None,
        );
        // Same confidence and age, but a success: score 0.5 → not protected.
        let plain_id = insert_edge(
            &store,
            "add index to users table",
            "query success fast",
            0.5,
            "rule",
            Some("db"),
            NOW - 10 * DAY,
            None,
        );

        let report = consolidate(&store, &default_config(), false, NOW).unwrap();

        // Protected: decay over 10/2 = 5 days. Plain: full 10 days.
        let expected_protected = 0.5 * 0.99_f64.powi(5);
        let expected_plain = 0.5 * 0.99_f64.powi(10);
        assert!(
            (edge_conf(&store, protected_id) - expected_protected).abs() < 1e-9,
            "protected edge decays at half rate: got {}",
            edge_conf(&store, protected_id)
        );
        assert!(
            (edge_conf(&store, plain_id) - expected_plain).abs() < 1e-9,
            "unprotected edge decays at full rate: got {}",
            edge_conf(&store, plain_id)
        );
        assert_eq!(report.decayed, 2);
        assert_eq!(report.boosted, 0, "write-back happens after downscale");

        // Write-back: only the replayed edge is marked, with this cycle's time.
        assert_eq!(report.replayed, 1);
        let protected_edge = store.get_edge(protected_id).unwrap().unwrap();
        assert_eq!(protected_edge.last_accessed_at, Some(NOW));
        assert!(protected_edge
            .decision_text
            .contains("skip migration backup"));
        let plain_edge = store.get_edge(plain_id).unwrap().unwrap();
        assert_eq!(plain_edge.last_accessed_at, None, "not replayed → unmarked");
    }

    #[test]
    fn test_replay_protected_gc_threshold_more_lenient() {
        let store = CausalStore::open_in_memory().unwrap();
        // Protected failure edge: 0.5 * 0.99^(200/2) ≈ 0.183 — below the
        // normal GC threshold (0.2) but above the protected one (0.1).
        let protected_id = insert_edge(
            &store,
            "skip migration backup",
            "data loss error",
            0.5,
            "rule",
            Some("db"),
            NOW - 200 * DAY,
            None,
        );
        // Same age and confidence, unprotected: 0.5 * 0.99^200 ≈ 0.067 → GC'd.
        let plain_id = insert_edge(
            &store,
            "add index to users table",
            "query success fast",
            0.5,
            "rule",
            Some("db"),
            NOW - 200 * DAY,
            None,
        );

        let report = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert!(
            edge_valid(&store, protected_id),
            "replay-protected edge survives below the normal GC threshold"
        );
        assert!(
            !edge_valid(&store, plain_id),
            "unprotected edge at the same confidence is collected"
        );
        assert_eq!(report.gc_invalidated, 1);
    }

    #[test]
    fn test_replay_feedback_loop_across_cycles() {
        let store = CausalStore::open_in_memory().unwrap();
        let protected_id = insert_edge(
            &store,
            "skip migration backup",
            "data loss error",
            0.6,
            "rule",
            Some("db"),
            NOW - 2 * DAY,
            None,
        );
        let control_id = insert_edge(
            &store,
            "add index to users table",
            "query success fast",
            0.6,
            "rule",
            Some("db"),
            NOW - 2 * DAY,
            None,
        );

        // Cycle 1: protected edge decays halved (2/2 = 1 day) and is marked.
        let report1 = consolidate(&store, &default_config(), false, NOW).unwrap();
        assert!((edge_conf(&store, protected_id) - 0.6 * 0.99_f64).abs() < 1e-9);
        assert!((edge_conf(&store, control_id) - 0.6 * 0.99_f64.powi(2)).abs() < 1e-9);
        assert_eq!(report1.replayed, 1);
        assert_eq!(report1.boosted, 0);

        // Cycle 2 (one day later): the mark makes the edge "recently
        // accessed" → access boost on top of halved decay (3/2 = 1.5 days).
        let report2 = consolidate(&store, &default_config(), false, NOW + DAY).unwrap();
        let expected = (0.6 * 0.99_f64 * 0.99_f64.powf(1.5) + 0.05).min(0.95);
        assert!(
            (edge_conf(&store, protected_id) - expected).abs() < 1e-9,
            "replayed edge gets boost + half decay: got {}, expected {expected}",
            edge_conf(&store, protected_id)
        );
        // Control: full 3-day decay, no boost.
        assert!(
            (edge_conf(&store, control_id) - 0.6 * 0.99_f64.powi(2) * 0.99_f64.powi(3)).abs()
                < 1e-9
        );
        assert!(
            edge_conf(&store, protected_id) > edge_conf(&store, control_id),
            "replay → consolidate → survives better"
        );
        assert_eq!(report2.boosted, 1);
        assert_eq!(report2.replayed, 1);
        let edge = store.get_edge(protected_id).unwrap().unwrap();
        assert_eq!(edge.last_accessed_at, Some(NOW + DAY));
    }

    #[test]
    fn test_dry_run_does_not_mark_replayed() {
        let store = CausalStore::open_in_memory().unwrap();
        let id = insert_edge(
            &store,
            "skip migration backup",
            "data loss error",
            0.5,
            "rule",
            Some("db"),
            NOW - 10 * DAY,
            None,
        );
        let report = consolidate(&store, &default_config(), true, NOW).unwrap();
        // Decay is still reported (halved), but nothing is written or marked.
        assert_eq!(report.decayed, 1);
        assert_eq!(report.replayed, 0);
        let edge = store.get_edge(id).unwrap().unwrap();
        assert!((edge.confidence - 0.5).abs() < 1e-12);
        assert_eq!(edge.last_accessed_at, None);
    }
}
