//! Chunk-level fused retrieval (Cycle 2 Batch 3): a self-contained
//! BM25 ⊕ vector path that answers with RAW TEXT, with no graph involvement.
//!
//! Why it exists: the unified spreading-activation engine
//! (`unified_spread_hits`) is built for agent-shaped memory — it seeds on
//! facts/chunks, spreads over typed causal edges, and materializes facts and
//! causal LESSONS. The AMC text track asks a different question: "which
//! passage of the ingested conversation is the evidence?" A raw turn is a
//! graph node there, but a spread hit returns the EDGE the turn sits on, not
//! the turn. This path returns the turn itself.
//!
//! Deliberate differences from the other retrieval entry points in this
//! module (which are side-effecting by design, and must stay that way):
//! - **no** `ensure_graph_current` / `maybe_rebuild_graph`: the graph is
//!   never touched, so this path can never pay (or trigger) an O(store)
//!   rebuild;
//! - **no** `buffer_cooccurrences`: no Hebbian learning is recorded;
//! - **no** recall metrics / audit row: a pure read, so A/B arms can be
//!   interleaved without one arm's side effects polluting the other.
//! What it does cost is bounded: two index-backed legs plus one brute-force
//! cosine scan of `chunk_embeddings` (the same scan the unified seed layer
//! already runs).

use std::collections::HashMap;

use super::format::rrf_fuse_many;
use super::ops::MemoryHit;
use super::{block_on, Memory};

/// Candidate depth per leg. RRF only needs the top of each list, but a leg
/// truncated to exactly `limit` starves the fusion: an item ranked 3rd by
/// BM25 and 1st by the vectors is the whole point of fusing, and it is
/// invisible if both legs hand over only one candidate.
const FUSED_LEG_DEPTH: usize = 50;

/// Hard ceiling on the leg depth. Materialization issues one `IN (…)` with a
/// host variable per surviving id, and SQLite's default host-variable floor
/// is 999 — a caller asking for 10k hits must not break the statement.
const FUSED_MAX_DEPTH: usize = 200;

impl Memory {
    /// Chunk-level fused retrieval: BM25 ⊕ chunk-vector RRF, materialized as
    /// the raw chunk text. Returns the hits plus the mode tag (`"fused"`).
    ///
    /// Both legs are ranked INDEPENDENTLY and fused by Reciprocal Rank
    /// Fusion — `score(key) = Σ_legs 1 / (RRF_K + rank)`, ranks 1-based —
    /// so an item that only one leg can see still outranks an item both legs
    /// place low. A missing embedder (no `local-embed` feature, no
    /// `CAUSAL_MEMORY_EMBED_API`) simply empties the semantic leg and leaves
    /// the BM25 ranking untouched: the formula above already degenerates to
    /// pure-BM25 scoring for a single list.
    ///
    /// The BM25 leg is `CausalStore::bm25_seed_ids`, not
    /// `bm25_candidates_and_rank`: the latter ranks causal EDGES and filters
    /// the index to the non-`fact:` namespace, i.e. it cannot rank a raw
    /// turn at all. `bm25_seed_ids` ranks chunk ids across every namespace,
    /// and that is kept as-is — no `raw:`-only filter — because the index is
    /// the honest candidate set; ids that turn out to have no `chunks` row
    /// (the `fact:{id}` namespace indexes a fact's key/value but writes no
    /// chunk row) are dropped at materialization, where "原文证据" is the
    /// only thing this path can return. Their ranks are still spent, so a
    /// fact hit vacates a position rather than shifting the chunks below it.
    pub fn search_chunks_fused(&self, query: &str, limit: usize) -> (Vec<MemoryHit>, &'static str) {
        if limit == 0 {
            return (Vec::new(), "fused");
        }
        let depth = limit.max(FUSED_LEG_DEPTH).min(FUSED_MAX_DEPTH);

        let bm25 = self
            .store
            .bm25_seed_ids(query, None, depth)
            .unwrap_or_default();
        // `scope = None`: chunk evidence is scope-free (same rule as the
        // unified seed layer).
        // QUERY SIDE: `embed_shared_query` adds the model's query instruction
        // (BGE-en needs it; the stored passage vectors are bare).
        let semantic: Vec<String> = match block_on(crate::embed::embed_shared_query(query)) {
            Some(Ok(vec)) => self
                .store
                .search_chunks_semantic(&vec, depth)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            // No embedder, or the endpoint failed: the leg is empty and the
            // fusion degrades to the BM25 ranking.
            _ => Vec::new(),
        };

        let fused = fuse_ranked_ids(&bm25, &semantic);
        let ids: Vec<String> = fused.iter().map(|(id, _)| id.clone()).collect();
        let rows = self.chunk_rows_by_ids(&ids);

        let mut hits: Vec<MemoryHit> = Vec::new();
        for (id, score) in fused {
            if hits.len() >= limit {
                break;
            }
            if let Some((text, created_at)) = rows.get(&id) {
                hits.push(MemoryHit {
                    key: id,
                    content: text.clone(),
                    score,
                    // Position in the RETURNED list: `score` already carries
                    // the true fused rank, and the caller (the AMC response
                    // contract) reads `rank` as "order of this row".
                    rank: hits.len() + 1,
                    created_at: Some(*created_at),
                });
            }
        }
        (hits, "fused")
    }

