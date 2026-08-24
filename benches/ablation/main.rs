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
//!
//! --self-queries-paraphrase: same derivation, but the outcome text is
//! mechanically paraphrased first (see paraphrase_drop_anchors) — the
//! highest-IDF tokens (the literal-match anchors) are dropped, simulating
//! a user describing the problem in their own words. This is the FAIR
//! query form for measuring spreading activation's associative value:
//! verbatim queries are trivially served by the seeding layer alone.

use std::collections::{HashMap, HashSet};

use causal_memory::memory::Memory;
use causal_memory::store::CausalStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut db = "benches/longmemeval/db/longmemeval_distill.db".to_string();
    let mut data = "benches/longmemeval/data/longmemeval_s_cleaned.json".to_string();
    let mut n: Option<usize> = None;
    let mut self_queries = false;
    let mut paraphrase = false;
    let mut note: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db = args.next().expect("--db needs a value"),
            "--data" => data = args.next().expect("--data needs a value"),
            "-n" | "--n" => n = Some(args.next().expect("-n needs a value").parse()?),
            "--self-queries" => self_queries = true,
            "--self-queries-paraphrase" => {
                self_queries = true;
                paraphrase = true;
            }
            "--note" => note = Some(args.next().expect("--note needs a value")),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }

    let (queries, data_label, skipped, n) = if self_queries {
        let (qs, skipped) = load_self_queries(&db, paraphrase)?;
        eprintln!(
            "self-queries{}: derived {} query pairs from causal_edges ({} skipped)",
            if paraphrase { "/paraphrase" } else { "" },
            qs.len(),
            skipped
        );
        let label = if paraphrase {
            "self-derived from causal_edges, PARAPHRASED queries (top-IDF anchor tokens dropped), gold=causal edge hit key"
        } else {
            "self-derived from causal_edges (query=outcome text, gold=causal edge hit key)"
        };
        (qs, label.to_string(), skipped, n.unwrap_or(usize::MAX))
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
        (qs, data, 0, n.unwrap_or(100))
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

    let path = write_results(&db, &data_label, skipped, note.as_deref(), &arms)?;
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
    skipped: usize,
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
        "skipped_queries": skipped,
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
/// judge meaningfully). With `paraphrase`, the outcome text is first
/// mechanically paraphrased (top-IDF anchor tokens dropped); pairs whose
/// paraphrased query keeps < 4 tokens are skipped. Returns (pairs,
/// skipped_count) — skipped counts only the paraphrase short-query drops.
fn load_self_queries(db: &str, paraphrase: bool) -> anyhow::Result<(Vec<AblationQuery>, usize)> {
    const MIN_TEXT_LEN: usize = 20;
    const MIN_QUERY_TOKENS: usize = 4;
    let store = CausalStore::open(db)?;
    let idf = if paraphrase {
        Some(chunk_idf(&store)?)
    } else {
        None
    };
    let mut skipped = 0usize;
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
            let query_text = match &idf {
                Some(idf) => {
                    let p = paraphrase_drop_anchors(&outcome_text, idf);
                    if p.split_whitespace().count() < MIN_QUERY_TOKENS {
                        skipped += 1;
                        continue;
                    }
                    p
                }
                None => outcome_text,
            };
            out.push(AblationQuery {
                id: format!("edge{edge_id}"),
                text: query_text,
                gold: Gold::HitKey(format!("causal:{edge_id}")),
            });
        }
        Ok((out, skipped))
    })
}

/// IDF over the store's chunk corpus (the query-side document space):
/// idf(t) = ln((N+1)/(df+1)) + 1, df counting chunks containing t.
fn chunk_idf(store: &CausalStore) -> anyhow::Result<HashMap<String, f64>> {
    store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT text FROM chunks")?;
        let texts = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut n_docs = 0usize;
        for t in texts {
            n_docs += 1;
            let distinct: HashSet<String> = causal_memory::patterns::tokenize(&t?).into_iter().collect();
            for tok in distinct {
                *df.entry(tok).or_insert(0) += 1;
            }
        }
        Ok(df
            .into_iter()
            .map(|(t, d)| {
                let idf = ((n_docs as f64 + 1.0) / (d as f64 + 1.0)).ln() + 1.0;
                (t, idf)
            })
            .collect())
    })
}

