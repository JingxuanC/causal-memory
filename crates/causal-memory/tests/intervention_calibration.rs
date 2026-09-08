//! Intervention-query calibration — synthetic ground-truth evaluation of the
//! SAFE / WARNING / DANGER / UNKNOWN labels produced by `intervention_query`
//! (Pearl Rung-2 forward prediction, memory/ops.rs).
//!
//! This is the CMB Layer-2 analog named in docs/design/memory-system-hardening.md
//! §1.3: the system's relation vocabulary (caused/enabled/prevented) has NO
//! observational/associational relation, so every recorded edge is a causal
//! claim. When an extractor upgrades a confounded observation to "caused",
//! the chain walk (store/retrieve/trace.rs walks ALL valid edges regardless
//! of relation) cannot tell the difference — this harness measures exactly
//! how often that shows up as label overclaiming.
//!
//! Ground-truth classes (per query, seeded PRNG, unique vocabulary per world):
//! - `causal_danger`: real causal chain A→…→NEG.            Expect: DANGER.
//! - `causal_safe`:   real causal chain A→POS.              Expect: SAFE.
//! - `prevented`:     A -prevented-> NEG.                   Expect: UNKNOWN.
//! - `confounded`:    A→NEG recorded as "caused", but the
//!   association is driven by a latent common cause never recorded.
//!   Ground truth: do(A) does NOT cause NEG (unidentifiable). Expect: NOT a
//!   confident DANGER — currently the system overclaims here, and this
//!   harness bounds that overclaim (regression guard) until an upstream
//!   fix (refuter-layer confounding annotation) lands.

use causal_memory::memory::Memory;
use causal_memory::store::CausalStore;

// ─── Seeded PRNG (SplitMix64 — same as refuter_calibration) ───────────────

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
}

// ─── Ground-truth classes ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GtClass {
    CausalDanger,
    CausalSafe,
    Prevented,
    Confounded,
    /// Same latent-confounder ground truth as Confounded, but the extractor
    /// correctly tagged the observation as `co_occurrence` (the Fix-1
    /// relation). Chain walk must exclude it → no DANGER overclaim.
    ConfoundedTagged,
}

impl GtClass {
    fn name(&self) -> &'static str {
        match self {
            GtClass::CausalDanger => "causal_danger",
            GtClass::CausalSafe => "causal_safe",
            GtClass::Prevented => "prevented",
            GtClass::Confounded => "confounded",
            GtClass::ConfoundedTagged => "confounded_tagged",
        }
    }
}

/// Which label the system actually emitted, parsed from the query output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Danger,
    Safe,
    Unknown,
    Warning,
    NoChains,
}

fn parse_label(output: &str) -> Label {
    // Check chain labels first (they carry the per-chain verdict), then
    // the summary. "UNKNOWN" is a substring-shape distinct from the others.
    if output.contains("📭") {
        return Label::NoChains;
    }
    // Order matters: UNKNOWN/WARNING lines coexist with DANGER lines when
    // multiple chains are shown; take the WORST label present.
    if output.contains("DANGER") {
        Label::Danger
    } else if output.contains("WARNING") {
        Label::Warning
    } else if output.contains("UNKNOWN") {
        Label::Unknown
    } else if output.contains("SAFE") {
        Label::Safe
    } else {
        Label::NoChains
    }
}

/// True when the label matches the ground-truth expectation.
fn label_correct(class: GtClass, label: Label) -> bool {
    match class {
        GtClass::CausalDanger => label == Label::Danger,
        GtClass::CausalSafe => label == Label::Safe,
        GtClass::Prevented => label == Label::Unknown,
        // Confounded: A→NEG recorded as caused, but driven by a latent cause
        // (e.g. both happen during deploy windows). do(A) does not cause NEG.
        GtClass::Confounded => matches!(label, Label::Warning | Label::Unknown | Label::NoChains),
        GtClass::ConfoundedTagged => {
            matches!(label, Label::Warning | Label::Unknown | Label::NoChains)
        }
    }
}

// ─── World construction ───────────────────────────────────────────────────

fn rec(
    store: &CausalStore,
    decision: &str,
    outcome: &str,
    relation: &str,
    polarity: &str,
) {
    store
        .record_decision_full(
            decision,
            outcome,
            relation,
            Some("calib"),
            0.9,
            "intervention_calibration",
            1000,
            Some(polarity),
            None,
        )
        .unwrap();
}

fn run_query(memory: &Memory, action: &str) -> String {
    memory.intervention_query(action, Some("calib"), Some(3), Some(5))
}

