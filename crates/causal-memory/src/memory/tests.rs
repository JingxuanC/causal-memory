//! Memory facade tests (moved from the CLI server module during the facade
//! extraction — they cover the orchestration helpers and the LLM-free paths
//! of the counterfactual/reconstruct ops).

use super::format::*;
use super::output::*;
use super::{Memory, RRF_K};
use crate::store::CausalEntry;

#[cfg(test)]
mod tests {
    use super::*;

    // ─── P5: layered loading + token budget ───────────────────────────────

    #[test]
    fn test_format_entry_layered_l0_compact() {
        let entry = CausalEntry {
            edge_id: 1, decision_id: "d1".into(),
            decision_text: "used Redis for caching".into(),
            outcome_id: "o1".into(), outcome_text: "cache stampede".into(),
            relation: "caused".into(), confidence: 0.9,
            task_tag: Some("caching".into()), event_time: 0, valid_to: None,
            access_count: 0, last_accessed_at: None,
            discovered_by: "agent".into(), discovered_at: 0, outcome_polarity: None,
            superseded_by: None,
        };
        let (l0, t0) = format_entry_layered(&entry, 1, "l0");
        let (l2, t2) = format_entry_layered(&entry, 1, "l2");
        assert!(t0 < t2);
        assert!(l0.contains("→(caused)→"));
        assert!(l2.contains("confidence"));
    }

    #[test]
    fn test_token_budget_truncates() {
        let mut b = TokenBudget::new(100);
        assert!(b.try_spend(60));
        assert!(b.try_spend(40));
        assert!(!b.try_spend(1));
    }

    #[test]
    fn test_token_budget_unlimited() {
        let mut b = TokenBudget::new(0);
        assert!(b.try_spend(999999));
    }

