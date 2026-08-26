//! Layered formatting, token budgets, and the shared RRF fusion.

use super::RRF_K;
use crate::store::{AgentFact, CausalEntry};

/// Format one entry at L0/L1/L2 detail, with an approximate token cost.
/// Pub: the CLI's bench_tokens binary re-uses it for token measurements.
pub fn format_entry_layered(entry: &CausalEntry, rank: usize, level: &str) -> (String, usize) {
    let dt = entry.decision_text.as_str();
    let ot = entry.outcome_text.as_str();
    let tag = entry.task_tag.as_deref().unwrap_or("untagged");
    let conf = (entry.confidence * 100.0).round() as u32;
    let superseded_note = if entry.superseded_by.is_some() {
        "   ⚠ superseded later by a newer memory — check it before relying on this\n"
    } else {
        ""
    };
    match level {
        "l0" => {
            let line = format!(
                "{rank}. [{}] {} →({})→ {}\n",
                tag,
                truncate_chars(dt, 40),
                entry.relation,
                truncate_chars(ot, 40),
            );
            (line, 25)
        }
        "l1" => {
            let line = format!(
                "{rank}. [{}] \"{}\"\n   →({})→ \"{}\" (confidence: {conf}%)\n{superseded_note}",
                tag, dt, entry.relation, ot,
            );
            (line, 60)
        }
        _ => {
            let line = format!(
                "{rank}. [{}] \"{}\"\n   →({})→ \"{}\"\n   confidence: {conf}%\n{superseded_note}\n",
                tag, dt, entry.relation, ot,
            );
            (line, 100)
        }
    }
}

/// Format one spreading-activation hit at L0/L1/L2 detail, with an
/// approximate token cost. Activation hits carry only chunk text (no
/// decision/outcome pair), so the levels differ by text verbosity:
/// l0 pointer (40 chars), l1 overview (80 chars), l2 full text.
pub fn format_activation_layered(
    text: &str,
    activation: f32,
    rank: usize,
    level: &str,
) -> (String, usize) {
    let sign = if activation > 0.0 { "+" } else { "-" };
    let act = activation.abs() * 100.0;
    match level {
        "l0" => (
            format!(
                "{rank}. [{act:.0}%{sign}] \"{}\"\n",
                truncate_chars(text, 40)
            ),
            25,
        ),
        "l1" => (
            format!(
                "{rank}. [{act:.0}%{sign}] \"{}\"\n",
                truncate_chars(text, 80)
            ),
            60,
        ),
        _ => (format!("{rank}. [{act:.0}%{sign}] \"{text}\"\n"), 100),
    }
}

/// Format one fact (unified-display style) at L0/L1/L2 detail, with an
/// approximate token cost. L2 is byte-identical to the historical
/// `render_unified` fact line — the 60-char display cap stays (full text
/// remains available via the search_facts deep-dive); L0/L1 are cheaper
/// tiers for token-constrained agents.
pub fn format_fact_layered(fact: &AgentFact, rank: usize, level: &str) -> (String, usize) {
    let conf = (fact.confidence * 100.0).round() as u32;
    match level {
        "l0" => (
            format!(
                "  #{rank} [{}] {} = \"{}\"\n",
                fact.scope,
                fact.key,
                truncate_chars(&fact.value, 40)
            ),
            25,
        ),
        "l1" => (
            format!(
                "  #{rank} [{}] {} = \"{}\" (confidence: {conf}%)\n",
                fact.scope,
                fact.key,
                truncate_chars(&fact.value, 40)
            ),
            60,
        ),
        _ => (
            format!(
                "  #{rank} [{}] {} = \"{}\" (confidence: {conf}%)\n",
                fact.scope,
                fact.key,
                truncate_chars(&fact.value, 60)
            ),
            100,
        ),
    }
}

