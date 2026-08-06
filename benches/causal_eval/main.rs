//! CausalEval — graph-grounded causal memory benchmark (docs/causal-eval-2026.md).
//!
//! Subcommands:
//!   generate --graphs N --out DIR   deterministic typed-DAG graphs + questions
//!   narrate  --data DIR             LLM narrativization + verification pass
//!   run      --data DIR [--search-only] [--topk N] [--concurrency N]
//!
//! Design: the causal graph IS the answer key. Conversations are generated from
//! a known typed DAG (caused/enabled/prevented), questions are derived from the
//! graph structure per capability class (C1-C7), and gold answers come from the
//! graph — zero hand annotation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

// ─── Data model ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalNode {
    pub id: usize,
    /// The person who acts ("Melanie").
    pub person: String,
    /// The action/event text ("deployed without running the test suite").
    pub action: String,
    /// Event semantics for gold derivation (positive/negative/neutral).
    pub polarity: String,
    /// Temporal order within the graph (construction order).
    pub order: usize,
    /// Task domain tag (for C6 cross-task transfer).
    pub task_tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalEdge {
    pub from: usize,
    pub to: usize,
    pub relation: String, // caused | enabled | prevented
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalGraph {
    pub id: usize,
    pub nodes: Vec<CausalNode>,
    pub edges: Vec<CausalEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub number: u32,
    pub date_time: String,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarratedGraph {
    #[serde(default)]
    pub graph_id: usize,
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalQa {
    /// 11..17 → C1..C7.
    pub category: u32,
    pub question: String,
    pub answer: String,
    /// Graph node ids whose events carry the answer (evidence anchors).
    pub evidence_nodes: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphBundle {
    pub graph: CausalGraph,
    pub conversations: Vec<NarratedGraph>,
    pub qa: Vec<CausalQa>,
}

// ─── RNG (deterministic, xorshift — no external dep) ──────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ─── Graph generation ──────────────────────────────────────────────────────

const PERSONS: &[&str] = &["Melanie", "Caroline", "Nate", "Joanna", "Priya", "Sam"];
const TASKS: &[&str] = &["deployment", "data-migration", "auth", "api", "testing", "infra"];
const POLARITIES: &[&str] = &["positive", "negative", "neutral"];
const RELATIONS: &[&str] = &["caused", "enabled", "prevented"];

/// Deterministic action templates per task domain — events sound like real
/// engineering decisions (the dialogue domain).
/// Semantically coherent causal pairs: (task, bad_practice, failure_it_causes,
/// good_practice, what_good_achieves). The bad practice's failure and the good
/// practice's achievement are causally linked — a dialogue about them is
/// coherent, and the graph-derived gold answers are unambiguous.
const CAUSAL_PAIRS: &[(&str, &str, &str, &str, &str)] = &[
    ("deployment",
     "deployed without running the test suite",
     "a regression slipped into production",
     "set up a CI gate that blocks merges without tests",
     "untested code never reached production"),
    ("deployment",
     "ran the deployment during peak hours",
     "the release caused an outage",
     "scheduled deployments for off-peak hours",
     "releases went out without incidents"),
    ("data-migration",
     "migrated the schema without a backup",
     "the migration corrupted customer records",
     "took a full snapshot before the migration",
     "the migration could always be rolled back"),
    ("data-migration",
     "ran the migration during peak traffic",
     "the migration locked the database",
     "ran the migration in a maintenance window",
     "the migration finished without locking anything"),
    ("auth",
     "stored passwords without hashing",
     "user accounts were compromised",
     "hashed passwords before storing them",
     "credentials stayed safe even in a breach"),
    ("auth",
     "left debug credentials in production",
     "an attacker found the debug account",
     "removed debug credentials from production",
     "no backdoor account was left exposed"),
    ("api",
     "shipped the API without pagination",
     "the API timed out under load",
     "added pagination to the API",
     "the API held up under heavy load"),
    ("api",
     "logged sensitive data to the query log",
     "customer data leaked into the logs",
     "removed sensitive fields from the logs",
     "the logs contained no customer data"),
    ("testing",
     "skipped the flaky tests to make CI green",
     "a regression slipped into production",
     "ran the full regression suite before release",
     "regressions were caught before shipping"),
    ("testing",
     "deleted the end-to-end tests to save time",
     "the app broke right after release",
     "kept the end-to-end tests in the pipeline",
     "the app stayed healthy after release"),
    ("infra",
     "pointed DNS at the new cluster before it was ready",
     "traffic hit a cluster that could not serve it",
     "waited for the cluster health check before switching DNS",
     "traffic moved over without a hitch"),
    ("infra",
     "provisioned instances without autoscaling",
     "the service went down under a traffic spike",
     "enabled autoscaling on the instances",
     "the service absorbed the traffic spike"),
];

fn pairs_for(task: &str) -> Vec<&'static (&'static str, &'static str, &'static str, &'static str, &'static str)> {
    CAUSAL_PAIRS.iter().filter(|p| p.0 == task).collect()
}

/// Generate one deterministic typed DAG with a guaranteed causal chain depth
/// ≥ 2 (for C1) and at least one prevented edge (for C4).
fn generate_graph(id: usize, seed: u64) -> CausalGraph {
    let mut rng = Rng::new(seed);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let person = (*rng.pick(PERSONS)).to_string();
    let task = (*rng.pick(TASKS)).to_string();
    let pairs = pairs_for(&task);
    assert!(!pairs.is_empty(), "no causal pairs for task {task}");
    let bad_pair = **rng.pick(&pairs);
    let good_pair = **rng.pick(&pairs);

    // 0: bad practice; 1: its failure (caused); 2: good practice; 3: good
    // outcome (enabled by 2); 4: preventer (prevents 1's failure); 5-6: twin.
    let mk = |id: usize, action: &str, polarity: &str, order: usize| CausalNode {
        id,
        person: person.clone(),
        action: action.to_string(),
        polarity: polarity.to_string(),
        order,
        task_tag: task.clone(),
    };
    nodes.push(mk(0, bad_pair.1, "neutral", 0));
    nodes.push(mk(1, bad_pair.2, "negative", 1));
    nodes.push(mk(2, good_pair.3, "neutral", 2));
    nodes.push(mk(3, good_pair.4, "positive", 3));
    nodes.push(mk(4, good_pair.3, "positive", 4));
    edges.push(CausalEdge { from: 0, to: 1, relation: "caused".to_string() });
    edges.push(CausalEdge { from: 2, to: 3, relation: "enabled".to_string() });
    edges.push(CausalEdge { from: 4, to: 1, relation: "prevented".to_string() });

    // Cross-task twin (C6): a bad practice in a different task with its failure.
    let twin_task = loop {
        let t = (*rng.pick(TASKS)).to_string();
        if t != task {
            break t;
        }
    };
    let twin_pairs = pairs_for(&twin_task);
    let twin_pair = **rng.pick(&twin_pairs);
    nodes.push(CausalNode {
        id: 5,
        person: person.clone(),
        action: twin_pair.1.to_string(),
        polarity: "negative".to_string(),
        order: 5,
        task_tag: twin_task.clone(),
    });
    nodes.push(CausalNode {
        id: 6,
        person: person.clone(),
        action: twin_pair.2.to_string(),
        polarity: "negative".to_string(),
        order: 6,
        task_tag: twin_task,
    });
    edges.push(CausalEdge { from: 5, to: 6, relation: "caused".to_string() });

    CausalGraph { id, nodes, edges }
}

// ─── Question generation (deterministic from the graph) ───────────────────

fn node_action(g: &CausalGraph, id: usize) -> String {
    format!("{} {}", g.nodes[id].person, g.nodes[id].action)
}

fn question_for(g: &CausalGraph, class: u32) -> Option<CausalQa> {
    let p = &g.nodes[0].person;
    match class {
        // C1: why did the bad outcome happen? gold = the causal chain back to
        // the root decision (nodes 0 → 1 → ... odd nodes).
        11 => {
            // Bad outcome = node 1 (first odd). Chain: root decision caused it.
            let bad = &g.nodes[1];
            let root = &g.nodes[0];
            let answer = format!("{} caused it: {} → {}", root.person, root.action, bad.action);
            Some(CausalQa {
                category: 11,
                question: format!("Why did {} happen?", bad.action),
                answer,
                evidence_nodes: vec![0, 1],
            })
        }
        // C2: intervention — what happens if the root decision is repeated?
        12 => {
            let root = &g.nodes[0];
            let bad = &g.nodes[1];
            Some(CausalQa {
                category: 12,
                question: format!("If {} does this again — {} — what will happen?", p, root.action),
                answer: bad.action.clone(),
                evidence_nodes: vec![0, 1],
            })
        }
        // C3: counterfactual — the fix decision (node 2) vs the root decision.
        13 => {
            let root = &g.nodes[0];
            let fix = &g.nodes[2];
            let fix_out = &g.nodes[3];
            Some(CausalQa {
                category: 13,
                question: format!("{} has two options: do {} again, or do {} instead. Which should {} choose?", p, root.action, fix.action, p),
                answer: format!("{} ({})", fix.action, fix_out.action),
                evidence_nodes: vec![0, 1, 2, 3],
            })
        }
        // C4: inhibition — what prevented the bad outcome (or how to prevent).
        14 => {
            let bad = &g.nodes[1];
            let preventer = &g.nodes[4]; // the prevented edge's source
            Some(CausalQa {
                category: 14,
                question: format!("What stopped {} from happening again?", bad.action),
                answer: preventer.action.clone(),
                evidence_nodes: vec![preventer.id, bad.id],
            })
        }
        // C5: temporal order on the chain.
        15 => {
            let a = &g.nodes[0];
            let b = &g.nodes[1];
            Some(CausalQa {
                category: 15,
                question: format!("Which happened first: {} or {}?", a.action, b.action),
                answer: format!("{}", a.action),
                evidence_nodes: vec![0, 1],
            })
        }
        // C6: lesson transfer — the twin failed; what should the person avoid?
        16 => {
            let twin = &g.nodes[g.nodes.len() - 2];
            Some(CausalQa {
                category: 16,
                question: format!("Facing a similar situation in a different area, what should {} avoid doing?", p),
                answer: twin.action.clone(),
                evidence_nodes: vec![twin.id],
            })
        }
        // C7: update — after a later correction, what does the person believe?
        17 => {
            let fix = &g.nodes[2];
            Some(CausalQa {
                category: 17,
                question: format!("After everything, what does {} now believe is the right way to handle deployments?", p),
                answer: fix.action.clone(),
                evidence_nodes: vec![2],
            })
        }
        _ => None,
    }
}

// ─── CLI ───────────────────────────────────────────────────────────────────

fn usage() {
    eprintln!(
        "causal-memory-causal-eval <generate|narrate|run> [options]\n\
         \n\
         generate --graphs N --out DIR   deterministic graphs + questions\n\
         narrate  --data DIR             LLM narrativization + verification\n\
         run      --data DIR [--search-only] [--topk N] [--concurrency N]"
    );
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(1);
    }
    match args[1].as_str() {
        "generate" => cmd_generate(&args[2..]),
        "narrate" => cmd_narrate(&args[2..]),
        "run" => cmd_run(&args[2..]),
        other => anyhow::bail!("unknown subcommand {other:?}"),
    }
}

fn take(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("missing value for {flag}"))
}

fn cmd_generate(args: &[String]) -> Result<()> {
    let mut graphs = 10usize;
    let mut out = PathBuf::from("benches/causal_eval/data");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--graphs" => graphs = take(args, &mut i, "--graphs")?.parse()?,
            "--out" => out = PathBuf::from(take(args, &mut i, "--out")?),
            other => anyhow::bail!("unknown flag {other:?}"),
        }
        i += 1;
    }
    std::fs::create_dir_all(&out)?;
    let base_seed: u64 = 20_260_806;
    for g in 0..graphs {
        let graph = generate_graph(g, base_seed + g as u64 * 7919);
        let qa: Vec<CausalQa> = (11..=17).filter_map(|c| question_for(&graph, c)).collect();
        let bundle = GraphBundle {
            graph,
            conversations: Vec::new(),
            qa,
        };
        let path = out.join(format!("graph_{g}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&bundle)?)?;
        eprintln!("generated graph {g} -> {}", path.display());
    }
    Ok(())
}

// ─── Placeholder narrate/run (implemented next) ────────────────────────────

// ─── LLM client (minimal, OpenAI-compatible, retry) ───────────────────────

struct LlmCfg {
    api_base: String,
    api_key: String,
    model: String,
}

impl LlmCfg {
    fn from_env() -> Result<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .or_else(|_| std::env::var("CAUSAL_MEMORY_LLM_KEY"))
            .map_err(|_| anyhow!("DEEPSEEK_API_KEY not set"))?;
        Ok(Self {
            api_base: std::env::var("LOCOMO_LLM_API")
                .unwrap_or_else(|_| "https://api.deepseek.com/v1".into()),
            api_key,
            model: std::env::var("LOCOMO_LLM_MODEL").unwrap_or_else(|_| "deepseek-chat".into()),
        })
    }
}

