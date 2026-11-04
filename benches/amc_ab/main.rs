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
//! 2. **Planted gold facts**, each with a differently-worded query and a
//!    choice-question `options` list (one correct answer, 2-3 same-topic
//!    distractors). Three slices, each verified at startup:
//!    - *keyword*: the query and the evidence share content tokens, so a
//!      BM25 leg can find it;
//!    - *semantic-only*: zero token overlap — only a vector leg can;
//!    - *option-reachable*: zero token overlap with the query, but the
//!      CORRECT OPTION's wording does overlap. These are the probes whose
//!      only lexical bridge to the evidence is the option text, i.e. the
//!      slice options-expanded retrieval is supposed to win.
//!    Whether such a probe is ALSO semantically reachable is what the run
//!    measures, not what the label asserts — only the lexical half of every
//!    label is machine-checked.
//! 3. **Marker scoring**: a hit counts when its `content` contains the gold's
//!    distinctive marker string. Arm-agnostic on purpose — for the same raw
//!    turns the spread engine returns `"{decision}" →(no_effect)→ "{outcome}"`
//!    lessons, so key- or shape-based matching would score it zero for a
//!    formatting difference the platform's answer model never sees.
//! 4. Four arms, one measured pass each — `spread`, `fused`, `merge`, and
//!    `fused+options` (the same fused path driven with the gold's options,
//!    which is what `AMC_OPTIONS_RETRIEVAL=on` turns on). Every arm sees the
//!    same stores and the same golds, so the table's recall columns are
//!    **counts over a fixed denominator** (`33/42`, not `0.786`) and the
//!    paired table reports the same gold flipping between two arms.
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
//!
//! `CAUSAL_MEMORY_QUERY_PREFIX=off` turns off the model's query-side
//! instruction, so the query-prefix fix can be A/B'd by running this harness
//! twice on the same seed (the arms are not affected — the prefix is a
//! property of the query embedding, not of the retrieval path).

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

/// Which probe shape a gold exercises. Verified against the actual token
/// overlap at startup — never taken on trust.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slice {
    /// The query shares a content token with the evidence: BM25-reachable.
    Keyword,
    /// Zero overlap: only a semantic leg can reach it.
    Semantic,
    /// Zero overlap with the QUERY, but the correct option's wording does
    /// overlap — the option text is the only lexical bridge.
    OptionReachable,
}

impl Slice {
    const ALL: [Slice; 3] = [Slice::Keyword, Slice::Semantic, Slice::OptionReachable];

    fn label(self) -> &'static str {
        match self {
            Slice::Keyword => "keyword",
            Slice::Semantic => "semantic-only",
            Slice::OptionReachable => "option-reachable",
        }
    }
}

/// One planted fact: an evidence turn in the corpus, a probe phrasing the
/// same fact differently, the choice-question options that accompany it, and
/// the marker that identifies the evidence.
struct GoldSpec {
    /// Distinctive substring of the evidence turn — present verbatim in
    /// whatever a correct retrieval returns.
    marker: &'static str,
    /// The evidence turn as it is ingested.
    evidence: &'static str,
    /// The probe: same fact, different wording.
    query: &'static str,
    /// The choice-question candidates, as the platform sends them:
    /// `options[0]` is the CORRECT answer and must contain the marker; the
    /// rest are same-topic distractors that must not. Both are enforced at
    /// startup, so "the correct option" is a checked claim, not a comment.
    options: &'static [&'static str],
    slice: Slice,
}

