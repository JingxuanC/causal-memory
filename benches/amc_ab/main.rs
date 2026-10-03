//! `bench-amc-ab`: retrieval-path A/B for the AMC text track.
//!
//! The AMC server can serve `/search` through three paths (see
//! `causal-memory-amc`'s `AMC_RETRIEVAL`):
//! - `spread` — the unified spreading-activation engine (`search_memory_entries`),
//! - `fused`  — chunk-level BM25 ⊕ vector RRF (`search_chunks_fused`),
//! - `merge`  — both, fused again by RRF.
//!
//! This harness decides between them with data, on the metric the platform
//! actually grades: **does the returned text contain the evidence?**
//!
//! Protocol:
//! 1. **Deterministic synthetic corpus** (SplitMix64-seeded, same generator
//!    pattern as the other benches): `--users` stores, each with `--sessions`
//!    sessions of `--turns` dialogue turns, ingested through the REAL raw
//!    write path (`remember_raw_turns_with_request_id`) with increasing
//!    timestamps. No LLM, no network beyond the configured embedder.
//! 2. **Planted gold facts**, each with a differently-worded query. Half are
//!    the *keyword* slice (query and evidence share content tokens — a BM25
//!    leg can find them), half are the *semantic-only* slice (zero token
//!    overlap — only a vector leg can). The split is verified at startup.
//! 3. **Marker scoring**: a hit counts when its `content` contains the gold's
//!    distinctive marker string. Arm-agnostic on purpose — for the same raw
//!    turns the spread engine returns `"{decision}" →(no_effect)→ "{outcome}"`
//!    lessons, so key- or shape-based matching would score it zero for a
//!    formatting difference the platform's answer model never sees.
//! 4. Three arms, one measured pass each: recall@5, recall@10, MRR,
//!    p50/p95 query latency, plus recall@10 split by slice.
//!
//! Usage:
//!   cargo run --release --bin causal-memory-amc-ab [--seed 42] [--users 3]
//!       [--sessions 6] [--turns 12] [--out DIR] [--db-dir DIR]
//!
//! Env: needs a LIVE embedder — the semantic leg is half of what is being
//! measured, so the harness refuses to run without one. Build with
//! `--features local-embed` (plus a populated FASTEMBED_CACHE_DIR), or set
//! CAUSAL_MEMORY_EMBED_API + CAUSAL_MEMORY_EMBED_KEY.
//! `CAUSAL_MEMORY_EMBED_WRITE` is forced on: chunk vectors are the point.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, Result};
use causal_memory::memory::ops::MemoryHit;
use causal_memory::memory::Memory;

// ─── Deterministic RNG (SplitMix64, same pattern as the other benches) ─────

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ─── Ground truth ─────────────────────────────────────────────────────────

/// One planted fact: an evidence turn in the corpus, a probe phrasing the
/// same fact differently, and the marker that identifies the evidence.
struct GoldSpec {
    /// Distinctive substring of the evidence turn — present verbatim in
    /// whatever a correct retrieval returns.
    marker: &'static str,
    /// The evidence turn as it is ingested.
    evidence: &'static str,
    /// The probe: same fact, different wording.
    query: &'static str,
    /// `true` = the query shares at least one content token with the
    /// evidence (the BM25-reachable slice); `false` = zero overlap (only a
    /// semantic leg can reach it). Verified at startup, never assumed.
    keyword: bool,
}

