//! Type-agnostic multi-pass retrieval for cross-session questions.
//!
//! Design: docs/design/multi-session-retrieval-2026-08.md (Step A).
//!
//! The production search path cannot rely on the benchmark's question-type
//! labels. Instead the evidence topology is inferred at runtime:
//!   - content entities from the question -> one BM25 query each (the P7
//!     widening, lib-ported);
//!   - an optional temporal anchor ("last month", "past two weeks",
//!     "since the start of the year") -> an inclusive [start, end] window
//!     used to WEIGHT (not hard-filter) retrieval;
//!   - aggregation shapes (how many / how much / list / which / totals)
//!     -> the caller additionally expands every touched session fully so a
//!     complete evidence set reaches the answerer (the P8 widening).
//!
//! Everything here is deterministic and LLM-free; the verification loop that
//! re-queries with entities extracted from a first answer lives at the
//! caller (it needs LLM I/O).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::Datelike;
use regex::Regex;

use crate::store::{CausalEntry, CausalStore};

/// Decomposed query plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryPlan {
    /// Content entities (>=4 chars, stopwords removed, deduped, lowercase).
    pub entities: Vec<String>,
    /// Inclusive [start_ts, end_ts] window in unix seconds, when the query
    /// carries an explicit relative-time anchor.
    pub time_window: Option<(i64, i64)>,
    /// Aggregation shape (how many / list / totals...): needs a COMPLETE
    /// evidence set, so the caller expands full sessions and may verify.
    pub aggregation: bool,
}

const STOPWORDS: &[&str] = &[
    "how", "many", "what", "which", "who", "whom", "whose", "where", "when", "why", "do", "did",
    "does", "is", "are", "was", "were", "have", "has", "had", "i", "you", "we", "they", "he",
    "she", "it", "the", "a", "an", "of", "in", "on", "at", "to", "for", "with", "from", "by",
    "and", "or", "but", "not", "this", "that", "these", "those", "my", "your", "me", "need",
    "pick", "up", "return", "list", "all", "items", "kind", "types", "led", "leading", "worked",
    "bought", "am", "currently", "much", "more", "most", "total", "spent", "money", "did", "get",
    "got", "going", "been", "being", "some", "any", "would", "could", "should", "will", "can",
];

/// Lowercased word tokens of a query (alphanumeric runs only).
fn words(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Content entities: tokens >=4 chars, non-stopword, deduped, order kept.
pub fn extract_entities(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for w in words(query) {
        if w.len() < 4 || STOPWORDS.contains(&w.as_str()) {
            continue;
        }
        if seen.insert(w.to_string()) {
            out.push(w.to_string());
        }
    }
    out
}

const MONTHS: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];

const UNITS_DAY: &str = "day";
const UNITS_WEEK: &str = "week";
const UNITS_MONTH: &str = "month";
const UNITS_YEAR: &str = "year";

fn unit_of(tok: &str) -> Option<(&str, i64)> {
    // returns (unit key, seconds-in-unit for rolling windows)
    match tok {
        "day" | "days" => Some((UNITS_DAY, 86_400)),
        "week" | "weeks" => Some((UNITS_WEEK, 7 * 86_400)),
        "month" | "months" => Some((UNITS_MONTH, 30 * 86_400)),
        "year" | "years" => Some((UNITS_YEAR, 365 * 86_400)),
        _ => None,
    }
}

/// Parse a small cardinal number ("4", "four") — enough for the anchor rules.
fn parse_number(tok: &str) -> Option<i64> {
    if let Ok(n) = tok.parse::<i64>() {
        return Some(n);
    }
    const NUMS: [(&str, i64); 12] = [
        ("one", 1),
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
        ("eleven", 11),
        ("twelve", 12),
    ];
    NUMS.iter().find(|(w, _)| *w == tok).map(|(_, n)| *n)
}

fn day_start_ts(y: i32, m: u32, d: u32) -> Option<i64> {
    chrono::NaiveDate::from_ymd_opt(y, m, d)?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp()
        .into()
}

fn month_index(name: &str) -> Option<u32> {
    MONTHS.iter().position(|m| *m == name).map(|i| i as u32 + 1)
}

