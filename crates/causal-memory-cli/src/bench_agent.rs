//! `bench-agent`: end-to-end ablation — the same LLM agent solving a sequence
//! of tasks with (B) vs without (A) causal memory attached.
//!
//! "Trap world" design: every task belongs to a trap family whose correct
//! solution is NOT stated in the task text. First exposure is expected to
//! fail (the failure observation carries the hint); the question the bench
//! answers is whether the agent steps into the SAME trap again on the 2nd+
//! exposure. A memory-less agent must rediscover every fix; an agent with
//! persistent cross-task memory can recall it.
//!
//! Anti-cheat: task texts never mention the trap or the solution; the
//! scenario is seeded (`--seed`) → reproducible. LLM behavior is NOT
//! reproducible — model and temperature are recorded in the report header.
//!
//! Scoring is fully deterministic (string-matching verdicts + counters); no
//! LLM judge in the loop.

use anyhow::Result;
use causal_memory::llm::{chat, LlmConfig};
use causal_memory::store::CausalStore;

use crate::bench::SplitMix64;

// ─── World model (deterministic, no LLM) ──────────────────────────────────

/// A trap family: a class of task whose naive solution fails in a specific,
/// discoverable way.
#[derive(Debug, Clone, PartialEq)]
pub struct TrapFamily {
    pub id: &'static str,
    /// Task description shown to the agent — deliberately contains NO hint
    /// about the trap or its solution.
    pub task: &'static str,
    /// Observation returned when the agent steps into the trap. Carries the
    /// hint needed to discover the fix (first-exposure failure is expected).
    pub fail_output: &'static str,
    /// Observation returned on the correct solution.
    pub ok_output: &'static str,
}

pub const FAMILIES: &[TrapFamily] = &[
    TrapFamily {
        id: "proxy-github",
        task: "把当前 git 仓库与远程同步：获取 origin/main 的最新提交到本地。",
        // The hint must be directly actionable — the agent cannot guess a
        // proxy host:port out of a bare timeout.
        fail_output: "fatal: unable to access 'https://github.com/org/repo.git/': Failed to connect to github.com port 443 after 75000 ms: Connection timed out\nhint: this network requires a proxy for github.com — retry with: git -c http.proxy=http://127.0.0.1:1097 fetch origin",
        ok_output: "From github.com:org/repo\n * branch main → FETCH_HEAD (3 new commits)",
    },
    TrapFamily {
        id: "cargo-env",
        task: "构建本项目的 release 二进制。",
        fail_output: "bash: cargo: command not found\nhint: the toolchain lives in the project-local .cargo directory — retry with: export PATH=\".cargo/bin:$PATH\" && cargo build --release",
        ok_output: "   Compiling causal-memory v0.9.0\n    Finished `release` profile in 38.2s",
    },
    TrapFamily {
        id: "bearer-token",
        task: "查询 https://api.internal.local/v1/status 的服务状态并汇报。",
        fail_output: "HTTP/1.1 401 Unauthorized\n{\"error\": \"missing bearer token\", \"hint\": \"retry with header: Authorization: Bearer dev-token-123\"}",
        ok_output: "HTTP/1.1 200 OK\n{\"status\": \"healthy\", \"uptime_s\": 40231}",
    },
    TrapFamily {
        id: "nextest",
        task: "运行本项目的完整测试套件并确认全部通过。",
        fail_output: "error: test failed: flaky_test_7 panicked\nhint: this suite is only stable under the nextest runner — retry with: cargo nextest run",
        ok_output: "        PASS [   4.212s] 118 tests run: 118 passed, 0 failed",
    },
    TrapFamily {
        id: "db-path",
        task: "用 causal-memory CLI 把本地因果记忆导出为 lessons.jsonl。",
        fail_output: "Error: unable to open database file at /usr/share/causal-memory/causal.db (readonly file system)\nhint: point CAUSAL_MEMORY_DB at a writable path — retry with: CAUSAL_MEMORY_DB=/tmp/causal.db causal-memory export lessons.jsonl",
        ok_output: "=== Export complete ===\n  edges: 12  meta edges: 3",
    },
    TrapFamily {
        id: "json-content-type",
        task: "向 https://api.internal.local/v1/jobs 提交一个 job（name=demo）。",
        fail_output: "HTTP/1.1 415 Unsupported Media Type\n{\"error\": \"expected application/json\", \"hint\": \"retry with header: Content-Type: application/json\"}",
        ok_output: "HTTP/1.1 201 Created\n{\"job_id\": \"j-8842\", \"name\": \"demo\"}",
    },
];

/// Verdict of one `run_command` against the task's trap family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdVerdict {
    /// The correct solution — task complete.
    Solution,
    /// Stepped into the trap.
    Trap,
    /// Neither — executed but did not complete the task.
    Neutral,
}

