//! Agent Memory Challenge (AMC/01) integration server.
//!
//! A thin HTTP frontend over the shared memory facade
//! (`causal_memory::memory::Memory`) — the same pipeline the MCP server
//! (stdio + HTTP) and the Python bindings run. No private store, no
//! private scoring: the AMC leaderboard exercises the production system.
//!
//!   POST /add     — write a memory batch   → `Memory::remember` (distill
//!                   mode) or `Memory::remember_raw_turns` (raw mode)
//!   POST /search  — fused retrieval        → `Memory::search_memory_entries`
//!   GET  /health  — liveness probe
//!
//! Contract rules this server honors:
//! - `user_id` is the retrieval isolation boundary: one `Memory` (one
//!   SQLite db) per user; Search only ever sees that user's store.
//! - Add returns HTTP 200 only after the messages are durably stored and
//!   searchable (synchronous write, no background queue).
//! - Search returns raw memory evidence only — it never generates answers.
//! - Results are ordered most-relevant first (RRF fusion rank); `top_k`
//!   is respected. Every hit carries `score` and `created_at`.
//!
//! Write modes (`--write-mode`):
//! - `distill` (default): full production pipeline — LLM extracts
//!   facts/lessons/causal edges; write-time gatekeeping (LLM extraction is
//!   the sole path into the retrieval pool). Requires CAUSAL_MEMORY_LLM_*
//!   env; degrades to `raw` with a warning when absent.
//! - `raw`: pre-gatekeeping baseline — raw turns enter the retrieval pool
//!   directly (what the v0.3 leaderboard entry did). No write-time LLM.
//!   Both modes share the same retrieval stack, so A/B isolates the value
//!   of write-time distillation.
//!
//! Usage:
//!   cargo build --release --bin causal-memory-amc
//!   ./target/release/causal-memory-amc --db-dir amc_data --port 8787 \
//!       --write-mode distill
//!
//! Self-test: `cargo test -p causal-memory-cli --bin causal-memory-amc`
//! spins the server on an ephemeral port and runs Add → Search round-trips.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use causal_memory::memory::Memory;
use serde::{Deserialize, Serialize};

// ─── Per-user memory registry ──────────────────────────────────────────────

/// One `Memory` (one SQLite db file) per `user_id` — physical isolation,
/// the contract's retrieval boundary. Opened lazily on first sight.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    Distill,
    Raw,
}

struct UserMemories {
    dir: PathBuf,
    mode: WriteMode,
    users: RwLock<HashMap<String, Arc<Memory>>>,
}

/// Lock a RwLock ignoring poisoning — registry writes can't panic, so a
/// poisoned guard only means some other thread panicked elsewhere; the map
/// is still structurally valid.
fn poison_read<'a, T>(lock: &'a RwLock<T>) -> std::sync::RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn poison_write<'a, T>(lock: &'a RwLock<T>) -> std::sync::RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

impl UserMemories {
    fn new(dir: PathBuf, mode: WriteMode) -> Self {
        Self {
            dir,
            mode,
            users: RwLock::new(HashMap::new()),
        }
    }

    /// Filesystem-safe db name per user (defensive: user ids are external
    /// input; never let them escape the db dir).
    fn db_path(&self, user_id: &str) -> PathBuf {
        let safe: String = user_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let hashed = format!("{:x}", fnv1a(user_id.as_bytes()));
        self.dir.join(format!("{safe}.{hashed}.db"))
    }

    fn get(&self, user_id: &str) -> Result<Arc<Memory>> {
        if let Some(m) = poison_read(&self.users).get(user_id) {
            return Ok(Arc::clone(m));
        }
        let mut guard = poison_write(&self.users);
        if let Some(m) = guard.get(user_id) {
            return Ok(Arc::clone(m));
        }
        let path = self.db_path(user_id);
        let memory = Arc::new(Memory::open(&path)?);
        guard.insert(user_id.to_string(), Arc::clone(&memory));
        Ok(memory)
    }
}

/// FNV-1a — tiny stable hash for collision-resistant file names.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ─── Request / response schema (contract-frozen) ───────────────────────────

#[derive(Debug, Deserialize)]
struct AddMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AddRequest {
    #[serde(default)]
    request_id: String,
    user_id: String,
    session_id: String,
    messages: Vec<AddMessage>,
}