/// Weekday name -> days-from-Monday index (monday=0 … sunday=6), 0-based
/// like `chrono::Weekday::num_days_from_monday`.
fn weekday_index(name: &str) -> Option<u32> {
    const DAYS: [&str; 7] = [
        "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
    ];
    DAYS.iter().position(|d| *d == name).map(|i| i as u32)
}

/// Parse a relative-time anchor from the query against the reference
/// timestamp `now` (the question's date in the benchmark; the call time in
/// production). Returns an inclusive [start_ts, end_ts] window. Rule-based,
/// conservative: unknown phrasing -> None (never blocks the main path).
/// Matching uses the `regex` crate (previously hand-rolled word-window
/// scanning); rule priority and window math are unchanged.
pub fn parse_temporal_anchor(query: &str, now: i64) -> Option<(i64, i64)> {
    let q = query.to_lowercase();
    let now_dt = chrono::DateTime::from_timestamp(now, 0)?;
    let today = now_dt.date_naive();
    let y = today.year();
    let m = today.month();

    // 1. "past|last N unit" / "in the last N unit" / "over the (last|past) N unit"
    //    -> rolling window ending now.
    let re_num = Regex::new(
        r"(?:past|last|in the last|over the (?:last|past))\s+(\d+|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)\s+(day|week|month|year)s?",
    )
    .ok()?;
    if let Some(c) = re_num.captures(&q) {
        let n = parse_number(&c[1])?;
        let (_, secs) = unit_of(&c[2])?;
        return Some((now - n * secs, now));
    }
    // 2. "past few unit" -> roughly 3 units back.
    let re_few = Regex::new(r"past few (day|week|month|year)s?").ok()?;
    if let Some(c) = re_few.captures(&q) {
        let (_, secs) = unit_of(&c[1])?;
        return Some((now - 3 * secs, now));
    }
    // 3. "N unit ago". "a"/"an" count as 1 ("a week ago" — LongMemEval's
    // temporal questions use the article form as often as the numeral).
    let re_ago = Regex::new(
        r"(a|an|\d+|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)\s+(day|week|month|year)s?\s+ago",
    )
    .ok()?;
    if let Some(c) = re_ago.captures(&q) {
        let n = if matches!(c.get(1).map(|m| m.as_str()), Some("a" | "an")) {
            1
        } else {
            parse_number(&c[1])?
        };
        let (_, secs) = unit_of(&c[2])?;
        return Some((now - (n + 1) * secs, now - n * secs));
    }
    // 4. bare "last month|week|year" -> previous calendar period.
    let re_last = Regex::new(r"\blast (day|week|month|year)\b").ok()?;
    if let Some(c) = re_last.captures(&q) {
        let (unit, _) = unit_of(&c[1])?;
        return match unit {
            UNITS_WEEK => {
                let dow = today.weekday().num_days_from_monday();
                let this_mon = today - chrono::Duration::days(dow as i64);
                let last_mon = this_mon - chrono::Duration::days(7);
                Some((
                    day_start_ts(last_mon.year(), last_mon.month(), last_mon.day())?,
                    day_start_ts(this_mon.year(), this_mon.month(), this_mon.day())? - 1,
                ))
            }
            UNITS_MONTH => {
                let first_this = today.with_day(1)?;
                let first_last = first_this.checked_sub_months(chrono::Months::new(1))?;
                let end_last = first_this - chrono::Duration::days(1);
                Some((
                    day_start_ts(first_last.year(), first_last.month(), first_last.day())?,
                    day_start_ts(end_last.year(), end_last.month(), end_last.day())? + 86_399,
                ))
            }
            UNITS_YEAR => Some((
                day_start_ts(y - 1, 1, 1)?,
                day_start_ts(y - 1, 12, 31)? + 86_399,
            )),
            _ => None,
        };
    }
    // bare "past week|month|year" -> rolling window.
    let re_past = Regex::new(r"\bpast (day|week|month|year)\b").ok()?;
    if let Some(c) = re_past.captures(&q) {
        let (_, secs) = unit_of(&c[1])?;
        return Some((now - secs, now));
    }
    // 5. "this month|week|year" -> current period start -> now.
    let re_this = Regex::new(r"\bthis (day|week|month|year)\b").ok()?;
    if let Some(c) = re_this.captures(&q) {
        let (unit, _) = unit_of(&c[1])?;
        return match unit {
            UNITS_WEEK => {
                let dow = today.weekday().num_days_from_monday();
                let mon = today - chrono::Duration::days(dow as i64);
                Some((day_start_ts(mon.year(), mon.month(), mon.day())?, now))
            }
            UNITS_MONTH => Some((day_start_ts(y, m, 1)?, now)),
            UNITS_YEAR => Some((day_start_ts(y, 1, 1)?, now)),
            _ => None,
        };
    }
    // 6. "since the (start|beginning) of the year".
    let re_since_year =
        Regex::new(r"since (?:the )?(?:start|beginning) of (?:the )?year").ok()?;
    if re_since_year.is_match(&q) {
        return Some((day_start_ts(y, 1, 1)?, now));
    }
    // 7. "since <month name>" -> that month this year -> now.
    let re_since_month = Regex::new(
        r"since (january|february|march|april|may|june|july|august|september|october|november|december)",
    )
    .ok()?;
    if let Some(c) = re_since_month.captures(&q) {
        let mi = month_index(&c[1])?;
        return Some((day_start_ts(y, mi, 1)?, now));
    }
    // 8. "in|during <month name>" -> that calendar month.
    let re_in_month = Regex::new(
        r"\b(?:in|during) (january|february|march|april|may|june|july|august|september|october|november|december)\b",
    )
    .ok()?;
    if let Some(c) = re_in_month.captures(&q) {
        let mi = month_index(&c[1])?;
        let first = day_start_ts(y, mi, 1)?;
        let first_next = first
            .checked_add(30 * 86_400)
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|dt| dt.date_naive().with_day(1))
            .and_then(|d| d.and_then(|dd| dd.and_hms_opt(0, 0, 0)))
            .and_then(|dt| dt.and_utc().timestamp().into());
        return Some((first, first_next? - 1));
    }
    // 9. "yesterday".
    if q.contains("yesterday") {
        let yest = today - chrono::Duration::days(1);
        return Some((
            day_start_ts(yest.year(), yest.month(), yest.day())?,
            day_start_ts(yest.year(), yest.month(), yest.day())? + 86_399,
        ));
    }
    // 10. "last <weekday>" / "on <weekday>" -> the most recent OCCURRENCE
    //     strictly before today (LongMemEval: "I received a piece of
    //     jewelry last Saturday" — the article, not a calendar-period
    //     reading, distinguishes this from rule 4's "last week").
    //     The window is that single day.
    let re_weekday = Regex::new(
        r"(?:last|on|this) (monday|tuesday|wednesday|thursday|friday|saturday|sunday)",
    )
    .ok()?;
    if let Some(c) = re_weekday.captures(&q) {
        if let Some(dow) = weekday_index(&c[1]) {
            let today_dow = today.weekday().num_days_from_monday();
            // Days to subtract: 1..=7 (a weekday equal to today counts
            // as the PREVIOUS week's occurrence, matching "last
            // Saturday" said on a Saturday).
            let back = (today_dow + 7 - dow) % 7;
            let back = if back == 0 { 7 } else { back };
            let day = today - chrono::Duration::days(back as i64);
            let start = day_start_ts(day.year(), day.month(), day.day())?;
            return Some((start, start + 86_399));
        }
    }
    None
}