const GOLDS: &[GoldSpec] = &[
    // ── Keyword slice: a BM25 leg can win these on its own ──────────────
    GoldSpec {
        marker: "mongoose",
        evidence: "the billing job uses the mongoose ORM",
        query: "which ORM does the billing job use",
        keyword: true,
    },
    GoldSpec {
        marker: "PostgreSQL 16",
        evidence: "the analytics warehouse was upgraded to PostgreSQL 16 in march",
        query: "when was the analytics warehouse upgraded to PostgreSQL",
        keyword: true,
    },
    GoldSpec {
        marker: "kubernetes cluster",
        evidence: "the ingest workers now run on a kubernetes cluster in frankfurt",
        query: "where do the ingest workers run their kubernetes jobs",
        keyword: true,
    },
    GoldSpec {
        marker: "oat milk",
        evidence: "my usual coffee order is a flat white with oat milk",
        query: "what milk goes in my coffee order",
        keyword: true,
    },
    GoldSpec {
        marker: "redis cluster",
        evidence: "the session cache was moved to a redis cluster with three shards",
        query: "which cache did we move to a redis cluster",
        keyword: true,
    },
    GoldSpec {
        marker: "jitter",
        evidence: "the flaky retry test was fixed by adding jitter to the backoff",
        query: "how was the flaky retry test fixed",
        keyword: true,
    },
    // ── Semantic-only slice: zero token overlap with the query ──────────
    GoldSpec {
        marker: "neovim",
        evidence: "all my coding happens in neovim with a custom lua config",
        query: "which editor do I use",
        keyword: false,
    },
    GoldSpec {
        marker: "dim palette",
        evidence: "I switch every interface to a dim palette after sunset",
        query: "do I prefer light or dark themes",
        keyword: false,
    },
    GoldSpec {
        marker: "six engineers",
        evidence: "the on-call rotation covers six engineers across two time zones",
        query: "how big is the pager group",
        keyword: false,
    },
    GoldSpec {
        marker: "384-dimensional",
        evidence: "we settled on a 384-dimensional sentence encoder for the recall pipeline",
        query: "which embedding model does the memory service use",
        keyword: false,
    },
    GoldSpec {
        marker: "non-cryptographic",
        evidence: "account names are hashed with a tiny non-cryptographic hash before they hit the filesystem",
        query: "how does the server turn user ids into file paths",
        keyword: false,
    },
    GoldSpec {
        marker: "conflict-free",
        evidence: "simultaneous edits are merged with a conflict-free replicated data type",
        query: "how do two devices reconcile their changes",
        keyword: false,
    },
];

/// Distractor vocabulary. Deliberately topic-adjacent (an ingest store is
/// full of `billing`/`cache`/`schema` turns, so the keyword slice faces real
/// competition) but free of every marker above — enforced at startup.
const FILLER_NOUNS: &[&str] = &[
    "billing",
    "ingest",
    "search",
    "cache",
    "auth",
    "reporting",
    "sync",
    "index",
    "upload",
    "notify",
    "shard",
    "schema",
    "retention",
    "migration",
];

const FILLER_NAMES: &[&str] = &[
    "dana", "ravi", "mei", "oscar", "lena", "tomas", "priya", "kai",
];

const FILLER_TEMPLATES: &[&str] = &[
    "{n} reviewed the {w} dashboard before the standup",
    "the {w} pipeline re-ran after the {w2} change",
    "we archived the old {w} exports to cold storage",
    "{n} swapped the {w} config to the new format",
    "the {w} alert fired twice overnight",
    "the team split the {w} rollout into two batches",
    "the {w} dashboard shows a dip the {w2} job did not",
    "{n} documented the {w} runbook for the next rotation",
];

// ─── Corpus ───────────────────────────────────────────────────────────────

/// One user's ingested store: sessions of (role, text, ts) turns.
struct UserCorpus {
    sessions: Vec<(String, Vec<(String, String, i64)>)>,
}

/// A gold fact and where its evidence turn lives. `turn` also makes the
/// chunk id deterministic — `raw:{session}:req-{session}:{turn}`.
struct PlacedGold {
    user: usize,
    session: usize,
    turn: usize,
    spec: &'static GoldSpec,
}

impl PlacedGold {
    fn session_id(&self) -> String {
        format!("u{}-s{}", self.user, self.session)
    }

    fn chunk_id(&self) -> String {
        let session = self.session_id();
        format!("raw:{session}:req-{session}:{}", self.turn)
    }
}

