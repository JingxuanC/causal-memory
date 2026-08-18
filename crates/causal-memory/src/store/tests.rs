use super::*;
use rusqlite::params;

    #[test]
    fn test_record_and_search() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used Redis with mutex lock",
                "deadlock — holder crashed without releasing",
                "caused",
                Some("concurrency"),
                0.85,
                "llm_inferred",
            )
            .unwrap();
        store
            .record_decision(
                "switched to channel/single-flight",
                "successfully fixed race condition",
                "caused",
                Some("concurrency"),
                0.95,
                "user_feedback",
            )
            .unwrap();

        // Search by task
        let results = store.search_causal(Some("concurrency"), None).unwrap();
        assert_eq!(results.len(), 2);
        // Higher confidence first
        assert!(results[0].confidence >= results[1].confidence);

        // Search by query text
        let results = store.search_causal(None, Some("mutex")).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].decision_text.contains("mutex"));

        // Trace cause
        let causes = store.trace_cause("deadlock").unwrap();
        assert_eq!(causes.len(), 1);
        assert!(causes[0].decision_text.contains("mutex"));

        // Recent decisions
        let dir = store.recent_decisions(5).unwrap();
        assert_eq!(dir.len(), 2);
    }

    #[test]
    fn test_multi_hop_trace() {
        let store = CausalStore::open_in_memory().unwrap();

        // Build a 3-hop chain:
        // A: "configured Redis without TTL" → B: "cache entries never expired"
        // B: "cache entries never expired" → C: "memory grew unbounded"
        // C: "memory grew unbounded" → D: "service OOM and crashed"
        let _id_a = store
            .record_decision(
                "configured Redis without TTL",
                "cache entries never expired",
                "caused",
                Some("caching"),
                0.8,
                "llm_inferred",
            )
            .unwrap();
        let _id_b = store
            .record_decision(
                "cache entries never expired",
                "memory grew unbounded",
                "caused",
                Some("caching"),
                0.85,
                "llm_inferred",
            )
            .unwrap();
        // Link B's outcome to C's decision — but B's outcome is not a decision chunk.
        // For this test we create a synthetic chain by making the "outcome" of step 1
        // the "decision" text of step 2. In production the auto-extractor would handle
        // this via outcome-to-decision bridging.
        let _id_c = store
            .record_decision(
                "memory grew unbounded",
                "service OOM and crashed",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();

        // Manually create bridge edges (the chain_linker would do this automatically)
        // These connect outcome_i → decision_j so the CTE can walk multi-hop.
        store.with_conn(|conn| {
            // outcome of A (cache entries never expired) → decision of B (cache entries never expired)
            // But since record_decision creates new chunk IDs each time, we link by text match.
            // Instead, link outcome of B → decision of C:
            conn.execute(
                "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                 SELECT ct.id, cf2.id, 'caused', 0.7, 'rule', 0, 0, 'caching'
                 FROM causal_edges ce1
                 JOIN chunks ct ON ct.id = ce1.to_id
                 JOIN causal_edges ce2 ON ce2.id != ce1.id
                 JOIN chunks cf2 ON cf2.id = ce2.from_id
                 WHERE ct.text = 'memory grew unbounded' AND cf2.text = 'memory grew unbounded'",
                [],
            )?;
            // outcome of A → decision of B
            conn.execute(
                "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                 SELECT ct.id, cf2.id, 'caused', 0.7, 'rule', 0, 0, 'caching'
                 FROM chunks ct
                 JOIN chunks cf2 ON cf2.text = 'cache entries never expired'
                 WHERE ct.text = 'cache entries never expired' AND ct.id LIKE 'o%' AND cf2.id LIKE 'd%' AND cf2.text != 'configured Redis without TTL'",
                [],
            )?;
            Ok(())
        }).unwrap();

        // Single-hop still works
        let single = store.trace_cause("OOM").unwrap();
        assert!(!single.is_empty());

        // Multi-hop: search for "crashed" and walk back 3 hops
        let chains = store.trace_cause_chain("crashed", 5, 0.3).unwrap();
        assert!(!chains.is_empty(), "expected at least one causal chain");
        // Every returned chain must be multi-hop (trace_cause_chain filters depth >= 2)
        let max_len = chains.iter().map(|c| c.len()).max().unwrap();
        assert!(max_len >= 2, "expected a multi-hop (depth >= 2) chain");
        assert!(
            chains.iter().all(|c| c.len() >= 2),
            "trace_cause_chain must only return chains with depth >= 2"
        );
    }

    /// Test helper: insert a raw chunk-to-chunk causal edge, creating chunks on
    /// demand. Chunk ids are derived from the text so the same text is always
    /// the same node — this lets us build clean multi-hop graphs without the
    /// bridge edges record_decision would need.
    fn link(store: &CausalStore, from: &str, to: &str, relation: &str, conf: f64) -> i64 {
        store
            .with_conn(|conn| {
                for text in [from, to] {
                    conn.execute(
                        "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, 1000)",
                        params![format!("chunk:{text}"), text],
                    )?;
                }
                conn.execute(
                    "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
                     VALUES (?1, ?2, ?3, ?4, 'rule', 1000, 1000)",
                    params![format!("chunk:{from}"), format!("chunk:{to}"), relation, conf],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .unwrap()
    }

    #[test]
    fn test_trace_effect_chain_three_hops() {
        let store = CausalStore::open_in_memory().unwrap();
        // A → B → C → D forward chain
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        link(&store, "state bravo", "state charlie", "caused", 0.8);
        link(&store, "state charlie", "state delta", "caused", 0.7);

        let chains = store.trace_effect_chain("action alpha", 5, 0.1).unwrap();
        // CTE yields chains of depth 1, 2 and 3 from the same anchor
        let full = chains
            .iter()
            .find(|c| c.len() == 3)
            .expect("expected the full 3-hop chain");
        // Hop order and texts
        assert_eq!(full[0].hop, 1);
        assert_eq!(full[0].decision_text, "action alpha");
        assert_eq!(full[0].outcome_text, "state bravo");
        assert_eq!(full[1].hop, 2);
        assert_eq!(full[1].decision_text, "state bravo");
        assert_eq!(full[1].outcome_text, "state charlie");
        assert_eq!(full[2].hop, 3);
        assert_eq!(full[2].decision_text, "state charlie");
        assert_eq!(full[2].outcome_text, "state delta");
        // chain_confidence multiplies hop by hop
        assert!((full[0].chain_confidence - 0.9).abs() < 1e-9);
        assert!((full[1].chain_confidence - 0.9 * 0.8).abs() < 1e-9);
        assert!((full[2].chain_confidence - 0.9 * 0.8 * 0.7).abs() < 1e-9);
    }

    #[test]
    fn test_trace_effect_chain_branching() {
        let store = CausalStore::open_in_memory().unwrap();
        // One decision, two downstream effects
        link(&store, "action alpha", "outcome one", "caused", 0.9);
        link(&store, "action alpha", "outcome two", "enabled", 0.8);

        let chains = store.trace_effect_chain("action alpha", 3, 0.1).unwrap();
        assert_eq!(chains.len(), 2, "each downstream edge is its own chain");
        let terminals: Vec<&str> = chains
            .iter()
            .map(|c| c.last().unwrap().outcome_text.as_str())
            .collect();
        assert!(terminals.contains(&"outcome one"));
        assert!(terminals.contains(&"outcome two"));
    }

    #[test]
    fn test_trace_effect_chain_min_confidence_pruning() {
        let store = CausalStore::open_in_memory().unwrap();
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        link(&store, "state bravo", "state charlie", "caused", 0.4);

        // 0.4 edge is below the per-edge threshold and also drags the running
        // chain confidence (0.9*0.4=0.36) below 0.5 — must be pruned.
        let chains = store.trace_effect_chain("action alpha", 5, 0.5).unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 1);
        assert_eq!(chains[0][0].outcome_text, "state bravo");
    }

    #[test]
    fn test_trace_effect_chain_max_depth() {
        let store = CausalStore::open_in_memory().unwrap();
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        link(&store, "state bravo", "state charlie", "caused", 0.9);
        link(&store, "state charlie", "state delta", "caused", 0.9);

        let chains = store.trace_effect_chain("action alpha", 2, 0.1).unwrap();
        let max_len = chains.iter().map(|c| c.len()).max().unwrap();
        assert_eq!(max_len, 2, "max_depth=2 must cap chain length at 2");
    }

    #[test]
    fn test_trace_effect_chain_excludes_invalidated() {
        let store = CausalStore::open_in_memory().unwrap();
        link(&store, "action alpha", "state bravo", "caused", 0.9);
        let edge_bc = link(&store, "state bravo", "state charlie", "caused", 0.9);
        assert!(store.invalidate_edge(edge_bc).unwrap());

        let chains = store.trace_effect_chain("action alpha", 5, 0.1).unwrap();
        assert!(
            chains
                .iter()
                .flatten()
                .all(|h| h.outcome_text != "state charlie"),
            "invalidated edges must not appear in forward chains"
        );
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 1);
    }

    #[test]
    fn test_trace_effect_chain_access_tracking() {
        let store = CausalStore::open_in_memory().unwrap();
        let edge_ab = link(&store, "action alpha", "state bravo", "caused", 0.9);
        let edge_bc = link(&store, "state bravo", "state charlie", "caused", 0.9);

        assert_eq!(store.get_edge(edge_ab).unwrap().unwrap().access_count, 0);
        assert_eq!(store.get_edge(edge_bc).unwrap().unwrap().access_count, 0);

        store.trace_effect_chain("action alpha", 5, 0.1).unwrap();

        let ab = store.get_edge(edge_ab).unwrap().unwrap();
        let bc = store.get_edge(edge_bc).unwrap().unwrap();
        assert_eq!(ab.access_count, 1, "hit edges get access_count + 1");
        assert_eq!(bc.access_count, 1);
        assert!(ab.last_accessed_at.is_some());
        assert!(bc.last_accessed_at.is_some());
    }

    #[test]
    fn test_invalidate_edge() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used global mutable cache",
                "data race under concurrent writes",
                "caused",
                Some("concurrency"),
                0.85,
                "user_feedback",
            )
            .unwrap();

        let edge_id = store.search_causal(Some("concurrency"), None).unwrap()[0].edge_id;

        // Invalidate
        assert!(store.invalidate_edge(edge_id).unwrap());

        // All read paths stop returning it
        assert!(store
            .search_causal(Some("concurrency"), None)
            .unwrap()
            .is_empty());
        assert!(store.search_causal(None, Some("cache")).unwrap().is_empty());
        assert!(store.trace_cause("data race").unwrap().is_empty());
        assert!(store
            .trace_cause_chain("data race", 3, 0.1)
            .unwrap()
            .is_empty());

        // get_edge still sees it, with valid_to set and audit fields populated
        let edge = store.get_edge(edge_id).unwrap().expect("edge must exist");
        assert!(edge.valid_to.is_some());
        assert_eq!(edge.decision_text, "used global mutable cache");
        assert_eq!(edge.discovered_by, "user_feedback");
        assert!(edge.discovered_at > 0);
        // search_causal hit it once before invalidation
        assert_eq!(edge.access_count, 1);
        assert!(edge.last_accessed_at.is_some());

        // Re-invalidate is a no-op
        assert!(!store.invalidate_edge(edge_id).unwrap());
        // Unknown id: false, no error
        assert!(!store.invalidate_edge(999_999).unwrap());
        assert!(store.get_edge(999_999).unwrap().is_none());
    }

    #[test]
    fn test_contradiction_short_circuit() {
        let store = CausalStore::open_in_memory().unwrap();

        // Same decision recorded twice with opposite outcomes: the new evidence
        // falsifies the old lesson, so the old edge is auto-invalidated.
        store
            .record_decision(
                "用方案A部署",
                "部署失败 error: port already in use",
                "caused",
                Some("deploy"),
                0.7,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "用方案A部署",
                "部署成功",
                "caused",
                Some("deploy"),
                0.95,
                "user_feedback",
            )
            .unwrap();

        assert_eq!(store.count_edges().unwrap(), 2);

        // Only the new edge survives on read paths
        let results = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome_text, "部署成功");
        assert!(results[0].valid_to.is_none());
        let new_edge_id = results[0].edge_id;

        // The old edge is invalidated but auditable via get_edge
        let edges: Vec<i64> = store
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT id FROM causal_edges ORDER BY id")?;
                let ids = stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(ids)
            })
            .unwrap();
        let old_edge_id = edges
            .iter()
            .find(|id| **id != new_edge_id)
            .copied()
            .unwrap();
        let old_edge = store.get_edge(old_edge_id).unwrap().unwrap();
        assert!(old_edge.valid_to.is_some());
        assert!(old_edge.outcome_text.contains("部署失败"));

        // trace_cause on the old failure outcome no longer returns it
        assert!(store.trace_cause("port already in use").unwrap().is_empty());
    }

    #[test]
    fn test_contradiction_not_triggered_by_same_direction() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "用方案B部署",
                "部署失败 error",
                "caused",
                Some("deploy"),
                0.7,
                "rule",
            )
            .unwrap();
        // Same-direction (also failure): NOT a contradiction, both stay valid.
        store
            .record_decision(
                "用方案B部署",
                "部署再次失败 timeout",
                "caused",
                Some("deploy"),
                0.7,
                "rule",
            )
            .unwrap();
        let results = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(
            results.len(),
            2,
            "both same-direction edges must stay valid"
        );
    }

    #[test]
    fn test_outcomes_contradict() {
        // Contradicting pairs (one failure, one not) — EN
        assert!(outcomes_contradict(
            "deploy failed with error",
            "deploy succeeded"
        ));
        assert!(outcomes_contradict(
            "deadlock: holder crashed",
            "fixed the race condition"
        ));
        assert!(outcomes_contradict(
            "all tests pass",
            "timeout in integration test"
        ));
        // Contradicting pairs — ZH
        assert!(outcomes_contradict("部署失败,报错端口占用", "部署成功"));
        assert!(outcomes_contradict("服务崩溃", "问题已修复,运行正常"));
        // Failure vs neutral counts as contradiction (old lesson falsified)
        assert!(outcomes_contradict(
            "panic: index out of bounds",
            "deploy finished"
        ));

        // Same direction — NOT contradictions
        assert!(!outcomes_contradict(
            "deploy failed",
            "another error occurred"
        ));
        assert!(!outcomes_contradict("死锁复现", "再次崩溃"));
        assert!(!outcomes_contradict(
            "deploy succeeded",
            "all checks passed"
        ));
        assert!(!outcomes_contradict("部署成功", "测试通过"));
        // Both neutral — NOT a contradiction
        assert!(!outcomes_contradict("deploy finished", "rollout completed"));
        // Success overrides a co-occurring failure word — same direction, NOT a contradiction
        assert!(!outcomes_contradict("fixed the error", "deploy succeeded"));
    }

    #[test]
    fn test_outcome_polarity_word_boundaries() {
        // "unresolved" must NOT hit the "resolved" success token
        assert_eq!(outcome_polarity("unresolved issue"), None);
        // Failure word names the problem that was fixed → success
        assert_eq!(outcome_polarity("deadlock resolved"), Some(true));
        assert_eq!(outcome_polarity("deploy success"), Some(true));
        // Inflections of "success" still match on word boundaries
        assert_eq!(
            outcome_polarity("cargo build completed successfully"),
            Some(true)
        );
        assert_eq!(outcome_polarity("build succeeded"), Some(true));
        // ...but the negated form does not
        assert_eq!(outcome_polarity("deploy unsuccessful"), None);
        // Short-token boundary checks: "invoke"/"compass" are not "ok"/"pass"
        assert_eq!(outcome_polarity("invoke compass"), None);
        assert_eq!(outcome_polarity("all tests pass"), Some(true));
        // ZH signals keep substring matching
        assert_eq!(outcome_polarity("问题已修复,运行正常"), Some(true));
    }

    #[test]
    fn test_semantic_search() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "used mutex",
                "deadlock",
                "caused",
                Some("concurrency"),
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "switched to channel",
                "fixed race",
                "caused",
                Some("concurrency"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "redis without ttl",
                "stampede",
                "caused",
                Some("caching"),
                0.7,
                "rule",
            )
            .unwrap();

        // Fresh in-memory DB → edge ids are 1, 2, 3 in insertion order.
        store.put_embedding(1, "test", &[1.0, 0.0, 0.0]).unwrap();
        store.put_embedding(2, "test", &[0.9, 0.1, 0.0]).unwrap();
        store.put_embedding(3, "test", &[0.0, 1.0, 0.0]).unwrap();

        // Ranking: descending cosine similarity, exact match first.
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 10)
            .unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].0.edge_id, 1);
        assert!((res[0].1 - 1.0).abs() < 1e-6);
        assert!(res[0].1 > res[1].1 && res[1].1 > res[2].1);

        // task_tag filter
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], Some("caching"), 10)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0.edge_id, 3);

        // limit
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 1)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0.edge_id, 1);

        // Invalidated edges must not appear.
        store.invalidate_edge(1).unwrap();
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 10)
            .unwrap();
        assert!(res.iter().all(|(e, _)| e.edge_id != 1));

        // Edges without an embedding never appear in semantic results.
        store
            .record_decision("no vector edge", "nothing", "caused", None, 0.5, "rule")
            .unwrap();
        let res = store
            .search_causal_semantic(&[1.0, 0.0, 0.0], None, 10)
            .unwrap();
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_put_embedding_overwrites() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision("d", "o", "caused", None, 0.5, "rule")
            .unwrap();
        store.put_embedding(1, "test", &[1.0, 0.0]).unwrap();
        // Overwrite with a different vector — must replace, not duplicate/fail.
        store.put_embedding(1, "test", &[0.0, 1.0]).unwrap();
        let res = store.search_causal_semantic(&[1.0, 0.0], None, 10).unwrap();
        assert_eq!(res.len(), 1);
        assert!(
            res[0].1 < 1e-6,
            "overwritten vector must be the one searched"
        );
    }

    #[test]
    fn test_edges_without_embedding() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision("mutex", "deadlock", "caused", None, 0.8, "rule")
            .unwrap();
        store
            .record_decision("channel", "fixed race", "caused", None, 0.9, "rule")
            .unwrap();

        let pending = store.edges_without_embedding(10).unwrap();
        assert_eq!(pending.len(), 2);
        // Text is "decision outcome" — the shape the record path embeds.
        assert!(pending[0].1.contains("mutex"));
        assert!(pending[0].1.contains("deadlock"));

        store.put_embedding(1, "test", &[1.0]).unwrap();
        let pending = store.edges_without_embedding(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, 2);

        // limit
        let pending = store.edges_without_embedding(1).unwrap();
        assert_eq!(pending.len(), 1);

        // Invalidated edges are excluded from backfill.
        store.invalidate_edge(2).unwrap();
        assert!(store.edges_without_embedding(10).unwrap().is_empty());
    }

    #[test]
    fn test_similar_decision_edges() {
        let store = CausalStore::open_in_memory().unwrap();
        // Distinct decision texts keep the contradiction short-circuit out of
        // the way; fresh in-memory DB → edge ids 1..=5 in insertion order.
        store
            .record_decision("used Redis mutex", "deadlock", "caused", None, 0.8, "rule")
            .unwrap();
        store
            .record_decision(
                "used Redis lock",
                "deadlock again",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "switched to channel",
                "fixed race",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "added cache TTL",
                "stampede stopped",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 5 gets no embedding — semantic paths must never return it.
        store
            .record_decision("no vector edge", "nothing", "caused", None, 0.5, "rule")
            .unwrap();

        store.put_embedding(1, "test", &[1.0, 0.0, 0.0]).unwrap(); // sim 1.0
        store.put_embedding(2, "test", &[0.9, 0.1, 0.0]).unwrap(); // sim ≈ 0.994
        store.put_embedding(3, "test", &[0.6, 0.8, 0.0]).unwrap(); // sim 0.6
        store.put_embedding(4, "test", &[0.0, 1.0, 0.0]).unwrap(); // sim 0.0

        // Threshold 0.5: edges 1-3, ranked by similarity descending.
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 10, 0.5)
            .unwrap();
        let ids: Vec<i64> = res.iter().map(|(e, _)| e.edge_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(res[0].1 > res[1].1 && res[1].1 > res[2].1);

        // A higher threshold filters out the mid-similarity edge.
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 10, 0.9)
            .unwrap();
        let ids: Vec<i64> = res.iter().map(|(e, _)| e.edge_id).collect();
        assert_eq!(ids, vec![1, 2]);

        // limit applies to the sorted list.
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 1, 0.5)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0.edge_id, 1);

        // Invalidated edges never seed interventions.
        store.invalidate_edge(2).unwrap();
        let res = store
            .similar_decision_edges(&[1.0, 0.0, 0.0], 10, 0.5)
            .unwrap();
        assert!(res.iter().all(|(e, _)| e.edge_id != 2));
    }

    #[test]
    fn test_invalidate_semantic_contradictions() {
        let store = CausalStore::open_in_memory().unwrap();
        // Edge 1: old lesson with a failure outcome, vector close to the query.
        store
            .record_decision(
                "用 Redis 加互斥锁",
                "死锁:持有者崩溃",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 2: vector equally close, but the outcome does NOT contradict.
        store
            .record_decision(
                "Redis mutex for stampede protection",
                "成功防止缓存击穿",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 3: contradicting outcome, but a distant (orthogonal) vector.
        store
            .record_decision(
                "switched to channel single-flight",
                "panic under load",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();
        // Edge 4: exact same decision text as the new edge — that is the
        // exact-match path's job; the semantic path must skip it even with a
        // close vector and a contradicting outcome.
        store
            .record_decision(
                "used Redis with mutex lock",
                "deadlock occurred",
                "caused",
                None,
                0.8,
                "rule",
            )
            .unwrap();

        store.put_embedding(1, "test", &[0.95, 0.05, 0.0]).unwrap();
        store.put_embedding(2, "test", &[0.95, 0.05, 0.0]).unwrap();
        store.put_embedding(3, "test", &[0.0, 1.0, 0.0]).unwrap();
        store.put_embedding(4, "test", &[1.0, 0.0, 0.0]).unwrap();

        // New edge: decision text differs from edges 1-3, and its success
        // outcome contradicts the failure outcomes of edges 1/3/4.
        let n = store
            .invalidate_semantic_contradictions(
                "used Redis with mutex lock",
                "成功修复,运行正常",
                None,
                &[1.0, 0.0, 0.0],
                0.85,
            )
            .unwrap();
        assert_eq!(n, 1, "only edge 1 (close vector + contradicting outcome)");

        assert!(store.get_edge(1).unwrap().unwrap().valid_to.is_some());
        let e2 = store.get_edge(2).unwrap().unwrap();
        assert!(e2.valid_to.is_none(), "no contradiction → kept");
        let e3 = store.get_edge(3).unwrap().unwrap();
        assert!(e3.valid_to.is_none(), "low similarity → kept");
        let e4 = store.get_edge(4).unwrap().unwrap();
        assert!(e4.valid_to.is_none(), "same text → exact-match path's job");

        // A query vector with no close neighbors invalidates nothing.
        let n = store
            .invalidate_semantic_contradictions(
                "another decision",
                "再次失败",
                None,
                &[0.0, 0.0, 1.0],
                0.85,
            )
            .unwrap();
        assert_eq!(n, 0);
        assert!(store.get_edge(2).unwrap().unwrap().valid_to.is_none());
        assert!(store.get_edge(3).unwrap().unwrap().valid_to.is_none());
        assert!(store.get_edge(4).unwrap().unwrap().valid_to.is_none());
    }

    #[test]
    fn test_record_with_polarity_and_cte_propagation() {
        let store = CausalStore::open_in_memory().unwrap();
        // Build A → B → C with the link helper (raw edges, polarity NULL).
        let edge_ab = link(&store, "action alpha", "state bravo", "caused", 0.9);
        let edge_bc = link(&store, "state bravo", "state charlie", "caused", 0.9);

        // Both edges lack a stored polarity → eligible for backfill.
        let pending = store.edges_without_polarity(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].0, edge_ab);
        assert!(pending[0].1.contains("action alpha"));
        assert!(pending[0].2.contains("state bravo"));

        store.set_outcome_polarity(edge_ab, "negative").unwrap();
        store.set_outcome_polarity(edge_bc, "mixed").unwrap();
        assert!(store.edges_without_polarity(10).unwrap().is_empty());
        // Out-of-enum values are rejected by the CHECK constraint.
        assert!(store.set_outcome_polarity(edge_ab, "bogus").is_err());

        // Forward CTE hops carry the stored polarity.
        let chains = store.trace_effect_chain("action alpha", 5, 0.1).unwrap();
        let full = chains.iter().find(|c| c.len() == 2).expect("2-hop chain");
        assert_eq!(full[0].outcome_polarity.as_deref(), Some("negative"));
        assert_eq!(full[1].outcome_polarity.as_deref(), Some("mixed"));

        // Backward CTE hops carry it too.
        let chains = store.trace_cause_chain("state charlie", 5, 0.1).unwrap();
        let full = chains.iter().find(|c| c.len() == 2).expect("2-hop chain");
        assert_eq!(full[0].outcome_polarity.as_deref(), Some("mixed"));
        assert_eq!(full[1].outcome_polarity.as_deref(), Some("negative"));

        // record_decision_full persists the polarity it is given; the plain
        // record_decision path stores NULL.
        store
            .record_decision_full(
                "used Redis mutex",
                "deadlock under load; fixed by switching to channels",
                "caused",
                None,
                0.8,
                "rule",
                1000,
                Some("mixed"),
            )
            .unwrap();
        store
            .record_decision("plain record", "nothing", "caused", None, 0.5, "rule")
            .unwrap();
        let pending = store.edges_without_polarity(10).unwrap();
        assert_eq!(pending.len(), 1, "only the NULL-polarity edge is pending");
        assert!(pending[0].1.contains("plain record"));
    }

    #[test]
    fn test_contradiction_stored_polarity() {
        let store = CausalStore::open_in_memory().unwrap();
        let record = |outcome: &str, polarity: Option<&str>| {
            store
                .record_decision_full(
                    "用方案A部署",
                    outcome,
                    "caused",
                    Some("deploy"),
                    0.8,
                    "rule",
                    1000,
                    polarity,
                )
                .unwrap();
        };

        // stored negative old + stored positive new → old invalidated, even
        // though both outcome TEXTS look neutral to the heuristic.
        record("rollout done", Some("negative"));
        record("rollout done again", Some("positive"));
        let valid = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].outcome_text, "rollout done again");

        // stored mixed old + stored positive new → NOT invalidated (mixed
        // never triggers on either side), even though the old outcome text
        // contains a failure signal the heuristic would latch onto.
        record("deadlock occurred; fixed later", Some("mixed"));
        let valid = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(valid.len(), 2, "mixed old edge must survive");

        // stored positive old + stored negative new → NOT invalidated
        // (conservative: only negative-old + positive-new invalidates).
        record("再次失败", Some("negative"));
        let valid = store.search_causal(Some("deploy"), None).unwrap();
        assert_eq!(
            valid.len(),
            3,
            "nothing invalidated: positive edge + mixed edge + the new negative edge itself"
        );
    }

    #[test]
    fn test_semantic_contradiction_stored_polarity() {
        let store = CausalStore::open_in_memory().unwrap();
        // Edge 1: text looks like failure, but stored 'mixed' — the stored
        // value must win and protect it from invalidation.
        store
            .record_decision_full(
                "用 Redis 加互斥锁",
                "死锁:持有者崩溃",
                "caused",
                None,
                0.8,
                "rule",
                1000,
                Some("mixed"),
            )
            .unwrap();
        // Edge 2: stored 'negative' with a neutral-looking text — stored wins,
        // so a positive new edge invalidates it.
        store
            .record_decision_full(
                "Redis mutex for stampede protection",
                "rollout finished",
                "caused",
                None,
                0.8,
                "rule",
                1001,
                Some("negative"),
            )
            .unwrap();
        store.put_embedding(1, "test", &[1.0, 0.0]).unwrap();
        store.put_embedding(2, "test", &[1.0, 0.0]).unwrap();

        let n = store
            .invalidate_semantic_contradictions(
                "used Redis with mutex lock",
                "rollout completed",
                Some("positive"),
                &[1.0, 0.0],
                0.85,
            )
            .unwrap();
        assert_eq!(n, 1, "only the stored-negative edge is invalidated");
        assert!(
            store.get_edge(1).unwrap().unwrap().valid_to.is_none(),
            "mixed → kept"
        );
        assert!(store.get_edge(2).unwrap().unwrap().valid_to.is_some());
    }

    // ── search_causal_bm25 ───────────────────────────────────────────────

    /// Three caching edges + one unrelated edge, all valid.
    fn bm25_store() -> CausalStore {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "cache stampede protection with Redis",
                "stampede stopped, hit ratio recovered",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "used Redis mutex lock",
                "deadlock under load",
                "caused",
                Some("caching"),
                0.8,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "added cache TTL to Redis",
                "memory grew bounded again",
                "caused",
                Some("caching"),
                0.85,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "rewrote parser in rust",
                "build success",
                "caused",
                Some("compiler"),
                0.95,
                "rule",
            )
            .unwrap();
        store
    }

    #[test]
    fn test_bm25_beats_like_on_word_order() {
        // The LoCoMo failure case: LIKE on the full question string can never
        // match a doc whose words appear in a different order ("Redis cache
        // stampede" vs "cache stampede protection with Redis"); BM25 does.
        let store = bm25_store();
        assert!(store
            .search_causal(None, Some("Redis cache stampede"))
            .unwrap()
            .is_empty());
        let res = store
            .search_causal_bm25(None, "Redis cache stampede", 10)
            .unwrap();
        assert!(!res.is_empty());
        assert_eq!(
            res[0].decision_text, "cache stampede protection with Redis",
            "the 3-term doc must outrank the 2-term docs"
        );
        // The unrelated compiler edge must not appear.
        assert!(res.iter().all(|e| e.task_tag.as_deref() == Some("caching")));
    }

    #[test]
    fn test_bm25_task_tag_filter_scopes_idf() {
        let store = bm25_store();
        let res = store
            .search_causal_bm25(Some("compiler"), "redis cache stampede build", 10)
            .unwrap();
        assert_eq!(res.len(), 1, "task filter must exclude caching edges");
        assert_eq!(res[0].task_tag.as_deref(), Some("compiler"));
        // An unknown tag → empty candidate set → empty result, not an error.
        assert!(store
            .search_causal_bm25(Some("nope"), "redis", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_bm25_limit_and_score_order() {
        let store = bm25_store();
        let res = store.search_causal_bm25(None, "redis", 2).unwrap();
        assert_eq!(res.len(), 2, "limit truncates the ranked list");
        let full = store.search_causal_bm25(None, "redis", 10).unwrap();
        assert!(full.len() >= 2);
        assert_eq!(
            res.iter().map(|e| e.edge_id).collect::<Vec<_>>(),
            full[..2].iter().map(|e| e.edge_id).collect::<Vec<_>>(),
            "limit must keep the top of the same ranking"
        );
    }

    #[test]
    fn test_bm25_excludes_invalidated_and_tracks_access() {
        let store = bm25_store();
        let hit = store
            .search_causal_bm25(None, "cache stampede", 10)
            .unwrap();
        assert!(!hit.is_empty());
        let top_id = hit[0].edge_id;

        // record_access: every returned edge gets access_count + 1.
        let before = store.get_edge(top_id).unwrap().unwrap().access_count;
        store
            .search_causal_bm25(None, "cache stampede", 10)
            .unwrap();
        let after = store.get_edge(top_id).unwrap().unwrap();
        assert_eq!(after.access_count, before + 1);
        assert!(after.last_accessed_at.is_some());

        // Invalidated edges no longer participate in the index.
        store.invalidate_edge(top_id).unwrap();
        let res = store
            .search_causal_bm25(None, "cache stampede", 10)
            .unwrap();
        assert!(res.iter().all(|e| e.edge_id != top_id));
    }

    #[test]
    fn test_bm25_oov_and_empty_query_fallback() {
        let store = bm25_store();
        // All query terms out-of-vocabulary → empty (not an error).
        assert!(store
            .search_causal_bm25(None, "zzzxqqq", 10)
            .unwrap()
            .is_empty());
        // Empty / stop-words-only query → plain task_tag listing fallback.
        let res = store.search_causal_bm25(Some("caching"), "", 10).unwrap();
        assert_eq!(res.len(), 3);
        let res = store
            .search_causal_bm25(Some("caching"), "the a an", 2)
            .unwrap();
        assert_eq!(res.len(), 2, "fallback respects limit");
    }

    #[test]
    fn test_bm25_chinese_bigrams() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "用Redis做缓存防止缓存击穿",
                "缓存命中率恢复成功",
                "caused",
                Some("caching"),
                0.9,
                "rule",
            )
            .unwrap();
        store
            .record_decision(
                "重写数据库连接池",
                "连接耗尽错误消失",
                "caused",
                Some("db"),
                0.8,
                "rule",
            )
            .unwrap();
        let res = store.search_causal_bm25(None, "缓存击穿", 10).unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].decision_text.contains("缓存击穿"));
    }

    #[test]
    fn test_markov_blanket() {
        let store = CausalStore::open_in_memory().unwrap();
        // Graph: A→B, A→C, D→B, B→E, plus an unrelated F→G.
        let seed = link(&store, "node A", "node B", "caused", 0.9);
        let e_ac = link(&store, "node A", "node C", "caused", 0.8);
        let e_db = link(&store, "node D", "node B", "caused", 0.7);
        let e_be = link(&store, "node B", "node E", "caused", 0.6);
        let _e_fg = link(&store, "node F", "node G", "caused", 0.5);

        let blanket = store.markov_blanket(&[seed], 20).unwrap();
        let ids: Vec<i64> = blanket.iter().map(|e| e.edge_id).collect();
        // Seed first, then co-parent (A→C), parent (D→B), child (B→E).
        assert_eq!(ids[0], seed);
        assert!(ids.contains(&e_ac), "shares from_id (co-parent)");
        assert!(ids.contains(&e_db), "shares to_id (parent)");
        assert!(ids.contains(&e_be), "shares from_id of B (child)");
        assert_eq!(ids.len(), 4, "unrelated F→G excluded: {ids:?}");

        // Neighbors are confidence-ordered after the seeds.
        let neighbor_confs: Vec<f64> = blanket[1..].iter().map(|e| e.confidence).collect();
        assert!(neighbor_confs.windows(2).all(|w| w[0] >= w[1]));

        // max_edges caps the total, seeds kept.
        let capped = store.markov_blanket(&[seed], 2).unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].edge_id, seed);

        // Unknown seed → empty blanket.
        assert!(store.markov_blanket(&[999_999], 20).unwrap().is_empty());

        // Invalidated neighbors are excluded.
        store.invalidate_edge(e_be).unwrap();
        let blanket = store.markov_blanket(&[seed], 20).unwrap();
        assert!(blanket.iter().all(|e| e.edge_id != e_be));
    }

    // -- record_distilled --

    fn item(
        kind: crate::distill::ItemKind,
        text: &str,
        date: &str,
        supersedes: Option<&str>,
    ) -> crate::distill::MemoryItem {
        crate::distill::MemoryItem {
            kind,
            text: text.to_string(),
            date: Some(date.to_string()),
            supersedes: supersedes.map(str::to_string),
            causal_relation: None,
            decision: None,
        }
    }

    #[test]
    fn test_record_distilled_basic() {
        let store = CausalStore::open_in_memory().unwrap();
        let out = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "The user prefers Vim keybindings.",
                    "2025-06-03",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(!out.duplicate);
        let edge_id = out.edge_id.expect("new item must create an edge");
        assert!(out.invalidated_edge_ids.is_empty());

        let edge = store.get_edge(edge_id).unwrap().unwrap();
        // Chunk text carries the [date] prefix; self-edge keeps retrieval to
        // one line per item.
        assert_eq!(
            edge.decision_text,
            "[2025-06-03] The user prefers Vim keybindings."
        );
        assert_eq!(edge.decision_id, edge.outcome_id);
        assert_eq!(edge.task_tag.as_deref(), Some("p1"));
        assert_eq!(edge.event_time, 1_748_908_800); // 2025-06-03T00:00:00Z
        assert_eq!(edge.discovered_by, "distill");

        // Visible to BM25 (the bench retrieval path).
        let hits = store
            .search_causal_bm25(Some("p1"), "Vim keybindings", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].edge_id, edge_id);
    }

    #[test]
    fn test_record_distilled_idempotent() {
        let store = CausalStore::open_in_memory().unwrap();
        let it = item(
            crate::distill::ItemKind::Fact,
            "The user works as a software engineer.",
            "2025-06-03",
            None,
        );
        let first = store.record_distilled(&it, Some("p1")).unwrap();
        let second = store.record_distilled(&it, Some("p1")).unwrap();
        assert!(second.duplicate);
        assert_eq!(first.chunk_id, second.chunk_id);
        assert_eq!(second.edge_id, None);
        assert_eq!(store.count_edges().unwrap(), 1, "no duplicate edge");
    }

    #[test]
    fn test_record_distilled_supersedes_invalidates_old() {
        let store = CausalStore::open_in_memory().unwrap();
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user added Buy groceries to their todo list.",
                    "2025-06-01",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let old_edge_id = old.edge_id.unwrap();

        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user completed the Buy groceries todo.",
                    "2025-06-05",
                    Some("Buy groceries todo"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert_eq!(new.invalidated_edge_ids, vec![old_edge_id]);

        // Old edge is soft-invalidated: gone from BM25, auditable via get_edge.
        let old_edge = store.get_edge(old_edge_id).unwrap().unwrap();
        assert!(old_edge.valid_to.is_some());
        let hits = store
            .search_causal_bm25(Some("p1"), "groceries todo", 10)
            .unwrap();
        // Two valid hits now: the new item and the negation memory spawned
        // for the killed entry (guard 3) — the invalidated original is gone.
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.decision_text.contains("completed")));
        assert!(hits
            .iter()
            .any(|h| h.decision_text.contains("Cancelled/superseded")));
        assert!(hits.iter().all(|h| h.edge_id != old_edge_id));
    }

    #[test]
    fn test_record_distilled_supersedes_below_threshold_keeps_old() {
        let store = CausalStore::open_in_memory().unwrap();
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "The user prefers Vim keybindings.",
                    "2025-06-01",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user booked a flight to Berlin.",
                    "2025-06-05",
                    Some("flight Berlin booking"), // unrelated hint
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(new.invalidated_edge_ids.is_empty());
        let old_edge = store.get_edge(old.edge_id.unwrap()).unwrap().unwrap();
        assert!(old_edge.valid_to.is_none());
    }

    #[test]
    fn test_record_distilled_supersedes_scoped_to_task_tag() {
        let store = CausalStore::open_in_memory().unwrap();
        // Same text under another persona must not be invalidated.
        let other = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user added Buy groceries to their todo list.",
                    "2025-06-01",
                    None,
                ),
                Some("p2"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user completed the Buy groceries todo.",
                    "2025-06-05",
                    Some("Buy groceries todo"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(new.invalidated_edge_ids.is_empty());
        assert!(store
            .get_edge(other.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_none());
    }

    #[test]
    fn test_record_distilled_supersedes_guards() {
        let store = CausalStore::open_in_memory().unwrap();
        // One-token hint: even though containment would score 1.0, the
        // shared-token guard must prevent invalidation.
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "The user likes space opera books.",
                    "2025-06-01",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "The user now prefers hard sci-fi books.",
                    "2025-06-05",
                    Some("books"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(
            new.invalidated_edge_ids.is_empty(),
            "one-token hint must not invalidate"
        );
        assert!(store
            .get_edge(old.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_none());

        // Future-dated candidate: a supersedes hint must not invalidate an
        // edge NEWER than the item carrying the hint.
        let store = CausalStore::open_in_memory().unwrap();
        let future = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user rescheduled the mechanic visit to 2025-06-10.",
                    "2025-06-04",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user scheduled a mechanic visit for 2025-06-15.",
                    "2025-06-01",
                    Some("mechanic visit scheduled"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(
            new.invalidated_edge_ids.is_empty(),
            "must not invalidate an edge newer than the item"
        );
        assert!(store
            .get_edge(future.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_none());
    }

    #[test]
    fn test_record_distilled_missing_date_uses_now() {
        let store = CausalStore::open_in_memory().unwrap();
        let it = crate::distill::MemoryItem {
            kind: crate::distill::ItemKind::Fact,
            text: "Undated fact.".into(),
            date: None,
            supersedes: None,
            causal_relation: None,
            decision: None,
        };
        let out = store.record_distilled(&it, None).unwrap();
        let edge = store.get_edge(out.edge_id.unwrap()).unwrap().unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!((edge.event_time - now).abs() < 86_400);
        assert!(edge.decision_text.starts_with('['));
    }

    #[test]
    fn test_record_distilled_supersedes_kills_all_matches() {
        // Guard 1 (kill-all): an outdated fact scattered over SEVERAL chunks
        // must lose every matching copy, not just the best one — otherwise
        // the survivors still get retrieved and answered (Memora round-1
        // "single-point invalidation residue" failure).
        let store = CausalStore::open_in_memory().unwrap();
        let old1 = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user scheduled a dentist appointment for 2025-07-01.",
                    "2025-06-01",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let old2 = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "Reminder noted: dentist appointment on 2025-07-01 needs insurance card.",
                    "2025-06-03",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user cancelled the dentist appointment entirely.",
                    "2025-06-20",
                    Some("dentist appointment reminder scheduled"),
                ),
                Some("p1"),
            )
            .unwrap();
        let mut killed = new.invalidated_edge_ids.clone();
        killed.sort_unstable();
        let mut expected = vec![old1.edge_id.unwrap(), old2.edge_id.unwrap()];
        expected.sort_unstable();
        assert_eq!(killed, expected, "ALL matches must be invalidated");
        for eid in expected {
            assert!(store.get_edge(eid).unwrap().unwrap().valid_to.is_some());
        }
        // One negation memory per killed entry.
        let neg = store
            .search_causal_bm25(Some("p1"), "cancelled superseded dentist", 10)
            .unwrap();
        assert_eq!(
            neg.iter()
                .filter(|h| h.decision_text.contains("Cancelled/superseded"))
                .count(),
            2
        );
    }

    #[test]
    fn test_record_distilled_supersedes_same_date_exempt() {
        // Guard 2 (same-fact exemption): the Memora weekly calendar chain —
        // "scheduled 06-15" -> "rescheduled to 06-10" -> "confirmed 06-10".
        // The confirmation must NOT kill the reschedule: both mention
        // 2025-06-10, i.e. they are the same fact restated, while the
        // original 06-15 scheduling (a different date) is a real retraction
        // target and must still die.
        let store = CausalStore::open_in_memory().unwrap();
        let scheduled = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user scheduled a mechanic visit for 2025-06-15.",
                    "2025-06-01",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let rescheduled = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user rescheduled the mechanic visit to 2025-06-10.",
                    "2025-06-04",
                    Some("mechanic visit scheduled"),
                ),
                Some("p1"),
            )
            .unwrap();
        // The reschedule kills the original 06-15 appointment (different
        // date tokens -> a true retraction).
        assert_eq!(
            rescheduled.invalidated_edge_ids,
            vec![scheduled.edge_id.unwrap()]
        );

        let confirmed = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user confirmed the mechanic visit on 2025-06-10.",
                    "2025-06-06",
                    Some("mechanic visit scheduled"),
                ),
                Some("p1"),
            )
            .unwrap();
        // Shared date 2025-06-10 -> restatement, NOT a retraction: the
        // rescheduled entry survives.
        assert!(
            confirmed.invalidated_edge_ids.is_empty(),
            "same-date restatement must not invalidate"
        );
        assert!(store
            .get_edge(rescheduled.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_none());
        let hits = store
            .search_causal_bm25(Some("p1"), "mechanic visit", 10)
            .unwrap();
        assert!(hits.iter().any(|h| h.decision_text.contains("rescheduled")));
        assert!(hits.iter().any(|h| h.decision_text.contains("confirmed")));
        assert!(!hits
            .iter()
            .any(|h| h.decision_text.contains("for 2025-06-15")
                && !h.decision_text.contains("Cancelled/superseded")));
    }

    #[test]
    fn test_record_distilled_supersedes_same_day_retraction_still_kills() {
        // The record-date prefix must NOT activate the same-fact exemption:
        // a preference stated and retracted on the SAME day ("likes 2010s
        // music" -> "no longer likes 2010s music", both 2025-06-05) is a
        // true retraction. Only CONTENT dates count for the exemption.
        let store = CausalStore::open_in_memory().unwrap();
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "User likes music from the 2010s, especially electronic pop.",
                    "2025-06-05",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "User no longer likes music from the 2010s as of 2025-06-05.",
                    "2025-06-05",
                    Some("likes music 2010s"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert_eq!(
            new.invalidated_edge_ids,
            vec![old.edge_id.unwrap()],
            "same RECORD date must not exempt a true retraction"
        );
    }

    #[test]
    fn test_record_distilled_auto_supersedes_without_hint() {
        // Auto-hint fallback: the distiller left `supersedes` empty, but the
        // item text announces a retraction ("no longer ...") — the item's
        // own text becomes the kill hint and the outdated item dies.
        let store = CausalStore::open_in_memory().unwrap();
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "User likes music from the 2010s, especially electronic pop.",
                    "2025-06-05",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "User no longer likes music from the 2010s as of 2025-06-05.",
                    "2025-06-05",
                    None, // <-- no LLM hint; retraction markers take over
                ),
                Some("p1"),
            )
            .unwrap();
        assert_eq!(
            new.invalidated_edge_ids,
            vec![old.edge_id.unwrap()],
            "retraction-marked item must auto-supersede without an LLM hint"
        );
    }

    #[test]
    fn test_generated_ids_survive_process_restart() {
        // Regression (LongMemEval chunked distill): generated chunk ids embed
        // a per-process sequence that restarts at 0 on process start. Without
        // seeding at open, a second process writing to the same DB collides
        // on the chunks PRIMARY KEY.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let item = |seq: usize| {
            crate::distill::MemoryItem {
                kind: crate::distill::ItemKind::Event,
                text: format!("event number {seq}"),
                date: Some("2025-06-05".to_string()),
                supersedes: None,
                causal_relation: None,
                decision: None,
            }
        };
        {
            let store = CausalStore::open(&db).unwrap();
            store.record_distilled(&item(1), None).unwrap();
            store.record_distilled(&item(2), None).unwrap();
        }
        // Process "restart" simulation. A real restart zeroes the
        // process-global ID_COUNTER, but this global is shared by every
        // parallel test — directly resetting it here corrupts their id
        // generation (they would mint chunks colliding with ours). Instead
        // verify the recovery property the seed exists for: reopening the
        // store must raise the counter back above COUNT(*) + 1, and the
        // reopen must never collide regardless of the current global value
        // (fetch_max only grows it; the write path also retries on
        // UNIQUE violations, see insert_chunk_with_retry).
        {
            let store = CausalStore::open(&db).unwrap();
            store
                .record_distilled(&item(3), None)
                .expect("second process must not collide on generated chunk ids");
            let n: i64 = store
                .with_conn(|c| {
                    Ok(c.query_row(
                        "SELECT COUNT(*) FROM chunks WHERE id LIKE 'distill:%'",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(n, 3);
        }
    }

    #[test]
    fn test_retraction_records_are_never_kill_targets() {
        // Two retractions sharing vocabulary ("no longer likes music") must
        // NOT kill each other — retracting a retraction spawns a nonsense
        // double negation ("Cancelled/superseded: User no longer likes
        // Bonobo ...") that resurrects the dead fact (Memora round-2b).
        let store = CausalStore::open_in_memory().unwrap();
        let bonobo = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "User no longer likes Bonobo's music as of 2025-06-02.",
                    "2025-06-02",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Preference,
                    "User no longer likes music from the 2010s as of 2025-06-05.",
                    "2025-06-05",
                    Some("no longer likes music"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(
            new.invalidated_edge_ids.is_empty(),
            "retraction records must be exempt from supersedes kills"
        );
        assert!(store
            .get_edge(bonobo.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_none());
        // And no double-negation memory was written.
        let hits = store
            .search_causal_bm25(Some("p1"), "cancelled superseded bonobo", 10)
            .unwrap();
        assert!(hits
            .iter()
            .all(|h| !h.decision_text.contains("Cancelled/superseded")));
    }

    #[test]
    fn test_supersedes_hint_digit_tokens_ignored() {
        // Date tokens inside a hint must not bridge to same-day chunks:
        // without digit filtering, hint "... 2025-06-05" shares 2025/06/05
        // with EVERY chunk recorded that day (the record prefix tokenizes
        // to digits) and containment over-fires.
        let store = CausalStore::open_in_memory().unwrap();
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user bought groceries and milk.",
                    "2025-06-05",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        let new = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user removed the obsolete entry from the document.",
                    "2025-06-05",
                    Some("removed obsolete entry 2025-06-05"),
                ),
                Some("p1"),
            )
            .unwrap();
        assert!(
            new.invalidated_edge_ids.is_empty(),
            "date digits alone must not make a chunk a kill candidate"
        );
        assert!(store
            .get_edge(old.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_none());
    }

    #[test]
    fn test_record_distilled_negation_memory_retrievable() {
        // Guard 3 (negation memory): a killed entry leaves behind a valid,
        // retrievable Event memory marked "Cancelled/superseded" so the
        // answer side can say "this was cancelled" instead of "no such
        // thing". task_tag is inherited from the new item's scope.
        let store = CausalStore::open_in_memory().unwrap();
        let old = store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user added Buy groceries to their todo list.",
                    "2025-06-01",
                    None,
                ),
                Some("p1"),
            )
            .unwrap();
        store
            .record_distilled(
                &item(
                    crate::distill::ItemKind::Event,
                    "The user completed the Buy groceries todo.",
                    "2025-06-05",
                    Some("Buy groceries todo"),
                ),
                Some("p1"),
            )
            .unwrap();

        let hits = store
            .search_causal_bm25(Some("p1"), "cancelled groceries", 10)
            .unwrap();
        let neg = hits
            .iter()
            .find(|h| h.decision_text.contains("Cancelled/superseded"))
            .expect("negation memory must be retrievable");
        assert_eq!(
            neg.decision_text,
            "[2025-06-05] Cancelled/superseded: [2025-06-01] The user added \
             Buy groceries to their todo list."
        );
        // It is an ordinary valid edge in the same task_tag scope.
        let neg_edge = store.get_edge(neg.edge_id).unwrap().unwrap();
        assert!(neg_edge.valid_to.is_none());
        assert_eq!(neg_edge.task_tag.as_deref(), Some("p1"));
        assert_eq!(neg_edge.discovered_by, "distill");
        // And it must not resurrect the killed edge.
        assert!(store
            .get_edge(old.edge_id.unwrap())
            .unwrap()
            .unwrap()
            .valid_to
            .is_some());
    }

    #[test]
    fn test_date_tokens() {
        // The leading bracket prefix is the RECORD date, not content — it is
        // ignored so same-day retractions stay killable.
        let dates = date_tokens("[2025-06-06] Confirmed the visit on 2025-06-10.");
        assert_eq!(dates.len(), 1);
        assert!(dates.contains("2025-06-10"));
        // Raw-turn prefix form is stripped too.
        let dates = date_tokens("[session_12 2025-06-03] user: see you on 2025-06-10");
        assert_eq!(dates.len(), 1);
        assert!(dates.contains("2025-06-10"));
        // Without a bracket prefix, a standalone date counts.
        assert!(date_tokens("moved to 2025-06-06 and 2025-06-10").len() == 2);
        // Invalid calendar dates and embedded digit runs are rejected.
        assert!(date_tokens("code 2025-13-45 and id 12025-06-01").is_empty());
        assert!(date_tokens("no dates here").is_empty());
        assert!(date_tokens("2025-06-0").is_empty());
    }

    #[test]
    fn test_containment_similarity() {
        let tok = |s: &str| crate::patterns::tokenize(s);
        // Keyword hint fully contained in the longer chunk text -> 1.0.
        assert_eq!(
            containment_similarity(
                &tok("buy groceries todo"),
                &tok("the user added buy groceries to their todo list")
            ),
            1.0
        );
        // Partial overlap.
        let sim = containment_similarity(&tok("groceries flight"), &tok("buy groceries todo"));
        assert!((sim - 0.5).abs() < 1e-9);
        // Disjoint / empty.
        assert_eq!(containment_similarity(&tok("vim"), &tok("emacs")), 0.0);
        assert_eq!(containment_similarity(&[], &tok("x")), 0.0);
    }

    // ─── Agent facts (v6) ─────────────────────────────────────────────────

    #[test]
    fn test_record_fact_idempotent_and_revive() {
        let store = CausalStore::open_in_memory().unwrap();
        let id1 = store
            .record_fact("preference", "TypeScript", "user", "agent", 0.8)
            .unwrap();
        // Re-recording the same (key, value, scope) is idempotent: same id.
        let id2 = store
            .record_fact("preference", "TypeScript", "user", "agent", 0.9)
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(store.list_facts(None, 10).unwrap().len(), 1);
        // Confidence refreshed by the second write.
        assert!((store.list_facts(None, 10).unwrap()[0].confidence - 0.9).abs() < 1e-9);

        // Invalidate, then re-record: the fact is revived (valid_to → NULL).
        assert!(store.invalidate_fact(id1).unwrap());
        assert!(store.list_facts(None, 10).unwrap().is_empty());
        let id3 = store
            .record_fact("preference", "TypeScript", "user", "agent", 0.85)
            .unwrap();
        assert_eq!(id3, id1);
        assert_eq!(store.list_facts(None, 10).unwrap().len(), 1);

        // Invalidating twice is a no-op.
        assert!(store.invalidate_fact(id1).unwrap());
        assert!(!store.invalidate_fact(id1).unwrap());
    }

    #[test]
    fn test_invalidate_other_facts_for_key() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_fact("package_manager", "npm", "user", "agent", 0.8)
            .unwrap();
        let new_id = store
            .record_fact("package_manager", "pnpm", "user", "agent", 0.9)
            .unwrap();
        // Different key is untouched.
        store
            .record_fact("preference", "TypeScript", "user", "agent", 0.8)
            .unwrap();

        let retired = store
            .invalidate_other_facts_for_key("package_manager", "user", "pnpm")
            .unwrap();
        assert_eq!(retired, 1);

        let facts = store.list_facts(None, 10).unwrap();
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().any(|f| f.id == new_id && f.value == "pnpm"));
        assert!(!facts.iter().any(|f| f.value == "npm"));

        // Scope isolation: an 'agent'-scoped npm fact survives a 'user' retire.
        store
            .record_fact("package_manager", "npm", "agent", "agent", 0.8)
            .unwrap();
        let retired = store
            .invalidate_other_facts_for_key("package_manager", "user", "pnpm")
            .unwrap();
        assert_eq!(retired, 0);
        assert!(store
            .list_facts(Some("agent"), 10)
            .unwrap()
            .iter()
            .any(|f| f.value == "npm"));
    }

    #[test]
    fn test_record_fact_replacing_atomic() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_fact("package_manager", "npm", "user", "agent", 0.8)
            .unwrap();
        store
            .record_fact("package_manager", "yarn", "user", "agent", 0.8)
            .unwrap();

        // One call: records the new value AND retires every other value
        // under the same key+scope.
        let (id, retired) = store
            .record_fact_replacing("package_manager", "pnpm", "user", "agent", 0.9)
            .unwrap();
        assert_eq!(retired, 2);

        let facts = store.list_facts(Some("user"), 10).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, id);
        assert_eq!(facts[0].value, "pnpm");

        // Re-running with the same value retires nothing (idempotent).
        let (_, retired) = store
            .record_fact_replacing("package_manager", "pnpm", "user", "agent", 0.9)
            .unwrap();
        assert_eq!(retired, 0);
    }

    #[test]
    fn test_search_facts_bm25_ranking_and_scope() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_fact("tech_stack", "Redis 7.2 for caching", "user", "agent", 0.8)
            .unwrap();
        store
            .record_fact(
                "preference",
                "TypeScript over JavaScript",
                "user",
                "agent",
                0.8,
            )
            .unwrap();
        store
            .record_fact(
                "tech_stack",
                "PostgreSQL 16 primary store",
                "session",
                "agent",
                0.8,
            )
            .unwrap();

        // Token-overlap ranking: "caching redis" hits the Redis fact first.
        let hits = store.search_facts_bm25("caching redis", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].value.contains("Redis"));

        // Scope filter: session-scoped query only sees the session fact.
        let hits = store
            .search_facts_bm25("database store", Some("session"), 5)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope, "session");

        // Invalidated facts are hidden from search.
        let id = store
            .record_fact("config", "legacy endpoint /api/v0", "user", "agent", 0.8)
            .unwrap();
        store.invalidate_fact(id).unwrap();
        assert!(store
            .search_facts_bm25("legacy endpoint", None, 5)
            .unwrap()
            .is_empty());

        // Empty query degrades to list (no panic, deterministic).
        let listed = store.search_facts_bm25("", None, 10).unwrap();
        assert_eq!(listed.len(), store.list_facts(None, 10).unwrap().len());
    }

    #[test]
    fn test_fact_embedding_semantic_search() {
        let store = CausalStore::open_in_memory().unwrap();
        let a = store
            .record_fact("preference", "TypeScript", "user", "agent", 0.8)
            .unwrap();
        let b = store
            .record_fact("tech_stack", "Redis 7.2", "user", "agent", 0.8)
            .unwrap();
        // Two orthogonal-ish toy vectors: a ≈ [1, 0], b ≈ [0, 1].
        store
            .put_fact_embedding(a, "test-model", &[1.0, 0.01])
            .unwrap();
        store
            .put_fact_embedding(b, "test-model", &[0.01, 1.0])
            .unwrap();

        let hits = store.search_facts_semantic(&[1.0, 0.0], None, 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0.id, a, "closest vector must rank first");
        assert!(hits[0].1 > hits[1].1);

        // embedding_model tracked for version management.
        let model: String = store
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT embedding_model FROM agent_facts WHERE id = ?1",
                    params![a],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(model, "test-model");
    }

    // ── Architecture hardening A1: file-backed store pragmas ─────────────
    // open() must enable WAL (concurrent readers + writer), a busy timeout
    // (contended writes wait instead of SQLITE_BUSY), and NORMAL synchronous
    // (WAL-safe, no per-write fsync). In-memory stores keep defaults.

    #[test]
    fn test_open_enables_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pragma.db");
        let store = CausalStore::open(&db_path).unwrap();

        store.with_conn(|conn| {
            let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            assert_eq!(mode, "wal", "file-backed store must run in WAL mode");
            // synchronous is returned as an integer: 0=OFF 1=NORMAL 2=FULL 3=EXTRA
            let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            assert_eq!(sync, 1, "WAL-safe NORMAL synchronous expected");
            let busy: i64 = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            assert!(busy >= 5000, "busy_timeout must be >= 5s, got {busy}");
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

        // In-memory stores must still work (defaults, no WAL requirement).
        let mem = CausalStore::open_in_memory().unwrap();
        mem.record_decision("d", "o", "caused", Some("t"), 0.5, "rule").unwrap();
    }

    // ── Architecture hardening A2: pooled connections under concurrency ───
    // With WAL + busy_timeout, parallel readers and writers on one store must
    // not deadlock or throw SQLITE_BUSY. The old single Mutex<Connection>
    // serialized everything; the pool checks connections out per method.

    #[test]
    fn test_concurrent_reads_and_writes_pool() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("conc.db");
        let store = CausalStore::open(&db_path).unwrap();
        let store = std::sync::Arc::new(store);

        // Seed a few edges so readers have something to scan.
        for i in 0..5 {
            store
                .record_decision(
                    &format!("seed decision {i}"),
                    &format!("seed outcome {i}"),
                    "caused",
                    Some("concurrency"),
                    0.5,
                    "rule",
                )
                .unwrap();
        }

        let mut handles = Vec::new();
        for t in 0..8 {
            let store = std::sync::Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    if t % 2 == 0 {
                        // Writer
                        store
                            .record_decision(
                                &format!("t{t} decision {i}"),
                                &format!("t{t} outcome {i}"),
                                "caused",
                                Some("concurrency"),
                                0.5,
                                "rule",
                            )
                            .unwrap();
                    } else {
                        // Reader
                        let r = store.search_causal(Some("concurrency"), None).unwrap();
                        assert!(!r.is_empty(), "reader must always see seeds");
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // All 8*25/2 writers landed.
        let n = store.count_edges().unwrap();
        assert_eq!(n, 5 + 4 * 25, "writers must all commit, got {n}");
    }

    // ── D1: Hebbian co-occurrence edges ────────────────────────────────
    // bump_cooccurrences creates pairs at 0.2 and reinforces them;
    // load_cooccurrences returns them; reopening the store and rebuilding
    // the graph loads them as CoOccurrence edges (learned associations
    // survive restarts).

    #[test]
    fn test_cooccurrence_learning_persists() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cooc.db");
        let store = CausalStore::open(&db).unwrap();
        store
            .record_decision("used Redis cache", "fast reads", "caused", Some("t"), 0.8, "rule")
            .unwrap();
        store
            .record_decision("added TTL", "no stale data", "caused", Some("t"), 0.8, "rule")
            .unwrap();

        // First co-activation creates the pair at 0.2.
        let d1: String = store
            .with_conn(|c| Ok(c.query_row("SELECT id FROM chunks WHERE text = ?1", rusqlite::params!["used Redis cache"], |r| r.get(0))?))
            .unwrap();
        let d2: String = store
            .with_conn(|c| Ok(c.query_row("SELECT id FROM chunks WHERE text = ?1", rusqlite::params!["added TTL"], |r| r.get(0))?))
            .unwrap();
        store.bump_cooccurrences(&[(d1.clone(), d2.clone())]).unwrap();
        store.bump_cooccurrences(&[(d1.clone(), d2.clone())]).unwrap();

        let loaded = store.load_cooccurrences().unwrap();
        assert_eq!(loaded.len(), 1, "one pair created");
        let (a, b, w) = &loaded[0];
        assert!(a == &d1 || a == &d2, "pair endpoint matches");
        assert!(*w > 0.2, "second bump reinforced the weight, got {w}");

        // Reopen (simulates restart) and rebuild the graph: the pair must
        // load as a CoOccurrence edge.
        drop(store);
        let store2 = CausalStore::open(&db).unwrap();
        let graph = crate::hippocampus::CausalGraph::from_store(&store2).unwrap();
        let mut found_cooc = false;
        for e in 0..graph.num_edges() {
            if graph.edge_relation_at(e) == crate::hippocampus::Relation::CoOccurrence {
                found_cooc = true;
            }
        }
        assert!(found_cooc, "reopened store loads the learned co-occurrence edge");
    }

// ── trace_cause_cross_session ──────────────────────────────────────────

fn link_tagged(
    store: &CausalStore,
    from: &str,
    to: &str,
    relation: &str,
    conf: f64,
    tag: &str,
) -> i64 {
    store
        .with_conn(|conn| {
            for text in [from, to] {
                conn.execute(
                    "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, 1000)",
                    params![format!("chunk:{text}"), text],
                )?;
            }
            conn.execute(
                "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                 VALUES (?1, ?2, ?3, ?4, 'rule', 1000, 1000, ?5)",
                params![
                    format!("chunk:{from}"),
                    format!("chunk:{to}"),
                    relation,
                    conf,
                    tag
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .unwrap()
}

#[test]
fn test_trace_cause_cross_session_basic() {
    let store = CausalStore::open_in_memory().unwrap();

    // Session "api": api deeper cause → api root → api mid → api outcome
    let _e_api_deep = link_tagged(
        &store,
        "api deeper cause",
        "api root",
        "caused",
        0.9,
        "api",
    );
    let _e_api_root = link_tagged(&store, "api root", "api mid", "caused", 0.85, "api");
    let _e_api_mid = link_tagged(&store, "api mid", "api outcome", "caused", 0.8, "api");

    // Session "auth": auth deeper cause → auth root → auth mid → auth outcome
    let _e_auth_deep = link_tagged(
        &store,
        "auth deeper cause",
        "auth root",
        "caused",
        0.9,
        "auth",
    );
    let _e_auth_root = link_tagged(&store, "auth root", "auth mid", "caused", 0.85, "auth");
    let _e_auth_mid = link_tagged(&store, "auth mid", "auth outcome", "caused", 0.8, "auth");

    // Meta bridge linking the two root causes across sessions
    store
        .upsert_meta_edge(
            "chunk:api root",
            "chunk:auth root",
            "similar_to",
            "root cause pattern",
            0.8,
        )
        .unwrap();

    // Query from api outcome; max_depth=2 stops the backward chain at api_root
    let results = store
        .trace_cause_cross_session("api outcome", 2, 0.1, 5)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected at least one cross-session result"
    );

    let cross = results
        .iter()
        .find(|r| r.segments.len() == 2)
        .expect("expected a 2-segment cross-session chain");

    // Segment 0: api session
    assert_eq!(cross.segments[0].task_tag.as_deref(), Some("api"));
    assert_eq!(cross.segments[0].hops.len(), 2);
    assert_eq!(cross.segments[0].hops[0].decision_text, "api mid");
    assert_eq!(cross.segments[0].hops[0].outcome_text, "api outcome");
    assert!((cross.segments[0].hops[0].confidence - 0.8).abs() < 1e-9);
    assert_eq!(cross.segments[0].hops[1].decision_text, "api root");
    assert_eq!(cross.segments[0].hops[1].outcome_text, "api mid");
    assert!((cross.segments[0].hops[1].confidence - 0.85).abs() < 1e-9);

    // Segment 1: auth session
    assert_eq!(cross.segments[1].task_tag.as_deref(), Some("auth"));
    assert_eq!(cross.segments[1].hops.len(), 1);
    assert_eq!(
        cross.segments[1].hops[0].decision_text,
        "auth deeper cause"
    );
    assert_eq!(cross.segments[1].hops[0].outcome_text, "auth root");
    assert!((cross.segments[1].hops[0].confidence - 0.9).abs() < 1e-9);

    // Overall confidence = 0.8 * 0.85 * 0.8 (meta bridge) * 0.9 = 0.4896
    let expected_overall = 0.8 * 0.85 * 0.8 * 0.9;
    assert!(
        (cross.overall_confidence - expected_overall).abs() < 1e-9,
        "overall confidence mismatch: got {}, expected {}",
        cross.overall_confidence,
        expected_overall
    );
}

#[test]
fn test_trace_cause_cross_session_single_session_fallback() {
    let store = CausalStore::open_in_memory().unwrap();

    // Only one session, no meta bridges
    let _e_root = link_tagged(&store, "root cause", "mid", "caused", 0.85, "single");
    let _e_mid = link_tagged(&store, "mid", "outcome", "caused", 0.8, "single");

    let results = store
        .trace_cause_cross_session("outcome", 2, 0.1, 5)
        .unwrap();

    assert_eq!(results.len(), 1, "expected exactly one single-session chain");
    assert_eq!(results[0].segments.len(), 1);
    assert_eq!(results[0].segments[0].task_tag.as_deref(), Some("single"));
    assert_eq!(results[0].segments[0].hops.len(), 2);
    assert_eq!(results[0].segments[0].hops[0].decision_text, "mid");
    assert_eq!(results[0].segments[0].hops[1].decision_text, "root cause");
}

#[test]
fn test_trace_cause_cross_session_no_seeds() {
    let store = CausalStore::open_in_memory().unwrap();
    let results = store
        .trace_cause_cross_session("nonexistent outcome", 3, 0.1, 5)
        .unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_trace_cause_cross_session_same_session_bridge_skipped() {
    let store = CausalStore::open_in_memory().unwrap();

    // Two chains in the SAME session
    let _e_a = link_tagged(&store, "root A", "mid A", "caused", 0.9, "same");
    let _e_b = link_tagged(&store, "mid A", "outcome A", "caused", 0.8, "same");
    let _e_c = link_tagged(&store, "root B", "mid B", "caused", 0.85, "same");
    let _e_d = link_tagged(&store, "mid B", "outcome B", "caused", 0.75, "same");

    // Meta bridge within the SAME session — should be skipped
    store
        .upsert_meta_edge(
            "chunk:root A",
            "chunk:root B",
            "similar_to",
            "same session pattern",
            0.8,
        )
        .unwrap();

    // Query from outcome A
    let results = store
        .trace_cause_cross_session("outcome A", 2, 0.1, 5)
        .unwrap();

    // Should only get single-session chains, no cross-session because bridge is same-session
    assert!(
        results.iter().all(|r| r.segments.len() == 1),
        "same-session bridges must be skipped"
    );
}

    #[test]
    fn test_retire_facts_by_hint_excludes_new_fact() {
        // Bug fix (bench-memory 2026-08-05): a transition fact like
        // "switched from almond milk to oat milk" mentions the OLD value in
        // its own text — the supersedes hint "almond milk" used to retire the
        // NEW fact itself, hiding it from retrieval. The new fact must never
        // be a retirement target (edge-layer parity: invalidate_superseded
        // excludes the new chunk).
        let store = CausalStore::open_in_memory().unwrap();

        // Old fact: prefers almond milk (fact layer).
        let _old_id = store
            .record_fact("preference", "user prefers almond milk in their coffee", "user", "distill", 0.8)
            .unwrap();

        // New fact mentions the old value (transition) and carries the hint.
        let new_id = store
            .record_fact(
                "preference",
                "user switched from almond milk to oat milk in their coffee",
                "user",
                "distill",
                0.8,
            )
            .unwrap();
        let retired = store.retire_facts_by_hint("preference", "user", "almond milk", Some(new_id)).unwrap();

        // The old fact (stored as a distilled preference fact) must be
        // retired — it shares "almond milk" without being the new fact.
        assert_eq!(retired, 1, "the old fact must be retired");
        // The NEW fact must still be retrievable.
        let hits = store.search_facts_bm25("oat milk", None, 10).unwrap();
        assert!(
            hits.iter().any(|h| h.value.contains("oat milk")),
            "the new fact must survive its own supersedes hint"
        );

        // Without the exclusion the new fact retires itself (the old buggy
        // behavior this guard fixes — demonstrated, not asserted away).
        let retired_self = store
            .retire_facts_by_hint("preference", "user", "almond milk", None)
            .unwrap();
        assert_eq!(retired_self, 1, "without the exclusion the new fact retires itself");
        let hits = store.search_facts_bm25("oat milk", None, 10).unwrap();
        assert!(!hits.iter().any(|h| h.value.contains("oat milk")));
    }

    // ─── v8: recurrence distill substrate (P1) ────────────────────────────

    #[test]
    fn test_log_session_turn_with_embedding_roundtrip() {
        let store = CausalStore::open_in_memory().unwrap();
        let emb = vec![0.1f32, 0.2, 0.3, 0.4];
        store
            .log_session_turn("s1:0", 7, 0, "user", "hello", 1_700_000_000, None, Some(&emb))
            .unwrap();
        store
            .log_session_turn("s1:1", 7, 1, "assistant", "hi", 1_700_000_001, None, None)
            .unwrap();
        // The session embedding rides on turn 0.
        assert_eq!(store.session_embedding(7).unwrap(), Some(emb));
        let turns = store.session_turns(7).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].1, "hello");
        assert_eq!(store.session_date(7).unwrap(), Some(1_700_000_000));
    }

    #[test]
    fn test_undistilled_and_sessions_with_embeddings() {
        let store = CausalStore::open_in_memory().unwrap();
        let emb = vec![0.5f32; 8];
        store
            .log_session_turn("a:0", 1, 0, "user", "a", 100, None, Some(&emb))
            .unwrap();
        store
            .log_session_turn("b:0", 2, 0, "user", "b", 200, None, Some(&emb))
            .unwrap();
        // Both pending, oldest first.
        assert_eq!(store.undistilled_session_ids(10).unwrap(), vec![1, 2]);
        store.mark_session_distilled(1, Some(999)).unwrap();
        // Only distilled sessions with an embedding are recurrence candidates.
        let cands = store.sessions_with_embeddings(10).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].0, 1);
        assert_eq!(store.undistilled_session_ids(10).unwrap(), vec![2]);
    }

    #[test]
    fn test_sessions_without_embeddings_skipped_from_candidates() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .log_session_turn("a:0", 1, 0, "user", "no embedding", 100, None, None)
            .unwrap();
        store.mark_session_distilled(1, Some(999)).unwrap();
        assert!(store.sessions_with_embeddings(10).unwrap().is_empty());
    }

    // ─── v8: reversible consolidation (P3) ────────────────────────────────

    #[test]
    fn test_supersede_marks_and_restore_revives() {
        use crate::distill::{ItemKind, MemoryItem};
        let store = CausalStore::open_in_memory().unwrap();
        // Old lesson: prefers coffee.
        let old = MemoryItem {
            kind: ItemKind::Preference,
            text: "user prefers coffee".to_string(),
            date: Some("2026-07-01".to_string()),
            supersedes: None,
            causal_relation: None,
            decision: None,
        };
        let old_out = store.record_distilled(&old, None).unwrap();
        assert!(!old_out.duplicate);
        let old_edge = old_out.edge_id.unwrap();

        // New lesson supersedes it: switched to tea.
        let new = MemoryItem {
            kind: ItemKind::Preference,
            text: "user switched from coffee to tea".to_string(),
            date: Some("2026-08-01".to_string()),
            supersedes: Some("prefers coffee".to_string()),
            causal_relation: None,
            decision: None,
        };
        let new_out = store.record_distilled(&new, None).unwrap();
        assert_eq!(new_out.invalidated_edge_ids, vec![old_edge]);

        // Marked, not deleted: superseded_by records WHICH edge killed it,
        // and it is invisible to search while superseded.
        let e = store.get_edge(old_edge).unwrap().unwrap();
        assert!(e.valid_to.is_some());
        assert_eq!(e.superseded_by, new_out.edge_id);
        let hits = store.search_causal_bm25(None, "coffee", 10).unwrap();
        assert!(hits.iter().all(|h| h.edge_id != old_edge));

        // Reversible: later evidence proves the old memory right.
        assert!(store.restore_edge(old_edge).unwrap());
        let e = store.get_edge(old_edge).unwrap().unwrap();
        assert!(e.valid_to.is_none());
        assert!(e.superseded_by.is_none());
        let hits = store.search_causal_bm25(None, "coffee", 10).unwrap();
        assert!(hits.iter().any(|h| h.edge_id == old_edge));
        // A second restore is a no-op.
        assert!(!store.restore_edge(old_edge).unwrap());
    }

    #[test]
    fn test_superseded_edges_audit_view() {
        use crate::distill::{ItemKind, MemoryItem};
        let store = CausalStore::open_in_memory().unwrap();
        let old = MemoryItem {
            kind: ItemKind::Fact,
            text: "server is node A".to_string(),
            date: Some("2026-07-01".to_string()),
            supersedes: None,
            causal_relation: None,
            decision: None,
        };
        let new = MemoryItem {
            kind: ItemKind::Fact,
            text: "server migrated to node B".to_string(),
            date: Some("2026-08-01".to_string()),
            supersedes: Some("server node A".to_string()),
            causal_relation: None,
            decision: None,
        };
        store.record_distilled(&old, None).unwrap();
        store.record_distilled(&new, None).unwrap();
        let superseded = store.superseded_edges(10).unwrap();
        assert_eq!(superseded.len(), 1);
        assert!(superseded[0].superseded_by.is_some());
        assert!(superseded[0].valid_to.is_some());
    }

