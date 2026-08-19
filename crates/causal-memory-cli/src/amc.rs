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
//! Retrieval fuses two signals when an embedder is available (built with
//! `--features local-embed` for the offline fastembed ONNX backend, or an
//! HTTP embedding endpoint via env): the shared-prefix lexical score and
//! cosine similarity, merged by reciprocal rank fusion. Without an
//! embedder it degrades gracefully to lexical-only. The response schema
//! carries `score` and `created_at` on every hit.
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
use causal_memory::embed::{UnifiedEmbedder, blob_to_vec, cosine_similarity, vec_to_blob};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

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
                event_time INTEGER,
                embedding BLOB
            );
            CREATE INDEX IF NOT EXISTS idx_amc_user ON amc_memories(user_id);",
        )?;
        // v2: existing DBs gain the embedding column.
        let has_embed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('amc_memories') WHERE name = 'embedding'",
            [],
            |r| r.get(0),
        )?;
        if has_embed == 0 {
            conn.execute_batch("ALTER TABLE amc_memories ADD COLUMN embedding BLOB")?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Persist one Add request's messages. Returns only after every message
    /// is committed — the contract's "stored and available to Search".
    /// `embeddings` are aligned with `messages` (None per message when the
    /// embedder is unavailable).
    fn add(
        &self,
        user_id: &str,
        session_id: &str,
        messages: &[AddMessage],
        embeddings: &[Option<Vec<f32>>],
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("store lock: {e}"))?;
        let mut stmt = conn.prepare(
            "INSERT INTO amc_memories (user_id, session_id, role, content, event_time, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (msg, emb) in messages.iter().zip(embeddings) {
            stmt.execute(params![
                user_id,
                session_id,
                msg.role,
                msg.content,
                msg.timestamp,
                emb.as_deref().map(vec_to_blob),
            ])?;
        }
        Ok(())
    }

    /// All memories under one user_id, oldest first (stable search input).
    fn memories_for(&self, user_id: &str) -> Result<Vec<MemoryRow>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("store lock: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, content, event_time, embedding FROM amc_memories
             WHERE user_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, content, event_time, blob) =
                row.map_err(|e| anyhow::anyhow!("row read: {e}"))?;
            // Decode outside the closure — anyhow errors can't live inside a
            // rusqlite row mapper.
            let embedding = match blob {
                Some(b) => match blob_to_vec(&b) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!("amc: stored embedding decode failed for mem_{id}: {e}");
                        None
                    }
                },
                None => None,
            };
            out.push(MemoryRow {
                id,
                content,
                event_time,
                embedding,
            });
        }
        Ok(out)
    }
}