const GOLDS: &[GoldSpec] = &[
    // ── Keyword slice: a BM25 leg can win these on its own ──────────────
    GoldSpec {
        marker: "mongoose",
        evidence: "the billing job uses the mongoose ORM",
        query: "which ORM does the billing job use",
        options: &["the mongoose ORM", "sequelize models", "a prisma client", "typeorm entities"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "PostgreSQL 16",
        evidence: "the analytics warehouse was upgraded to PostgreSQL 16 in march",
        query: "when was the analytics warehouse upgraded to PostgreSQL",
        options: &["PostgreSQL 16", "MySQL 8", "SQLite 3", "Oracle 19c"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "kubernetes cluster",
        evidence: "the ingest workers now run on a kubernetes cluster in amsterdam",
        query: "where do the ingest workers run their kubernetes jobs",
        options: &[
            "the kubernetes cluster in Amsterdam",
            "a bare metal fleet",
            "ECS tasks",
            "Nomad jobs",
        ],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "oat milk",
        evidence: "my usual coffee order is a flat white with oat milk",
        query: "what milk goes in my coffee order",
        options: &["oat milk", "whole milk", "soy milk", "almond milk"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "redis cluster",
        evidence: "the session cache was moved to a redis cluster with three shards",
        query: "which cache did we move to a redis cluster",
        options: &[
            "a redis cluster with three shards",
            "a memcached pool",
            "a postgres table",
            "a local file cache",
        ],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "jitter",
        evidence: "the flaky retry test was fixed by adding jitter to the backoff",
        query: "how was the flaky retry test fixed",
        options: &[
            "adding jitter to the backoff",
            "raising the retry limit",
            "pinning the system clock",
            "disabling the test",
        ],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "gRPC",
        evidence: "the internal service mesh moved from REST to gRPC last quarter",
        query: "which protocol does the internal service mesh speak now",
        options: &["gRPC", "GraphQL", "SOAP", "Thrift"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "terraform",
        evidence: "all the staging infrastructure is described in terraform modules",
        query: "how is the staging infrastructure described",
        options: &[
            "terraform modules",
            "ansible playbooks",
            "cloudformation templates",
            "pulumi stacks",
        ],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "sourdough",
        evidence: "I bake sourdough every sunday morning",
        query: "what do I bake every sunday",
        options: &["sourdough", "focaccia", "brioche", "a banana loaf"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "Grafana",
        evidence: "the latency dashboards live in Grafana on the ops host",
        query: "where do the latency dashboards live",
        options: &["in Grafana", "in Kibana", "in Datadog", "in a spreadsheet"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "ClickHouse",
        evidence: "the event rollups are stored in ClickHouse for the analytics team",
        query: "what stores the event rollups",
        options: &["ClickHouse", "BigQuery", "DuckDB", "Redshift"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "pytest",
        evidence: "the python services run their tests with pytest in CI",
        query: "how do the python services run their tests",
        options: &["with pytest", "with unittest", "with behave", "with nose"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "Frankfurt",
        evidence: "the primary region for the new cluster is Frankfurt",
        query: "which region hosts the new primary cluster",
        options: &["Frankfurt", "Dublin", "Singapore", "Sydney"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "two weeks",
        evidence: "the incident review is due two weeks after the outage",
        query: "how long after the outage is the incident review due",
        options: &["two weeks", "one week", "one month", "the next sprint"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "Rust",
        evidence: "the edge proxy was rewritten in Rust to cut the memory footprint",
        query: "what language was the edge proxy rewritten in",
        options: &["Rust", "Go", "Zig", "C++"],
        slice: Slice::Keyword,
    },
    GoldSpec {
        marker: "blue-green",
        evidence: "deploys use a blue-green swap so rollback is instant",
        query: "how do deploys make rollback instant",
        options: &[
            "a blue-green swap",
            "a canary release",
            "a recreate rollout",
            "a rolling update",
        ],
        slice: Slice::Keyword,
    },
    // ── Semantic-only slice: zero token overlap with the query ──────────
    GoldSpec {
        marker: "neovim",
        evidence: "all my coding happens in neovim with a custom lua config",
        query: "which editor do I use",
        options: &["neovim", "VS Code", "IntelliJ", "Sublime Text"],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "dim palette",
        evidence: "I switch every interface to a dim palette after sunset",
        query: "do I prefer light or dark themes",
        options: &[
            "a dim palette",
            "a bright palette",
            "the system default",
            "high contrast mode",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "six engineers",
        evidence: "the on-call rotation covers six engineers across two time zones",
        query: "how big is the pager group",
        options: &[
            "six engineers",
            "three engineers",
            "a dozen engineers",
            "two engineers",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "384-dimensional",
        evidence: "we settled on a 384-dimensional sentence encoder for the recall pipeline",
        query: "which embedding model does the memory service use",
        options: &[
            "a 384-dimensional encoder",
            "a 768-dimensional encoder",
            "a 1536-dimensional encoder",
            "a sparse BM25 index",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "non-cryptographic",
        evidence: "account names are hashed with a tiny non-cryptographic hash before they hit the filesystem",
        query: "how does the server turn user ids into file paths",
        options: &[
            "a non-cryptographic hash",
            "a SHA-256 digest",
            "an encrypted mapping",
            "a sequential counter",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "conflict-free",
        evidence: "simultaneous edits are merged with a conflict-free replicated data type",
        query: "how do two devices reconcile their changes",
        options: &[
            "a conflict-free replicated data type",
            "a last-write-wins timestamp",
            "a central lock",
            "a manual merge",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "standing desk",
        evidence: "my desk at home is a standing desk I crank up after lunch",
        query: "do I prefer to sit or stay upright while typing",
        options: &[
            "a standing desk",
            "a kneeling chair",
            "a treadmill desk",
            "a yoga ball",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "twice a year",
        evidence: "the whole team flies out for an offsite twice a year",
        query: "how often does everyone meet in person",
        options: &[
            "twice a year",
            "once a quarter",
            "every month",
            "every other year",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "sleep tracker",
        evidence: "I wear a sleep tracker ring to bed every night",
        query: "how do I keep an eye on my rest",
        options: &[
            "a sleep tracker ring",
            "a fitness watch",
            "a chest strap",
            "a phone alarm",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "fifty percent",
        evidence: "we cut the cold start time by fifty percent after the rewrite",
        query: "how much faster is the service now",
        options: &[
            "fifty percent faster",
            "twice as fast",
            "ten percent faster",
            "no faster at all",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "one on one",
        evidence: "my manager and I sync one on one every friday",
        query: "how regularly does the user talk to the boss",
        options: &[
            "a weekly one on one",
            "a monthly team review",
            "an email thread",
            "a quarterly survey",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "cast iron",
        evidence: "I cook almost everything in a cast iron skillet",
        query: "what cookware do I reach for most",
        options: &[
            "a cast iron skillet",
            "a nonstick pan",
            "a stainless steel pot",
            "a wok",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "three days",
        evidence: "the team is remote and meets in the office three days a week",
        query: "how often does everyone come in",
        options: &[
            "three days a week",
            "every day",
            "once a month",
            "twice a week",
        ],
        slice: Slice::Semantic,
    },
    GoldSpec {
        marker: "cassette",
        evidence: "my first album was a cassette I bought at a flea market",
        query: "what started the record collection",
        options: &["a cassette", "a CD", "a minidisc", "a streaming playlist"],
        slice: Slice::Semantic,
    },
    // ── Option-reachable slice: the OPTION is the only lexical bridge ───
    // Every query here is oblique on purpose (it shares no content token
    // with the evidence), while options[0] names the evidence's own
    // vocabulary. This is the shape the AMC choice questions arrive in.
    GoldSpec {
        marker: "espresso",
        evidence: "I start the day with a double espresso from the corner shop",
        query: "what gets me going in the morning",
        options: &[
            "a double espresso",
            "a pot of green tea",
            "an orange juice",
            "a glass of water",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "budget spreadsheet",
        evidence: "every purchase over a hundred dollars goes into a budget spreadsheet first",
        query: "how do I keep track of spending",
        options: &[
            "a budget spreadsheet",
            "a banking app",
            "a paper ledger",
            "a shoebox of receipts",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "window seat",
        evidence: "I always book a window seat when I fly for work",
        query: "what small thing makes a trip better",
        options: &["a window seat", "an aisle seat", "extra legroom", "a lounge pass"],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "noise cancelling headphones",
        evidence: "I put on noise cancelling headphones the moment the room fills up",
        query: "what helps me concentrate when it is busy around me",
        options: &[
            "noise cancelling headphones",
            "a white noise machine",
            "lo-fi playlists",
            "a desk by the window",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "sqlite",
        evidence: "the side project keeps everything in a single sqlite file",
        query: "how is that hobby app storing its data",
        options: &[
            "one sqlite file",
            "a postgres server",
            "a json file on disk",
            "a hosted database",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "hand written",
        evidence: "the changelog is hand written by the on-call engineer",
        query: "how does the team document what ships",
        options: &[
            "hand written by the on-call engineer",
            "generated from the commit log",
            "pasted from the ticket tracker",
            "summarised by a tool",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "night shift",
        evidence: "the batch jobs all run on a night shift schedule",
        query: "when does the heavy processing happen",
        options: &[
            "a night shift schedule",
            "during the morning lull",
            "continuously through the day",
            "at the end of the quarter",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "bamboo",
        evidence: "my desk chair has a bamboo frame",
        query: "which natural material shows up in the workspace",
        options: &["a bamboo frame", "recycled plastic", "brushed aluminium", "solid oak"],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "compost",
        evidence: "the kitchen scraps all go into a compost bin",
        query: "how does the house deal with food waste",
        options: &[
            "into a compost bin",
            "into the regular trash",
            "to a neighbour's chickens",
            "down the sink",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "paper map",
        evidence: "on trips I still navigate with a paper map",
        query: "what do I rely on when I am somewhere new",
        options: &[
            "a paper map",
            "the maps app on my phone",
            "asking a local",
            "street signs",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "vinyl",
        evidence: "I still buy vinyl records from the shop on the corner",
        query: "how do I support the local music scene",
        options: &[
            "buying vinyl records",
            "a streaming subscription",
            "gig tickets",
            "band merch",
        ],
        slice: Slice::OptionReachable,
    },
    GoldSpec {
        marker: "kayak",
        evidence: "most weekends I take the kayak out on the river",
        query: "what do I do to unwind outdoors",
        options: &[
            "take the kayak out",
            "go for a long run",
            "work in the garden",
            "read on the balcony",
        ],
        slice: Slice::OptionReachable,
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
/// the actual token overlap, the correct option is really the one carrying
/// the answer, and no distractor leaks a marker — not its own gold's, not
/// another gold's, not from the option list, not from the filler turns.
fn verify_corpus(corpora: &[UserCorpus], placed: &[PlacedGold]) -> Result<()> {
    use causal_memory::patterns::tokenize_expanded;

    // Two golds sharing a marker would cross-score: a hit on either turn
    // would count for both.
    for (i, a) in GOLDS.iter().enumerate() {
        if GOLDS.iter().skip(i + 1).any(|b| b.marker == a.marker) {
            return Err(anyhow!("two golds share the marker '{}'", a.marker));
        }
    }

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
        match spec.slice {
            Slice::Keyword if !shared => {
                return Err(anyhow!(
                    "gold '{}' is labeled keyword but shares no token with its query",
                    spec.marker
                ));
            }
            // Both non-keyword slices need ZERO query/evidence overlap: the
            // point of each is that the query wording carries no lexical
            // handle on the evidence.
            Slice::Semantic | Slice::OptionReachable if shared => {
                return Err(anyhow!(
                    "gold '{}' is labeled {} but shares a token with its query",
                    spec.marker,
                    spec.slice.label()
                ));
            }
            _ => {}
        }

        // ── options invariants ──
        let (correct, distractors) = spec
            .options
            .split_first()
            .ok_or_else(|| anyhow!("gold '{}' carries no options", spec.marker))?;
        if distractors.is_empty() {
            return Err(anyhow!(
                "gold '{}' has no distractor option (a choice question needs one)",
                spec.marker
            ));
        }
        if !correct.contains(spec.marker) {
            return Err(anyhow!(
                "gold '{}': the correct option '{correct}' does not carry the marker",
                spec.marker
            ));
        }
        if spec.options.iter().filter(|o| *o == correct).count() > 1 {
            return Err(anyhow!("gold '{}' repeats its correct option", spec.marker));
        }
        for distractor in distractors {
            if distractor.contains(spec.marker) {
                return Err(anyhow!(
                    "gold '{}': distractor '{distractor}' carries the marker — it would \
                     be a second correct answer",
                    spec.marker
                ));
            }
            if let Some(other) = GOLDS.iter().find(|g| distractor.contains(g.marker)) {
                return Err(anyhow!(
                    "gold '{}': distractor '{distractor}' carries the marker '{}' of \
                     another gold — it would leak that gold's answer",
                    spec.marker,
                    other.marker
                ));
            }
        }
        if let Slice::OptionReachable = spec.slice {
            // The defining property of this slice: the option is the ONLY
            // lexical bridge to the evidence. Without it the gold would be
            // indistinguishable from semantic-only, and the slice would
            // measure nothing about options.
            let option_tokens = tokenize_expanded(correct);
            if !option_tokens.iter().any(|t| evidence_tokens.contains(t)) {
                return Err(anyhow!(
                    "gold '{}' is labeled option-reachable but its correct option shares \
                     no token with the evidence",
                    spec.marker
                ));
            }
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
                let own = placed
                    .iter()
                    .find(|g| g.user == user && g.session == session && g.turn == turn);
                for spec in GOLDS {
                    // A gold's own evidence turn is *supposed* to carry its
                    // marker — but only its own. A second marker on it would
                    // make that gold score a hit on someone else's fact.
                    if own.is_some_and(|g| g.spec.marker == spec.marker) {
                        continue;
                    }
                    if text.contains(spec.marker) {
                        return Err(anyhow!(
                            "{} leaks the marker '{}': {text}",
                            if own.is_some() {
                                "a gold's evidence turn"
                            } else {
                                "a distractor turn"
                            },
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

/// Which retrieval path `/search` runs (`AMC_RETRIEVAL`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Path {
    Spread,
    Fused,
    Merge,
}

/// One measured configuration: a retrieval path, plus whether the query is
/// expanded with the gold's options (`AMC_OPTIONS_RETRIEVAL=on`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Arm {
    path: Path,
    /// Only ever `true` for [`Path::Fused`]: the server expands options
    /// nowhere else, so a `spread+options` arm would measure a configuration
    /// that cannot be deployed — a row nobody could act on.
    options: bool,
}

impl Arm {
    const SPREAD: Arm = Arm {
        path: Path::Spread,
        options: false,
    };
    const FUSED: Arm = Arm {
        path: Path::Fused,
        options: false,
    };
    const FUSED_OPTIONS: Arm = Arm {
        path: Path::Fused,
        options: true,
    };
    const MERGE: Arm = Arm {
        path: Path::Merge,
        options: false,
    };
    const ALL: [Arm; 4] = [Arm::SPREAD, Arm::FUSED, Arm::FUSED_OPTIONS, Arm::MERGE];

    fn label(self) -> &'static str {
        match (self.path, self.options) {
            (Path::Spread, _) => "spread",
            (Path::Fused, false) => "fused",
            (Path::Fused, true) => "fused+options",
            (Path::Merge, _) => "merge",
        }
    }

    /// One retrieval, capped exactly like the AMC server caps a response
    /// (the `.take(top_k)` at its exit) — and, for the options arm, through
    /// the very same library call the server makes, so this table cannot
    /// drift from what `AMC_OPTIONS_RETRIEVAL=on` actually serves.
    fn search(
        self,
        memory: &Memory,
        query: &str,
        options: &[String],
        top_k: usize,
    ) -> Vec<MemoryHit> {
        match self.path {
            Path::Spread => memory
                .search_memory_entries(query, None, None, top_k)
                .0
                .into_iter()
                .take(top_k)
                .collect(),
            Path::Fused if self.options => {
                memory
                    .search_chunks_fused_with_options(query, options, top_k)
                    .0
            }
            Path::Fused => memory.search_chunks_fused(query, top_k).0,
            Path::Merge => {
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
    /// `(slice, rank_of_first_marker_hit)`, one entry per gold — the SAME
    /// gold order in every arm, which is what makes the paired table legal.
    per_gold: Vec<(Slice, Option<usize>)>,
    latencies_ms: Vec<f64>,
}

impl ArmRun {
    /// `(hits, denominator)` within `k`. The pair, not the ratio: "0.750"
    /// says nothing about whether it was measured on 4 golds or 40.
    fn hits_at(&self, k: usize) -> (usize, usize) {
        Self::hits_for(&self.per_gold, k)
    }

    fn hits_at_slice(&self, k: usize, slice: Slice) -> (usize, usize) {
        let rows: Vec<(Slice, Option<usize>)> = self
            .per_gold
            .iter()
            .copied()
            .filter(|(s, _)| *s == slice)
            .collect();
        Self::hits_for(&rows, k)
    }

    fn hits_for(rows: &[(Slice, Option<usize>)], k: usize) -> (usize, usize) {
        let hits = rows
            .iter()
            .filter(|(_, rank)| rank.is_some_and(|r| r <= k))
            .count();
        (hits, rows.len())
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

/// `hits/total`, or `-` for an empty slice (never a fake `0/0`).
fn frac((hits, total): (usize, usize)) -> String {
    if total == 0 {
        "-".to_string()
    } else {
        format!("{hits}/{total}")
    }
}

/// Paired recall@k difference over the SAME gold list: `b` counts golds the
/// first arm hits and the second misses, `c` the reverse. Two arms sharing a
/// denominator makes this the only fair comparison — the unpaired columns
/// can move together with the corpus and hide a real flip.
struct Paired {
    b: usize,
    c: usize,
    n: usize,
}

impl Paired {
    /// `(b - c) / n`, positive when the FIRST arm won.
    fn delta(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        (self.b as f64 - self.c as f64) / self.n as f64
    }

    /// McNemar's exact test, two-sided, on the discordant pairs. Under the
    /// null (both arms equally good) each discordant gold is a fair coin, so
    /// `p = 2 · P(X ≤ min(b, c))` for `X ~ Binomial(b + c, 0.5)`, capped at
    /// 1. With ~40 golds this is the difference between "Δ = 0.14" and "Δ =
    /// 0.14, p = 0.004" — i.e. between a story and a number. Computed with
    /// an iterative pmf so nothing overflows at any n.
    fn mcnemar_p(&self) -> f64 {
        let n = self.b + self.c;
        if n == 0 {
            return 1.0;
        }
        let (nf, k) = (n as f64, self.b.min(self.c));
        let mut pmf = 0.5f64.powf(nf); // P(X = 0)
        let mut tail = pmf;
        for i in 1..=k {
            pmf *= (nf - i as f64 + 1.0) / i as f64;
            tail += pmf;
        }
        (2.0 * tail).min(1.0)
    }
}

fn paired(first: &ArmRun, second: &ArmRun, k: usize) -> Paired {
    let mut out = Paired {
        b: 0,
        c: 0,
        n: first.per_gold.len(),
    };
    for ((_, a), (_, b)) in first.per_gold.iter().zip(second.per_gold.iter()) {
        let hit_a = a.is_some_and(|r| r <= k);
        let hit_b = b.is_some_and(|r| r <= k);
        match (hit_a, hit_b) {
            (true, false) => out.b += 1,
            (false, true) => out.c += 1,
            _ => {}
        }
    }
    out
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
         · {} golds ({})",
        causal_memory::embed::shared_embedder_model().unwrap_or_else(|| "?".into()),
        GOLDS.len(),
        Slice::ALL
            .iter()
            .map(|s| format!(
                "{} {}",
                GOLDS.iter().filter(|g| g.slice == *s).count(),
                s.label()
            ))
            .collect::<Vec<_>>()
            .join(" / "),
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
    // The options arm gets its turns here too: each option text is its own
    // probe and must not pay a cold embed inside a measured query.
    let gold_options: Vec<Vec<String>> = placed
        .iter()
        .map(|g| g.spec.options.iter().map(|o| (*o).to_string()).collect())
        .collect();
    for memory in &memories {
        let _ = Arm::FUSED.search(memory, "warm the retrieval paths", &[], 10);
        let _ = Arm::FUSED_OPTIONS.search(
            memory,
            "warm the option probes",
            &["warm the option probes".to_string()],
            10,
        );
        let _ = Arm::SPREAD.search(memory, "warm the activation graph", &[], 10);
    }
    for (gold, options) in placed.iter().zip(&gold_options) {
        // The plain fused arm ignores `options` (it takes the slice only to
        // share the call shape), so it warms the query alone; the options arm
        // warms the query AND every option text.
        let _ = Arm::FUSED.search(&memories[gold.user], gold.spec.query, &[], 10);
        let _ = Arm::FUSED_OPTIONS.search(&memories[gold.user], gold.spec.query, options, 10);
    }

    // 3. Measure, one arm at a time. Every arm sees the same probe set on the
    // same stores, in the same gold order; only the retrieval configuration
    // differs.
    const TOP_K: usize = 10;
    let mut runs: Vec<(Arm, ArmRun)> = Vec::new();
    for arm in Arm::ALL {
        let mut per_gold = Vec::with_capacity(placed.len());
        let mut latencies_ms = Vec::with_capacity(placed.len());
        for (gold, options) in placed.iter().zip(&gold_options) {
            let memory = &memories[gold.user];
            let t0 = Instant::now();
            let hits = arm.search(memory, gold.spec.query, options, TOP_K);
            latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            per_gold.push((gold.spec.slice, marker_rank(&hits, gold.spec.marker)));
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
         ({turns_written} turns ingested, {vectors} chunk vectors) · {} gold queries\n",
        runs.first().map_or(0, |(_, r)| r.per_gold.len())
    ));
    out.push_str("slices:");
    for slice in Slice::ALL {
        let n = GOLDS.iter().filter(|g| g.slice == slice).count();
        out.push_str(&format!(" {} {n} ·", slice.label()));
    }
    out.push_str("\n\n");

    // Recall is printed as a count over its own denominator: the same gold
    // list backs every arm, so `33/42` is comparable across rows and cannot
    // be misread as a share of a different probe set.
    out.push_str("| retrieval path | recall@5 | recall@10 | MRR | p50 (ms) | p95 (ms) |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for (arm, run) in runs {
        out.push_str(&format!(
            "| {} | {} | {} | {:.3} | {:.1} | {:.1} |\n",
            arm.label(),
            frac(run.hits_at(5)),
            frac(run.hits_at(10)),
            run.mrr(),
            run.percentile(0.50),
            run.percentile(0.95),
        ));
    }

    out.push_str(
        "\nrecall@10 by slice — `keyword` = the query and the evidence share a content \
         token; `semantic-only` = zero overlap; `option-reachable` = no query overlap, \
         but the CORRECT option's wording does overlap (so the option text is the only \
         lexical bridge):\n\n",
    );
    out.push_str("| retrieval path |");
    for slice in Slice::ALL {
        let n = GOLDS.iter().filter(|g| g.slice == slice).count();
        out.push_str(&format!(" {} (n={n}) |", slice.label()));
    }
    out.push_str("\n|---|");
    for _ in Slice::ALL {
        out.push_str("---|");
    }
    out.push('\n');
    for (arm, run) in runs {
        out.push_str(&format!("| {} |", arm.label()));
        for slice in Slice::ALL {
            out.push_str(&format!(" {} |", frac(run.hits_at_slice(10, slice))));
        }
        out.push('\n');
    }

    // Paired comparison of every arm against the default (`fused`, options
    // off) and of the options arm against its own baseline — the two
    // questions this run exists to answer.
    out.push_str(
        "\npaired recall@10 — same golds, two arms: b = first arm hits & second misses, \
         c = the reverse; Δ = (b−c)/n is positive when the FIRST arm won; p = McNemar \
         exact two-sided over the discordant pairs (with ~40 golds a bare Δ is a story, \
         a Δ with p is a number):\n\n",
    );
    out.push_str("| first → second | b | c | Δ | p |\n|---|---|---|---|---|\n");
    for (first, run_a) in runs {
        for (second, run_b) in runs {
            if first == second {
                continue;
            }
            let p = paired(run_a, run_b, 10);
            out.push_str(&format!(
                "| {} → {} | {} | {} | {:+.3} | {:.3} |\n",
                first.label(),
                second.label(),
                p.b,
                p.c,
                p.delta(),
                p.mcnemar_p(),
            ));
        }
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
