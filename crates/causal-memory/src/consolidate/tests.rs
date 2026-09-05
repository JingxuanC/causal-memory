use crate::consolidate::{
    consolidate, recent_diversity, resolve_supersessions_with, ConsolidateConfig,
    ConsolidateReport, SupersessionAction,
};
use crate::store::CausalStore;
use std::collections::HashMap;
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

#[test]
fn test_half_life_temporal_tier() {
    // temporal edges decay by half-life 168h (7d): 10 days -> 0.5^(240/168)
    let store = CausalStore::open_in_memory().unwrap();
    let id = insert_edge(
        &store,
        "noticed flaky test",
        "suspect timing",
        0.8,
        "temporal",
        Some("db"),
        NOW - 10 * DAY,
        None,
    );
    let report = consolidate(&store, &default_config(), false, NOW).unwrap();
    let expected = 0.8 * 0.5_f64.powf(10.0 * 24.0 / 168.0);
    assert!(
        (edge_conf(&store, id) - expected).abs() < 1e-9,
        "temporal half-life: got {}, expected {expected}",
        edge_conf(&store, id)
    );
    assert_eq!(report.decayed, 1);
}

#[test]
fn test_half_life_user_feedback_tier() {
    // user_feedback half-life 2160h (90d): 10 days -> 0.5^(240/2160).
    // conf 0.5 keeps the edge below the replay-protect score (0.5+0.3=0.8)
    // so the tier math is tested directly, not halved by protection.
    let store = CausalStore::open_in_memory().unwrap();
    let id = insert_edge(
        &store,
        "pinned by user",
        "confirmed correct",
        0.5,
        "user_feedback",
        Some("db"),
        NOW - 10 * DAY,
        None,
    );
    consolidate(&store, &default_config(), false, NOW).unwrap();
    let expected = 0.5 * 0.5_f64.powf(10.0 * 24.0 / 2160.0);
    assert!(
        (edge_conf(&store, id) - expected).abs() < 1e-9,
        "user_feedback half-life: got {}, expected {expected}",
        edge_conf(&store, id)
    );
}

#[test]
fn test_half_life_legacy_unmapped_source() {
    // Unmapped source (distill) keeps the legacy flat 0.99/day decay.
    let store = CausalStore::open_in_memory().unwrap();
    let id = insert_edge(
        &store,
        "distilled lesson",
        "noted",
        0.7,
        "distill",
        Some("db"),
        NOW - 10 * DAY,
        None,
    );
    consolidate(&store, &default_config(), false, NOW).unwrap();
    let expected = 0.7 * 0.99_f64.powi(10);
    assert!(
        (edge_conf(&store, id) - expected).abs() < 1e-9,
        "legacy decay: got {}, expected {expected}",
        edge_conf(&store, id)
    );
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
    // The pinned edge still decays (it just can't be collected); user_feedback
    // now follows the 2160h half-life tier (0.1 is below protect score).
    let expected = 0.1 * 0.5_f64.powf(10.0 * 24.0 / 2160.0);
    assert!((edge_conf(&store, pinned_id) - expected).abs() < 1e-9);
}

#[test]
fn test_gc_bounded_forgetting_budget() {
    let store = CausalStore::open_in_memory().unwrap();
    // 100 edges that ALL fall below the GC threshold after decay
    // (0.21 × 0.5^(240/2160) ≈ 0.194 < 0.2). An unbounded cycle would wipe
    // every one (the LongMemEval mass-extinction repro: burst ingest +
    // uniform age → uniform decay). The budget caps this cycle at
    // max(gc_floor=50, 0.2×100) = 50 invalidations, weakest first.
    let mut ids = Vec::new();
    for i in 0..100 {
        ids.push(insert_edge(
            &store,
            &format!("speculative tweak number {i} attempted here"),
            "no measurable effect",
            0.21,
            "llm_inferred",
            Some("perf"),
            NOW - 10 * DAY,
            None,
        ));
    }
    // Weakest candidate (0.05 → ~0.046): must be invalidated first.
    let weakest = insert_edge(
        &store,
        "wild guess refactor of the whole module",
        "nothing improved at all",
        0.05,
        "llm_inferred",
        Some("perf"),
        NOW - 10 * DAY,
        None,
    );
    // Strongest candidate (0.215 → ~0.199, still < 0.2): spared this cycle.
    let strongest = insert_edge(
        &store,
        "borderline optimization of the retry path",
        "marginal effect only",
        0.215,
        "llm_inferred",
        Some("perf"),
        NOW - 10 * DAY,
        None,
    );
    let report = consolidate(&store, &default_config(), false, NOW).unwrap();
    // 102 candidates total; budget = max(50, 0.2×102) = 50.
    assert_eq!(report.gc_invalidated, 50);
    assert_eq!(report.gc_deferred, 52);
    assert!(!edge_valid(&store, weakest), "weakest candidate GC'd first");
    assert!(
        edge_valid(&store, strongest),
        "strongest below-threshold edge survives under the budget"
    );
    // The weakest edge is one of the 50 invalidated, so 51 of the 100
    // identical-confidence edges survive alongside it... minus its seat:
    // 50 invalidated = weakest + 49 from `ids` → 51 of `ids` survive.
    let survivors = ids.iter().filter(|&&id| edge_valid(&store, id)).count();
    assert_eq!(survivors, 51, "invalidation stopped at the budget");
}