/// Build `users` corpora plus the gold placement. Deterministic in `seed`:
/// which filler sentence lands in which turn is drawn from the seeded RNG,
/// and the gold positions follow from the gold index.
fn generate(
    seed: u64,
    users: usize,
    sessions: usize,
    turns: usize,
) -> (Vec<UserCorpus>, Vec<PlacedGold>) {
    let mut rng = SplitMix64(seed);
    let base_ts: i64 = 1_700_000_000;

    // The k-th gold of a user goes to session `k % sessions`, turn
    // `3 + k / sessions` — two golds of one user never share a turn.
    let placed: Vec<PlacedGold> = GOLDS
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let k = i / users;
            PlacedGold {
                user: i % users,
                session: k % sessions,
                turn: 3 + k / sessions,
                spec,
            }
        })
        .collect();

    let mut corpora: Vec<UserCorpus> = Vec::with_capacity(users);
    for user in 0..users {
        let mut sessions_out = Vec::with_capacity(sessions);
        for s in 0..sessions {
            let mut turns_out = Vec::with_capacity(turns);
            for t in 0..turns {
                let ts = base_ts + ((user * 1000 + s * 16 + t) as i64) * 60;
                let text = match placed
                    .iter()
                    .find(|g| g.user == user && g.session == s && g.turn == t)
                {
                    Some(gold) => gold.spec.evidence.to_string(),
                    None => {
                        let template = FILLER_TEMPLATES[rng.below(FILLER_TEMPLATES.len())];
                        let noun = FILLER_NOUNS[rng.below(FILLER_NOUNS.len())];
                        let noun2 = FILLER_NOUNS[rng.below(FILLER_NOUNS.len())];
                        let name = FILLER_NAMES[rng.below(FILLER_NAMES.len())];
                        template
                            .replace("{w2}", noun2)
                            .replace("{w}", noun)
                            .replace("{n}", name)
                    }
                };
                let role = if t % 2 == 0 { "user" } else { "assistant" };
                turns_out.push((role.to_string(), text, ts));
            }
            sessions_out.push((format!("u{user}-s{s}"), turns_out));
        }
        corpora.push(UserCorpus {
            sessions: sessions_out,
        });
    }
    (corpora, placed)
}

