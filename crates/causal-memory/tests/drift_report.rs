//! Drift report synthetic validation (hardening §3.3): two stores, zero LLM.
//! (a) healthy store — balanced polarity, multiple tags, real variance:
//!     the report must show NO alert in any section;
//! (b) drifted store — injected drift: one tag's polarity slides 50% → 95%
//!     across weeks, an invalidated edge keeps 3 valid influenced followers,
//!     one decision repeats 5× with zero variance, one tag is dominated by
//!     user_feedback. Every section must catch its injection.

use causal_memory::drift::{drift_report, DriftOptions};
use causal_memory::store::CausalStore;

const DAY: i64 = 86_400;
const T0: i64 = 1_700_000_000; // fixed anchor; window = [T0, T0 + 28d]

#[allow(clippy::too_many_arguments)]
fn rec(
    store: &CausalStore,
    decision: &str,
    outcome: &str,
    tag: &str,
    polarity: &str,
    conf: f64,
    discovered_by: &str,
    t: i64,
    influenced_by: Option<&[i64]>,
) -> i64 {
    store
        .record_decision_full(
            decision,
            outcome,
            "caused",
            Some(tag),
            conf,
            discovered_by,
            t,
            Some(polarity),
            None,
            influenced_by,
        )
        .unwrap()
        .1
}

/// (a) Healthy store: two tags, ~50/50 polarity every week, repeated
/// decisions WITH variance, llm_inferred dominance. Decision texts are
/// unique per week (no chunk reuse), and the repeated decision writes
/// positives before negatives — the write-path contradiction rule
/// (old-negative → new-positive) must never fire during setup.
fn healthy_store() -> CausalStore {
    let store = CausalStore::open_in_memory().unwrap();
    for week in 0..4 {
        for i in 0..6 {
            let t = T0 + (week * 7 + i) * DAY;
            for tag in ["deploy", "caching"] {
                // Alternate polarity → balanced ratio, real variance.
                let pol = if i % 2 == 0 { "positive" } else { "negative" };
                rec(
                    &store,
                    &format!("{tag} change w{week}-{i}"),
                    &format!("{tag} outcome w{week}-{i}"),
                    tag,
                    pol,
                    0.7,
                    "llm_inferred",
                    t,
                    None,
                );
            }
        }
    }
    // A repeated decision with MIXED outcomes — no zero-variance flag.
    // Positives first: a positive write after a negative one would
    // soft-invalidate the negative (contradiction short-circuit).
    for (i, pol) in ["positive", "positive", "positive", "negative", "negative"]
        .iter()
        .enumerate()
    {
        rec(
            &store,
            "nightly index rebuild",
            &format!("rebuild round {i}"),
            "database",
            pol,
            0.7,
            "rule",
            T0 + (i as i64) * DAY,
            None,
        );
    }
    store
}

/// (b) Drifted store with one injected anomaly per report section.
fn drifted_store() -> (CausalStore, i64) {
    let store = CausalStore::open_in_memory().unwrap();

    // Trend injection: tag "deploy" slides 50% → ~100% positive; "caching"
    // stays balanced as a control.
    for week in 0..4 {
        for i in 0..6 {
            let t = T0 + (week * 7 + i) * DAY;
            let deploy_pol = if week < 2 {
                if i % 2 == 0 {
                    "positive"
                } else {
                    "negative"
                } // 50%
            } else {
                "positive" // 100% in the later weeks
            };
            rec(
                &store,
                &format!("deploy change {week}-{i}"),
                &format!("deploy outcome {week}-{i}"),
                "deploy",
                deploy_pol,
                0.7,
                "llm_inferred",
                t,
                None,
            );
            rec(
                &store,
                &format!("caching change {week}-{i}"),
                &format!("caching outcome {week}-{i}"),
                "caching",
                if i % 2 == 0 { "positive" } else { "negative" },
                0.7,
                "llm_inferred",
                t,
                None,
            );
        }
    }

    // Propagation injection: a bad lesson, invalidated, with 3 valid
    // followers that cited it via influenced_by (§2.3 chain).
    let bad = rec(
        &store,
        "always retry on timeout",
        "retry storm masked the bug",
        "ops",
        "negative",
        0.7,
        "rule",
        T0 + 3 * DAY,
        None,
    );
    for i in 0..3 {
        rec(
            &store,
            &format!("added retry to worker {i}"),
            &format!("worker {i} incident"),
            "ops",
            "negative",
            0.6,
            "rule",
            T0 + (10 + i) * DAY,
            Some(&[bad]),
        );
    }
    store.invalidate_edge(bad).unwrap();

    // Reinforcement injection (a): zero-variance repeated decision.
    for i in 0..5 {
        rec(
            &store,
            "cleared the cache manually",
            &format!("manual clear {i} worked"),
            "ops",
            "positive",
            0.6,
            "llm_inferred",
            T0 + (15 + i) * DAY,
            None,
        );
    }
    // Reinforcement injection (b): a tag dominated by user_feedback.
    for i in 0..6 {
        rec(
            &store,
            &format!("self-certified change {i}"),
            &format!("self-certified outcome {i}"),
            "selfcert",
            if i % 2 == 0 { "positive" } else { "negative" },
            0.9,
            "user_feedback",
            T0 + (20 + i) * DAY,
            None,
        );
    }
    (store, bad)
}

