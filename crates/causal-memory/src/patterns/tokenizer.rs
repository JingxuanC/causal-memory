//! Text tokenization and similarity utilities for pattern mining.

use std::collections::HashSet;

use crate::extractor::DECISION_WORTHY_TOOLS;

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

/// Boilerplate tokens contributed by tool names alone (e.g. decision texts like
/// `write(insights/09.md)` tokenize to `write`, `insights`, ...). Built from the
/// extractor's `DECISION_WORTHY_TOOLS`: each tool name is tokenized, so
/// `search_replace` contributes {"search", "replace"}. These tokens carry no
/// decision content and inflate Jaccard on short tool-invocation texts, so they
/// are stripped before similarity is computed.
pub fn boilerplate_tokens() -> HashSet<String> {
    DECISION_WORTHY_TOOLS
        .iter()
        .flat_map(|t| tokenize(t))
        .collect()
}

/// Content tokens of a decision text: `tokenize` minus tool-name boilerplate.
pub fn content_tokens(text: &str, boilerplate: &HashSet<String>) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|t| !boilerplate.contains(t))
        .collect()
}

/// Normalize decision text for duplicate grouping: lowercase, collapse
/// whitespace. Two texts with the same normalization are the same decision.
pub(crate) fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Mid-sentence capitalized words (person/place/entity names), stoplist
/// filtered, lowercase-normalized (`Melanie's` → `melanie`). Sentence-initial
/// words are excluded — sentence caps are capitalization noise, not entities.
///
/// Deliberately frequency-favoring: an entity appearing in many chunks is
/// exactly what multi-hop person-anchored retrieval needs, so no IDF
/// weighting is applied (the entity signal is the inverse of the lexical one).
pub fn entity_tokens(text: &str) -> Vec<String> {
    const ENTITY_STOP: &[&str] = &[
        "the",
        "a",
        "an",
        "and",
        "but",
        "so",
        "or",
        "if",
        "then",
        "than",
        "she",
        "he",
        "they",
        "we",
        "i",
        "you",
        "me",
        "us",
        "her",
        "him",
        "his",
        "hers",
        "their",
        "my",
        "your",
        "our",
        "its",
        "what",
        "when",
        "where",
        "why",
        "how",
        "who",
        "which",
        "that",
        "this",
        "these",
        "those",
        "there",
        "here",
        "it",
        "as",
        "at",
        "of",
        "in",
        "on",
        "to",
        "for",
        "with",
        "by",
        "from",
        "do",
        "does",
        "did",
        "have",
        "has",
        "had",
        "was",
        "were",
        "will",
        "would",
        "shall",
        "should",
        "can",
        "could",
        "may",
        "might",
        "must",
        "is",
        "are",
        "be",
        "been",
        "being",
        "yes",
        "no",
        "ok",
        "okay",
        "hi",
        "hey",
        "hello",
        "thanks",
        "thank",
        "please",
        "like",
        "going",
        "got",
        "get",
        "go",
        "went",
        "one",
        "two",
        "three",
        "first",
        "last",
        "today",
        "tomorrow",
        "yesterday",
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '\'') {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        let word = raw.strip_suffix("'s").unwrap_or(&raw);
        let upper_initial = chars[start].is_ascii_uppercase();
        // Sentence-initial: preceded by start-of-text or sentence punctuation.
        let prev = (0..start).rev().find(|&j| !chars[j].is_whitespace());
        let sentence_start = match prev {
            None => true,
            Some(j) => matches!(chars[j], '.' | '!' | '?' | '"' | '\u{201c}' | '\u{201d}'),
        };
        let w = word.to_lowercase();
        if upper_initial && !sentence_start && w.len() >= 2 && !ENTITY_STOP.contains(&w.as_str()) {
            out.push(w);
        }
    }
    out
}