/// Aggregation shapes need the COMPLETE evidence set: counting, listing,
/// totals/sums/averages, and best-of comparisons.
pub fn looks_aggregation(query: &str) -> bool {
    let l = query.to_lowercase();
    // Date-math carve-out: "how many days ago did X" / "how many weeks
    // between A and B" ask for arithmetic over ONE (or two) events, not an
    // enumeration. Matching them as aggregation arms the full-session
    // expansion, the wide fact queries, and the verification loop
    // downstream — burying the single evidence turn in noise (LongMemEval
    // temporal-reasoning: 7 of 11 multipass regressions were date-math
    // questions at 2-3x context inflation; true aggregations like "how
    // many books did I buy" contain neither pattern and stay matched).
    const DATE_MATH_UNITS: &[&str] = &[
        "days ago", "weeks ago", "months ago", "years ago", "hours ago", "long ago",
    ];
    if DATE_MATH_UNITS.iter().any(|p| l.contains(p)) {
        return false;
    }
    if l.contains("how many") && l.contains("between") {
        return false;
    }
    const PHRASES: &[&str] = &[
        "how many",
        "how much",
        "list ",
        "which ",
        " all of ",
        " every ",
        "total",
        "sum",
        "amount",
        "average",
        "raised",
        "earned",
        "spent",
        "cost",
        "distance",
        "increase",
        "number of",
    ];
    PHRASES.iter().any(|p| l.contains(p))
}