#[derive(Serialize)]
struct ChatReq<'a> {
    model: &'a str,
    messages: Vec<ChatMsg<'a>>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMsgOwned,
}

#[derive(Deserialize)]
struct ChatMsgOwned {
    content: String,
    #[serde(default)]
    reasoning_content: String,
}

/// One chat call with retries (JSON-mode for structured outputs).
async fn chat(
    cfg: &LlmCfg,
    system: &str,
    user: &str,
    max_tokens: u32,
    json_mode: bool,
) -> Result<String> {
    let client = reqwest::Client::new();
    let mut last_err = "no attempt made".to_string();
    for attempt in 0..3 {
        let req = ChatReq {
            model: &cfg.model,
            messages: vec![
                ChatMsg { role: "system", content: system },
                ChatMsg { role: "user", content: user },
            ],
            max_tokens,
            temperature: 0.0,
        };
        let url = format!("{}/chat/completions", cfg.api_base.trim_end_matches('/'));
        let mut builder = client.post(&url).header("Authorization", format!("Bearer {}", cfg.api_key));
        if json_mode {
            builder = builder.json(&serde_json::json!({
                "model": &cfg.model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user}
                ],
                "max_tokens": max_tokens,
                "temperature": 0.0,
                "response_format": {"type": "json_object"}
            }));
        } else {
            builder = builder.json(&req);
        }
        match builder.send().await {
            Ok(resp) if resp.status().is_success() => {
                let parsed: ChatResp = resp.json().await.map_err(|e| anyhow!("json: {e}"))?;
                if let Some(c) = parsed.choices.first() {
                    let content = c.message.content.trim();
                    if !content.is_empty() {
                        return Ok(content.to_string());
                    }
                    let reasoning = c.message.reasoning_content.trim();
                    if !reasoning.is_empty() {
                        return Ok(reasoning.to_string());
                    }
                }
                last_err = "empty response".to_string();
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!("LLM API {status}: {}", &body[..body.len().min(200)]);
                if status.is_client_error() && status.as_u16() != 429 {
                    return Err(anyhow!("{last_err}"));
                }
            }
            Err(e) => last_err = format!("{e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
    }
    Err(anyhow!("{last_err}"))
}

