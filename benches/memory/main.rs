//! `bench-memory`: comprehensive memory-system evaluation, LLM end-to-end.
//!
//! Protocol:
//! 1. **Scenario generation** (seeded, deterministic): a synthetic agent
//!    "life" — facts, preferences, causal lessons (caused/prevented),
//!    trap+fix pairs, a cross-session supersession chain, and a 3-hop causal
//!    chain — all woven into natural dialogue turns with absolute dates.
//! 2. **Write through the REAL pipeline**: each session is distilled by the
//!    LLM (`Distiller`), then recorded through the same path the CLI uses
//!    (facts → agent_facts with supersedes retirement; lessons/causal →
//!    causal_edges). This measures extraction quality, not a mock.
//! 3. **Deterministic scoring**: for every ground-truth item, the
//!    corresponding retrieval tool runs and the gold value must appear in
//!    top-k. No judge LLM — this benchmark measures whether the MEMORY
//!    system returns the right memory.
//! 4. **Two write channels, labeled separately**:
//!    - LLM-extracted items (facts/preferences/causal lessons/supersessions)
//!    - synthetic chain seeds (text-derived chunk ids, as `store::link` in
//!      the test suite) — read-path mechanics that must not depend on LLM
//!      extraction luck (multi-hop chains, forward simulation, reversibility).
//!
//! Metrics: fact/preference/causal recall@k, relation accuracy,
//! supersession detection + reversibility, chain recall, forward-simulation
//! accuracy, trap-warning detection, and per-query token cost.
//!
//! Usage:
//!   causal-memory-bench-memory [--seed 42] [--topk 5] [--out DIR]
//!
//! Env: DEEPSEEK_API_KEY (or CAUSAL_MEMORY_LLM_KEY) — the write path is the
//! real LLM distiller, so (like bench-agent) it refuses to run unconfigured.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use causal_memory::distill::{Distiller, ItemKind, MemoryItem};
use causal_memory::hippocampus::CausalGraph;
use causal_memory::store::CausalStore;

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

// ─── Ground truth ──────────────────────────────────────────────────────────

/// A fact/preference planted in a session's dialogue; the LLM must extract
/// it, and retrieval must return the value.
struct GoldFact {
    key: &'static str,
    value: &'static str,
    /// Tokens the retrieval query will use.
    query: &'static str,
}

/// A causal lesson planted in the dialogue: "doing X caused/enabled/
/// prevented Y".
struct GoldCausal {
    decision: &'static str,
    outcome: &'static str,
    relation: &'static str,
    /// Distinctive retrieval tokens that survive LLM rephrasing (the
    /// planted decision sentence is too brittle for BM25 after extraction).
    query: &'static str,
}

/// The generated scenario: sessions of (speaker, text) turns + gold items.
struct Scenario {
    sessions: Vec<Vec<(String, String)>>,
    facts: Vec<GoldFact>,
    causals: Vec<GoldCausal>,
    /// (old, new) — the new statement must supersede the old.
    supersession: (usize, &'static str, &'static str),
    /// 3-hop chain: consecutive texts link via text-derived chunk ids.
    chain: Vec<(&'static str, &'static str)>,
}