/// Build the query plan for a question.
pub fn plan_query(query: &str, now: i64) -> QueryPlan {
    QueryPlan {
        entities: extract_entities(query),
        time_window: parse_temporal_anchor(query, now),
        aggregation: looks_aggregation(query),
    }
}

/// Session key of a chunk id in the `{scope}::{session}::{turn}` layout
/// (LongMemEval harness convention): the first two `::`-separated parts.
/// Non-session chunk ids (no separator) -> None, so production data with
/// flat chunk ids simply never expands sessions.
pub fn session_key(chunk_id: &str) -> Option<String> {
    let parts: Vec<&str> = chunk_id.split("::").collect();
    if parts.len() >= 2 {
        Some(format!("{}::{}", parts[0], parts[1]))
    } else {
        None
    }
}

/// Multi-pass retrieval: base BM25(query) + one BM25 per content entity,
/// merged with edge-id dedup. A single top-k query suffices when the hits
/// stay within one session and the question is not aggregation-shaped and
/// carries no temporal anchor; otherwise the entity queries widen recall.
/// Time-window weighting (when an anchor exists) floats in-window evidence
/// to the top WITHOUT filtering anything out.
pub fn retrieve_multi_pass(
    store: &CausalStore,
    task_tag: Option<&str>,
    query: &str,
    plan: &QueryPlan,
    per_query_cap: usize,
) -> Result<Vec<CausalEntry>> {    let base = store.search_causal_bm25(task_tag, query, per_query_cap)?;
    let mut seen: HashSet<i64> = HashSet::new();
    let mut merged: Vec<CausalEntry> = Vec::new();
    for e in base {
        if seen.insert(e.edge_id) {
            merged.push(e);
        }
    }

    // Evidence topology: cross-session span, aggregation shape, or a temporal
    // anchor — any of these means one top-k query is not enough.
    let span = merged
        .iter()
        .filter_map(|e| session_key(&e.decision_id))
        .collect::<HashSet<_>>()
        .len();
    let multi = plan.aggregation || plan.time_window.is_some() || span >= 2;
    if !multi {
        return Ok(merged);
    }

    for term in &plan.entities {
        let hits = store.search_causal_bm25(task_tag, term, per_query_cap / 2)?;
        for e in hits {
            if seen.insert(e.edge_id) {
                merged.push(e);
            }
        }
    }

    if let Some((start, end)) = plan.time_window {
        merged.sort_by(|a, b| {
            let aw = a.event_time >= start && a.event_time <= end;
            let bw = b.event_time >= start && b.event_time <= end;
            bw.cmp(&aw).then(b.event_time.cmp(&a.event_time))
        });
    }
    Ok(merged)
}

/// One BM25 query per term, merged with edge-id dedup (verification-loop
/// helper: re-query with entities extracted from a first answer).
pub fn query_terms(
    store: &CausalStore,
    task_tag: Option<&str>,
    terms: &[String],
    per_query_cap: usize,
) -> Result<Vec<CausalEntry>> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut out: Vec<CausalEntry> = Vec::new();
    for term in terms {
        let hits = store.search_causal_bm25(task_tag, term, per_query_cap)?;
        for e in hits {
            if seen.insert(e.edge_id) {
                out.push(e);
            }
        }
    }
    Ok(out)
}