fn arg_data(args: &[String]) -> Result<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--data" {
            return Ok(PathBuf::from(take(args, &mut i, "--data")?));
        }
        i += 1;
    }
    anyhow::bail!("--data DIR is required")
}

fn load_bundles(data: &Path) -> Result<Vec<GraphBundle>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(data)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("graph_") && name.ends_with(".json") {
            let bundle: GraphBundle =
                serde_json::from_str(&std::fs::read_to_string(entry.path())?)
                    .with_context(|| format!("parsing {}", entry.path().display()))?;
            out.push(bundle);
        }
    }
    out.sort_by_key(|b| b.graph.id);
    Ok(out)
}

// ─── Narrate: LLM turns a graph into conversations ────────────────────────

const NARRATE_SYSTEM: &str = "You are a fiction writer creating realistic work conversations between two colleagues. You always respond with raw JSON only — never prose, never explanations, never meta-commentary. Begin your response with { and end with }.";

fn narrate_prompt(g: &CausalGraph, retry_hint: &str) -> String {
    let person = &g.nodes[0].person;
    let events: Vec<String> = g
        .nodes
        .iter()
        .map(|n| format!("{} {}", n.person, n.action))
        .collect();
    format!(
        "Write a realistic chat between {} and a colleague (2-3 sessions, different dates, 8-14 turns total). The following things happened; mention each one naturally at least once (rephrasing fine):\n{}\n\n{}Begin your response directly with the JSON, no prose.\n\nExample shape (fill with YOUR content):\n{{\"sessions\": [{{\"number\": 1, \"date_time\": \"2023-05-08 09:00\", \"turns\": [{{\"speaker\": \"Nate\", \"text\": \"morning!\"}}, {{\"speaker\": \"Kim\", \"text\": \"hey\"}}]}}]}}",
        person,
        events.join("\n"),
        retry_hint
    )
}