#[test]
fn healthy_store_reports_no_alerts() {
    let store = healthy_store();
    let report = drift_report(&store, &DriftOptions::default());

    let bias = report.bias.as_ref().expect("bias snapshot");
    assert!(
        bias.polarity_skew.is_empty(),
        "skew: {:?}",
        bias.polarity_skew.len()
    );
    assert!(bias.low_variance.is_empty(), "low variance must not fire");
    assert!(
        bias.confidence_drift.is_none(),
        "confidence drift must not fire"
    );
    assert!(
        report.trend_alerts.is_empty(),
        "healthy store must have no trend alerts: {:?}",
        report
            .trend_alerts
            .iter()
            .map(|a| &a.detail)
            .collect::<Vec<_>>()
    );
    assert!(report.propagation.is_empty(), "no propagation paths");
    assert!(
        report.self_reinforcement.is_empty(),
        "no reinforcement signals: {:?}",
        report
            .self_reinforcement
            .iter()
            .map(|r| &r.detail)
            .collect::<Vec<_>>()
    );
    assert!(
        report.section_errors.is_empty(),
        "{:?}",
        report.section_errors
    );
}

#[test]
fn drifted_store_catches_each_injection() {
    let (store, bad_edge) = drifted_store();
    let report = drift_report(&store, &DriftOptions::default());
    assert!(
        report.section_errors.is_empty(),
        "{:?}",
        report.section_errors
    );

    // ② trend: the deploy polarity slide is caught; the balanced control
    // tag must NOT alert.
    let shift = report
        .trend_alerts
        .iter()
        .filter(|a| a.kind == "polarity_shift" && a.task_tag == "deploy")
        .count();
    assert!(shift >= 1, "deploy polarity slide must alert: {report:?}");
    assert!(
        !report
            .trend_alerts
            .iter()
            .any(|a| a.kind == "polarity_shift" && a.task_tag == "caching"),
        "balanced control tag must not alert"
    );

    // ③ propagation: the invalidated lesson lists its 3 valid followers.
    let path = report
        .propagation
        .iter()
        .find(|p| p.edge_id == bad_edge)
        .expect("invalidated edge with followers must appear");
    assert_eq!(path.followers.len(), 3);
    assert!(path.decision_text.contains("always retry"));

    // ① + ④: zero-variance repeat caught by both the bias snapshot and the
    // reinforcement section; the user_feedback-dominated tag is flagged.
    let bias = report.bias.as_ref().expect("bias snapshot");
    assert!(
        bias.low_variance
            .iter()
            .any(|v| v.decision_text == "cleared the cache manually"),
        "low_variance detector must hit"
    );
    assert!(
        report
            .self_reinforcement
            .iter()
            .any(|r| r.kind == "low_variance" && r.detail.contains("cleared the cache manually")),
        "reinforcement section must surface the zero-variance repeat"
    );
    assert!(
        report
            .self_reinforcement
            .iter()
            .any(|r| r.kind == "user_feedback_share" && r.detail.contains("selfcert")),
        "self-certifying tag must be flagged"
    );
}

#[test]
fn drift_report_never_panics_on_empty_store() {
    let store = CausalStore::open_in_memory().unwrap();
    let report = drift_report(&store, &DriftOptions::default());
    assert_eq!(report.edges_total, 0);
    assert!(report.trends.is_empty());
    assert!(report.propagation.is_empty());
    // JSON rendering must work for every section state.
    serde_json::to_string(&report).unwrap();
}