#[derive(Serialize, Deserialize)]
struct AddResponse {
    success: bool,
    request_id: String,
    user_id: String,
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct SearchRequest {
    query: String,
    /// Choice-question options; not used for retrieval (the platform's
    /// answer model sees the memories) but accepted for contract fidelity.
    #[serde(default)]
    options: Option<Vec<String>>,
    user_id: String,
    top_k: usize,
}

#[derive(Serialize, Deserialize)]
struct SearchHit {
    id: String,
    content: String,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SearchResponse {
    data: Vec<SearchHit>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

// ─── Handlers ──────────────────────────────────────────────────────────────

async fn handle_add(
    State(users): State<Arc<UserMemories>>,
    Json(req): Json<AddRequest>,
) -> Result<Json<AddResponse>, (axum::http::StatusCode, String)> {
    if req.messages.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "no messages in add request".into(),
        ));
    }
    let memory = users
        .get(&req.user_id)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("open store: {e}")))?;

    // `remember` runs the distiller synchronously (one LLM call per batch,
    // seconds). Raw mode is a pure local write. Both return only after the
    // data is durably searchable — the contract's synchronous-add rule.
    let result = match users.mode {
        WriteMode::Distill => {
            let text: String = req
                .messages
                .iter()
                .map(|m| format!("{}: {}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            // Off the async executor: the facade blocks on the LLM call.
            let res = tokio::task::spawn_blocking(move || {
                (memory.clone(), memory.remember(&text, None))
            })
            .await
            .map_err(|e| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("add task panicked: {e}"),
                )
            })?;
            eprintln!("amc/add [{}] distill: {}", req.user_id, res.1);
            res.0
        }
        WriteMode::Raw => {
            let turns: Vec<(String, String)> = req
                .messages
                .iter()
                .map(|m| (m.role.clone(), m.content.clone()))
                .collect();
            let n = memory.remember_raw_turns(&turns, &req.session_id);
            eprintln!("amc/add [{}] raw: {n} turn(s) stored", req.user_id);
            memory
        }
    };
    let _ = result; // store handle; write already committed inside the facade

    Ok(Json(AddResponse {
        success: true,
        request_id: req.request_id,
        user_id: req.user_id,
        session_id: req.session_id,
    }))
}

async fn handle_search(
    State(users): State<Arc<UserMemories>>,
    Json(req): Json<SearchRequest>,
) -> Json<SearchResponse> {
    // `options` is contract-fidelity input (choice questions): the platform's
    // answer model receives the memories; options do not change retrieval.
    let _ = &req.options;
    let Ok(memory) = users.get(&req.user_id) else {
        return Json(SearchResponse { data: Vec::new() });
    };
    let top_k = req.top_k.max(1);
    let query = req.query.clone();
    let hits = match tokio::task::spawn_blocking(move || {
        memory.search_memory_entries(&query, None, None, top_k)
    })
    .await
    {
        Ok((hits, mode)) => {
            eprintln!(
                "amc/search [{}] {} hit(s) [{} mode]",
                req.user_id,
                hits.len(),
                mode
            );
            hits
        }
        Err(e) => {
            eprintln!("amc/search task panicked: {e}");
            Vec::new()
        }
    };
    Json(SearchResponse {
        data: hits
            .into_iter()
            .map(|h| SearchHit {
                id: h.key,
                content: h.content,
                score: h.score,
                created_at: h.created_at.map(|ts| {
                    chrono::DateTime::from_timestamp(ts, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default()
                }),
            })
            .collect(),
    })
}

async fn handle_health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

// ─── Entry point ───────────────────────────────────────────────────────────

type AppState = Arc<UserMemories>;

fn build_app(users: AppState) -> Router {
    Router::new()
        .route("/add", post(handle_add))
        .route("/search", post(handle_search))
        .route("/health", get(handle_health))
        .with_state(users)
}

fn main() -> Result<()> {
    let mut db_dir = PathBuf::from("amc_data");
    let mut port = 8787u16;
    let mut mode = WriteMode::Distill;
    let mut i = 0;
    let args: Vec<String> = std::env::args().skip(1).collect();
    while i < args.len() {
        match args[i].as_str() {
            "--db-dir" => {
                i += 1;
                db_dir = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--db-dir needs a value"))?,
                );
            }
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?
                    .parse()?;
            }
            "--write-mode" => {
                i += 1;
                let m = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--write-mode needs a value"))?;
                mode = match m.as_str() {
                    "distill" => WriteMode::Distill,
                    "raw" => WriteMode::Raw,
                    other => anyhow::bail!("--write-mode must be distill|raw (got {other})"),
                };
            }
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }
    std::fs::create_dir_all(&db_dir)?;

    // Honest degradation: distill without an LLM config would store raw
    // stubs through `remember`'s fallback — surface it and switch to raw.
    if mode == WriteMode::Distill && causal_memory::llm::LlmConfig::from_env().is_none() {
        eprintln!(
            "⚠ --write-mode distill but no LLM configured \
             (CAUSAL_MEMORY_LLM_API/KEY); falling back to raw"
        );
        mode = WriteMode::Raw;
    }

    match causal_memory::embed::init_embedder() {
        Some(e) => println!("causal-memory-amc embedding: {} (semantic layer live)", e.model()),
        None => println!("causal-memory-amc embedding: none (BM25-only retrieval)"),
    }
    let users = Arc::new(UserMemories::new(db_dir, mode));
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    println!(
        "causal-memory-amc listening on http://{addr} (write-mode: {}, one store per user_id)",
        match mode {
            WriteMode::Distill => "distill",
            WriteMode::Raw => "raw",
        }
    );
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
        axum::serve(listener, build_app(users))
            .await
            .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
        Ok(())
    })
}

