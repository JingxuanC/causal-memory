#[cfg(test)]
mod tests {
    use super::super::*;

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
}
