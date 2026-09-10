//! Output-shaping helpers for the memory op responses.

use super::block_on;
use super::format::truncate_chars;
use crate::store::{outcome_polarity, ChainHop};

pub(crate) fn judge_outcome_polarity(decision: &str, outcome: &str) -> String {
    // C2: the LLM judge runs synchronously inside record_decision (8s
    // timeout). CAUSAL_MEMORY_NO_LLM_POLARITY=1 skips it entirely and uses
    // the signal-word heuristic — write latency stays flat when the LLM is
    // only wanted for reading. The `polarity` backfill CLI can re-judge
    // edges later (edges_without_polarity).
    if std::env::var("CAUSAL_MEMORY_NO_LLM_POLARITY").is_err() {
        if let Some(config) = crate::llm::LlmConfig::from_env() {
            if let Ok(pol) = block_on(crate::llm::judge_polarity(&config, decision, outcome)) {
                return pol;
            }
            // LLM failed — fall through to the heuristic.
        }
    }
    match outcome_polarity(outcome) {
        Some(true) => "positive",
        Some(false) => "negative",
        None => "neutral",
    }
    .to_string()
}

/// Label a forward (intervention) chain by its terminal hop. Stored polarity
/// (v4) wins over the text heuristic; `mixed` gets its own WARNING label
/// instead of being forced into SAFE/DANGER. A failure outcome that a
/// `prevented` edge on the path blocked before downgrades DANGER → UNKNOWN.
pub(crate) fn chain_label(
    terminal_polarity: Option<&str>,
    terminal_text: &str,
    has_prevented: bool,
) -> &'static str {
    if terminal_polarity == Some("mixed") {
        return "⚠️ WARNING (mixed outcome)";
    }
    match crate::store::effective_polarity(terminal_polarity, terminal_text) {
        Some(false) if has_prevented => {
            "ℹ️ UNKNOWN (failure outcome, but a prevented edge on this path blocked it before)"
        }
        Some(false) => "⚠️ DANGER",
        Some(true) => "✅ SAFE",
        _ => "ℹ️ UNKNOWN",
    }
}

/// Polarity bucket from (stored polarity, outcome text): stored wins, NULL
/// falls back to the signal-word heuristic.
pub(crate) fn polarity_bucket(stored: Option<&str>, text: &str) -> &'static str {
    match stored {
        Some("positive") => "positive",
        Some("negative") => "negative",
        Some("mixed") => "mixed",
        Some(_) => "neutral",
        None => match outcome_polarity(text) {
            Some(true) => "positive",
            Some(false) => "negative",
            None => "neutral",
        },
    }
}

/// Terminal-outcome bucket for stratified aggregation: stored polarity (v4)
/// wins; NULL falls back to the signal-word heuristic.
pub(crate) fn terminal_bucket(hop: &ChainHop) -> &'static str {
    polarity_bucket(hop.outcome_polarity.as_deref(), &hop.outcome_text)
}

/// Outcome distribution of one group of chains ("other" = mixed + neutral
/// terminal buckets).
#[derive(Default)]
pub(crate) struct StratumDist {
    pub(crate) positive: usize,
    pub(crate) negative: usize,
    pub(crate) other: usize,
}

impl StratumDist {
    pub(crate) fn add(&mut self, bucket: &str) {
        match bucket {
            "positive" => self.positive += 1,
            "negative" => self.negative += 1,
            _ => self.other += 1,
        }
    }
    pub(crate) fn total(&self) -> usize {
        self.positive + self.negative + self.other
    }
    /// Majority direction; "mixed" on a tie or an empty group.
    pub(crate) fn direction(&self) -> &'static str {
        if self.positive > self.negative {
            "positive"
        } else if self.negative > self.positive {
            "negative"
        } else {
            "mixed"
        }
    }
}