    /// Options-expanded fused retrieval: the main query PLUS one retrieval
    /// per option, merged by EQUAL-weight RRF.
    ///
    /// Why this exists: the AMC contract's `/search` carries a choice
    /// question's `options` alongside the query. The query alone is often
    /// oblique ("what do I normally have in the morning") while the correct
    /// option names the evidence's own vocabulary ("oat milk") — so the
    /// options are extra probes that can reach a passage the query cannot.
    ///
    /// Equal weights are the whole trick: an evidence chunk that only ONE of
    /// the options (usually a distractor, whose wording also matches the
    /// conversation's topic) drags in collects a single `1/(K+rank)`
    /// contribution, while the chunk the main query AND the correct option
    /// both rank collects two and floats above it. Weighting by any guess at
    /// option quality would need an oracle this layer does not have.
    ///
    /// COMPLIANCE: option text is never returned as evidence. Every list
    /// merged here comes from [`Memory::search_chunks_fused`], whose hits are
    /// materialized from `chunks.text` rows — so `content` can only ever be
    /// ingested conversation text, never a caller-supplied option string.
    ///
    /// Sequential on purpose: `embed_shared` holds the process-global
    /// embedder mutex across its await (deliberate serialization), so
    /// "parallel" option probes would just queue on that mutex — with the
    /// extra cost of N parked threads.
    pub fn search_chunks_fused_with_options(
        &self,
        query: &str,
        options: &[String],
        limit: usize,
    ) -> (Vec<MemoryHit>, &'static str) {
        let options = sanitize_query_options(options);
        if limit == 0 || options.is_empty() {
            // Nothing to expand: byte-identical to the plain fused path.
            return self.search_chunks_fused(query, limit);
        }
        let mut lists: Vec<Vec<MemoryHit>> = Vec::with_capacity(options.len() + 1);
        lists.push(self.search_chunks_fused(query, limit).0);
        for option in &options {
            lists.push(self.search_chunks_fused(option, limit).0);
        }
        let refs: Vec<&[MemoryHit]> = lists.iter().map(Vec::as_slice).collect();
        (Self::rrf_merge_hits(&refs, limit), "fused+options")
    }

    /// Fuse N already-ranked hit lists into one, by key, with the same RRF
    /// formula the core retrieval paths use. The AMC `merge` arm runs the
    /// spread engine and [`Memory::search_chunks_fused`] and stitches them
    /// here: a key present in both lists sums both contributions and floats
    /// up, a key in one keeps its single-leg score. Content/metadata come
    /// from the first list that carried the key; `score`/`rank` are the
    /// fused ones. Ties keep first-seen order (stable sort).
    pub fn rrf_merge_hits(lists: &[&[MemoryHit]], limit: usize) -> Vec<MemoryHit> {
        if limit == 0 {
            return Vec::new();
        }
        let keys: Vec<Vec<String>> = lists
            .iter()
            .map(|list| list.iter().map(|h| h.key.clone()).collect())
            .collect();
        let refs: Vec<&[String]> = keys.iter().map(|k| k.as_slice()).collect();
        let fused = rrf_fuse_many(&refs);

        let mut by_key: HashMap<&str, &MemoryHit> = HashMap::new();
        for list in lists {
            for hit in *list {
                by_key.entry(hit.key.as_str()).or_insert(hit);
            }
        }
        fused
            .into_iter()
            .enumerate()
            .filter_map(|(rank, (key, score))| {
                let hit = by_key.get(key.as_str())?;
                Some(MemoryHit {
                    key,
                    content: hit.content.clone(),
                    score,
                    rank: rank + 1,
                    created_at: hit.created_at,
                })
            })
            .take(limit)
            .collect()
    }