/// Deterministic trap rules. The solution check runs first (a correct fix
/// never counts as stepping into the trap). Matching is case-insensitive and
/// tolerant of reasonable command variants (quoting, env-prefix style,
/// absolute paths).
pub fn classify(family_id: &str, cmd: &str) -> CmdVerdict {
    let cmd = cmd.to_lowercase();
    let has = |needle: &str| cmd.contains(needle);
    let any = |needles: &[&str]| needles.iter().any(|n| cmd.contains(n));
    match family_id {
        "proxy-github" => {
            // Any git command that routes through the known proxy:port.
            if has("127.0.0.1:1097") {
                CmdVerdict::Solution
            } else if has("git") && any(&["fetch", "pull", "push", "clone"]) {
                CmdVerdict::Trap
            } else {
                CmdVerdict::Neutral
            }
        }
        "cargo-env" => {
            // The fix is getting the project-local .cargo toolchain onto PATH
            // (source .cargo/env, export PATH=.cargo/bin:…, or invoking
            // .cargo/bin/cargo directly).
            if has(".cargo") {
                CmdVerdict::Solution
            } else if has("cargo") {
                CmdVerdict::Trap
            } else {
                CmdVerdict::Neutral
            }
        }
        "bearer-token" => {
            if has("api.internal.local/v1/status") && has("bearer") {
                CmdVerdict::Solution
            } else if has("api.internal.local/v1/status") {
                CmdVerdict::Trap
            } else {
                CmdVerdict::Neutral
            }
        }
        "nextest" => {
            if has("nextest") {
                CmdVerdict::Solution
            } else if has("cargo test") {
                CmdVerdict::Trap
            } else {
                CmdVerdict::Neutral
            }
        }
        "db-path" => {
            if has("causal-memory export") && has("causal_memory_db=") {
                CmdVerdict::Solution
            } else if has("causal-memory export") {
                CmdVerdict::Trap
            } else {
                CmdVerdict::Neutral
            }
        }
        "json-content-type" => {
            if has("api.internal.local/v1/jobs") && has("application/json") {
                CmdVerdict::Solution
            } else if has("api.internal.local/v1/jobs") {
                CmdVerdict::Trap
            } else {
                CmdVerdict::Neutral
            }
        }
        _ => CmdVerdict::Neutral,
    }
}

/// One task instance: an occurrence of a trap family in the sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub family: &'static TrapFamily,
    /// Position in the task sequence (0-based).
    pub index: usize,
}

/// Seeded scenario: `n_families` families drawn from the built-in templates,
/// `k` tasks in round-robin order — which guarantees the required spacing
/// (no two adjacent tasks share a family when n_families ≥ 2) and gives each
/// family ⌈k/F⌉/⌊k/F⌋ exposures (2-3 at the default k=8, F=3).
pub fn generate_tasks(seed: u64, k: usize, n_families: usize) -> Vec<Task> {
    let mut rng = SplitMix64(seed);
    let mut order: Vec<usize> = (0..FAMILIES.len()).collect();
    for i in (1..order.len()).rev() {
        let j = rng.below(i + 1);
        order.swap(i, j);
    }
    let fams: Vec<&TrapFamily> = order
        .into_iter()
        .take(n_families.min(FAMILIES.len()))
        .map(|i| &FAMILIES[i])
        .collect();
    (0..k)
        .map(|i| Task {
            family: fams[i % fams.len()],
            index: i,
        })
        .collect()
}

// ─── Agent action protocol (text JSON, parsed deterministically) ──────────

#[derive(Debug, Clone, PartialEq)]
pub enum AgentAction {
    Run(String),
    Finish,
    Record { decision: String, outcome: String },
    Search(String),
    /// Forward simulation: "if I do X, what will happen?"
    /// Returns predicted outcomes from the causal graph, including
    /// prevented-edge warnings (negative activation).
    InterventionQuery(String),
}

/// Extract the first balanced `{…}` object (string- and escape-aware). The
/// agent sometimes emits two actions in one reply (e.g. record_memory +
/// finish after a fix) — spanning to the LAST `}` would produce invalid
/// JSON and silently drop the memory write; the first object is the action
/// to execute now, the rest is retried next turn.
fn first_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in s.char_indices().skip(start) {
        match c {
            '\\' if in_str => {
                escape = !escape;
                continue;
            }
            '"' if !escape => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
        escape = false;
    }
    None
}

/// Parse the agent's one-JSON-per-turn output. Tolerates markdown fences and
/// surrounding prose (executes the FIRST balanced JSON object); malformed
/// JSON and unknown actions are Err — the driver feeds the error back as an
/// observation and burns one step.
pub fn parse_action(raw: &str) -> Result<AgentAction, String> {
    let s = raw.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s).trim();
    let json = first_json_object(s).ok_or("no JSON object found")?;
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    match v["action"].as_str() {
        Some("run_command") => v["cmd"]
            .as_str()
            .map(|c| AgentAction::Run(c.to_string()))
            .ok_or_else(|| "run_command missing \"cmd\"".into()),
        Some("finish") | Some("finish_task") => Ok(AgentAction::Finish),
        Some("record_memory") => match (v["decision"].as_str(), v["outcome"].as_str()) {
            (Some(d), Some(o)) => Ok(AgentAction::Record {
                decision: d.to_string(),
                outcome: o.to_string(),
            }),
            _ => Err("record_memory missing \"decision\"/\"outcome\"".into()),
        },
        Some("search_memory") => v["query"]
            .as_str()
            .map(|q| AgentAction::Search(q.to_string()))
            .ok_or_else(|| "search_memory missing \"query\"".into()),
        Some("intervention_query") => v["query"]
            .as_str()
            .map(|q| AgentAction::InterventionQuery(q.to_string()))
            .ok_or_else(|| "intervention_query missing \"query\"".into()),
        Some(other) => Err(format!("unknown action: {other}")),
        None => Err("missing \"action\" field".into()),
    }
}

// ─── Metrics (deterministic counters) ─────────────────────────────────────

/// The core metric: for every family's first exposure vs 2nd+ exposures, did
/// the FIRST run_command of the task still step into the trap? `sequence` is
/// one entry per task: (family_id, verdict of the task's first run_command).
#[derive(Debug, Default, PartialEq)]
pub struct ExposureStats {
    pub first_exposures: usize,
    pub first_trapped: usize,
    pub repeat_exposures: usize,
    pub repeat_trapped: usize,
}

pub fn exposure_stats(sequence: &[(&str, Option<CmdVerdict>)]) -> ExposureStats {
    let mut stats = ExposureStats::default();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (family, verdict) in sequence {
        let trapped = *verdict == Some(CmdVerdict::Trap);
        if seen.insert(family) {
            stats.first_exposures += 1;
            if trapped {
                stats.first_trapped += 1;
            }
        } else {
            stats.repeat_exposures += 1;
            if trapped {
                stats.repeat_trapped += 1;
            }
        }
    }
    stats
}