/// Most frequent non-None stratum (ties → first seen); None when every chain
/// is untagged.
pub(crate) fn modal_stratum(chains: &[(Option<String>, &str)]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for (tag, _) in chains {
        if let Some(t) = tag {
            match counts.iter_mut().find(|(k, _)| *k == t.as_str()) {
                Some((_, n)) => *n += 1,
                None => counts.push((t.as_str(), 1)),
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(t, _)| t.to_string())
}

/// Stratified summary block appended to intervention_query output: pooled vs
/// reference-stratum terminal-outcome distribution, with a Simpson warning
/// when the pooled majority and the stratum majority point in opposite
/// directions (the pooled estimate is then likely confounded by task_tag).
/// Returns an empty string when there are no chains.
pub(crate) fn stratified_summary(
    chains: &[(Option<String>, &str)],
    reference: Option<&str>,
) -> String {
    if chains.is_empty() {
        return String::new();
    }
    let mut pooled = StratumDist::default();
    let mut within = StratumDist::default();
    let mut across = StratumDist::default();
    for (tag, bucket) in chains {
        pooled.add(bucket);
        match reference {
            Some(r) if tag.as_deref() == Some(r) => within.add(bucket),
            _ => across.add(bucket),
        }
    }

    let mut out = String::from("Stratified by task_tag (terminal outcomes):\n");
    out.push_str(&format!(
        "  pooled (n={}): {} positive / {} negative / {} other → {}\n",
        pooled.total(),
        pooled.positive,
        pooled.negative,
        pooled.other,
        pooled.direction()
    ));
    if let Some(r) = reference {
        out.push_str(&format!(
            "  task_tag={r} (n={}): {} positive / {} negative / {} other → {}\n",
            within.total(),
            within.positive,
            within.negative,
            within.other,
            within.direction()
        ));
        out.push_str(&format!(
            "  other strata (n={}): {} positive / {} negative / {} other → {}\n",
            across.total(),
            across.positive,
            across.negative,
            across.other,
            across.direction()
        ));
        let (p, w) = (pooled.direction(), within.direction());
        if within.total() > 0 && across.total() > 0 && p != "mixed" && w != "mixed" && p != w {
            out.push_str(&format!(
                "  ⚠️ Simpson's paradox: pooled result is {p} but within task_tag={r} it is {w} — pooled estimate likely confounded\n"
            ));
        }
    }
    out
}

/// Outcome distribution of one side of a counterfactual comparison.
#[derive(Default)]
pub(crate) struct CfDist {
    pub(crate) positive: usize,
    pub(crate) negative: usize,
    pub(crate) mixed: usize,
    pub(crate) neutral: usize,
}

impl CfDist {
    pub(crate) fn add(&mut self, bucket: &str) {
        match bucket {
            "positive" => self.positive += 1,
            "negative" => self.negative += 1,
            "mixed" => self.mixed += 1,
            _ => self.neutral += 1,
        }
    }
    pub(crate) fn total(&self) -> usize {
        self.positive + self.negative + self.mixed + self.neutral
    }
    /// Net evidence score: positive counts +1, negative -1, mixed -0.5.
    pub(crate) fn score(&self) -> f64 {
        self.positive as f64 - self.negative as f64 - 0.5 * self.mixed as f64
    }
}

/// Verdict phrase fragments (v14.1): the formatters embed them in
/// human-readable conclusions and the ledger's verdict-code matcher reads
/// them back. ONE source of truth — the pre-constant era had paired output
/// "favor" while the matcher expected "favors", silently mis-coding every
/// paired verdict as no_difference.
pub(crate) const VERDICT_FAVORS_A: &str = "favors A";
pub(crate) const VERDICT_FAVORS_B: &str = "favors B";

/// Conclusion of a counterfactual comparison between two outcome
/// distributions. Deterministic: the side with the higher net evidence score
/// wins; equal scores (or missing data) are honestly "insufficient".
pub(crate) fn counterfactual_verdict(a: &CfDist, b: &CfDist) -> String {
    match (a.total() == 0, b.total() == 0) {
        (true, true) => "📭 insufficient evidence: no recorded episodes for either option — record outcomes with record_decision to build it.".to_string(),
        (true, false) => "insufficient evidence: no recorded episodes matching option A.".to_string(),
        (false, true) => "insufficient evidence: no recorded episodes matching option B.".to_string(),
        (false, false) => {
            let (sa, sb) = (a.score(), b.score());
            if sa > sb {
                format!("recorded evidence {VERDICT_FAVORS_A} (net {sa:+.1} vs {sb:+.1})")
            } else if sb > sa {
                format!("recorded evidence {VERDICT_FAVORS_B} (net {sb:+.1} vs {sa:+.1})")
            } else {
                format!("insufficient evidence to distinguish (both net {sa:+.1})")
            }
        }
    }
}

/// v14 paired (same-context) verdict over fork pairs. A pair votes for the
/// query side whose branch ended positive while the other ended negative —
/// a direct same-world-state contrast. `ids_a`/`ids_b` map pair endpoints
/// to the query's A/B sides (an endpoint retrieved by neither side still
/// renders in the display but casts no vote). None = no cross-side votes
/// (caller falls back to the pooled-distribution verdict).
pub(crate) fn paired_verdict(
    forks: &[crate::store::ForkPair],
    ids_a: &[i64],
    ids_b: &[i64],
) -> Option<String> {
    let side_of = |id: i64| -> Option<char> {
        if ids_a.contains(&id) {
            Some('A')
        } else if ids_b.contains(&id) {
            Some('B')
        } else {
            None
        }
    };
    let (mut va, mut vb, mut contrast) = (0usize, 0usize, 0usize);
    for f in forks {
        let (Some(sa), Some(sb)) = (side_of(f.edge_id_a), side_of(f.edge_id_b)) else {
            continue;
        };
        if sa == sb {
            continue; // both branches landed on the same query side
        }
        let (pa, pb) = (
            f.a_polarity.as_deref().unwrap_or("?"),
            f.b_polarity.as_deref().unwrap_or("?"),
        );
        let winner = match (pa, pb) {
            ("positive", "negative") => Some(sa),
            ("negative", "positive") => Some(sb),
            _ => None,
        };
        contrast += 1;
        match winner {
            Some('A') => va += 1,
            Some('B') => vb += 1,
            _ => {}
        }
    }
    if contrast == 0 || va == vb {
        return None;
    }
    let (w, n) = if va > vb { ('A', va) } else { ('B', vb) };
    let frag = if w == 'A' {
        VERDICT_FAVORS_A
    } else {
        VERDICT_FAVORS_B
    };
    Some(format!(
        "same-context evidence {frag} ({n}/{contrast} contrasting pair(s))"
    ))
}

/// Char-safe truncation to at most `n` chars, appending "…" when cut.
pub(crate) fn edge_stub(e: &crate::store::CausalEntry) -> String {
    let pol = e.outcome_polarity.as_deref().unwrap_or("?");
    let base = format!(
        "#{} {} conf={:.2} pol={pol}",
        e.edge_id, e.relation, e.confidence
    );
    let overhead = base.chars().count() + " | \"\" → \"\"".chars().count();
    let budget = 120usize.saturating_sub(overhead).max(2);
    let half = budget / 2;
    let d = truncate_chars(&e.decision_text, half);
    let o = truncate_chars(&e.outcome_text, budget - half);
    format!("{base} | \"{d}\" → \"{o}\"")
}

/// Mean pairwise Jaccard similarity over the token sets of independent
/// reconstructions (multi-sample calibration). 1.0 = perfect agreement;
/// below the caller's threshold the underlying memories are flagged as
/// potentially unreliable. Fewer than 2 texts → 1.0 (nothing to compare).
pub(crate) fn reconstruction_agreement(texts: &[String]) -> f64 {
    if texts.len() < 2 {
        return 1.0;
    }
    let sets: Vec<std::collections::HashSet<String>> = texts
        .iter()
        .map(|t| crate::patterns::tokenize(t).into_iter().collect())
        .collect();
    let mut sum = 0.0;
    let mut pairs = 0usize;
    for i in 0..sets.len() {
        for j in i + 1..sets.len() {
            let union = sets[i].union(&sets[j]).count();
            let sim = if union == 0 {
                1.0 // two empty texts agree vacuously
            } else {
                sets[i].intersection(&sets[j]).count() as f64 / union as f64
            };
            sum += sim;
            pairs += 1;
        }
    }
    sum / pairs as f64
}