#[test]
fn test_entity_boost_amplifies_semantic_scores() {
    let store = CausalStore::open_in_memory().unwrap();
    for (dec, out) in [
        ("went camping with Melanie at Yosemite", "loved it"),
        ("Switched to oat milk", "tastes fine"),
    ] {
        store
            .record_decision(dec, out, "caused", None, 0.8, "rule")
            .unwrap();
    }
    // Identical synthetic embeddings for both edges: plain semantic ties;
    // only the entity boost can separate them.
    let edges = store.all_valid_edges().unwrap();
    for e in &edges {
        store.put_embedding(e.edge_id, "test", &[1.0f32, 0.0, 0.0, 0.0]).unwrap();
    }

    let qv = [1.0f32, 0.0, 0.0, 0.0];
    let boosted = store
        .search_causal_semantic_entity_boosted(&qv, "Where has Melanie camped?", None, 10)
        .unwrap();
    assert_eq!(boosted.len(), 2);
    assert!(
        boosted[0].0.decision_text.contains("Melanie"),
        "entity-boosted edge must win the tie: {:?}",
        boosted[0].0.decision_text
    );
    assert!(boosted[0].1 > boosted[1].1);

    // No-entity query → boost 1.0 → identical to plain semantic (tie).
    let plain = store.search_causal_semantic(&qv, None, 10).unwrap();
    assert_eq!(plain.len(), 2);
}

