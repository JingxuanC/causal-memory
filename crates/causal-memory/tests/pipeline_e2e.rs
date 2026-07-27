//! Phase 7 e2e: extraction pipeline — from a real-shaped grok session log
//! through extraction, chain linking, multi-hop trace, sleep consolidation,
//! pattern mining, and invalidation.
//!
//! Fixture narrative (tests/fixtures/session/):
//!   1. `cargo build app verbose color`   → error E0433 (module not found) [failure]
//!   2. write src/foo.rs                  → file written                   [success]
//!   3. `cargo build app verbose plain`   → error E0425 (typo `intit`)     [failure]
//!   4. search_replace src/foo.rs         → updated                        [success]
//!   5. `cargo build app verbose release` → build succeeded                [success]
//!   6. view_file                         → (not decision-worthy, skipped)
//!
//! The three build commands share 4/6 content tokens (Jaccard ≈ 0.667), so the
//! pruned pattern miner still links them; the write/search_replace pair is
//! tool-name boilerplate over identical 3-token paths and is correctly skipped.
//!
//! The two failures each sit 100s before a follow-up decision, so the
//! ChainLinker's temporal strategy bridges them (failure outcome → next
//! decision), producing multi-hop chains:
//!   build#2-error ← build#2-decision ←(bridge)← build#1-error ← build#1-decision

use std::path::PathBuf;

use causal_memory::chain_linker::ChainLinker;
use causal_memory::consolidate::{consolidate, ConsolidateConfig};
use causal_memory::extractor::DecisionExtractor;
use causal_memory::patterns::{MinerConfig, PatternMiner};
use causal_memory::store::CausalStore;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session")
}

#[test]
fn pipeline_end_to_end() {
    let store = CausalStore::open_in_memory().unwrap();

    // ── 1. Extraction ────────────────────────────────────────────────────
    let stats = DecisionExtractor::extract_from_session(&store, &fixture_dir()).unwrap();

    assert_eq!(stats.decisions_found, 6, "all tool calls are parsed");
    assert_eq!(
        stats.skipped_low_value, 1,
        "view_file is not decision-worthy"
    );
    assert_eq!(stats.results_matched, 5);
    assert_eq!(stats.edges_inserted, 5, "5 decision-worthy edges");
    assert!(
        stats.errors_captured >= 1,
        "the two failed builds are captured as high-value errors"
    );
    assert_eq!(store.count_edges().unwrap(), 5);

    // The failure outcome is correctly linked back to its build decision.
    let causes = store.trace_cause("could not compile").unwrap();
    assert_eq!(causes.len(), 2, "both failed builds traced");
    assert!(causes
        .iter()
        .all(|e| e.decision_text.contains("cargo build") && e.relation == "caused"));
    assert!(
        causes.iter().all(|e| e.confidence >= 0.7),
        "failure + content-relation → high-confidence rule edges: {causes:?}"
    );

    // Extractor parsed the real event timestamps (fixture is 2026-07-20),
    // which is what makes temporal linking below possible.
    let edges = store.all_valid_edges().unwrap();
    let first_build = edges
        .iter()
        .find(|e| e.outcome_text.contains("E0433"))
        .unwrap();
    let second_build = edges
        .iter()
        .find(|e| e.outcome_text.contains("E0425"))
        .unwrap();
    assert!(
        first_build.event_time > 0 && second_build.event_time > first_build.event_time,
        "event_time comes from events.jsonl, ordered"
    );

    // ── 2. Chain linking ─────────────────────────────────────────────────
    let link = ChainLinker::link_chains(&store).unwrap();
    assert_eq!(link.edges_scanned, 5);
    assert!(
        link.bridge_edges_created >= 2,
        "failure outcomes bridge to their follow-up decisions: {link:?}"
    );
    assert_eq!(
        store.count_edges().unwrap(),
        5 + link.bridge_edges_created as i64
    );

    // ── 3. Multi-hop trace ───────────────────────────────────────────────
    // Backward from the second build failure:
    //   edge(build#2) → bridge(build#1-error → build#2-decision) → edge(build#1)
    let chains = store
        .trace_cause_chain("could not compile", 5, 0.15)
        .unwrap();
    let max_depth = chains.iter().map(|c| c.len()).max().unwrap_or(0);
    assert!(
        max_depth >= 3,
        "expected a 3-hop chain through the bridge, got {chains:?}"
    );
    let chain = chains.iter().find(|c| c.len() >= 3).unwrap();
    assert!(chain[0].outcome_text.contains("E0425"), "anchor hop");
    assert!(chain[2].outcome_text.contains("E0433"), "root hop");

    // ── 4. Sleep consolidation (fixed now → no age decay) ────────────────
    let now = chrono::Utc::now().timestamp();
    let report = consolidate(&store, &ConsolidateConfig::default(), false, now).unwrap();
    assert_eq!(
        report.reactivated.len(),
        store.count_edges().unwrap() as usize
    );
    assert_eq!(report.decayed, 0, "same-day edges do not decay");
    assert_eq!(report.merged_edges, 0, "no duplicate chunk pairs");
    assert_eq!(
        report.gc_invalidated, 0,
        "all confidences above GC threshold"
    );
    assert!(
        report.boosted >= 1,
        "edges hit by the trace above get the access boost"
    );
    // Nothing was invalidated: library state is intact.
    assert_eq!(
        store.all_valid_edges().unwrap().len() as i64,
        store.count_edges().unwrap()
    );

    // ── 5. Pattern mining ────────────────────────────────────────────────
    // The fixture is designed to mine at least:
    //   similar_to(build#1, build#2)  — 4/6 content-token overlap, both failed
    //   refines(build#N → build#3)    — same task, failure → later success
    let mine = PatternMiner::new(&store, MinerConfig::default())
        .mine()
        .unwrap();
    let total = mine.similar_to + mine.repeated + mine.contradicts + mine.refines;
    assert!(total >= 2, "fixture should yield meta edges: {mine:?}");
    assert!(
        mine.refines >= 1,
        "failure → later success refines: {mine:?}"
    );
    let patterns = store.search_patterns(None, None, 100).unwrap();
    assert!(patterns.len() >= total);
    assert!(patterns.iter().all(|p| matches!(
        p.relation.as_str(),
        "similar_to" | "repeated" | "contradicts" | "refines"
    )));

    // ── 6. Invalidation changes trace results ────────────────────────────
    // Invalidate the bridge (build#1-error → build#2-decision): the 3-hop
    // chain from step 3 must disappear.
    let second_build_decision = second_build.decision_id.clone();
    let bridge = store
        .all_valid_edges()
        .unwrap()
        .into_iter()
        .find(|e| e.outcome_id == second_build_decision && e.discovered_by == "temporal")
        .expect("bridge edge from step 2 must exist");
    assert!(store.invalidate_edge(bridge.edge_id).unwrap());
    assert!(
        store
            .get_edge(bridge.edge_id)
            .unwrap()
            .unwrap()
            .valid_to
            .is_some(),
        "soft-invalidation keeps the row for audit"
    );
    let chains_after = store
        .trace_cause_chain("could not compile", 5, 0.15)
        .unwrap();
    assert!(
        chains_after.is_empty(),
        "without the bridge no multi-hop chain remains: {chains_after:?}"
    );
}
