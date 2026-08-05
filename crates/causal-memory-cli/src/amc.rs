//! Agent Memory Challenge (AMC/01) integration server.
//!
//! Implements the Add/Search HTTP contract of the Agent Memory Leaderboard
//! (agentmemories.ai) so causal-memory can enter the first evaluation cycle:
//!
//!   POST /add     — store one memory chunk (echo request_id/user_id/session_id)
//!   POST /search  — return ordered memory evidence for a question
//!   GET  /health  — liveness probe
//!
//! Contract rules this server honors:
//! - `user_id` is the retrieval isolation boundary: Search only ever returns
//!   memories written under the SAME user_id.
//! - Add returns HTTP 200 only after the messages are durably stored and
//!   searchable (synchronous insert, no background queue).
//! - Search returns raw memory evidence only — it never generates answers.
//! - Results are ordered most-relevant first; `top_k` is respected.
//!
//! Retrieval is BM25 over the raw memory text (`crate::bm25`), the same
//! tokenizer/ranker the benchmark harnesses use. Embedding fusion is a
//! planned upgrade; the response schema already carries `score` and
//! `created_at` so fused hits slot in without a contract change.
//!
//! Usage:
//!   cargo build --release --bin causal-memory-amc
//!   ./target/release/causal-memory-amc --db amc.db --port 8787
//!
//! Self-test: `cargo test -p causal-memory-cli --bin causal-memory-amc`
//! spins the server on an ephemeral port and runs an Add → Search round-trip.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

// ─── Store ─────────────────────────────────────────────────────────────────

struct AmcStore {
    conn: Mutex<Connection>,
}

impl AmcStore {
    fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS amc_memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                event_time INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_amc_user ON amc_memories(user_id);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Persist one Add request's messages. Returns only after every message
    /// is committed — the contract's "stored and available to Search".
    fn add(&self, user_id: &str, session_id: &str, messages: &[AddMessage]) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("store lock: {e}"))?;
        let mut stmt = conn.prepare(
            "INSERT INTO amc_memories (user_id, session_id, role, content, event_time)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for msg in messages {
            stmt.execute(params![
                user_id,
                session_id,
                msg.role,
                msg.content,
                msg.timestamp,
            ])?;
        }
        Ok(())
    }

    /// All memories under one user_id, oldest first (stable search input).
    fn memories_for(&self, user_id: &str) -> Result<Vec<MemoryRow>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("store lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, content, event_time FROM amc_memories
             WHERE user_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                content: r.get(1)?,
                event_time: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| anyhow::anyhow!("row read: {e}"))?);
        }
        Ok(out)
    }
}

struct MemoryRow {
    id: i64,
    content: String,
    event_time: Option<i64>,
}

// ─── Contract types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AddMessage {
    role: String,
    content: String,
    #[serde(default)]
    timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AddRequest {
    request_id: String,
    messages: Vec<AddMessage>,
    user_id: String,
    session_id: String,
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
    State(state): State<Arc<AmcStore>>,
    Json(req): Json<AddRequest>,
) -> Result<Json<AddResponse>, (axum::http::StatusCode, String)> {
    state
        .add(&req.user_id, &req.session_id, &req.messages)
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("storage failed: {e}"),
            )
        })?;
    Ok(Json(AddResponse {
        success: true,
        request_id: req.request_id,
        user_id: req.user_id,
        session_id: req.session_id,
    }))
}

async fn handle_search(
    State(state): State<Arc<AmcStore>>,
    Json(req): Json<SearchRequest>,
) -> Json<SearchResponse> {
    // `options` is contract-fidelity input (choice questions): the platform's
    // answer model receives the memories; options do not change retrieval.
    let _ = &req.options;
    let rows = match state.memories_for(&req.user_id) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("search: store read failed: {e}");
            return Json(SearchResponse { data: Vec::new() });
        }
    };
    if rows.is_empty() {
        return Json(SearchResponse { data: Vec::new() });
    }

    // Morphology-robust prefix scoring over this user's memory text
    // (per-user corpus → tractable size; no embedding model required).
    let query_tokens = causal_memory::patterns::tokenize(&req.query);
    let mut scored: Vec<(f64, &MemoryRow)> = rows
        .iter()
        .map(|r| {
            (
                prefix_score(&query_tokens, &causal_memory::patterns::tokenize(&r.content)),
                r,
            )
        })
        .collect();
    // Most relevant first; score ties → newer memory first (stable).
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.id.cmp(&a.1.id))
    });
    let hits: Vec<SearchHit> = scored
        .iter()
        .filter(|(score, _)| !query_tokens.is_empty() || *score > 0.0 || true)
        .take(req.top_k)
        .map(|(score, r)| SearchHit {
            id: format!("mem_{}", r.id),
            content: r.content.clone(),
            score: *score,
            created_at: r.event_time.map(iso_time),
        })
        .collect();

    Json(SearchResponse { data: hits })
}

