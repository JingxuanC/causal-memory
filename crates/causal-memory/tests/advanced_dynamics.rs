//! Advanced dynamics benchmark — end-to-end validation of three remaining
//! system layers: meta-edge pattern mining, Hebbian co-occurrence dynamics,
//! and forward-simulation intervention queries.
//!
//! Together with the capability and longitudinal benchmarks, this completes
//! end-to-end coverage of all 16 designed system layers.

use causal_memory::hippocampus::{CausalGraph, EdgeData, NodeData, Relation};
use causal_memory::patterns::{MinerConfig, PatternMiner};
use causal_memory::store::CausalStore;

fn mk(id: &str, text: &str) -> NodeData {
    NodeData {
        id: id.into(),
        text: text.into(),
        event_time: 0,
        q_value: 0.5,
        replay_count: 0,
        last_activated: 0,
        task_tag: None,
        scope: None,
    }
}

fn edge(from: &str, to: &str, rel: Relation, weight: f32) -> EdgeData {
    EdgeData {
        from_id: from.into(),
        to_id: to.into(),
        relation: rel,
        weight,
        valid: true,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. META EDGES — Cross-Task Pattern Discovery
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn meta_pattern_miner_detects_repeated_decisions() {
    let store = CausalStore::open_in_memory().unwrap();

    // Two incidents in different sessions with the same root cause
    store
        .record_decision(
            "deployed without running tests to production environment",
            "production crash with 500 errors at 3am",
            "caused",
            Some("incident_001"),
            0.9,
            "test",
        )
        .unwrap();
    store
        .record_decision(
            "deployed without running tests to staging environment",
            "staging crash with 500 errors during smoke test",
            "caused",
            Some("incident_002"),
            0.85,
            "test",
        )
        .unwrap();

    // Run pattern miner
    let miner = PatternMiner::new(&store, MinerConfig::default());
    let report = miner.mine().unwrap();

    println!("\n=== Meta-Edge Pattern Mining ===");
    println!("  repeated: {}, similar_to: {}", report.repeated, report.similar_to);
    println!("  skipped_self: {}, skipped_short: {}", report.skipped_self, report.skipped_short);

    // The miner should detect that these two decisions are similar
    assert!(
        report.repeated > 0 || report.similar_to > 0,
        "should detect cross-session pattern between similar incidents"
    );
}

#[test]
fn meta_pattern_miner_dry_run_writes_nothing() {
    let store = CausalStore::open_in_memory().unwrap();

    store
        .record_decision(
            "skipped code review to merge faster",
            "hidden bugs caused production issues",
            "caused",
            Some("sprint_a"),
            0.8,
            "test",
        )
        .unwrap();
    store
        .record_decision(
            "skipped code review to ship faster",
            "hidden bugs caused customer complaints",
            "caused",
            Some("sprint_b"),
            0.8,
            "test",
        )
        .unwrap();

    // Dry run
    let miner = PatternMiner::new(&store, MinerConfig::default());
    let dry = miner.mine_dry_run().unwrap();

    // Check no meta edges were written
    let store_meta_count = store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM meta_causal_edges",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap();

    println!("\n=== Dry Run ===");
    println!("  detected: {} repeated, {} similar", dry.repeated, dry.similar_to);
    println!("  meta edges in DB: {store_meta_count}");

    assert_eq!(
        store_meta_count, 0,
        "dry run must not write meta edges"
    );
    assert!(
        dry.repeated > 0 || dry.similar_to > 0,
        "dry run should still detect patterns"
    );
}

#[test]
fn meta_pattern_miner_ignores_dissimilar_decisions() {
    let store = CausalStore::open_in_memory().unwrap();

    // Two completely unrelated decisions
    store
        .record_decision(
            "configured nginx reverse proxy for load balancing",
            "reduced server response time by 40 percent",
            "caused",
            Some("task_a"),
            0.8,
            "test",
        )
        .unwrap();
    store
        .record_decision(
            "organized team building pottery class on tuesday",
            "team morale improved according to survey",
            "caused",
            Some("task_b"),
            0.7,
            "test",
        )
        .unwrap();

    let miner = PatternMiner::new(&store, MinerConfig::default());
    let report = miner.mine().unwrap();

    println!("\n=== Dissimilar Decisions ===");
    println!("  repeated: {}, similar_to: {}", report.repeated, report.similar_to);

    // Should NOT detect any pattern between nginx and pottery
    assert_eq!(
        report.repeated + report.similar_to,
        0,
        "unrelated decisions should not produce meta edges"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. CO-OCCURRENCE HEBBIAN — Use-Frequency Strengthening
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn hebbian_repeated_co_activation_strengthens_connection() {
    // Build a graph with a co-occurrence edge between two nodes.
    // Initial weight 0.01 (well below the Hebbian equilibrium of η/λ ≈ 4.0).
    let nodes = vec![
        mk("redis", "redis caching layer"),
        mk("performance", "application performance metrics"),
        mk("unrelated", "unrelated system component"),
    ];
    let edges = vec![
        edge("redis", "performance", Relation::CoOccurrence, 0.01),
        edge("redis", "unrelated", Relation::CoOccurrence, 0.01),
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    // Find the co-occurrence edge between redis(0) and performance(1)
    // Edge 0 is redis→performance (first in build order)
    let redis_perf_edge = 0usize;
    let initial_weight = graph.edge_raw_weight(redis_perf_edge);

    // Simulate 10 retrievals where redis + performance are always co-active
    for _ in 0..10 {
        graph.hebbian_update(&[0, 1], 0.995, 0.02); // nodes 0=redis, 1=performance
    }

    let final_weight = graph.edge_raw_weight(redis_perf_edge);

    println!("\n=== Hebbian Co-Activation ===");
    println!("  redis↔performance weight: {initial_weight:.4} → {final_weight:.4}");

    assert!(
        final_weight > initial_weight,
        "co-active nodes should strengthen: {final_weight} > {initial_weight}"
    );
}

#[test]
fn hebbian_non_co_active_decays() {
    let nodes = vec![
        mk("redis", "redis caching layer"),
        mk("performance", "application performance metrics"),
    ];
    let edges = vec![
        edge("redis", "performance", Relation::CoOccurrence, 0.05),
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    let edge_idx = 0;
    let initial_weight = graph.edge_raw_weight(edge_idx);

    // Simulate 10 retrievals where ONLY redis is active (performance is not)
    for _ in 0..10 {
        graph.hebbian_update(&[0], 0.995, 0.02); // only node 0 active
    }

    let final_weight = graph.edge_raw_weight(edge_idx);

    println!("\n=== Hebbian Decay ===");
    println!("  redis↔performance weight: {initial_weight:.4} → {final_weight:.4}");

    assert!(
        final_weight < initial_weight,
        "non-co-active nodes should decay: {final_weight} < {initial_weight}"
    );
}

#[test]
fn hebbian_differential_reinforcement() {
    // Two edges from the same node: one gets co-activated a lot, the other doesn't
    let nodes = vec![
        mk("deploy", "deploy to production"),
        mk("tested", "well-tested deployment"),
        mk("untested", "poorly-tested deployment"),
    ];
    let edges = vec![
        edge("deploy", "tested", Relation::CoOccurrence, 0.01),
        edge("deploy", "untested", Relation::CoOccurrence, 0.01),
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    // Always co-activate deploy + tested, never deploy + untested
    for _ in 0..20 {
        graph.hebbian_update(&[0, 1], 0.995, 0.02);
    }

    let tested_weight = graph.edge_raw_weight(0);
    let untested_weight = graph.edge_raw_weight(1);

    println!("\n=== Differential Reinforcement ===");
    println!("  deploy↔tested:   {tested_weight:.4} (reinforced)");
    println!("  deploy↔untested: {untested_weight:.4} (not reinforced)");

    assert!(
        tested_weight > untested_weight,
        "reinforced edge should be stronger: {tested_weight} > {untested_weight}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. INTERVENTION QUERY — Forward Simulation / Prediction
// ═══════════════════════════════════════════════════════════════════════════

/// Build a prediction graph: actions and their forward consequences.
fn build_prediction_graph() -> CausalGraph {
    let nodes = vec![
        mk("skip_tests", "skip tests"),
        mk("crash", "production crash"),
        mk("downtime", "service downtime"),
        mk("complaints", "user complaints"),
        mk("add_tests", "add tests"),
        mk("stable", "stable release"),
        mk("fast_merge", "faster merge"),
        mk("tech_debt", "technical debt"),
    ];
    let edges = vec![
        // Forward chain from skip_tests
        edge("skip_tests", "crash", Relation::Caused, 0.9),
        edge("crash", "downtime", Relation::Caused, 0.8),
        edge("downtime", "complaints", Relation::Caused, 0.7),
        // Alternative: skip_tests → fast_merge (short term benefit)
        edge("skip_tests", "fast_merge", Relation::Enabled, 0.6),
        // But also → tech_debt
        edge("skip_tests", "tech_debt", Relation::Caused, 0.5),
        // Good path: add_tests → stable (prevented crash)
        edge("add_tests", "stable", Relation::Caused, 0.85),
        edge("add_tests", "crash", Relation::Prevented, 0.9),
    ];
    CausalGraph::build(&nodes, &edges)
}

#[test]
fn intervention_forward_predicts_consequences() {
    let mut graph = build_prediction_graph();

    // "What if I skip tests?" — forward spreading activation
    let results = graph.spreading_activation_opts("skip tests", None, false, false);

    println!("\n=== Intervention Query: 'skip tests' ===");
    for r in &results {
        let signal = if r.activation > 0.0 { "+" } else { "−" };
        println!("  [{signal}{:.3}] {}", r.activation.abs(), r.text);
    }

    // Should predict: crash (caused), fast_merge (enabled), tech_debt (caused)
    let predicts_crash = results.iter().any(|r| r.text.contains("crash") && r.activation > 0.0);
    let predicts_fast = results.iter().any(|r| r.text.contains("faster") && r.activation > 0.0);

    assert!(predicts_crash, "should predict crash as a consequence");
    assert!(
        predicts_fast,
        "should predict faster merge as an enabled outcome"
    );
}

#[test]
fn intervention_multi_hop_prediction() {
    let mut graph = build_prediction_graph();

    let results = graph.spreading_activation_opts("skip tests", None, false, false);

    // 2-hop: skip_tests → crash → downtime
    let predicts_downtime = results
        .iter()
        .any(|r| r.text.contains("downtime") && r.activation > 0.0);
    // 3-hop: skip_tests → crash → downtime → complaints
    let predicts_complaints = results
        .iter()
        .any(|r| r.text.contains("complaints") && r.activation > 0.0);

    println!("\n=== Multi-Hop Prediction ===");
    println!("  predicts downtime:   {predicts_downtime}");
    println!("  predicts complaints: {predicts_complaints}");

    assert!(
        predicts_downtime,
        "2-hop prediction: skip_tests → crash → downtime"
    );
}

#[test]
fn intervention_prevented_edge_as_warning() {
    let mut graph = build_prediction_graph();

    // "What if I add tests?" — should show stable (+) AND crash (−, prevented)
    let results = graph.spreading_activation_opts("add tests", None, false, false);

    println!("\n=== Intervention Query: 'add tests' ===");
    for r in &results {
        let signal = if r.activation > 0.0 { "+" } else { "−" };
        println!("  [{signal}{:.3}] {}", r.activation.abs(), r.text);
    }

    let stable = results.iter().find(|r| r.text.contains("stable"));
    let crash = results.iter().find(|r| r.text.contains("crash"));

    assert!(
        stable.is_some() && stable.unwrap().activation > 0.0,
        "adding tests should predict stable release (+)"
    );
    assert!(
        crash.is_some() && crash.unwrap().activation < 0.0,
        "adding tests should show crash as prevented (−, warning that this action blocks it)"
    );
}

#[test]
fn intervention_comparison_good_vs_bad() {
    let mut graph = build_prediction_graph();

    // Compare "skip tests" vs "add tests" — the system should produce
    // qualitatively different predictions
    let skip_results = graph.spreading_activation_opts("skip tests", None, false, false);
    let add_results = graph.spreading_activation_opts("add tests", None, false, false);

    // Skip tests: crash is positive (will happen)
    let skip_crash = skip_results
        .iter()
        .find(|r| r.text.contains("crash"))
        .map(|r| r.activation)
        .unwrap_or(0.0);

    // Add tests: crash is negative (prevented)
    let add_crash = add_results
        .iter()
        .find(|r| r.text.contains("crash"))
        .map(|r| r.activation)
        .unwrap_or(0.0);

    println!("\n=== Intervention Comparison ===");
    println!("  'skip tests' → crash activation: {skip_crash:+.3} (predicted to happen)");
    println!("  'add tests'  → crash activation: {add_crash:+.3} (predicted to be prevented)");

    assert!(
        skip_crash > 0.0 && add_crash < 0.0,
        "same outcome, opposite polarity: skip=caused (+), add=prevented (−)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// SUMMARY
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn advanced_dynamics_summary() {
    println!("\n╔══════════════════════════════════════════════════╗");
    println!("║   Advanced Dynamics — Coverage Summary          ║");
    println!("╚══════════════════════════════════════════════════╝\n");

    // Run all three subsystems and report
    let mut passed = 0;
    let mut total = 0;

    let tests: Vec<(&str, fn())> = vec![
        ("meta_pattern_mining", || {
            let store = CausalStore::open_in_memory().unwrap();
            store.record_decision(
                "deployed without running tests to production",
                "production crash with errors",
                "caused", Some("inc1"), 0.9, "test",
            ).unwrap();
            store.record_decision(
                "deployed without running tests to staging",
                "staging crash with errors",
                "caused", Some("inc2"), 0.85, "test",
            ).unwrap();
            let miner = PatternMiner::new(&store, MinerConfig::default());
            let r = miner.mine().unwrap();
            assert!(r.repeated > 0 || r.similar_to > 0);
        }),
        ("hebbian_reinforcement", || {
            let nodes = vec![mk("a", "node a"), mk("b", "node b")];
            let edges = vec![edge("a", "b", Relation::CoOccurrence, 0.01)];
            let mut g = CausalGraph::build(&nodes, &edges);
            for _ in 0..10 {
                g.hebbian_update(&[0, 1], 0.995, 0.02);
            }
            assert!(g.edge_raw_weight(0) > 0.01);
        }),
        ("intervention_prediction", || {
            let mut g = build_prediction_graph();
            let r = g.spreading_activation_opts("skip tests", None, false, false);
            assert!(r.iter().any(|r| r.text.contains("crash")));
        }),
    ];

    for (name, test) in &tests {
        total += 1;
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(test)).is_ok() {
            passed += 1;
            println!("  ✅ {name}");
        } else {
            println!("  ❌ {name}");
        }
    }

    println!("\n  Advanced dynamics: {passed}/{total} capabilities verified");
    assert_eq!(passed, total, "all advanced dynamics must pass");
}
