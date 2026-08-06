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
//! Similarity is Jaccard over tokenized decision text (`tokenize`) with
//! tool-name boilerplate stripped (`content_tokens`); outcome
//! direction reuses the Phase-2 signal-word polarity (`store::outcome_polarity`)
//! and `store::outcomes_contradict`. Writes go through
//! `CausalStore::upsert_meta_edge_stratified`, so `mine()` is idempotent.
//!
//! Pruning (added after dogfooding showed an O(n²) blowup: 508 edges → 17k
//! pairs): edges are deduped into decision-text groups before pairing (kills
//! X ≈ X self-pairs), trivially similar pairs (identical token sets / substring
//! texts) and pairs with too few content tokens are skipped, and accepted pairs
//! are capped per decision (top-N by similarity) and globally (`max_pairs`).
//!
//! Stratified replication test (v5, honest engineering stand-in for a PC-style
//! conditional-independence check): candidate pairs are grouped by their
//! shared decision-token signature, and a pattern is only promoted at full
//! confidence when it holds in ≥ 2 distinct strata (task_tag). A pattern seen
//! in a single stratum is marked `confounded` (half confidence) — it may be
//! domain-specific. When the outcome direction flips between strata within a
//! group, the group is marked `simpson` (Simpson's-paradox warning). The
//! verdict pools all detected candidates for a signature, before the top-N /
//! `max_pairs` caps truncate. Re-running re-tests and upgrades/downgrades
//! existing meta edges.

use std::collections::HashMap;

use anyhow::Result;

use crate::store::CausalStore;

mod classify;
mod tokenizer;

pub use tokenizer::{
    boilerplate_tokens, content_tokens, entity_tokens, jaccard, tokenize,
};

use classify::{classify_pair, pair_signature, PatternHit, StrataAcc, StrataVerdict};
use tokenizer::normalize;

/// Miner configuration.
#[derive(Debug, Clone, Copy)]
pub struct MinerConfig {
    /// Minimum Jaccard similarity for two decisions to be considered a pair.
    /// 0.65 rather than 0.5: short decision texts tokenize into few tokens, so
    /// any shared word pair pushes Jaccard above 0.5 spuriously.
    pub similarity_threshold: f64,
    /// Minimum size of the (boilerplate-stripped) token set on *each* side of a
    /// pair. Pairs involving shorter texts are unreliable (a single shared
    /// file/tool word dominates) and are skipped.
    pub min_tokens: usize,
    /// Per-decision cap: each decision keeps at most this many meta edges
    /// (highest similarity wins).
    pub max_pairs_per_decision: usize,
    /// Global cap on meta edges written in one run; excess detected pairs are
    /// truncated by similarity. Guard against O(n²) blowup on large stores.
    pub max_pairs: usize,
}

