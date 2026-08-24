//! Formal ablation harness (roadmap: "ablate SWR / spreading / prevented
//! once each and quantify contributions"). Three arms over the REAL
//! LongMemEval distill store — no synthetic graphs, no self-judged eval:
//! the metric is RETRIEVAL-level (evidence hit rate + rank), so it needs
//! no LLM and no judge, immune to the self-evaluation caveat.
//!
//! Arms:
//! - baseline: the production pipeline as-is
//! - no-spread: `Memory::disable_spread()` — the seeding layer (BM25/
//!   semantic direct hits, Q-weighted) still runs, but zero spread hops,
//!   so the graph's associative reach is removed (engine-level switch,
//!   in-memory; the store is untouched)
//! - no-inhibition: prevented edges flipped to caused (the inhibitory
//!   side removed; negative spread gone)
//! - no-swr: consolidation-processed fields neutralized (q_value seeding
//!   flattened to 0.5, decayed confidence restored to pre-decay) —
//!   approximates a never-consolidated store without re-running history
//!
//! Metric per arm: evidence_hit_rate over N questions + mean evidence
//! rank in the hit set + token mass of the returned pool. Judge-free:
//! the "answer correctness" layer is deliberately out of scope (that is
//! what CausalEval measures); this isolates the RETRIEVAL contribution
//! of each mechanism.
//!
//! Usage: cargo run --release --bin causal-memory-ablation -- \
//!   [--db benches/longmemeval/db/longmemeval_distill.db] \
//!   [--data benches/longmemeval/data/longmemeval_s_cleaned.json] [-n 100] \
//!   [--self-queries] [--note "free-text added to summary notes"]
//!
//! --self-queries: derive query/gold pairs from the DB itself instead of
//! the LongMemEval dataset — one pair per valid causal edge (query =
//! outcome text, gold = the edge's `causal:{edge_id}` hit key, i.e. the
//! decision chunk surfaced inside that edge's hit). Edges with either
//! endpoint text < 20 chars are skipped. Default n = all derived pairs.
//! This mode measures causal-lesson retrieval against a REAL store where
//! the LongMemEval evidence-turn labels are meaningless.

use std::collections::HashSet;

use causal_memory::memory::Memory;
use causal_memory::store::CausalStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut db = "benches/longmemeval/db/longmemeval_distill.db".to_string();
    let mut data = "benches/longmemeval/data/longmemeval_s_cleaned.json".to_string();
    let mut n: Option<usize> = None;
    let mut self_queries = false;
    let mut note: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db = args.next().expect("--db needs a value"),
            "--data" => data = args.next().expect("--data needs a value"),
            "-n" | "--n" => n = Some(args.next().expect("-n needs a value").parse()?),
            "--self-queries" => self_queries = true,
            "--note" => note = Some(args.next().expect("--note needs a value")),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }

    let (queries, data_label, n) = if self_queries {
        let qs = load_self_queries(&db)?;
        eprintln!("self-queries: derived {} query pairs from causal_edges", qs.len());
        (
            qs,
            "self-derived from causal_edges (query=outcome text, gold=causal edge hit key)"
                .to_string(),
            n.unwrap_or(usize::MAX),
        )
    } else {
        let raw = std::fs::read(&data)?;
        let qs = load_questions(&raw)?
            .into_iter()
            .map(|q| AblationQuery {
                id: q.question_id,
                text: q.question,
                gold: Gold::Evidence(q.evidence),
            })
            .collect();
        (qs, data, n.unwrap_or(100))
    };

    let mut arms = Vec::new();

    // Arm 1: baseline (production pipeline, read-only).
    let base = run_arm(&db, &queries, n, Arm::Baseline).await?;
    println!("{base}");
    arms.push(base);

    // Arm 2: no-spread — engine-level switch on the SAME store (read-only;
    // the switch lives on the in-memory graph instance). Seeding intact,
    // zero spread hops: seed-hits-only retrieval.
    let nospread = run_arm(&db, &queries, n, Arm::NoSpread).await?;
    println!("{nospread}");
    arms.push(nospread);

    // Arm 3: no-inhibition — copy DB, flip prevented → caused.
    let noinhib_db = copy_db(&db, "ablation_noinhib")?;
    let flipped = CausalStore::open(&noinhib_db)?.with_conn(|c| {
        Ok(c.execute(
            "UPDATE causal_edges SET relation='caused' WHERE relation='prevented'",
            [],
        )?)
    })?;
    eprintln!("no-inhibition: flipped {flipped} prevented edges");
    let noinhib = run_arm(&noinhib_db, &queries, n, Arm::NoInhibition).await?;
    println!("{noinhib}");
    arms.push(noinhib);

    // Arm 4: no-swr — copy DB, flatten q_value to 0.5 and restore decayed
    // confidences is not reconstructable (decay is lossy); approximate by
    // flattening q_value only (the seeding contribution).
    let noswr_db = copy_db(&db, "ablation_noswr")?;
    let flattened = CausalStore::open(&noswr_db)?
        .with_conn(|c| Ok(c.execute("UPDATE chunks SET q_value = 0.5", [])?))?;
    eprintln!("no-swr: flattened {flattened} chunk q-values");
    let noswr = run_arm(&noswr_db, &queries, n, Arm::NoSwr).await?;
    println!("{noswr}");
    arms.push(noswr);

    let path = write_results(&db, &data_label, note.as_deref(), &arms)?;
    println!("results written to {path}");

    Ok(())
}