/// Build the fixed scenario, then shuffle session order by seed.
fn generate_scenario(seed: u64) -> Scenario {
    let mut rng = SplitMix64(seed);

    // Facts (key, value, query tokens, session).
    let facts = vec![
        GoldFact { key: "editor", value: "vim", query: "vim", },
        GoldFact { key: "production_db", value: "PostgreSQL 16", query: "PostgreSQL", },
        GoldFact { key: "deployment", value: "Kubernetes", query: "Kubernetes", },
        GoldFact { key: "theme", value: "dark mode", query: "dark mode", },
        GoldFact { key: "coffee_milk", value: "oat milk", query: "oat milk", },
    ];

    // Causal lessons (decision → outcome, relation).
    let causals = vec![
        GoldCausal {
            decision: "deployed without running tests",
            outcome: "production crash that took two hours to roll back",
            relation: "caused",
            query: "tests deploy crash",
        },
        GoldCausal {
            decision: "added input validation",
            outcome: "blocked the SQL injection attempt",
            relation: "prevented",
            query: "validation injection blocked",
        },
        GoldCausal {
            decision: "ran the migration without a backup",
            outcome: "data loss during the rollback",
            relation: "caused",
            query: "migration backup data",
        },
        GoldCausal {
            decision: "took a nightly snapshot before the migration",
            outcome: "the rollback restored everything",
            relation: "prevented",
            query: "snapshot rollback restored",
        },
    ];

    // Sessions: dialogue turns weaving the gold items in naturally
    // (dates come from `session_date(idx)` at ingest time).
    let mut sessions: Vec<Vec<(String, String)>> = vec![
        vec![
            ("user".into(), "I've been using vim for all my editing lately — it's so fast once you get used to it.".into()),
            ("assistant".into(), "vim's modal editing has a learning curve but the speed is real.".into()),
        ],
        vec![
            ("user".into(), "We're running PostgreSQL 16 in production now after the upgrade.".into()),
            ("user".into(), "And we deployed without running tests last week, which caused a production crash. Took two hours to roll back.".into()),
            ("assistant".into(), "That sounds painful — running tests before deploy is a good habit.".into()),
        ],
        vec![
            ("user".into(), "Everything deploys to Kubernetes these days.".into()),
            ("user".into(), "Adding input validation blocked a SQL injection attempt last month — lesson learned.".into()),
        ],
        vec![
            ("user".into(), "I prefer dark mode everywhere, light themes hurt my eyes.".into()),
            ("user".into(), "We ran the migration without a backup and lost data during the rollback. Never again.".into()),
            ("user".into(), "Taking a nightly snapshot before the migration meant the rollback restored everything.".into()),
        ],
        vec![
            ("user".into(), "I switched to oat milk in my coffee, almond milk was getting boring.".into()),
        ],
        // Session 5: supersession — old state.
        vec![
            ("user".into(), "Our session cache is on Redis 7.2.4, it's been fine for months.".into()),
        ],
        // Session 6: supersession — new state (must supersede the old;
        // naming the old version in the same breath is what makes the LLM
        // emit the supersedes hint).
        vec![
            ("user".into(), "We upgraded the session cache from Redis 7.2.4 to Redis 7.4 last week, much better hit rates. The old 7.2.4 setup is gone now.".into()),
        ],
    ];

    // 3-hop causal chain (text-derived chunk ids link the hops):
    // skipped tests → release broke API → hotfix restored it → broke again.
    let chain: Vec<(&str, &str)> = vec![
        ("skipped integration tests before the release", "the release broke the payment API"),
        ("the release broke the payment API", "a hotfix restored the payment API"),
        ("a hotfix restored the payment API", "the API broke again under load"),
    ];

    // Seed a deterministic session order shuffle.
    let n = sessions.len();
    for i in (1..n).rev() {
        let j = rng.below(i + 1);
        sessions.swap(i, j);
    }

    Scenario {
        sessions,
        facts,
        causals,
        supersession: (6, "Redis 7.2.4", "Redis 7.4"),
        chain,
    }
}

// ─── Text-derived chunk seeding (read-path mechanics) ──────────────────────

