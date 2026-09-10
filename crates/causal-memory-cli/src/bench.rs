//! `bench-compaction`: reproducible harness for the compaction-degradation
//! experiment (papers/02, issue #14) — causal facts degrade fastest when the
//! session text is repeatedly compacted by an LLM; the causal table survives.
//!
//! Protocol (Core-Memory style anti-cheat):
//! - Gold answers / keyword lists NEVER enter the compaction context: the
//!   session text contains only chatter and decision events, never the QA
//!   pairs they are scored against.
//! - Each compression depth k uses an INDEPENDENT freshly generated session
//!   (seed + k), so measurements at different depths never share a session.
//! - The scenario is seeded (`--seed`) → fully reproducible. LLM compression
//!   and answers are NOT reproducible — the model and temperature are
//!   recorded in the report header for honesty.
//!
//! Scoring is deterministic: an answer passes iff it contains ALL gold
//! keywords (case-insensitive). No LLM judge — avoids a circular dependency
//! on the thing being measured.

use anyhow::Result;
use causal_memory::llm::{chat, LlmConfig};
use causal_memory::store::CausalStore;

/// The compaction instruction, kept as a shareable file so the experiment is
/// reproducible byte-for-byte.
const COMPACTION_PROMPT: &str = include_str!("../../../benches/compaction_prompt.txt");

const ANSWER_PROMPT: &str = "You are answering questions about an agent session log. Answer concisely and factually, using ONLY the provided log. If the log does not contain the answer, say \"not in log\".";

/// One decision/outcome event embedded in the generated session, plus the
/// gold QA pair derived from it (kept OUT of the session text).
#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioDecision {
    pub decision: String,
    pub outcome: String,
    pub task_tag: String,
    /// Short distinctive substring used to probe the causal table.
    pub probe: String,
    pub causal_q: String,
    pub causal_keywords: Vec<String>,
    pub factual_q: String,
    pub factual_keywords: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub session_text: String,
    pub decisions: Vec<ScenarioDecision>,
}

/// One row of the results table.
#[derive(Debug, Clone)]
pub struct BenchRow {
    pub k: usize,
    pub text_recall: f64,
    pub causal_q_recall: f64,
    pub factual_q_recall: f64,
    pub table_recall: f64,
}

/// Decision templates: each carries a version-like fact (non-causal gold
/// answer) and a failure-reason outcome (causal gold answer). Both facts live
/// in the session text; the QA pairs do not.
/// Fields: (decision, outcome, tag, probe, causal_q, causal_kw, factual_q, factual_kw)
type DecisionTemplate = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

const DECISION_TEMPLATES: &[DecisionTemplate] = &[
    // (decision, outcome, tag, probe, causal_q, causal_kw, factual_q, factual_kw)
    (
        "chose Redis 7.2.4 for the session cache",
        "cache stampede during deploy; fixed by adding jittered TTL",
        "caching",
        "Redis 7.2.4",
        "Why did the session cache fail during deploy?",
        "stampede",
        "Which Redis version was used for the session cache?",
        "7.2.4",
    ),
    (
        "used a global mutex in tokio 1.38 for shared state",
        "deadlock under concurrent load; fixed by switching to channel ownership",
        "concurrency",
        "tokio 1.38",
        "Why did the shared-state design deadlock?",
        "mutex",
        "Which tokio version was the deadlock observed on?",
        "1.38",
    ),
    (
        "enabled gzip level 9 for kafka 3.7.0 producer batches",
        "CPU saturated at 95% and throughput collapsed; fixed by dropping to level 5",
        "streaming",
        "kafka 3.7.0",
        "Why did producer throughput collapse?",
        "gzip",
        "Which kafka version was the producer running?",
        "3.7.0",
    ),
    (
        "set HDFS block size to 16MB on hadoop 3.3.6",
        "NameNode heap exhausted by block-map growth; fixed by raising to 128MB",
        "storage",
        "hadoop 3.3.6",
        "Why was the NameNode heap exhausted?",
        "block",
        "Which hadoop version hit the NameNode issue?",
        "3.3.6",
    ),
    (
        "ran migrations with flyway 9.22 without a backup",
        "data loss during a failed rollback; fixed by restoring the nightly snapshot",
        "database",
        "flyway 9.22",
        "Why was there data loss during the migration?",
        "backup",
        "Which flyway version ran the migration?",
        "9.22",
    ),
    (
        "pinned the build to rust 1.79.0 with lto=fat",
        "CI link time exploded past 20 minutes; fixed by switching to lto=thin",
        "build",
        "rust 1.79.0",
        "Why did CI link time explode?",
        "lto",
        "Which rust version was pinned for the build?",
        "1.79.0",
    ),
];

const CHATTER: &[&str] = &[
    "agent: scanning the issue tracker for related tickets",
    "agent: reading the deployment runbook section on rollbacks",
    "user: can you also check the dashboards afterward",
    "agent: summarizing the current incident timeline",
    "user: please keep the change minimal",
    "agent: comparing a few candidate approaches first",
    "agent: noting the relevant config file paths for later",
    "user: what does the on-call doc say about this",
];

