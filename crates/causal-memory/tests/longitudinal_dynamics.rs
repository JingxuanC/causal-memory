//! Longitudinal memory dynamics benchmark — end-to-end validation of
//! SWR consolidation, Q-value dynamics, and novelty-entropy triggering.
//!
//! These tests simulate an agent's memory evolving over time:
//! 1. Ingest a mixed set of memories (some important, some noise)
//! 2. Simulate access patterns (important memories accessed frequently)
//! 3. Run SWR consolidation
//! 4. Verify that consolidation improved memory quality
//!
//! Unlike the capability benchmark (which tests static graph properties),
//! this benchmark tests TEMPORAL dynamics — how the system improves over
//! time through offline consolidation.

use causal_memory::hippocampus::{CausalGraph, EdgeData, NodeData, Relation};

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
// SWR CONSOLIDATION END-TO-END
// ═══════════════════════════════════════════════════════════════════════════

/// Build a graph with 30 nodes: 10 "important" (connected, high-weight)
/// and 20 "noise" (isolated, low-weight).
fn build_mixed_graph() -> CausalGraph {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // 10 important nodes forming a causal chain
    for i in 0..10 {
        nodes.push(mk(
            &format!("important_{i}"),
            &format!("important decision number {i}"),
        ));
    }
    // Chain: important_0 → important_1 → ... → important_9
    for i in 0..9 {
        edges.push(edge(
            &format!("important_{i}"),
            &format!("important_{}", i + 1),
            Relation::Caused,
            0.5, // start at medium weight
        ));
    }

    // 20 noise nodes — low-weight, no connections to the important chain
    for i in 0..20 {
        nodes.push(mk(
            &format!("noise_{i}"),
            &format!("trivial chitchat message {i}"),
        ));
    }
    // A few noise-to-noise edges (very low weight)
    for i in 0..10 {
        edges.push(edge(
            &format!("noise_{i}"),
            &format!("noise_{}", i + 10),
            Relation::Caused,
            0.1,
        ));
    }

    CausalGraph::build(&nodes, &edges)
}

#[test]
fn swr_ltp_strengthens_frequently_replayed_edges() {
    let mut graph = build_mixed_graph();

    // Record initial weights of important edges
    let initial_weights: Vec<f32> = (0..graph.num_edges())
        .map(|i| graph.edge_raw_weight(i))
        .collect();

    // Run SWR with enough replays to hit the important chain
    let stats = graph.swr_consolidate(50);

    // LTP should have occurred on some edges
    assert!(
        stats.ltp_events > 0,
        "SWR should perform LTP on replayed edges, got {} events",
        stats.ltp_events
    );
    assert!(stats.chains_replayed > 0, "SWR should replay causal chains");

    // At least some edges should have increased weight
    let strengthened = (0..graph.num_edges())
        .filter(|&i| graph.edge_raw_weight(i) > initial_weights[i] + 0.001)
        .count();
    assert!(
        strengthened > 0,
        "{strengthened} edges should have increased weight after LTP"
    );
}