#[derive(Clone, Copy)]
enum Arm {
    Baseline,
    NoSpread,
    NoInhibition,
    NoSwr,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Baseline => "baseline",
            Arm::NoSpread => "no-spread",
            Arm::NoInhibition => "no-inhibition",
            Arm::NoSwr => "no-swr",
        }
    }
}

struct ArmMetrics {
    arm: &'static str,
    hits: usize,
    total: usize,
    hit_rate: f64,
    mean_rank: f64,
    avg_tok: usize,
}

impl std::fmt::Display for ArmMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<14} evidence_hit {}/{} ({:.1}%)  mean_rank {:.1}  avg_pool_tokens {}",
            self.arm, self.hits, self.total, self.hit_rate, self.mean_rank, self.avg_tok
        )
    }
}

/// Persist per-arm metrics + deltas vs baseline, same summary-json style
/// as benches/longmemeval/results/run_*_summary.json.
fn write_results(
    db: &str,
    data: &str,
    note: Option<&str>,
    arms: &[ArmMetrics],
) -> anyhow::Result<String> {
    let dir = std::path::Path::new("benches/ablation/results");
    std::fs::create_dir_all(dir)?;
    let run_id = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let git_commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let base = &arms[0];
    let arm_json: Vec<serde_json::Value> = arms
        .iter()
        .map(|a| {
            serde_json::json!({
                "arm": a.arm,
                "evidence_hits": a.hits,
                "total_questions": a.total,
                "evidence_hit_rate": a.hit_rate / 100.0,
                "mean_rank": a.mean_rank,
                "avg_pool_tokens": a.avg_tok,
                "delta_hit_rate_vs_baseline": (a.hit_rate - base.hit_rate) / 100.0,
                "delta_mean_rank_vs_baseline": a.mean_rank - base.mean_rank,
                "delta_avg_pool_tokens_vs_baseline": a.avg_tok as i64 - base.avg_tok as i64,
            })
        })
        .collect();
    let mut notes = serde_json::json!({
        "no-spread": "engine-level switch (Memory::disable_spread): seeding layer intact (BM25/semantic direct hits, Q-weighted), zero spreading-activation hops — seed-hits-only retrieval over the same store",
        "no-inhibition": "DB copy with prevented edges flipped to caused (inhibitory side removed, negative spread gone)",
        "no-swr": "DB copy with chunk q_value flattened to 0.5 (approximates a never-consolidated store; decayed confidence is lossy and not restored)"
    });
    if let Some(note) = note {
        notes["run"] = serde_json::Value::String(note.to_string());
    }
    let doc = serde_json::json!({
        "run_id": run_id,
        "date": chrono::Local::now().to_rfc3339(),
        "git_commit": git_commit,
        "db": db,
        "data": data,
        "n": base.total,
        "metric": "retrieval-level: evidence hit rate + mean evidence rank + pool token mass (judge-free)",
        "arms": arm_json,
        "notes": notes,
    });
    let path = dir.join(format!("ablation_{run_id}_summary.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path.display().to_string())
}

#[derive(serde::Deserialize)]
struct LmeQ {
    question_id: String,
    question: String,
    /// Official evidence turn texts (turns flagged has_answer in the
    /// haystack) — rebuilt from the dataset at load time below.
    #[serde(default)]
    evidence: Vec<String>,
}

/// Load questions and derive evidence texts (has_answer turns) from the
/// haystack sessions, keyed by (session_id, turn_idx).
fn load_questions(data: &[u8]) -> anyhow::Result<Vec<LmeQ>> {
    #[derive(serde::Deserialize)]
    struct Raw {
        question_id: String,
        question: String,
        haystack_sessions: Vec<Vec<Turn>>,
    }
    #[derive(serde::Deserialize)]
    struct Turn {
        #[serde(rename = "type")]
        _t: Option<String>,
        has_answer: Option<bool>,
        #[serde(default)]
        message: String,
        #[serde(default)]
        content: String,
    }
    let raws: Vec<Raw> = serde_json::from_slice(data)?;
    Ok(raws
        .into_iter()
        .map(|r| {
            let evidence = r
                .haystack_sessions
                .iter()
                .flat_map(|s| s.iter())
                .filter(|t| t.has_answer == Some(true))
                .map(|t| {
                    if t.content.is_empty() {
                        t.message.clone()
                    } else {
                        t.content.clone()
                    }
                })
                .collect();
            LmeQ {
                question_id: r.question_id,
                question: r.question,
                evidence,
            }
        })
        .collect())
}

/// One ablation query: text to search plus the gold standard a hit is
/// judged against (mode-dependent).
struct AblationQuery {
    id: String,
    text: String,
    gold: Gold,
}

enum Gold {
    /// LME mode: distinctive-token overlap against evidence turn texts.
    Evidence(Vec<String>),
    /// Self-derived mode: exact `MemoryHit.key` match (`causal:{edge_id}` —
    /// the edge whose decision chunk is the gold evidence).
    HitKey(String),
}

/// Derive query/gold pairs from the store's own causal edges (self-queries
/// mode): one pair per VALID edge — query = outcome text, gold = the
/// edge's hit key (its decision text is surfaced verbatim inside a causal
/// hit, so an edge hit == the decision chunk retrieved). Edges with either
/// endpoint text shorter than 20 chars are skipped (too short to seed or
/// judge meaningfully).
fn load_self_queries(db: &str) -> anyhow::Result<Vec<AblationQuery>> {
    const MIN_TEXT_LEN: usize = 20;
    let store = CausalStore::open(db)?;
    store.with_conn(|c| {
        let mut stmt = c.prepare(
            "SELECT ce.id, cf.text, ct.text
             FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             JOIN chunks ct ON ct.id = ce.to_id
             WHERE ce.valid_to IS NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (edge_id, decision_text, outcome_text) = row?;
            if decision_text.len() < MIN_TEXT_LEN || outcome_text.len() < MIN_TEXT_LEN {
                continue;
            }
            out.push(AblationQuery {
                id: format!("edge{edge_id}"),
                text: outcome_text,
                gold: Gold::HitKey(format!("causal:{edge_id}")),
            });
        }
        Ok(out)
    })
}

async fn run_arm(
    db: &str,
    questions: &[AblationQuery],
    n: usize,
    arm: Arm,
) -> anyhow::Result<ArmMetrics> {
    let mem = Memory::open(db)?;
    if let Arm::NoSpread = arm {
        mem.disable_spread();
        eprintln!("no-spread: spreading activation disabled (seed-hits-only)");
    }
    let mut hits = 0usize;
    let mut rank_sum = 0usize;
    let mut rank_n = 0usize;
    let mut tok_mass = 0usize;
    let qs: Vec<&AblationQuery> = questions.iter().take(n).collect();
    let total = qs.len();
    for q in &qs {
        let t0 = std::time::Instant::now();
        let (hit_rows, _mode) = mem.search_memory_entries(&q.text, None, None, 10);
        eprintln!("  q {} {:?} mode={}", q.id, t0.elapsed(), _mode);
        tok_mass += hit_rows.iter().map(|h| h.content.len() / 4).sum::<usize>();
        let best_rank: Option<usize> = match &q.gold {
            // Evidence hit: any returned content shares a distinctive token
            // with any evidence string (retrieval-level proxy, judge-free).
            Gold::Evidence(evidence) => {
                let ev: HashSet<&str> = evidence.iter().map(|s| s.as_str()).collect();
                hit_rows
                    .iter()
                    .position(|h| ev.iter().any(|e| overlap(&h.content, e)))
                    .map(|p| p + 1)
            }
            // Edge hit: the gold causal edge itself is in the returned pool
            // (exact id match via MemoryHit.key).
            Gold::HitKey(key) => hit_rows.iter().position(|h| &h.key == key).map(|p| p + 1),
        };
        if let Some(r) = best_rank {
            hits += 1;
            rank_sum += r;
            rank_n += 1;
        }
    }
    let hit_rate = hits as f64 / total as f64 * 100.0;
    let mean_rank = if rank_n > 0 {
        rank_sum as f64 / rank_n as f64
    } else {
        0.0
    };
    let avg_tok = tok_mass / total.max(1);
    Ok(ArmMetrics {
        arm: arm.name(),
        hits,
        total,
        hit_rate,
        mean_rank,
        avg_tok,
    })
}

/// Distinctive-token overlap: ≥2 shared tokens of ≥4 chars.
fn overlap(a: &str, b: &str) -> bool {
    let ta: HashSet<String> = causal_memory::patterns::tokenize(a)
        .into_iter()
        .filter(|t| t.len() >= 4)
        .collect();
    let tb: HashSet<String> = causal_memory::patterns::tokenize(b)
        .into_iter()
        .filter(|t| t.len() >= 4)
        .collect();
    ta.intersection(&tb).count() >= 2
}

fn copy_db(src: &str, tag: &str) -> anyhow::Result<String> {
    let dst = format!("/tmp/{tag}.db");
    let _ = std::fs::remove_file(&dst);
    let src_conn = rusqlite::Connection::open(src)?;
    src_conn.execute("VACUUM INTO ?1", rusqlite::params![&dst])?;
    Ok(dst)
}
