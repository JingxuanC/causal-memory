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
            edge_id: 1,
            decision_id: "d1".into(),
            decision_text: "used Redis for caching".into(),
            outcome_id: "o1".into(),
            outcome_text: "cache stampede".into(),
            relation: "caused".into(),
            confidence: 0.9,
            task_tag: Some("caching".into()),
            event_time: 0,
            valid_to: None,
            access_count: 0,
            last_accessed_at: None,
            discovered_by: "agent".into(),
            discovered_at: 0,
            outcome_polarity: None,
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
    fn test_format_activation_layered_levels() {
        let text = "a".repeat(200);
        let (l0, t0) = format_activation_layered(&text, 0.8, 1, "l0");
        let (l1, t1) = format_activation_layered(&text, 0.8, 1, "l1");
        let (l2, t2) = format_activation_layered(&text, 0.8, 1, "l2");
        // Cost rises with detail; l0/l1 truncate, l2 keeps the full text.
        assert!(t0 < t1 && t1 < t2);
        assert!(l0.contains("[80%+]"));
        assert!(l0.len() < l1.len());
        assert!(l2.contains(&text));
        // Negative activation keeps its sign at every level.
        assert!(format_activation_layered("x", -0.5, 1, "l1")
            .0
            .contains("[50%-]"));
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
            (
                "alice".to_string(),
                "we moved the build to bazel".to_string(),
            ),
            (
                "bob".to_string(),
                "bazel cut our build time in half".to_string(),
            ),
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
        let text = memory.search_memory("redis mutex", None, None, Some(10), None, None, None);
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

    // ─── Phase A: record_fact marks the graph dirty ──────────────────────

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: memory construction must succeed or the test is meaningless"
    )]
    fn test_record_fact_marks_graph_dirty_for_rebuild() {
        let memory = Memory::open_in_memory().expect("memory");
        // 1 causal write + 5 fact writes ≥ GRAPH_REBUILD_WRITES (5): the
        // next hippocampus query must rebuild and see the fresh facts.
        memory.record_decision("cfg", "zsh plugins load", "caused", "shell", None);
        for i in 0..5 {
            memory.record_fact(
                &format!("pref_{i}"),
                &format!("user prefers zsh setup variant {i}"),
                Some("user"),
                None,
                None,
            );
        }
        let out = memory.search_causal(None, Some("prefers zsh setup"), Some(5), None, None, None);
        assert!(
            out.starts_with("[hippocampus"),
            "fresh fact must be reachable via the graph path, got: {out}"
        );
        assert!(
            out.contains("user prefers zsh setup"),
            "fact text must appear in the activation results: {out}"
        );
    }

    // ─── Phase B: search_memory served by the unified spread engine ─────

    /// The exact staleness the MCP e2e caught: 4 writes < the lazy rebuild
    /// threshold, so the graph still predates the facts — the engine must
    /// detect the seed miss and rebuild, never serve a stale graph.
    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: memory construction must succeed or the test is meaningless"
    )]
    fn test_unified_engine_rebuilds_on_stale_seed_miss() {
        let memory = Memory::open_in_memory().expect("memory");
        // Startup graph is EMPTY; these 4 writes stay under the lazy
        // threshold (5), no rebuild fires before the query.
        memory.record_decision(
            "used global lock for cache",
            "deadlock error under load",
            "caused",
            "locking",
            None,
        );
        memory.record_decision(
            "ran backup migration",
            "backup completed",
            "caused",
            "backup",
            None,
        );
        memory.record_fact("tech_stack", "Redis 7.2", Some("user"), None, None);
        memory.record_fact("tech_stack", "Redis 8.0", Some("user"), None, Some(true));

        let (hits, mode) = memory.search_memory_entries("redis cache", None, None, 10);
        assert_eq!(mode, "spread", "engine must serve, got: {hits:?}");
        assert!(
            hits.iter()
                .any(|h| h.key.starts_with("fact:") && h.content.contains("Redis 8.0")),
            "fresh fact must surface despite the stale graph: {hits:?}"
        );
        assert!(
            !hits.iter().any(|h| h.content.contains("Redis 7.2")),
            "retired fact must not surface: {hits:?}"
        );
    }

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: memory construction must succeed or the test is meaningless"
    )]
    fn test_search_memory_uses_unified_spread_engine() {
        let memory = Memory::open_in_memory().expect("memory");
        memory.record_decision(
            "rewrote module in TypeScript",
            "compile errors dropped",
            "caused",
            "rust",
            None,
        );
        memory.record_fact(
            "editor_preference",
            "user prefers TypeScript for module rewrites",
            Some("user"),
            None,
            None,
        );
        // 2 real writes < GRAPH_REBUILD_WRITES(5): pad with 3 more writes
        // so the lazy rebuild fires and the engine sees fresh nodes.
        for i in 0..3 {
            memory.record_fact(
                &format!("pad_{i}"),
                &format!("padding entry {i}"),
                Some("user"),
                None,
                None,
            );
        }

        // Structured core: one spread, typed hits, "spread" mode.
        let (hits, mode) = memory.search_memory_entries("TypeScript module", None, None, 10);
        assert_eq!(mode, "spread", "unified engine must serve this query");
        assert!(
            hits.iter().any(|h| h.key.starts_with("fact:")),
            "fact hits: {hits:?}"
        );
        assert!(
            hits.iter().any(|h| h.key.starts_with("causal:")),
            "causal hits: {hits:?}"
        );

        // Text tool: same engine, grouped display.
        let text = memory.search_memory("TypeScript module", None, None, None, None, None, None);
        assert!(text.starts_with("[unified/spread]"), "{text}");
        assert!(text.contains("editor_preference"), "{text}");
        assert!(text.contains("Causal lessons"), "{text}");
    }

    // ─── Phase C: write-path patches — instant visibility + differential ──

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: temp file and memory construction must succeed"
    )]
    fn test_write_then_query_patched_equals_rebuilt() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("phase_c.db");

        // Instance 1: write, then query IMMEDIATELY (2 writes < the lazy
        // threshold of 5, well inside the 30s window → no rebuild may have
        // run; the visibility must come from the write-path patch).
        let m1 = Memory::open(&db).expect("memory 1");
        m1.record_decision(
            "rewrote module in TypeScript",
            "compile errors dropped",
            "caused",
            "rust",
            None,
        );
        m1.record_fact(
            "editor_preference",
            "user prefers TypeScript for module rewrites",
            Some("user"),
            None,
            None,
        );
        let writes_before = m1.graph_writes.load(Ordering::Relaxed);

        let (hits1, mode) = m1.search_memory_entries("TypeScript module", None, None, 10);
        assert_eq!(mode, "spread");
        assert!(
            hits1.iter().any(|h| h.key.starts_with("fact:")),
            "patched fact must surface instantly: {hits1:?}"
        );
        assert!(
            hits1.iter().any(|h| h.key.starts_with("causal:")),
            "patched edge must surface instantly: {hits1:?}"
        );
        // No rebuild consumed the dirty counter: the 2 writes are still
        // pending (below threshold), so visibility came from patches.
        assert_eq!(
            m1.graph_writes.load(Ordering::Relaxed),
            writes_before,
            "no lazy rebuild may have fired (writes pending: {})",
            writes_before
        );

        // Differential: a SECOND instance on the same file does a full
        // from_store at startup — its results must match the patched view.
        let m2 = Memory::open(&db).expect("memory 2");
        let (hits2, _) = m2.search_memory_entries("TypeScript module", None, None, 10);
        let keys1: std::collections::HashSet<String> =
            hits1.iter().map(|h| h.key.clone()).collect();
        let keys2: std::collections::HashSet<String> =
            hits2.iter().map(|h| h.key.clone()).collect();
        assert_eq!(
            keys1, keys2,
            "patched state must equal fully-rebuilt state (differential assertion)"
        );
    }
}