// ── Stage 3: triple-criterion GC (HeLa-Mem adaptive forgetting) ──────

#[test]
fn test_gc_recently_accessed_weak_edge_survives() {
    let store = CausalStore::open_in_memory().unwrap();
    // Both edges: 10 days old, 0.21 → ~0.194 < 0.2 — weak AND dormant.
    // The one read an hour ago is spared by the access-freshness criterion;
    // only the never-accessed twin is collected.
    let accessed_id = insert_edge(
        &store,
        "weak but still consulted lesson",
        "marginal effect",
        0.21,
        "llm_inferred",
        Some("perf"),
        NOW - 10 * DAY,
        Some(NOW - 3600),
    );
    let stale_id = insert_edge(
        &store,
        "weak and never consulted lesson",
        "marginal effect",
        0.21,
        "llm_inferred",
        Some("perf"),
        NOW - 10 * DAY,
        None,
    );
    let report = consolidate(&store, &default_config(), false, NOW).unwrap();
    assert!(
        edge_valid(&store, accessed_id),
        "weak but recently accessed edge survives (old-but-active)"
    );
    assert!(
        !edge_valid(&store, stale_id),
        "weak + dormant + untouched edge is collected"
    );
    assert_eq!(report.gc_invalidated, 1);
}

#[test]
fn test_gc_dormancy_grace_for_young_edges() {
    let store = CausalStore::open_in_memory().unwrap();
    // Recorded at 0.19 (already below gc_threshold) only 2 days ago: weak
    // but NOT dormant (< gc_min_age_hours = 168h) → grace, survives cycle 1.
    let id = insert_edge(
        &store,
        "fresh speculative idea, low initial confidence",
        "unverified",
        0.19,
        "llm_inferred",
        Some("perf"),
        NOW - 2 * DAY,
        None,
    );
    consolidate(&store, &default_config(), false, NOW).unwrap();
    assert!(
        edge_valid(&store, id),
        "young weak edge gets the dormancy grace period"
    );
    // Six days later the edge is 8 days old → dormant; still weak and never
    // accessed → collected.
    consolidate(&store, &default_config(), false, NOW + 6 * DAY).unwrap();
    assert!(
        !edge_valid(&store, id),
        "once dormant, the weak untouched edge is collected"
    );
}

