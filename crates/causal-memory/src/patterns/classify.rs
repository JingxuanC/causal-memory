//! Pair classification and stratified replication for pattern mining.

use std::collections::{HashMap, HashSet};

use crate::store::{outcome_polarity, outcomes_contradict, CausalEntry};

/// A detected pattern for one edge pair.
pub(crate) struct PatternHit<'a> {
    pub relation: &'static str,
    pub from_id: &'a str,
    pub to_id: &'a str,
    pub confidence: f64,
    pub pattern: String,
}

/// Shared-token signature of a pair: the sorted intersection of the two
/// (boilerplate-stripped) token sets. Pairs about the same decision family
/// share a signature, so their strata are pooled for the replication test.
pub(crate) fn pair_signature(a: &[String], b: &[String]) -> String {
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
pub(crate) struct StrataAcc {
    /// stratum → (saw_success, saw_failure) over endpoint outcomes.
    /// `None` task tags count as the "untagged" stratum.
    dirs: HashMap<String, (bool, bool)>,
}

impl StrataAcc {
    pub fn observe(&mut self, e: &CausalEntry) {
        let stratum = e.task_tag.clone().unwrap_or_else(|| "untagged".into());
        let dir = self.dirs.entry(stratum).or_default();
        match outcome_polarity(&e.outcome_text) {
            Some(true) => dir.0 = true,
            Some(false) => dir.1 = true,
            None => {}
        }
    }

    pub fn verdict(&self) -> StrataVerdict {
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
pub(crate) struct StrataVerdict {
    /// Pattern holds in a single stratum only — possibly domain-specific.
    pub confounded: bool,
    /// Outcome direction flips between strata (Simpson's-paradox signal).
    pub simpson: bool,
    /// Strata in which the pattern holds (sorted task tags).
    pub strata: Vec<String>,
}

/// Classify one similar edge pair into at most one relation.
///
/// NOTE on priority: the spec orders contradicts > refines, but `refines`
/// (same task, failure → later success) always also satisfies
/// `outcomes_contradict` (fail vs non-fail), which would make refines
/// unreachable. Since refines is strictly more specific (it adds same-task +
/// temporal-improvement information), it is checked first; the remaining
/// priority is contradicts > repeated > similar_to as specified.
pub(crate) fn classify_pair<'a>(
    a: &'a CausalEntry,
    b: &'a CausalEntry,
    sim: f64,
) -> Option<PatternHit<'a>> {
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
        let direction = if pol_a == Some(true) { "都成功" } else { "都失败" };
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
