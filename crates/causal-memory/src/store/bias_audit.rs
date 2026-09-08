//! Bias audit (hardening §2.2): statistical self-consistency checks over the
//! valid-edge population, run once per sleep-consolidation cycle.
//!
//! Three detectors, all purely observational — they flag SUSPICIOUS
//! structure for human review, never invalidate anything:
//!
//! 1. **Polarity skew per task_tag** — a tag whose recorded outcomes are
//!    almost all one direction (e.g. 12/13 positive) is either a genuinely
//!    rosy domain or a memory that stopped recording failures. Binomially,
//!    a 13-sample with one dissent is a ~0.2% event under a fair process.
//! 2. **Zero-variance repeated decisions** — the same decision recorded
//!    ≥3 times with the SAME outcome polarity every time is a
//!    self-reinforcement signature: the memory may be echoing itself rather
//!    than tracking the world. (Real repeated decisions usually see at
//!    least one mixed/neutral outcome eventually.)
//! 3. **Confidence drift** — the mean confidence of the most recent N edges
//!    vs everything older, shifted beyond a threshold, signals systematic
//!    over- or under-confidence in what the extractor/judge has been
//!    emitting lately. Report-only (no per-edge flag: no single edge is
//!    at fault).
//!
//! Flags persist on the edge row (`bias_flag`, v17) and are visible via
//! [`CausalStore::bias_flagged_edges`]; retrieval, decay, and GC ignore
//! them by design.

use anyhow::Result;

use crate::store::utils::effective_polarity;
use crate::store::CausalStore;

/// One task_tag whose outcome polarity distribution is suspiciously one-sided.
#[derive(Debug, Clone, PartialEq)]
pub struct PolaritySkew {
    pub task_tag: String,
    /// Edges in the tag with a KNOWN effective polarity (unknown ones don't
    /// count toward the distribution — they can't evidence either way).
    pub polarized: usize,
    /// Share of the dominant direction, 0.5..=1.0.
    pub dominant_ratio: f64,
    /// True when the dominant direction is success.
    pub dominant_positive: bool,
    /// All valid edge ids in this tag — the BiasAudit stage stamps these.
    pub edge_ids: Vec<i64>,
}

/// One decision chunk recorded repeatedly with an unchanging outcome polarity.
#[derive(Debug, Clone, PartialEq)]
pub struct LowVariancePattern {
    /// Decision text (the from-chunk), for human review.
    pub decision_text: String,
    /// Times this decision was recorded with a known polarity.
    pub repetitions: usize,
    /// The single polarity every recorded outcome agreed on.
    pub polarity: bool,
    /// The zero-variance edge ids — the BiasAudit stage stamps these.
    pub edge_ids: Vec<i64>,
}

/// Recent-vs-historical confidence mean shift. Report-only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfidenceDrift {
    pub window: usize,
    pub recent_mean: f64,
    pub historical_mean: f64,
    pub delta: f64,
}

/// One flagged edge, as listed for human review.
#[derive(Debug, Clone, PartialEq)]
pub struct FlaggedEdge {
    pub edge_id: i64,
    /// Which detector flagged it: `polarity_skew:<tag>` or
    /// `low_variance:<decision snippet>`.
    pub flag: String,
    pub decision_text: String,
}

impl CausalStore {
    /// Detector 1: task_tags whose known-polarity outcomes are one-sided.
    ///
    /// `min_edges` bounds the sample below which skew is meaningless;
    /// `skew_ratio` is the dominant-direction share at/above which the tag
    /// is flagged (0.9 = 90% one direction).
    pub fn audit_polarity_skew(
        &self,
        min_edges: usize,
        skew_ratio: f64,
    ) -> Result<Vec<PolaritySkew>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT ce.task_tag, ce.outcome_polarity, ct.text, ce.id
             FROM causal_edges ce
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL AND ce.task_tag IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;

        use std::collections::HashMap;
        // tag -> ((pos, neg), all valid edge ids in the tag)
        let mut tags: HashMap<String, ((usize, usize), Vec<i64>)> = HashMap::new();
        for row in rows {
            let (tag, stored, outcome_text, edge_id) = row?;
            let entry = tags.entry(tag).or_default();
            entry.1.push(edge_id);
            match effective_polarity(stored.as_deref(), &outcome_text) {
                Some(true) => entry.0 .0 += 1,
                Some(false) => entry.0 .1 += 1,
                None => {}
            }
        }

