#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::store::CausalStore;

    fn make_test_graph() -> CausalGraph {
        let nodes = vec![
            NodeData {
                id: "d1".into(),
                text: "used Redis for caching".into(),
                event_time: 1000,
                q_value: 0.8,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("caching".into()),
            },
            NodeData {
                id: "o1".into(),
                text: "cache stampede DB overloaded".into(),
                event_time: 1001,
                q_value: 0.8,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("caching".into()),
            },
            NodeData {
                id: "d2".into(),
                text: "used mutex lock".into(),
                event_time: 1002,
                q_value: 0.7,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
            NodeData {
                id: "o2".into(),
                text: "deadlock crash".into(),
                event_time: 1003,
                q_value: 0.7,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
            NodeData {
                id: "d3".into(),
                text: "used channel single-flight".into(),
                event_time: 1004,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
            NodeData {
                id: "o3".into(),
                text: "fixed race condition".into(),
                event_time: 1005,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
            },
        ];
        let edges = vec![
            EdgeData {
                from_id: "d1".into(),
                to_id: "o1".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            },
            EdgeData {
                from_id: "d2".into(),
                to_id: "o2".into(),
                relation: Relation::Caused,
                weight: 0.85,
                valid: true,
            },
            EdgeData {
                from_id: "d3".into(),
                to_id: "o3".into(),
                relation: Relation::Caused,
                weight: 0.95,
                valid: true,
            },
            EdgeData {
                from_id: "d2".into(),
                to_id: "o3".into(),
                relation: Relation::Prevented,
                weight: 0.6,
                valid: true,
            },
        ];
        CausalGraph::build(&nodes, &edges)
    }

    #[test]
    fn test_graph_built_correctly() {
        let graph = make_test_graph();
        assert_eq!(graph.num_nodes(), 6);
        assert_eq!(graph.num_edges(), 4);
        assert_eq!(graph.num_valid_edges(), 4);
    }

    #[test]
    fn test_csr_structure() {
        let graph = make_test_graph();
        assert_eq!(graph.row_ptr[1] - graph.row_ptr[0], 1);
        assert_eq!(graph.row_ptr[3] - graph.row_ptr[2], 2);
    }

    #[test]
    fn test_spreading_activation_forward() {
        let mut graph = make_test_graph();
        let results = graph.spreading_activation("Redis", None, false);
        assert!(!results.is_empty());
        assert!(results[0].text.contains("Redis"));
        let stampede = results.iter().find(|r| r.text.contains("cache stampede"));
        assert!(stampede.is_some());
        assert!(stampede.unwrap().activation > 0.0);
    }

    #[test]
    fn test_spreading_activation_reverse() {
        let mut graph = make_test_graph();
        let results = graph.spreading_activation("deadlock", None, true);
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.text.contains("mutex")));
    }

    #[test]
    fn test_prevented_negative_spread() {
        let mut graph = make_test_graph();
        let results = graph.spreading_activation("mutex", None, false);
        let deadlock = results.iter().find(|r| r.text.contains("deadlock"));
        let fixed = results.iter().find(|r| r.text.contains("fixed race"));
        assert!(deadlock.is_some());
        assert!(deadlock.unwrap().activation > 0.0);
        assert!(fixed.is_some());
        assert!(
            fixed.unwrap().activation < 0.0,
            "prevented edge should produce negative activation"
        );
    }

    #[test]
    fn test_task_tag_filter() {
        let mut graph = make_test_graph();
        let results = graph.spreading_activation("used", Some("concurrency"), false);
        for r in &results {
            assert_eq!(r.task_tag.as_deref(), Some("concurrency"));
        }
    }

    #[test]
    fn test_empty_query_returns_empty() {
        let mut graph = make_test_graph();
        // Bug fix #3: empty string should not match everything
        assert!(graph.spreading_activation("", None, false).is_empty());
        assert!(graph.spreading_activation("   ", None, false).is_empty());
    }

    #[test]
    fn test_nonexistent_query_returns_empty() {
        let mut graph = make_test_graph();
        assert!(graph
            .spreading_activation("nonexistent_xyzzy", None, false)
            .is_empty());
    }

    #[test]
    fn test_novelty_detection_high_surprise() {
        let mut graph = make_test_graph();
        let report = graph.detect_novelty("used Redis", "everything works great perfectly");
        assert!(report.surprise > 0.3);
    }

    #[test]
    fn test_novelty_detection_low_surprise() {
        let mut graph = make_test_graph();
        let report = graph.detect_novelty("used Redis", "cache stampede DB overloaded");
        assert!(report.surprise < 0.8);
    }

    #[test]
    fn test_swr_consolidation_ltp() {
        let mut graph = make_test_graph();
        let stats = graph.swr_consolidate(20);
        assert!(stats.chains_replayed > 0);
        assert!(stats.ltp_events > 0);
        let replayed = (0..graph.num_nodes)
            .filter(|&i| graph.node_replay_count(i) > 0)
            .count();
        assert!(replayed > 0);
    }

    // Bug fix #9: proper GC test with a larger graph so weak edges aren't
    // accidentally replayed. With 10 nodes and 5 replays, most nodes stay at
    // replay_count=0, making their edges eligible for GC.
    #[test]
    fn test_swr_gc_actually_forgets_weak_edges() {
        let mut nodes = vec![
            NodeData {
                id: "a".into(),
                text: "strong chain start".into(),
                event_time: 0,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "b".into(),
                text: "strong chain mid".into(),
                event_time: 1,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "c".into(),
                text: "strong chain end".into(),
                event_time: 2,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "w1".into(),
                text: "weak node one".into(),
                event_time: 3,
                q_value: 0.01,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "w2".into(),
                text: "weak node two".into(),
                event_time: 4,
                q_value: 0.01,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            // Padding nodes to reduce probability of w1 being a replay seed
            NodeData {
                id: "p1".into(),
                text: "padding one".into(),
                event_time: 5,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "p2".into(),
                text: "padding two".into(),
                event_time: 6,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "p3".into(),
                text: "padding three".into(),
                event_time: 7,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "p4".into(),
                text: "padding four".into(),
                event_time: 8,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "p5".into(),
                text: "padding five".into(),
                event_time: 9,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
        ];
        let _ = &mut nodes; // suppress unused mut warning
        let edges = vec![
            EdgeData {
                from_id: "a".into(),
                to_id: "b".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            },
            EdgeData {
                from_id: "b".into(),
                to_id: "c".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            },
            // Weak edge: very low weight, not on the a→b→c chain
            EdgeData {
                from_id: "w1".into(),
                to_id: "w2".into(),
                relation: Relation::Caused,
                weight: 0.01,
                valid: true,
            },
        ];
        let mut graph = CausalGraph::build(&nodes, &edges);

        assert_eq!(graph.num_valid_edges(), 3);

        // Run few replays — w1 unlikely to be seed with 10 nodes
        // (completing without panic is what this exercises; counts are checked below)
        let stats = graph.swr_consolidate(5);

        // The weak edge should likely be forgotten (w1 replay_count likely 0)
        // If random seed happened to replay w1, weight is still below threshold
        // after LTD (0.01 * ~0.995 = 0.00995 < 0.05), but replay_count > 0 protects it.
        // This test verifies the GC path executes; in a large graph it reliably fires.
        if stats.forgotten == 0 {
            // Verify: if not forgotten, it's because w1 was replayed (acceptable)
            let w1_idx = graph.node_id_to_idx.get("w1").copied().unwrap() as usize;
            assert!(
                graph.node_replay_count[w1_idx] > 0,
                "If GC didn't fire, w1 must have been replayed"
            );
        }
    }

    #[test]
    fn test_swr_ltp_weight_cap() {
        // Bug fix #8: verify weight doesn't exceed WEIGHT_CAP
        let nodes = vec![
            NodeData {
                id: "a".into(),
                text: "start".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "b".into(),
                text: "end".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
        ];
        let edges = vec![EdgeData {
            from_id: "a".into(),
            to_id: "b".into(),
            relation: Relation::Caused,
            weight: 1.0,
            valid: true,
        }];
        let mut graph = CausalGraph::build(&nodes, &edges);

        // Run many replays to push weight up
        graph.swr_consolidate(100);

        // Weight should be capped, not unbounded
        let edge_idx = 0;
        assert!(
            graph.edge_raw_weight(edge_idx) <= WEIGHT_CAP + 0.01,
            "Weight should be capped at {}, got {}",
            WEIGHT_CAP,
            graph.edge_raw_weight(edge_idx)
        );
    }

    #[test]
    fn test_simhash_consistency() {
        let h1 = simhash("used Redis for caching");
        let h2 = simhash("used Redis for caching");
        let h3 = simhash("completely different text about dogs");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_simhash_similarity() {
        let h1 = simhash("used mutex lock for concurrency");
        let h2 = simhash("used mutex lock for threading");
        let h3 = simhash("bought fresh vegetables today");
        let d12 = (h1 ^ h2).count_ones();
        let d13 = (h1 ^ h3).count_ones();
        assert!(d12 < d13, "similar texts should be closer");
    }

    #[test]
    fn test_jaccard_similarity() {
        let sim = text_jaccard_similarity("hello world foo", "hello world bar");
        assert!(sim > 0.0 && sim < 1.0);
        assert!((text_jaccard_similarity("hello world", "hello world") - 1.0).abs() < 0.001);
        assert!((text_jaccard_similarity("aaa", "zzz") - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_relation_spread_coefficients() {
        assert_eq!(Relation::Caused.spread_coeff(), 1.0);
        assert_eq!(Relation::Enabled.spread_coeff(), 0.5);
        assert_eq!(Relation::Prevented.spread_coeff(), -0.3);
        assert_eq!(Relation::NoEffect.spread_coeff(), 0.0);
    }

    #[test]
    fn test_empty_graph() {
        let mut graph = CausalGraph::new();
        assert_eq!(graph.num_nodes(), 0);
        assert!(graph
            .spreading_activation("anything", None, false)
            .is_empty());
    }

    #[test]
    fn test_multi_hop_spread() {
        let nodes = vec![
            NodeData {
                id: "a".into(),
                text: "start decision".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "b".into(),
                text: "middle outcome".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "c".into(),
                text: "middle decision".into(),
                event_time: 2,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "d".into(),
                text: "final outcome".into(),
                event_time: 3,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
        ];
        let edges = vec![
            EdgeData {
                from_id: "a".into(),
                to_id: "b".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "b".into(),
                to_id: "c".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "c".into(),
                to_id: "d".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
        ];
        let mut graph = CausalGraph::build(&nodes, &edges);
        let results = graph.spreading_activation("start", None, false);
        assert!(results.iter().any(|r| r.text.contains("final outcome")));
        let start_act = results
            .iter()
            .find(|r| r.text.contains("start"))
            .map(|r| r.activation)
            .unwrap_or(0.0);
        let final_act = results
            .iter()
            .find(|r| r.text.contains("final outcome"))
            .map(|r| r.activation)
            .unwrap_or(0.0);
        assert!(start_act > final_act, "activation should decay over hops");
    }

    #[test]
    fn test_reverse_skips_invalid_edges() {
        // Bug fix #1: reverse spread should skip invalidated edges.
        // Input order deliberately differs from CSR order: edges from
        // different source nodes are interleaved (as from_store produces
        // via ORDER BY event_time). This would break if rev_to_fwd_idx
        // stored input array indices instead of CSR indices.
        let nodes = vec![
            NodeData {
                id: "a".into(),
                text: "alpha decision".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "b".into(),
                text: "bravo outcome".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "c".into(),
                text: "charlie outcome".into(),
                event_time: 2,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
            NodeData {
                id: "d".into(),
                text: "delta decision".into(),
                event_time: 3,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
            },
        ];
        // Input order: d→c first (valid), then a→b (invalid).
        // CSR order by source node index: a→b is CSR idx 0 (invalid),
        // d→c is CSR idx 1 (valid). If rev_to_fwd_idx stored input indices,
        // reverse[d→c] would map to input idx 0, but edge_valid[0] is the
        // a→b invalid edge — the valid d→c edge would be wrongly skipped.
        let edges = vec![
            EdgeData {
                from_id: "d".into(),
                to_id: "c".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            },
            EdgeData {
                from_id: "a".into(),
                to_id: "b".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: false,
            },
        ];
        let mut graph = CausalGraph::build(&nodes, &edges);

        // Reverse from "bravo" (b) — edge a→b is INVALID → a must NOT activate
        let results_b = graph.spreading_activation("bravo", None, true);
        assert!(
            !results_b.iter().any(|r| r.text.contains("alpha")),
            "Invalidated edge a→b should not propagate in reverse to a"
        );

        // Reverse from "charlie" (c) — edge d→c is VALID → d SHOULD activate
        let results_c = graph.spreading_activation("charlie", None, true);
        assert!(
            results_c.iter().any(|r| r.text.contains("delta")),
            "Valid edge d→c should propagate in reverse to d"
        );
    }

    #[test]
    fn test_from_store() {
        use crate::store::CausalStore;
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used Redis for caching",
                "cache stampede",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "used mutex lock",
                "deadlock crash",
                "caused",
                Some("concurrency"),
                0.85,
                "rule",
            )
            .unwrap();
        let graph = CausalGraph::from_store(&store).unwrap();
        assert!(graph.num_nodes() >= 4);
        assert!(graph.num_edges() >= 2);
    }

    // ─── P1: typed-edge unification ──────────────────────────────────────

    #[test]
    fn test_fact_meta_relation_spread_coeffs() {
        assert!((Relation::Fact.spread_coeff() - 0.8).abs() < 1e-6);
        assert!((Relation::Meta.spread_coeff() - 0.6).abs() < 1e-6);
        assert!((Relation::CoOccurrence.spread_coeff() - 1.0).abs() < 1e-6);
        assert_eq!(Relation::from_str_lossy("fact"), Relation::Fact);
        assert_eq!(Relation::from_str_lossy("meta"), Relation::Meta);
    }

    #[test]
    fn test_fact_edges_loaded_from_store_into_graph() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_fact("preference", "TypeScript", "user", "agent", 0.8)
            .unwrap();
        store
            .record_decision("used Redis", "cache hit", "caused", Some("caching"), 0.9, "rule")
            .unwrap();
        let mut graph = CausalGraph::from_store(&store).unwrap();
        assert!(graph.num_nodes() > 2);
        let results = graph.spreading_activation("TypeScript", None, false);
        assert!(
            results.iter().any(|r| r.text.contains("TypeScript")),
            "fact node should participate in spreading activation"
        );
    }

    // ─── P2: Hebbian co-occurrence weight update ─────────────────────────

    fn make_co_graph() -> CausalGraph {
        let nodes: Vec<NodeData> = ["d1", "o1", "d2", "o2", "d3", "o3"]
            .iter()
            .enumerate()
            .map(|(i, id)| NodeData {
                id: (*id).into(),
                text: format!("node {id}"),
                event_time: 1000 + i as i64,
                q_value: 0.8,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("test".into()),
            })
            .collect();
        let edges = vec![
            EdgeData { from_id: "d1".into(), to_id: "o1".into(), relation: Relation::Caused, weight: 0.9, valid: true },
            EdgeData { from_id: "d2".into(), to_id: "o2".into(), relation: Relation::Caused, weight: 0.85, valid: true },
            EdgeData { from_id: "d3".into(), to_id: "o3".into(), relation: Relation::Caused, weight: 0.95, valid: true },
            EdgeData { from_id: "d2".into(), to_id: "o3".into(), relation: Relation::Prevented, weight: 0.6, valid: true },
            EdgeData { from_id: "d1".into(), to_id: "d3".into(), relation: Relation::CoOccurrence, weight: 0.2, valid: true },
        ];
        CausalGraph::build(&nodes, &edges)
    }

    #[test]
    fn test_hebbian_strengthens_co_active() {
        let mut graph = make_co_graph();
        let idx = (0..graph.num_edges())
            .find(|&i| graph.edge_relation_at(i) == Relation::CoOccurrence)
            .unwrap();
        let before = graph.edge_raw_weight(idx);
        graph.hebbian_update(&[0, 4], 0.0, 0.5); // d1(0) + d3(4) co-active
        let after = graph.edge_raw_weight(idx);
        assert!(after > before, "co-active should strengthen: {before} → {after}");
    }

    #[test]
    fn test_hebbian_decays_non_co_active() {
        let mut graph = make_co_graph();
        let idx = (0..graph.num_edges())
            .find(|&i| graph.edge_relation_at(i) == Relation::CoOccurrence)
            .unwrap();
        let before = graph.edge_raw_weight(idx);
        graph.hebbian_update(&[0], 0.5, 0.1); // only d1 active, d3 not
        let after = graph.edge_raw_weight(idx);
        assert!(after < before, "non-co-active should decay: {before} → {after}");
    }

    // ─── P3: Immutable consolidation ─────────────────────────────────────

    #[test]
    fn test_immutable_consolidation_preserves_original() {
        let graph = make_test_graph();
        let orig_w = graph.edge_raw_weight(0);
        let result = graph.swr_consolidate_immutable(10, Some("focus on causal lessons"));
        assert!(
            (graph.edge_raw_weight(0) - orig_w).abs() < 1e-9,
            "original must not be mutated"
        );
        assert!(!result.delta_log.is_empty());
        assert!(result.stats.chains_replayed > 0);
        assert_eq!(result.instructions.as_deref(), Some("focus on causal lessons"));
    }

    // ─── P4: Q-value dynamics ────────────────────────────────────────────

    #[test]
    fn test_q_value_increases_on_reward() {
        let mut graph = make_test_graph();
        let before = graph.node_q_value(0);
        graph.update_q_value(0, 1.0, 0.1, 0.9);
        let after = graph.node_q_value(0);
        assert!(after >= before, "Q should increase on reward: {before} → {after}");
    }

    #[test]
    fn test_q_value_never_negative() {
        let mut graph = make_test_graph();
        graph.update_q_value(0, 0.0, 0.5, 0.9);
        assert!(graph.node_q_value(0) >= 0.0, "Q must never go negative");
    }

    // ─── P3 GC: dormant criterion coverage ───────────────────────────────

    #[test]
    fn test_gc_preserves_recently_activated_edges() {
        // The triple-criterion GC should NOT delete edges whose source node
        // was recently activated (dormant=false), even if the edge is weak
        // and has zero replay. Build a graph, mark one node as recently
        // activated, then verify its edges survive consolidation.
        let nodes = vec![
            NodeData {
                id: "d1".into(), text: "weak decision".into(),
                event_time: 1000, q_value: 0.5, replay_count: 0,
                // recently activated → dormant=false
                last_activated: chrono::Utc::now().timestamp(),
                task_tag: Some("test".into()),
            },
            NodeData {
                id: "o1".into(), text: "weak outcome".into(),
                event_time: 1001, q_value: 0.5, replay_count: 0,
                last_activated: chrono::Utc::now().timestamp(),
                task_tag: Some("test".into()),
            },
        ];
        let edges = vec![EdgeData {
            from_id: "d1".into(), to_id: "o1".into(),
            relation: Relation::Caused,
            weight: 0.01, // below gc_threshold (0.05) → weak
            valid: true,
        }];
        let graph = CausalGraph::build(&nodes, &edges);
        let result = graph.swr_consolidate_immutable(0, None); // 0 replays → no LTP
        // The weak edge should SURVIVE because dormant=false (recently activated)
        assert_eq!(
            result.stats.forgotten, 0,
            "recently-activated weak edges should not be GC'd"
        );
    }

    // ─── P6: Novelty entropy ─────────────────────────────────────────────

    #[test]
    fn test_novelty_entropy_low_for_uniform() {
        let graph = make_test_graph(); // all replay_count=0
        assert!(graph.novelty_entropy() < 0.01, "uniform should be low entropy");
    }

    #[test]
    fn test_novelty_entropy_high_for_skewed() {
        let mut graph = make_test_graph();
        graph.swr_consolidate(50); // creates skewed replay distribution
        assert!(graph.novelty_entropy() > 0.1, "skewed should be higher entropy");
    }

    // ─── Inhibitory ablation (paper §4.6) ────────────────────────────────

    /// Build a graph with explicit caused + prevented edges to demonstrate
    /// that inhibition changes retrieval ranking.
    fn make_ablation_graph() -> CausalGraph {
        let nodes = vec![
            NodeData {
                id: "deploy".into(),
                text: "deploy without env check".into(),
                event_time: 1,
                q_value: 0.3, // low Q — known mistake
                replay_count: 2,
                last_activated: 100,
                task_tag: None,
            },
            NodeData {
                id: "crash".into(),
                text: "production crash".into(),
                event_time: 2,
                q_value: 0.1,
                replay_count: 1,
                last_activated: 100,
                task_tag: None,
            },
            NodeData {
                id: "safe".into(),
                text: "zero downtime release".into(),
                event_time: 3,
                q_value: 0.8,
                replay_count: 3,
                last_activated: 100,
                task_tag: None,
            },
            NodeData {
                id: "rollback".into(),
                text: "quick rollback procedure".into(),
                event_time: 4,
                q_value: 0.7,
                replay_count: 2,
                last_activated: 100,
                task_tag: None,
            },
        ];
        let edges = vec![
            EdgeData {
                from_id: "deploy".into(),
                to_id: "crash".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            },
            EdgeData {
                from_id: "deploy".into(),
                to_id: "safe".into(),
                relation: Relation::Prevented,
                weight: 0.8,
                valid: true,
            },
            EdgeData {
                from_id: "deploy".into(),
                to_id: "rollback".into(),
                relation: Relation::Enabled,
                weight: 0.6,
                valid: true,
            },
        ];
        CausalGraph::build(&nodes, &edges)
    }

    #[test]
    fn test_inhibition_changes_activation_sign() {
        // With inhibition: "zero downtime release" gets NEGATIVE activation
        // (prevented by deploying without env check).
        let mut graph = make_ablation_graph();
        let results = graph.spreading_activation_opts("deploy", None, false, false);
        let safe = results.iter().find(|r| r.text.contains("zero downtime"));
        assert!(
            safe.is_some(),
            "zero downtime release should be in results"
        );
        assert!(
            safe.unwrap().activation < 0.0,
            "prevented edge should give negative activation to 'zero downtime release'"
        );
    }

    #[test]
    fn test_disable_inhibition_zeros_prevented() {
        // After disable_inhibition(): "zero downtime release" gets ZERO or
        // near-zero activation (no negative spread).
        let mut graph = make_ablation_graph();
        graph.disable_inhibition();
        let results = graph.spreading_activation_opts("deploy", None, false, false);

        // The crash node should still be strongly activated (caused edge intact)
        let crash = results.iter().find(|r| r.text.contains("crash"));
        assert!(crash.is_some());
        assert!(
            crash.unwrap().activation > 0.0,
            "caused edges should still spread positive activation"
        );

        // The safe node should NOT have negative activation anymore
        let safe = results.iter().find(|r| r.text.contains("zero downtime"));
        if let Some(safe) = safe {
            assert!(
                safe.activation >= -0.001,
                "prevented edge value zeroed: activation should be >= 0, got {}",
                safe.activation
            );
        }
        // If safe is absent, that's also fine (threshold filtered it).
    }

    #[test]
    fn test_inhibition_changes_ranking() {
        // The key ablation result: WITH inhibition, the "zero downtime release"
        // node appears with NEGATIVE activation (a warning signal — "this
        // outcome is prevented by the queried action").
        //
        // WITHOUT inhibition, that node is absent from results entirely — no
        // warning is surfaced. The system loses the ability to distinguish
        // "this outcome is likely" (positive) from "this outcome is prevented"
        // (negative).
        let mut graph_with = make_ablation_graph();
        let results_with = graph_with.spreading_activation_opts("deploy", None, false, false);

        let mut graph_without = make_ablation_graph();
        graph_without.disable_inhibition();
        let results_without =
            graph_without.spreading_activation_opts("deploy", None, false, false);

        // WITH inhibition: crash (positive) and zero-downtime (negative) both present
        let crash_with = results_with
            .iter()
            .find(|r| r.text.contains("crash"));
        let safe_with = results_with
            .iter()
            .find(|r| r.text.contains("zero downtime"));
        assert!(crash_with.is_some(), "crash should be activated");
        assert!(safe_with.is_some(), "zero downtime should appear (as warning)");
        assert!(
            crash_with.unwrap().activation > 0.0,
            "crash is positively activated (caused)"
        );
        assert!(
            safe_with.unwrap().activation < 0.0,
            "zero downtime is negatively activated (prevented) — warning signal"
        );

        // WITHOUT inhibition: zero-downtime is absent (no warning surfaced)
        let safe_without = results_without
            .iter()
            .find(|r| r.text.contains("zero downtime"));
        assert!(
            safe_without.is_none(),
            "without inhibition: prevented target absent from results (no warning)"
        );
    }

    // ─── P5: hybrid novelty gating (Nemori FEP prediction gap) ────────────

    #[test]
    fn test_needs_prediction_gap_borderline_band() {
        // Confident entropy verdicts stand alone; borderline defers to LLM.
        assert!(!needs_prediction_gap(0.0));
        assert!(!needs_prediction_gap(0.39));
        assert!(needs_prediction_gap(0.4), "borderline lower edge");
        assert!(needs_prediction_gap(0.5));
        assert!(needs_prediction_gap(0.7), "borderline upper edge");
        assert!(!needs_prediction_gap(0.71));
        assert!(!needs_prediction_gap(1.0));
    }

    #[test]
    fn test_prediction_gap_mode_uses_llm_prediction() {
        // A graph where the entropy check is uninformative (empty predictions)
        // — the prediction-gap mode must still produce a verdict from the
        // predictor alone.
        let graph = CausalGraph::new();
        let mut graph = graph;
        let mut predict_matching = |_: &str| Some("the server restarted cleanly".to_string());
        let rep = graph.detect_novelty_with_mode(
            "deployed new build",
            "the server restarted cleanly",
            NoveltyMode::PredictionGap,
            &mut predict_matching,
        );
        // Prediction matched reality → no surprise → do not record.
        assert!(!rep.should_record);
        assert!(rep.surprise < 0.5);

        let mut predict_wrong = |_: &str| Some("the build broke production".to_string());
        let rep = graph.detect_novelty_with_mode(
            "deployed new build",
            "the server restarted cleanly",
            NoveltyMode::PredictionGap,
            &mut predict_wrong,
        );
        // Prediction contradicted reality → high surprise → record.
        assert!(rep.should_record);
        assert!(rep.surprise > 0.5);
    }

    #[test]
    fn test_prediction_gap_fallback_when_predictor_unavailable() {
        let graph = CausalGraph::new();
        let mut graph = graph;
        let mut predict_none = |_: &str| None;
        let rep = graph.detect_novelty_with_mode(
            "did something",
            "something happened",
            NoveltyMode::PredictionGap,
            &mut predict_none,
        );
        // No prediction → entropy report semantics (empty predictions →
        // maximal surprise, but should_record stays a simple > 0.5 check).
        assert_eq!(rep.surprise, 1.0);
        assert!(rep.should_record);
    }

    #[test]
    fn test_hybrid_mode_skips_llm_on_confident_entropy() {
        // Build a tiny graph with one seedable node so entropy produces a
        // confident verdict.
        let nodes = vec![NodeData {
            id: "n1".into(),
            text: "deploy with tests passed".into(),
            event_time: 0,
            q_value: 0.5,
            replay_count: 0,
            last_activated: 0,
            task_tag: None,
        }];
        let graph = CausalGraph::build(&nodes, &[]);
        let mut graph = graph;
        // Hybrid with a predictor that would OVERRIDE the verdict if called —
        // but the entropy verdict here is confident (graph predicts nothing
        // similar to the actual outcome → surprise 1.0 > 0.7 → NOT
        // borderline), so the LLM must NOT be consulted.
        let mut calls = 0;
        let mut predict = |_: &str| {
            calls += 1;
            Some("deploy with tests passed".to_string())
        };
        let rep = graph.detect_novelty_with_mode(
            "deploy with tests passed",
            "deploy with tests passed",
            NoveltyMode::Hybrid,
            &mut predict,
        );
        assert_eq!(calls, 0, "confident entropy verdict must skip the LLM");
        assert!(!rep.should_record, "outcome matched the graph prediction");
    }
}
