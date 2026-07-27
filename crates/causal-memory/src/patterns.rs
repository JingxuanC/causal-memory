//! Phase 3: heuristic pattern miner — the "neocortex" layer.
//!
//! `causal_edges` are episodic (hippocampus: fast, per-task). This module scans
//! all valid edges and distils cross-task semantic abstractions into
//! `meta_causal_edges` (decision → decision):
//!
//! - `similar_to`  — two decisions with high token overlap
//! - `repeated`    — similar decisions across *different* task tags, same outcome direction
//! - `contradicts` — similar decisions, opposite outcomes (one failed, one didn't)
//! - `refines`     — same task, a failed decision later improved by a successful one
//!
//! Similarity is Jaccard over tokenized decision text (`tokenize`); outcome
//! direction reuses the Phase-2 signal-word polarity (`store::outcome_polarity`)
//! and `store::outcomes_contradict`. Writes go through
//! `CausalStore::upsert_meta_edge`, so `mine()` is idempotent.
//!
//! Stratified replication test (v5, honest engineering stand-in for a PC-style
//! conditional-independence check): candidate pairs are grouped by their
//! shared decision-token signature, and a pattern is only promoted at full
//! confidence when it holds in ≥ 2 distinct strata (task_tag). A pattern seen
//! in a single stratum is marked `confounded` (half confidence) — it may be
//! domain-specific. When the outcome direction flips between strata within a
//! group, the group is marked `simpson` (Simpson's-paradox warning).
//! Re-running re-tests and upgrades/downgrades existing meta edges.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::store::{outcome_polarity, outcomes_contradict, CausalEntry, CausalStore};

/// Common English stop words removed during tokenization.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "to", "and", "or", "of", "in", "on", "for", "with", "is", "are", "was",
    "were", "be", "by", "at", "as", "it", "this", "that", "we", "i",
];

/// Tokenize decision text for similarity comparison.
///
/// - ASCII runs of alphanumeric chars are lowercased into word tokens;
///   stop words are dropped.
/// - Non-ASCII alphanumeric chars (e.g. Chinese) are grouped into runs and
///   emitted as bigrams (a lone char is emitted as-is).
/// - Everything else (punctuation, whitespace) is a separator.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_ascii(tokens: &mut Vec<String>, buf: &mut String) {
        if !buf.is_empty() {
            if !STOP_WORDS.contains(&buf.as_str()) {
                tokens.push(std::mem::take(buf));
            } else {
                buf.clear();
            }
        }
    }
    fn flush_cjk(tokens: &mut Vec<String>, buf: &mut Vec<char>) {
        if buf.len() == 1 {
            tokens.push(buf[0].to_string());
        } else {
            for w in buf.windows(2) {
                tokens.push(w.iter().collect());
            }
        }
        buf.clear();
    }

    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            flush_cjk(&mut tokens, &mut cjk);
            ascii.push(c.to_ascii_lowercase());
        } else if c.is_alphanumeric() {
            flush_ascii(&mut tokens, &mut ascii);
            cjk.push(c);
        } else {
            flush_ascii(&mut tokens, &mut ascii);
            flush_cjk(&mut tokens, &mut cjk);
        }
    }
    flush_ascii(&mut tokens, &mut ascii);
    flush_cjk(&mut tokens, &mut cjk);
    tokens
}

/// Jaccard similarity |A ∩ B| / |A ∪ B| over token multisets (as sets).
/// Two empty token sets are defined as disjoint (0.0).
pub fn jaccard(a: &[String], b: &[String]) -> f64 {
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    let union = sa.union(&sb).count();
    if union == 0 {
        return 0.0;
    }
    sa.intersection(&sb).count() as f64 / union as f64
}

/// Miner configuration.
#[derive(Debug, Clone, Copy)]
pub struct MinerConfig {
    /// Minimum Jaccard similarity for two decisions to be considered a pair.
    pub similarity_threshold: f64,
}

impl Default for MinerConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.5,
        }
    }
}

/// Counts of meta edges written (inserted or refreshed) per relation in one run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MineReport {
    pub similar_to: usize,
    pub repeated: usize,
    pub contradicts: usize,
    pub refines: usize,
    /// Hits whose pattern held in a single stratum only (halved confidence).
    pub confounded: usize,
    /// Hits in groups where the outcome direction flips across strata.
    pub simpson: usize,
}

