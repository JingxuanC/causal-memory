use super::*;
use crate::store::CausalStore;

fn store_with(entries: &[(&str, &str, Option<&str>, i64)]) -> CausalStore {
    let store = CausalStore::open_in_memory().unwrap();
    for (dec, out, tag, t) in entries {
        store
            .record_decision_at(dec, out, "caused", *tag, 0.8, "rule", *t)
            .unwrap();
    }
    store
}

fn mine(store: &CausalStore) -> MineReport {
    PatternMiner::new(store, MinerConfig::default())
        .mine()
        .unwrap()
}

fn meta_count(store: &CausalStore) -> i64 {
    store
        .with_conn(|conn| {
            let n: i64 =
                conn.query_row("SELECT COUNT(*) FROM meta_causal_edges", [], |r| r.get(0))?;
            Ok(n)
        })
        .unwrap()
}

// ── tokenize / jaccard pure functions ────────────────────────────────

#[test]
fn test_tokenize_english_and_stopwords() {
    let toks = tokenize("Used the Redis Cache to Fix a Deadlock");
    assert!(toks.contains(&"used".to_string()));
    assert!(toks.contains(&"redis".to_string()));
    assert!(toks.contains(&"deadlock".to_string()));
    // stop words removed, lowercased
    assert!(!toks.contains(&"the".to_string()));
    assert!(!toks.contains(&"to".to_string()));
    assert!(!toks.contains(&"Redis".to_string()));
}

#[test]
fn test_tokenize_chinese_bigrams() {
    let toks = tokenize("用Redis做缓存");
    assert!(toks.contains(&"redis".to_string()));
    assert!(toks.contains(&"做缓".to_string()));
    assert!(toks.contains(&"缓存".to_string()));
    // mixed separators split runs: "缓存击穿" → 缓存, 存击, 击穿
    let toks2 = tokenize("缓存击穿");
    assert_eq!(toks2, vec!["缓存", "存击", "击穿"]);
}

#[test]
fn test_jaccard() {
    let a = tokenize("use redis for cache");
    let b = tokenize("use redis for session");
    let sim = jaccard(&a, &b);
    // tokens: {use, redis, cache} vs {use, redis, session} → 2/4
    assert!((sim - 0.5).abs() < 1e-9);
    assert_eq!(jaccard(&[], &[]), 0.0);
    assert_eq!(jaccard(&a, &a), 1.0);
}

// ── the four pattern types ───────────────────────────────────────────
//
// Test texts are built to clear the default signal-quality bar: ≥4 content
// tokens per side and Jaccard ≥ 0.65. The common shape "… share 4 of 5
// tokens" yields 4/6 ≈ 0.667.