        let mut out = Vec::new();
        for (task_tag, ((pos, neg), edge_ids)) in tags {
            let polarized = pos + neg;
            if polarized < min_edges {
                continue;
            }
            let dominant = pos.max(neg);
            let ratio = dominant as f64 / polarized as f64;
            if ratio >= skew_ratio {
                out.push(PolaritySkew {
                    task_tag,
                    polarized,
                    dominant_ratio: ratio,
                    dominant_positive: pos >= neg,
                    edge_ids,
                });
            }
        }
        out.sort_by(|a, b| {
            b.dominant_ratio
                .partial_cmp(&a.dominant_ratio)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    /// Detector 2: decisions recorded ≥ `min_count` times where every
    /// recorded outcome with a known polarity agrees (zero variance).
    pub fn audit_low_variance_decisions(
        &self,
        min_count: usize,
    ) -> Result<Vec<LowVariancePattern>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT cf.text, ce.outcome_polarity, ct.text, ce.id
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;

        use std::collections::HashMap;

        /// Accumulator for one decision's repeated-outcome statistics.
        #[derive(Default)]
        struct Group {
            /// Recorded outcomes with a known polarity.
            known: usize,
            /// The first known polarity seen.
            polarity: Option<bool>,
            /// True once two known outcomes disagree (variance seen).
            mixed: bool,
            /// All valid edge ids for this decision (stamped when flagged).
            edge_ids: Vec<i64>,
        }

        let mut groups: HashMap<String, Group> = HashMap::new();
        for row in rows {
            let (decision, stored, outcome_text, edge_id) = row?;
            if let Some(p) = effective_polarity(stored.as_deref(), &outcome_text) {
                let g = groups.entry(decision).or_default();
                g.known += 1;
                g.edge_ids.push(edge_id);
                match g.polarity {
                    None => g.polarity = Some(p),
                    Some(prev) if prev != p => g.mixed = true,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        for (decision_text, g) in groups {
            if g.known >= min_count && !g.mixed {
                if let Some(polarity) = g.polarity {
                    out.push(LowVariancePattern {
                        decision_text,
                        repetitions: g.known,
                        polarity,
                        edge_ids: g.edge_ids,
                    });
                }
            }
        }
        out.sort_by(|a, b| b.repetitions.cmp(&a.repetitions));
        Ok(out)
    }

    /// Detector 3: mean-confidence shift between the most recent `window`
    /// valid edges and everything older. Returns None when the store has
    /// too little history to compare (<= window edges total) — drift is
    /// meaningless without a baseline. `threshold` is the absolute mean
    /// shift that counts as drift.
    pub fn audit_confidence_drift(
        &self,
        window: usize,
        threshold: f64,
    ) -> Result<Option<ConfidenceDrift>> {
        if window == 0 {
            return Ok(None);
        }
        let conn = self.acquire()?;
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NULL",
            [],
            |r| r.get(0),
        )?;
        if total <= window as i64 {
            return Ok(None); // no historical baseline
        }
        let recent: f64 = conn.query_row(
            "SELECT AVG(confidence) FROM (
                 SELECT confidence FROM causal_edges
                 WHERE valid_to IS NULL ORDER BY id DESC LIMIT ?1
             )",
            rusqlite::params![window as i64],
            |r| r.get(0),
        )?;
        let historical: f64 = conn.query_row(
            "SELECT AVG(confidence) FROM (
                 SELECT confidence FROM causal_edges
                 WHERE valid_to IS NULL ORDER BY id DESC LIMIT -1 OFFSET ?1
             )",
            rusqlite::params![window as i64],
            |r| r.get(0),
        )?;
        let delta = recent - historical;
        if delta.abs() < threshold {
            return Ok(None);
        }
        Ok(Some(ConfidenceDrift {
            window,
            recent_mean: recent,
            historical_mean: historical,
            delta,
        }))
    }

    /// Stamp a bias flag on one edge (BiasAudit write path; dry runs skip).
    pub fn set_bias_flag(&self, edge_id: i64, flag: &str) -> Result<()> {
        let conn = self.acquire()?;
        conn.execute(
            "UPDATE causal_edges SET bias_flag = ?1 WHERE id = ?2",
            rusqlite::params![flag, edge_id],
        )?;
        Ok(())
    }

    /// All currently flagged edges (any flag value), newest first — the
    /// human review queue.
    pub fn bias_flagged_edges(&self) -> Result<Vec<FlaggedEdge>> {
        let conn = self.acquire()?;
        let mut stmt = conn.prepare(
            "SELECT ce.id, ce.bias_flag, cf.text
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             WHERE ce.bias_flag IS NOT NULL
             ORDER BY ce.id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FlaggedEdge {
                edge_id: r.get(0)?,
                flag: r.get(1)?,
                decision_text: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