/// Heuristic pattern miner over all valid causal edges.
pub struct PatternMiner<'a> {
    store: &'a CausalStore,
    config: MinerConfig,
}

impl<'a> PatternMiner<'a> {
    pub fn new(store: &'a CausalStore, config: MinerConfig) -> Self {
        Self { store, config }
    }

    /// Scan all valid causal edges, detect pattern pairs, and upsert meta edges.
    /// Idempotent: re-running refreshes confidence/discovered_at but never
    /// duplicates a (from_id, to_id, relation) meta edge.
    pub fn mine(&self) -> Result<MineReport> {
        self.mine_inner(true)
    }

    /// Detection-only pass: same report as `mine()` but writes nothing.
    /// Used by the Phase-4 consolidation dry-run path.
    pub fn mine_dry_run(&self) -> Result<MineReport> {
        self.mine_inner(false)
    }

    fn mine_inner(&self, write: bool) -> Result<MineReport> {
        let edges = self.store.all_valid_edges()?;
        let tokens: Vec<Vec<String>> = edges.iter().map(|e| tokenize(&e.decision_text)).collect();
        let mut report = MineReport::default();

        // Pass 1: classify every similar pair, keeping the pair's shared-token
        // signature and endpoint strata/polarities for the replication test.
        let mut hits: Vec<PatternHit> = Vec::new();
        let mut hit_sigs: Vec<String> = Vec::new();
        // signature → group accumulator
        let mut groups: HashMap<String, StrataAcc> = HashMap::new();
        for (i, a) in edges.iter().enumerate() {
            for (j, b) in edges.iter().enumerate().skip(i + 1) {
                let sim = jaccard(&tokens[i], &tokens[j]);
                if sim < self.config.similarity_threshold {
                    continue;
                }
                if let Some(hit) = classify_pair(a, b, sim) {
                    let sig = pair_signature(&tokens[i], &tokens[j]);
                    let acc = groups.entry(sig.clone()).or_default();
                    acc.observe(a);
                    acc.observe(b);
                    hit_sigs.push(sig);
                    hits.push(hit);
                }
            }
        }
        let verdicts: Vec<StrataVerdict> =
            hit_sigs.iter().map(|sig| groups[sig].verdict()).collect();

        // Pass 2: upsert with the stratification verdict attached.
        for (hit, verdict) in hits.iter().zip(&verdicts) {
            // Confounded (single-stratum) patterns are kept but distrusted.
            let confidence = if verdict.confounded {
                hit.confidence * 0.5
            } else {
                hit.confidence
            };
            if write {
                self.store.upsert_meta_edge_stratified(
                    hit.from_id,
                    hit.to_id,
                    hit.relation,
                    &hit.pattern,
                    confidence,
                    Some(&verdict.strata),
                    Some(verdict.confounded),
                    Some(verdict.simpson),
                )?;
            }
            match hit.relation {
                "contradicts" => report.contradicts += 1,
                "refines" => report.refines += 1,
                "repeated" => report.repeated += 1,
                _ => report.similar_to += 1,
            }
            if verdict.confounded {
                report.confounded += 1;
            }
            if verdict.simpson {
                report.simpson += 1;
            }
        }
        Ok(report)
    }
}

/// A detected pattern for one edge pair.
struct PatternHit<'a> {
    relation: &'static str,
    from_id: &'a str,
    to_id: &'a str,
    confidence: f64,
    pattern: String,
}

/// Shared-token signature of a pair: the sorted intersection of the two
/// token sets. Pairs about the same decision family share a signature, so
/// their strata are pooled for the replication test.
fn pair_signature(a: &[String], b: &[String]) -> String {
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    let mut inter: Vec<&str> = sa.intersection(&sb).copied().collect();
    inter.sort_unstable();
    inter.join(" ")
}

/// Per-signature accumulator for the stratified replication test: which
/// strata (task_tag) the decision family appears in, and the outcome
/// direction seen in each stratum.
#[derive(Default)]
struct StrataAcc {
    /// stratum → (saw_success, saw_failure) over endpoint outcomes.
    /// `None` task tags count as the "untagged" stratum.
    dirs: HashMap<String, (bool, bool)>,
}

impl StrataAcc {
    fn observe(&mut self, e: &CausalEntry) {
        let stratum = e.task_tag.clone().unwrap_or_else(|| "untagged".into());
        let dir = self.dirs.entry(stratum).or_default();
        match outcome_polarity(&e.outcome_text) {
            Some(true) => dir.0 = true,
            Some(false) => dir.1 = true,
            None => {}
        }
    }