/// Per-condition totals.
#[derive(Debug, Default)]
pub struct RunStats {
    pub tasks: usize,
    pub solved: usize,
    pub total_steps: usize,
    pub exposure: ExposureStats,
    pub mem_writes: usize,
    pub mem_searches: usize,
    /// After a search_memory, the immediately following run_command...
    pub post_search_runs: usize,
    /// ...is the correct solution (the search found the fix).
    pub post_search_hits: usize,
    /// Steps burned on unparseable agent output.
    pub parse_errors: usize,
    /// LLM call failures (after retries) that burned a step.
    pub llm_errors: usize,
    /// Number of intervention_query calls (condition C only).
    pub intervention_queries: usize,
    /// Times intervention_query correctly predicted a trap (returned DANGER/WARNING
    /// and the agent avoided it).
    pub predictions_avoided_trap: usize,
    /// Times intervention_query returned a prediction that matched the actual outcome.
    pub predictions_correct: usize,
    /// Total predictions made (for accuracy rate).
    pub predictions_total: usize,
}

fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// Render the A-vs-B markdown report (bench-compaction style).
pub fn render_report(
    a: Option<&RunStats>,
    b: Option<&RunStats>,
    model: &str,
    temperature: f32,
    seed: u64,
    timestamp: i64,
) -> String {
    let mut out = format!(
        "# bench-agent results (trap-world ablation)\n\n- model: {model}\n- temperature: {temperature}\n- seed: {seed}\n- timestamp: {timestamp}\n- protocol: task texts never mention traps/solutions; first-exposure failure is expected; B's memory persists across all tasks\n- note: the scenario is reproducible; LLM behavior is NOT (model/version dependent)\n\n"
    );
    out.push_str(
        "| group | tasks solved | avg steps | first-exposure trap rate | repeat-mistake rate |\n",
    );
    out.push_str("|---|---|---|---|---|\n");
    if let Some(a) = a {
        out.push_str(&group_row("A (no memory)", a));
        out.push_str(&failure_mode_line("A", a));
    }
    if let Some(b) = b {
        out.push_str(&group_row("B (causal memory)", b));
        out.push_str(&failure_mode_line("B", b));
        out.push_str(&format!(
            "\nB extras: memory writes {} · searches {} · post-search first-action hit rate {:.0}% ({}/{})\n",
            b.mem_writes,
            b.mem_searches,
            pct(b.post_search_hits, b.post_search_runs),
            b.post_search_hits,
            b.post_search_runs,
        ));
    }
    out
}

/// One table row for a condition group.
fn group_row(name: &str, s: &RunStats) -> String {
    format!(
        "| {} | {}/{} | {:.1} | {:.0}% ({}/{}) | {:.0}% ({}/{}) |\n",
        name,
        s.solved,
        s.tasks,
        s.total_steps as f64 / s.tasks.max(1) as f64,
        pct(s.exposure.first_trapped, s.exposure.first_exposures),
        s.exposure.first_trapped,
        s.exposure.first_exposures,
        pct(s.exposure.repeat_trapped, s.exposure.repeat_exposures),
        s.exposure.repeat_trapped,
        s.exposure.repeat_exposures,
    )
}

/// One-line failure-mode summary for a group: how many tasks went unsolved
/// (all unsolved tasks burned their full step budget by construction) and
/// how many steps were wasted on parse/LLM failures.
fn failure_mode_line(name: &str, s: &RunStats) -> String {
    let unsolved = s.tasks - s.solved;
    format!(
        "\n{name} failure modes: {unsolved} unsolved (step budget exhausted) · {parse_errors} invalid-action steps · {llm_errors} LLM call failures (after retries)\n",
        parse_errors = s.parse_errors,
        llm_errors = s.llm_errors,
    )
}

// ─── Agent driver (LLM in the loop; not unit-tested) ──────────────────────

const SYSTEM_A: &str = r#"You are an autonomous agent completing tasks in a simulated shell world. Each turn, output EXACTLY ONE JSON action on a single line, nothing else — no prose, no <think> text, no Observation lines (the world writes those):
{"action":"run_command","cmd":"<shell command to try>"}
{"action":"finish"}
The world replies with an Observation after each action. Failure observations contain hints — read them carefully and adapt your next command. Only finish when the task is actually complete."#;

const SYSTEM_B_MEMORY: &str = r#"
Additionally, you have a persistent causal memory shared across ALL tasks in this session:
{"action":"search_memory","query":"<keywords>"} — recall past lessons BEFORE a non-trivial command
{"action":"record_memory","decision":"<what you chose/tried>","outcome":"<what happened>"} — save a lesson AFTER discovering a fix (especially right after a failure)
Memory persists across tasks: a fix recorded once should never be rediscovered.
HARD RULES:
1. Your FIRST action of every task must be search_memory with the task's keywords. If the answer is "no memories found", immediately move on to run_command — do NOT search again for the same task.
2. When a run_command FAILS, search_memory once for a known fix before retrying (skip if you already searched this task).
3. record_memory is allowed ONLY after you have seen real results in run_command Observations — record the trap symptom and the exact fix that worked, including the task's keywords (e.g. proxy / github / nextest / CAUSAL_MEMORY_DB). NEVER record imagined or assumed results. At most ONE record_memory per task; further records are rejected by the world."#;

const SYSTEM_C_CAUSAL: &str = r#"
Additionally, you have a CAUSAL MEMORY system with forward simulation:

{"action":"intervention_query","query":"<action you're considering>"} — BEFORE you run a command, simulate what would happen. The system predicts outcomes based on past experience, including WARNING signals for actions that previously FAILED or were PREVENTED.
{"action":"search_memory","query":"<keywords>"} — recall past lessons
{"action":"record_memory","decision":"<what you tried>","outcome":"<what happened>"} — save a causal lesson after observing results

