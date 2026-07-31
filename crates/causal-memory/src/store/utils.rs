//! Utility functions and constants shared across the store.

use std::collections::HashSet;

/// Minimum containment similarity between a `supersedes` hint and an existing
/// chunk's tokens for the older edge to be soft-invalidated. 0.5 = at least
/// half of the smaller token set is shared.
pub const SUPERSEDES_SIM_THRESHOLD: f64 = 0.5;

/// Minimum shared-token count for a supersedes match, on top of
/// `SUPERSEDES_SIM_THRESHOLD`. Guards against one/two-token hints
/// ("books", "music") nuking every chunk that happens to contain the word:
/// with the min-denominator containment metric a single shared token already
/// scores 1.0.
pub const SUPERSEDES_MIN_SHARED_TOKENS: usize = 2;

/// Retraction markers (case-insensitive, substring match): a memory whose
/// text contains one of these RECORDS a retraction rather than stating a
/// current fact ("User no longer likes X", "Removed X from the list",
/// "Cancelled/superseded: X"). Two uses:
/// 1. write time — when the distiller left `supersedes` empty but the item
///    text announces a retraction, the item's own text becomes the kill
///    hint (the LLM forgets the field surprisingly often, and every miss
///    leaves the outdated fact retrievable: Memora weekly round-2 FAA).
/// 2. candidacy — retraction records are never supersedes TARGETS: they
///    share their whole retraction vocabulary ("no longer likes music")
///    with every later hint, and killing one spawns a nonsense double negation
///    ("Cancelled/superseded: User no longer likes Bonobo ...") that
///    actively resurrects the dead fact in answers.
pub const RETRACTION_MARKERS: [&str; 10] = [
    "no longer",
    "not anymore",
    "removed",
    "deleted",
    "cancelled",
    "canceled",
    "completed",
    "moved on",
    " over ",
    "instead of",
];

/// True when `text` records a retraction (see `RETRACTION_MARKERS`) or is a
/// negation memory spawned by guard 3.
pub fn is_retraction_record(text: &str) -> bool {
    let lower = text.to_lowercase();
    RETRACTION_MARKERS.iter().any(|m| lower.contains(m))
}

/// Containment (overlap-coefficient) similarity: |a ∩ b| / min(|a|, |b|).
/// Chosen over Jaccard because supersedes hints are keyword-style and much
/// shorter than the chunk text — Jaccard would punish the length mismatch
/// and miss clear matches. Returns 0.0 when either side is empty.
pub fn containment_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_a: HashSet<&String> = a.iter().collect();
    let set_b: HashSet<&String> = b.iter().collect();
    let inter = set_a.intersection(&set_b).count() as f64;
    inter / set_a.len().min(set_b.len()) as f64
}

/// Extract absolute date tokens (YYYY-MM-DD) from text. Powers the
/// supersedes same-fact guard: when the new item and a kill candidate
/// mention the SAME absolute date, the new item is almost always a
/// restatement/confirmation of that dated fact, not a retraction of it —
/// e.g. "rescheduled to 06-10" followed by "confirmed 06-10" describes one
/// appointment, and invalidating the first wipes the whole calendar chain
/// (Memora weekly round-1 finding). Dates are validated by chrono, so
/// arbitrary 10-char digit runs do not count.
///
/// The leading bracket prefix ("[2025-06-05] " on distilled chunks,
/// "[session_12 2025-06-03] " on raw turn chunks) is stripped first: it is
/// the RECORD date, not content. Without stripping, a same-day retraction
/// ("likes 2010s music" -> later that day "no longer likes 2010s music")
/// would be exempted by the shared record date and the outdated item could
/// never be killed (Memora weekly round-2 finding).
pub fn date_tokens(text: &str) -> HashSet<String> {
    let text = strip_bracket_prefix(text);
    let bytes = text.as_bytes();
    let mut out = HashSet::new();
    if bytes.len() < 10 {
        return out;
    }
    for i in 0..=(bytes.len() - 10) {
        let w = &bytes[i..i + 10];
        if !(w[0].is_ascii_digit()
            && w[1].is_ascii_digit()
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && w[4] == b'-'
            && w[5].is_ascii_digit()
            && w[6].is_ascii_digit()
            && w[7] == b'-'
            && w[8].is_ascii_digit()
            && w[9].is_ascii_digit())
        {
            continue;
        }
        // Boundary check: not embedded in a longer digit run.
        if i > 0 && bytes[i - 1].is_ascii_digit() {
            continue;
        }
        if i + 10 < bytes.len() && bytes[i + 10].is_ascii_digit() {
            continue;
        }
        let s = &text[i..i + 10];
        if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() {
            out.insert(s.to_string());
        }
    }
    out
}