/// Test helper: insert a raw chunk-to-chunk causal edge, creating chunks on
/// demand (text-derived ids). Defined here for tests that build clean
/// multi-hop graphs; `record_decision` creates fresh chunk ids per call.
#[allow(dead_code)]
fn hop_link(store: &CausalStore, from: &str, to: &str, conf: f64) -> i64 {
    store
        .with_conn(|conn| {
            for text in [from, to] {
                conn.execute(
                    "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, 1000)",
                    params![format!("chunk:{text}"), text],
                )?;
            }
            conn.execute(
                "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
                 VALUES (?1, ?2, 'caused', ?3, 'rule', 1000, 1000)",
                params![format!("chunk:{from}"), format!("chunk:{to}"), conf],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .unwrap()
}

#[test]
fn test_search_causal_hop_expands_chain() {
    let store = CausalStore::open_in_memory().unwrap();
    // Chunk ids are text-derived (hop_link): shared text = shared node, so
    // adjacency is real — like turn chunks in the LoCoMo harness.
    let id_a = hop_link(&store, "went camping with Melanie at Yosemite", "Melanie loved the trip", 0.9);
    hop_link(&store, "Melanie loved the trip", "booked another camping trip", 0.8);
    hop_link(&store, "booked another camping trip", "flew to Banff", 0.7);
    // 无关边,不应出现在 hop 结果里
    hop_link(&store, "Switched to oat milk", "tastes fine", 0.8);

    let hop = store
        .search_causal_hop("Where has Melanie camped?", &[id_a], 10)
        .unwrap();
    // 1-hop: 共享 "Melanie loved the trip" 节点的边在结果里;无关边不在。
    assert!(!hop.is_empty());
    assert!(hop.iter().any(|e| e.outcome_text.contains("booked another")));
    assert!(!hop.iter().any(|e| e.outcome_text.contains("tastes fine")));

    // 2-hop 需要 distill 边:造一条 2 跳可达的蒸馏边,验证精度闸门
    // (banff 与问题无共享 token → 被过滤)。
    store.with_conn(|conn| {
        conn.execute(
            "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
             VALUES ('chunk:booked another camping trip', 'chunk:flew to Banff', 'caused', 0.9, 'distill', 1000, 1000)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let gated = store
        .search_causal_hop("Where has Melanie camped?", &[id_a], 10)
        .unwrap();
    assert!(!gated.iter().any(|e| e.outcome_text.contains("Banff")));
}

#[test]
fn test_record_decision_reuses_identical_text_chunk() {
    let store = CausalStore::open_in_memory().unwrap();
    let id1 = store
        .record_decision("switched to oat milk", "tastes fine", "caused", None, 0.8, "rule")
        .unwrap();
    let id2 = store
        .record_decision("switched to oat milk", "tastes fine", "caused", None, 0.9, "rule")
        .unwrap();
    // v9: identical text reuses the SAME chunk ids — one fact, one node.
    assert_eq!(id1, id2, "identical decision text must reuse the chunk id");
    // Both edges still exist (reuse is at the chunk level, not dedup).
    let n = store.all_valid_edges().unwrap().len();
    assert_eq!(n, 2);
    // sparse_code persisted on the reused chunk.
    let code: String = store
        .with_conn(|c| {
            c.query_row("SELECT sparse_code FROM chunks WHERE id = ?1", params![id1], |r| r.get(0))
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    assert_eq!(code.len(), 32, "128-bit simhash hex");
}

#[test]
fn test_q_value_persists_and_reinforces() {
    let store = CausalStore::open_in_memory().unwrap();
    store
        .record_decision("deployed without tests", "prod incident", "caused", None, 0.9, "user_feedback")
        .unwrap();
    store
        .record_decision("added integration tests", "no more prod incidents", "caused", None, 0.7, "rule")
        .unwrap();

    // Baseline: all chunks start at default 0.5.
    let q0: f64 = store
        .with_conn(|c| {
            c.query_row("SELECT q_value FROM chunks LIMIT 1", [], |r| r.get(0))
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    assert_eq!(q0, 0.5);

    // Consolidate with a low protect threshold so the high-confidence edge is
    // replayed → its endpoint chunks get Bellman reward (Q > 0.5).
    use crate::consolidate::{consolidate, ConsolidateConfig};
    let report = consolidate(
        &store,
        &ConsolidateConfig {
            replay_protect_score: 0.5,
            ..ConsolidateConfig::default()
        },
        false,
        1000,
    )
    .unwrap();
    assert!(
        report.q_updates > 0,
        "protected edges must trigger Q reinforcement (q_updates={})",
        report.q_updates
    );

    // Persisted: at least one chunk's Q value rose above the 0.5 default.
    let max_q: f64 = store
        .with_conn(|c| {
            c.query_row("SELECT MAX(q_value) FROM chunks", [], |r| r.get(0))
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .unwrap();
    assert!(max_q > 0.5, "Bellman reward must persist to chunks.q_value");

    // The graph loads the persisted Q (from_store reads the column).
    let graph = crate::hippocampus::CausalGraph::from_store(&store).unwrap();
    let max_q = (0..graph.num_nodes())
        .map(|i| graph.node_q_value(i))
        .fold(0.0f32, f32::max);
    assert!(max_q > 0.5, "graph must load persisted Q values");
}