// ─── Self-tests (ephemeral port, real HTTP round-trip) ─────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Test client: no keep-alive reuse. On the current-thread tokio runtime
    /// the reqwest pool can reset a reused idle connection between requests
    /// (a test-harness artifact — the server handles reuse fine, as the
    /// manual curl round-trip on the multi-thread runtime shows).
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap()
    }

    async fn wait_ready(client: &reqwest::Client, base: &str) {
        for _ in 0..100 {
            if client
                .get(format!("{base}/health"))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("server did not become ready");
    }

    async fn spawn_server(mode: WriteMode) -> (String, tokio::task::JoinHandle<()>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "amc-test-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let app = build_app(Arc::new(UserMemories::new(dir.clone(), mode)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), server, dir)
    }

    fn add_body(user: &str, session: &str, msgs: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "request_id": format!("req-{user}-{session}"),
            "user_id": user,
            "session_id": session,
            "messages": msgs.iter().map(|(r, c)| serde_json::json!({"role": r, "content": c})).collect::<Vec<_>>(),
        })
    }

    #[tokio::test]
    async fn raw_roundtrip_isolation_and_topk() {
        let (base, _server, dir) = spawn_server(WriteMode::Raw).await;
        let client = test_client();
        wait_ready(&client, &base).await;

        // Two users, disjoint content.
        for (user, fruit) in [("alice", "dragonfruit"), ("bob", "persimmon")] {
            let resp = client
                .post(format!("{base}/add"))
                .json(&add_body(
                    user,
                    "s1",
                    &[
                        ("user", "what exotic fruit did I buy last week?"),
                        ("assistant", &format!("you bought a {fruit} at the market")),
                    ],
                ))
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success());
        }

        // Isolation: alice never sees bob's fruit and vice versa.
        for (user, mine, theirs) in [("alice", "dragonfruit", "persimmon"), ("bob", "persimmon", "dragonfruit")] {
            let resp = client
                .post(format!("{base}/search"))
                .json(&serde_json::json!({
                    "query": "exotic fruit market",
                    "user_id": user,
                    "top_k": 5,
                }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap();
            let data = resp["data"].as_array().unwrap();
            assert!(!data.is_empty(), "{user} must see own memory");
            let all: String = data
                .iter()
                .map(|h| h["content"].as_str().unwrap_or_default())
                .collect();
            assert!(all.contains(mine), "{user} content missing: {all}");
            assert!(!all.contains(theirs), "isolation broken for {user}: {all}");
        }

        // top_k respected.
        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body(
                "carol",
                "s1",
                &[
                    ("user", "deploy notes"),
                    ("assistant", "carol fixed the flaky retry test by adding jitter"),
                    ("assistant", "carol moved the cache to redis cluster"),
                    ("assistant", "carol enabled pprof on the api server"),
                ],
            ))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let resp = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({"query": "carol", "user_id": "carol", "top_k": 2}))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(resp["data"].as_array().unwrap().len(), 2, "top_k=2 must bind");

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn empty_search_and_unknown_user() {
        let (base, _server, dir) = spawn_server(WriteMode::Raw).await;
        let client = test_client();
        wait_ready(&client, &base).await;
        let resp = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({"query": "anything", "user_id": "ghost", "top_k": 5}))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(resp["data"].as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn distill_mode_without_llm_still_serves() {
        // No LLM env in the test harness: the handler must not fail the add
        // (remember's own fallback stores a raw stub). The contract's
        // synchronous-searchable rule holds either way.
        std::env::remove_var("CAUSAL_MEMORY_LLM_API");
        let (base, _server, dir) = spawn_server(WriteMode::Distill).await;
        let client = test_client();
        wait_ready(&client, &base).await;
        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body("dave", "s1", &[("user", "hello there")]))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        std::fs::remove_dir_all(dir).ok();
    }
}
