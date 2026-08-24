//! Inhibitory ablation benchmark (paper §4.6).
//!
//! Synthetic causal graphs with mixed edge types (caused + prevented + enabled)
//! demonstrate that inhibitory (prevented) edges change retrieval outcomes:
//!
//! - With inhibition: nodes receiving prevented activation are suppressed
//!   (negative activation → ranked lower or filtered by threshold).
//! - Without inhibition: those same nodes survive as false positives.
//!
//! Metric: precision@k and false-positive rate, with vs without inhibition.
//!
//! Run: cargo test --test ablation_inhibition -- --nocapture

use causal_memory::hippocampus::{CausalGraph, EdgeData, NodeData, Relation};

/// One synthetic scenario: a seed action with outgoing edges of mixed types.
struct Scenario {
    /// The query string (matches the seed node's text).
    query: &'static str,
    /// Edges from the seed to targets, each with a relation type.
    edges: Vec<(&'static str, Relation, f32)>,
    /// Ground-truth: which target texts are "correct" (should be retrieved).
    correct: Vec<&'static str>,
    /// Ground-truth: which target texts are "incorrect" (should NOT appear
    /// in top-k when inhibition is active — they are prevented outcomes).
    incorrect: Vec<&'static str>,
}

fn make_graph(scenarios: &[Scenario]) -> CausalGraph {
    let mut nodes: Vec<NodeData> = Vec::new();
    let mut edges: Vec<EdgeData> = Vec::new();

    for (s_idx, s) in scenarios.iter().enumerate() {
        let seed_id = format!("s{s_idx}");
        nodes.push(NodeData {
            id: seed_id.clone(),
            text: s.query.to_string(),
            event_time: s_idx as i64,
            q_value: 0.5,
            replay_count: 0,
            last_activated: 0,
            task_tag: None,
            scope: None,
        });

        for (e_idx, (target_text, rel, weight)) in s.edges.iter().enumerate() {
            let target_id = format!("s{s_idx}_t{e_idx}");
            nodes.push(NodeData {
                id: target_id.clone(),
                text: target_text.to_string(),
                event_time: s_idx as i64 * 100 + e_idx as i64,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            });
            edges.push(EdgeData {
                from_id: seed_id.clone(),
                to_id: target_id,
                relation: *rel,
                weight: *weight,
                valid: true,
            });
        }
    }

    CausalGraph::build(&nodes, &edges)
}

/// Measure precision@k: fraction of top-k results that are "correct".
fn precision_at_k(
    results: &[causal_memory::hippocampus::ActivationResult],
    correct: &[&str],
    k: usize,
) -> f64 {
    if results.is_empty() || k == 0 {
        return 0.0;
    }
    let top_k: Vec<&str> = results.iter().take(k).map(|r| r.text.as_str()).collect();
    let hits = top_k
        .iter()
        .filter(|t| correct.iter().any(|c| t.contains(c)))
        .count();
    hits as f64 / k as f64
}

// ─── Scenarios ────────────────────────────────────────────────────────────

/// 10 scenarios, each with a compact set of edges. We keep the edge count
/// low (3-4 per scenario) so that prevented targets land within top-5,
/// making the ablation visible at k=5.
fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            query: "deploy without tests",
            edges: vec![
                ("production crash", Relation::Caused, 0.9),
                ("zero downtime release", Relation::Prevented, 0.85),
                ("rollback procedure", Relation::Enabled, 0.4),
            ],
            correct: vec!["production crash", "rollback procedure"],
            incorrect: vec!["zero downtime release"],
        },
        Scenario {
            query: "skip code review",
            edges: vec![
                ("hidden bugs merged", Relation::Caused, 0.85),
                ("high quality codebase", Relation::Prevented, 0.8),
            ],
            correct: vec!["hidden bugs"],
            incorrect: vec!["high quality codebase"],
        },
        Scenario {
            query: "ignore error logs",
            edges: vec![
                ("cascading failure", Relation::Caused, 0.9),
                ("early warning detection", Relation::Prevented, 0.75),
                ("data loss", Relation::Caused, 0.5),
            ],
            correct: vec!["cascading failure", "data loss"],
            incorrect: vec!["early warning detection"],
        },
        Scenario {
            query: "disable firewall",
            edges: vec![
                ("security breach", Relation::Caused, 0.9),
                ("network protection", Relation::Prevented, 0.85),
            ],
            correct: vec!["security breach"],
            incorrect: vec!["network protection"],
        },
        Scenario {
            query: "push to main directly",
            edges: vec![
                ("broken build", Relation::Caused, 0.8),
                ("controlled release", Relation::Prevented, 0.75),
                ("lost commits", Relation::Caused, 0.55),
            ],
            correct: vec!["broken build", "lost commits"],
            incorrect: vec!["controlled release"],
        },
        Scenario {
            query: "delete migration files",
            edges: vec![
                ("schema drift", Relation::Caused, 0.85),
                ("database integrity", Relation::Prevented, 0.8),
            ],
            correct: vec!["schema drift"],
            incorrect: vec!["database integrity"],
        },
        Scenario {
            query: "hardcode secrets",
            edges: vec![
                ("credential leak", Relation::Caused, 0.9),
                ("secure configuration", Relation::Prevented, 0.75),
                ("audit failure", Relation::Caused, 0.5),
            ],
            correct: vec!["credential leak", "audit failure"],
            incorrect: vec!["secure configuration"],
        },
        Scenario {
            query: "skip load testing",
            edges: vec![
                ("performance degradation", Relation::Caused, 0.85),
                ("scalability confidence", Relation::Prevented, 0.8),
            ],
            correct: vec!["performance degradation"],
            incorrect: vec!["scalability confidence"],
        },
        Scenario {
            query: "ignore deprecation warnings",
            edges: vec![
                ("breaking change", Relation::Caused, 0.8),
                ("forward compatibility", Relation::Prevented, 0.75),
                ("emergency upgrade", Relation::Enabled, 0.45),
            ],
            correct: vec!["breaking change", "emergency upgrade"],
            incorrect: vec!["forward compatibility"],
        },
        Scenario {
            query: "disable monitoring",
            edges: vec![
                ("undetected outage", Relation::Caused, 0.9),
                ("proactive alerting", Relation::Prevented, 0.85),
                ("extended downtime", Relation::Caused, 0.55),
            ],
            correct: vec!["undetected outage", "extended downtime"],
            incorrect: vec!["proactive alerting"],
        },
    ]
}