async fn handle_health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Shared-prefix length of two tokens (0 when they diverge immediately).
///
/// The crate's tokenizer does no stemming, so the platform's questions
/// ("Which editor does the user prefer?") never match memory morphology
/// ("...vim for editing"). Instead of a fragile suffix stemmer, retrieval
/// scores on shared prefixes: "editor"/"editing" share "edit" (≥4 chars) and
/// match; "deployment"/"deploy" share 6. A single rule, no morphology list.
fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .count()
}

/// Morphology-robust relevance: for each query token, how many document
/// tokens share a ≥4-char prefix. Score grows with matched query tokens
/// (ln-weighted for repeated hits) — ordering is what the contract needs.
fn prefix_score(query_tokens: &[String], doc_tokens: &[String]) -> f64 {
    let mut score = 0.0;
    for q in query_tokens {
        let hits = doc_tokens
            .iter()
            .filter(|d| shared_prefix_len(q, d) >= 4)
            .count();
        if hits > 0 {
            score += 1.0 + (hits as f64).ln();
        }
    }
    score
}

/// Unix-millis timestamp → RFC3339 (contract example format).
fn iso_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let nanos = (ms.rem_euclid(1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

// ─── Entry point ───────────────────────────────────────────────────────────

fn build_app(store: Arc<AmcStore>) -> Router {
    Router::new()
        .route("/add", post(handle_add))
        .route("/search", post(handle_search))
        .route("/health", get(handle_health))
        .with_state(store)
}

fn main() -> Result<()> {
    let mut db = String::from("amc.db");
    let mut port = 8787u16;
    let mut i = 0;
    let args: Vec<String> = std::env::args().skip(1).collect();
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                db = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--db needs a value"))?
                    .clone();
            }
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?
                    .parse()?;
            }
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }

    let store = Arc::new(AmcStore::open(&db)?);
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    println!("causal-memory-amc listening on http://{addr} (db: {db})");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // Bind with the tokio (non-blocking) listener directly — wrapping a
        // std blocking socket panics in recent tokio versions.
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
        axum::serve(listener, build_app(store))
            .await
            .map_err(|e| anyhow::anyhow!("server error: {e}"))
    })
}