/// The startup integrity check. A harness that lies about its own slices
/// produces a table nobody can act on, so this runs before the expensive
/// ingest and aborts on any violation: every marker is verbatim in its
/// evidence turn, every gold sits inside the corpus, the slice labels match
/// the actual token overlap, and no distractor leaks a marker.
fn verify_corpus(corpora: &[UserCorpus], placed: &[PlacedGold]) -> Result<()> {
    use causal_memory::patterns::tokenize_expanded;

    for gold in placed {
        let spec = gold.spec;
        if !spec.evidence.contains(spec.marker) {
            return Err(anyhow!(
                "gold '{}': marker is not verbatim in its evidence turn",
                spec.marker
            ));
        }
        let query_tokens = tokenize_expanded(spec.query);
        let evidence_tokens = tokenize_expanded(spec.evidence);
        let shared = query_tokens.iter().any(|t| evidence_tokens.contains(t));
        if spec.keyword && !shared {
            return Err(anyhow!(
                "gold '{}' is labeled keyword but shares no token with its query",
                spec.marker
            ));
        }
        if !spec.keyword && shared {
            return Err(anyhow!(
                "gold '{}' is labeled semantic-only but shares a token with its query",
                spec.marker
            ));
        }
        let placed_ok = corpora
            .get(gold.user)
            .and_then(|c| c.sessions.get(gold.session))
            .is_some_and(|(id, turns)| *id == gold.session_id() && gold.turn < turns.len());
        if !placed_ok {
            return Err(anyhow!(
                "gold '{}' is placed outside the corpus ({} × {} grid, user {} session {})",
                spec.marker,
                corpora.len(),
                corpora.first().map_or(0, |c| c.sessions.len()),
                gold.user,
                gold.session
            ));
        }
    }

    for (user, corpus) in corpora.iter().enumerate() {
        for (session, (_, turns)) in corpus.sessions.iter().enumerate() {
            for (turn, (_, text, _)) in turns.iter().enumerate() {
                // The gold's own evidence turn is *supposed* to carry its
                // marker; every other turn must not.
                if placed
                    .iter()
                    .any(|g| g.user == user && g.session == session && g.turn == turn)
                {
                    continue;
                }
                for spec in GOLDS {
                    if text.contains(spec.marker) {
                        return Err(anyhow!(
                            "distractor turn leaks the marker '{}': {text}",
                            spec.marker
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

// ─── Arms ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    Spread,
    Fused,
    Merge,
}

impl Arm {
    const ALL: [Arm; 3] = [Arm::Spread, Arm::Fused, Arm::Merge];

    fn label(self) -> &'static str {
        match self {
            Arm::Spread => "spread",
            Arm::Fused => "fused",
            Arm::Merge => "merge",
        }
    }

    /// One retrieval, capped exactly like the AMC server caps a response
    /// (the `.take(top_k)` at its exit).
    fn search(self, memory: &Memory, query: &str, top_k: usize) -> Vec<MemoryHit> {
        match self {
            Arm::Spread => memory
                .search_memory_entries(query, None, None, top_k)
                .0
                .into_iter()
                .take(top_k)
                .collect(),
            Arm::Fused => memory.search_chunks_fused(query, top_k).0,
            Arm::Merge => {
                let spread = memory.search_memory_entries(query, None, None, top_k).0;
                let fused = memory.search_chunks_fused(query, top_k).0;
                Memory::rrf_merge_hits(&[spread.as_slice(), fused.as_slice()], top_k)
            }
        }
    }
}

/// Rank (1-based) of the first hit carrying `marker`, or None.
fn marker_rank(hits: &[MemoryHit], marker: &str) -> Option<usize> {
    hits.iter()
        .position(|h| h.content.contains(marker))
        .map(|i| i + 1)
}

// ─── Metrics ──────────────────────────────────────────────────────────────

struct ArmRun {
    /// `(is_keyword_slice, rank_of_first_marker_hit)`, one entry per gold.
    per_gold: Vec<(bool, Option<usize>)>,
    latencies_ms: Vec<f64>,
}

impl ArmRun {
    fn recall_at(&self, k: usize) -> f64 {
        Self::recall_for(&self.per_gold, k)
    }

    fn recall_at_slice(&self, k: usize, keyword: bool) -> f64 {
        let slice: Vec<(bool, Option<usize>)> = self
            .per_gold
            .iter()
            .copied()
            .filter(|(kw, _)| *kw == keyword)
            .collect();
        Self::recall_for(&slice, k)
    }

    fn recall_for(rows: &[(bool, Option<usize>)], k: usize) -> f64 {
        if rows.is_empty() {
            return 0.0;
        }
        let hits = rows
            .iter()
            .filter(|(_, rank)| rank.is_some_and(|r| r <= k))
            .count();
        hits as f64 / rows.len() as f64
    }

    fn mrr(&self) -> f64 {
        if self.per_gold.is_empty() {
            return 0.0;
        }
        let sum: f64 = self
            .per_gold
            .iter()
            .map(|(_, rank)| rank.map_or(0.0, |r| 1.0 / r as f64))
            .sum();
        sum / self.per_gold.len() as f64
    }

    fn percentile(&self, p: f64) -> f64 {
        if self.latencies_ms.is_empty() {
            return 0.0;
        }
        let mut sorted = self.latencies_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[idx]
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────

/// Positional value for `--flag value`.
fn value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str> {
    args.get(i)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("{flag} needs a value"))
}

fn run(args: &[String]) -> Result<()> {
    let mut seed = 42u64;
    let mut users = 3usize;
    let mut sessions = 6usize;
    let mut turns = 12usize;
    let mut out_dir: Option<PathBuf> = None;
    let mut db_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                seed = value(args, i + 1, "--seed")?.parse()?;
                i += 1;
            }
            "--users" => {
                users = value(args, i + 1, "--users")?.parse()?;
                i += 1;
            }
            "--sessions" => {
                sessions = value(args, i + 1, "--sessions")?.parse()?;
                i += 1;
            }
            "--turns" => {
                turns = value(args, i + 1, "--turns")?.parse()?;
                i += 1;
            }
            "--out" => {
                out_dir = Some(PathBuf::from(value(args, i + 1, "--out")?));
                i += 1;
            }
            "--db-dir" => {
                db_dir = Some(PathBuf::from(value(args, i + 1, "--db-dir")?));
                i += 1;
            }
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }
    if users == 0 || sessions == 0 || turns < 6 {
        anyhow::bail!("--users/--sessions must be > 0 and --turns >= 6");
    }

    // The A/B is half semantic. Without a live embedder the table would
    // measure BM25 three times over and read as "the paths are equivalent" —
    // refuse to publish that.
    if !causal_memory::embed::embedder_available() {
        eprintln!("bench-amc-ab needs a LIVE embedder — the semantic leg is half the A/B.");
        eprintln!("  build with --features local-embed (and a populated FASTEMBED_CACHE_DIR),");
        eprintln!("  or set CAUSAL_MEMORY_EMBED_API + CAUSAL_MEMORY_EMBED_KEY.");
        std::process::exit(1);
    }
    // Unlike the bulk-ingest paths, this harness is *about* chunk vectors: an
    // explicit off would silently delete the fused arm's semantic leg.
    std::env::set_var("CAUSAL_MEMORY_EMBED_WRITE", "1");

    let (corpora, placed) = generate(seed, users, sessions, turns);
    verify_corpus(&corpora, &placed)?;

    let db_dir = db_dir.unwrap_or_else(|| std::env::temp_dir().join("causal-memory-amc-ab"));
    let _ = std::fs::remove_dir_all(&db_dir);
    std::fs::create_dir_all(&db_dir)?;

    println!("=== bench-amc-ab ===");
    println!(
        "embedder: {} · seed={seed} · {users} users × {sessions} sessions × {turns} turns \
         · {} golds ({} keyword / {} semantic-only)",
        causal_memory::embed::shared_embedder_model().unwrap_or_else(|| "?".into()),
        GOLDS.len(),
        GOLDS.iter().filter(|g| g.keyword).count(),
        GOLDS.len() - GOLDS.iter().filter(|g| g.keyword).count(),
    );

    // 1. Ingest through the real raw write path.
    let mut memories: Vec<Memory> = Vec::with_capacity(users);
    let mut turns_written = 0usize;
    let ingest_t0 = Instant::now();
    for (user, corpus) in corpora.iter().enumerate() {
        let memory = Memory::open(db_dir.join(format!("user{user}.db")))?;
        for (session_id, session_turns) in &corpus.sessions {
            let request = format!("req-{session_id}");
            let batch: Vec<(String, String, Option<i64>)> = session_turns
                .iter()
                .map(|(role, text, ts)| (role.clone(), text.clone(), Some(*ts)))
                .collect();
            turns_written +=
                memory.remember_raw_turns_with_request_id(&batch, session_id, &request);
        }
        memories.push(memory);
    }
    let vectors: i64 = memories
        .iter()
        .map(|m| {
            m.store()
                .with_conn(|c| -> Result<i64> {
                    Ok(c.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0))?)
                })
                .unwrap_or(0)
        })
        .sum();
    let expected_turns = users * sessions * turns;
    println!(
        "ingest: {turns_written}/{expected_turns} turns · {vectors} chunk vectors · {:.1}s",
        ingest_t0.elapsed().as_secs_f64()
    );
    if turns_written != expected_turns {
        anyhow::bail!("ingest lost turns: {turns_written} != {expected_turns}");
    }
    if vectors == 0 {
        anyhow::bail!(
            "no chunk vectors were written — the semantic leg would be dead and the A/B void"
        );
    }
    for gold in &placed {
        let id = gold.chunk_id();
        let exists = memories[gold.user]
            .store()
            .with_conn(|c| -> Result<bool> {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE id = ?1",
                    [id.as_str()],
                    |r| r.get::<_, i64>(0),
                )? > 0)
            })
            .unwrap_or(false);
        if !exists {
            anyhow::bail!(
                "gold '{}' evidence chunk {id} is not in the store",
                gold.spec.marker
            );
        }
    }

    // 2. Warm-up, before any measurement: the embed LRU (every probe text
    // embeds once) and each user's graph (the spread arm pays an O(store)
    // build on its first query — a startup cost, not a retrieval cost).
    for memory in &memories {
        let _ = Arm::Fused.search(memory, "warm the retrieval paths", 10);
        let _ = Arm::Spread.search(memory, "warm the activation graph", 10);
    }
    for gold in &placed {
        let _ = Arm::Fused.search(&memories[gold.user], gold.spec.query, 10);
    }

    // 3. Measure, one arm at a time. Every arm sees the same probe set on the
    // same stores; only the retrieval path differs.
    const TOP_K: usize = 10;
    let mut runs: Vec<(Arm, ArmRun)> = Vec::new();
    for arm in Arm::ALL {
        let mut per_gold = Vec::with_capacity(placed.len());
        let mut latencies_ms = Vec::with_capacity(placed.len());
        for gold in &placed {
            let memory = &memories[gold.user];
            let t0 = Instant::now();
            let hits = arm.search(memory, gold.spec.query, TOP_K);
            latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            per_gold.push((gold.spec.keyword, marker_rank(&hits, gold.spec.marker)));
        }
        runs.push((
            arm,
            ArmRun {
                per_gold,
                latencies_ms,
            },
        ));
    }

    let report = render_report(&runs, seed, users, sessions, turns, turns_written, vectors);
    println!("{report}");

    if let Some(dir) = out_dir {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!(
            "bench-amc-ab-{}.md",
            chrono::Utc::now().timestamp()
        ));
        std::fs::write(&path, &report)?;
        println!("report written to {}", path.display());
    }
    Ok(())
}