/// Full-coverage session expansion: gather the session keys touched by the
/// given chunk ids, rank sessions by how many ids they own, and fetch every
/// chunk of each session up to `budget` total. This replaces the harness's
/// "top-5 hit sessions" cap — aggregation questions scatter their evidence
/// over sessions that each contribute few BM25 hits.
pub fn expand_session_chunks(
    store: &CausalStore,
    chunk_ids: &[String],
    budget: usize,
) -> Result<Vec<(String, String)>> {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for id in chunk_ids {
        if let Some(k) = session_key(id) {
            *freq.entry(k).or_insert(0) += 1;
        }
    }
    if freq.is_empty() {
        return Ok(Vec::new());
    }
    let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
    ranked.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let mut out: Vec<(String, String)> = Vec::new();
    for (key, _) in ranked {
        let chunks = store.chunks_by_prefix(&key)?;
        for (id, text) in chunks {
            if out.len() >= budget {
                return Ok(out);
            }
            out.push((id, text));
        }
    }
    Ok(out)
}

/// Layered quota (retrieval-scoring.md §4): distill episodes are compact
/// paraphrases of the original turns. BM25's length normalization ranks a
/// 30-token summary above the 176-394-token turn it summarizes (same
/// term hits, shorter doc), and the summary's phrasing ("acquired...
/// last month") tracks the question's phrasing more closely than the
/// conversation's ("got...") — so episodes crowd original evidence out
/// of top-k and dilute the prompt, while the answer's full context sits
/// in the turn. Cap episodes at `max_episodes` of the merged pool;
/// originals keep their rank order. Entries arrive rank-ordered, so the
/// FIRST `max_episodes` episodes (highest-ranked) survive.
pub fn apply_episode_quota(entries: Vec<CausalEntry>, max_episodes: usize) -> Vec<CausalEntry> {
    if max_episodes == usize::MAX {
        return entries;
    }
    let mut kept: Vec<CausalEntry> = Vec::with_capacity(entries.len());
    let mut episodes = 0usize;
    for e in entries {
        if e.discovered_by == "distill" {
            if episodes >= max_episodes {
                continue;
            }
            episodes += 1;
        }
        kept.push(e);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_math_questions_are_not_aggregation() {
        // Temporal-distance lookups: ONE event + date arithmetic — the
        // aggregation pipeline (full-session expansion, wide fact queries,
        // verify loop) buries their single evidence turn in noise.
        assert!(!looks_aggregation("How many days ago did I buy a smoker?"));
        assert!(!looks_aggregation(
            "How many weeks ago did I attend the 'Summer Nights' festival?"
        ));
        assert!(!looks_aggregation("How many months ago did we last meet?"));
        assert!(!looks_aggregation("How long ago did I harvest the herbs?"));
        assert!(!looks_aggregation(
            "How many days passed between the wedding and the sale?"
        ));
    }

    #[test]
    fn true_aggregations_still_match() {
        assert!(looks_aggregation("How many books did I buy?"));
        assert!(looks_aggregation("How much did I spend on groceries?"));
        assert!(looks_aggregation("list all the concerts I attended"));
        assert!(looks_aggregation("What's the total number of plants?"));
    }

    #[test]
    fn episode_quota_caps_paraphrases_keeps_original_order() {
        let entry = |id: i64, by: &str| CausalEntry {
            edge_id: id,
            decision_id: format!("d{id}"),
            decision_text: format!("text {id}"),
            outcome_id: format!("o{id}"),
            outcome_text: format!("out {id}"),
            relation: "caused".into(),
            confidence: 0.8,
            task_tag: None,
            event_time: 0,
            valid_to: None,
            access_count: 0,
            last_accessed_at: None,
            discovered_by: by.into(),
            discovered_at: 0,
            outcome_polarity: None,
            superseded_by: None,
        };
        // Rank-ordered pool: 5 episodes above 5 originals (the §4 shape —
        // BM25 length normalization floats the summaries).
        let pool: Vec<CausalEntry> = (0..5)
            .map(|i| entry(i, "distill"))
            .chain((5..10).map(|i| entry(i, "temporal")))
            .collect();

        let capped = apply_episode_quota(pool, 3);
        let episodes = capped.iter().filter(|e| e.discovered_by == "distill").count();
        let originals = capped.iter().filter(|e| e.discovered_by == "temporal").count();
        assert_eq!(episodes, 3, "episodes capped at the quota");
        assert_eq!(originals, 5, "originals never dropped");
        // Highest-ranked episodes survive (first 3 of 5), originals keep order.
        assert!(capped.iter().take(3).all(|e| e.discovered_by == "distill"));
        assert_eq!(
            capped
                .iter()
                .filter(|e| e.discovered_by == "temporal")
                .map(|e| e.edge_id)
                .collect::<Vec<_>>(),
            vec![5, 6, 7, 8, 9]
        );

        // All-original pool is untouched.
        let originals_only: Vec<CausalEntry> = (0..4).map(|i| entry(i, "temporal")).collect();
        let n = apply_episode_quota(originals_only, 3).len();
        assert_eq!(n, 4);
        // Quota of 0 drops every episode (pure-original mode).
        assert_eq!(
            apply_episode_quota(
                vec![entry(0, "distill"), entry(1, "temporal")],
                0
            )
            .len(),
            1
        );
    }

    #[test]
    fn plan_for_smoker_question_skips_expansion() {
        // The exact LongMemEval regression shape: aggregation=false means
        // the harness returns the merged multi-pass entries directly —
        // no expand_and_inject, no verify loop, topk fact queries only.
        let plan = plan_query("How many days ago did I buy a smoker?", 1_700_000_000);
        assert!(!plan.aggregation);
        assert!(!plan.entities.is_empty());
    }

    /// 2026-08-21 is a Friday. Anchors below are verified against it.
    const FRIDAY_TS: i64 = 1_787_270_400; // 2026-08-21T00:00:00Z

    #[test]
    #[allow(
        clippy::expect_used,
        reason = "test invariant: the anchor must parse or the test is meaningless"
    )]
    fn temporal_anchor_article_number_forms() {
        // "a week ago" must anchor (the article counts as 1) — previously
        // returned None and left the question unanchored.
        let (start, end) = parse_temporal_anchor("Which book did I finish a week ago?", FRIDAY_TS)
            .expect("a week ago must anchor");
        let week = 7 * 86_400;
        // Rule 3's window for n=1: [now-2u, now-u] — the week that ended
        // one week before now.
        assert_eq!(end - start, week, "window spans exactly one week");
        assert_eq!(FRIDAY_TS - end, week, "window is the week ending 1 week before now");
        assert!(end < FRIDAY_TS, "window lies strictly before now");
        // "one week ago" (spelled numeral) matches the same shape.
        assert!(parse_temporal_anchor("I went one week ago", FRIDAY_TS).is_some());
    }

    #[test]
    #[allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "test invariant: the anchor and timestamp must parse"
    )]
    fn temporal_anchor_last_weekday() {
        // Friday 2026-08-21: "last Saturday" = 2026-08-15 (6 days back).
        let (start, end) =
            parse_temporal_anchor("I received jewelry last Saturday from whom?", FRIDAY_TS)
                .expect("last Saturday must anchor");
        assert_eq!(end - start + 1, 86_400, "window is exactly one day");
        let dt = chrono::DateTime::from_timestamp(start, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-08-15");
        assert_eq!(dt.weekday().to_string(), "Sat");

        // Said ON a Saturday: "last Saturday" means the PREVIOUS week's
        // occurrence (7 days back), not today.
        const SAT_TS: i64 = 1_786_752_000; // 2026-08-15T00:00:00Z (Sat)
        let (start, _) =
            parse_temporal_anchor("I saw her last Saturday", SAT_TS).expect("must anchor");
        let dt = chrono::DateTime::from_timestamp(start, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-08-08");
    }
    use crate::store::CausalStore;

    fn insert_turn(store: &CausalStore, id: &str, text: &str, ts: i64, task_tag: &str) {
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, text, ts],
                )?;
                Ok(())
            })
            .unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![id, id, "caused", 0.4, "temporal", ts, ts, task_tag],
                )?;
                Ok(())
            })
            .unwrap();
    }

    const NOW: i64 = 1_684_000_000; // ~2023-05-13

    #[test]
    fn plan_entities_and_aggregation() {
        let p = plan_query("How many items of clothing did I buy from Zara?", NOW);
        assert!(p.aggregation);
        assert!(p.entities.iter().any(|e| e == "clothing"));
        assert!(p.entities.iter().any(|e| e == "zara"));
        assert!(!p.entities.iter().any(|e| e == "how" || e == "many"));

        let p2 = plan_query("What time did I go to bed yesterday?", NOW);
        assert!(!p2.aggregation, "time questions are not aggregation");
        let w = p2.time_window.expect("yesterday anchors to a calendar-day window");
        assert_eq!(w.1 - w.0, 86_399);
        assert_eq!(w.0 % 86_400, 0, "yesterday window starts at local midnight");
    }

    #[test]
    fn time_anchor_variants() {
        assert_eq!(
            parse_temporal_anchor("How many plants in the past two weeks?", NOW),
            Some((NOW - 2 * 7 * 86_400, NOW))
        );
        let w = parse_temporal_anchor("spent on items last month?", NOW).unwrap();
        assert!(w.1 < NOW && w.0 < w.1, "{w:?}");
        let w = parse_temporal_anchor("raised since the start of the year?", NOW).unwrap();
        assert_eq!(w.1, NOW);
        assert!(w.0 < NOW);
        let w = parse_temporal_anchor("What time did I go to bed yesterday?", NOW).unwrap();
        assert_eq!(w.1 - w.0, 86_399);
        assert_eq!(parse_temporal_anchor("What did I buy at the store?", NOW), None);
        assert_eq!(parse_temporal_anchor("How do you make coffee?", NOW), None);
    }

    #[test]
    fn session_key_parsing() {
        assert_eq!(session_key("q1::s2::4"), Some("q1::s2".to_string()));
        assert_eq!(session_key("d1715000000"), None);
        assert_eq!(session_key("a::b"), Some("a::b".to_string()));
    }

    #[test]
    fn multi_pass_widens_across_sessions() {
        let store = CausalStore::open_in_memory().unwrap();
        insert_turn(&store, "q1::s1::1", "I love watching movies on weekends.", 1_000, "q1");
        insert_turn(&store, "q1::s2::1", "I bought three plants from the nursery.", 2_000, "q1");
        // s3 has one BM25-hit turn plus an evidence turn with NO query token:
        // full-coverage expansion must pull BOTH once the session is touched.
        insert_turn(&store, "q1::s3::1", "My sister also gave me plants.", 3_000, "q1");
        insert_turn(&store, "q1::s3::2", "The snake plant is in the living room.", 3_001, "q1");

        let plan = QueryPlan {
            entities: vec!["plants".into(), "nursery".into()],
            time_window: None,
            aggregation: true,
        };
        let hits = retrieve_multi_pass(&store, Some("q1"), "How many plants did I get?", &plan, 5)
            .unwrap();
        let texts: Vec<&str> = hits.iter().map(|e| e.decision_text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("three plants")),
            "entity widening must reach the s2 evidence: {texts:?}"
        );
        let ids: Vec<String> = hits
            .iter()
            .flat_map(|e| [e.decision_id.clone(), e.outcome_id.clone()])
            .collect();
        let expanded = expand_session_chunks(&store, &ids, 20).unwrap();
        let exp_texts: Vec<&str> = expanded.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            exp_texts.iter().any(|t| t.contains("living room")),
            "full-coverage expansion must reach the non-hit s3 turn: {exp_texts:?}"
        );
    }

    #[test]
    fn single_session_query_stays_single_pass() {
        let store = CausalStore::open_in_memory().unwrap();
        insert_turn(&store, "q1::s1::1", "I bought a Nikon camera in May.", 1_000, "q1");
        insert_turn(&store, "q1::s1::2", "The Nikon cost 1200 dollars.", 1_001, "q1");
        let plan = plan_query("What camera did I buy?", NOW);
        let hits = retrieve_multi_pass(&store, Some("q1"), "What camera did I buy?", &plan, 5)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(plan.time_window.is_none());
    }
}