/// Insert a chunk-id-derived edge (same text → same node), exactly like the
/// store test suite's `link` helper — lets multi-hop chains and simulation
/// graphs form deterministically without depending on LLM extraction.
fn link(store: &CausalStore, from: &str, to: &str, relation: &str, conf: f64) -> Result<i64> {
    store.with_conn(|conn| {
        for text in [from, to] {
            conn.execute(
                "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, 1000)",
                rusqlite::params![format!("chunk:{text}"), text],
            )?;
        }
        conn.execute(
            "INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at)
             VALUES (?1, ?2, ?3, ?4, 'rule', 1000, 1000)",
            rusqlite::params![
                format!("chunk:{from}"),
                format!("chunk:{to}"),
                relation,
                conf
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
}

// ─── Ingestion (LLM distill, real write path) ──────────────────────────────

/// Write distilled items through the production path: facts/preferences →
/// the fact layer (with supersedes retirement), lessons/causal →
/// record_distilled. Mirrors the CLI's write_distilled_items.
fn record_items(
    store: &CausalStore,
    items: &[MemoryItem],
    retired: &mut usize,
) -> Result<()> {
    for item in items {
        match item.kind {
            ItemKind::Fact | ItemKind::Preference => {
                let kind = match item.kind {
                    ItemKind::Fact => "fact",
                    ItemKind::Preference => "preference",
                    _ => "fact",
                };
                let fact_id = store.record_fact(kind, &item.text, "user", "distill", 0.8)?;
                if let Some(hint) = item.supersedes.as_deref() {
                    *retired += store
                        .retire_facts_by_hint(kind, "user", hint, Some(fact_id))
                        .unwrap_or(0);
                }
            }
            ItemKind::Lesson | ItemKind::Event | ItemKind::Causal => {
                let _ = store.record_distilled(item, None)?;
            }
        }
    }
    Ok(())
}

/// Distill every session through the real LLM distiller and record the items.
async fn ingest(
    store: &CausalStore,
    distiller: &Distiller,
    scenario: &Scenario,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    for (idx, turns) in scenario.sessions.iter().enumerate() {
        let date = session_date(idx);
        match distiller.distill_session(date, turns).await {
            Ok(items) => {
                let before = store.count_edges()?;
                let mut retired = 0usize;
                record_items(store, &items, &mut retired)?;
                stats.items_extracted += items.len();
                for item in &items {
                    stats.extracted.push((
                        format!("{:?}", item.kind).to_lowercase(),
                        item.text.clone(),
                    ));
                }
                stats.facts_written += items
                    .iter()
                    .filter(|i| matches!(i.kind, ItemKind::Fact | ItemKind::Preference))
                    .count();
                stats.edges_written += (store.count_edges()? - before) as usize;
                stats.retired += retired;
            }
            Err(e) => {
                eprintln!("⚠️ distill failed for session {idx}: {e}");
                stats.distill_failures += 1;
            }
        }
    }
    Ok(stats)
}

fn session_date(idx: usize) -> &'static str {
    const DATES: [&str; 7] = [
        "2026-06-20", "2026-06-24", "2026-06-28", "2026-07-02",
        "2026-07-06", "2026-07-10", "2026-07-14",
    ];
    DATES[idx % DATES.len()]
}

#[derive(Default)]
struct IngestStats {
    items_extracted: usize,
    facts_written: usize,
    edges_written: usize,
    retired: usize,
    distill_failures: usize,
    /// (kind, text) per extracted item — the extraction-fidelity audit view.
    extracted: Vec<(String, String)>,
}

// ─── Evaluation (deterministic scoring) ────────────────────────────────────

struct Metrics {
    fact_recall: (usize, usize),
    causal_recall: (usize, usize),
    relation_accuracy: (usize, usize),
    /// How often a distilled causal item kept its decision (formed a real
    /// decision → outcome edge) vs. the LLM dropping the decision field.
    decision_attachment: (usize, usize),
    supersession_detected: bool,
    supersession_note: String,
    reversibility: (bool, bool), // (superseded_edges, restored_ok)
    chain_recall: (usize, usize),
    simulation_hits: (usize, usize),
    warning_detected: bool,
    avg_ctx_tokens: f64,
}

/// One row of the per-item token report: (category, label, est. tokens).
type TokenRow = (String, String, usize);

/// Evaluate every gold item against the real retrieval tools.
fn evaluate(
    store: &CausalStore,
    scenario: &Scenario,
    topk: usize,
) -> Result<(Metrics, Vec<TokenRow>)> {
    let mut m = Metrics {
        fact_recall: (0, 0),
        causal_recall: (0, 0),
        relation_accuracy: (0, 0),
        decision_attachment: (0, 0),
        supersession_detected: false,
        supersession_note: String::new(),
        reversibility: (false, false),
        chain_recall: (0, 0),
        simulation_hits: (0, 0),
        warning_detected: false,
        avg_ctx_tokens: 0.0,
    };
    let mut rows: Vec<TokenRow> = Vec::new(); // (category, label, tokens)
    let mut token_sum = 0usize;
    let mut token_n = 0usize;

    // ── Facts & preferences: search_facts_bm25 with the value tokens ─────
    for f in &scenario.facts {
        m.fact_recall.1 += 1;
        let hits = store.search_facts_bm25(f.query, None, topk).unwrap_or_default();
        let ctx = hits
            .iter()
            .map(|h| h.value.clone())
            .collect::<Vec<_>>()
            .join("\n");
        token_sum += causal_memory::token::estimate_tokens(&ctx);
        token_n += 1;
        let gold_lower = f.value.to_lowercase();
        let hit = hits.iter().any(|h| h.value.to_lowercase().contains(&gold_lower));
        if hit {
            m.fact_recall.0 += 1;
        }
        rows.push((
            "fact".into(),
            format!("{} → {}", f.key, f.value),
            causal_memory::token::estimate_tokens(&ctx),
        ));
    }

    // ── Causal lessons: search by distinctive topic tokens. The planted
    //    decision sentence is too brittle (the LLM rephrases), so the query
    //    is 3 topic tokens and a hit requires ≥2 of them in an entry's
    //    decision+outcome text — tolerant of rephrasing, still meaningful.
    //    `decision_attachment` separately reports how often the distilled
    //    item KEPT its decision (a real decision → outcome edge). ─────────
    for c in &scenario.causals {
        m.causal_recall.1 += 1;
        let query_tokens: Vec<&str> = c.query.split_whitespace().collect();
        let hits = store.search_causal_bm25(None, c.query, topk).unwrap_or_default();
        let ctx = hits
            .iter()
            .map(|h| format!("{} → {}", h.decision_text, h.outcome_text))
            .collect::<Vec<_>>()
            .join("\n");
        token_sum += causal_memory::token::estimate_tokens(&ctx);
        token_n += 1;
        let matches = |text: &str| -> bool {
            let lower = text.to_lowercase();
            query_tokens
                .iter()
                .filter(|t| lower.contains(&t.to_lowercase()))
                .count()
                >= 2
        };
        // Decision attachment: a real decision chunk exists for this lesson.
        m.decision_attachment.1 += 1;
        if hits.iter().any(|h| matches(&h.decision_text)) {
            m.decision_attachment.0 += 1;
        }
        let hit = hits.iter().find(|h| matches(&format!("{} {}", h.decision_text, h.outcome_text)));
        if hit.is_some() {
            m.causal_recall.0 += 1;
        }
        // Relation accuracy: among hits whose outcome matches, the relation
        // must be the planted one.
        if let Some(h) = hit {
            m.relation_accuracy.1 += 1;
            if h.relation == c.relation {
                m.relation_accuracy.0 += 1;
            }
        }
        rows.push((
            "causal".into(),
            format!("{} → {} ({})", c.decision, c.outcome, c.relation),
            causal_memory::token::estimate_tokens(&ctx),
        ));
    }

    // ── Supersession: the old value must be retired, the new live; then
    //    re-record the old fact and check it is retrievable again (the fact
    //    layer's revive path — record_fact resurrects an invalidated fact).
    //    Retired facts are invisible to search_facts_bm25 (valid_to filter),
    //    so the audit view is queried directly. ────────────────────────────
    let (_, old_val, new_val) = scenario.supersession;
    let old_live: i64 = store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM agent_facts WHERE value LIKE ?1 AND valid_to IS NULL",
                rusqlite::params![format!("%{old_val}%")],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(0);
    let old_retired: i64 = store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM agent_facts WHERE value LIKE ?1 AND valid_to IS NOT NULL",
                rusqlite::params![format!("%{old_val}%")],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(0);
    let new_live: i64 = store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM agent_facts WHERE value LIKE ?1 AND valid_to IS NULL",
                rusqlite::params![format!("%{new_val}%")],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(0);
    // Diagnostic: if the new value exists anywhere (facts OR edges) but the
    // fact-layer retirement never fired, the LLM classified the upgrade as
    // an event (extraction gap) rather than the memory system failing.
    let new_anywhere: i64 = store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM chunks WHERE text LIKE ?1",
                rusqlite::params![format!("%{new_val}%")],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(0);
    m.supersession_detected = old_retired > 0 && new_live > 0 && old_live == 0;
    m.supersession_note = if m.supersession_detected {
        "ok".to_string()
    } else if new_anywhere > 0 {
        format!("extracted as event/edge, not fact (new value present in {new_anywhere} chunks)")
    } else {
        "new value not extracted at all".to_string()
    };
    if old_retired > 0 {
        // Revive: re-record the retired value through the idempotent path.
        let _ = store.record_fact("fact", old_val, "user", "distill", 0.8);
        let revived: i64 = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM agent_facts WHERE value LIKE ?1 AND valid_to IS NULL",
                    rusqlite::params![format!("%{old_val}%")],
                    |r| r.get(0),
                )?)
            })
            .unwrap_or(0);
        m.reversibility = (true, revived > 0);
    }

    // ── Reversible consolidation (store mechanics, seeded directly on an
    //    independent topic — the LLM path is the supersession metric above):
    //    a transition fact retires the old one, and re-recording revives it;
    //    an edge-level supersede marks superseded_by, and restore_edge
    //    brings the old lesson back. ─────────────────────────────────────
    let mech_old = store
        .record_fact("preference", "prefers black coffee in the morning", "user", "distill", 0.8)?;
    let mech_new = store
        .record_fact("preference", "switched from black coffee to green tea", "user", "distill", 0.8)?;
    let mech_retired = store
        .retire_facts_by_hint("preference", "user", "black coffee", Some(mech_new))
        .unwrap_or(0);
    let _ = mech_old;
    let _ = mech_retired;
    // Fact revive path: re-recording the retired value resurrects it.
    let _ = store.record_fact("preference", "prefers black coffee in the morning", "user", "distill", 0.8)?;
    let revived: i64 = store
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM agent_facts WHERE value LIKE ?1 AND valid_to IS NULL",
                rusqlite::params!["%black coffee%"],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(0);
    // Edge-level supersede + restore. The decision must predate the
    // superseding item (invalidate_superseded's event_time guard), so it is
    // recorded with an explicit past timestamp.
    let _ = store.record_decision_at(
        "debugged with print statements",
        "took a long time to find the bug",
        "caused",
        Some("mech"),
        0.8,
        "llm_inferred",
        1_750_000_000, // 2026-06-15 — before the superseding item's date
    )?;
    let mech_item = MemoryItem {
        kind: ItemKind::Preference,
        text: "switched to structured debugging with a debugger".to_string(),
        date: Some("2026-08-01".to_string()),
        supersedes: Some("debugged with print statements".to_string()),
        causal_relation: None,
        decision: None,
    };
    let _ = store.record_distilled(&mech_item, None)?;
    let superseded_edges = store.superseded_edges(10)?;
    let mut restored_ok = false;
    if let Some(edge) = superseded_edges.iter().find(|e| e.decision_text.contains("print statements")) {
        restored_ok = store.restore_edge(edge.edge_id).unwrap_or(false)
            && store
                .search_causal_bm25(None, "print statements", 5)
                .unwrap_or_default()
                .iter()
                .any(|e| e.outcome_text.contains("took a long time"));
    }
    m.reversibility = (!superseded_edges.is_empty(), revived > 0 && restored_ok);

    // ── Multi-hop chain (seeded): trace_cause_chain walks back from the
    //    last outcome and must surface ≥2 of the planted decisions. ───────
    for (i, (d, o)) in scenario.chain.iter().enumerate() {
        let _ = link(store, d, o, "caused", 0.9)?;
        let _ = i;
    }
    let last_outcome = scenario.chain.last().map(|(_, o)| *o).unwrap_or_default();
    let chains = store.trace_cause_chain(last_outcome, 5, 0.3).unwrap_or_default();
    let gold_decisions: Vec<String> = scenario
        .chain
        .iter()
        .map(|(d, _)| d.to_lowercase())
        .collect();
    let mut best = 0usize;
    for chain in &chains {
        let covered = chain
            .iter()
            .filter(|h| gold_decisions.iter().any(|g| h.decision_text.to_lowercase().contains(g)))
            .count();
        best = best.max(covered);
    }
    m.chain_recall = (best.min(2), 2); // ≥2 of the 3 planted decisions covered

    // ── Forward simulation: seed spreading activation with each topic
    //    token separately (spreading_activation matches SUBSTRING seeds, so
    //    multi-word phrases never hit) and check whether the actual outcome
    //    surfaces in the top-k of any token's spread. Prevented lessons
    //    must additionally produce a WARNING (negative activation). ───────
    let graph = CausalGraph::from_store(store)?;
    let mut sim_tested = 0usize;
    let mut sim_hits = 0usize;
    for c in &scenario.causals {
        let outcome_lower = c.outcome.to_lowercase();
        let mut hit = false;
        let mut warned = false;
        for token in c.query.split_whitespace() {
            let mut g = graph.clone();
            let results = g.spreading_activation(token, None, false);
            hit |= results
                .iter()
                .take(topk)
                .any(|r| r.text.to_lowercase().contains(&outcome_lower));
            warned |= results.iter().any(|r| r.activation < 0.0);
        }
        sim_tested += 1;
        if hit {
            sim_hits += 1;
        }
        if c.relation == "prevented" {
            m.warning_detected |= warned;
        }
    }
    m.simulation_hits = (sim_hits, sim_tested);

    m.avg_ctx_tokens = if token_n == 0 {
        0.0
    } else {
        token_sum as f64 / token_n as f64
    };

    Ok((m, rows))
}