impl Default for MinerConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.65,
            min_tokens: 4,
            max_pairs_per_decision: 5,
            max_pairs: 1000,
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
    /// Pairs skipped as trivially similar: identical normalized text, one text
    /// a substring of the other, or identical token sets (would score 1.0
    /// without carrying any real signal).
    pub skipped_self: usize,
    /// Pairs skipped because one side had fewer than `min_tokens` content tokens.
    pub skipped_short: usize,
    /// Detected pairs dropped by the per-decision top-N / global max-pairs caps.
    pub capped: usize,
    /// Accepted hits whose pattern held in a single stratum only
    /// (halved confidence).
    pub confounded: usize,
    /// Accepted hits in groups where the outcome direction flips across strata.
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
        let boilerplate = boilerplate_tokens();
        let mut report = MineReport::default();

        // 1. Dedup by normalized decision text: edges are grouped into
        //    "decision-text groups" and only the first edge (edges come ordered
        //    by id) represents the group. Mining compares groups, not edges, so
        //    N identical texts can never self-pair (X ≈ X) and produce at most
        //    one pair per text combination instead of N×M.
        let mut groups: Vec<&crate::store::CausalEntry> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for e in &edges {
            let key = normalize(&e.decision_text);
            if let std::collections::hash_map::Entry::Vacant(entry) = seen.entry(key) {
                entry.insert(groups.len());
                groups.push(e);
            }
        }

        // 2. Precompute normalized texts and content-token sets per group.
        let norms: Vec<String> = groups.iter().map(|e| normalize(&e.decision_text)).collect();
        let tokens: Vec<Vec<String>> = groups
            .iter()
            .map(|e| content_tokens(&e.decision_text, &boilerplate))
            .collect();
        let token_sets: Vec<std::collections::HashSet<&str>> = tokens
            .iter()
            .map(|t| t.iter().map(String::as_str).collect())
            .collect();

        // 3. Detect candidate pairs, pooling each pair's endpoints into the
        //    strata accumulator for its shared-token signature (used by the
        //    replication test in step 4).
        struct Candidate<'a> {
            hit: PatternHit<'a>,
            sim: f64,
            sig: String,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        // signature → strata accumulator
        let mut strata_groups: HashMap<String, StrataAcc> = HashMap::new();
        for i in 0..groups.len() {
            for j in i + 1..groups.len() {
                // Trivial similarity: identical token sets (pure punctuation /
                // word-order differences → would score 1.0) or one text a
                // substring of the other. No signal — skip.
                if token_sets[i] == token_sets[j]
                    || norms[i].contains(&norms[j])
                    || norms[j].contains(&norms[i])
                {
                    report.skipped_self += 1;
                    continue;
                }
                // Too few content tokens → similarity is dominated by a shared
                // file/tool word; unreliable.
                if token_sets[i].len() < self.config.min_tokens
                    || token_sets[j].len() < self.config.min_tokens
                {
                    report.skipped_short += 1;
                    continue;
                }
                let sim = jaccard(&tokens[i], &tokens[j]);
                if sim < self.config.similarity_threshold {
                    continue;
                }
                if let Some(hit) = classify_pair(groups[i], groups[j], sim) {
                    let sig = pair_signature(&tokens[i], &tokens[j]);
                    let acc = strata_groups.entry(sig.clone()).or_default();
                    acc.observe(groups[i]);
                    acc.observe(groups[j]);
                    candidates.push(Candidate { hit, sim, sig });
                }
            }
        }

        // 4. Stratified replication verdicts, computed over ALL detected
        //    candidates of a decision family (before the caps truncate).
        let verdicts: HashMap<&str, StrataVerdict> = strata_groups
            .iter()
            .map(|(sig, acc)| (sig.as_str(), acc.verdict()))
            .collect();

        // 5. Caps: keep the top-N pairs per decision and at most max_pairs
        //    overall, highest similarity first. Deterministic tie-break on ids.
        candidates.sort_by(|a, b| {
            b.sim
                .partial_cmp(&a.sim)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.hit.from_id.cmp(b.hit.from_id))
                .then(a.hit.to_id.cmp(b.hit.to_id))
        });
        let mut per_decision: HashMap<&str, usize> = HashMap::new();
        let mut accepted = 0usize;
        for c in candidates {
            if accepted >= self.config.max_pairs
                || per_decision.get(c.hit.from_id).copied().unwrap_or(0)
                    >= self.config.max_pairs_per_decision
                || per_decision.get(c.hit.to_id).copied().unwrap_or(0)
                    >= self.config.max_pairs_per_decision
            {
                report.capped += 1;
                continue;
            }
            accepted += 1;
            *per_decision.entry(c.hit.from_id).or_default() += 1;
            *per_decision.entry(c.hit.to_id).or_default() += 1;
            let verdict = &verdicts[c.sig.as_str()];
            // Confounded (single-stratum) patterns are kept but distrusted.
            let confidence = if verdict.confounded {
                c.hit.confidence * 0.5
            } else {
                c.hit.confidence
            };
            if write {
                self.store.upsert_meta_edge_stratified(
                    c.hit.from_id,
                    c.hit.to_id,
                    c.hit.relation,
                    &c.hit.pattern,
                    confidence,
                    Some(&verdict.strata),
                    Some(verdict.confounded),
                    Some(verdict.simpson),
                )?;
            }
            match c.hit.relation {
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

#[cfg(test)]
mod tests;