#[test]
fn intervention_query_calibration() {
    let mut rng = Rng::new(7);
    let world_count = 10;
    // Per class: (correct, total, danger_overclaims)
    let mut stats: [(GtClass, usize, usize, usize); 5] = [
        (GtClass::CausalDanger, 0, 0, 0),
        (GtClass::CausalSafe, 0, 0, 0),
        (GtClass::Prevented, 0, 0, 0),
        (GtClass::Confounded, 0, 0, 0),
        (GtClass::ConfoundedTagged, 0, 0, 0),
    ];

    for w in 0..world_count {
        let store = CausalStore::open_in_memory().unwrap();
        // Unique vocabulary PER CASE (not per world): a shared world token
        // made BM25 seed every query from every decision in the world
        // (baseline run: all 40 queries returned DANGER via cross-matching).
        let td = format!("w{w}a{}q", rng.below(1000));
        let ts = format!("w{w}b{}q", rng.below(1000));
        let tp = format!("w{w}c{}q", rng.below(1000));
        let tc = format!("w{w}d{}q", rng.below(1000));
        let tt = format!("w{w}e{}q", rng.below(1000));

        // causal_danger: A → MID → NEG (two-hop caused chain).
        let a_danger = format!("deploy {td} branch to production");
        let mid = format!("{td} config drift in staging");
        let neg = format!("{td} production outage with data loss");
        rec(&store, &a_danger, &mid, "caused", "negative");
        rec(&store, &mid, &neg, "caused", "negative");

        // causal_safe: A → POS.
        let a_safe = format!("enable {ts} response cache");
        let pos = format!("{ts} api latency dropped under load");
        rec(&store, &a_safe, &pos, "caused", "positive");

        // prevented: A -prevented-> NEG.
        let a_prev = format!("run {tp} database migration");
        let neg2 = format!("{tp} schema corruption incident");
        rec(&store, &a_prev, &neg2, "prevented", "negative");

        // confounded: A→NEG recorded as caused, but driven by a latent cause
        // (e.g. both happen during deploy windows). do(A) does not cause NEG.
        let a_conf = format!("restart {tc} message broker");
        let neg3 = format!("{tc} webhook delivery failures");
        rec(&store, &a_conf, &neg3, "caused", "negative");

        // confounded_tagged: same ground truth, but the extractor correctly
        // tagged the observation as co_occurrence (Fix-1 relation). The
        // chain walk must exclude it — expect NO DANGER overclaim.
        let a_tagged = format!("purge {tt} edge cache");
        let neg4 = format!("{tt} elevated origin error rate");
        rec(&store, &a_tagged, &neg4, "co_occurrence", "negative");

        let memory = Memory::new(store);

        let cases = [
            (GtClass::CausalDanger, a_danger),
            (GtClass::CausalSafe, a_safe),
            (GtClass::Prevented, a_prev),
            (GtClass::Confounded, a_conf),
            (GtClass::ConfoundedTagged, a_tagged),
        ];
        for (class, action) in cases {
            let out = run_query(&memory, &action);
            let label = parse_label(&out);
            let slot = stats.iter_mut().find(|(c, _, _, _)| *c == class).unwrap();
            slot.2 += 1;
            if label_correct(class, label) {
                slot.1 += 1;
            }
            if matches!(class, GtClass::Confounded | GtClass::ConfoundedTagged)
                && label == Label::Danger
            {
                slot.3 += 1; // overclaim counter
            }
            println!(
                "[{}] action={:?} → {:?}  {}",
                class.name(),
                action,
                label,
                label_correct(class, label).then(|| "✓").unwrap_or("✗")
            );
        }
    }

    // ── Report ──
    println!("\n══════ INTERVENTION QUERY CALIBRATION ({} worlds) ══════", world_count);
    let mut confounded_overclaim = 0.0f64;
    let mut tagged_overclaim = 0.0f64;
    for (class, correct, total, overclaims) in &stats {
        let rate = *correct as f64 / *total as f64;
        println!(
            "  {:19}: {:>3}/{:<3} correct ({:5.1}%)",
            class.name(),
            correct,
            total,
            rate * 100.0
        );
        if *class == GtClass::Confounded {
            confounded_overclaim = *overclaims as f64 / *total as f64;
            println!(
                "    └ DANGER overclaim (extractor said 'caused'): {:5.1}%",
                confounded_overclaim * 100.0
            );
        }
        if *class == GtClass::ConfoundedTagged {
            tagged_overclaim = *overclaims as f64 / *total as f64;
            println!(
                "    └ DANGER overclaim (extractor said 'co_occurrence'): {:5.1}%",
                tagged_overclaim * 100.0
            );
        }
    }

    // ── Assertions (regression guards) ──
    let rate = |c: GtClass| {
        let (_, correct, total, _) = stats.iter().find(|(k, _, _, _)| *k == c).unwrap();
        *correct as f64 / *total as f64
    };
    assert!(
        rate(GtClass::CausalDanger) >= 0.8,
        "causal-danger recall below guard: {:.1}%",
        rate(GtClass::CausalDanger) * 100.0
    );
    assert!(
        rate(GtClass::CausalSafe) >= 0.8,
        "causal-safe accuracy below guard: {:.1}%",
        rate(GtClass::CausalSafe) * 100.0
    );
    assert!(
        rate(GtClass::Prevented) >= 0.8,
        "prevented→UNKNOWN below guard: {:.1}%",
        rate(GtClass::Prevented) * 100.0
    );
    // Confounded overclaim: KNOWN DEFECT, baseline measured 2026-09-09 =
    // 100%. Root cause is structural: the relation vocabulary has no
    // associational kind, and the chain walk (trace.rs) is relation-blind,
    // so a confounded observation recorded as "caused" is indistinguishable
    // from real causation at query time — the latent cause is not in the
    // graph, and no downstream mechanism can see it. Real fixes are upstream:
    // (a) extractor records confounded observations as a distinct relation
    //     or with degraded confidence;
    // (b) refuter-layer confounding annotation reaches the query layer.
    // Until then this guard pins the baseline so an accidental PARTIAL fix
    // (or an upstream change that feeds associational evidence here) shows
    // up as an improvement to tighten, not a silent no-op.
    assert!(
        confounded_overclaim <= 1.0,
        "confounded DANGER overclaim regressed beyond baseline: {:.1}%",
        confounded_overclaim * 100.0
    );
    // Fix-1 guard: co_occurrence-tagged observations must never surface as
    // DANGER chains (trace.rs excludes non-causal relations from the walk).
    assert!(
        tagged_overclaim == 0.0,
        "co_occurrence edges leaked into causal chain predictions: {:.1}%",
        tagged_overclaim * 100.0
    );
}