/// Format one causal lesson (unified-display style) at L0/L1/L2 detail,
/// with an approximate token cost. L2 is byte-identical to the historical
/// `render_unified` causal line — the 50-char display caps stay (the
/// search_causal deep-dive renders full text via
/// [`format_entry_layered`]). L0/L1 are the cheaper tiers.
pub fn format_lesson_layered(entry: &CausalEntry, rank: usize, level: &str) -> (String, usize) {
    let tag = entry.task_tag.as_deref().unwrap_or("untagged");
    let conf = (entry.confidence * 100.0).round() as u32;
    match level {
        "l0" => (
            format!(
                "  #{rank} [{tag}] {} →({})→ {}\n",
                truncate_chars(&entry.decision_text, 40),
                entry.relation,
                truncate_chars(&entry.outcome_text, 40),
            ),
            25,
        ),
        "l1" => (
            format!(
                "  #{rank} [{tag}] \"{}\" →({})→ \"{}\"\n",
                truncate_chars(&entry.decision_text, 50),
                entry.relation,
                truncate_chars(&entry.outcome_text, 50),
            ),
            60,
        ),
        _ => (
            format!(
                "  #{rank} [{tag}] \"{}\" →({})→ \"{}\" (confidence: {conf}%)\n",
                truncate_chars(&entry.decision_text, 50),
                entry.relation,
                truncate_chars(&entry.outcome_text, 50),
            ),
            100,
        ),
    }
}

/// Flip-path marking: the explain tag for one hit — `[seed]` for direct
/// seed hits, `[spread hop=N via relation←"from"]` for spread-lit hits.
pub(crate) fn provenance_tag(
    hop: u8,
    via_relation: Option<&'static str>,
    via_from_text: Option<&str>,
) -> String {
    if hop == 0 {
        return "[seed]".to_string();
    }
    match (via_relation, via_from_text) {
        (Some(rel), Some(from)) => format!(
            "[spread hop={} via {}←\"{}\"]",
            hop,
            rel,
            truncate_chars(from, 40)
        ),
        _ => format!("[spread hop={hop}]"),
    }
}

/// P5: Token budget tracker — yields entries until the budget is exhausted.
pub(crate) struct TokenBudget {
    remaining: usize,
    limit: usize,
}

impl TokenBudget {
    pub(crate) fn new(max_tokens: usize) -> Self {
        Self {
            remaining: max_tokens,
            limit: max_tokens,
        }
    }
    pub(crate) fn try_spend(&mut self, cost: usize) -> bool {
        if self.limit == 0 {
            return true; // 0 = unlimited
        }
        if cost > self.remaining {
            return false;
        }
        self.remaining -= cost;
        true
    }
}

/// Fuse N ranked key lists by Reciprocal Rank Fusion:
/// `score(key) = Σ_lists 1 / (RRF_K + rank)` with 1-based ranks. A key
/// present in multiple lists scores from each — cross-layer agreement floats
/// to the top. Returns keys with fused scores, sorted descending; ties keep
/// first-seen order (stable sort).
pub(crate) fn rrf_fuse_many(lists: &[&[String]]) -> Vec<(String, f64)> {
    let mut scores: Vec<(String, f64)> = Vec::new();
    let add = |key: &str, rank: usize, scores: &mut Vec<(String, f64)>| {
        let s = 1.0 / (RRF_K + rank as f64 + 1.0);
        match scores.iter_mut().find(|(k, _)| k == key) {
            Some((_, acc)) => *acc += s,
            None => scores.push((key.to_string(), s)),
        }
    };
    for list in lists {
        for (i, k) in list.iter().enumerate() {
            add(k, i, &mut scores);
        }
    }
    scores.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
    scores
}

/// Two-list convenience wrapper (kept for tests and for the CLI's
/// bench_tokens, which re-exports it). Pub: re-exported cross-crate.
pub fn rrf_fuse(a: &[String], b: &[String]) -> Vec<(String, f64)> {
    rrf_fuse_many(&[a, b])
}

/// Write-time outcome polarity: LLM judge when an LLM is configured, falling
/// back to the signal-word heuristic on any failure or when unconfigured
/// (Some(true)→positive, Some(false)→negative, None→neutral).
pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars().take(n.saturating_sub(1)).collect::<String>()
    )
}