#[test]
fn test_fact_gc_dormancy_grace() {
    let store = CausalStore::open_in_memory().unwrap();
    // Weak (0.1 < 0.2) but only 2 days old → not dormant → spared;
    // agent_facts has no access tracking, so the criterion is weak+dormant.
    let young = store
        .record_fact("editor", "zed", "user", "agent", 0.1)
        .unwrap();
    let old = store
        .record_fact("pager", "less", "user", "agent", 0.1)
        .unwrap();
    backdate_fact(&store, young, NOW - 2 * DAY);
    backdate_fact(&store, old, NOW - 10 * DAY);
    let report = consolidate(&store, &default_config(), false, NOW).unwrap();
    let (_, young_valid) = fact_state(&store, young);
    let (_, old_valid) = fact_state(&store, old);
    assert!(young_valid, "young weak fact gets the dormancy grace");
    assert!(!old_valid, "old weak fact is collected");
    assert_eq!(report.facts_gc, 1);
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
    // Two pattern pairs with similar shape but fully disjoint task tags:
    // (A,B) mine into meta edge M1 over tags {t1,t2};
    // (C,D) mine into meta edge M2 over tags {t3,t4}.
    // Texts are built for the default miner bar (≥4 content tokens,
    // Jaccard ≥ 0.65): within a pair the overlap is 4/6 ≈ 0.667; the two
    // meta edges' combined texts overlap 5/7 ≈ 0.714.
    insert_edge(
        &store,
        "use redis cache layer alpha",
        "deploy success",
        0.8,
        "rule",
        Some("t1"),
        NOW,
        None,
    );
    insert_edge(
        &store,
        "use redis cache layer beta",
        "rollout success",
        0.8,
        "rule",
        Some("t2"),
        NOW,
        None,
    );
    insert_edge(
        &store,
        "use redis cache pool alpha",
        "deploy success",
        0.8,
        "rule",
        Some("t3"),
        NOW,
        None,
    );
    insert_edge(
        &store,
        "use redis cache pool beta",
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

#[test]
fn test_rem_same_task_tag_no_transfer() {
    let store = CausalStore::open_in_memory().unwrap();
    // Same shape as the cross-domain test, but the two pattern pairs share
    // task tag t2 → tags are not disjoint → no transfer may be written.
    insert_edge(
        &store,
        "use redis cache layer alpha",
        "deploy success",
        0.8,
        "rule",
        Some("t1"),
        NOW,
        None,
    );
    insert_edge(
        &store,
        "use redis cache layer beta",
        "rollout success",
        0.8,
        "rule",
        Some("t2"),
        NOW,
        None,
    );
    insert_edge(
        &store,
        "use redis cache pool alpha",
        "deploy success",
        0.8,
        "rule",
        Some("t2"),
        NOW,
        None,
    );
    insert_edge(
        &store,
        "use redis cache pool beta",
        "rollout success",
        0.8,
        "rule",
        Some("t3"),
        NOW,
        None,
    );

    let report = consolidate(&store, &default_config(), false, NOW).unwrap();
    assert_eq!(
        report.rem_transfers, 0,
        "overlapping task tags must block transfer: {report:?}"
    );
    let transfer = store
        .search_patterns(Some("cross-domain transfer"), None, 10)
        .unwrap();
    assert!(transfer.is_empty());
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
        "use global lock for cache data",
        "deadlock error under load",
        0.6,
        "rule",
        Some("locking"),
        NOW,
        None,
    );
    let b = insert_edge(
        &store,
        "use global lock for queue data",
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
        (edge_conf(&store, control_id) - 0.6 * 0.99_f64.powi(2) * 0.99_f64.powi(3)).abs() < 1e-9
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

#[test]
fn test_diversity_gate_skips_uniform_experience() {
    let store = CausalStore::open_in_memory().unwrap();
    // Near-uniform recent text (many tokens shared) → low diversity → gate
    // skips the cycle. (Pure repetition now dedupes to one chunk via v9 text
    // reuse, so the skew must come from similar-but-distinct texts.)
    for i in 0..20 {
        store
            .record_decision(
                &format!("routine maintenance task number {i}"),
                "completed as expected",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
    }
    let report = consolidate(
        &store,
        &ConsolidateConfig {
            min_diversity: 0.9,
            ..ConsolidateConfig::default()
        },
        false,
        1000,
    )
    .unwrap();
    assert!(report.skipped_low_diversity);
    assert_eq!(report.q_updates, 0, "skipped cycle must not reinforce Q");
    assert!(report.diversity < 0.9);
}

#[test]
fn test_diversity_high_when_varied() {
    let store = CausalStore::open_in_memory().unwrap();
    for i in 0..20 {
        store
            .record_decision(
                &format!("distinct decision number {i}"),
                &format!("distinct outcome number {i}"),
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
    }
    let d = recent_diversity(&store, 64).unwrap();
    assert!(
        d > 0.5,
        "varied recent text must score high diversity (got {d:.2})"
    );
}

// ── Stage 1.7: C7 supersession resolution ─────────────────────────────

/// Re-record the same decision text with a *different* outcome: the newer
/// evidence is a falsification candidate for the original edge.
fn insert_falsified_pair(store: &CausalStore) {
    store
        .record_decision(
            "deploy hotfix directly to prod",
            "caused an outage",
            "caused",
            None,
            0.8,
            "rule",
        )
        .unwrap();
    store
        .record_decision(
            "deploy hotfix directly to prod",
            "was safe, no incident",
            "caused",
            None,
            0.8,
            "rule",
        )
        .unwrap();
}

/// The judge is injected directly (no process-env mutation): a `None`
/// config must make the stage a silent no-op that invalidates nothing.
#[test]
fn test_supersession_skips_without_llm() {
    let store = CausalStore::open_in_memory().unwrap();
    insert_falsified_pair(&store);
    let mut report = ConsolidateReport::default();
    resolve_supersessions_with(
        &store,
        &default_config(),
        false,
        &mut report,
        None,
        SupersessionAction::Retire,
    )
    .unwrap();
    assert_eq!(
        report.superseded_lessons, 0,
        "no LLM configured → stage must be a silent no-op"
    );
    let valid = store.all_valid_edges().unwrap();
    assert_eq!(valid.len(), 2, "no-LLM cycle must not invalidate anything");
}

/// Unreachable endpoint: the judge call fails fast (connection refused)
/// and the conservative fallback must keep the edge. Dry-run counts 0 and
/// writes nothing either way.
#[test]
fn test_supersession_judge_failure_keeps_edge() {
    let bad = crate::llm::LlmConfig {
        api_base: "http://127.0.0.1:1/v1".to_string(),
        api_key: "test-key".to_string(),
        model: "test-model".to_string(),
    };
    let store = CausalStore::open_in_memory().unwrap();
    insert_falsified_pair(&store);
    let mut report = ConsolidateReport::default();
    resolve_supersessions_with(
        &store,
        &default_config(),
        false,
        &mut report,
        Some(&bad),
        SupersessionAction::Retire,
    )
    .unwrap();
    assert_eq!(
        report.superseded_lessons, 0,
        "judge failure must be conservative (keep the edge)"
    );
    let valid = store.all_valid_edges().unwrap();
    assert_eq!(valid.len(), 2, "failing judge must not invalidate the edge");
}
/// LIVE verification (not run by default): seeds a falsified pair and runs
/// the stage with a REAL LLM judge from the environment, asserting the old
/// edge is retired on the write path (2 recorded -> 1 valid).
/// Run explicitly:
///   CAUSAL_MEMORY_LLM_API=... CAUSAL_MEMORY_LLM_KEY=... \
///     cargo test -p causal-memory --lib -- --ignored test_supersession_live_apply
#[test]
#[ignore = "requires CAUSAL_MEMORY_LLM_API/KEY (real LLM calls)"]
fn test_supersession_live_apply() {
    let Some(llm) = crate::llm::LlmConfig::from_env() else {
        eprintln!("skipped: no CAUSAL_MEMORY_LLM_API/KEY configured");
        return;
    };
    let store = CausalStore::open_in_memory().unwrap();
    insert_falsified_pair(&store);
    assert_eq!(store.all_valid_edges().unwrap().len(), 2, "precondition");
    let mut report = ConsolidateReport::default();
    resolve_supersessions_with(
        &store,
        &default_config(),
        false,
        &mut report,
        Some(&llm),
        SupersessionAction::Retire,
    )
    .unwrap();
    let valid = store.all_valid_edges().unwrap();
    assert_eq!(
        valid.len(),
        1,
        "live judge must retire the falsified old edge (got {:?})",
        valid
            .iter()
            .map(|e| e.outcome_text.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(report.superseded_lessons, 1);
}

/// LIVE verification (not run by default): the Annotate action must keep
/// BOTH edges valid and set `superseded_by` on the old one, pointing at
/// the correcting edge — soft supersession, nothing hidden.
/// Run explicitly:
///   CAUSAL_MEMORY_LLM_API=... CAUSAL_MEMORY_LLM_KEY=... \
///     cargo test -p causal-memory --lib -- --ignored test_supersession_annotate_live
#[test]
#[ignore = "requires CAUSAL_MEMORY_LLM_API/KEY (real LLM calls)"]
fn test_supersession_annotate_live() {
    let Some(llm) = crate::llm::LlmConfig::from_env() else {
        eprintln!("skipped: no CAUSAL_MEMORY_LLM_API/KEY configured");
        return;
    };
    let store = CausalStore::open_in_memory().unwrap();
    insert_falsified_pair(&store);
    let mut report = ConsolidateReport::default();
    resolve_supersessions_with(
        &store,
        &default_config(),
        false,
        &mut report,
        Some(&llm),
        SupersessionAction::Annotate,
    )
    .unwrap();
    let valid = store.all_valid_edges().unwrap();
    assert_eq!(
        valid.len(),
        2,
        "annotate must keep both edges retrievable (got {:?})",
        valid
            .iter()
            .map(|e| e.outcome_text.as_str())
            .collect::<Vec<_>>()
    );
    let annotated = valid.iter().find(|e| e.outcome_text == "caused an outage");
    let corrected_by = valid
        .iter()
        .find(|e| e.outcome_text == "was safe, no incident");
    match (annotated, corrected_by) {
        (Some(old), Some(new)) => {
            assert_eq!(
                old.superseded_by,
                Some(new.edge_id),
                "soft mark must point at the correcting edge"
            );
            assert_eq!(
                new.superseded_by, None,
                "the correction itself is not superseded"
            );
        }
        _ => panic!("both edges must survive annotation"),
    }
    assert_eq!(report.superseded_lessons, 1);
}

// ── Phase D: fact downscaling + supersession lineage ─────────────────

/// Backdate a fact's updated_at so stage 3 sees it as `days` old.
#[allow(
    clippy::unwrap_used,
    reason = "test invariant: panicking on failure is the desired behavior"
)]
fn backdate_fact(store: &CausalStore, fact_id: i64, updated_at: i64) {
    store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE agent_facts SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![updated_at, fact_id],
            )?;
            Ok(())
        })
        .unwrap();
}

#[allow(
    clippy::unwrap_used,
    reason = "test invariant: panicking on failure is the desired behavior"
)]
fn fact_state(store: &CausalStore, fact_id: i64) -> (f64, bool) {
    store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT confidence, valid_to IS NULL FROM agent_facts WHERE id = ?1",
                rusqlite::params![fact_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap()
}

#[test]
#[allow(
    clippy::unwrap_used,
    reason = "test invariant: panicking on failure is the desired behavior"
)]
fn test_fact_half_life_decay_and_gc() {
    let store = CausalStore::open_in_memory().unwrap();
    // One old fact (400 days → well past the 90d half-life, below the GC
    // threshold), one fresh fact (same-day → untouched).
    let old = store
        .record_fact("ci_tool", "jenkins", "user", "agent", 0.8)
        .unwrap();
    let fresh = store
        .record_fact("os", "macos", "user", "agent", 0.9)
        .unwrap();
    backdate_fact(&store, old, NOW - 400 * DAY);

    // Dry run: counted, not written.
    let report = consolidate(&store, &default_config(), true, NOW).unwrap();
    assert!(report.facts_decayed >= 1, "{:?}", report.facts_decayed);
    assert_eq!(report.facts_gc, 1, "400d-old fact must be collected");
    let (conf, valid) = fact_state(&store, old);
    assert!(valid && (conf - 0.8).abs() < 1e-9, "dry run must not write");

    // Real run: the old fact retires with decayed confidence, the fresh
    // one keeps its confidence and stays valid.
    let report = consolidate(&store, &default_config(), false, NOW).unwrap();
    assert_eq!(report.facts_gc, 1);
    let (conf, valid) = fact_state(&store, old);
    assert!(!valid, "old fact must retire");
    assert!(
        conf < 0.2,
        "retired confidence must be below the threshold: {conf}"
    );
    let (conf, valid) = fact_state(&store, fresh);
    assert!(valid, "fresh fact must stay valid");
    assert!(
        (conf - 0.9).abs() < 1e-9,
        "fresh fact must not decay: {conf}"
    );
}