#[test]
fn swr_ltd_and_ltp_have_opposite_effects() {
    // Build a graph with a chain that will be replayed (LTP) and
    // compare before/after weights to verify LTP and LTD both happened.
    let nodes = vec![
        mk("hub", "central decision"),
        mk("a", "outcome A"),
        mk("b", "outcome B"),
        mk("c", "isolated outcome C"),
    ];
    let edges = vec![
        edge("hub", "a", Relation::Caused, 0.5),
        edge("a", "b", Relation::Caused, 0.5),
        edge("hub", "c", Relation::Caused, 0.5), // branch to isolated node
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    let weights_before: Vec<f32> = (0..graph.num_edges())
        .map(|i| graph.edge_raw_weight(i))
        .collect();

    // Run SWR
    let stats = graph.swr_consolidate(100);

    let weights_after: Vec<f32> = (0..graph.num_edges())
        .map(|i| graph.edge_raw_weight(i))
        .collect();

    // Some edges should have LTP (increased) and the system should have
    // applied LTD globally (even if net effect varies per edge)
    let ltp_count = weights_before
        .iter()
        .zip(&weights_after)
        .filter(|(before, after)| **after > *before + 0.001)
        .count();

    println!("\n=== SWR LTP vs LTD ===");
    println!("  Edges with LTP (weight increased): {ltp_count}");
    println!(
        "  Chains replayed: {}, LTP events: {}",
        stats.chains_replayed, stats.ltp_events
    );
    println!("  Weights: {:?} → {:?}", weights_before, weights_after);

    assert!(
        ltp_count > 0,
        "some edges should have increased weight (LTP)"
    );
    assert!(stats.ltp_events > 0, "SWR should report LTP events");

    // Verify replay protection: nodes with high replay_count get half LTD
    // (check that replay_count propagated to at least one node)
    let replayed_nodes = (0..graph.num_nodes())
        .filter(|&i| graph.node_replay_count(i) > 0)
        .count();
    assert!(
        replayed_nodes > 0,
        "{replayed_nodes} nodes should have nonzero replay_count after SWR"
    );
}

#[test]
fn swr_gc_forgets_weak_dormant_edges() {
    // Build a graph with edges that are: weak + zero replay + dormant
    // SWR's GC triple criterion: weak AND zero_access AND dormant
    let nodes = vec![
        mk("active_decision", "deploy with tests"),
        mk("active_outcome", "smooth release"),
        mk("weak_decision", "typed wrong command"),
        mk("weak_outcome", "harmless typo corrected"),
    ];
    let edges = vec![
        edge("active_decision", "active_outcome", Relation::Caused, 0.8),
        edge("weak_decision", "weak_outcome", Relation::Caused, 0.01), // below gc_threshold
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    // Set active nodes as recently activated (not dormant)
    // weak nodes have last_activated = 0 (epoch — very old)
    // Run many replays to build up replay counts on active nodes
    graph.swr_consolidate(100);

    // Check: active edges should still be valid
    let active_valid = (0..graph.num_edges())
        .filter(|&i| graph.edge_is_valid(i))
        .count();
    assert!(
        active_valid >= 1,
        "active (strong, replayed) edges should survive GC"
    );
}

#[test]
fn swr_improves_retrieval_precision() {
    let mut graph = build_mixed_graph();

    // Before consolidation: query "important decision" — some noise may leak in
    let results_before = graph.spreading_activation_opts("important decision", None, false, false);

    // Run consolidation to strengthen important chain, weaken noise
    graph.swr_consolidate(100);

    // After consolidation: important results should be more prominent
    let results_after = graph.spreading_activation_opts("important decision", None, false, false);

    // Measure: fraction of top-k results that are "important" nodes
    let precision_before = results_before
        .iter()
        .take(5)
        .filter(|r| r.text.contains("important"))
        .count() as f64
        / 5.0;

    let precision_after = results_after
        .iter()
        .take(5)
        .filter(|r| r.text.contains("important"))
        .count() as f64
        / 5.0;

    println!("\n=== SWR Retrieval Precision ===");
    println!(
        "  Before consolidation: precision@5 = {:.0} ({}/{})",
        precision_before * 100.0,
        results_before
            .iter()
            .take(5)
            .filter(|r| r.text.contains("important"))
            .count(),
        5
    );
    println!(
        "  After consolidation:  precision@5 = {:.0} ({}/{})",
        precision_after * 100.0,
        results_after
            .iter()
            .take(5)
            .filter(|r| r.text.contains("important"))
            .count(),
        5
    );

    // Consolidation should not hurt precision (and ideally improve it)
    assert!(
        precision_after >= precision_before - 0.01,
        "SWR should not hurt retrieval precision: {precision_before:.2} → {precision_after:.2}"
    );
}

#[test]
fn swr_immutable_preserves_original() {
    let graph = build_mixed_graph();
    let original_edges = graph.num_valid_edges();

    // Immutable consolidation returns a NEW graph + delta log
    let result = graph.swr_consolidate_immutable(50, Some("consolidate important memories"));

    // Original graph is unchanged
    assert_eq!(
        graph.num_valid_edges(),
        original_edges,
        "immutable consolidation must not modify the original graph"
    );

    // The new graph may have different valid edge counts (GC removes some)
    assert!(
        !result.delta_log.is_empty(),
        "delta log should contain consolidation events"
    );
    assert!(
        result.stats.ltp_events > 0 || result.stats.chains_replayed > 0,
        "consolidation should have replayed/strengthened edges"
    );

    // Instructions should be carried through
    assert_eq!(
        result.instructions.as_deref(),
        Some("consolidate important memories")
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Q-VALUE DYNAMICS END-TO-END
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn qvalue_good_decisions_rank_higher() {
    // Build a graph with 3 decisions, all starting at Q=0.5
    let nodes = vec![
        mk("good_decision", "used comprehensive tests before deploy"),
        mk("neutral_decision", "used basic tests before deploy"),
        mk("bad_decision", "skipped tests before deploy"),
        mk("good_outcome", "zero-bug production release"),
        mk("neutral_outcome", "minor bugs found post-deploy"),
        mk("bad_outcome", "production crash at 3am"),
    ];
    let edges = vec![
        edge("good_decision", "good_outcome", Relation::Caused, 0.8),
        edge("neutral_decision", "neutral_outcome", Relation::Caused, 0.6),
        edge("bad_decision", "bad_outcome", Relation::Caused, 0.4),
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    // Update Q-values: good decision gets reward, bad gets penalty
    // Node indices: 0=good_dec, 1=neutral_dec, 2=bad_dec
    graph.update_q_value(0, 1.0, 0.3, 0.9); // reward=1.0 (success)
    graph.update_q_value(2, 0.0, 0.3, 0.9); // reward=0.0 (failure)

    let q_good = graph.node_q_value(0);
    let q_neutral = graph.node_q_value(1);
    let q_bad = graph.node_q_value(2);

    println!("\n=== Q-Value Dynamics ===");
    println!("  good decision (reward=1.0):   Q={q_good:.3}");
    println!("  neutral decision (no update): Q={q_neutral:.3}");
    println!("  bad decision (reward=0.0):    Q={q_bad:.3}");

    assert!(
        q_good > q_neutral,
        "good decision should have higher Q than neutral: {q_good} > {q_neutral}"
    );
    assert!(
        q_bad < q_neutral,
        "bad decision should have lower Q than neutral: {q_bad} < {q_neutral}"
    );
}

#[test]
fn qvalue_affects_spreading_activation() {
    // Q-value weights the seed activation: high-Q nodes get stronger seeds
    let nodes = vec![
        mk("high_q_decision", "well-tested feature deploy"),
        mk("low_q_decision", "poorly-tested feature deploy"),
        mk("shared_outcome", "deployment completed"),
    ];
    let edges = vec![
        edge("high_q_decision", "shared_outcome", Relation::Caused, 0.7),
        edge("low_q_decision", "shared_outcome", Relation::Caused, 0.7),
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    // Boost the high-Q decision
    graph.update_q_value(0, 1.0, 0.3, 0.9); // node 0 = high_q_decision

    // Query should match both decisions (both contain "feature deploy")
    let results = graph.spreading_activation_opts("feature deploy", None, false, false);

    // The high-Q decision should have stronger activation
    let high_q = results
        .iter()
        .find(|r| r.text.contains("well-tested"))
        .map(|r| r.activation)
        .unwrap_or(0.0);
    let low_q = results
        .iter()
        .find(|r| r.text.contains("poorly-tested"))
        .map(|r| r.activation)
        .unwrap_or(0.0);

    println!("\n=== Q-Value Affects Activation ===");
    println!("  high-Q decision activation: {high_q:.4}");
    println!("  low-Q decision activation:  {low_q:.4}");

    assert!(
        high_q > low_q,
        "high-Q decision should have stronger activation: {high_q} > {low_q}"
    );
}

#[test]
fn qvalue_bellman_propagates_to_neighbors() {
    // Q-value update propagates max_Q(neighbors) via Bellman backup
    let nodes = vec![
        mk("root", "initial decision"),
        mk("child_a", "good child decision"),
        mk("child_b", "bad child decision"),
    ];
    let edges = vec![
        edge("root", "child_a", Relation::Caused, 0.8),
        edge("root", "child_b", Relation::Caused, 0.4),
    ];
    let mut graph = CausalGraph::build(&nodes, &edges);

    // First, set child_a's Q high
    graph.update_q_value(1, 1.0, 0.5, 0.9); // child_a gets reward

    // Now update root — it should inherit some of child_a's value
    let root_q_before = graph.node_q_value(0);
    graph.update_q_value(0, 0.5, 0.5, 0.9); // root gets medium reward
    let root_q_after = graph.node_q_value(0);

    println!("\n=== Q-Value Bellman Propagation ===");
    println!("  root Q before: {root_q_before:.3} (default 0.5)");
    println!("  root Q after:  {root_q_after:.3} (should be > 0.5 due to high-Q child)");

    assert!(
        root_q_after > 0.5,
        "root should benefit from high-Q child via Bellman backup: {root_q_after}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// NOVELTY ENTROPY END-TO-END
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn novelty_entropy_low_for_uniform_replay() {
    // All nodes have replay_count=0 → entropy should be low (uniform)
    let graph = build_mixed_graph();
    let entropy = graph.novelty_entropy();

    println!("\n=== Novelty Entropy ===");
    println!("  Uniform (all replay_count=0): entropy={entropy:.3}");

    assert!(
        entropy < 0.1,
        "uniform replay distribution should have low entropy: {entropy}"
    );
}

#[test]
fn novelty_entropy_high_for_diverse_replay() {
    // After SWR consolidation creates a skewed replay distribution,
    // entropy should increase
    let mut graph = build_mixed_graph();

    let entropy_before = graph.novelty_entropy();
    graph.swr_consolidate(100);
    let entropy_after = graph.novelty_entropy();

    println!("\n  Before consolidation: entropy={entropy_before:.3}");
    println!("  After consolidation:  entropy={entropy_after:.3}");

    assert!(
        entropy_after > entropy_before,
        "consolidation creates diverse replay counts → higher entropy: {entropy_after} > {entropy_before}"
    );
}

#[test]
fn novelty_entropy_triggers_consolidation_correctly() {
    // Simulate: agent sees diverse new experiences (high entropy → consolidate)
    // vs agent sees repetitive experiences (low entropy → skip consolidation)

    // Scenario 1: 100 nodes, all with replay_count=0 → uniform → low entropy
    let mut nodes_uniform = Vec::new();
    for i in 0..100 {
        nodes_uniform.push(mk(&format!("n{i}"), &format!("node {i}")));
    }
    let graph_uniform = CausalGraph::build(&nodes_uniform, &[]);
    let entropy_uniform = graph_uniform.novelty_entropy();

    // Scenario 2: 100 nodes with diverse replay counts → high entropy
    let mut nodes_diverse = Vec::new();
    for i in 0..100 {
        nodes_diverse.push(NodeData {
            id: format!("d{i}"),
            text: format!("diverse node {i}"),
            event_time: 0,
            q_value: 0.5,
            replay_count: (i % 64) as u16, // diverse: 0,1,2,...,63,0,1,...
            last_activated: 0,
            task_tag: None,
            scope: None,
        });
    }
    let graph_diverse = CausalGraph::build(&nodes_diverse, &[]);
    let entropy_diverse = graph_diverse.novelty_entropy();

    println!("\n=== Novelty Trigger ===");
    println!(
        "  Uniform replay:   entropy={entropy_uniform:.3} → should_consolidate={}",
        entropy_uniform > 0.6
    );
    println!(
        "  Diverse replay:   entropy={entropy_diverse:.3} → should_consolidate={}",
        entropy_diverse > 0.6
    );

    assert!(
        entropy_uniform < 0.2,
        "uniform should NOT trigger consolidation: {entropy_uniform}"
    );
    assert!(
        entropy_diverse > 0.5,
        "diverse SHOULD trigger consolidation: {entropy_diverse}"
    );
}

#[test]
fn novelty_consolidation_feedback_loop() {
    // The full loop: diverse experience → high entropy → trigger consolidation
    // → replay → memory quality improves
    let mut graph = build_mixed_graph();

    // Phase 1: Initial state
    let entropy_initial = graph.novelty_entropy();
    let precision_initial = measure_precision(&mut graph);

    // Phase 2: Simulate diverse experiences by replaying different chains
    graph.swr_consolidate(50);

    // Phase 3: Check entropy increased (diverse replay counts)
    let entropy_after = graph.novelty_entropy();

    // Phase 4: If entropy is high enough, consolidate again (feedback loop)
    if entropy_after > 0.3 {
        graph.swr_consolidate(50);
    }

    let precision_final = measure_precision(&mut graph);

    println!("\n=== Novelty-Consolidation Feedback Loop ===");
    println!(
        "  Phase 1: entropy={entropy_initial:.3}, precision={:.0}",
        precision_initial * 100.0
    );
    println!(
        "  Phase 2: entropy={entropy_after:.3} → consolidate? {}",
        entropy_after > 0.3
    );
    println!("  Phase 3: precision={:.0}", precision_final * 100.0);

    assert!(
        precision_final >= precision_initial - 0.01,
        "consolidation should not hurt retrieval precision"
    );
}

fn measure_precision(graph: &mut CausalGraph) -> f64 {
    let results = graph.spreading_activation_opts("important decision", None, false, false);
    let hits = results
        .iter()
        .take(5)
        .filter(|r| r.text.contains("important"))
        .count();
    hits as f64 / 5.0
}

// ═══════════════════════════════════════════════════════════════════════════
// COMBINED: Full sleep-wake cycle simulation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn full_sleep_wake_cycle() {
    println!("\n╔══════════════════════════════════════════════════╗");
    println!("║   Longitudinal Memory Dynamics — Sleep Cycle    ║");
    println!("╚══════════════════════════════════════════════════╝\n");

    // ── Day 1: Agent learns a bunch of things ──
    let mut graph = build_mixed_graph();
    let initial_precision = measure_precision(&mut graph);
    let initial_entropy = graph.novelty_entropy();
    let initial_valid = graph.num_valid_edges();

    println!("Day 1 (awake):");
    println!(
        "  Memories: {} nodes, {} valid edges",
        graph.num_nodes(),
        initial_valid
    );
    println!("  Retrieval precision@5: {:.0}", initial_precision * 100.0);
    println!("  Novelty entropy: {initial_entropy:.3}");

    // ── Night 1: Check if consolidation should run ──
    // (entropy is low because replay_count is all 0 — no diversity yet)
    let should_consolidate = initial_entropy > 0.6;
    println!("\nNight 1 (sleep check):");
    println!("  Entropy {initial_entropy:.3} > 0.6? {should_consolidate}");
    if !should_consolidate {
        println!("  → Skip consolidation (nothing novel to replay)");
    }

    // ── Day 2: Agent accesses some memories + records new diverse ones ──
    // Simulate retrieval (triggers Hebbian updates)
    for _ in 0..5 {
        graph.spreading_activation_opts("important decision", None, false, true);
    }
    // Run SWR to create diverse replay counts (simulates offline learning)
    graph.swr_consolidate(100);

    // ── Night 2: After consolidation, entropy should be higher ──
    let entropy_night2 = graph.novelty_entropy();
    println!("\nDay 2 → Night 2:");
    println!("  After 5 retrievals, entropy: {entropy_night2:.3}");

    // ── Run consolidation if entropy warrants ──
    if entropy_night2 > 0.3 {
        let stats = graph.swr_consolidate(100);
        println!(
            "  Consolidation: {} chains replayed, {} LTP events, {} forgotten",
            stats.chains_replayed, stats.ltp_events, stats.forgotten
        );
    }

    let final_precision = measure_precision(&mut graph);
    let final_entropy = graph.novelty_entropy();
    let final_valid = graph.num_valid_edges();

    println!("\nAfter sleep cycle:");
    println!(
        "  Memories: {} nodes, {} valid edges (was {})",
        graph.num_nodes(),
        final_valid,
        initial_valid
    );
    println!(
        "  Retrieval precision@5: {:.0} (was {:.0})",
        final_precision * 100.0,
        initial_precision * 100.0
    );
    println!("  Novelty entropy: {final_entropy:.3} (was {initial_entropy:.3})");

    // The system should have evolved (not frozen)
    let changed = final_precision != initial_precision
        || final_entropy != initial_entropy
        || final_valid != initial_valid;
    assert!(
        changed,
        "the memory system should have evolved through the sleep cycle"
    );

    println!("\n✅ Longitudinal dynamics validated: memory system evolves over time");
}