CRITICAL: Your FIRST action of every task must be intervention_query with the task's key action. The system will tell you if that action has previously caused failures (DANGER), was prevented (WARNING), or is safe (SAFE). Use this to AVOID known traps before stepping into them.

When intervention_query returns a DANGER or WARNING prediction, do NOT run that command — try the predicted safe alternative instead.

Memory persists across tasks. Record every trap you discover with its exact fix."#;

fn system_prompt(with_memory: bool) -> String {
    if with_memory {
        format!("{SYSTEM_A}\n{SYSTEM_B_MEMORY}")
    } else {
        SYSTEM_A.to_string()
    }
}

fn system_prompt_c() -> String {
    format!("{SYSTEM_A}\n{SYSTEM_C_CAUSAL}")
}

/// Run one condition (A: no memory, B: persistent causal memory) over the
/// task sequence. Every LLM turn costs one step; exceeding `max_steps` fails
/// the task.
async fn run_condition(
    config: &LlmConfig,
    tasks: &[Task],
    with_memory: bool,
    max_steps: usize,
    temperature: f32,
) -> Result<(RunStats, Vec<String>)> {
    let mut stats = RunStats {
        tasks: tasks.len(),
        ..Default::default()
    };
    let mut transcripts: Vec<String> = Vec::new();
    let mut sequence: Vec<(&str, Option<CmdVerdict>)> = Vec::new();
    // B's memory persists across every task in the run — the core mechanism.
    let store = if with_memory {
        Some(CausalStore::open_in_memory()?)
    } else {
        None
    };
    let system = system_prompt(with_memory);

    for task in tasks {
        let mut transcript = format!(
            "Task {} of {} [{}]: {}",
            task.index + 1,
            tasks.len(),
            task.family.id,
            task.family.task
        );
        let mut solved = false;
        let mut first_verdict: Option<CmdVerdict> = None;
        let mut awaiting_post_search = false;
        // Per-task record_memory guardrails (mirrors the HARD RULES): no
        // recording before any real command observation, one record per task.
        let mut ran_command = false;
        let mut recorded_this_task = false;
        // Per-task search cap (rule 1 allows one up-front search, rule 2 one
        // after a failure) — prevents search spirals that burn the budget.
        let mut searches_this_task = 0usize;

        for step in 1..=max_steps {
            stats.total_steps += 1;
            // One failed LLM call must not kill the whole run: retry twice,
            // then burn the step and continue.
            let mut reply = None;
            for attempt in 0..3 {
                match chat(config, &system, &transcript, 300, temperature).await {
                    Ok(r) => {
                        reply = Some(r);
                        break;
                    }
                    Err(e) => {
                        stats.llm_errors += 1;
                        if attempt < 2 {
                            eprintln!(
                                "  [task {} step {step}] LLM call failed (retry {}): {e}",
                                task.index + 1,
                                attempt + 1
                            );
                        }
                    }
                }
            }
            let Some(reply) = reply else {
                transcript.push_str("\n(harness) LLM call failed 3× — step wasted.");
                continue;
            };
            transcript.push_str(&format!("\nYou: {}", reply.trim()));
            let action = match parse_action(&reply) {
                Ok(a) => a,
                Err(e) => {
                    stats.parse_errors += 1;
                    transcript.push_str(&format!(
                        "\nObservation: invalid action ({e}). Output exactly one JSON action."
                    ));
                    continue;
                }
            };

            let observation = match action {
                AgentAction::Finish => {
                    if solved {
                        break;
                    }
                    "the task is NOT complete yet — finish rejected.".to_string()
                }
                AgentAction::Run(cmd) => {
                    ran_command = true;
                    let verdict = classify(task.family.id, &cmd);
                    if first_verdict.is_none() {
                        first_verdict = Some(verdict);
                    }
                    if awaiting_post_search {
                        stats.post_search_runs += 1;
                        if verdict == CmdVerdict::Solution {
                            stats.post_search_hits += 1;
                        }
                        awaiting_post_search = false;
                    }
                    match verdict {
                        CmdVerdict::Solution => {
                            solved = true;
                            task.family.ok_output.to_string()
                        }
                        CmdVerdict::Trap => task.family.fail_output.to_string(),
                        CmdVerdict::Neutral => {
                            "command executed, but the task is not complete.".to_string()
                        }
                    }
                }
                AgentAction::Record { decision, outcome } => match &store {
                    Some(_) if !ran_command => {
                        "no observations yet — run_command first; never record imagined results."
                            .to_string()
                    }
                    Some(_) if recorded_this_task => {
                        "already recorded for this task (max 1 per task).".to_string()
                    }
                    Some(store) => {
                        store.record_decision(
                            &decision,
                            &outcome,
                            "caused",
                            Some("agent-bench"),
                            0.6,
                            "llm_inferred",
                        )?;
                        stats.mem_writes += 1;
                        recorded_this_task = true;
                        format!(
                            "recorded: \"{}\" → \"{}\" (memory #{})",
                            decision, outcome, stats.mem_writes
                        )
                    }
                    None => "memory actions are not available in this condition.".to_string(),
                },
                AgentAction::Search(query) => match &store {
                    Some(_) if searches_this_task >= 2 => {
                        "search limit reached for this task (max 2) — proceed with run_command."
                            .to_string()
                    }
                    Some(store) => {
                        stats.mem_searches += 1;
                        searches_this_task += 1;
                        awaiting_post_search = true;
                        let hits = store
                            .search_causal_bm25(None, &query, 3)
                            .unwrap_or_default();
                        if hits.is_empty() {
                            "no memories found.".to_string()
                        } else {
                            let mut text = String::from("memories found:");
                            for h in &hits {
                                text.push_str(&format!(
                                    "\n- \"{}\" → \"{}\" (confidence {:.0}%)",
                                    h.decision_text,
                                    h.outcome_text,
                                    h.confidence * 100.0
                                ));
                            }
                            text
                        }
                    }
                    None => "memory actions are not available in this condition.".to_string(),
                },
                AgentAction::InterventionQuery(query) => match &store {
                    Some(store) => {
                        stats.intervention_queries += 1;
                        // Build a temporary hippocampus graph for forward simulation
                        let graph = causal_memory::hippocampus::CausalGraph::from_store(store);
                        if let Ok(mut g) = graph {
                            let results = g.spreading_activation_opts(&query, Some("agent-bench"), false, false);
                            stats.predictions_total += 1;
                            if results.is_empty() {
                                "intervention result: UNKNOWN — no past experience with this action. Proceed with caution.".to_string()
                            } else {
                                let mut text = String::from("intervention predictions:");
                                let has_danger = results.iter().any(|r| r.activation < 0.0);
                                for r in results.iter().take(5) {
                                    let label = if r.activation < 0.0 {
                                        "⚠️ WARNING (prevented/blocked)"
                                    } else if r.activation > 0.3 {
                                        "DANGER (likely to happen)"
                                    } else {
                                        "possible outcome"
                                    };
                                    text.push_str(&format!("\n  [{label}] {}", r.text));
                                }
                                if has_danger {
                                    stats.predictions_avoided_trap += 1;
                                    text.push_str("\n\n⚠️ This action has previously caused problems. Consider an alternative approach.");
                                }
                                text
                            }
                        } else {
                            "intervention result: no causal graph available.".to_string()
                        }
                    }
                    None => "causal memory not available in this condition.".to_string(),
                },
            };
            transcript.push_str(&format!("\nObservation: {observation}"));
            if solved && step < max_steps {
                // Let the agent emit finish itself; nudge via the transcript.
            }
        }

        if solved {
            stats.solved += 1;
        }
        sequence.push((task.family.id, first_verdict));
        transcript.push_str(&format!(
            "\n[task {} outcome: {} after ≤{max_steps} steps]",
            task.index + 1,
            if solved { "SOLVED" } else { "UNSOLVED" }
        ));
        transcripts.push(transcript);
    }
    stats.exposure = exposure_stats(&sequence);
    Ok((stats, transcripts))
}