/// Mechanical, judge-free paraphrase (deterministic — no RNG, no LLM):
/// drop the k highest-IDF tokens of the text, k = ceil(30% of the
/// distinct-token count) clamped to [1, 5]. The dropped tokens are the
/// literal-match anchors a BM25/semantic seed would need; what remains
/// simulates a user describing the problem in their own (generic) words.
/// Tokens only ever get REMOVED, so no decision-side information can leak
/// into the query. Ties broken by token text for reproducibility.
fn paraphrase_drop_anchors(text: &str, idf: &HashMap<String, f64>) -> String {
    let mut seen = HashSet::new();
    let tokens: Vec<String> = causal_memory::patterns::tokenize(text)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect();
    let k = ((tokens.len() as f64 * 0.3).ceil() as usize).clamp(1, 5);
    let mut by_idf: Vec<&String> = tokens.iter().collect();
    by_idf.sort_by(|a, b| {
        let ia = idf.get(*a).copied().unwrap_or(f64::MAX);
        let ib = idf.get(*b).copied().unwrap_or(f64::MAX);
        ib.partial_cmp(&ia).unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    let drop: HashSet<&String> = by_idf.into_iter().take(k).collect();
    tokens
        .iter()
        .filter(|t| !drop.contains(t))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn idf_of(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn paraphrase_drops_high_idf_anchors() {
        // Anchors (rare in the corpus, high IDF) vs generic context.
        let idf = idf_of(&[
            ("dockerfile", 5.0),
            ("alpine", 5.0),
            ("syncuser", 5.0),
            ("root", 5.0),
            ("build", 1.2),
            ("fails", 1.3),
            ("when", 1.0),
            ("using", 1.1),
            ("cache", 1.4),
            ("network", 1.2),
            ("timeout", 1.3),
            ("error", 1.1),
            ("check", 1.2),
            ("config", 1.3),
        ]);
        let text = "dockerfile alpine syncuser root build fails when using cache network timeout error check config";
        let p = paraphrase_drop_anchors(text, &idf);
        // k = ceil(14 × 0.3) = 5 → the four 5.0 anchors + the next-highest.
        for anchor in ["dockerfile", "alpine", "syncuser", "root"] {
            assert!(!p.split_whitespace().any(|t| t == anchor), "{anchor} leaked");
        }
        // Generic tokens survive.
        assert!(p.split_whitespace().any(|t| t == "build"));
        assert!(p.split_whitespace().any(|t| t == "fails"));

        // Overlap with the original text drops significantly (Jaccard of
        // distinct tokens well below the verbatim 1.0).
        let orig: HashSet<String> = causal_memory::patterns::tokenize(text).into_iter().collect();
        let para: HashSet<String> = p.split_whitespace().map(|s| s.to_string()).collect();
        let inter = orig.intersection(&para).count();
        let union = orig.union(&para).count();
        let jaccard = inter as f64 / union as f64;
        assert!(
            jaccard <= 0.7,
            "token overlap must drop materially, jaccard={jaccard:.2}"
        );

        // No-information-leak invariant: every query token comes from the
        // original outcome text (transform is removal-only, so nothing
        // from the decision side can enter).
        assert!(
            para.iter().all(|t| orig.contains(t)),
            "paraphrase introduced foreign tokens: {para:?}"
        );
    }

    #[test]
    fn paraphrase_never_contains_decision_side_tokens() {
        // The transform's only input is the outcome text — assert on a
        // realistic pair that decision-specific tokens cannot appear.
        let idf = idf_of(&[("mysqldump", 5.0), ("导入", 5.0), ("备份", 4.0)]);
        let outcome = "mysqldump 文件不含 CREATE DATABASE 语句导致 导入 失败";
        let p = paraphrase_drop_anchors(outcome, &idf);
        for decision_token in ["手动", "创建", "目标", "数据库"] {
            assert!(
                !p.split_whitespace().any(|t| t == decision_token),
                "decision-side token leaked: {decision_token}"
            );
        }
    }
}