// ─── Production-path sink of the bench optimizations ──────────────

/// multi_pass now runs the episode quota + weighted top-N expansion
/// (the LME dilution cut). Production chunk ids are flat (no
/// '::session::' segments), so session-keyed expansion is a no-op on
/// agent-native stores by design (comment on session_key: "production
/// data with flat chunk ids simply never expands sessions") — this
/// test pins that the QUOTA half applies and that session-structured
/// stores (harness convention) get the whitelist behavior.
#[test]
#[allow(
    clippy::expect_used,
    reason = "test invariant: memory construction must succeed"
)]
fn multi_pass_sinks_bench_optimizations() {
    let memory = Memory::open_in_memory().expect("memory");
    for turn in 0..6 {
        memory.record_decision(
            &format!("bought plants number {turn} for the garden"),
            &format!("plants thriving batch {turn}"),
            "caused",
            "garden",
            None,
        );
    }
    let out = memory.search_memory_multi_pass("how many plants did I buy", None, None, Some(10));
    // Flat-id store: no session expansion (documented), but the
    // multi-pass path itself must return the garden evidence.
    assert!(
        out.contains("bought plants"),
        "multi-pass must surface evidence: {out}"
    );
    assert!(out.contains("[multi-pass]"), "mode tag present: {out}");
}