    #[test]
    fn test_rrf_fuse_single_list_order() {
        // One empty list: the other list's order is preserved, scores by rank.
        let a = vec!["fact:1".to_string(), "fact:2".to_string()];
        let fused = rrf_fuse(&a, &[]);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "fact:1");
        assert!((fused[0].1 - 1.0 / (RRF_K + 1.0)).abs() < 1e-12);
        assert!((fused[1].1 - 1.0 / (RRF_K + 2.0)).abs() < 1e-12);
    }

    #[test]
    fn test_rrf_fuse_production_shape_interleaving() {
        // Production keys are layer-namespaced ("fact:{id}" / "causal:{id}"),
        // so no key ever appears in both lists: fusion is rank-interleaving.
        // Rank-1 items from each list tie and surface first, in first-seen
        // order (facts list is passed first).
        let a = vec!["fact:1".to_string(), "fact:2".to_string()];
        let b = vec!["causal:9".to_string(), "causal:2".to_string()];
        let fused = rrf_fuse(&a, &b);
        assert_eq!(fused.len(), 4);
        assert_eq!(fused[0].0, "fact:1"); // facts rank 1
        assert_eq!(fused[1].0, "causal:9"); // causal rank 1 (tied score)
        assert_eq!(fused[2].0, "fact:2");
        assert_eq!(fused[3].0, "causal:2");
    }

    #[test]
    fn test_rrf_fuse_shared_key_accumulates() {
        // The helper itself is generic: a key present in BOTH lists
        // accumulates both list scores and outranks single-list rank-1.
        // (Unreachable with layer-namespaced production keys; the fusion
        // helper is kept honest about its own semantics.)
        let a = vec!["x".to_string(), "shared".to_string()];
        let b = vec!["y".to_string(), "shared".to_string()];
        let fused = rrf_fuse(&a, &b);
        assert_eq!(fused[0].0, "shared");
        let expected = 2.0 / (RRF_K + 2.0);
        assert!((fused[0].1 - expected).abs() < 1e-12);
    }

    #[test]
    fn test_rrf_fuse_disjoint_union() {
        let a = vec!["fact:1".to_string()];
        let b = vec!["causal:9".to_string(), "causal:2".to_string()];
        let fused = rrf_fuse(&a, &b);
        // Union of both lists, rank-1 items first (tie → first-seen).
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, "fact:1");
        assert_eq!(fused[1].0, "causal:9");
        assert_eq!(fused[2].0, "causal:2");
    }

    #[test]
    fn test_chain_label_stored_polarity() {
        // Stored polarity wins over the text, whatever the text says.
        assert_eq!(
            chain_label(Some("negative"), "一切正常", false),
            "⚠️ DANGER"
        );
        assert_eq!(
            chain_label(Some("positive"), "deadlock occurred", false),
            "✅ SAFE"
        );
        // mixed gets its own WARNING, never forced into SAFE/DANGER — even
        // when the text heuristic would call it a success.
        assert_eq!(
            chain_label(
                Some("mixed"),
                "deadlock under load; fixed by switching to channels",
                false
            ),
            "⚠️ WARNING (mixed outcome)"
        );
        assert_eq!(
            chain_label(Some("mixed"), "deadlock under load; fixed later", true),
            "⚠️ WARNING (mixed outcome)"
        );
        // neutral is never SAFE/DANGER.
        assert_eq!(
            chain_label(Some("neutral"), "deploy finished", false),
            "ℹ️ UNKNOWN"
        );
    }

    #[test]
    fn test_chain_label_prevented_downgrade() {
        let downgraded =
            "ℹ️ UNKNOWN (failure outcome, but a prevented edge on this path blocked it before)";
        // Stored negative + a prevented edge on the path → UNKNOWN.
        assert_eq!(chain_label(Some("negative"), "x", true), downgraded);
        // Heuristic failure + prevented → UNKNOWN (pre-v4 behavior preserved).
        assert_eq!(chain_label(None, "service crashed", true), downgraded);
        // positive/neutral are unaffected by prevented.
        assert_eq!(chain_label(Some("positive"), "x", true), "✅ SAFE");
        assert_eq!(chain_label(Some("neutral"), "x", true), "ℹ️ UNKNOWN");
    }

    #[test]
    fn test_chain_label_heuristic_fallback() {
        // NULL stored polarity → identical to the pre-v4 text heuristic.
        assert_eq!(
            chain_label(None, "deadlock — holder crashed", false),
            "⚠️ DANGER"
        );
        assert_eq!(
            chain_label(None, "successfully fixed race", false),
            "✅ SAFE"
        );
        assert_eq!(chain_label(None, "deploy finished", false), "ℹ️ UNKNOWN");
        // The documented heuristic quirk is preserved on the fallback path:
        // failure+success in one text counts as success (use chain_label with
        // a stored 'mixed' to get the WARNING instead).
        assert_eq!(
            chain_label(
                None,
                "Deadlock under concurrent load; fixed by switching to channel-based ownership",
                false
            ),
            "✅ SAFE"
        );
    }

    #[test]
    fn test_stratified_summary_simpson_warning() {
        // Same action family, opposite outcomes per stratum: 2 negative in
        // caching, 3 positive in auth → pooled leans positive while the
        // caching stratum is purely negative → Simpson warning.
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (Some("caching".to_string()), "negative"),
            (Some("auth".to_string()), "positive"),
            (Some("auth".to_string()), "positive"),
            (Some("auth".to_string()), "positive"),
        ];
        let s = stratified_summary(&chains, Some("caching"));
        assert!(s.contains("pooled (n=5): 3 positive / 2 negative / 0 other → positive"));
        assert!(s.contains("task_tag=caching (n=2): 0 positive / 2 negative / 0 other → negative"));
        assert!(s.contains("other strata (n=3)"));
        assert!(
            s.contains(
                "Simpson's paradox: pooled result is positive but within task_tag=caching it is negative"
            ),
            "warning must name both directions: {s}"
        );
    }

    #[test]
    fn test_stratified_summary_single_stratum_no_warning() {
        // All chains in one stratum → nothing to confound, no warning.
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (Some("caching".to_string()), "negative"),
        ];
        let s = stratified_summary(&chains, Some("caching"));
        assert!(s.contains("task_tag=caching (n=2)"));
        assert!(s.contains("other strata (n=0)"));
        assert!(!s.contains("Simpson"));

        // Same-direction strata → no warning either.
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (Some("auth".to_string()), "negative"),
        ];
        let s = stratified_summary(&chains, Some("caching"));
        assert!(!s.contains("Simpson"));

        // Empty input → empty block.
        assert!(stratified_summary(&[], Some("caching")).is_empty());
    }

    #[test]
    fn test_modal_stratum() {
        let chains = vec![
            (Some("caching".to_string()), "negative"),
            (None, "positive"),
            (Some("auth".to_string()), "positive"),
            (Some("caching".to_string()), "positive"),
        ];
        assert_eq!(modal_stratum(&chains).as_deref(), Some("caching"));
        // All untagged → no reference stratum.
        let chains = vec![(None, "positive"), (None, "negative")];
        assert_eq!(modal_stratum(&chains), None);
    }

    // ── counterfactual_query ─────────────────────────────────────────────

    #[test]
    fn test_counterfactual_verdict() {
        let fav_a = (
            CfDist {
                positive: 2,
                ..Default::default()
            },
            CfDist {
                negative: 2,
                ..Default::default()
            },
        );
        assert!(counterfactual_verdict(&fav_a.0, &fav_a.1).contains("favors A"));
        assert!(counterfactual_verdict(&fav_a.1, &fav_a.0).contains("favors B"));
        // Both empty / one empty → insufficient.
        let empty = CfDist::default();
        assert!(counterfactual_verdict(&empty, &empty).contains("no recorded episodes for either"));
        assert!(counterfactual_verdict(&empty, &fav_a.0).contains("option A"));
        assert!(counterfactual_verdict(&fav_a.0, &empty).contains("option B"));
        // Equal net scores → honestly indistinguishable. mixed counts -0.5.
        let tie_a = CfDist {
            positive: 1,
            mixed: 2,
            ..Default::default()
        }; // 1 - 1 = 0
        let tie_b = CfDist {
            positive: 1,
            negative: 1,
            ..Default::default()
        }; // 0
        assert!(
            counterfactual_verdict(&tie_a, &tie_b).contains("insufficient evidence to distinguish")
        );
    }

    /// Build a memory over an in-memory store with two edge families:
    /// "redis mutex …" edges (stored negative) and "channel …" (positive).
    fn counterfactual_memory() -> Memory {
        let store = crate::store::CausalStore::open_in_memory().unwrap();
        for (i, (dec, out, pol)) in [
            (
                "used redis mutex for cache",
                "deadlock under load",
                "negative",
            ),
            ("used redis mutex for queue", "deadlock again", "negative"),
            (
                "switched to channel ownership",
                "race fixed, all tests pass",
                "positive",
            ),
        ]
        .iter()
        .enumerate()
        {
            store
                .record_decision_full(
                    dec,
                    out,
                    "caused",
                    Some("concurrency"),
                    0.8,
                    "rule",
                    1000 + i as i64,
                    Some(pol),
                )
                .unwrap();
        }
        Memory::new(store)
    }

    #[test]
    fn test_counterfactual_inner_bm25_path() {
        let memory = counterfactual_memory();
        let out = memory.counterfactual_inner("redis mutex", "channel", None, 5);
        assert!(out.starts_with(
            "⚠️ contrastive/empirical counterfactual over recorded alternatives — not a Pearl Rung-3 SCM counterfactual"
        ));
        assert!(out.contains("[bm25]"), "no embedder → BM25 tag: {out}");
        assert!(
            out.contains("A. \"redis mutex\" (n=2): 0 positive / 2 negative"),
            "{out}"
        );
        assert!(
            out.contains("B. \"channel\" (n=1): 1 positive / 0 negative"),
            "{out}"
        );
        assert!(out.contains("recorded evidence favors B"), "{out}");

        // One side without recorded episodes → insufficient.
        let out = memory.counterfactual_inner("redis mutex", "nonexistent option", None, 5);
        assert!(
            out.contains("insufficient evidence: no recorded episodes matching option B"),
            "{out}"
        );

        // task_tag filter excludes everything → both sides empty.
        let out = memory.counterfactual_inner("redis mutex", "channel", Some("other-tag"), 5);
        assert!(
            out.contains("no recorded episodes for either option"),
            "{out}"
        );
    }

    // ── reconstruct_lesson ───────────────────────────────────────────────

    #[test]
    fn test_edge_stub_format_and_cap() {
        let store = crate::store::CausalStore::open_in_memory().unwrap();
        let long = "x".repeat(500);
        store
            .record_decision_full(
                &long,
                &long,
                "caused",
                None,
                0.8,
                "rule",
                1000,
                Some("mixed"),
            )
            .unwrap();
        let edge = store.get_edge(1).unwrap().unwrap();
        let stub = edge_stub(&edge);
        assert!(
            stub.chars().count() <= 120,
            "stub over budget: {}",
            stub.chars().count()
        );
        assert!(
            stub.starts_with("#1 caused conf=0.80 pol=mixed | \""),
            "{stub}"
        );
        assert!(stub.contains('…'), "long texts are truncated: {stub}");
    }

    #[test]
    fn test_reconstruction_agreement() {
        let a = "use channels to avoid deadlock in shared state".to_string();
        assert_eq!(reconstruction_agreement(&[a.clone(), a.clone()]), 1.0);
        // Completely disjoint token sets → 0.0.
        let b = "xyz qrs".to_string();
        assert_eq!(reconstruction_agreement(&[a.clone(), b]), 0.0);
        // Partial overlap lands strictly between.
        let c = "use channels to fix the parser".to_string();
        let mid = reconstruction_agreement(&[a.clone(), c]);
        assert!(mid > 0.0 && mid < 1.0, "got {mid}");
        // Fewer than two samples → vacuous 1.0.
        assert_eq!(reconstruction_agreement(&[a]), 1.0);
        assert_eq!(reconstruction_agreement(&[]), 1.0);
    }

    #[test]
    fn test_reconstruct_lesson_degraded_without_llm() {
        let memory = counterfactual_memory();
        let out = memory.reconstruct_lesson_inner("redis mutex", 20, 0, None);
        assert!(
            out.contains("[bm25] Causal subgraph for \"redis mutex\""),
            "{out}"
        );
        // Stubs of the seeded edges are present, capped format.
        assert!(out.contains("conf=0.80 pol=negative"), "{out}");
        assert!(
            out.contains("(configure CAUSAL_MEMORY_LLM_* for narrative reconstruction)"),
            "{out}"
        );
        assert!(
            !out.contains("Reconstructed lesson"),
            "no LLM → no narrative: {out}"
        );

        // No matching history at all.
        let out = memory.reconstruct_lesson_inner("totally unknown topic", 20, 0, None);
        assert!(out.contains("📭 No recorded causal context"), "{out}");
    }

    // ─── AMC backend: raw write path + structured retrieval core ──────────

    #[test]
    fn test_remember_raw_turns_writes_searchable_pool() {
        let memory = Memory::new(crate::store::CausalStore::open_in_memory().unwrap());
        let turns = vec![
            ("alice".to_string(), "we moved the build to bazel".to_string()),
            ("bob".to_string(), "bazel cut our build time in half".to_string()),
        ];
        let written = memory.remember_raw_turns(&turns, "s7");
        assert_eq!(written, 2);
        let (hits, _mode) = memory.search_memory_entries("bazel build", None, None, 5);
        assert!(!hits.is_empty(), "raw turns must be retrievable");
        let all: String = hits.iter().map(|h| h.content.as_str()).collect();
        assert!(all.contains("bazel"), "hit content: {all}");
        // Layer-namespaced keys, ranked, fused score present.
        assert!(hits[0].key.starts_with("causal:") || hits[0].key.starts_with("fact:"));
        assert_eq!(hits[0].rank, 1);
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn test_search_memory_entries_matches_text_tool_layers() {
        let memory = counterfactual_memory();
        // Same query through both presentations: the structured core must
        // surface the same memories the text tool reports.
        let text = memory.search_memory("redis mutex", None, None, Some(10));
        let (hits, _mode) = memory.search_memory_entries("redis mutex", None, None, 10);
        assert!(!hits.is_empty(), "seeded edges must surface");
        assert!(text.contains("redis"), "text tool: {text}");
        // Every structured hit's decision text appears in the text output.
        for h in &hits {
            let needle = h.content.split('"').nth(1).unwrap_or_default();
            if !needle.is_empty() && h.key.starts_with("causal:") {
                assert!(
                    text.contains(&needle[..needle.len().min(20)]),
                    "text tool missing {} from {}",
                    needle,
                    h.key
                );
            }
        }
    }
}