/// Run condition C (causal memory with intervention_query).
///
/// Unlike condition B (text search), condition C gives the agent:
/// 1. `intervention_query` — forward simulation BEFORE acting
/// 2. Records failures as `caused` edges and fixes as `prevented` edges
/// 3. The hippocampus graph is used for prediction, not just BM25
async fn run_condition_c(
    config: &LlmConfig,
    tasks: &[Task],
    max_steps: usize,
    temperature: f32,
) -> Result<(RunStats, Vec<String>)> {
    let mut stats = RunStats {
        tasks: tasks.len(),
        ..Default::default()
    };
    let mut transcripts: Vec<String> = Vec::new();
    let mut sequence: Vec<(&str, Option<CmdVerdict>)> = Vec::new();
    let store = CausalStore::open_in_memory()?;
    let system = system_prompt_c();

    for task in tasks {
        let mut transcript = format!(
            "Task {} of {} [{}]: {}",
            task.index + 1,
            tasks.len(),
            task.family.id,
            task.family.task
        );
        let mut solved = false;
        let mut first_verdict: Option<CmdVerdict> = None;
        let mut ran_command = false;
        let mut recorded_this_task = false;
        let mut queries_this_task = 0usize;

        for step in 1..=max_steps {
            stats.total_steps += 1;
            let mut reply = None;
            for attempt in 0..3 {
                match chat(config, &system, &transcript, 300, temperature).await {
                    Ok(r) => {
                        reply = Some(r);
                        break;
                    }
                    Err(e) => {
                        stats.llm_errors += 1;
                        if attempt < 2 {
                            eprintln!(
                                "  [task {} step {step}] LLM call failed (retry {}): {e}",
                                task.index + 1,
                                attempt + 1
                            );
                        }
                    }
                }
            }
            let Some(reply) = reply else {
                transcript.push_str("\n(harness) LLM call failed 3× — step wasted.");
                continue;
            };
            transcript.push_str(&format!("\nYou: {}", reply.trim()));
            let action = match parse_action(&reply) {
                Ok(a) => a,
                Err(e) => {
                    stats.parse_errors += 1;
                    transcript.push_str(&format!(
                        "\nObservation: invalid action ({e}). Output exactly one JSON action."
                    ));
                    continue;
                }
            };

            let observation = match action {
                AgentAction::Finish => {
                    if solved {
                        break;
                    }
                    "the task is NOT complete yet — finish rejected.".to_string()
                }
                AgentAction::Run(cmd) => {
                    ran_command = true;
                    let verdict = classify(task.family.id, &cmd);
                    if first_verdict.is_none() {
                        first_verdict = Some(verdict);
                    }
                    match verdict {
                        CmdVerdict::Solution => {
                            solved = true;
                            // Record the FIX as a prevented edge: "doing the fix
                            // prevented the trap from happening"
                            if !recorded_this_task {
                                store.record_decision(
                                    &cmd,
                                    &format!("avoided {} trap", task.family.id),
                                    "prevented",
                                    Some("agent-bench"),
                                    0.8,
                                    "llm_inferred",
                                )?;
                                stats.mem_writes += 1;
                                recorded_this_task = true;
                            }
                            task.family.ok_output.to_string()
                        }
                        CmdVerdict::Trap => {
                            // Record the TRAP as a caused edge: "naive approach
                            // caused the trap to trigger"
                            if !recorded_this_task {
                                store.record_decision(
                                    &cmd,
                                    task.family.fail_output,
                                    "caused",
                                    Some("agent-bench"),
                                    0.7,
                                    "llm_inferred",
                                )?;
                                stats.mem_writes += 1;
                                recorded_this_task = true;
                            }
                            task.family.fail_output.to_string()
                        }
                        CmdVerdict::Neutral => {
                            "command executed, but the task is not complete.".to_string()
                        }
                    }
                }
                AgentAction::Record { decision, outcome } => {
                    if !ran_command {
                        "no observations yet — run_command first.".to_string()
                    } else if recorded_this_task {
                        "already recorded for this task.".to_string()
                    } else {
                        store.record_decision(
                            &decision, &outcome, "caused",
                            Some("agent-bench"), 0.6, "llm_inferred",
                        )?;
                        stats.mem_writes += 1;
                        recorded_this_task = true;
                        format!("recorded: \"{decision}\" → \"{outcome}\"")
                    }
                }
                AgentAction::Search(query) => {
                    stats.mem_searches += 1;
                    let hits = store
                        .search_causal_bm25(Some("agent-bench"), &query, 3)
                        .unwrap_or_default();
                    if hits.is_empty() {
                        "no memories found.".to_string()
                    } else {
                        let mut text = String::from("memories found:");
                        for h in &hits {
                            text.push_str(&format!(
                                "\n- \"{}\" → \"{}\" ({})",
                                h.decision_text, h.outcome_text, h.relation
                            ));
                        }
                        text
                    }
                }
                AgentAction::InterventionQuery(query) => {
                    if queries_this_task >= 2 {
                        "query limit reached — proceed with run_command.".to_string()
                    } else {
                        queries_this_task += 1;
                        stats.intervention_queries += 1;
                        let graph = causal_memory::hippocampus::CausalGraph::from_store(&store);
                        if let Ok(mut g) = graph {
                            let results = g.spreading_activation_opts(
                                &query, Some("agent-bench"), false, false,
                            );
                            stats.predictions_total += 1;
                            if results.is_empty() {
                                "intervention: UNKNOWN — no past experience. Proceed with caution.".to_string()
                            } else {
                                let has_warning = results.iter().any(|r| r.activation < 0.0);
                                let mut text = String::from("intervention predictions:");
                                for r in results.iter().take(5) {
                                    let label = if r.activation < 0.0 {
                                        "⚠️ WARNING"
                                    } else if r.activation > 0.3 {
                                        "DANGER"
                                    } else {
                                        "possible"
                                    };
                                    text.push_str(&format!("\n  [{label}] {}", r.text));
                                }
                                if has_warning {
                                    stats.predictions_avoided_trap += 1;
                                    text.push_str("\n⚠️ Avoid this action — it caused problems before.");
                                }
                                text
                            }
                        } else {
                            "intervention: no graph available.".to_string()
                        }
                    }
                }
            };
            transcript.push_str(&format!("\nObservation: {observation}"));
        }

        if solved {
            stats.solved += 1;
        }
        sequence.push((task.family.id, first_verdict));
        transcript.push_str(&format!(
            "\n[task {} outcome: {} after ≤{max_steps} steps]",
            task.index + 1,
            if solved { "SOLVED" } else { "UNSOLVED" }
        ));
        transcripts.push(transcript);
    }
    stats.exposure = exposure_stats(&sequence);
    Ok((stats, transcripts))
}
const RESULTS_DIR: &str = "benches/agent/results";