/// Robust JSON extraction: strip ``` fences and prose, take the outermost
/// {...} (the model frequently wraps or prefixes despite json_object mode).
fn extract_json(raw: &str) -> Option<serde_json::Value> {
    let s = raw.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s).trim();
    let start = s.find('{')?;
    // 1) Candidate close braces from the END backward: the model sometimes
    //    appends decorative brackets after the JSON; the first candidate that
    //    parses wins.
    let mut ends: Vec<usize> = s.match_indices('}').map(|(i, _)| i).collect();
    ends.reverse();
    for end in ends {
        if end <= start {
            break;
        }
        if let Ok(v) = serde_json::from_str(&s[start..=end]) {
            return Some(v);
        }
    }
    // 2) Whole-tail repair: deepseek truncates long JSON at the very end,
    //    dropping the final closing brace(s) — retry with 1-2 appended.
    let tail = &s[start..];
    for extra in 1..=2usize {
        let mut repaired = tail.to_string();
        for _ in 0..extra {
            repaired.push('}');
        }
        if let Ok(v) = serde_json::from_str(&repaired) {
            return Some(v);
        }
    }
    None
}

/// Key tokens of an action (lowercased words ≥ 4 chars, stopwords dropped).
fn key_tokens(text: &str) -> Vec<String> {
    const STOP: &[&str] = &["with", "without", "before", "after", "during", "under", "into", "from", "were", "was", "the", "and", "that", "this", "have", "has", "had", "did", "they", "their", "there", "them"];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

fn action_mentioned(convo_text: &str, action: &str) -> bool {
    let tokens = key_tokens(action);
    if tokens.is_empty() {
        return false;
    }
    let lower = convo_text.to_lowercase();
    tokens.iter().filter(|t| lower.contains(t.as_str())).count() >= (tokens.len() / 2 + 1).max(1)
}

fn verify_narration(g: &CausalGraph, conv_text: &str) -> Vec<usize> {
    g.nodes
        .iter()
        .filter(|n| !action_mentioned(conv_text, &n.action))
        .map(|n| n.id)
        .collect()
}

fn cmd_narrate(args: &[String]) -> Result<()> {
    let data = arg_data(args)?;
    let cfg = LlmCfg::from_env()?;
    let bundles = load_bundles(&data)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        for mut bundle in bundles {
            let g = &bundle.graph;
            let mut narrations = Vec::new();
            let mut missing_all: Vec<usize> = (0..g.nodes.len()).collect();
            for attempt in 0..3 {
                let hint = if attempt == 0 {
                    String::new()
                } else {
                    let names: Vec<String> = missing_all
                        .iter()
                        .map(|&i| g.nodes[i].action.clone())
                        .collect();
                    format!(
                        "\n\nWARNING from the previous attempt: these events were NOT mentioned — you MUST include them: {}.\n",
                        names.join("; ")
                    )
                };
                let prompt = narrate_prompt(g, &hint);
                // json_mode intentionally OFF: the response_format constraint
                // triggers meta-prose on this model; extract_json handles fences.
                match chat(&cfg, NARRATE_SYSTEM, &prompt, 8000, false).await {
                    Ok(raw) => match extract_json(&raw)
                        .filter(|v| v.get("sessions").is_some())
                        .and_then(|v| serde_json::from_value::<NarratedGraph>(v).ok())
                    {
                        Some(mut ng) => {
                            ng.graph_id = g.id;
                            let text = ng
                                .sessions
                                .iter()
                                .flat_map(|s| s.turns.iter().map(|t| t.text.as_str()))
                                .collect::<Vec<_>>()
                                .join(" ");
                            missing_all = verify_narration(g, &text);
                            if missing_all.is_empty() {
                                narrations = vec![ng];
                                break;
                            }
                            narrations = vec![ng];
                            eprintln!(
                                "graph {} attempt {}: {} events missing, retrying",
                                g.id,
                                attempt + 1,
                                missing_all.len()
                            );
                        }
                        None => {
                            eprintln!(
                                "graph {} attempt {}: unparseable narration len={}",
                                g.id,
                                attempt + 1,
                                raw.len()
                            );
                            let _ = std::fs::write("benches/causal_eval/debug_raw.json", &raw);
                        }
                    },
                    Err(e) => {
                        eprintln!("graph {} attempt {}: LLM failed: {e}", g.id, attempt + 1);
                    }
                }
            }
            if let Some(ng) = narrations.first() {
                bundle.conversations = vec![ng.clone()];
                let path = data.join(format!("graph_{}.json", g.id));
                std::fs::write(&path, serde_json::to_string_pretty(&bundle)?)?;
                if missing_all.is_empty() {
                    eprintln!("graph {}: narrated OK ({} turns)", g.id,
                        ng.sessions.iter().map(|s| s.turns.len()).sum::<usize>());
                } else {
                    eprintln!("graph {}: narrated with {} events still missing: {:?}", g.id, missing_all.len(), missing_all);
                }
            } else {
                anyhow::bail!("graph {}: narration failed after 3 attempts", g.id);
            }
        }
        Ok(())
    })
}