// ─── Report ────────────────────────────────────────────────────────────────

fn render_report(
    m: &Metrics,
    ingest: &IngestStats,
    rows: &[(String, String, usize)],
    model: &str,
    temperature: f32,
    seed: u64,
    topk: usize,
) -> String {
    let pct = |a: usize, b: usize| -> String {
        if b == 0 {
            "n/a".to_string()
        } else {
            format!("{:.0}% ({a}/{b})", a as f64 / b as f64 * 100.0)
        }
    };
    let mut out = format!(
        "# bench-memory results\n\n- model: {model}\n- temperature: {temperature}\n- seed: {seed}\n- topk: {topk}\n- protocol: LLM-distilled write path, deterministic retrieval scoring (no judge LLM)
- note: the scenario is reproducible; LLM extraction is NOT (model/version dependent — expect run-to-run variance in the extraction metrics)\n\n"
    );

    out.push_str("## LLM extraction quality (write path)\n\n");
    out.push_str("| metric | score |\n|---|---|\n");
    out.push_str(&format!("| fact/preference recall@{topk} | {} |\n", pct(m.fact_recall.0, m.fact_recall.1)));
    out.push_str(&format!("| causal recall@{topk} | {} |\n", pct(m.causal_recall.0, m.causal_recall.1)));
    out.push_str(&format!("| relation accuracy | {} |\n", pct(m.relation_accuracy.0, m.relation_accuracy.1)));
    out.push_str(&format!("| decision attachment | {} |\n", pct(m.decision_attachment.0, m.decision_attachment.1)));
    out.push_str(&format!(
        "| supersession detected | {} — {} |\n",
        m.supersession_detected, m.supersession_note
    ));
    out.push_str(&format!(
        "| extracted items | {} ({} facts, {} edges) · {} distill failures |\n",
        ingest.items_extracted, ingest.facts_written, ingest.edges_written, ingest.distill_failures
    ));

    out.push_str("\n## Read-path mechanics (deterministic)\n\n");
    out.push_str("| metric | score |\n|---|---|\n");
    out.push_str(&format!("| multi-hop chain recall | {} |\n", pct(m.chain_recall.0, m.chain_recall.1)));
    out.push_str(&format!("| forward-simulation recall@{topk} | {} |\n", pct(m.simulation_hits.0, m.simulation_hits.1)));
    out.push_str(&format!("| trap warning detected | {} |\n", m.warning_detected));
    out.push_str(&format!(
        "| reversibility (restore) | superseded={} restored={} |\n",
        m.reversibility.0, m.reversibility.1
    ));

    out.push_str("\n## Efficiency\n\n");
    out.push_str("| metric | value |\n|---|---|\n");
    out.push_str(&format!("| avg context tokens/query | {:.0} |\n", m.avg_ctx_tokens));

    out.push_str("\n## Extraction fidelity (what the LLM wrote)\n\n");
    out.push_str("| kind | text |\n|---|---|\n");
    for (kind, text) in &ingest.extracted {
        let snippet: String = text.chars().take(120).collect();
        out.push_str(&format!("| {kind} | {snippet} |\n"));
    }

    out.push_str("\n## Per-item retrieval tokens\n\n");
    out.push_str("| category | item | est. tokens |\n|---|---|---|\n");
    for (cat, label, tokens) in rows {
        out.push_str(&format!("| {cat} | {label} | {tokens} |\n"));
    }
    out
}