// ─── search_memory detail_level / max_tokens + invalidate_pattern ──────

#[test]
fn search_memory_detail_levels_and_default_compat() {
    let memory = Memory::open_in_memory().expect("memory");
    memory.record_fact(
        "editor",
        "Neovim with a fairly long configuration description",
        None,
        None,
        None,
    );
    memory.record_decision(
        "used redis mutex for cache invalidation",
        "deadlock under concurrent load",
        "caused",
        "concurrency",
        None,
    );

    let l2 = memory.search_memory("redis mutex", None, None, Some(10), None, None, None);
    let l0 = memory.search_memory("redis mutex", None, None, Some(10), Some("l0"), None, None);
    let l1 = memory.search_memory("redis mutex", None, None, Some(10), Some("l1"), None, None);

    // l0 is strictly cheaper than l2; l1 sits between (or equal at l1's cap).
    assert!(
        l0.len() < l2.len(),
        "l0 must be shorter than l2\nl0: {l0}\nl2: {l2}"
    );
    assert!(l1.len() <= l2.len(), "l1 must not exceed l2");
    // l0 pointers drop the confidence annotations.
    assert!(!l0.contains("confidence:"), "l0 drops confidence: {l0}");
    assert!(l2.contains("confidence:"), "l2 keeps confidence: {l2}");

    // Default (None, None) is byte-identical to explicit l2 + unlimited —
    // and to the pre-feature format (same lines, no truncation note).
    let explicit = memory.search_memory(
        "redis mutex",
        None,
        None,
        Some(10),
        Some("l2"),
        Some(0),
        None,
    );
    assert_eq!(l2, explicit, "default == explicit l2/0");
    assert!(
        !l2.contains("truncated (token budget)"),
        "unlimited default must not truncate: {l2}"
    );

    // Invalid level rejected like invalid scope.
    let bad = memory.search_memory("redis mutex", None, None, Some(10), Some("l9"), None, None);
    assert!(bad.contains("Invalid detail_level"), "{bad}");
}

#[test]
fn search_memory_max_tokens_truncates() {
    let memory = Memory::open_in_memory().expect("memory");
    for i in 0..6 {
        memory.record_decision(
            &format!("deployed cache variant {i} without warmup"),
            &format!("cold start latency spike {i}"),
            "caused",
            "deploy",
            None,
        );
    }
    let full = memory.search_memory(
        "cache warmup deploy",
        None,
        None,
        Some(10),
        None,
        None,
        None,
    );
    // Budget below the cost of the full pool: items are dropped and the
    // truncation note reports how many.
    let capped = memory.search_memory(
        "cache warmup deploy",
        None,
        None,
        Some(10),
        None,
        Some(150),
        None,
    );
    assert!(
        capped.len() < full.len(),
        "capped must be shorter\ncapped: {capped}\nfull: {full}"
    );
    assert!(
        capped.contains("more result(s) truncated (token budget)"),
        "truncation note: {capped}"
    );
}

