//! `drift` subcommand (hardening §3.3): long-horizon drift report for any
//! store. Pure read, best-effort — the analysis lives in
//! `causal_memory::drift`; this file is flag parsing + text/JSON rendering.

use std::path::PathBuf;

use anyhow::Result;
use causal_memory::drift::{drift_report, DriftOptions, DriftReport};
use causal_memory::store::CausalStore;

use crate::get_db_path;

fn truncate60(s: &str) -> String {
    s.chars().take(60).collect()
}

const USAGE: &str = "Usage: causal-memory drift [--db <PATH>] [--json] [--days N]
                     [--polarity-shift F] [--confidence-shift F]
                     [--volume-ratio F] [--feedback-share F]";

fn render_text(report: &DriftReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "=== Drift Report (hardening §3.3) ===\nwindow: last {} day(s); edges: {} total / {} valid\n",
        report.window_days, report.edges_total, report.edges_valid
    ));

    // ① Bias snapshot (BiasAudit detectors, read-only).
    out.push_str("\n① Bias audit snapshot (read-only, no flags stamped):\n");
    match &report.bias {
        None => out.push_str("  (unavailable — see section errors)\n"),
        Some(b)
            if b.polarity_skew.is_empty()
                && b.low_variance.is_empty()
                && b.confidence_drift.is_none() =>
        {
            out.push_str("  ✅ no suspicious patterns detected\n");
        }
        Some(b) => {
            for s in &b.polarity_skew {
                out.push_str(&format!(
                    "  ⚠️ polarity skew [{}]: {:.0}% {} over {} polarized edge(s)\n",
                    s.task_tag,
                    s.dominant_ratio * 100.0,
                    if s.dominant_positive {
                        "positive"
                    } else {
                        "negative"
                    },
                    s.polarized
                ));
            }
            for v in &b.low_variance {
                out.push_str(&format!(
                    "  ⚠️ low variance: \"{}\" repeated {}× with unchanging outcome\n",
                    v.decision_text, v.repetitions
                ));
            }
            if let Some(d) = &b.confidence_drift {
                out.push_str(&format!("  ⚠️ confidence drift: {d}\n"));
            }
        }
    }

    // ② Trend drift.
    out.push_str("\n② Trend drift (week-over-week per task_tag):\n");
    if report.trend_alerts.is_empty() {
        out.push_str("  ✅ no week-over-week shift beyond threshold\n");
    } else {
        for a in &report.trend_alerts {
            out.push_str(&format!(
                "  ⚠️ [{}] {}: {} (magnitude {:+.2})\n",
                a.task_tag, a.kind, a.detail, a.magnitude
            ));
        }
    }
    for t in &report.trends {
        let series: Vec<String> = t
            .buckets
            .iter()
            .map(|b| {
                let pr = b
                    .pos_ratio
                    .map(|r| format!("{:.0}%+", r * 100.0))
                    .unwrap_or_else(|| "—".into());
                format!("{}e/{}/c{:.2}", b.edges, pr, b.avg_confidence)
            })
            .collect();
        out.push_str(&format!("  [{}] {}\n", t.task_tag, series.join(" → ")));
    }

    // ③ Error propagation (§2.3 influence chains).
    out.push_str(
        "\n③ Error propagation (invalidated memory → still-valid influenced decisions):\n",
    );
    if report.propagation.is_empty() {
        out.push_str("  ✅ no invalidated edge has live followers\n");
    } else {
        for p in &report.propagation {
            out.push_str(&format!(
                "  ⚠️ #{} \"{}\" → \"{}\" invalidated, yet {} valid follower(s) cite it:\n",
                p.edge_id,
                truncate60(&p.decision_text),
                truncate60(&p.outcome_text),
                p.followers.len()
            ));
            for (fid, fdec, fout) in p.followers.iter().take(5) {
                out.push_str(&format!(
                    "     #{fid} \"{}\" → \"{}\" — consider reviewing\n",
                    truncate60(fdec),
                    truncate60(fout)
                ));
            }
        }
    }

    // ④ Self-reinforcement.
    out.push_str("\n④ Self-reinforcement signals:\n");
    if report.self_reinforcement.is_empty() {
        out.push_str("  ✅ no self-reinforcement signal\n");
    } else {
        for r in &report.self_reinforcement {
            out.push_str(&format!("  ⚠️ [{}] {}\n", r.kind, r.detail));
        }
    }

    if !report.section_errors.is_empty() {
        out.push_str("\nsection errors (best-effort):\n");
        for e in &report.section_errors {
            out.push_str(&format!("  ❗ {e}\n"));
        }
    }
    out
}

/// Sections ②–④ only, for embedding into the sleep report (⑤.1 already
/// covers the bias snapshot there).
pub(crate) fn render_tail_sections(report: &DriftReport) -> String {
    let full = render_text(report);
    // Split at section ② — the header and ① belong to the standalone command.
    match full.find("\n② Trend drift") {
        Some(idx) => full[idx..].trim_start().to_string(),
        None => full,
    }
}

pub(crate) fn run_drift(args: &[String]) -> Result<()> {
    let mut db: Option<PathBuf> = None;
    let mut json = false;
    let mut opts = DriftOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--db requires a path\n{USAGE}");
                };
                db = Some(PathBuf::from(p));
            }
            "--json" => json = true,
            "--days" => {
                i += 1;
                opts.days = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--days requires an integer\n{USAGE}"))?;
            }
            "--polarity-shift" => {
                i += 1;
                opts.polarity_shift = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--polarity-shift requires a float\n{USAGE}"))?;
            }
            "--confidence-shift" => {
                i += 1;
                opts.confidence_shift =
                    args.get(i).and_then(|v| v.parse().ok()).ok_or_else(|| {
                        anyhow::anyhow!("--confidence-shift requires a float\n{USAGE}")
                    })?;
            }
            "--volume-ratio" => {
                i += 1;
                opts.volume_ratio = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--volume-ratio requires a float\n{USAGE}"))?;
            }
            "--feedback-share" => {
                i += 1;
                opts.feedback_share = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--feedback-share requires a float\n{USAGE}"))?;
            }
            other => anyhow::bail!("unknown flag: {other}\n{USAGE}"),
        }
        i += 1;
    }

    let store = CausalStore::open(db.unwrap_or_else(get_db_path))?;
    let report = drift_report(&store, &opts);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(())
}