// ─── Run: ingest + retrieve + answer + judge ──────────────────────────────

const ANSWER_SYSTEM: &str = "You are answering a question using retrieved memories from past work conversations between two colleagues. Follow these steps IN ORDER.\n\n## Step 1: SCAN ALL MEMORIES\nRead EVERY memory. Details are often scattered across the whole list.\n\n## Step 2: ENTITY VERIFICATION\nOnly use memories about the correct person.\n\n## Step 3: COMBINE AND REASON\nCombine facts across memories. For causal questions: identify what caused what, what prevented what, and what the person now believes.\n\n## Step 4: COMMIT AND ANSWER\nGive a direct, specific answer after \"ANSWER:\". Never say \"not specified\" when a memory contains the information. Keep the final answer short.\n- IRON RULE: your response MUST contain the marker \"ANSWER:\" followed by the final answer as the LAST line.";

const JUDGE_SYSTEM: &str = "You are an impartial judge evaluating whether a predicted answer correctly answers a question about past work conversations. Respond with ONLY a JSON object (no markdown): {\"verdict\": \"correct\" or \"incorrect\", \"reason\": \"<one sentence>\"}";

fn cmd_run(args: &[String]) -> Result<()> {
    let mut data = PathBuf::from("benches/causal_eval/data");
    let mut search_only = false;
    let mut topk = 10usize;
    let mut concurrency = 4usize;
    let mut limit: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => data = PathBuf::from(take(args, &mut i, "--data")?),
            "--search-only" => search_only = true,
            "--topk" => topk = take(args, &mut i, "--topk")?.parse()?,
            "--concurrency" => concurrency = take(args, &mut i, "--concurrency")?.parse()?,
            "--limit" => limit = Some(take(args, &mut i, "--limit")?.parse()?),
            other => anyhow::bail!("unknown flag {other:?}"),
        }
        i += 1;
    }
    let cfg = LlmCfg::from_env()?;
    let bundles = load_bundles(&data)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let embedder: std::sync::Arc<tokio::sync::Mutex<Option<causal_memory::embed::UnifiedEmbedder>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(causal_memory::embed::init_embedder()));
        for bundle in &bundles {
            let g = &bundle.graph;
            let qas: Vec<&CausalQa> = bundle.qa.iter().collect();
            if let Some(l) = limit {
                if qas.len() > l { continue; }
            }
            // Per-graph store.
            std::fs::create_dir_all("benches/causal_eval/db")?;
            let db_path = format!("benches/causal_eval/db/graph_{}.db", g.id);
            let store = causal_memory::store::CausalStore::open(&db_path)?;
            // Ingest conversations (turn chunks + temporal edges), if not present.
            let chunk_count: i64 = store
                .with_conn(|c| {
                    c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get::<_, i64>(0))
                        .map_err(|e| anyhow!("{e}"))
                })?;
            if chunk_count == 0 {
                ingest_conversations(&store, &bundle.conversations)?;
                distill_if_available(&store, &bundle.conversations, concurrency).await?;
            }
            let evidence_ids = precompute_evidence(&store, g)?;
            eprintln!("graph {}: {} chunks, {} questions", g.id, chunk_count, qas.len());
            for qa in qas {
                let row = run_question(&cfg, &store, &embedder, g, qa, &evidence_ids, topk, search_only).await;
                println!("{}", serde_json::to_string(&row)?);
            }
        }
        Ok(())
    })
}