fn render_report(
    runs: &[(Arm, ArmRun)],
    seed: u64,
    users: usize,
    sessions: usize,
    turns: usize,
    turns_written: usize,
    vectors: i64,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\ncorpus: seed={seed} · {users} users × {sessions} sessions × {turns} turns \
         ({turns_written} turns ingested, {vectors} chunk vectors) · {} gold queries\n\n",
        runs.first().map_or(0, |(_, r)| r.per_gold.len())
    ));
    out.push_str("| retrieval path | recall@5 | recall@10 | MRR | p50 (ms) | p95 (ms) |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for (arm, run) in runs {
        out.push_str(&format!(
            "| {} | {:.3} | {:.3} | {:.3} | {:.1} | {:.1} |\n",
            arm.label(),
            run.recall_at(5),
            run.recall_at(10),
            run.mrr(),
            run.percentile(0.50),
            run.percentile(0.95),
        ));
    }
    out.push_str(
        "\nrecall@10 by slice — `keyword` = the query and the evidence share a content token:\n\n",
    );
    out.push_str("| retrieval path | keyword | semantic-only |\n|---|---|---|\n");
    for (arm, run) in runs {
        out.push_str(&format!(
            "| {} | {:.3} | {:.3} |\n",
            arm.label(),
            run.recall_at_slice(10, true),
            run.recall_at_slice(10, false),
        ));
    }
    out
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Multi-thread runtime, like the other benches: the retrieval paths
    // bridge to the async embedder with `block_in_place`, which needs one.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { run(&args) })
}