// ─── Contract self-test ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_scoring_handles_morphology() {
        // Question morphology must match memory morphology — one rule, no
        // stemmer list: shared prefix ≥ 4 chars.
        assert_eq!(shared_prefix_len("editor", "editing"), 4);
        assert_eq!(shared_prefix_len("deployment", "deploy"), 6);
        assert_eq!(shared_prefix_len("preference", "preferences"), 10);
        assert!(shared_prefix_len("vim", "vivid") < 4);
        // Scoring: a query token matches via shared prefix.
        let q = crate_patterns_tokens("which editor does the user prefer");
        let doc = crate_patterns_tokens("I prefer vim for editing all my work");
        assert_eq!(prefix_score(&q, &doc), 2.0); // prefer + editor↔editing
        // Irrelevant doc scores zero.
        let other = crate_patterns_tokens("the cat is named Luna");
        assert_eq!(prefix_score(&q, &other), 0.0);
    }

    fn crate_patterns_tokens(text: &str) -> Vec<String> {
        causal_memory::patterns::tokenize(text)
    }

    fn test_store() -> Arc<AmcStore> {
        Arc::new(AmcStore::open(":memory:").unwrap())
    }

    /// Poll /health until the accept loop is actually serving — the spawned
    /// server task may not have started when the first request fires.
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

    #[tokio::test]
    async fn add_search_roundtrip_and_isolation() {
        let store = test_store();
        let app = build_app(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        wait_ready(&client, &base).await;

        // Health.
        let health = client.get(format!("{base}/health")).send().await.unwrap();
        assert!(health.status().is_success());

        // Add memory for user A about vim.
        let add_a = client
            .post(format!("{base}/add"))
            .json(&serde_json::json!({
                "request_id": "eval:run1:conv-0:chunk-0",
                "messages": [{"role": "user", "timestamp": 1704067200000i64, "content": "I prefer the vim editor for all my work"}],
                "user_id": "eval:run1:conv-0",
                "session_id": "eval:run1:sample:0"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(add_a.status(), 200);
        let echo: AddResponse = add_a.json().await.unwrap();
        assert!(echo.success);
        assert_eq!(echo.request_id, "eval:run1:conv-0:chunk-0");
        assert_eq!(echo.user_id, "eval:run1:conv-0");
        assert_eq!(echo.session_id, "eval:run1:sample:0");

        // Add memory for user B (different topic) — must not leak.
        let _ = client
            .post(format!("{base}/add"))
            .json(&serde_json::json!({
                "request_id": "eval:run1:conv-1:chunk-0",
                "messages": [{"role": "user", "content": "We deploy everything on Kubernetes clusters"}],
                "user_id": "eval:run1:conv-1",
                "session_id": "eval:run1:sample:1"
            }))
            .send()
            .await
            .unwrap();

        // Search A for the vim topic → A's memory, NOT B's.
        let search = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "Which editor does the user prefer?",
                "user_id": "eval:run1:conv-0",
                "top_k": 10
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(search.status(), 200);
        let resp: SearchResponse = search.json().await.unwrap();
        assert!(!resp.data.is_empty(), "A's memory must be retrieved");
        assert!(resp.data[0].content.contains("vim"));
        assert!(resp.data[0].id.starts_with("mem_"));
        assert!(resp.data[0].score > 0.0);
        assert_eq!(
            resp.data[0].created_at.as_deref(),
            Some("2024-01-01T00:00:00+00:00")
        );
        assert!(
            resp.data.iter().all(|h| !h.content.contains("Kubernetes")),
            "user_id isolation must hold"
        );

        // Search B → only B's memory.
        let search_b = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "deployment platform",
                "user_id": "eval:run1:conv-1",
                "top_k": 10
            }))
            .send()
            .await
            .unwrap();
        let resp_b: SearchResponse = search_b.json().await.unwrap();
        assert!(resp_b.data.iter().any(|h| h.content.contains("Kubernetes")));

        // Unknown user → empty.
        let search_c = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "anything",
                "user_id": "eval:run1:conv-999",
                "top_k": 10
            }))
            .send()
            .await
            .unwrap();
        let resp_c: SearchResponse = search_c.json().await.unwrap();
        assert!(resp_c.data.is_empty());

        server.abort();
    }

    #[tokio::test]
    async fn search_orders_by_relevance_and_respects_top_k() {
        let store = test_store();
        let app = build_app(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        wait_ready(&client, &base).await;

        let msgs: Vec<serde_json::Value> = vec![
            "user's favorite editor is vim",
            "the user's cat is named Luna",
            "user switched to oat milk in coffee",
            "user uses vim keybindings in the terminal",
        ]
        .into_iter()
        .map(|c| serde_json::json!({"role": "user", "content": c}))
        .collect();
        let _ = client
            .post(format!("{base}/add"))
            .json(&serde_json::json!({
                "request_id": "r:chunk-0",
                "messages": msgs,
                "user_id": "u1",
                "session_id": "s1"
            }))
            .send()
            .await
            .unwrap();

        // Top-2 for a vim query: the vim memory must be first, only 2 hits.
        let resp: SearchResponse = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({"query": "editor vim", "user_id": "u1", "top_k": 2}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp.data.len(), 2, "top_k must cap results");
        assert!(
            resp.data[0].content.contains("vim"),
            "most relevant first: got {:?}",
            resp.data[0].content
        );
        assert!(
            resp.data[0].score >= resp.data[1].score,
            "scores must be non-increasing"
        );

        server.abort();
    }
}