struct MemoryRow {
    id: i64,
    content: String,
    event_time: Option<i64>,
    embedding: Option<Vec<f32>>,
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

/// Embedder shared across requests (embed() takes &mut self). None when no
/// backend is available — the server then runs lexical-only.
type SharedEmbedder = Arc<AsyncMutex<Option<UnifiedEmbedder>>>;

/// BGE retrieval instruction: bge-en-v1.5 expects queries prefixed, passages
/// unprefixed (BAAI recommended usage).
const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

async fn handle_add(
    State(state): State<(Arc<AmcStore>, SharedEmbedder)>,
    Json(req): Json<AddRequest>,
) -> Result<Json<AddResponse>, (axum::http::StatusCode, String)> {
    // Embed every message when a backend is available (one call per chunk;
    // the tokio mutex is fine to hold across await).
    let mut embeddings: Vec<Option<Vec<f32>>> = Vec::with_capacity(req.messages.len());
    {
        let mut guard = state.1.lock().await;
        if let Some(embedder) = guard.as_mut() {
            for msg in &req.messages {
                match embedder.embed(&msg.content).await {
                    Ok(v) => embeddings.push(Some(v)),
                    Err(e) => {
                        eprintln!("add: embed failed (falling back to lexical): {e}");
                        embeddings.push(None);
                    }
                }
            }
        } else {
            embeddings.extend(std::iter::repeat_n(None, req.messages.len()));
        }
    }
    state
        .0
        .add(&req.user_id, &req.session_id, &req.messages, &embeddings)
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
    State(state): State<(Arc<AmcStore>, SharedEmbedder)>,
    Json(req): Json<SearchRequest>,
) -> Json<SearchResponse> {
    // `options` is contract-fidelity input (choice questions): the platform's
    // answer model receives the memories; options do not change retrieval.
    let _ = &req.options;
    let rows = match state.0.memories_for(&req.user_id) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("search: store read failed: {e}");
            return Json(SearchResponse { data: Vec::new() });
        }
    };
    if rows.is_empty() {
        return Json(SearchResponse { data: Vec::new() });
    }

    // Signal 1 — morphology-robust, IDF-weighted lexical scores (per-user corpus).
    let query_tokens = causal_memory::patterns::tokenize(&req.query);
    if query_tokens.is_empty() && req.query.trim().is_empty() {
        // Contract requires a query, but guard anyway: most recent first.
        let hits: Vec<SearchHit> = rows
            .iter()
            .rev()
            .take(req.top_k)
            .map(|r| SearchHit {
                id: format!("mem_{}", r.id),
                content: r.content.clone(),
                score: 0.0,
                created_at: r.event_time.map(iso_time),
            })
            .collect();
        return Json(SearchResponse { data: hits });
    }
    let doc_tokens: Vec<Vec<String>> = rows
        .iter()
        .map(|r| causal_memory::patterns::tokenize(&r.content))
        .collect();
    let idfs = token_idfs(&query_tokens, &doc_tokens);
    let lex_scores: Vec<(i64, f64)> = rows
        .iter()
        .zip(&doc_tokens)
        .map(|(r, dt)| (r.id, lexical_score(&query_tokens, dt, &idfs)))
        .collect();

    let mut signals: Vec<Vec<(i64, usize)>> = Vec::new();
    // Lexical rank list (score-descending; ties → newer id first).
    let mut lex_ranked = lex_scores;
    lex_ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.0.cmp(&a.0))
    });
    signals.push(
        lex_ranked
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i + 1))
            .collect(),
    );

    // Signal 2 — semantic cosine, when an embedder AND stored vectors exist.
    let query_vec: Option<Vec<f32>> = {
        let mut guard = state.1.lock().await;
        match guard.as_mut() {
            Some(embedder) => {
                embedder
                    .embed(&format!("{BGE_QUERY_PREFIX}{}", req.query))
                    .await
                    .ok()
            }
            None => None,
        }
    };
    if let Some(qv) = query_vec {
        let mut sem: Vec<(i64, f64)> = rows
            .iter()
            .map(|r| {
                let sim = r
                    .embedding
                    .as_ref()
                    .map(|dv| cosine_similarity(&qv, dv).max(0.0))
                    .unwrap_or(0.0);
                (r.id, sim)
            })
            .collect();
        sem.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.0.cmp(&a.0))
        });
        signals.push(
            sem.iter()
                .enumerate()
                .map(|(i, (id, _))| (*id, i + 1))
                .collect(),
        );
    }

    // Fuse by reciprocal rank — the contract promises RRF (see module docs).
    // Fused ids always come from `rows`, so every id resolves.
    let fused = rrf_fuse(&signals);
    let hits: Vec<SearchHit> = fused
        .iter()
        .take(req.top_k)
        .filter_map(|(id, score)| {
            rows.iter().find(|r| r.id == *id).map(|row| SearchHit {
                id: format!("mem_{}", row.id),
                content: row.content.clone(),
                score: *score,
                created_at: row.event_time.map(iso_time),
            })
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

/// Adaptive prefix threshold: words match on ≥4 shared chars; short tokens
/// (git, vim, api, ide) match at their own length — a 3-char token can never
/// share 4 chars, so the old flat rule silently dropped them from lexical
/// recall and left them to the small embedder alone.
fn prefix_threshold(a: &str, b: &str) -> usize {
    4.min(a.len().min(b.len()))
}

fn token_prefix_hit(q: &str, d: &str) -> bool {
    shared_prefix_len(q, d) >= prefix_threshold(q, d)
}

/// Per-query-token inverse document frequency over the corpus (prefix-aware
/// document frequency). Common tokens ("code", "build") match many memories
/// and must not drown the discriminative ones; rare tokens carry ranking.
/// idf = ln((N+1)/(df+1)), floored at 0.3 so a common-but-present token
/// still contributes.
fn token_idfs(query_tokens: &[String], docs: &[Vec<String>]) -> Vec<f64> {
    let n = docs.len() as f64;
    query_tokens
        .iter()
        .map(|q| {
            let df = docs
                .iter()
                .filter(|toks| toks.iter().any(|t| token_prefix_hit(q, t)))
                .count() as f64;
            ((n + 1.0) / (df + 1.0)).ln().max(0.3)
        })
        .collect()
}

/// Morphology-robust, IDF-weighted relevance: for each query token, how many
/// document tokens share an adaptive-length prefix, weighted by the token's
/// inverse document frequency. Score grows with matched query tokens
/// (ln-weighted for repeated hits) — ordering is what the contract needs.
fn lexical_score(query_tokens: &[String], doc_tokens: &[String], idfs: &[f64]) -> f64 {
    let mut score = 0.0;
    for (q, idf) in query_tokens.iter().zip(idfs) {
        let hits = doc_tokens.iter().filter(|d| token_prefix_hit(q, d)).count();
        if hits > 0 {
            score += idf * (1.0 + (hits as f64).ln());
        }
    }
    score
}

/// Reciprocal rank fusion over per-signal rank lists `(doc_id, rank)`:
/// fused = Σ 1/(k + rank). Rank-based, so the unbounded lexical score and
/// the [-1, 1] cosine never fight over scale — the module contract promises
/// RRF; this is the implementation.
const RRF_K: f64 = 60.0;

fn rrf_fuse(signals: &[Vec<(i64, usize)>]) -> Vec<(i64, f64)> {
    let mut acc: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    for signal in signals {
        for (id, rank) in signal {
            *acc.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + *rank as f64);
        }
    }
    let mut out: Vec<(i64, f64)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
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

fn build_app(store: Arc<AmcStore>, embedder: SharedEmbedder) -> Router {
    Router::new()
        .route("/add", post(handle_add))
        .route("/search", post(handle_search))
        .route("/health", get(handle_health))
        .with_state((store, embedder))
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
    // Semantic backend: HTTP embeddings via env, else local ONNX
    // (fastembed, offline) when built with --features local-embed.
    let embedder: SharedEmbedder = Arc::new(AsyncMutex::new(causal_memory::embed::init_embedder()));
    {
        let guard = embedder.blocking_lock();
        match guard.as_ref() {
            Some(e) => println!("causal-memory-amc embedding: {} (fused retrieval)", e.model()),
            None => println!("causal-memory-amc embedding: none (lexical-only; build with --features local-embed for semantic fusion)"),
        }
    }
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    println!("causal-memory-amc listening on http://{addr} (db: {db})");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // Bind with the tokio (non-blocking) listener directly — wrapping a
        // std blocking socket panics in recent tokio versions.
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
        axum::serve(listener, build_app(store, embedder))
            .await
            .map_err(|e| anyhow::anyhow!("server error: {e}"))
    })
}