#[derive(Serialize)]
struct ResultRow {
    graph: usize,
    category: u32,
    question: String,
    gold: String,
    predicted: String,
    verdict: String,
    evidence_hit: bool,
    retrieved_ids: Vec<String>,
}

fn ingest_conversations(store: &causal_memory::store::CausalStore, convs: &[NarratedGraph]) -> Result<()> {
    use rusqlite::params;
    let mut base_ts: i64 = 1_683_000_000;
    for conv in convs {
        for session in &conv.sessions {
            let mut prev_other: Option<&Turn> = None;
            for (idx, turn) in session.turns.iter().enumerate() {
                let chunk_id = format!("g{}:s{}:t{}", conv.graph_id, session.number, idx);
                let ts = base_ts + idx as i64 * 3600;
                let text = format!("[session_{} {}] {}: {}", session.number, session.date_time, turn.speaker, turn.text);
                store.with_conn(|c| {
                    c.execute(
                        "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                        params![&chunk_id, &text, ts],
                    )?;
                    if let Some(prev) = prev_other {
                        let prev_id = format!("g{}:s{}:t{}", conv.graph_id, session.number, idx - 1);
                        c.execute(
                            "INSERT OR IGNORE INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                             VALUES (?1, ?2, 'caused', 0.4, 'temporal', ?3, ?3, NULL)",
                            params![&prev_id, &chunk_id, ts],
                        )?;
                    }
                    Ok(())
                })?;
                if turn.speaker != session.turns[0].speaker {
                    prev_other = Some(turn);
                }
            }
            base_ts += 86_400;
        }
    }
    Ok(())
}

