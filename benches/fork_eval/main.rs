//! ForkEval — same-context counterfactual verdict accuracy, fork evidence
//! ON vs OFF (docs/design/counterfactual-rung3.md §8).
//!
//! Protocol: N deterministic scenarios. Each has a shared context, a bad
//! option and a good option (some pairs share a content token — e.g.
//! "join", "index" — deliberately, since competitive separation plus rank
//! ordering keep the pools clean; that interaction is part of what this
//! harness exercises), with outcome texts carrying explicit polarity
//! signal words. The same episodes are recorded twice:
//!
//!   fork-on  DB — every record carries the shared context ⇒ write-time
//!                 fork detection links the branches as natural experiments
//!   fork-off DB — identical records, no context ⇒ distribution-only
//!                 comparison (the pre-v14 verdict path)
//!
//! Verdict correctness = counterfactual_query(bad, good) concludes
//! "favors B" (the good side). Ambiguous/insufficient verdicts count as
//! incorrect (honest denominator). Expected: both modes can be right when
//! pools are clean, but the paired verdict is what makes same-context
//! evidence explicit; this harness pins the mechanism and guards
//! regressions in both paths.
//!
//! Run: causal-memory-fork-eval

use causal_memory::memory::Memory;

/// One scenario: (bad option, bad outcome), (good option, good outcome),
/// shared context.
type Scenario = (
    (&'static str, &'static str),
    (&'static str, &'static str),
    &'static str,
);

/// Deterministic scenario vocabulary: bad/good option pairs and their
/// outcomes, token-disjoint across sides (BM25 matches decision AND
/// outcome text — shared tokens would pool both sides).
const SCENARIOS: &[Scenario] = &[
    (
        ("picked mysql", "migration deadlock"),
        ("adopted postgres", "cutover passed"),
        "rust agent, sqlite, single node",
    ),
    (
        ("enabled lto", "build timeout"),
        ("kept defaults", "build passed in seconds"),
        "release profile tuning",
    ),
    (
        ("skipped tests", "production outage"),
        ("ran the suite", "regression caught, release passed"),
        "friday deploy",
    ),
    (
        ("cached everything", "memory exhausted"),
        ("cached lookups only", "footprint ok, load passed"),
        "api gateway, go",
    ),
    (
        ("used regex html parsing", "parser panic"),
        (
            "switched to scraper crate",
            "extraction fixed, all pages parsed ok",
        ),
        "crawler, rust",
    ),
    (
        ("one big transaction", "lock contention"),
        ("batched commits", "throughput resolved, targets met"),
        "ingest pipeline, sqlite",
    ),
    (
        ("premature optimization", "complexity explosion"),
        ("measured first", "hotspot found and fixed"),
        "profilo benchmark",
    ),
    (
        ("manual db migration", "schema drift"),
        (
            "versioned migrations",
            "migrations passed on every environment",
        ),
        "deploy tooling",
    ),
    (
        ("hardcoded secrets", "credential leak"),
        ("env config", "rotation succeeded, leak fixed"),
        "ci pipeline",
    ),
    (
        ("sync io in handler", "request latency spike"),
        ("async runtime", "latency ok under full load"),
        "http service, tokio",
    ),
    (
        ("mutable global state", "race detected"),
        ("ownership transfer", "race fixed, tests passed"),
        "worker pool",
    ),
    (
        ("naive o(n2) join", "query timeout"),
        ("hash join", "queries pass the budget check"),
        "reporting db",
    ),
    (
        ("eager loading", "cold boot crawl"),
        ("lazy init", "startup passed the budget"),
        "cli tool",
    ),
    (
        ("string ids everywhere", "type confusion bug"),
        ("typed ids", "mismatch caught — bug fixed before merge"),
        "domain model",
    ),
    (
        ("retry storm", "cascade failure"),
        ("backoff with jitter", "load resolved, no retries storm"),
        "payment gateway",
    ),
    (
        ("single region", "latency for far users"),
        ("edge replication", "p95 passed the sla"),
        "global saas",
    ),
    (
        ("monolithic release", "blast radius huge"),
        ("feature flags", "rollback succeeded in minutes"),
        "product launch",
    ),
    (
        ("logs without structure", "debugging blind"),
        ("structured logs", "root cause found, incident resolved"),
        "observability",
    ),
    (
        ("manual dependency bumps", "silent breakage"),
        ("lockfile + ci check", "upgrade passed ci, nothing broke"),
        "supply chain",
    ),
    (
        ("index everything", "write amplification"),
        ("index the hot path", "writes ok, reads pass sla"),
        "oltp store",
    ),
];

