//! Token estimation utilities (P6).
//!
//! `estimate_tokens` is a deterministic, dependency-free yardstick for
//! token-efficiency comparisons (raw vs RRF vs layered loading, per-question
//! context accounting in the bench harnesses). It is deliberately NOT a real
//! tokenizer — relative numbers are what matter, and a real tokenizer would
//! pull in a heavy dependency for a benchmark-only measurement.

/// Estimate the token cost of a text: CJK characters count ~1 token each
/// (they are dense in every real tokenizer), everything else ~1 token per 4
/// characters. Deterministic across platforms and versions.
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3000}'..='\u{303f}').contains(&c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_cjk_aware() {
        // ASCII: ~1 token per 4 chars (45 chars → 12).
        let ascii = estimate_tokens("hello world this is a longer english sentence");
        assert_eq!(ascii, 45usize.div_ceil(4));
        // CJK: ~1 token per char.
        let cjk = estimate_tokens("因果记忆系统测试中文分词");
        assert_eq!(cjk, 12);
        // Mixed: CJK counted per char, ASCII per 4 ("causal"=6→2, 记忆=2,
        // " system"=7→2 → 6).
        let mixed = estimate_tokens("causal记忆 system");
        assert_eq!(mixed, 6);
        // Empty and whitespace.
        assert_eq!(estimate_tokens(""), 0);
    }
}