#[test]
fn invalidate_pattern_soft_deletes_meta_edge() {
    let memory = Memory::open_in_memory().expect("memory");
    memory.record_decision(
        "switched apk sources to Aliyun mirrors",
        "alpine build stabilized",
        "caused",
        "docker",
        None,
    );
    // Mine a pattern between the two chunks of that lesson.
    let (from_id, to_id) = {
        let store = memory.store();
        store
            .with_conn(|c| {
                Ok(
                    c.query_row("SELECT from_id, to_id FROM causal_edges LIMIT 1", [], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?,
                )
            })
            .expect("chunk ids")
    };
    let meta_id = memory
        .store()
        .upsert_meta_edge(
            &from_id,
            &to_id,
            "similar_to",
            "both are mirror-source fixes",
            0.7,
        )
        .expect("meta edge");

    // Visible before revocation, with the #id handle exposed.
    let before = memory.search_patterns(Some("mirror"), None, Some(10));
    assert!(before.contains("similar_to"), "pattern listed: {before}");
    assert!(
        before.contains(&format!("(#{meta_id})")),
        "id exposed: {before}"
    );

    // Revoke: confirmation message, then gone from search_patterns.
    let msg = memory.invalidate_pattern(meta_id, Some("spurious"));
    assert!(msg.starts_with("✅ Invalidated pattern edge"), "{msg}");
    assert!(msg.contains("(reason: spurious)"), "{msg}");
    let after = memory.search_patterns(Some("mirror"), None, Some(10));
    assert!(
        after.contains("No cross-task patterns"),
        "revoked pattern must not be listed: {after}"
    );

    // Idempotent: second revocation is a clean no-op message, not an error.
    let again = memory.invalidate_pattern(meta_id, None);
    assert!(again.contains("already invalidated"), "{again}");

    // Unknown id is a clean miss.
    let missing = memory.invalidate_pattern(999_999, None);
    assert!(missing.contains("not found"), "{missing}");
}

// ─── explain (Flip-path marking) + recall audit e2e ──────────────────

#[test]
#[allow(
    clippy::expect_used,
    reason = "test invariant: memory construction must succeed"
)]
fn search_explain_tags_and_default_invariance() {
    let memory = Memory::open_in_memory().expect("memory");
    memory.record_decision(
        "skipped the test suite before the release",
        "production outage on friday",
        "caused",
        "release",
        None,
    );
    memory.record_decision(
        "deployed without env check",
        "crash loop in production",
        "caused",
        "release",
        None,
    );

    // Default == explicit explain=false, byte-identical, no ↳ markers.
    let default = memory.search_causal(None, Some("test suite release"), Some(5), None, None, None);
    let explicit_false = memory.search_causal(
        None,
        Some("test suite release"),
        Some(5),
        None,
        None,
        Some(false),
    );
    assert_eq!(default, explicit_false, "default == explain=false");
    assert!(!default.contains('↳'), "default has no explain tags");

    // explain=true: every surfaced hit carries a provenance tag.
    let explained = memory.search_causal(
        None,
        Some("test suite release"),
        Some(5),
        None,
        None,
        Some(true),
    );
    assert!(
        explained.contains("↳ ["),
        "explain tags present: {explained}"
    );
    assert!(
        explained.contains("[seed]") || explained.contains("[spread hop="),
        "tag shape: {explained}"
    );

    // search_memory: same contract.
    let d = memory.search_memory("test suite release", None, None, Some(10), None, None, None);
    let e = memory.search_memory(
        "test suite release",
        None,
        None,
        Some(10),
        None,
        None,
        Some(true),
    );
    assert!(!d.contains('↳'), "default unified output unchanged");
    assert!(e.contains("↳ ["), "unified explain tags: {e}");

    // Every recall wrote an audit row (v13 recall_audit).
    let audits = memory.store().recent_recall_audits(10).expect("audit read");
    assert!(
        audits.iter().any(|a| a.query == "test suite release"),
        "audit row for the recall: {audits:?}"
    );
    let row = audits
        .iter()
        .find(|a| a.query == "test suite release")
        .expect("audit row");
    assert!(row.result_count > 0);
    assert!(!row.results.as_array().unwrap().is_empty());
}
