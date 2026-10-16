//! Long-horizon drift reporting (hardening §3.3): a pure-read, best-effort
//! analysis pass over any store — "系统会自我强化偏差吗？"的报表机器。
//!
//! Four sections:
//! 1. **Bias snapshot** — the three §2.2 BiasAudit detectors, run read-only
//!    (no `bias_flag` stamping; the consolidate stage owns the write path).
//! 2. **Trend drift** — per-task_tag weekly buckets over the trailing
//!    window: positive-outcome share, mean confidence, new-edge rate;
//!    week-over-week shifts beyond threshold are alerted.
//! 3. **Error propagation** (§2.3's long-term use) — invalidated edges that
//!    still have VALID followers in their influence chain.
//! 4. **Self-reinforcement signals** — zero-variance repeated decisions plus
//!    tags dominated by user_feedback (self-certifying loop suspicion).
//!
//! Everything degrades gracefully: a section that fails to compute is
//! reported as unavailable, never a panic and never a write.

use anyhow::Result;
use serde::Serialize;

use crate::store::{effective_polarity, CausalStore};

/// Tunables for the drift report. Defaults are aligned with the §2.2
/// BiasAudit knobs where the same quantity is involved.
#[derive(Debug, Clone)]
pub struct DriftOptions {
    /// Trend window in days (trailing, anchored at the newest event_time).
    pub days: u32,
    /// Trend bucket length in days (weekly by default).
    pub bucket_days: u32,
    /// Week-over-week positive-share shift that alerts (0.15 = the §2.2
    /// confidence-drift threshold's magnitude, same "meaningful shift" bar).
    pub polarity_shift: f64,
    /// Week-over-week mean-confidence shift that alerts.
    pub confidence_shift: f64,
    /// New-edge rate spike: latest bucket ≥ ratio × previous bucket.
    pub volume_ratio: f64,
    /// Minimum edges per bucket before rate/ratio alerts fire.
    pub min_bucket_edges: usize,
    /// user_feedback share within a tag that reads as self-certifying.
    pub feedback_share: f64,
    /// Minimum tag size for the feedback-share check.
    pub feedback_min_edges: usize,
    /// Top-N propagation paths listed.
    pub propagation_top: usize,
}