async fn distill_if_available(
    store: &causal_memory::store::CausalStore,
    convs: &[NarratedGraph],
    concurrency: usize,
) -> Result<()> {
    let Some(distiller) = causal_memory::distill::Distiller::from_env() else {
        eprintln!("causal-eval: no Distiller (DEEPSEEK_API_KEY unset); raw chunks only");
        return Ok(());
    };
    for conv in convs {
        for session in &conv.sessions {
            let turns: Vec<(String, String)> = session
                .turns
                .iter()
                .map(|t| (t.speaker.clone(), t.text.clone()))
                .collect();
            let date = session.date_time.clone();
            match distiller.distill_session(&date, &turns).await {
                Ok(items) => {
                    for item in &items {
                        let _ = causal_memory::distill::record_items(store, &[item.clone()], None)?;
                    }
                }
                Err(e) => eprintln!("distill session {} failed: {e}", session.number),
            }
        }
    }
    Ok(())
}

/// Precompute the chunk id that mentions each node's action (evidence anchor).
fn precompute_evidence(
    store: &causal_memory::store::CausalStore,
    g: &CausalGraph,
) -> Result<HashMap<usize, String>> {
    let rows: Vec<(String, String)> = store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT id, text FROM chunks")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut out = HashMap::new();
    for n in &g.nodes {
        let tokens = key_tokens(&n.action);
        let hit = rows.iter().find(|(_, text)| {
            let lower = text.to_lowercase();
            tokens.iter().filter(|t| lower.contains(t.as_str())).count() >= (tokens.len() / 2 + 1).max(1)
        });
        if let Some((id, _)) = hit {
            out.insert(n.id, id.clone());
        }
    }
    Ok(out)
}