/// Drop a leading "[...] " bracket prefix (the record-date stamp every
/// stored chunk carries: "[2025-06-05] " on distilled items,
/// "[session_12 2025-06-03] " on raw turns). Only the FIRST bracket is
/// removed — later brackets are content.
pub fn strip_bracket_prefix(text: &str) -> &str {
    let text = text.trim_start();
    if !text.starts_with('[') {
        return text;
    }
    match text.find("] ") {
        Some(end) => text[end + 2..].trim_start(),
        None => text,
    }
}

// ─── Outcome polarity helpers ────────────────────────────────────────────

/// Failure signal words (lowercased substring match, EN + ZH).
/// Kept as substring match: English failure words have many inflections
/// ("failed", "errors", "crashed", "timeouts") that a token match would miss,
/// and substring false positives are rare for these words.
const FAILURE_SIGNALS: &[&str] = &[
    "fail", "error", "crash", "deadlock", "timeout", "panic", "失败", "报错", "死锁", "崩溃",
];

/// Chinese success signal words (lowercased substring match).
const SUCCESS_SIGNALS_ZH: &[&str] = &["成功", "通过", "修复"];

/// English success signal tokens (exact word match after splitting on
/// non-alphanumeric characters, same style as `patterns::tokenize`).
const SUCCESS_TOKENS_EN: &[&str] = &[
    "ok",
    "pass",
    "passed",
    "fixed",
    "resolved",
    "succeed",
    "succeeds",
    "succeeded",
];

fn contains_signal(text: &str, signals: &[&str]) -> bool {
    let lower = text.to_lowercase();
    signals.iter().any(|s| lower.contains(s))
}

/// English success words are matched on word boundaries so "unresolved" does
/// not hit "resolved" and "invoke"/"compass" do not hit "ok"/"pass".
/// Inflections of "success" ("successful", "successfully") are covered by a
/// prefix check that excludes the "unsuccess…" negation.
fn contains_success_signal(text: &str) -> bool {
    let lower = text.to_lowercase();
    if contains_signal(&lower, SUCCESS_SIGNALS_ZH) {
        return true;
    }
    lower.split(|c: char| !c.is_alphanumeric()).any(|tok| {
        SUCCESS_TOKENS_EN.contains(&tok)
            || (tok.starts_with("success") && !tok.starts_with("unsuccess"))
    })
}

/// Outcome polarity: `Some(false)` = clearly failure, `Some(true)` = clearly
/// success, `None` = neutral.
///
/// When both failure and success signals co-occur, success wins: the failure
/// word names the problem that was fixed ("deadlock resolved",
/// "fixed the error"), so the outcome itself is a success.
/// Exported for the Phase-3 pattern miner (same-direction / refinement checks).
pub fn outcome_polarity(text: &str) -> Option<bool> {
    let fail = contains_signal(text, FAILURE_SIGNALS);
    let success = contains_success_signal(text);
    match (fail, success) {
        (_, true) => Some(true),
        (true, false) => Some(false),
        (false, false) => None,
    }
}

/// Rule-based contradiction check between two outcomes of the same decision.
///
/// Returns true when one side is clearly a failure and the other side is not
/// (success or neutral) — i.e. the new evidence falsifies the old lesson.
/// Both-failure and both-success/neutral pairs are NOT contradictions.
pub fn outcomes_contradict(old: &str, new: &str) -> bool {
    match (outcome_polarity(old), outcome_polarity(new)) {
        (Some(false), other) => other != Some(false),
        (other, Some(false)) => other != Some(false),
        _ => false,
    }
}

/// Effective polarity of an edge's outcome for contradiction checks and
/// intervention labels: a stored polarity (v4) wins over the text heuristic —
/// 'negative' counts as failure, 'positive' as success, and 'mixed'/'neutral'
/// as neither (they never auto-invalidate and never label a chain SAFE/DANGER
/// on their own). `None` (legacy rows) falls back to the signal-word
/// heuristic on the outcome text.
pub fn effective_polarity(stored: Option<&str>, outcome_text: &str) -> Option<bool> {
    match stored {
        Some("negative") => Some(false),
        Some("positive") => Some(true),
        Some(_) => None,
        None => outcome_polarity(outcome_text),
    }
}