impl Default for DriftOptions {
    fn default() -> Self {
        Self {
            days: 28,
            bucket_days: 7,
            polarity_shift: 0.15,
            confidence_shift: 0.10,
            volume_ratio: 2.0,
            min_bucket_edges: 5,
            feedback_share: 0.5,
            feedback_min_edges: 5,
            propagation_top: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkewItem {
    pub task_tag: String,
    pub polarized: usize,
    pub dominant_ratio: f64,
    pub dominant_positive: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VarianceItem {
    pub decision_text: String,
    pub repetitions: usize,
    pub polarity: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BiasSnapshot {
    pub polarity_skew: Vec<SkewItem>,
    pub low_variance: Vec<VarianceItem>,
    pub confidence_drift: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketStat {
    /// Bucket start (unix seconds).
    pub from: i64,
    pub edges: usize,
    /// Positive share among edges with known polarity; None if none known.
    pub pos_ratio: Option<f64>,
    pub avg_confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagTrend {
    pub task_tag: String,
    pub buckets: Vec<BucketStat>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrendAlert {
    pub task_tag: String,
    /// "polarity_shift" | "confidence_shift" | "volume_spike"
    pub kind: &'static str,
    pub detail: String,
    pub magnitude: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PropagationPath {
    pub edge_id: i64,
    pub decision_text: String,
    pub outcome_text: String,
    pub invalidated_at: i64,
    /// (edge_id, decision_text, outcome_text) of valid followers.
    pub followers: Vec<(i64, String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReinforcementItem {
    /// "low_variance" | "user_feedback_share"
    pub kind: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftReport {
    pub generated_at: i64,
    pub window_days: u32,
    pub edges_total: usize,
    pub edges_valid: usize,
    pub bias: Option<BiasSnapshot>,
    pub trends: Vec<TagTrend>,
    pub trend_alerts: Vec<TrendAlert>,
    pub propagation: Vec<PropagationPath>,
    pub self_reinforcement: Vec<ReinforcementItem>,
    /// Sections that failed to compute (best-effort discipline).
    pub section_errors: Vec<String>,
}

const SECS_PER_DAY: i64 = 86_400;

/// Compute the full drift report. Pure read; per-section best-effort.
pub fn drift_report(store: &CausalStore, opts: &DriftOptions) -> DriftReport {
    let now = chrono::Utc::now().timestamp();
    let mut section_errors = Vec::new();

    let (edges_total, edges_valid) = store
        .with_conn(|conn| {
            let total: i64 =
                conn.query_row("SELECT COUNT(*) FROM causal_edges", [], |r| r.get(0))?;
            let valid: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NULL",
                [],
                |r| r.get(0),
            )?;
            Ok((total as usize, valid as usize))
        })
        .unwrap_or_else(|e| {
            section_errors.push(format!("edge counts: {e}"));
            (0, 0)
        });

    // ── ① Bias snapshot (read-only detectors, §2.2 default knobs) ───────
    let bias_cfg = crate::consolidate::ConsolidateConfig::default();
    let bias = match (
        store.audit_polarity_skew(bias_cfg.bias_min_tag_edges, bias_cfg.bias_skew_ratio),
        store.audit_low_variance_decisions(bias_cfg.bias_min_repetitions),
        store.audit_confidence_drift(bias_cfg.bias_drift_window, bias_cfg.bias_drift_threshold),
    ) {
        (Ok(skew), Ok(low_var), Ok(drift)) => Some(BiasSnapshot {
            polarity_skew: skew
                .into_iter()
                .map(|s| SkewItem {
                    task_tag: s.task_tag,
                    polarized: s.polarized,
                    dominant_ratio: s.dominant_ratio,
                    dominant_positive: s.dominant_positive,
                })
                .collect(),
            low_variance: low_var
                .into_iter()
                .map(|v| VarianceItem {
                    decision_text: v.decision_text,
                    repetitions: v.repetitions,
                    polarity: v.polarity,
                })
                .collect(),
            confidence_drift: drift.map(|d| {
                format!(
                    "recent {} mean confidence {:.2} vs historical {:.2} (delta {:+.2})",
                    d.window, d.recent_mean, d.historical_mean, d.delta
                )
            }),
        }),
        (a, b, c) => {
            for e in [a.err(), b.err(), c.err()].into_iter().flatten() {
                section_errors.push(format!("bias detectors: {e}"));
            }
            None
        }
    };

    // ── ② Trend drift (weekly buckets, anchored at newest event_time) ────
    let (trends, trend_alerts) = match compute_trends(store, opts) {
        Ok(v) => v,
        Err(e) => {
            section_errors.push(format!("trends: {e}"));
            (Vec::new(), Vec::new())
        }
    };

    // ── ③ Error propagation (invalidated edge → valid followers) ─────────
    let propagation = match compute_propagation(store, opts.propagation_top) {
        Ok(v) => v,
        Err(e) => {
            section_errors.push(format!("propagation: {e}"));
            Vec::new()
        }
    };

    // ── ④ Self-reinforcement signals ─────────────────────────────────────
    let self_reinforcement = match compute_reinforcement(store, opts, bias.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            section_errors.push(format!("reinforcement: {e}"));
            Vec::new()
        }
    };

    DriftReport {
        generated_at: now,
        window_days: opts.days,
        edges_total,
        edges_valid,
        bias,
        trends,
        trend_alerts,
        propagation,
        self_reinforcement,
        section_errors,
    }
}

fn compute_trends(
    store: &CausalStore,
    opts: &DriftOptions,
) -> Result<(Vec<TagTrend>, Vec<TrendAlert>)> {
    struct Row {
        tag: String,
        event_time: i64,
        confidence: f64,
        polarity: Option<bool>,
    }
    let (rows, anchor): (Vec<Row>, i64) = store.with_conn(|conn| {
        let anchor: Option<i64> = conn.query_row(
            "SELECT MAX(event_time) FROM causal_edges WHERE valid_to IS NULL",
            [],
            |r| r.get(0),
        )?;
        let Some(anchor) = anchor else {
            return Ok((Vec::new(), 0i64));
        };
        let window_start = anchor - i64::from(opts.days) * SECS_PER_DAY;
        let mut stmt = conn.prepare(
            "SELECT ce.task_tag, ce.event_time, ce.confidence, ce.outcome_polarity, ct.text
             FROM causal_edges ce
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL AND ce.task_tag IS NOT NULL
               AND ce.event_time >= ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![window_start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (tag, event_time, confidence, stored, text) = r?;
            out.push(Row {
                tag,
                event_time,
                confidence,
                polarity: effective_polarity(stored.as_deref(), &text),
            });
        }
        Ok((out, anchor))
    })?;
    if rows.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let bucket_len = i64::from(opts.bucket_days.max(1)) * SECS_PER_DAY;
    let bucket_count =
        ((i64::from(opts.days) * SECS_PER_DAY + bucket_len - 1) / bucket_len) as usize;
    let window_start = anchor - i64::from(opts.days) * SECS_PER_DAY;

    use std::collections::BTreeMap;
    let mut by_tag: BTreeMap<String, Vec<Vec<&Row>>> = BTreeMap::new();
    for row in &rows {
        let b =
            ((row.event_time - window_start).div_euclid(bucket_len) as usize).min(bucket_count - 1);
        by_tag
            .entry(row.tag.clone())
            .or_insert_with(|| vec![Vec::new(); bucket_count])[b]
            .push(row);
    }

    let mut trends = Vec::new();
    let mut alerts = Vec::new();
    for (tag, buckets) in by_tag {
        let stats: Vec<BucketStat> = buckets
            .iter()
            .enumerate()
            .map(|(i, rs)| {
                let known: Vec<bool> = rs.iter().filter_map(|r| r.polarity).collect();
                BucketStat {
                    from: window_start + i as i64 * bucket_len,
                    edges: rs.len(),
                    pos_ratio: if known.is_empty() {
                        None
                    } else {
                        Some(known.iter().filter(|p| **p).count() as f64 / known.len() as f64)
                    },
                    avg_confidence: if rs.is_empty() {
                        0.0
                    } else {
                        rs.iter().map(|r| r.confidence).sum::<f64>() / rs.len() as f64
                    },
                }
            })
            .collect();
        // Week-over-week alerts between consecutive buckets.
        for w in stats.windows(2) {
            let (prev, cur) = (&w[0], &w[1]);
            if let (Some(p), Some(c)) = (prev.pos_ratio, cur.pos_ratio) {
                let d = c - p;
                if d.abs() >= opts.polarity_shift
                    && prev.edges >= opts.min_bucket_edges
                    && cur.edges >= opts.min_bucket_edges
                {
                    alerts.push(TrendAlert {
                        task_tag: tag.clone(),
                        kind: "polarity_shift",
                        detail: format!(
                            "positive share {:.0}% → {:.0}% week-over-week",
                            p * 100.0,
                            c * 100.0
                        ),
                        magnitude: d,
                    });
                }
            }
            if prev.edges >= opts.min_bucket_edges && cur.edges >= opts.min_bucket_edges {
                let d = cur.avg_confidence - prev.avg_confidence;
                if d.abs() >= opts.confidence_shift {
                    alerts.push(TrendAlert {
                        task_tag: tag.clone(),
                        kind: "confidence_shift",
                        detail: format!(
                            "mean confidence {:.2} → {:.2} week-over-week",
                            prev.avg_confidence, cur.avg_confidence
                        ),
                        magnitude: d,
                    });
                }
                if prev.edges > 0 && cur.edges as f64 >= prev.edges as f64 * opts.volume_ratio {
                    alerts.push(TrendAlert {
                        task_tag: tag.clone(),
                        kind: "volume_spike",
                        detail: format!("new-edge rate {} → {} per week", prev.edges, cur.edges),
                        magnitude: cur.edges as f64 / prev.edges as f64,
                    });
                }
            }
        }
        trends.push(TagTrend {
            task_tag: tag,
            buckets: stats,
        });
    }
    alerts.sort_by(|a, b| {
        b.magnitude
            .abs()
            .partial_cmp(&a.magnitude.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((trends, alerts))
}

fn compute_propagation(store: &CausalStore, top: usize) -> Result<Vec<PropagationPath>> {
    let invalidated: Vec<(i64, String, String, i64)> = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT ce.id, cf.text, ct.text, ce.valid_to
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)
    })?;
    let mut paths: Vec<PropagationPath> = Vec::new();
    for (id, dec, out, valid_to) in invalidated {
        let followers = store.influenced_decisions(id)?;
        if !followers.is_empty() {
            paths.push(PropagationPath {
                edge_id: id,
                decision_text: dec,
                outcome_text: out,
                invalidated_at: valid_to,
                followers,
            });
        }
    }
    paths.sort_by_key(|p| std::cmp::Reverse(p.followers.len()));
    paths.truncate(top);
    Ok(paths)
}

fn compute_reinforcement(
    store: &CausalStore,
    opts: &DriftOptions,
    bias: Option<&BiasSnapshot>,
) -> Result<Vec<ReinforcementItem>> {
    let mut out = Vec::new();
    // (a) zero-variance repeats, straight from the bias snapshot (detector 2).
    if let Some(b) = bias {
        for v in &b.low_variance {
            out.push(ReinforcementItem {
                kind: "low_variance",
                detail: format!(
                    "\"{}\" recorded {}× with unchanging {} outcome",
                    v.decision_text,
                    v.repetitions,
                    if v.polarity { "positive" } else { "negative" }
                ),
            });
        }
    }
    // (b) user_feedback-dominated tags: the agent (or its operator) marking
    // its own outcomes as confirmed — a self-certifying loop when dominant.
    let shares: Vec<(String, i64, i64)> = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT task_tag, COUNT(*),
                    SUM(discovered_by = 'user_feedback')
             FROM causal_edges
             WHERE valid_to IS NULL AND task_tag IS NOT NULL
             GROUP BY task_tag",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)
    })?;
    for (tag, total, feedback) in shares {
        if total as usize >= opts.feedback_min_edges
            && feedback as f64 / total as f64 >= opts.feedback_share
        {
            out.push(ReinforcementItem {
                kind: "user_feedback_share",
                detail: format!(
                    "[{tag}] {feedback}/{total} edges self-confirmed (user_feedback {:.0}%)",
                    feedback as f64 * 100.0 / total as f64
                ),
            });
        }
    }
    Ok(out)
}