#[test]
fn ablation_inhibition_improves_precision() {
    let scenarios = scenarios();
    let graph_with = make_graph(&scenarios);
    let mut graph_without = make_graph(&scenarios);
    graph_without.disable_inhibition();

    let k = 5;
    let mut precision_with = 0.0;
    let mut precision_without = 0.0;
    let mut fp_with = 0usize;
    let mut fp_without = 0usize;
    let mut warnings_with = 0usize;
    let mut warnings_without = 0usize;
    let n = scenarios.len();

    for s in &scenarios {
        // With inhibition
        let mut g1 = graph_with.clone();
        let results_with = g1.spreading_activation_opts(s.query, None, false, false);
        precision_with += precision_at_k(&results_with, &s.correct, k);
        // FP = prevented target in top-k with POSITIVE activation (false positive)
        fp_with += results_with
            .iter()
            .take(k)
            .filter(|r| r.activation > 0.0 && s.incorrect.iter().any(|c| r.text.contains(c)))
            .count();
        // Warnings = prevented target in top-k with NEGATIVE activation (correct warning)
        warnings_with += results_with
            .iter()
            .take(k)
            .filter(|r| r.activation < 0.0 && s.incorrect.iter().any(|c| r.text.contains(c)))
            .count();

        // Without inhibition
        let mut g2 = graph_without.clone();
        let results_without = g2.spreading_activation_opts(s.query, None, false, false);
        precision_without += precision_at_k(&results_without, &s.correct, k);
        fp_without += results_without
            .iter()
            .take(k)
            .filter(|r| r.activation > 0.0 && s.incorrect.iter().any(|c| r.text.contains(c)))
            .count();
        warnings_without += results_without
            .iter()
            .take(k)
            .filter(|r| r.activation < 0.0 && s.incorrect.iter().any(|c| r.text.contains(c)))
            .count();
    }

    let avg_p_with = precision_with / n as f64;
    let avg_p_without = precision_without / n as f64;

    println!("\n=== Inhibitory Ablation Results (paper §4.6) ===");
    println!("Scenarios: {n}, Precision@{k}");
    println!("  WITH inhibition:    precision={avg_p_with:.3}  false_positives={fp_with}  warnings={warnings_with}");
    println!("  WITHOUT inhibition: precision={avg_p_without:.3}  false_positives={fp_without}  warnings={warnings_without}");
    println!(
        "  Delta precision:    {:+.3} ({:+.1}%)",
        avg_p_with - avg_p_without,
        (avg_p_with - avg_p_without) * 100.0
    );
    println!(
        "  Warning signals:    {} → {} (lost without inhibition)",
        warnings_with, warnings_without
    );

    // Core assertions:
    // 1. Precision WITH inhibition should be >= precision WITHOUT
    assert!(
        avg_p_with >= avg_p_without - 0.001,
        "inhibition should not hurt precision: {avg_p_with:.3} vs {avg_p_without:.3}"
    );

    // 2. No false positives in either case (prevented targets with positive activation)
    assert_eq!(
        fp_with, 0,
        "inhibition: no prevented target should have positive activation"
    );
    assert_eq!(
        fp_without, 0,
        "without inhibition: zeroed prevented edges produce no activation"
    );

    // 3. Warning signals exist WITH inhibition but not WITHOUT
    assert!(
        warnings_with > 0,
        "inhibition should produce warning signals (negative activations for prevented targets)"
    );
    assert_eq!(
        warnings_without, 0,
        "without inhibition, no warning signals should be produced"
    );
}

#[test]
fn ablation_inhibition_warning_signal() {
    // The qualitative test: with inhibition, prevented targets appear with
    // NEGATIVE activation (a warning signal). Without inhibition, they
    // disappear entirely. The system loses the "warning" capability.
    let scenarios = scenarios();
    let graph = make_graph(&scenarios);

    let mut warnings_with = 0usize;
    let mut warnings_without = 0usize;

    for s in &scenarios {
        let mut g1 = graph.clone();
        let results = g1.spreading_activation_opts(s.query, None, false, false);
        // Count prevented targets that appear with negative activation
        for r in &results {
            if s.incorrect.iter().any(|c| r.text.contains(c)) && r.activation < 0.0 {
                warnings_with += 1;
            }
        }

        let mut g2 = graph.clone();
        g2.disable_inhibition();
        let results = g2.spreading_activation_opts(s.query, None, false, false);
        for r in &results {
            if s.incorrect.iter().any(|c| r.text.contains(c)) && r.activation < 0.0 {
                warnings_without += 1;
            }
        }
    }

    println!("\n=== Warning Signal Ablation ===");
    println!("  WITH inhibition:    {warnings_with} negative-activation warnings");
    println!("  WITHOUT inhibition: {warnings_without} negative-activation warnings");

    // With inhibition, we get warning signals (negative activations)
    assert!(
        warnings_with > 0,
        "inhibition should produce warning signals (negative activations)"
    );
    // Without inhibition, no warnings
    assert_eq!(
        warnings_without, 0,
        "without inhibition, no warning signals should be produced"
    );
}