/// Tiny deterministic RNG (SplitMix64) — no external dep, stable forever.
pub(crate) struct SplitMix64(pub(crate) u64);

impl SplitMix64 {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Deterministic scenario: `rounds` rounds of (chatter lines + one decision
/// event). Same seed → identical scenario; the keyword lists are fixed by
/// the templates selected through the seeded RNG.
pub fn generate_scenario(seed: u64, rounds: usize) -> Scenario {
    let mut rng = SplitMix64(seed);
    let mut text = String::from("session log — synthetic bench scenario\n");
    let mut decisions = Vec::new();
    // Pick templates without replacement (rounds <= template count).
    let mut order: Vec<usize> = (0..DECISION_TEMPLATES.len()).collect();
    for i in (1..order.len()).rev() {
        let j = rng.below(i + 1);
        order.swap(i, j);
    }
    for r in 0..rounds {
        for _ in 0..2 + rng.below(2) {
            let line = CHATTER[rng.below(CHATTER.len())];
            text.push_str(&format!("{line}\n"));
        }
        let (dec, out, tag, probe, cq, ckw, fq, fkw) = DECISION_TEMPLATES[order[r % order.len()]];
        text.push_str(&format!("agent: decision — {dec}. outcome — {out}.\n"));
        decisions.push(ScenarioDecision {
            decision: dec.to_string(),
            outcome: out.to_string(),
            task_tag: tag.to_string(),
            probe: probe.to_string(),
            causal_q: cq.to_string(),
            causal_keywords: vec![ckw.to_string()],
            factual_q: fq.to_string(),
            factual_keywords: vec![fkw.to_string()],
        });
    }
    Scenario {
        session_text: text,
        decisions,
    }
}

/// Deterministic scoring: an answer passes iff it contains ALL gold keywords
/// (case-insensitive substring match).
pub fn score_answer(answer: &str, keywords: &[String]) -> bool {
    let lower = answer.to_lowercase();
    keywords.iter().all(|k| lower.contains(&k.to_lowercase()))
}

/// Render the results as a markdown report replicating the paper's table.
pub fn render_report(
    rows: &[BenchRow],
    model: &str,
    temperature: f32,
    seed: u64,
    timestamp: i64,
) -> String {
    let mut out = format!(
        "# bench-compaction results\n\n- model: {model}\n- temperature: {temperature}\n- seed: {seed}\n- timestamp: {timestamp}\n- protocol: each k uses an independent seeded session; gold keywords never enter the compaction context\n- note: the scenario is reproducible; LLM compression/answers are NOT (model/version dependent)\n\n| compactions k | text recall | causal-Q recall | factual-Q recall | causal-table recall | gap (table − text) |\n|---|---|---|---|---|---|\n"
    );
    for r in rows {
        out.push_str(&format!(
            "| {} | {:.0}% | {:.0}% | {:.0}% | {:.0}% | {:+.0}% |\n",
            r.k,
            r.text_recall * 100.0,
            r.causal_q_recall * 100.0,
            r.factual_q_recall * 100.0,
            r.table_recall * 100.0,
            (r.table_recall - r.text_recall) * 100.0,
        ));
    }
    out
}

/// `causal-memory bench-compaction [--rounds N] [--compressions K] [--seed S]`
///
/// Requires CAUSAL_MEMORY_LLM_* — this bench inherently needs an LLM (it is
/// the thing being measured); unlike the zero-intrusion runtime paths, it
/// refuses to run unconfigured instead of silently degrading.
pub async fn run(args: &[String]) -> Result<()> {
    let mut rounds = 6usize;
    let mut compressions = 5usize;
    let mut seed = 42u64;
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> Result<&String> {
            *i += 1;
            args.get(*i)
                .ok_or_else(|| anyhow::anyhow!("missing value for {}", args[*i - 1]))
        };
        match args[i].as_str() {
            "--rounds" => rounds = take(&mut i)?.parse()?,
            "--compressions" => compressions = take(&mut i)?.parse()?,
            "--seed" => seed = take(&mut i)?.parse()?,
            other => anyhow::bail!(
                "unknown flag: {other}\nUsage: causal-memory bench-compaction [--rounds N] [--compressions K] [--seed S]"
            ),
        }
        i += 1;
    }
    let rounds = rounds.min(DECISION_TEMPLATES.len());

    let config = match LlmConfig::from_env() {
        Some(c) => c,
        None => {
            eprintln!("bench-compaction requires an LLM (it measures LLM compaction).");
            eprintln!("Set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY and retry.");
            std::process::exit(1);
        }
    };
    println!("LLM: {} @ {}", config.model, config.api_base);
    println!("seed={seed} rounds={rounds} compressions={compressions}\n");

    let mut rows = Vec::new();
    for k in 1..=compressions {
        // Independent session per depth k (anti-cheat: no shared session).
        let scenario = generate_scenario(seed + k as u64, rounds);

        // (b) the same decision events go into the causal table.
        let store = CausalStore::open_in_memory()?;
        for d in &scenario.decisions {
            store.record_decision(
                &d.decision,
                &d.outcome,
                "caused",
                Some(&d.task_tag),
                0.8,
                "rule",
            )?;
        }

        // Compress the session text k times.
        let mut text = scenario.session_text.clone();
        for round in 1..=k {
            text = chat(&config, COMPACTION_PROMPT, &text, 2000, 0.3).await?;
            println!(
                "  [k={k}] compaction {round}/{k} done ({} chars)",
                text.len()
            );
        }

        // Measure text recall via gold QA (answers scored by keywords only).
        let mut causal_pass = 0usize;
        let mut factual_pass = 0usize;
        for d in &scenario.decisions {
            for (q, kws, causal) in [
                (&d.causal_q, &d.causal_keywords, true),
                (&d.factual_q, &d.factual_keywords, false),
            ] {
                let user = format!("Session log:\n{text}\n\nQuestion: {q}");
                let ans = chat(&config, ANSWER_PROMPT, &user, 200, 0.0).await?;
                if score_answer(&ans, kws) {
                    if causal {
                        causal_pass += 1;
                    } else {
                        factual_pass += 1;
                    }
                }
            }
        }
        let n = scenario.decisions.len() as f64;
        let (c_recall, f_recall) = (causal_pass as f64 / n, factual_pass as f64 / n);
        let text_recall = (causal_pass + factual_pass) as f64 / (2.0 * n);

        // Measure causal-table recall (should hold at 100%: the table is
        // never compacted).
        let mut table_hit = 0usize;
        for d in &scenario.decisions {
            if !store
                .search_causal(None, Some(&d.probe))
                .unwrap_or_default()
                .is_empty()
            {
                table_hit += 1;
            }
        }
        let table_recall = table_hit as f64 / n;

        println!(
            "  [k={k}] text={:.0}% causal-Q={:.0}% factual-Q={:.0}% table={:.0}%",
            text_recall * 100.0,
            c_recall * 100.0,
            f_recall * 100.0,
            table_recall * 100.0
        );
        rows.push(BenchRow {
            k,
            text_recall,
            causal_q_recall: c_recall,
            factual_q_recall: f_recall,
            table_recall,
        });
    }

    let ts = chrono::Utc::now().timestamp();
    let report = render_report(&rows, &config.model, 0.3, seed, ts);
    println!("\n{report}");
    let file = format!("bench-results-{ts}.md");
    std::fs::write(&file, &report)?;
    println!("Report written to {file}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generator_deterministic() {
        let a = generate_scenario(42, 6);
        let b = generate_scenario(42, 6);
        assert_eq!(a, b, "same seed must produce the identical scenario");
        let c = generate_scenario(43, 6);
        assert_ne!(
            a.session_text, c.session_text,
            "different seed → different scenario"
        );
        // Keyword lists are fixed by the seed too.
        assert_eq!(
            a.decisions[0].causal_keywords,
            b.decisions[0].causal_keywords
        );
        // Anti-cheat: gold questions/keywords never appear in the session text.
        for d in &a.decisions {
            assert!(!a.session_text.contains(&d.causal_q));
            assert!(!a.session_text.contains(&d.factual_q));
        }
        // But the facts being asked about DO appear (otherwise trivially 0%).
        assert!(
            a.session_text.contains("7.2.4")
                || a.decisions
                    .iter()
                    .all(|d| !d.factual_keywords.contains(&"7.2.4".to_string()))
        );
    }

    #[test]
    fn test_score_answer() {
        let kws = vec!["Stampede".to_string()];
        assert!(
            score_answer("the cache stampede hit at deploy", &kws),
            "case-insensitive"
        );
        assert!(!score_answer("all went well", &kws));
        let multi = vec!["redis".to_string(), "7.2.4".to_string()];
        assert!(
            score_answer("Redis 7.2.4 was used", &multi),
            "ALL keywords required"
        );
        assert!(!score_answer("Redis was used", &multi));
        assert!(score_answer("anything", &[]), "no keywords → vacuous pass");
    }

    #[test]
    fn test_render_report() {
        let rows = vec![
            BenchRow {
                k: 1,
                text_recall: 0.9,
                causal_q_recall: 0.8,
                factual_q_recall: 1.0,
                table_recall: 1.0,
            },
            BenchRow {
                k: 5,
                text_recall: 0.4,
                causal_q_recall: 0.2,
                factual_q_recall: 0.6,
                table_recall: 1.0,
            },
        ];
        let md = render_report(&rows, "deepseek-chat", 0.3, 42, 1_700_000_000);
        assert!(md.contains("model: deepseek-chat"));
        assert!(md.contains("seed: 42"));
        assert!(md.contains("| 1 | 90% | 80% | 100% | 100% | +10% |"));
        assert!(md.contains("| 5 | 40% | 20% | 60% | 100% | +60% |"));
        assert!(
            md.contains("NOT"),
            "honesty note about LLM non-reproducibility"
        );
    }
}
