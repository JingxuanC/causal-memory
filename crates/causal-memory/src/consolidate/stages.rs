//! Consolidation stage implementations.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::patterns::{
    boilerplate_tokens, content_tokens, jaccard, tokenize,
};
use crate::store::{outcome_polarity, outcomes_contradict, CausalStore};

use super::types::{ConsolidateConfig, ConsolidateReport, MetaNode, ReactivationEntry, SECS_PER_DAY};

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
pub fn score_reactivation(
    store: &CausalStore,
    config: &ConsolidateConfig,
    now: i64,
) -> Result<Vec<ReactivationEntry>> {
    let edges = store.all_valid_edges()?;
    let tokens: Vec<Vec<String>> = edges.iter().map(|e| tokenize(&e.decision_text)).collect();
    let window_secs = i64::from(config.access_boost_window_days) * SECS_PER_DAY as i64;

    // Flag edges that participate in a contradiction pair.
    //
    // Token blocking, same discipline as PatternMiner: Jaccard ≥
    // similarity_threshold requires at least one shared token, so the
    // all-pairs O(E²) scan (3.1e10 Jaccard calls on the 248k-edge
    // LongMemEval store — sleep never finished, single core pegged for
    // 45+ minutes) is replaced by inverted-index candidate generation.
    // Tokens above the df cap are too frequent to be selective; pairs
    // sharing only such tokens cannot reach the threshold either.
    // Cap is n/1000 (floor 100), not n/100: the LongMemEval token df
    // distribution is heavy-tailed, and a 1%-of-N cap still yields 2.9e9
    // candidate pairs (measured via the bm25 index); n/1000 yields 8.6e7.
    let mut contradicted = vec![false; edges.len()];
    let df_cap = (edges.len() / 1000).max(100);
    let mut postings: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, toks) in tokens.iter().enumerate() {
        for t in toks {
            postings.entry(t.as_str()).or_default().push(idx);
        }
    }
    let mut cand: Vec<usize> = Vec::new();
    for i in 0..edges.len() {
        cand.clear();
        for t in &tokens[i] {
            let list = &postings[t.as_str()];
            if list.len() > df_cap {
                continue;
            }
            cand.extend_from_slice(list);
        }
        cand.sort_unstable();
        cand.dedup();
        for &j in &cand {
            if j <= i || (contradicted[i] && contradicted[j]) {
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
pub fn merge_redundant_edges(store: &CausalStore, dry_run: bool, now: i64) -> Result<usize> {
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
pub fn downscale(
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

    // Pass 1: compute post-decay confidence and GC candidacy per edge.
    struct Pending {
        edge_id: i64,
        new_conf: f64,
        changed: bool,
        collect: bool,
    }
    let mut pendings: Vec<Pending> = Vec::with_capacity(edges.len());
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
            // Vela-style half-life tiers by discovery source; None = legacy
            // flat decay_per_day (behaviour-compatible for unmapped sources).
            new_conf *= match config.half_life_hours(&e.discovered_by) {
                Some(halflife) => 0.5f64.powf(effective_days * 24.0 / halflife),
                None => config.decay_per_day.powf(effective_days),
            };
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
        pendings.push(Pending {
            edge_id: e.edge_id,
            new_conf,
            changed,
            collect,
        });
    }

    // GC budget (bounded forgetting): invalidate the weakest candidates
    // first, at most max(gc_floor, max_gc_fraction × population) per cycle.
    // Without the cap, a burst-ingested corpus with skewed timestamps
    // decays uniformly and one cycle wipes most of the store (LongMemEval:
    // 90% GC'd, evidence-hit 94%→50%). Exempt candidates keep their decayed
    // confidence and face the next cycle. Small stores (< gc_floor
    // candidates) are unaffected — exact pre-guard behaviour.
    let mut gc_candidates: Vec<&Pending> = pendings.iter().filter(|p| p.collect).collect();
    gc_candidates.sort_by(|a, b| {
        a.new_conf
            .partial_cmp(&b.new_conf)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.edge_id.cmp(&b.edge_id))
    });
    let budget = config
        .gc_floor
        .max((pendings.len() as f64 * config.max_gc_fraction) as usize);
    let invalidate: HashSet<i64> = gc_candidates
        .iter()
        .take(budget)
        .map(|p| p.edge_id)
        .collect();
    report.gc_invalidated = invalidate.len();
    report.gc_deferred = gc_candidates.len() - invalidate.len();

    // Pass 2: write.
    for p in &pendings {
        let collect = invalidate.contains(&p.edge_id);
        if dry_run || (!p.changed && !collect) {
            continue;
        }
        store.with_conn(|conn| {
            if collect {
                conn.execute(
                    "UPDATE causal_edges SET confidence = ?1, valid_to = ?2 WHERE id = ?3",
                    rusqlite::params![p.new_conf, now, p.edge_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE causal_edges SET confidence = ?1 WHERE id = ?2",
                    rusqlite::params![p.new_conf, p.edge_id],
                )?;
            }
            Ok(())
        })?;
    }
    downscale_facts(store, config, dry_run, now, report)
}

/// Phase D (one-graph-convergence): stage 3 fact downscaling — the same
/// half-life decay and GC the causal edges get, applied to `agent_facts`.
/// Age runs from `updated_at`; the tier is `half_life_fact_hours`
/// (default 90d): facts are high-trust "what is" knowledge, so they fade
/// far slower than temporal lessons. Facts below the GC threshold retire
/// (`valid_to`); supersession lineage (`superseded_by`) is untouched.
/// Same-day facts are not decayed, mirroring the edge path.
pub fn downscale_facts(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    now: i64,
    report: &mut ConsolidateReport,
) -> Result<()> {
    let facts: Vec<(i64, f64, i64)> = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, confidence, updated_at FROM agent_facts WHERE valid_to IS NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, i64>(2)?))
        })?;
        let v: std::result::Result<Vec<_>, rusqlite::Error> = rows.collect();
        Ok(v?)
    })?;

    // Same bounded-forgetting budget as the edge pass: at most
    // max(gc_floor, max_gc_fraction × population) invalidations per cycle,
    // weakest first; spared facts keep their decayed confidence.
    let population = facts.len();
    let mut gc_candidates: Vec<(i64, f64)> = Vec::new();
    for (id, confidence, updated_at) in facts {
        let days = (now - updated_at) as f64 / SECS_PER_DAY;
        if days < 1.0 {
            continue;
        }
        let halflife = f64::from(config.half_life_fact_hours);
        let new_conf = confidence * 0.5f64.powf(days * 24.0 / halflife);
        report.facts_decayed += 1;
        let collect = new_conf < config.gc_threshold;
        if collect {
            gc_candidates.push((id, new_conf));
            continue;
        }
        if dry_run {
            continue;
        }
        store.with_conn(|conn| {
            conn.execute(
                "UPDATE agent_facts SET confidence = ?1 WHERE id = ?2",
                rusqlite::params![new_conf, id],
            )?;
            Ok(())
        })?;
    }
    gc_candidates.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let budget = config
        .gc_floor
        .max((population as f64 * config.max_gc_fraction) as usize);
    report.facts_gc = gc_candidates.len().min(budget);
    report.gc_deferred += gc_candidates.len().saturating_sub(budget);
    if dry_run {
        return Ok(());
    }
    for (id, new_conf) in gc_candidates.iter().take(budget) {
        store.with_conn(|conn| {
            conn.execute(
                "UPDATE agent_facts SET confidence = ?1, valid_to = ?2 WHERE id = ?3",
                rusqlite::params![new_conf, now, id],
            )?;
            Ok(())
        })?;
    }
    // Deferred candidates still get their decayed confidence written.
    for (id, new_conf) in gc_candidates.iter().skip(budget) {
        store.with_conn(|conn| {
            conn.execute(
                "UPDATE agent_facts SET confidence = ?1 WHERE id = ?2",
                rusqlite::params![new_conf, id],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Stage 1 write-back: mark replay-protected edges with
/// `last_accessed_at = now`, so "was replayed" is visible to the next sleep
/// cycle (replayed → recently accessed → higher priority → more likely to
/// survive). Returns the number of edges marked.
pub fn replay_writeback(
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
pub fn snapshot_meta_edges(store: &CausalStore) -> Result<HashMap<i64, i64>> {
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

/// Stage 4: cross-domain transfer.
///
/// Compare this round's new/refreshed meta edges against all valid meta
/// edges; when two have similar patterns (Jaccard over their endpoint
/// decision texts ≥ miner threshold) but **disjoint, non-empty task tags** —
/// cross-domain transfer is only written when the two sides verifiably live in
/// different tasks — link their central decisions with a `similar_to` meta
/// edge marked as a cross-domain transfer. Only new-vs-existing pairs are
/// compared to avoid an all-pairs blowup on every run. Accepted transfers are
/// capped like the miner: top-N per central decision and `max_pairs` overall
/// (highest similarity first).
pub fn rem_integrate(
    store: &CausalStore,
    config: &ConsolidateConfig,
    dry_run: bool,
    meta_before: &HashMap<i64, i64>,
) -> Result<usize> {
    let meta = store.search_patterns(None, None, 10_000)?;
    if meta.len() < 2 {
        return Ok(0);
    }

    // Build nodes with task tags and tokens (tool-name boilerplate stripped,
    // same as the miner, so short tool-call patterns don't inflate Jaccard).
    let boilerplate = boilerplate_tokens();
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
            tokens: content_tokens(&format!("{} {}", m.from_text, m.to_text), &boilerplate),
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

    // Collect candidate transfers, then accept greedily under the same caps as
    // the miner (top-N per decision, max_pairs overall, similarity first).
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
    for (i, a) in nodes.iter().enumerate() {
        if !is_new(a) {
            continue;
        }
        for (j, b) in nodes.iter().enumerate().skip(i + 1) {
            // Never link a decision to itself (two meta edges can share a
            // central decision), and require both sides to carry task tags
            // that are provably different (non-empty and disjoint).
            if a.from_id == b.from_id
                || a.task_tags.is_empty()
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
            candidates.push((i, j, sim));
        }
    }

    candidates.sort_by(|&(ai, bi, asim), &(ci, di, bsim)| {
        bsim.partial_cmp(&asim)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(ai.cmp(&ci))
            .then(bi.cmp(&di))
    });
    let mut per_decision: HashMap<&str, usize> = HashMap::new();
    let mut transfers = 0;
    for (ai, bi, sim) in candidates {
        let a = &nodes[ai];
        let b = &nodes[bi];
        if transfers >= config.miner.max_pairs
            || per_decision.get(a.from_id.as_str()).copied().unwrap_or(0)
                >= config.miner.max_pairs_per_decision
            || per_decision.get(b.from_id.as_str()).copied().unwrap_or(0)
                >= config.miner.max_pairs_per_decision
        {
            continue;
        }
        transfers += 1;
        *per_decision.entry(a.from_id.as_str()).or_default() += 1;
        *per_decision.entry(b.from_id.as_str()).or_default() += 1;
        if !dry_run {
            let pattern = format!(
                "cross-domain transfer: \"{}\" ↔ \"{}\" (相似模式跨任务迁移)",
                a.from_text, b.from_text
            );
            store.upsert_meta_edge(&a.from_id, &b.from_id, "similar_to", &pattern, sim)?;
        }
    }
    Ok(transfers)
}