#[test]
#[allow(
    clippy::unwrap_used,
    reason = "test invariant: panicking on failure is the desired behavior"
)]
fn test_fact_replace_records_supersession_lineage() {
    let store = CausalStore::open_in_memory().unwrap();
    let old = store
        .record_fact("pm", "npm", "user", "agent", 0.8)
        .unwrap();
    let (new, retired) = store
        .record_fact_replacing("pm", "pnpm", "user", "agent", 0.9)
        .unwrap();
    assert_eq!(retired, 1);

    // The old row carries the lineage: retired AND pointing at its replacement.
    let (valid, superseded_by): (bool, Option<i64>) = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT valid_to IS NULL, superseded_by FROM agent_facts WHERE id = ?1",
                rusqlite::params![old],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert!(!valid, "old value must retire");
    assert_eq!(
        superseded_by,
        Some(new),
        "lineage must point at the new fact"
    );

    // Revive (re-record the old value) clears the lineage.
    store
        .record_fact("pm", "npm", "user", "agent", 0.8)
        .unwrap();
    let (valid, superseded_by): (bool, Option<i64>) = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT valid_to IS NULL, superseded_by FROM agent_facts WHERE id = ?1",
                rusqlite::params![old],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert!(valid, "re-recorded fact revives");
    assert_eq!(superseded_by, None, "revive clears the lineage");
}