#[test]
fn test_mine_similar_to() {
    // Similar decisions (4/6 token overlap), same task, neutral outcomes
    // → similar_to.
    let store = store_with(&[
        (
            "use redis for cache layer fast",
            "cache is now warm",
            Some("caching"),
            100,
        ),
        (
            "use redis for cache layer warm",
            "sessions now persist",
            Some("caching"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.similar_to, 1);
    assert_eq!(report.repeated, 0);
    let patterns = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(patterns.len(), 1);
    assert_eq!(patterns[0].relation, "similar_to");
    assert_eq!(patterns[0].from_text, "use redis for cache layer fast");
    assert_eq!(patterns[0].to_text, "use redis for cache layer warm");
    // Single stratum (only "caching") → confounded.
    assert_eq!(patterns[0].confounded, Some(true));
    assert_eq!(report.confounded, 1);
}

#[test]
fn test_mine_repeated_cross_task() {
    // Similar decisions, different task tags, both succeed → repeated.
    let store = store_with(&[
        (
            "use redis for cache layer alpha",
            "deploy success",
            Some("caching"),
            100,
        ),
        (
            "use redis for cache layer beta",
            "rollout success",
            Some("auth"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.repeated, 1);
    let p = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(p[0].relation, "repeated");
    // Two strata ("caching", "auth") → replicated, full confidence.
    assert!((p[0].confidence - 4.0 / 6.0 * 0.9).abs() < 1e-9);
    assert_eq!(p[0].confounded, Some(false));
    assert_eq!(p[0].strata_count, Some(2));
    assert_eq!(report.confounded, 0);
}

#[test]
fn test_mine_contradicts() {
    // Similar decisions, one failed and one succeeded → contradicts.
    let store = store_with(&[
        (
            "use global lock for cache data",
            "deadlock: holder crashed",
            Some("locking"),
            100,
        ),
        (
            "use global lock for queue data",
            "successfully fixed contention",
            Some("queue"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.contradicts, 1);
    let p = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(p[0].relation, "contradicts");
    assert!((p[0].confidence - 4.0 / 6.0 * 0.8).abs() < 1e-9);
    // One stratum purely positive ("queue"), the other with failures
    // ("locking") → Simpson's-paradox flag.
    assert_eq!(p[0].simpson, Some(true));
    assert_eq!(report.simpson, 1);
}

#[test]
fn test_mine_refines_same_task() {
    // Same task, failure then later success → refines, from=failed, to=success.
    let store = store_with(&[
        (
            "use ttl cache for session tokens",
            "timeout error under load",
            Some("auth"),
            100,
        ),
        (
            "use ttl cache for request tokens",
            "successfully fixed the issue",
            Some("auth"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.refines, 1);
    assert_eq!(report.contradicts, 0);
    assert_eq!(report.similar_to, 0);
    let p = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(p[0].relation, "refines");
    // from = the failed attempt, to = the successful refinement
    assert_eq!(p[0].from_text, "use ttl cache for session tokens");
    assert_eq!(p[0].to_text, "use ttl cache for request tokens");
    // Single stratum (only "auth") → confounded: confidence halved.
    assert!((p[0].confidence - 4.0 / 6.0 * 0.85 * 0.5).abs() < 1e-9);
    assert_eq!(p[0].confounded, Some(true));
    assert_eq!(p[0].strata_count, Some(1));
}

#[test]
fn test_mine_refines_direction_and_timing() {
    // Success recorded with an EARLIER event_time than the failure → not a
    // refinement (no temporal improvement); falls through to contradicts.
    let store = store_with(&[
        (
            "use mutex for cache guard race",
            "successfully fixed the race",
            Some("sync"),
            100,
        ),
        (
            "use mutex for cache guard lock",
            "deadlock error",
            Some("sync"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.refines, 0);
    assert_eq!(report.contradicts, 1);

    // Reversed insertion order: failed edge first, but the success is still
    // later in event_time → refines fires with from=failed regardless of
    // insertion order.
    let store2 = store_with(&[
        (
            "use mutex for cache guard lock",
            "deadlock error",
            Some("sync"),
            100,
        ),
        (
            "use mutex for cache guard race",
            "successfully fixed the race",
            Some("sync"),
            200,
        ),
    ]);
    let report2 = mine(&store2);
    assert_eq!(report2.refines, 1);
    let p = store2.search_patterns(None, None, 10).unwrap();
    assert!(p[0].from_text.contains("deadlock") || p[0].from_text.contains("lock"));
    assert_eq!(p[0].from_text, "use mutex for cache guard lock");
    assert_eq!(p[0].to_text, "use mutex for cache guard race");
}

// ── no false positives ───────────────────────────────────────────────

#[test]
fn test_no_false_positive_low_similarity() {
    let store = store_with(&[
        (
            "use redis for cache",
            "deploy success",
            Some("caching"),
            100,
        ),
        (
            "rewrite parser in rust",
            "build success",
            Some("compiler"),
            200,
        ),
    ]);
    let report = mine(&store);
    // No pattern of any kind is mined ("use redis for cache" has only 3
    // content tokens → the pair is skipped as too short).
    assert_eq!(report.similar_to, 0);
    assert_eq!(report.repeated, 0);
    assert_eq!(report.contradicts, 0);
    assert_eq!(report.refines, 0);
    assert_eq!(meta_count(&store), 0);
}

#[test]
fn test_same_task_same_direction_not_repeated() {
    let store = store_with(&[
        (
            "use redis for cache layer alpha",
            "deploy success",
            Some("caching"),
            100,
        ),
        (
            "use redis for cache layer beta",
            "rollout success",
            Some("caching"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.repeated, 0);
    assert_eq!(report.similar_to, 1);
    let p = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(p[0].relation, "similar_to");
}

// ── idempotency ──────────────────────────────────────────────────────

#[test]
fn test_mine_idempotent() {
    let store = store_with(&[
        (
            "use redis for cache layer alpha",
            "deploy success",
            Some("caching"),
            100,
        ),
        (
            "use redis for cache layer beta",
            "rollout success",
            Some("auth"),
            200,
        ),
        (
            "use global lock for cache data",
            "deadlock: holder crashed",
            Some("locking"),
            100,
        ),
        (
            "use global lock for cache queue data",
            "successfully fixed contention",
            Some("queue"),
            200,
        ),
    ]);
    mine(&store);
    let count1 = meta_count(&store);
    assert_eq!(count1, 2);
    mine(&store);
    mine(&store);
    assert_eq!(meta_count(&store), count1, "re-mining must not duplicate");
}

#[test]
fn test_upsert_meta_edge_updates_confidence() {
    let store = CausalStore::open_in_memory().unwrap();
    let id1 = store
        .upsert_meta_edge("d1", "d2", "similar_to", "p", 0.5)
        .unwrap();
    let id2 = store
        .upsert_meta_edge("d1", "d2", "similar_to", "p2", 0.9)
        .unwrap();
    assert_eq!(id1, id2);
    store
        .with_conn(|conn| {
            let (n, conf): (i64, f64) = conn.query_row(
                "SELECT COUNT(*), MAX(confidence) FROM meta_causal_edges",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(n, 1);
            assert!((conf - 0.9).abs() < 1e-9);
            Ok(())
        })
        .unwrap();
}

// ── priority ─────────────────────────────────────────────────────────

#[test]
fn test_priority_contradicts_over_similar_to() {
    // Different tags (so refines cannot fire): the pair satisfies both
    // contradicts and similar_to → only contradicts is written.
    let store = store_with(&[
        (
            "use redis for cache layer alpha",
            "cache stampede failure",
            Some("caching"),
            100,
        ),
        (
            "use redis for cache layer beta",
            "rollout success",
            Some("auth"),
            200,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.contradicts, 1);
    assert_eq!(report.similar_to, 0);
    assert_eq!(meta_count(&store), 1);
    let p = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(p[0].relation, "contradicts");
}

// ── search_patterns ──────────────────────────────────────────────────

#[test]
fn test_search_patterns_filters_and_order() {
    let store = store_with(&[
        (
            "use redis for cache layer alpha",
            "deploy success",
            Some("caching"),
            100,
        ),
        (
            "use redis for cache layer beta",
            "rollout success",
            Some("auth"),
            200,
        ),
        (
            "use global lock for cache data",
            "deadlock: holder crashed",
            Some("locking"),
            100,
        ),
        (
            "use global lock for cache queue data",
            "successfully fixed contention",
            Some("queue"),
            200,
        ),
    ]);
    mine(&store);

    // confidence order: contradicts (5/6*0.8≈0.667) > repeated (4/6*0.9=0.6)
    let all = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].confidence >= all[1].confidence);
    assert_eq!(all[0].relation, "contradicts");

    // query filter matches decision text
    let q = store
        .search_patterns(Some("global lock"), None, 10)
        .unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].relation, "contradicts");

    // query filter matches pattern summary
    let q2 = store.search_patterns(Some("跨任务重复"), None, 10).unwrap();
    assert_eq!(q2.len(), 1);

    // task_tag filter: either endpoint's task
    let t = store.search_patterns(None, Some("auth"), 10).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].relation, "repeated");

    // limit
    let l = store.search_patterns(None, None, 1).unwrap();
    assert_eq!(l.len(), 1);

    // invalidated meta edges are hidden
    store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE meta_causal_edges SET valid_to = 999 WHERE relation = 'repeated'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let after = store.search_patterns(None, None, 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].relation, "contradicts");
}

// ── pruning: dedup, trivial similarity, boilerplate, caps ────────────

#[test]
fn test_dedup_identical_texts_no_self_pairing() {
    // Dogfooding repro: three edges with the *same* decision text plus two
    // tool-invocation texts sharing a file name. The miner must produce no
    // X ≈ X self-pair and no pure tool-name/boilerplate similarity edge.
    let store = store_with(&[
        (
            "use redis cache layer alpha here",
            "deploy success",
            Some("t1"),
            100,
        ),
        (
            "use redis cache layer alpha here",
            "rollout success",
            Some("t2"),
            200,
        ),
        (
            "use redis cache layer alpha here",
            "cache warm",
            Some("t3"),
            300,
        ),
        // Tool-call shaped texts: identical content tokens after stripping
        // the tool name, and only 3 content tokens → too short to trust.
        (
            "write(insights/09.md)",
            "file written ok",
            Some("docs"),
            400,
        ),
        (
            "search_replace(insights/09.md)",
            "file edited ok",
            Some("docs"),
            500,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(
        meta_count(&store),
        0,
        "no meta edge may be mined: {report:?}"
    );
    // The tool-call pair is counted as skipped (3 content tokens < 4).
    assert!(report.skipped_short >= 1);
    // No self-pairs or identical-text pairs can exist in the DB.
    let (self_pairs, same_text): (i64, i64) = store
        .with_conn(|conn| {
            let s: i64 = conn.query_row(
                "SELECT COUNT(*) FROM meta_causal_edges WHERE from_id = to_id",
                [],
                |r| r.get(0),
            )?;
            let t: i64 = conn.query_row(
                "SELECT COUNT(*) FROM meta_causal_edges m
                 JOIN chunks a ON a.id = m.from_id JOIN chunks b ON b.id = m.to_id
                 WHERE a.text = b.text",
                [],
                |r| r.get(0),
            )?;
            Ok((s, t))
        })
        .unwrap();
    assert_eq!((self_pairs, same_text), (0, 0));
}

#[test]
fn test_skip_identical_token_sets_and_substrings() {
    let store = store_with(&[
        // Same token set as #2, different word order → trivially similar.
        (
            "alpha beta gamma delta epsilon",
            "outcome one",
            Some("t"),
            100,
        ),
        (
            "epsilon delta gamma beta alpha",
            "outcome two",
            Some("t"),
            200,
        ),
        // Substring of #4 → trivially similar.
        (
            "kappa lambda mu nu xi omicron",
            "outcome three",
            Some("t"),
            300,
        ),
        (
            "kappa lambda mu nu xi omicron pi",
            "outcome four",
            Some("t"),
            400,
        ),
    ]);
    let report = mine(&store);
    assert_eq!(report.skipped_self, 2);
    assert_eq!(meta_count(&store), 0);
}

#[test]
fn test_threshold_065_blocks_060_similarity() {
    // 3/5 = 0.6 Jaccard: above the old 0.5 default, below the new 0.65.
    let entries: &[(&str, &str, Option<&str>, i64)] = &[
        (
            "use redis cache layer",
            "deploy success",
            Some("caching"),
            100,
        ),
        (
            "use redis cache queue",
            "rollout success",
            Some("auth"),
            200,
        ),
    ];
    let store = store_with(entries);
    let report = mine(&store);
    assert_eq!(meta_count(&store), 0);
    assert_eq!(report.similar_to + report.repeated, 0);
    assert_eq!(report.skipped_short, 0, "4 tokens each → not too short");

    // Sanity: an explicitly lowered threshold still mines the pair.
    let store2 = store_with(entries);
    let report2 = PatternMiner::new(
        &store2,
        MinerConfig {
            similarity_threshold: 0.5,
            ..MinerConfig::default()
        },
    )
    .mine()
    .unwrap();
    assert_eq!(report2.repeated, 1);
}

#[test]
fn test_top_n_per_decision_keeps_highest_five() {
    // One central decision with 8 highly similar neighbours. Each variant
    // drops one of the centre's 8 tokens and adds a unique one:
    //   centre vs variant: 7/9 ≈ 0.78 ≥ threshold → candidate
    //   variant vs variant: 6/10 = 0.6 < threshold → NOT a candidate
    // so exactly the 8 centre pairs compete and the top-5 cap decides.
    const WORDS: [&str; 8] = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    ];
    let centre = WORDS.join(" ");
    let store = CausalStore::open_in_memory().unwrap();
    store
        .record_decision_at(&centre, "setup done", "caused", Some("t"), 0.8, "rule", 0)
        .unwrap();
    for (i, dropped) in WORDS.iter().enumerate() {
        let variant = format!(
            "{} uniq{i}",
            WORDS
                .iter()
                .filter(|w| *w != dropped)
                .copied()
                .collect::<Vec<_>>()
                .join(" ")
        );
        store
            .record_decision_at(
                &variant,
                "setup done",
                "caused",
                Some("t"),
                0.8,
                "rule",
                i as i64 + 1,
            )
            .unwrap();
    }
    let report = mine(&store);

    // 8 candidate pairs, 5 kept (centre cap), 3 capped.
    assert_eq!(report.similar_to, 5);
    assert_eq!(report.capped, 3);
    let patterns = store.search_patterns(None, None, 100).unwrap();
    assert_eq!(patterns.len(), 5);
    let centre_edges = patterns
        .iter()
        .filter(|p| p.from_text == centre || p.to_text == centre)
        .count();
    assert_eq!(centre_edges, 5, "centre keeps exactly its top 5");
    for p in &patterns {
        assert!(
            p.from_text == centre || p.to_text == centre,
            "only centre pairs may be mined: {p:?}"
        );
    }
}

#[test]
fn test_max_pairs_global_cap() {
    // Four independent high-similarity pairs, global cap 3.
    let store = store_with(&[
        ("use redis cache layer alpha", "ok done", Some("t"), 0),
        ("use redis cache layer beta", "ok done", Some("t"), 1),
        ("use memcached store pool alpha", "ok done", Some("t"), 2),
        ("use memcached store pool beta", "ok done", Some("t"), 3),
        ("use postgres index scan alpha", "ok done", Some("t"), 4),
        ("use postgres index scan beta", "ok done", Some("t"), 5),
        ("use kafka topic offset alpha", "ok done", Some("t"), 6),
        ("use kafka topic offset beta", "ok done", Some("t"), 7),
    ]);
    let report = PatternMiner::new(
        &store,
        MinerConfig {
            max_pairs: 3,
            ..MinerConfig::default()
        },
    )
    .mine()
    .unwrap();
    assert_eq!(report.similar_to, 3);
    assert_eq!(report.capped, 1);
    assert_eq!(meta_count(&store), 3);
}

// ── entity_tokens ─────────────────────────────────────────────────────

#[test]
fn test_entity_tokens_names_and_noise() {
    // Mid-sentence names are entities; possessives normalized.
    assert_eq!(entity_tokens("I talked to Melanie's kids today"), vec!["melanie"]);
    // Sentence-initial caps are capitalization noise, not entities.
    assert!(entity_tokens("Melanie went hiking.").is_empty());
    // Mixed sentence: mid-sentence names survive, sentence-initial ones drop.
    let toks = entity_tokens("I talked to Caroline yesterday. She mentioned Yosemite camping.");
    assert!(toks.contains(&"caroline".to_string()));
    assert!(toks.contains(&"yosemite".to_string()));
    assert!(!toks.iter().any(|t| t == "she"));
    // Stoplist + no proper nouns → no entities.
    assert!(entity_tokens("What is the name of the city?").is_empty());
    assert!(entity_tokens("which editor does the user prefer").is_empty());
}