    fn verdict(&self) -> StrataVerdict {
        let mut strata: Vec<String> = self.dirs.keys().cloned().collect();
        strata.sort();
        // Simpson: one stratum purely positive, another with failures —
        // the pooled direction depends on which stratum you look at.
        let pure_positive = strata.iter().any(|s| self.dirs[s].0 && !self.dirs[s].1);
        let any_negative = strata.iter().any(|s| self.dirs[s].1);
        StrataVerdict {
            confounded: strata.len() < 2,
            simpson: pure_positive && any_negative,
            strata,
        }
    }
}

/// The replication-test verdict for one signature group.
struct StrataVerdict {
    /// Pattern holds in a single stratum only — possibly domain-specific.
    confounded: bool,
    /// Outcome direction flips between strata (Simpson's-paradox signal).
    simpson: bool,
    /// Strata in which the pattern holds (sorted task tags).
    strata: Vec<String>,
}

/// Classify one similar edge pair into at most one relation.
///
/// NOTE on priority: the spec orders contradicts > refines, but `refines`
/// (same task, failure → later success) always also satisfies
/// `outcomes_contradict` (fail vs non-fail), which would make refines
/// unreachable. Since refines is strictly more specific (it adds same-task +
/// temporal-improvement information), it is checked first; the remaining
/// priority is contradicts > repeated > similar_to as specified.
fn classify_pair<'a>(a: &'a CausalEntry, b: &'a CausalEntry, sim: f64) -> Option<PatternHit<'a>> {
    let pol_a = outcome_polarity(&a.outcome_text);
    let pol_b = outcome_polarity(&b.outcome_text);
    let same_tag = a.task_tag.is_some() && a.task_tag == b.task_tag;

    // refines: same task, failure → strictly later success (the success refines
    // the failed attempt). Directional: from = failed, to = successful.
    if same_tag {
        let refined: Option<(&CausalEntry, &CausalEntry)> =
            if pol_a == Some(false) && pol_b == Some(true) && b.event_time > a.event_time {
                Some((a, b))
            } else if pol_b == Some(false) && pol_a == Some(true) && a.event_time > b.event_time {
                Some((b, a))
            } else {
                None
            };
        if let Some((failed, fixed)) = refined {
            return Some(PatternHit {
                relation: "refines",
                from_id: &failed.decision_id,
                to_id: &fixed.decision_id,
                confidence: sim * 0.85,
                pattern: format!(
                    "\"{}\" → \"{}\" (改进: 失败后成功)",
                    failed.decision_text, fixed.decision_text
                ),
            });
        }
    }

    // contradicts: one side clearly failed, the other did not.
    if outcomes_contradict(&a.outcome_text, &b.outcome_text) {
        return Some(PatternHit {
            relation: "contradicts",
            from_id: &a.decision_id,
            to_id: &b.decision_id,
            confidence: sim * 0.8,
            pattern: format!(
                "\"{}\" ≈ \"{}\" (结果矛盾: 一方失败一方未失败)",
                a.decision_text, b.decision_text
            ),
        });
    }

    // repeated: different task tags, same outcome direction.
    if a.task_tag != b.task_tag && pol_a.is_some() && pol_a == pol_b {
        let direction = if pol_a == Some(true) {
            "都成功"
        } else {
            "都失败"
        };
        return Some(PatternHit {
            relation: "repeated",
            from_id: &a.decision_id,
            to_id: &b.decision_id,
            confidence: sim * 0.9,
            pattern: format!(
                "\"{}\" ≈ \"{}\" (跨任务重复: {direction})",
                a.decision_text, b.decision_text
            ),
        });
    }

    // similar_to: fallback for any sufficiently similar pair.
    Some(PatternHit {
        relation: "similar_to",
        from_id: &a.decision_id,
        to_id: &b.decision_id,
        confidence: sim,
        pattern: format!(
            "\"{}\" ≈ \"{}\" (相似决策)",
            a.decision_text, b.decision_text
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_mine_similar_to() {
        // Similar decisions, same task, neutral outcomes → similar_to.
        let store = store_with(&[
            (
                "use redis for cache layer",
                "cache is now warm",
                Some("caching"),
                100,
            ),
            (
                "use redis for session layer",
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
        assert_eq!(patterns[0].from_text, "use redis for cache layer");
        assert_eq!(patterns[0].to_text, "use redis for session layer");
    }

    #[test]
    fn test_mine_repeated_cross_task() {
        // Similar decisions, different task tags, both succeed → repeated.
        let store = store_with(&[
            (
                "use redis for cache",
                "deploy success",
                Some("caching"),
                100,
            ),
            (
                "use redis for sessions",
                "rollout success",
                Some("auth"),
                200,
            ),
        ]);
        let report = mine(&store);
        assert_eq!(report.repeated, 1);
        let p = store.search_patterns(None, None, 10).unwrap();
        assert_eq!(p[0].relation, "repeated");
        assert!((p[0].confidence - 0.5 * 0.9).abs() < 1e-9);
    }

    #[test]
    fn test_mine_contradicts() {
        // Similar decisions, one failed and one succeeded → contradicts.
        let store = store_with(&[
            (
                "use global lock for cache",
                "deadlock: holder crashed",
                Some("locking"),
                100,
            ),
            (
                "use global lock for queue",
                "successfully fixed contention",
                Some("queue"),
                200,
            ),
        ]);
        let report = mine(&store);
        assert_eq!(report.contradicts, 1);
        let p = store.search_patterns(None, None, 10).unwrap();
        assert_eq!(p[0].relation, "contradicts");
        assert!((p[0].confidence - 0.6 * 0.8).abs() < 1e-9);
    }

    #[test]
    fn test_mine_refines_same_task() {
        // Same task, failure then later success → refines, from=failed, to=success.
        let store = store_with(&[
            (
                "use ttl cache for sessions",
                "timeout error under load",
                Some("auth"),
                100,
            ),
            (
                "use ttl cache for tokens",
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
        assert_eq!(p[0].from_text, "use ttl cache for sessions");
        assert_eq!(p[0].to_text, "use ttl cache for tokens");
        // Single stratum (only "auth") → confounded: confidence halved.
        assert!((p[0].confidence - 0.6 * 0.85 * 0.5).abs() < 1e-9);
        assert_eq!(p[0].confounded, Some(true));
        assert_eq!(p[0].strata_count, Some(1));
    }

    #[test]
    fn test_mine_refines_direction_and_timing() {
        // Success recorded with an EARLIER event_time than the failure → not a
        // refinement (no temporal improvement); falls through to contradicts.
        let store = store_with(&[
            (
                "use mutex for cache guard",
                "successfully fixed the race",
                Some("sync"),
                100,
            ),
            (
                "use mutex for cache lock",
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
                "use mutex for cache lock",
                "deadlock error",
                Some("sync"),
                100,
            ),
            (
                "use mutex for cache guard",
                "successfully fixed the race",
                Some("sync"),
                200,
            ),
        ]);
        let report2 = mine(&store2);
        assert_eq!(report2.refines, 1);
        let p = store2.search_patterns(None, None, 10).unwrap();
        assert!(p[0].from_text.contains("deadlock") || p[0].from_text.contains("lock"));
        assert_eq!(p[0].from_text, "use mutex for cache lock");
        assert_eq!(p[0].to_text, "use mutex for cache guard");
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
        assert_eq!(report, MineReport::default());
        assert_eq!(meta_count(&store), 0);
    }

    #[test]
    fn test_same_task_same_direction_not_repeated() {
        let store = store_with(&[
            (
                "use redis for cache",
                "deploy success",
                Some("caching"),
                100,
            ),
            (
                "use redis for sessions",
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
                "use redis for cache",
                "deploy success",
                Some("caching"),
                100,
            ),
            (
                "use redis for sessions",
                "rollout success",
                Some("auth"),
                200,
            ),
            (
                "use global lock for cache",
                "deadlock: holder crashed",
                Some("locking"),
                100,
            ),
            (
                "use global lock for queue",
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
                "use redis for cache",
                "cache stampede failure",
                Some("caching"),
                100,
            ),
            (
                "use redis for sessions",
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
                "use redis for cache",
                "deploy success",
                Some("caching"),
                100,
            ),
            (
                "use redis for sessions",
                "rollout success",
                Some("auth"),
                200,
            ),
            (
                "use global lock for cache",
                "deadlock: holder crashed",
                Some("locking"),
                100,
            ),
            (
                "use global lock for queue",
                "successfully fixed contention",
                Some("queue"),
                200,
            ),
        ]);
        mine(&store);

        // confidence order: contradicts (0.6*0.8=0.48) > repeated (0.5*0.9=0.45)
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
}