    /// `id → (text, created_at)` for the ids that actually have a `chunks`
    /// row. One statement, ids bound as host parameters (`ids` is bounded by
    /// [`FUSED_MAX_DEPTH`], so the variable count is safe).
    fn chunk_rows_by_ids(&self, ids: &[String]) -> HashMap<String, (String, i64)> {
        if ids.is_empty() {
            return HashMap::new();
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT id, text, created_at FROM chunks WHERE id IN ({placeholders})");
        self.store
            .with_conn(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                let collected: Vec<(String, String, i64)> =
                    rows.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(collected
                    .into_iter()
                    .map(|(id, text, created_at)| (id, (text, created_at)))
                    .collect())
            })
            .unwrap_or_default()
    }
}

/// RRF over the two ranked id legs. Private and pure so the fusion itself is
/// testable without an embedder (`init_embedder` is forced to `None` under
/// `cfg(test)`, so a test can never reach the semantic leg through the
/// vector store).
pub(super) fn fuse_ranked_ids(bm25: &[String], semantic: &[String]) -> Vec<(String, f64)> {
    rrf_fuse_many(&[bm25, semantic])
}

/// Hard ceiling on options honored per request. Each option costs a full
/// retrieval (two indexed legs plus one brute-force vector scan), so the
/// list length is a latency amplifier controlled by an external caller. The
/// contract's choice questions carry 3-5 options; 8 leaves headroom without
/// inviting abuse.
pub const MAX_QUERY_OPTIONS: usize = 8;

/// Normalize a request's `options` before they drive retrieval: trim each,
/// drop the blanks, drop exact duplicates (first occurrence wins, order
/// preserved), and truncate at [`MAX_QUERY_OPTIONS`] with one stderr note.
/// Pure, so it is unit-testable with no store and no embedder.
fn sanitize_query_options(options: &[String]) -> Vec<String> {
    let mut cleaned: Vec<String> = Vec::with_capacity(options.len().min(MAX_QUERY_OPTIONS));
    for raw in options {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if cleaned.iter().any(|seen| seen.as_str() == trimmed) {
            continue;
        }
        if cleaned.len() == MAX_QUERY_OPTIONS {
            eprintln!(
                "amc/search: {MAX_QUERY_OPTIONS}+ options — truncating the rest of the {} supplied",
                options.len()
            );
            break;
        }
        cleaned.push(trimmed.to_string());
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_sanitize_options_trims_drops_blanks_and_dedups() {
        let cleaned = sanitize_query_options(&owned(&[
            "  oat milk  ",
            "",
            "   ",
            "oat milk",
            "green tea",
            "green tea",
        ]));
        assert_eq!(cleaned, owned(&["oat milk", "green tea"]));
    }

    #[test]
    fn test_sanitize_options_caps_at_eight() {
        let twelve: Vec<String> = (0..12).map(|i| format!("option {i}")).collect();
        let cleaned = sanitize_query_options(&twelve);
        assert_eq!(cleaned.len(), MAX_QUERY_OPTIONS);
        assert_eq!(cleaned[0], "option 0", "order preserved from the front");
        assert_eq!(cleaned[MAX_QUERY_OPTIONS - 1], "option 7");
    }

    #[test]
    fn test_sanitize_options_empty_input() {
        assert!(sanitize_query_options(&[]).is_empty());
        assert!(sanitize_query_options(&owned(&["", "  "])).is_empty());
    }

    /// Equal-weight fusion is the mechanism that suppresses distractor
    /// noise: a chunk only one option pulls in keeps a single contribution,
    /// while a chunk the query and a second list both rank sums two.
    #[test]
    fn test_rrf_merge_equal_weight_consensus_wins() {
        let hit = |key: &str| MemoryHit {
            key: key.to_string(),
            content: format!("text of {key}"),
            score: 0.0,
            rank: 1,
            created_at: Some(0),
        };
        // `both` is rank 1 in two lists; `lone` is rank 1 in one list only.
        let a = vec![hit("both"), hit("lone")];
        let b = vec![hit("both")];
        let merged = Memory::rrf_merge_hits(&[a.as_slice(), b.as_slice()], 10);
        assert_eq!(merged[0].key, "both", "2 contributions beat 1");
        assert!(merged[0].score > merged[1].score);
        assert_eq!(merged[1].key, "lone");
    }
}