async fn run_question(
    cfg: &LlmCfg,
    store: &causal_memory::store::CausalStore,
    embedder: &std::sync::Arc<tokio::sync::Mutex<Option<causal_memory::embed::UnifiedEmbedder>>>,
    g: &CausalGraph,
    qa: &CausalQa,
    evidence_ids: &HashMap<usize, String>,
    topk: usize,
    search_only: bool,
) -> ResultRow {
    // Retrieval: BM25 + entity-boosted semantic + hop (same signals as the
    // LoCoMo harness — all store-side).
    let bm25 = store.search_causal_bm25(None, &qa.question, topk).unwrap_or_default();
    let mut lists: Vec<&[causal_memory::store::CausalEntry]> = vec![&bm25];
    let semantic: Vec<causal_memory::store::CausalEntry> = {
        let mut guard = embedder.lock().await;
        let embedder_ref = guard.as_mut();
        let qv = match embedder_ref {
            Some(e) => e.embed(&qa.question).await.ok(),
            None => None,
        };
        qv.map(|v| {
                store
                    .search_causal_semantic_entity_boosted(&v, &qa.question, None, topk * 2)
                    .map(|hits| hits.into_iter().map(|(en, _)| en).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };
    if !semantic.is_empty() {
        lists.push(&semantic);
    }
    let primary = causal_memory::store::retrieve::rrf_merge_many(&lists, topk);
    let seed_ids: Vec<i64> = primary.iter().map(|e| e.edge_id).collect();
    let hop = store.search_causal_hop(&qa.question, &seed_ids, topk * 2).unwrap_or_default();
    if !hop.is_empty() {
        lists.push(&hop);
    }
    let ranked = causal_memory::store::retrieve::rrf_merge_many(&lists, topk);

    let mut retrieved_ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in &ranked {
        for id in [&e.decision_id, &e.outcome_id] {
            if seen.insert(id.clone()) {
                retrieved_ids.push(id.clone());
            }
        }
    }
    // Evidence hit: any gold node's anchor chunk retrieved.
    let gold_ids: Vec<String> = qa
        .evidence_nodes
        .iter()
        .filter_map(|n| evidence_ids.get(n).cloned())
        .collect();
    let evidence_hit = gold_ids.iter().any(|gid| retrieved_ids.contains(gid));

    if search_only {
        return ResultRow {
            graph: g.id,
            category: qa.category,
            question: qa.question.clone(),
            gold: qa.answer.clone(),
            predicted: String::new(),
            verdict: "search_only".into(),
            evidence_hit,
            retrieved_ids,
        };
    }

    // Answer.
    let mut memory_lines = Vec::new();
    let mut seen2 = std::collections::HashSet::new();
    for e in &ranked {
        for (id, text) in [(&e.decision_id, &e.decision_text), (&e.outcome_id, &e.outcome_text)] {
            if seen2.insert(id.clone()) {
                memory_lines.push(format!("- {text}"));
            }
        }
    }
    let memories = if memory_lines.is_empty() { "(no memories retrieved)".to_string() } else { memory_lines.join("
") };
    let answer_user = format!("Memories:\n{memories}\n\nQuestion: {}\nAnswer:", qa.question);
    let raw = match chat(cfg, ANSWER_SYSTEM, &answer_user, 400, false).await {
        Ok(s) => s,
        Err(e) => {
            return ResultRow {
                graph: g.id,
                category: qa.category,
                question: qa.question.clone(),
                gold: qa.answer.clone(),
                predicted: String::new(),
                verdict: "error".into(),
                evidence_hit,
                retrieved_ids,
            };
        }
    };
    let predicted = raw.rsplit("ANSWER:").next().unwrap_or(&raw).trim().to_string();

    // Judge (with retry + JSON mode).
    let judge_user = format!(
        "Question: {}\nGold answer: {}\nPredicted answer: {}\n\nThe prediction is \"correct\" if it conveys the same information as the gold answer (wording may differ); otherwise \"incorrect\".",
        qa.question, qa.answer, predicted
    );
    let mut verdict = "error".to_string();
    let mut reason = String::new();
    for attempt in 0..3 {
        let u = if attempt == 0 {
            judge_user.clone()
        } else {
            format!("{judge_user}\n\nRespond with ONLY the JSON object. No other text.")
        };
        match chat(cfg, JUDGE_SYSTEM, &u, 512, true).await {
            Ok(rawj) => {
                if let Some(v) = extract_json(&rawj).filter(|v| v.get("verdict").is_some()) {
                    if let Some(vd) = v.get("verdict").and_then(|x| x.as_str()) {
                        let vd = vd.to_lowercase();
                        if vd == "correct" || vd == "incorrect" {
                            verdict = vd;
                            reason = v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            break;
                        }
                    }
                }
            }
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
    }
    let _ = reason;
    ResultRow {
        graph: g.id,
        category: qa.category,
        question: qa.question.clone(),
        gold: qa.answer.clone(),
        predicted,
        verdict,
        evidence_hit,
        retrieved_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphs_are_deterministic() {
        let a = generate_graph(0, 42);
        let b = generate_graph(0, 42);
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }

    #[test]
    fn graphs_have_causal_structure() {
        let g = generate_graph(0, 42);
        // chain depth ≥ 2 (C1), prevented edge (C4), twin pair (C6)
        let relations: Vec<&str> = g.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(relations.contains(&"prevented"));
        assert!(g.nodes.len() >= 6);
        // nodes carry distinct actions
        let actions: Vec<String> = g.nodes.iter().map(|n| n.action.clone()).collect();
        let uniq: std::collections::HashSet<_> = actions.iter().collect();
        assert!(uniq.len() >= 4, "actions must be distinct enough for narration");
    }

    #[test]
    fn questions_are_generated_per_class() {
        let g = generate_graph(0, 42);
        for class in 11..=17 {
            let q = question_for(&g, class).expect("every class must produce a question");
            assert!(!q.answer.is_empty());
            assert!(!q.evidence_nodes.is_empty());
        }
    }

    #[test]
    fn rng_is_reproducible() {
        let mut r = Rng::new(7);
        let seq1: Vec<u64> = (0..5).map(|_| r.next()).collect();
        let mut r2 = Rng::new(7);
        let seq2: Vec<u64> = (0..5).map(|_| r2.next()).collect();
        assert_eq!(seq1, seq2);
    }
}