/// Dump the full per-task transcripts of one condition for post-hoc
/// debugging: `benches/agent/results/bench-agent-transcript-<condition>-<timestamp>.md`.
fn write_transcripts(condition: &str, ts: i64, transcripts: &[String]) -> Result<()> {
    std::fs::create_dir_all(RESULTS_DIR)?;
    let file = format!("{RESULTS_DIR}/bench-agent-transcript-{condition}-{ts}.md");
    let mut out = format!("# bench-agent transcripts (condition {condition})\n");
    for t in transcripts {
        out.push_str("\n---\n\n```\n");
        out.push_str(t);
        out.push_str("\n```\n");
    }
    std::fs::write(&file, out)?;
    println!("  transcripts written to {file}");
    Ok(())
}

/// `causal-memory bench-agent [--tasks 8] [--seed 42] [--steps 15] [--condition both|a|b]`
///
/// Requires CAUSAL_MEMORY_LLM_* — the bench measures LLM-agent behavior, so
/// (like bench-compaction) it refuses to run unconfigured instead of
/// silently degrading.
pub async fn run(args: &[String]) -> Result<()> {
    let mut tasks_n = 8usize;
    let mut seed = 42u64;
    let mut steps = 15usize;
    let mut condition = "both".to_string();
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> Result<&String> {
            *i += 1;
            args.get(*i)
                .ok_or_else(|| anyhow::anyhow!("missing value for {}", args[*i - 1]))
        };
        match args[i].as_str() {
            "--tasks" => tasks_n = take(&mut i)?.parse()?,
            "--seed" => seed = take(&mut i)?.parse()?,
            "--steps" => steps = take(&mut i)?.parse()?,
            "--condition" => condition = take(&mut i)?.clone(),
            other => anyhow::bail!(
                "unknown flag: {other}\nUsage: causal-memory bench-agent [--tasks N] [--seed S] [--steps N] [--condition both|a|b]"
            ),
        }
        i += 1;
    }
    if !matches!(condition.as_str(), "both" | "a" | "b" | "c" | "abc") {
        anyhow::bail!("--condition must be both|a|b|c|abc, got: {condition}");
    }

    let config = match LlmConfig::from_env() {
        Some(c) => c,
        None => {
            eprintln!("bench-agent requires an LLM (it measures LLM-agent behavior).");
            eprintln!("Set CAUSAL_MEMORY_LLM_API + CAUSAL_MEMORY_LLM_KEY and retry.");
            std::process::exit(1);
        }
    };
    let tasks = generate_tasks(seed, tasks_n, 3);
    println!("LLM: {} @ {}", config.model, config.api_base);
    // The agent loop's transcript grows with every step; glm-class models
    // exceed the 8s MCP-path timeout on long contexts. 60s for the bench.
    // (chat() reads this per call; the default stays 8 elsewhere.)
    std::env::set_var("CAUSAL_MEMORY_HTTP_TIMEOUT_SECS", "60");
    println!(
        "seed={seed} tasks={} steps={steps} condition={condition} families: {}\n",
        tasks.len(),
        tasks
            .iter()
            .map(|t| t.family.id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ")
    );

    let ts = chrono::Utc::now().timestamp();
    let temperature = 0.3;
    let mut a_stats = None;
    let mut b_stats = None;
    let mut c_stats = None;
    let run_a = condition == "both" || condition == "a" || condition == "abc";
    let run_b = condition == "both" || condition == "b" || condition == "abc";
    let run_c = condition == "c" || condition == "abc";

    if run_a {
        println!("=== Group A (no memory) ===");
        let (s, transcripts) = run_condition(&config, &tasks, false, steps, temperature).await?;
        write_transcripts("a", ts, &transcripts)?;
        println!(
            "A: solved {}/{}, repeat-mistake {:.0}%\n",
            s.solved,
            s.tasks,
            pct(s.exposure.repeat_trapped, s.exposure.repeat_exposures)
        );
        a_stats = Some(s);
    }
    if run_b {
        println!("=== Group B (text-search memory) ===");
        let (s, transcripts) = run_condition(&config, &tasks, true, steps, temperature).await?;
        write_transcripts("b", ts, &transcripts)?;
        println!(
            "B: solved {}/{}, repeat-mistake {:.0}%, mem writes {}, searches {}\n",
            s.solved,
            s.tasks,
            pct(s.exposure.repeat_trapped, s.exposure.repeat_exposures),
            s.mem_writes,
            s.mem_searches
        );
        b_stats = Some(s);
    }
    if run_c {
        println!("=== Group C (causal memory + intervention_query) ===");
        let (s, transcripts) = run_condition_c(&config, &tasks, steps, temperature).await?;
        write_transcripts("c", ts, &transcripts)?;
        println!(
            "C: solved {}/{}, repeat-mistake {:.0}%, interventions {}, predictions avoided {}\n",
            s.solved,
            s.tasks,
            pct(s.exposure.repeat_trapped, s.exposure.repeat_exposures),
            s.intervention_queries,
            s.predictions_avoided_trap
        );
        c_stats = Some(s);
    }
    let report = render_report(
        a_stats.as_ref(),
        b_stats.as_ref(),
        &config.model,
        temperature,
        seed,
        ts,
    );
    println!("{report}");

    // Condition C extra report
    if let Some(ref c) = c_stats {
        println!("\n=== Condition C: Causal Memory Deep Metrics ===");
        println!("  Intervention queries:    {}", c.intervention_queries);
        println!("  Predictions total:       {}", c.predictions_total);
        println!("  Predictions avoided trap: {}", c.predictions_avoided_trap);
        println!("  Memory writes (causal):  {}", c.mem_writes);
        println!("  Repeat-mistake rate:     {:.0}%", pct(c.exposure.repeat_trapped, c.exposure.repeat_exposures));
    }

    std::fs::create_dir_all(RESULTS_DIR)?;
    let file = format!("{RESULTS_DIR}/bench-agent-results-{ts}.md");
    std::fs::write(&file, &report)?;
    println!("Report written to {file}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_tasks_deterministic() {
        let a = generate_tasks(42, 8, 3);
        let b = generate_tasks(42, 8, 3);
        assert_eq!(a, b, "same seed → identical task sequence");
        assert_eq!(a.len(), 8);
        // Spacing: no two adjacent tasks share a family.
        assert!(
            a.windows(2).all(|w| w[0].family.id != w[1].family.id),
            "same-family tasks must be spaced: {:?}",
            a.iter().map(|t| t.family.id).collect::<Vec<_>>()
        );
        // Default shape: exactly 3 families, each appearing 2-3 times.
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for t in &a {
            *counts.entry(t.family.id).or_default() += 1;
        }
        assert_eq!(counts.len(), 3);
        assert!(counts.values().all(|&n| (2..=3).contains(&n)));
        // Task text never leaks the trap or the solution (anti-cheat).
        for f in FAMILIES {
            assert!(!f.task.contains("proxy"));
            assert!(!f.task.contains("Bearer"));
            assert!(!f.task.contains("nextest"));
            assert!(!f.task.contains("CAUSAL_MEMORY_DB"));
            assert!(!f.task.contains(".cargo"));
            assert!(!f.task.contains("application/json"));
        }
        // A different seed gives a different sequence.
        assert_ne!(a, generate_tasks(43, 8, 3));
    }

    #[test]
    fn test_classify_trap_rules() {
        // proxy-github
        assert_eq!(
            classify("proxy-github", "git fetch origin"),
            CmdVerdict::Trap
        );
        assert_eq!(
            classify(
                "proxy-github",
                "git -c http.proxy=http://127.0.0.1:1097 fetch origin"
            ),
            CmdVerdict::Solution
        );
        // cargo-env
        assert_eq!(
            classify("cargo-env", "cargo build --release"),
            CmdVerdict::Trap
        );
        assert_eq!(
            classify("cargo-env", "source .cargo/env && cargo build --release"),
            CmdVerdict::Solution
        );
        // bearer-token
        assert_eq!(
            classify("bearer-token", "curl https://api.internal.local/v1/status"),
            CmdVerdict::Trap
        );
        assert_eq!(
            classify(
                "bearer-token",
                "curl -H 'Authorization: Bearer t0ken' https://api.internal.local/v1/status"
            ),
            CmdVerdict::Solution
        );
        // nextest
        assert_eq!(classify("nextest", "cargo test"), CmdVerdict::Trap);
        assert_eq!(
            classify("nextest", "cargo nextest run"),
            CmdVerdict::Solution
        );
        // db-path
        assert_eq!(
            classify("db-path", "causal-memory export lessons.jsonl"),
            CmdVerdict::Trap
        );
        assert_eq!(
            classify(
                "db-path",
                "CAUSAL_MEMORY_DB=/tmp/m.db causal-memory export lessons.jsonl"
            ),
            CmdVerdict::Solution
        );
        // json-content-type
        assert_eq!(
            classify(
                "json-content-type",
                "curl -X POST https://api.internal.local/v1/jobs -d 'name=demo'"
            ),
            CmdVerdict::Trap
        );
        assert_eq!(
            classify("json-content-type", "curl -H 'Content-Type: application/json' -d '{\"name\":\"demo\"}' https://api.internal.local/v1/jobs"),
            CmdVerdict::Solution
        );
        // Unrelated commands are neutral everywhere.
        for f in FAMILIES {
            assert_eq!(
                classify(f.id, "ls -la /tmp"),
                CmdVerdict::Neutral,
                "{}",
                f.id
            );
        }
    }

    #[test]
    fn test_parse_action() {
        assert_eq!(
            parse_action(r#"{"action":"run_command","cmd":"git fetch"}"#).unwrap(),
            AgentAction::Run("git fetch".into())
        );
        assert_eq!(
            parse_action(r#"{"action":"finish"}"#).unwrap(),
            AgentAction::Finish
        );
        assert_eq!(
            parse_action(
                r#"{"action":"record_memory","decision":"used proxy","outcome":"fetch succeeded"}"#
            )
            .unwrap(),
            AgentAction::Record {
                decision: "used proxy".into(),
                outcome: "fetch succeeded".into()
            }
        );
        assert_eq!(
            parse_action(r#"{"action":"search_memory","query":"github proxy"}"#).unwrap(),
            AgentAction::Search("github proxy".into())
        );
        // Markdown fences and surrounding prose are tolerated.
        assert_eq!(
            parse_action("Let me try:\n```json\n{\"action\":\"finish\"}\n```").unwrap(),
            AgentAction::Finish
        );
        // Malformed / unknown are errors, never panics.
        assert!(parse_action("not json at all").is_err());
        assert!(parse_action(r#"{"action":"teleport"}"#).is_err());
        assert!(parse_action(r#"{"cmd":"ls"}"#).is_err());
        assert!(parse_action(r#"{"action":"run_command"}"#).is_err());
        // Two actions in one reply (record then finish — the pattern glm
        // emits after the hard rule): the first object wins, the second is
        // retried next turn. This is what actually gets memories written.
        assert_eq!(
            parse_action(
                "{\"action\":\"record_memory\",\"decision\":\"used proxy\",\"outcome\":\"fetch ok\"}\n{\"action\":\"finish\"}"
            )
            .unwrap(),
            AgentAction::Record {
                decision: "used proxy".into(),
                outcome: "fetch ok".into()
            }
        );
        // Braces inside strings don't break the balance scan.
        assert_eq!(
            parse_action(r#"{"action":"run_command","cmd":"echo {not json}"}"#).unwrap(),
            AgentAction::Run("echo {not json}".into())
        );
    }

    #[test]
    fn test_exposure_stats() {
        // f1: trapped on exposure 1 AND on the first repeat, solved on the
        // second repeat; f2: solved on exposure 1, trapped on the repeat.
        let seq = vec![
            ("f1", Some(CmdVerdict::Trap)),
            ("f2", Some(CmdVerdict::Solution)),
            ("f1", Some(CmdVerdict::Trap)),
            ("f1", Some(CmdVerdict::Solution)),
            ("f2", Some(CmdVerdict::Trap)),
        ];
        let s = exposure_stats(&seq);
        assert_eq!(
            s,
            ExposureStats {
                first_exposures: 2,
                first_trapped: 1,
                repeat_exposures: 3,
                repeat_trapped: 2,
            }
        );
        // A task whose agent never issued run_command counts as an exposure
        // but not as trapped.
        let s = exposure_stats(&[("f1", None), ("f1", None)]);
        assert_eq!(s.first_exposures, 1);
        assert_eq!(s.first_trapped, 0);
        assert_eq!(s.repeat_exposures, 1);
        assert_eq!(s.repeat_trapped, 0);
    }

    #[test]
    fn test_render_report() {
        let mut a = RunStats {
            tasks: 8,
            solved: 6,
            total_steps: 80,
            ..Default::default()
        };
        a.exposure = ExposureStats {
            first_exposures: 3,
            first_trapped: 3,
            repeat_exposures: 5,
            repeat_trapped: 4,
        };
        let mut b = RunStats {
            tasks: 8,
            solved: 8,
            total_steps: 72,
            mem_writes: 3,
            mem_searches: 4,
            post_search_runs: 4,
            post_search_hits: 3,
            ..Default::default()
        };
        b.exposure = ExposureStats {
            first_exposures: 3,
            first_trapped: 2,
            repeat_exposures: 5,
            repeat_trapped: 1,
        };
        let md = render_report(Some(&a), Some(&b), "deepseek-chat", 0.3, 42, 1_700_000_000);
        assert!(md.contains("model: deepseek-chat"));
        assert!(md.contains("seed: 42"));
        assert!(md.contains("| A (no memory) | 6/8 | 10.0 | 100% (3/3) | 80% (4/5) |"));
        assert!(md.contains("| B (causal memory) | 8/8 | 9.0 | 67% (2/3) | 20% (1/5) |"));
        assert!(md.contains("post-search first-action hit rate 75% (3/4)"));
        assert!(
            md.contains("NOT"),
            "honesty note about LLM non-reproducibility"
        );
        // Single-condition renders just that group.
        let md = render_report(None, Some(&b), "m", 0.3, 1, 0);
        assert!(!md.contains("A (no memory)"));
    }
}
