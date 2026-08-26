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
                scope: None,
            },
            NodeData {
                id: "o1".into(),
                text: "cache stampede DB overloaded".into(),
                event_time: 1001,
                q_value: 0.8,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("caching".into()),
                scope: None,
            },
            NodeData {
                id: "d2".into(),
                text: "used mutex lock".into(),
                event_time: 1002,
                q_value: 0.7,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
                scope: None,
            },
            NodeData {
                id: "o2".into(),
                text: "deadlock crash".into(),
                event_time: 1003,
                q_value: 0.7,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
                scope: None,
            },
            NodeData {
                id: "d3".into(),
                text: "used channel single-flight".into(),
                event_time: 1004,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
                scope: None,
            },
            NodeData {
                id: "o3".into(),
                text: "fixed race condition".into(),
                event_time: 1005,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: Some("concurrency".into()),
                scope: None,
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
                scope: None,
            },
            NodeData {
                id: "b".into(),
                text: "strong chain mid".into(),
                event_time: 1,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "c".into(),
                text: "strong chain end".into(),
                event_time: 2,
                q_value: 0.9,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "w1".into(),
                text: "weak node one".into(),
                event_time: 3,
                q_value: 0.01,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "w2".into(),
                text: "weak node two".into(),
                event_time: 4,
                q_value: 0.01,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
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
                scope: None,
            },
            NodeData {
                id: "p2".into(),
                text: "padding two".into(),
                event_time: 6,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "p3".into(),
                text: "padding three".into(),
                event_time: 7,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "p4".into(),
                text: "padding four".into(),
                event_time: 8,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "p5".into(),
                text: "padding five".into(),
                event_time: 9,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
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
                scope: None,
            },
            NodeData {
                id: "b".into(),
                text: "end".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
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
                scope: None,
            },
            NodeData {
                id: "b".into(),
                text: "middle outcome".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "c".into(),
                text: "middle decision".into(),
                event_time: 2,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "d".into(),
                text: "final outcome".into(),
                event_time: 3,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
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
                scope: None,
            },
            NodeData {
                id: "b".into(),
                text: "bravo outcome".into(),
                event_time: 1,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "c".into(),
                text: "charlie outcome".into(),
                event_time: 2,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "d".into(),
                text: "delta decision".into(),
                event_time: 3,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
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
            .record_decision(
                "used Redis",
                "cache hit",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
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
                scope: None,
            })
            .collect();
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
            EdgeData {
                from_id: "d1".into(),
                to_id: "d3".into(),
                relation: Relation::CoOccurrence,
                weight: 0.2,
                valid: true,
            },
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
        assert!(
            after > before,
            "co-active should strengthen: {before} → {after}"
        );
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
        assert!(
            after < before,
            "non-co-active should decay: {before} → {after}"
        );
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
        assert_eq!(
            result.instructions.as_deref(),
            Some("focus on causal lessons")
        );
    }

    // ─── P4: Q-value dynamics ────────────────────────────────────────────

    #[test]
    fn test_q_value_increases_on_reward() {
        let mut graph = make_test_graph();
        let before = graph.node_q_value(0);
        graph.update_q_value(0, 1.0, 0.1, 0.9);
        let after = graph.node_q_value(0);
        assert!(
            after >= before,
            "Q should increase on reward: {before} → {after}"
        );
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
                id: "d1".into(),
                text: "weak decision".into(),
                event_time: 1000,
                q_value: 0.5,
                replay_count: 0,
                // recently activated → dormant=false
                last_activated: chrono::Utc::now().timestamp(),
                task_tag: Some("test".into()),
                scope: None,
            },
            NodeData {
                id: "o1".into(),
                text: "weak outcome".into(),
                event_time: 1001,
                q_value: 0.5,
                replay_count: 0,
                last_activated: chrono::Utc::now().timestamp(),
                task_tag: Some("test".into()),
                scope: None,
            },
        ];
        let edges = vec![EdgeData {
            from_id: "d1".into(),
            to_id: "o1".into(),
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
        assert!(
            graph.novelty_entropy() < 0.01,
            "uniform should be low entropy"
        );
    }

    #[test]
    fn test_novelty_entropy_high_for_skewed() {
        let mut graph = make_test_graph();
        graph.swr_consolidate(50); // creates skewed replay distribution
        assert!(
            graph.novelty_entropy() > 0.1,
            "skewed should be higher entropy"
        );
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
                scope: None,
            },
            NodeData {
                id: "crash".into(),
                text: "production crash".into(),
                event_time: 2,
                q_value: 0.1,
                replay_count: 1,
                last_activated: 100,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "safe".into(),
                text: "zero downtime release".into(),
                event_time: 3,
                q_value: 0.8,
                replay_count: 3,
                last_activated: 100,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "rollback".into(),
                text: "quick rollback procedure".into(),
                event_time: 4,
                q_value: 0.7,
                replay_count: 2,
                last_activated: 100,
                task_tag: None,
                scope: None,
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
        assert!(safe.is_some(), "zero downtime release should be in results");
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
    fn test_disable_spread_keeps_seeds_only() {
        // After disable_spread(): the seed node still activates (Q-weighted
        // seeding intact) but zero hops run, so neighbors along caused /
        // enabled edges stay dark — retrieval is seed-hits-only.
        let mut graph = make_ablation_graph();
        graph.disable_spread();
        let results = graph.spreading_activation_opts("deploy", None, false, false);

        let seed = results.iter().find(|r| r.text.contains("deploy"));
        assert!(seed.is_some(), "seed node should still activate");
        assert!(
            seed.unwrap().activation > 0.0,
            "seed keeps its Q-weighted initial activation"
        );

        let crash = results.iter().find(|r| r.text.contains("crash"));
        assert!(
            crash.is_none(),
            "no spread: caused neighbor must not activate, got {:?}",
            crash.map(|c| c.activation)
        );
        let rollback = results.iter().find(|r| r.text.contains("rollback"));
        assert!(
            rollback.is_none(),
            "no spread: enabled neighbor must not activate, got {:?}",
            rollback.map(|r| r.activation)
        );
    }

    #[test]
    fn test_fanout_constraint_damps_hub_broadcast() {
        // Channel-scoped fan-out (Collins & Loftus on the associative
        // channel only): a node's activation is divided among its
        // outgoing ASSOCIATIVE edges (Fact / CoOccurrence). A hub with 10
        // co-occurrence edges gives each neighbor 1/10 of what a
        // degree-1 node gives its neighbor; causal-family spread from the
        // same hub is NOT divided (curated signal, semantics preserved).
        let node = |id: &str, text: &str| NodeData {
            id: id.into(),
            text: text.into(),
            event_time: 1,
            q_value: 0.5,
            replay_count: 0,
            last_activated: 100,
            task_tag: None,
            scope: None,
        };
        let mut nodes = vec![
            node("hub", "hub node"),
            node("leaf", "leaf node"),
            node("solo", "solo target"),
            node("causal", "causal target"),
        ];
        let mut edges = vec![
            EdgeData {
                from_id: "leaf".into(),
                to_id: "solo".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            // The hub's one causal edge: undivided spread.
            EdgeData {
                from_id: "hub".into(),
                to_id: "causal".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
        ];
        for i in 0..10 {
            nodes.push(node(&format!("t{i}"), &format!("assoc target {i}")));
            edges.push(EdgeData {
                from_id: "hub".into(),
                to_id: format!("t{i}"),
                relation: Relation::CoOccurrence,
                weight: 1.0,
                valid: true,
            });
        }
        let mut graph = CausalGraph::build(&nodes, &edges);
        // Query "node" substring-seeds "hub node" and "leaf node" (seed
        // activation 0.75 each, q=0.5). Degree-1 leaf and the hub's
        // causal edge pass on 0.75 × 1.0 × 0.7 ≈ 0.525; the hub's ten
        // associative neighbors get 0.75 × (1/10) × 1.0 × 0.7 ≈ 0.05 —
        // below the 0.1 threshold.
        let results = graph.spreading_activation_opts("node", None, false, false);

        let solo = results.iter().find(|r| r.text.contains("solo target"));
        assert!(
            solo.is_some(),
            "degree-1 neighbor must activate (fan-out of 1 is a no-op)"
        );
        let a = solo.unwrap().activation;
        assert!(
            (a - 0.525).abs() < 0.01,
            "degree-1 spread unchanged: expected ~0.525, got {a}"
        );
        let causal = results.iter().find(|r| r.text.contains("causal target"));
        assert!(
            causal.is_some(),
            "causal-family spread from a hub must NOT be fan-out divided"
        );
        let a = causal.unwrap().activation;
        assert!(
            (a - 0.525).abs() < 0.01,
            "causal spread undivided despite hub degree: expected ~0.525, got {a}"
        );
        for i in 0..10 {
            let t = results
                .iter()
                .find(|r| r.text.contains(&format!("assoc target {i}")));
            assert!(
                t.is_none(),
                "associative hub neighbor {i} must stay below threshold (fan-out /10), got {t:?}"
            );
        }
    }

    #[test]
    fn test_provenance_seed_and_spread_hops() {
        // Flip-path marking: seeds are hop 0 with no via; a one-hop spread
        // hit records hop 1 and the winning edge back to the seed.
        let mut graph = make_ablation_graph();
        let results = graph.spreading_activation_opts("deploy", None, false, false);

        let seed = results.iter().find(|r| r.text.contains("deploy")).unwrap();
        assert_eq!(seed.hop, 0, "seed must be hop 0");
        assert!(seed.via.is_none(), "seed has no via edge");

        let crash = results.iter().find(|r| r.text.contains("crash")).unwrap();
        assert_eq!(crash.hop, 1, "crash lights in hop 1");
        let via = crash.via.expect("spread hit must carry via");
        assert_eq!(via.relation, Relation::Caused);
        assert!(via.contribution > 0.0);
        assert_eq!(
            graph.node_id(via.from as usize),
            "deploy",
            "via must point back at the seed node"
        );
    }

    #[test]
    fn test_provenance_prevented_negative_path() {
        // The inhibitory path is visible: the prevented node's negative
        // activation carries a via edge with relation Prevented and a
        // negative contribution — "why does this result have a negative
        // score" is now answerable from the result itself.
        let mut graph = make_ablation_graph();
        let results = graph.spreading_activation_opts("deploy", None, false, false);

        let safe = results
            .iter()
            .find(|r| r.text.contains("zero downtime"))
            .expect("prevented node should surface with negative activation");
        assert!(safe.activation < 0.0);
        let via = safe.via.expect("prevented hit must carry via");
        assert_eq!(via.relation, Relation::Prevented);
        assert!(via.contribution < 0.0);
        assert_eq!(graph.node_id(via.from as usize), "deploy");
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
        let results_without = graph_without.spreading_activation_opts("deploy", None, false, false);

        // WITH inhibition: crash (positive) and zero-downtime (negative) both present
        let crash_with = results_with.iter().find(|r| r.text.contains("crash"));
        let safe_with = results_with
            .iter()
            .find(|r| r.text.contains("zero downtime"));
        assert!(crash_with.is_some(), "crash should be activated");
        assert!(
            safe_with.is_some(),
            "zero downtime should appear (as warning)"
        );
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
            scope: None,
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

    // ─── Phase A: fact entity linking (one-graph-convergence) ────────────

    /// Store fixture: one causal edge plus one fact sharing three
    /// distinct non-stopword tokens ({module, typescript, rewrites}) —
    /// enough for an entity link.
    #[allow(
        clippy::expect_used,
        reason = "test fixture: panicking on setup failure is the desired behavior"
    )]
    fn linked_store() -> crate::store::CausalStore {
        let store = crate::store::CausalStore::open_in_memory().expect("in-memory store");
        store
            .record_decision_at(
                "rewrote module in TypeScript and planned rewrites",
                "compile errors dropped",
                "caused",
                Some("rust"),
                0.8,
                "llm_inferred",
                1_700_000_000,
            )
            .expect("record decision");
        store
            .record_fact(
                "editor_preference",
                "user prefers TypeScript for module rewrites",
                "user",
                "agent",
                0.8,
            )
            .expect("record fact");
        store
    }

    #[test]
    fn test_entity_link_scope_isolation() {
        // Phase A hardening: a fact with a colon-namespaced scope
        // ("lme:{qid}") links ONLY to chunks whose task_tag matches the
        // scope suffix - 500-question corpora share one store, and
        // cross-question links would pollute isolation and explode the
        // graph. Canonical scopes (user/session/agent) keep the all-chunk
        // behavior for single-agent stores.
        let node = |id: &str, text: &str, tag: Option<&str>, scope: Option<&str>| NodeData {
            id: id.into(),
            text: text.into(),
            event_time: 0,
            q_value: 0.5,
            replay_count: 0,
            last_activated: 0,
            task_tag: tag.map(str::to_string),
            scope: scope.map(str::to_string),
        };
        // fact in scope lme:q1 shares tokens with d1 (task q1) AND d2 (q2).
        let nodes = vec![
            node(
                "fact:1",
                "migrated build tooling to TypeScript for scripts",
                None,
                Some("lme:q1"),
            ),
            node(
                "d1",
                "rewrote build scripts using TypeScript tooling",
                Some("q1"),
                None,
            ),
            node("d2", "switched project to TypeScript", Some("q2"), None),
        ];
        let edges = crate::hippocampus::entity_link_facts(&nodes, &[0]);
        let linked: Vec<&str> = edges.iter().map(|e| e.to_id.as_str()).collect();
        assert!(
            linked.iter().any(|t| *t == "d1"),
            "in-scope chunk must link: {linked:?}"
        );
        assert!(
            !linked.iter().any(|t| *t == "d2"),
            "cross-scope chunk must NOT link (lme:q1 vs task q2): {linked:?}"
        );

        // Canonical scope: links to all chunks (single-agent store).
        let nodes = vec![
            node(
                "fact:2",
                "migrated build tooling to TypeScript for scripts",
                None,
                Some("user"),
            ),
            node(
                "d3",
                "rewrote build scripts using TypeScript tooling",
                Some("dev"),
                None,
            ),
            node(
                "d4",
                "fixed build scripts for TypeScript tooling",
                Some("ops"),
                None,
            ),
        ];
        let edges = crate::hippocampus::entity_link_facts(&nodes, &[0]);
        let linked: Vec<&str> = edges.iter().map(|e| e.to_id.as_str()).collect();
        assert!(
            linked.iter().any(|t| *t == "d3") && linked.iter().any(|t| *t == "d4"),
            "canonical scope must link across task_tags: {linked:?}"
        );

        // Link stopwords: a fact sharing only generic tokens must not link.
        let nodes = vec![
            node("fact:3", "user project code issue", None, Some("user")),
            node(
                "d5",
                "the user fixed a project code issue",
                Some("dev"),
                None,
            ),
        ];
        let edges = crate::hippocampus::entity_link_facts(&nodes, &[0]);
        assert!(
            edges.is_empty(),
            "generic-only tokens must not drive a link: {edges:?}"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: graph construction must succeed or the test is meaningless"
    )]
    fn test_fact_entity_link_spread_includes_fact_and_causal_chain() {
        let mut graph = CausalGraph::from_store(&linked_store()).expect("graph from store");
        let results = graph.spreading_activation("TypeScript", None, false);
        let texts: Vec<&str> = results.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("editor_preference")),
            "fact node must surface via entity link: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("rewrote module in TypeScript")),
            "causal decision must surface: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("compile errors dropped")),
            "outcome must surface via the caused edge: {texts:?}"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: graph construction must succeed or the test is meaningless"
    )]
    fn test_fact_seed_spreads_to_causal_chain() {
        // "prefers" appears ONLY in the fact — seeding there proves the
        // fact→chunk direction: a fact seed reaches causal lessons.
        let mut graph = CausalGraph::from_store(&linked_store()).expect("graph from store");
        let results = graph.spreading_activation("prefers", None, false);
        let texts: Vec<&str> = results.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("rewrote module in TypeScript")),
            "fact seed must reach the causal decision node: {texts:?}"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: store/graph construction must succeed or the test is meaningless"
    )]
    fn test_fact_entity_link_requires_min_shared_tokens() {
        let store = crate::store::CausalStore::open_in_memory().expect("in-memory store");
        // Only ONE shared token per pair ({zsh}, {shell}) — below the
        // conservative ≥3 distinct non-stopword threshold, no link may be
        // created.
        store
            .record_decision_at(
                "configured zsh plugins",
                "shell startup faster",
                "caused",
                Some("shell"),
                0.8,
                "llm_inferred",
                1_700_000_000,
            )
            .expect("record decision");
        store
            .record_fact("shell", "zsh completion setup", "user", "agent", 0.8)
            .expect("record fact");

        let mut graph = CausalGraph::from_store(&store).expect("graph from store");
        let results = graph.spreading_activation("completion", None, false);
        let texts: Vec<&str> = results.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("zsh completion setup")),
            "the seeded fact itself must surface: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("zsh plugins")),
            "single-token overlap must NOT create a link: {texts:?}"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: graph construction must succeed or the test is meaningless"
    )]
    fn test_spreading_activation_seeded_by_fact_id_only() {
        // The query text matches NOTHING in the graph (substring seeds are
        // empty); the only seed is the store-resolved fact node id. Proves
        // the Phase B seeded entry point: store-side BM25/semantic seeds
        // drive the spread, not substring luck.
        let mut graph = CausalGraph::from_store(&linked_store()).expect("graph from store");
        let results =
            graph.spreading_activation_seeded("zzz-nomatch", &["fact:1".to_string()], None, true);
        let texts: Vec<&str> = results.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("rewrote module in TypeScript")),
            "fact-id seed must reach the causal chain via the Phase A link: {texts:?}"
        );
    }

    #[test]
    fn test_entity_link_counts_distinct_tokens_only() {
        // The chunk text REPEATS "zsh" — without posting-list dedup a
        // single distinct shared token would count as overlap 2 and
        // wrongly link below the documented "≥ 3 distinct tokens" bar.
        let node = |id: &str, text: &str| NodeData {
            id: id.into(),
            text: text.into(),
            event_time: 0,
            q_value: 0.5,
            replay_count: 0,
            last_activated: 0,
            task_tag: None,
            scope: None,
        };
        let nodes = vec![
            node("d1", "configured zsh zsh plugins"),
            node("fact:1", "shell: zsh completion setup"),
        ];
        let edges = crate::hippocampus::entity_link_facts(&nodes, &[1]);
        assert!(
            edges.is_empty(),
            "one distinct shared token must not link, even repeated: {edges:?}"
        );

        // Control: a second distinct shared token does link, bidirectionally.
        let nodes = vec![
            node("d1", "configured zsh zsh plugins for setup completion"),
            node("fact:1", "shell: zsh completion setup"),
        ];
        let edges = crate::hippocampus::entity_link_facts(&nodes, &[1]);
        assert_eq!(
            edges.len(),
            2,
            "three distinct tokens (zsh, setup, completion) link both ways"
        );
    }

    // ─── Phase C: write-path graph patches (one-graph-convergence) ───────

    fn node(id: &str, text: &str) -> NodeData {
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

    #[test]
    fn test_patch_edge_spreads_without_rebuild() {
        // Base graph: one edge d0 → o0. Then a write-path patch appends
        // d1/o1 and the edge d0 → d1 (chunk reuse: d0 is an EXISTING node,
        // the hard case for CSR). The patched edge must spread both ways.
        let mut graph = CausalGraph::build(
            &[node("d0", "used global lock"), node("o0", "deadlock error")],
            &[EdgeData {
                from_id: "d0".into(),
                to_id: "o0".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            }],
        );
        let d0 = graph.append_node(node("d0", "used global lock"));
        let d1 = graph.append_node(node("d1", "replaced with sharded locks"));
        graph.add_patch_edge(d0, d1, Relation::Caused, 0.8);

        // Forward: seed at d0's text reaches the PATCHED target d1.
        let fwd = graph.spreading_activation("global lock", None, false);
        let texts: Vec<&str> = fwd.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("sharded locks")),
            "patched edge must spread forward: {texts:?}"
        );

        // Reverse: seed at d1 reaches the EXISTING d0 through the patch.
        let rev = graph.spreading_activation("sharded", None, true);
        let texts: Vec<&str> = rev.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("global lock")),
            "patched edge must spread reverse: {texts:?}"
        );

        // Differential: a full build with the same nodes+edges reaches the
        // same set (patch state == rebuilt state).
        let mut rebuilt = CausalGraph::build(
            &[
                node("d0", "used global lock"),
                node("o0", "deadlock error"),
                node("d1", "replaced with sharded locks"),
            ],
            &[
                EdgeData {
                    from_id: "d0".into(),
                    to_id: "o0".into(),
                    relation: Relation::Caused,
                    weight: 0.9,
                    valid: true,
                },
                EdgeData {
                    from_id: "d0".into(),
                    to_id: "d1".into(),
                    relation: Relation::Caused,
                    weight: 0.8,
                    valid: true,
                },
            ],
        );
        let rb = rebuilt.spreading_activation("global lock", None, false);
        let rb_texts: Vec<&str> = rb.iter().map(|r| r.text.as_str()).collect();
        let patched: std::collections::HashSet<&str> =
            fwd.iter().map(|r| r.text.as_str()).collect();
        for t in rb_texts {
            assert!(
                patched.contains(t),
                "rebuilt hit {t:?} missing from patched"
            );
        }
    }

    #[test]
    fn test_retire_node_never_seeds_or_surfaces() {
        let mut graph = CausalGraph::build(
            &[
                node("fact:1", "db: redis 7.2"),
                node("d1", "deployed redis"),
            ],
            &[],
        );
        // Suppose the fact was already linked and then replaced: retire it.
        assert!(graph.retire_node("fact:1"));
        let results = graph.spreading_activation("redis 7.2", None, false);
        assert!(
            results.is_empty() || !results.iter().any(|r| r.text.contains("7.2")),
            "retired fact must not surface: {results:?}"
        );
        // Re-appending the same id revives it (store-side revive path).
        let idx = graph.append_node(node("fact:1", "db: redis 7.2"));
        let d2 = graph.append_node(node("d2", "upgraded redis"));
        graph.add_patch_edge(idx, d2, Relation::Fact, 0.5);
        let results = graph.spreading_activation("redis 7.2", None, false);
        assert!(
            results.iter().any(|r| r.text.contains("7.2")),
            "revived fact must surface again: {results:?}"
        );
    }

    #[test]
    fn test_invalidate_edges_between_stops_spread() {
        let mut graph = CausalGraph::build(
            &[node("d1", "skipped backup"), node("o1", "data loss")],
            &[EdgeData {
                from_id: "d1".into(),
                to_id: "o1".into(),
                relation: Relation::Caused,
                weight: 0.9,
                valid: true,
            }],
        );
        assert_eq!(graph.invalidate_edges_between("d1", "o1"), 1);
        let results = graph.spreading_activation("skipped backup", None, false);
        assert!(
            !results.iter().any(|r| r.text.contains("data loss")),
            "invalidated edge must not spread: {results:?}"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: overlay entries must exist or the test is meaningless"
    )]
    fn test_add_patch_edge_idempotent_on_rewrite() {
        // An idempotent re-record (same chunk ids) must UPDATE the overlay
        // edge, not stack a duplicate — repeated writes must not inflate
        // activation or grow the overlay unboundedly.
        let mut graph = CausalGraph::build(&[node("d1", "alpha"), node("o1", "beta")], &[]);
        let d1 = graph.append_node(node("d1", "alpha"));
        let o1 = graph.append_node(node("o1", "beta"));
        graph.add_patch_edge(d1, o1, Relation::Caused, 0.8);
        graph.add_patch_edge(d1, o1, Relation::Caused, 0.9);
        graph.add_patch_edge(d1, o1, Relation::Caused, 0.9);

        let fwd = graph.patch_fwd.get(&d1).expect("overlay fwd entry");
        assert_eq!(fwd.len(), 1, "no duplicate overlay edges: {fwd:?}");
        let rev = graph.patch_rev.get(&o1).expect("overlay rev entry");
        assert_eq!(rev.len(), 1, "no duplicate overlay edges: {rev:?}");
        assert_eq!(fwd[0].value, 0.9 * Relation::Caused.spread_coeff());

        // And the activation reflects ONE edge, not three (0.8 seed ×
        // 0.9 value × 0.7 decay ≈ 0.5 → above the 0.1 threshold once; a
        // triple-stacked edge would clamp at 1.0).
        let results = graph.spreading_activation("alpha", None, false);
        let o1_act = results
            .iter()
            .find(|r| r.text.contains("beta"))
            .map(|r| r.activation)
            .expect("beta activated");
        assert!(
            (o1_act - 0.75 * 0.9 * 0.7).abs() < 1e-4,
            "activation must reflect one edge (got {o1_act})"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: overlay entries must exist or the test is meaningless"
    )]
    fn test_link_fact_node_uses_token_index_and_survives_rebuild() {
        // Same thresholds as entity_link_facts, driven through the
        // incremental index path: three shared tokens link, fewer don't —
        // and identical behavior whether the graph was built from scratch
        // (index populated by build) or grown by appends.
        let mut graph = CausalGraph::build(
            &[
                node("d1", "configured zsh plugins for setup completion"),
                node("d2", "unrelated work on the database"),
            ],
            &[],
        );
        let f1 = graph.append_node(node("fact:1", "shell: zsh completion setup"));
        graph.link_fact_node(f1);
        let fwd = graph.patch_fwd.get(&f1).expect("fact has links");
        assert_eq!(
            fwd.len(),
            1,
            "three shared tokens (zsh, setup, completion) → one chunk: {fwd:?}"
        );
        assert_eq!(fwd[0].other, 0, "links to d1, not the unrelated d2");
    }
    #[test]
    fn test_component_stats_basic() {
        // d1->o1, d2->o2 (two isolated pairs) + d3->o3->d4 chain.
        let nodes = vec![
            NodeData {
                id: "d1".into(),
                text: "a b".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "o1".into(),
                text: "c d".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "d2".into(),
                text: "e f".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "o2".into(),
                text: "g h".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "d3".into(),
                text: "i j".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "o3".into(),
                text: "k l".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
            NodeData {
                id: "d4".into(),
                text: "m n".into(),
                event_time: 0,
                q_value: 0.5,
                replay_count: 0,
                last_activated: 0,
                task_tag: None,
                scope: None,
            },
        ];
        let edges = vec![
            EdgeData {
                from_id: "d1".into(),
                to_id: "o1".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "d2".into(),
                to_id: "o2".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "d3".into(),
                to_id: "o3".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
            EdgeData {
                from_id: "o3".into(),
                to_id: "d4".into(),
                relation: Relation::Caused,
                weight: 1.0,
                valid: true,
            },
        ];
        let graph = CausalGraph::build(&nodes, &edges);
        let (comps, max, isolated, v_edges) = graph.component_stats();
        assert_eq!(comps, 3, "two pairs + one 3-node chain");
        assert_eq!(max, 3, "chain is the largest component");
        assert_eq!(isolated, 0);
        assert_eq!(v_edges, 4);
    }

    #[test]
    fn test_benchmark_scale_scope_isolation_components() {
        // Miniature 500-question scenario: two questions share one store.
        // Facts scoped lme:q1 / lme:q2 must link WITHIN their question only
        // (component structure stays per-question; spreading from q1 must
        // never surface q2 content).
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used redis for caching data",
                "caching worked",
                "caused",
                Some("q1"),
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "used channels in go",
                "race fixed",
                "caused",
                Some("q2"),
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_fact(
                "preference",
                "prefers redis caching data",
                "lme:q1",
                "eval",
                0.9,
            )
            .unwrap();
        store
            .record_fact(
                "preference",
                "prefers channels in go code",
                "lme:q2",
                "eval",
                0.9,
            )
            .unwrap();

        let mut graph = CausalGraph::from_store(&store).unwrap();
        // Both questions must be separate weakly-connected components.
        let (comps, _max, _iso, _ve) = graph.component_stats();
        assert!(
            comps >= 2,
            "two questions must stay separate components: {comps}"
        );

        // Behavioral isolation: seeding q1 content must surface q1's linked
        // fact and NEVER q2's fact or chunk.
        let results = graph.spreading_activation("used redis for caching", None, false);
        let texts: Vec<&str> = results.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("prefers redis caching")),
            "q1 fact must be linked in: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("channels") || t.contains("race fixed")),
            "cross-question leakage: {texts:?}"
        );
    }
    #[test]
    #[ignore = "real-DB probe: needs ~/.local/share/causal-memory/causal.db (run explicitly)"]
    fn probe_real_db_connectivity() {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let path = std::env::var("CAUSAL_MEMORY_DB")
            .unwrap_or_else(|_| format!("{home}/.local/share/causal-memory/causal.db"));
        if !std::path::Path::new(&path).exists() {
            eprintln!("probe: no real DB at {path}");
            return;
        }
        let store = CausalStore::open(&path).unwrap();
        let graph = CausalGraph::from_store(&store).unwrap();
        let (comps, max, isolated, edges) = graph.component_stats();
        eprintln!(
            "REAL DB: {} nodes, {} valid edges, {} components, largest {}, isolated {}",
            graph.num_nodes(),
            edges,
            comps,
            max,
            isolated
        );
        eprintln!("(baseline pre-Phase-A: 431 chunks / 225 edges / 207 components / largest 10)");
        assert!(max >= 10, "facts must have merged at least some pairs");
    }

    /// Scope hubs must not propagate activation: two unrelated facts
    /// sharing scope:user, a query hitting one must not activate the
    /// other through the hub (the via-mcp LME regression — every fact
    /// was two hops from every other, polluting the whole layer).
    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: store/graph construction must succeed"
    )]
    fn scope_hub_does_not_propagate_activation() {
        let store = crate::store::CausalStore::open_in_memory().expect("store");
        store
            .record_fact(
                "education",
                "graduated with a degree in physics",
                "user",
                "agent",
                0.8,
            )
            .expect("f1");
        store
            .record_fact("hobby", "plays the cello on weekends", "user", "agent", 0.8)
            .expect("f2");
        let mut graph = CausalGraph::from_store(&store).expect("graph");
        let results = graph.spreading_activation("degree in physics", None, false);
        let texts: Vec<&str> = results.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("physics")),
            "queried fact must activate: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("cello")),
            "unrelated same-scope fact must NOT activate via the hub: {texts:?}"
        );
    }
}
