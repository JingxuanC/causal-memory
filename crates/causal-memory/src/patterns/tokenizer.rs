//! Text tokenization and similarity utilities for pattern mining.

use std::collections::{HashMap, HashSet};

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