#[derive(Default)]
struct Score {
    correct: usize,
    ambiguous: usize,
    fork_sections: usize,
    total: usize,
}

fn run_mode(with_context: bool) -> Score {
    let mut score = Score::default();
    for ((bad, bad_out), (good, good_out), ctx) in SCENARIOS.iter() {
        // In-memory construction cannot fail on a fresh DB; a failure here
        // IS a harness bug worth panicking on (the cli crate lints agree).
        #[allow(clippy::expect_used)]
        let mem = Memory::open_in_memory().expect("memory");
        let task = "fork-eval";
        mem.record_decision(
            bad,
            bad_out,
            "caused",
            task,
            None,
            if with_context { Some(ctx) } else { None },
        );
        mem.record_decision(
            good,
            good_out,
            "caused",
            task,
            None,
            if with_context { Some(ctx) } else { None },
        );
        let out = mem.counterfactual_query(bad, good, None, Some(5));
        score.total += 1;
        // Pin the mechanism, not just the outcome: fork-on must actually
        // render the same-context section, fork-off must not — otherwise a
        // silently dead fork path would still score 100% via clean pools.
        if out.contains("🔀 Same-context branches") {
            score.fork_sections += 1;
        }
        if out.contains("favors B") {
            score.correct += 1;
        } else {
            score.ambiguous += 1;
        }
    }
    score
}

fn main() {
    let n = SCENARIOS.len();
    println!("ForkEval — same-context counterfactual verdict accuracy ({n} scenarios)");
    println!();

    let on = run_mode(true);
    let off = run_mode(false);

    let pct = |s: &Score| {
        format!(
            "{:.0}% ({}/{})",
            (s.correct as f64 / s.total as f64) * 100.0,
            s.correct,
            s.total
        )
    };
    println!("  fork OFF (distribution-only):   {}", pct(&off));
    println!(
        "    ambiguous/insufficient:      {}/{}",
        off.ambiguous, off.total
    );
    println!(
        "    fork sections rendered:      {}/{}",
        off.fork_sections, off.total
    );
    println!("  fork ON  (natural experiments): {}", pct(&on));
    println!(
        "    ambiguous/insufficient:      {}/{}",
        on.ambiguous, on.total
    );
    println!(
        "    fork sections rendered:      {}/{}",
        on.fork_sections, on.total
    );
    println!();
    println!("  fork evidence must never score below fork-off (paired verdict");
    println!("  outranks but falls back to the distribution when pairs don't contrast).");
    if on.correct < off.correct {
        eprintln!("REGRESSION: fork-on scored below fork-off");
        std::process::exit(1);
    }
    // Absolute accuracy floor (review finding on PR #19): the relative gate
    // above passes vacuously when BOTH modes collapse to 0/20 — exactly the
    // hand-copied-contract drift ("favors B" literal vs the verdict phrase)
    // this repo has already suffered once. A curated table with explicit
    // polarity signals must never score zero in either mode.
    if on.correct == 0 || off.correct == 0 {
        eprintln!(
            "REGRESSION: verdict accuracy floor breached (fork-on {}/{}, fork-off {}/{}) — the 'favors B' literal may have drifted from the verdict phrase",
            on.correct, on.total, off.correct, off.total
        );
        std::process::exit(1);
    }
    if on.fork_sections != on.total {
        eprintln!(
            "REGRESSION: fork-on rendered {}/{} fork sections (expected {})",
            on.fork_sections, on.total, on.total
        );
        std::process::exit(1);
    }
    if off.fork_sections != 0 {
        eprintln!(
            "REGRESSION: fork-off rendered {} fork sections (expected 0)",
            off.fork_sections
        );
        std::process::exit(1);
    }
    println!("ok");
}
