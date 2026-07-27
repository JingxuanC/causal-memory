//! BM25 keyword retrieval — pure Rust, zero external dependencies.
//!
//! Standard Okapi BM25 with the Robertson IDF variant:
//!
//! ```text
//! score(d, q) = Σ_{t ∈ q} IDF(t) · tf(t,d)·(k1+1) / (tf(t,d) + k1·(1 − b + b·|d|/avgdl))
//! IDF(t)      = ln((N − df(t) + 0.5) / (df(t) + 0.5) + 1)
//! ```
//!
//! with k1 = 1.2 (term-frequency saturation) and b = 0.75 (document-length
//! normalization) — the values used by Lucene/Elasticsearch and the BM25
//! literature (Robertson & Zaragoza, 2009). The "+1" inside the log keeps the
//! IDF non-negative even for terms present in more than half the corpus, so
//! every returned score is ≥ 0.
//!
//! Tokenization is NOT this module's job: callers pass pre-tokenized docs and
//! queries (in this crate, `patterns::tokenize` — English words minus stop
//! words, Chinese bigrams).

use std::collections::HashMap;

/// Term-frequency saturation parameter.
const K1: f64 = 1.2;
/// Document-length normalization parameter.
const B: f64 = 0.75;

/// An in-memory BM25 index over a small document collection.
///
/// Built per query over the candidate edge set (hundreds to low thousands of
/// documents) — at that scale a full rebuild costs microseconds and keeps the
/// IDF statistics exact for the filtered corpus, which a persisted global
/// index could not provide for per-task-tag queries.
pub struct Bm25Index {
    /// Document frequency per term (number of docs containing the term).
    df: HashMap<String, usize>,
    /// Per-document term frequencies, aligned with `keys`/`doc_lens`.
    doc_tfs: Vec<HashMap<String, usize>>,
    /// Document keys, aligned with `doc_tfs`.
    keys: Vec<String>,
    /// Token count per document, aligned with `doc_tfs`.
    doc_lens: Vec<usize>,
    /// Average document length (0.0 for an empty corpus).
    avgdl: f64,
}

impl Bm25Index {
    /// Build an index from `(doc_key, tokens)` pairs.
    pub fn build<I, S>(docs: I) -> Self
    where
        I: IntoIterator<Item = (String, Vec<S>)>,
        S: AsRef<str>,
    {
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut keys = Vec::new();
        let mut doc_tfs = Vec::new();
        let mut doc_lens = Vec::new();
        let mut total_len = 0usize;

        for (key, tokens) in docs {
            let mut tf: HashMap<String, usize> = HashMap::new();
            let mut len = 0usize;
            for tok in &tokens {
                *tf.entry(tok.as_ref().to_string()).or_insert(0) += 1;
                len += 1;
            }
            for term in tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            keys.push(key);
            doc_tfs.push(tf);
            doc_lens.push(len);
            total_len += len;
        }

        let n = doc_lens.len();
        let avgdl = if n == 0 {
            0.0
        } else {
            total_len as f64 / n as f64
        };
        Self {
            df,
            doc_tfs,
            keys,
            doc_lens,
            avgdl,
        }
    }

    /// Robertson IDF: `ln((N − df + 0.5) / (df + 0.5) + 1)`. Always ≥ 0.
    fn idf(&self, df: usize) -> f64 {
        let n = self.doc_lens.len() as f64;
        let df = df as f64;
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }

    /// Score `query_tokens` against every document and return the top `limit`
    /// `(doc_key, score)` pairs, score descending. Ties break by document key
    /// for deterministic output. Documents sharing no term with the query are
    /// omitted; a fully out-of-vocabulary query returns an empty vec.
    pub fn search(&self, query_tokens: &[String], limit: usize) -> Vec<(String, f64)> {
        let mut scores: Vec<(usize, f64)> = Vec::new();
        for (doc_idx, tf) in self.doc_tfs.iter().enumerate() {
            let mut score = 0.0;
            for term in query_tokens {
                let (Some(&df), Some(&term_freq)) = (self.df.get(term), tf.get(term)) else {
                    continue;
                };
                let idf = self.idf(df);
                let tf = term_freq as f64;
                let dl = self.doc_lens[doc_idx] as f64;
                let denom = tf + K1 * (1.0 - B + B * dl / self.avgdl);
                score += idf * (tf * (K1 + 1.0)) / denom;
            }
            if score > 0.0 {
                scores.push((doc_idx, score));
            }
        }
        scores.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.keys[a.0].cmp(&self.keys[b.0]))
        });
        scores
            .into_iter()
            .take(limit)
            .map(|(idx, score)| (self.keys[idx].clone(), score))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    fn build_docs(docs: &[(&str, &[&str])]) -> Bm25Index {
        Bm25Index::build(
            docs.iter()
                .map(|(k, ts)| (k.to_string(), toks(ts)))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn test_exact_term_hit_ranks_first() {
        let index = build_docs(&[
            ("redis", &["redis", "cache", "stampede", "protection"]),
            ("mysql", &["mysql", "query", "planner", "index"]),
            ("kafka", &["kafka", "consumer", "lag", "spike"]),
        ]);
        let res = index.search(&toks(&["redis"]), 10);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, "redis");
        assert!(res[0].1 > 0.0);
    }

    #[test]
    fn test_tf_saturation() {
        // A doc repeating the term 100× must beat a single mention, but the
        // gain is sub-linear: BM25 saturates tf at tf·(k1+1)/(tf+k1) → k1+1.
        let repeated: Vec<String> = std::iter::repeat_n("redis".to_string(), 100).collect();
        let index = Bm25Index::build(vec![
            ("spam".to_string(), repeated),
            ("plain".to_string(), toks(&["redis", "cache"])),
        ]);
        let res = index.search(&toks(&["redis"]), 10);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].0, "spam", "more tf still scores higher");
        // Saturation bound: single-term score ≤ idf·(k1+1); with N=2, df=1:
        // idf = ln((2−1+0.5)/1.5 + 1) = ln(2) ≈ 0.693, bound ≈ 0.693·2.2 ≈ 1.52.
        let bound = (2.0f64).ln() * (K1 + 1.0);
        assert!(
            res[0].1 < bound + 1e-9,
            "100× tf must saturate, got {} (bound {bound})",
            res[0].1
        );
        // ...and a linear (unsaturated) model would give 100× the tf=1 score
        // of the short doc; saturation keeps it far below that.
        let plain_score = res[1].1;
        assert!(
            res[0].1 < 10.0 * plain_score,
            "spam ({}) must be far below 10× the single-mention score ({plain_score})",
            res[0].1
        );
    }

    #[test]
    fn test_length_normalization() {
        // Same single occurrence of "redis": the short doc must outrank the
        // long one (b = 0.75 penalizes length).
        let long_tokens: Vec<String> = std::iter::once("redis".to_string())
            .chain(std::iter::repeat_n("filler".to_string(), 50))
            .collect();
        let index = Bm25Index::build(vec![
            ("short".to_string(), toks(&["redis", "cache"])),
            ("long".to_string(), long_tokens),
        ]);
        let res = index.search(&toks(&["redis"]), 10);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].0, "short", "short doc must outrank long doc");
        assert!(res[0].1 > res[1].1);
    }

    #[test]
    fn test_rare_term_outweighs_common_term() {
        // "cache" appears in all 3 docs (idf low), "stampede" in only 1
        // (idf high): a doc matching only the rare term beats a doc matching
        // only the common term.
        let index = build_docs(&[
            ("rare", &["stampede", "mitigation"]),
            ("common", &["cache", "warm"]),
            ("also_common", &["cache", "cold"]),
            ("third", &["cache", "hit", "ratio"]),
        ]);
        let res = index.search(&toks(&["stampede", "cache"]), 10);
        assert_eq!(res[0].0, "rare", "rare-term match must rank first");
        // Non-negative scores everywhere (Robertson +1 keeps idf ≥ 0 even for
        // terms in > N/2 docs).
        assert!(res.iter().all(|(_, s)| *s >= 0.0));
    }

    #[test]
    fn test_oov_query_returns_empty() {
        let index = build_docs(&[("a", &["redis", "cache"]), ("b", &["mysql", "index"])]);
        assert!(index.search(&toks(&["nonexistent", "zzzz"]), 10).is_empty());
        assert!(index.search(&[], 10).is_empty());
    }

    #[test]
    fn test_multi_term_query_aggregates() {
        let index = build_docs(&[
            ("both", &["redis", "cache", "stampede"]),
            ("one", &["redis", "mutex", "lock"]),
            ("none", &["mysql", "query", "planner"]),
        ]);
        let res = index.search(&toks(&["redis", "stampede"]), 10);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].0, "both", "matching both terms beats one term");
        assert_eq!(res[1].0, "one");
    }

    #[test]
    fn test_limit_and_deterministic_ties() {
        let index = build_docs(&[
            ("b_doc", &["redis", "cache"]),
            ("a_doc", &["redis", "cache"]),
            ("c_doc", &["redis", "cache"]),
        ]);
        let res = index.search(&toks(&["redis"]), 2);
        assert_eq!(res.len(), 2, "limit truncates");
        // Identical scores → key order breaks the tie.
        assert_eq!(res[0].0, "a_doc");
        assert_eq!(res[1].0, "b_doc");
    }

    #[test]
    fn test_empty_and_empty_token_docs() {
        let empty = Bm25Index::build(Vec::<(String, Vec<String>)>::new());
        assert!(empty.search(&toks(&["redis"]), 10).is_empty());
        // A doc with zero tokens must not divide by zero (avgdl > 0 path).
        let index = Bm25Index::build(vec![
            ("empty".to_string(), Vec::<String>::new()),
            ("real".to_string(), toks(&["redis"])),
        ]);
        let res = index.search(&toks(&["redis"]), 10);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, "real");
    }

    #[test]
    fn test_chinese_bigram_docs() {
        // Caller-side tokenization (patterns::tokenize) produces bigrams;
        // the index itself is token-agnostic.
        let index = build_docs(&[
            ("zh", &["缓存", "存击", "击穿", "redis"]),
            ("en", &["mysql", "index", "scan"]),
        ]);
        let res = index.search(&toks(&["缓存", "击穿"]), 10);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, "zh");
    }
}