// ─── Entry point ───────────────────────────────────────────────────────────

async fn run(args: &[String]) -> Result<()> {
    let mut seed = 42u64;
    let mut topk = 5usize;
    let mut out_dir = PathBuf::from("benches/memory/results");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                i += 1;
                seed = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--seed needs a value"))?
                    .parse()?;
            }
            "--topk" => {
                i += 1;
                topk = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--topk needs a value"))?
                    .parse()?;
            }
            "--out" => {
                i += 1;
                out_dir = PathBuf::from(
                    args.get(i).ok_or_else(|| anyhow!("--out needs a value"))?,
                );
            }
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }

    let distiller = match Distiller::from_env() {
        Some(d) => d,
        None => {
            eprintln!("bench-memory requires an LLM (the write path is the real distiller).");
            eprintln!("Set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY (or DEEPSEEK_API_KEY) and retry.");
            std::process::exit(1);
        }
    };

    let scenario = generate_scenario(seed);
    let store = CausalStore::open_in_memory()?;
    let temperature = 0.0;
    let model = distiller.model().to_string();

    println!("=== bench-memory ===");
    println!(
        "LLM: {model} · seed={seed} · topk={topk} · {} sessions · {} facts · {} causal lessons",
        scenario.sessions.len(),
        scenario.facts.len(),
        scenario.causals.len()
    );

    // 1. Write path: real LLM distill.
    let ingest_stats = ingest(&store, &distiller, &scenario).await?;
    println!(
        "ingest: {} items extracted ({} facts, {} edges) · {} supersedes fired · {} distill failures",
        ingest_stats.items_extracted,
        ingest_stats.facts_written,
        ingest_stats.edges_written,
        ingest_stats.retired,
        ingest_stats.distill_failures
    );

    // 2. Read path: deterministic evaluation.
    let (metrics, rows) = evaluate(&store, &scenario, topk)?;
    let report = render_report(
        &metrics,
        &ingest_stats,
        &rows,
        &model,
        temperature,
        seed,
        topk,
    );
    println!("{report}");

    // 3. Persist report + summary JSON.
    std::fs::create_dir_all(&out_dir)?;
    let ts = chrono::Utc::now().timestamp();
    let md_path = out_dir.join(format!("bench-memory-{ts}.md"));
    std::fs::write(&md_path, &report)?;
    println!("report written to {}", md_path.display());

    let summary = serde_json::json!({
        "seed": seed,
        "topk": topk,
        "model": model,
        "temperature": temperature,
        "ingest": {
            "items_extracted": ingest_stats.items_extracted,
            "facts_written": ingest_stats.facts_written,
            "edges_written": ingest_stats.edges_written,
            "supersedes_fired": ingest_stats.retired,
            "distill_failures": ingest_stats.distill_failures,
        },
        "metrics": {
            "fact_recall": { "hits": metrics.fact_recall.0, "total": metrics.fact_recall.1 },
            "causal_recall": { "hits": metrics.causal_recall.0, "total": metrics.causal_recall.1 },
            "relation_accuracy": { "hits": metrics.relation_accuracy.0, "total": metrics.relation_accuracy.1 },
            "decision_attachment": { "hits": metrics.decision_attachment.0, "total": metrics.decision_attachment.1 },
            "supersession_detected": metrics.supersession_detected,
            "supersession_note": metrics.supersession_note,
            "chain_recall": { "hits": metrics.chain_recall.0, "total": metrics.chain_recall.1 },
            "simulation_recall": { "hits": metrics.simulation_hits.0, "total": metrics.simulation_hits.1 },
            "warning_detected": metrics.warning_detected,
            "reversibility": { "superseded": metrics.reversibility.0, "restored": metrics.reversibility.1 },
            "avg_ctx_tokens": metrics.avg_ctx_tokens,
        },
    });
    let json_path = out_dir.join(format!("bench-memory-{ts}.json"));
    std::fs::write(&json_path, serde_json::to_string_pretty(&summary)?)?;
    println!("summary written to {}", json_path.display());
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(&args))
}