// ─── Contract self-test ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lexical_scoring_morphology_and_short_tokens() {
        // Question morphology must match memory morphology — one rule, no
        // stemmer list: shared prefix ≥ 4 chars.
        assert_eq!(shared_prefix_len("editor", "editing"), 4);
        assert_eq!(shared_prefix_len("deployment", "deploy"), 6);
        assert_eq!(shared_prefix_len("preference", "preferences"), 10);
        assert!(shared_prefix_len("vim", "vivid") < 4);
        // Short tokens now match at their own length — the old flat >=4 rule
        // could never match 3-char tokens (git/vim/api), so they had no
        // lexical recall and leaned entirely on the small embedder.
        assert!(token_prefix_hit("vim", "vim"));
        assert!(token_prefix_hit("vim", "vimmer"));
        assert!(!token_prefix_hit("vim", "visual"));
        assert!(token_prefix_hit("git", "github"));
        // Scoring with unit IDF: morphology + exact.
        let q = crate_patterns_tokens("which editor does the user prefer");
        let doc = crate_patterns_tokens("I prefer vim for editing all my work");
        let idfs = vec![1.0; q.len()];
        assert_eq!(lexical_score(&q, &doc, &idfs), 2.0); // prefer + editor↔editing
        // Irrelevant doc scores zero.
        let other = crate_patterns_tokens("the cat is named Luna");
        assert_eq!(lexical_score(&q, &other, &idfs), 0.0);
    }

    #[test]
    fn test_idf_weights_rare_tokens() {
        // A rare discriminative token must outweigh a common one: the doc
        // holding the rare token ranks above the doc holding the common one.
        let q = vec!["code".into(), "zephyr".into()];
        let docs = vec![
            vec!["code".into(), "build".into()],
            vec!["code".into(), "debug".into()],
            vec!["zephyr".into(), "helm".into()],
        ];
        let idfs = token_idfs(&q, &docs);
        assert!(idfs[1] > idfs[0], "rare zephyr must weigh more: {idfs:?}");
        let common = lexical_score(&q, &docs[0], &idfs);
        let rare = lexical_score(&q, &docs[2], &idfs);
        assert!(rare > common, "zephyr doc must outrank code doc: {common} vs {rare}");
    }

    #[test]
    fn test_rrf_fusion_prefers_rank_over_scale() {
        // C: strong lexical (#1) + mid semantic (#2). B: strong semantic (#1)
        // + weak lexical (#4). RRF must order C > B > A even though raw
        // lexical scores would put C far ahead of B.
        let signals = vec![
            vec![(3, 1), (1, 2), (4, 3), (2, 4)], // lexical ranks
            vec![(2, 1), (3, 2), (1, 3), (4, 4)], // semantic ranks
        ];
        let fused = rrf_fuse(&signals);
        let order: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![3, 2, 1, 4], "got {order:?}");

        // Zero-lexical high-cosine doc still beats a weak lexical-only doc.
        let signals2 = vec![
            vec![(9, 1), (8, 2), (7, 3), (6, 4)], // lexical: 9 best
            vec![(6, 1), (9, 4), (7, 2), (8, 3)], // semantic: 6 best, 9 worst
        ];
        let fused2 = rrf_fuse(&signals2);
        let first2 = fused2[0].0;
        assert!(first2 == 6 || first2 == 9);
        let score6 = fused2.iter().find(|(id, _)| *id == 6).map(|(_, s)| *s).unwrap();
        let score8 = fused2.iter().find(|(id, _)| *id == 8).map(|(_, s)| *s).unwrap();
        assert!(score6 > score8, "semantic-only #1 must beat weak lexical: {score6} vs {score8}");
    }

    fn crate_patterns_tokens(text: &str) -> Vec<String> {
        causal_memory::patterns::tokenize(text)
    }



    fn test_store() -> Arc<AmcStore> {
        Arc::new(AmcStore::open(":memory:").unwrap())
    }

    fn test_embedder() -> SharedEmbedder {
        Arc::new(AsyncMutex::new(None))
    }

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
        let app = build_app(store, test_embedder());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = test_client();
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
        let app = build_app(store, test_embedder());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = test_client();
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

    #[tokio::test]
    async fn search_recalls_short_token_and_morphology_gold() {
        // "Text recall" regression: the gold memory must surface in top-k even
        // when (a) the query's discriminative token is short ("git", 3 chars —
        // the old >=4-char prefix rule could never lexical-match it) and (b)
        // the corpus is noisy with heavy keyword overlap.
        let store = test_store();
        let app = build_app(store, test_embedder()); // lexical-only path
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = test_client();
        let base = format!("http://{addr}");
        wait_ready(&client, &base).await;

        let msgs: Vec<serde_json::Value> = vec![
            "the team uses code reviews for every build",
            "build times improved after the code refactor",
            "code quality gates run on each build",
            "build pipeline caches code artifacts",
            "the user prefers git for version control", // gold
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

        // Short discriminative token: git is 3 chars — the gold must still be
        // first (old flat >=4 rule scored it 0 and it sank to the bottom).
        let resp: SearchResponse = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({"query": "version control with git", "user_id": "u1", "top_k": 3}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            resp.data[0].content.contains("git"),
            "short-token gold must rank first: {:?}",
            resp.data.iter().map(|h| h.content.as_str()).collect::<Vec<_>>()
        );

        // Morphology + IDF: "prefers" matches "prefer" via shared prefix and
        // the rare "control" token outweighs the noisy "code/build" overlap.
        let resp2: SearchResponse = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({"query": "which editor does the user prefer", "user_id": "u1", "top_k": 3}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            resp2.data[0].content.contains("git"),
            "morphology gold must rank first: {:?}",
            resp2.data.iter().map(|h| h.content.as_str()).collect::<Vec<_>>()
        );

        server.abort();
    }

}
