//! Agent Memory Challenge (AMC/01) integration server.
//!
//! A thin HTTP frontend over the shared memory facade
//! (`causal_memory::memory::Memory`) — the same pipeline the MCP server
//! (stdio + HTTP) and the Python bindings run. No private store, no
//! private scoring: the AMC leaderboard exercises the production system.
//!
//!   POST /add     — write a memory batch   → `Memory::remember` (distill
//!                   mode) or `Memory::remember_raw_turns_with_request_id`
//!                   (raw mode)
//!   POST /search  — fused retrieval        → the path `AMC_RETRIEVAL` picks
//!                   (see `RetrievalMode`)
//!   GET  /health  — liveness + the live embedding model (`null` = the
//!                   semantic leg is down for this process; the startup log
//!                   carries a matching WARN)
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
//! Retrieval paths (`AMC_RETRIEVAL`, default `spread`):
//! - `spread`: the unified spreading-activation engine the MCP tools run —
//!   facts + causal lessons, graph-ranked.
//! - `fused`: `Memory::search_chunks_fused` — BM25 ⊕ chunk-vector RRF over
//!   raw chunk text, no graph. Returns the ingested passage itself.
//! - `merge`: both, fused by RRF (keys deduped).
//! Compare the arms with `bench-amc-ab`.
//!
//! Usage:
//!   cargo build --release --bin causal-memory-amc
//!   ./target/release/causal-memory-amc --db-dir amc_data --port 8787 \
//!       --write-mode distill
//!   AMC_RETRIEVAL=fused ./target/release/causal-memory-amc ...
//!
//! A/B harness: `cargo run --release --bin causal-memory-amc-ab`
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
use causal_memory::memory::ops::MemoryHit;
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

/// Which retrieval path `/search` runs. A/B switch (`AMC_RETRIEVAL`), with
/// the conservative default kept until the harness has data:
/// - `spread` (default): the unified spreading-activation engine the MCP
///   tools use — the production system, unchanged.
/// - `fused`: `Memory::search_chunks_fused` — BM25 ⊕ chunk-vector RRF over
///   raw chunk text, no graph.
/// - `merge`: both, fused again by RRF (keys deduped) — the "why not both"
///   arm, at ~2× the retrieval cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RetrievalMode {
    Spread,
    Fused,
    Merge,
}

impl RetrievalMode {
    /// Parse `AMC_RETRIEVAL`. Unset or unrecognized ⇒ `spread` (never fail
    /// the server over a typo; say so on stderr and serve the default).
    fn from_env() -> Self {
        match std::env::var("AMC_RETRIEVAL").as_deref() {
            Ok("fused") => Self::Fused,
            Ok("merge") => Self::Merge,
            Ok("spread") | Err(_) => Self::Spread,
            Ok(other) => {
                eprintln!("⚠ AMC_RETRIEVAL={other} is not spread|fused|merge — using spread");
                Self::Spread
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Spread => "spread",
            Self::Fused => "fused",
            Self::Merge => "merge",
        }
    }
}

struct UserMemories {
    dir: PathBuf,
    mode: WriteMode,
    /// Parsed once at startup (`main`), never re-read per request.
    retrieval: RetrievalMode,
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
    /// The retrieval arm is passed in, never read from the environment here:
    /// `main` parses `AMC_RETRIEVAL` once and logs it, and tests pin their
    /// arm explicitly so a stray env var cannot move them onto another path.
    fn with_retrieval(dir: PathBuf, mode: WriteMode, retrieval: RetrievalMode) -> Self {
        Self {
            dir,
            mode,
            retrieval,
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
        let memory = Arc::new(Memory::new_with_label(
            causal_memory::store::CausalStore::open(&path)?,
            "amc",
        ));
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
    /// Optional per-message timestamp (contract field): the event time of
    /// the turn, used to ground temporal ordering instead of ingest time.
    #[serde(default)]
    timestamp: Option<i64>,
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
    /// Live embedder model name, `null` when semantic retrieval is off.
    /// A dead semantic layer used to be invisible from outside the process
    /// (one startup println); the probe now reports it.
    embedding: Option<String>,
}

// ─── Handlers ──────────────────────────────────────────────────────────────

async fn handle_add(
    State(users): State<Arc<UserMemories>>,
    Json(req): Json<AddRequest>,
) -> Result<Json<AddResponse>, (axum::http::StatusCode, String)> {
    let t0 = std::time::Instant::now();
    let out = handle_add_inner(users, req).await;
    causal_memory::observability::metrics().record_request(
        "amc",
        "add",
        if out.is_ok() { "ok" } else { "error" },
        t0.elapsed().as_secs_f64(),
    );
    out
}

async fn handle_add_inner(
    users: Arc<UserMemories>,
    req: AddRequest,
) -> Result<Json<AddResponse>, (axum::http::StatusCode, String)> {
    if req.messages.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "no messages in add request".into(),
        ));
    }
    let memory = users.get(&req.user_id).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("open store: {e}"),
        )
    })?;

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
            let res =
                tokio::task::spawn_blocking(move || (memory.clone(), memory.remember(&text, None)))
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
            let turns: Vec<(String, String, Option<i64>)> = req
                .messages
                .iter()
                .map(|m| (m.role.clone(), m.content.clone(), m.timestamp))
                .collect();
            // The request id is part of the chunk id: a session's second
            // /add used to collide turn-for-turn with the first and be
            // dropped by INSERT OR IGNORE. An empty id is not addressable —
            // the facade then falls back to a session-scoped offset.
            let n =
                memory.remember_raw_turns_with_request_id(&turns, &req.session_id, &req.request_id);
            eprintln!(
                "amc/add [{}] raw: {n} turn(s) stored (request_id: {})",
                req.user_id,
                if req.request_id.is_empty() {
                    "<none>"
                } else {
                    req.request_id.as_str()
                }
            );
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
    let t0 = std::time::Instant::now();
    let out = handle_search_inner(users, req).await;
    causal_memory::observability::metrics().record_request(
        "amc",
        "search",
        "ok",
        t0.elapsed().as_secs_f64(),
    );
    out
}

/// One `/search` through the configured retrieval path.
fn run_retrieval(
    memory: &Memory,
    retrieval: RetrievalMode,
    query: &str,
    top_k: usize,
) -> (Vec<MemoryHit>, &'static str) {
    match retrieval {
        RetrievalMode::Spread => memory.search_memory_entries(query, None, None, top_k),
        RetrievalMode::Fused => memory.search_chunks_fused(query, top_k),
        RetrievalMode::Merge => {
            // Both arms run on their own; the two hit lists are fused again
            // by RRF (key-deduped), so a memory both paths agree on floats
            // above one only a single path found.
            let (spread, _) = memory.search_memory_entries(query, None, None, top_k);
            let (fused, _) = memory.search_chunks_fused(query, top_k);
            let merged = Memory::rrf_merge_hits(&[spread.as_slice(), fused.as_slice()], top_k);
            (merged, "merge")
        }
    }
}

async fn handle_search_inner(users: Arc<UserMemories>, req: SearchRequest) -> Json<SearchResponse> {
    // `options` is contract-fidelity input (choice questions): the platform's
    // answer model receives the memories; options do not change retrieval.
    let _ = &req.options;
    let Ok(memory) = users.get(&req.user_id) else {
        return Json(SearchResponse { data: Vec::new() });
    };
    let top_k = req.top_k.max(1);
    let query = req.query.clone();
    let retrieval = users.retrieval;
    let hits =
        match tokio::task::spawn_blocking(move || run_retrieval(&memory, retrieval, &query, top_k))
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
        // The fused result is per-layer capped at `limit`, i.e. up to
        // 2*top_k rows for a two-layer answer. The contract's `top_k` is a
        // ceiling on the RESPONSE, so truncate at the exit.
        data: hits
            .into_iter()
            .take(top_k)
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
    Json(HealthResponse {
        status: "ok",
        embedding: causal_memory::embed::shared_embedder_model(),
    })
}

// ─── Entry point ───────────────────────────────────────────────────────────

type AppState = Arc<UserMemories>;

fn build_app(users: AppState, auth_token: Option<String>) -> Router {
    // Observability: liveness/readiness + Prometheus text. /health stays
    // for backward compatibility; probes stay open (kubelet can't send
    // bearer headers); /metrics takes optional bearer auth — the challenge
    // harness contract covers /add /search only and never sets the token.
    let obs = Router::new()
        .route("/health", get(handle_health))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(handle_readyz));
    let metrics = causal_memory_cli::http_auth::protected(
        Router::new().route("/metrics", get(handle_metrics)),
        auth_token,
    );
    Router::new()
        .route("/add", post(handle_add))
        .route("/search", post(handle_search))
        .merge(obs)
        .merge(metrics)
        .with_state(users)
}

/// Readiness: a light store check (the DB dir must be writable + openable).
/// Fails 503 when the probe store can't be created/opened.
async fn handle_readyz(State(users): State<Arc<UserMemories>>) -> axum::http::StatusCode {
    let probe = users.get("__readyz_probe__");
    match probe {
        Ok(_) => axum::http::StatusCode::OK,
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Prometheus text exposition (process-wide registry).
async fn handle_metrics() -> String {
    causal_memory::observability::metrics().render_prometheus(None)
}

fn main() -> Result<()> {
    let mut db_dir = PathBuf::from("amc_data");
    let mut port = 8787u16;
    let mut mode = WriteMode::Distill;
    let mut warm_embed = false;
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
            // Image-build warm-up (see Dockerfile): initialize the embedder
            // so the model lands in FASTEMBED_CACHE_DIR, then exit. Reusing
            // the server binary means the warm-up exercises the exact init
            // path the runtime uses — no second entry point to drift.
            "--warm-embed" => warm_embed = true,
            other => anyhow::bail!("unknown flag: {other}"),
        }
        i += 1;
    }

    // The fastembed cache lives on the mounted volume (`/data/fastembed-cache`
    // in the image). A fresh volume has no such directory, and
    // `LocalEmbedder::new()` bails when it is missing — which silently kills
    // the semantic layer for the whole process, because `shared_embedder()`
    // initializes a OnceLock and never retries a failed init. Create it up
    // front so a missing directory can't be the reason.
    if let Ok(cache_dir) = std::env::var("FASTEMBED_CACHE_DIR") {
        if !cache_dir.is_empty() {
            if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                eprintln!(
                    "⚠ FASTEMBED_CACHE_DIR={cache_dir} could not be created: {e} \
                     (semantic layer will not initialize)"
                );
            }
        }
    }

    if warm_embed {
        return match causal_memory::embed::init_embedder() {
            Some(e) => {
                println!("warm-embed: {} ready", e.model());
                Ok(())
            }
            None => Err(anyhow::anyhow!(
                "warm-embed: no embedder could be initialized \
                 (build with --features local-embed, or set CAUSAL_MEMORY_EMBED_API/KEY)"
            )),
        };
    }

    std::fs::create_dir_all(&db_dir)?;

    // The AMC query path wants chunk vectors: the search reader embeds the
    // query anyway, and without stored chunk vectors that leg can never
    // match. An explicit env value wins (benchmarks replaying 32万 turns
    // through the same writer keep it off).
    if std::env::var_os("CAUSAL_MEMORY_EMBED_WRITE").is_none() {
        std::env::set_var("CAUSAL_MEMORY_EMBED_WRITE", "1");
    }

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
        Some(e) => println!(
            "causal-memory-amc embedding: {} (semantic layer live)",
            e.model()
        ),
        None => {
            // Loud on purpose: the line below is easy to skim past, and a
            // silent BM25-only server loses every semantic-only match for
            // the rest of its life (the OnceLock never retries).
            eprintln!(
                "⚠⚠ WARNING: embedding layer is DOWN — retrieval is BM25-only \
                 for this entire process.\n\
                 ⚠⚠   check FASTEMBED_CACHE_DIR (model cached there?), \
                 CAUSAL_MEMORY_EMBED_API/KEY, and that the binary was built \
                 with --features local-embed."
            );
            println!("causal-memory-amc embedding: none (BM25-only retrieval)");
        }
    }
    // A/B switch, parsed once (see RetrievalMode). `spread` stays the
    // default until the harness has data.
    let retrieval = RetrievalMode::from_env();
    let users = Arc::new(UserMemories::with_retrieval(db_dir, mode, retrieval));
    let auth_token = causal_memory_cli::http_auth::token_from_config();
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    println!(
        "causal-memory-amc listening on http://{addr} (write-mode: {}, retrieval: {}, one store per user_id)",
        match mode {
            WriteMode::Distill => "distill",
            WriteMode::Raw => "raw",
        },
        retrieval.label()
    );
    match &auth_token {
        Some(_) => println!("Auth: /metrics requires 'Authorization: Bearer <token>' (CAUSAL_MEMORY_HTTP_AUTH_TOKEN); /add /search /health* stay open"),
        None => println!("NOTE: /metrics is unauthenticated (bound on 0.0.0.0); set CAUSAL_MEMORY_HTTP_AUTH_TOKEN to lock it down"),
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
        axum::serve(listener, build_app(users, auth_token))
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

    /// Server pinned to the production retrieval path (`spread`) — the arm
    /// an unconfigured deployment runs.
    async fn spawn_server(
        mode: WriteMode,
        auth_token: Option<String>,
    ) -> (String, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        spawn_server_with_retrieval(mode, auth_token, RetrievalMode::Spread).await
    }

    /// Same, with an explicit retrieval arm. Tests never let the arm come
    /// from `AMC_RETRIEVAL`: a stray env var in the developer's shell would
    /// otherwise silently move every test onto another path.
    async fn spawn_server_with_retrieval(
        mode: WriteMode,
        auth_token: Option<String>,
        retrieval: RetrievalMode,
    ) -> (String, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        // tempfile::tempdir() gives an O_EXCL-unique dir; the old
        // pid+Instant::now() name could collide when the harness starts
        // several tests in the same tick, and one test's remove_dir_all
        // then deleted a sibling's db dir mid-run (intermittent 500
        // "open store: unable to open database file").
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let app = build_app(
            Arc::new(UserMemories::with_retrieval(dir, mode, retrieval)),
            auth_token,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), server, tmp)
    }

    fn add_body_with_request(
        user: &str,
        session: &str,
        request_id: &str,
        msgs: &[(&str, &str)],
    ) -> serde_json::Value {
        serde_json::json!({
            "request_id": request_id,
            "user_id": user,
            "session_id": session,
            "messages": msgs.iter().map(|(r, c)| serde_json::json!({"role": r, "content": c})).collect::<Vec<_>>(),
        })
    }

    fn add_body(user: &str, session: &str, msgs: &[(&str, &str)]) -> serde_json::Value {
        add_body_with_request(user, session, &format!("req-{user}-{session}"), msgs)
    }

    #[tokio::test]
    async fn raw_roundtrip_isolation_and_topk() {
        let (base, _server, _tmp) = spawn_server(WriteMode::Raw, None).await;
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
            assert!(
                resp.status().is_success(),
                "add failed: {} {}",
                resp.status(),
                resp.text().await.unwrap()
            );
        }

        // Isolation: alice never sees bob's fruit and vice versa.
        for (user, mine, theirs) in [
            ("alice", "dragonfruit", "persimmon"),
            ("bob", "persimmon", "dragonfruit"),
        ] {
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
                    (
                        "assistant",
                        "carol fixed the flaky retry test by adding jitter",
                    ),
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
        assert_eq!(
            resp["data"].as_array().unwrap().len(),
            2,
            "top_k=2 must bind"
        );
    }

    #[tokio::test]
    async fn raw_batches_per_session_keep_every_request() {
        // N1: several /add requests inside one session. With a session-only
        // chunk id (`raw:{session}:{idx}`) the second batch collided
        // turn-for-turn with the first and INSERT OR IGNORE dropped it
        // silently — the platform's whole second request vanished.
        let (base, _server, _tmp) = spawn_server(WriteMode::Raw, None).await;
        let client = test_client();
        wait_ready(&client, &base).await;

        let batches = [
            (
                "req-1",
                "frank adopted the mongoose ORM for the billing job",
            ),
            (
                "req-2",
                "frank retired the mongoose ORM after the lock incident",
            ),
        ];
        for (req, line) in batches {
            let resp = client
                .post(format!("{base}/add"))
                .json(&add_body_with_request(
                    "frank",
                    "s1",
                    req,
                    &[
                        ("user", "what changed in the billing job?"),
                        ("assistant", line),
                    ],
                ))
                .send()
                .await
                .unwrap();
            assert!(
                resp.status().is_success(),
                "add {req} failed: {}",
                resp.text().await.unwrap()
            );
        }

        let resp = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "mongoose ORM billing job",
                "user_id": "frank",
                "top_k": 10,
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let all: String = resp["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["content"].as_str().unwrap_or_default())
            .collect();
        assert!(
            all.contains("adopted the mongoose ORM"),
            "first request's turn is missing: {all}"
        );
        assert!(
            all.contains("retired the mongoose ORM"),
            "second request's turn was dropped: {all}"
        );
    }

    #[tokio::test]
    async fn raw_search_response_is_capped_at_top_k() {
        // S2: the per-layer cap is `limit`, so a fused answer could carry up
        // to 2*top_k rows. The contract's top_k caps the RESPONSE.
        let (base, _server, _tmp) = spawn_server(WriteMode::Raw, None).await;
        let client = test_client();
        wait_ready(&client, &base).await;

        let msgs: Vec<(String, String)> = (0..6)
            .map(|i| {
                (
                    "assistant".to_string(),
                    format!("grace tuned the ingest pipeline, step {i}"),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = msgs.iter().map(|(r, c)| (r.as_str(), c.as_str())).collect();
        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body("grace", "s1", &refs))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let resp = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "ingest pipeline",
                "user_id": "grace",
                "top_k": 3,
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let n = resp["data"].as_array().unwrap().len();
        assert!(n <= 3, "top_k=3 must cap the response, got {n}");
    }

    #[tokio::test]
    async fn empty_search_and_unknown_user() {
        let (base, _server, _tmp) = spawn_server(WriteMode::Raw, None).await;
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
    }

    #[tokio::test]
    async fn distill_mode_without_llm_still_serves() {
        // No LLM env in the test harness: the handler must not fail the add
        // (remember's own fallback stores a raw stub). The contract's
        // synchronous-searchable rule holds either way.
        std::env::remove_var("CAUSAL_MEMORY_LLM_API");
        let (base, _server, _tmp) = spawn_server(WriteMode::Distill, None).await;
        let client = test_client();
        wait_ready(&client, &base).await;
        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body("dave", "s1", &[("user", "hello there")]))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn metrics_bearer_gated_when_token_set() {
        // Opt-in auth: token set → /metrics 401s without (or with a wrong)
        // bearer and serves with the right one; the challenge-contract
        // routes (/add /search) and probes stay open either way.
        let (base, _server, _tmp) =
            spawn_server(WriteMode::Raw, Some("amc-bearer-token".into())).await;
        let client = test_client();
        wait_ready(&client, &base).await;

        let no_auth = client.get(format!("{base}/metrics")).send().await.unwrap();
        assert_eq!(no_auth.status(), axum::http::StatusCode::UNAUTHORIZED);

        let wrong = client
            .get(format!("{base}/metrics"))
            .header("Authorization", "Bearer nope")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), axum::http::StatusCode::UNAUTHORIZED);

        let ok = client
            .get(format!("{base}/metrics"))
            .header("Authorization", "Bearer amc-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), axum::http::StatusCode::OK);

        // Contract routes unaffected by the token.
        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body("eve", "s1", &[("user", "hello there")]))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn fused_retrieval_returns_raw_chunk_evidence() {
        // AMC_RETRIEVAL=fused: the answer is the ingested passage itself, so
        // every hit id is a raw chunk id — a shape the spread engine never
        // produces (it returns fact:/causal: keys).
        let (base, _server, _tmp) =
            spawn_server_with_retrieval(WriteMode::Raw, None, RetrievalMode::Fused).await;
        let client = test_client();
        wait_ready(&client, &base).await;

        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body(
                "henry",
                "s1",
                &[
                    ("user", "what does the billing job use?"),
                    ("assistant", "the billing job uses the mongoose ORM"),
                ],
            ))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "add failed");

        let resp = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "mongoose ORM billing",
                "user_id": "henry",
                "top_k": 5,
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let data = resp["data"].as_array().unwrap();
        assert!(!data.is_empty(), "fused search must hit the evidence turn");
        assert!(
            data.iter()
                .all(|h| h["id"].as_str().unwrap_or_default().starts_with("raw:")),
            "fused hits are chunk ids: {data:?}"
        );
        assert!(
            data[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("the billing job uses the mongoose ORM"),
            "the evidence turn must be first: {data:?}"
        );
        assert!(data[0]["created_at"].is_string());
        assert!(data[0]["score"].as_f64().unwrap_or(0.0) > 0.0);
    }

    #[tokio::test]
    async fn merge_retrieval_serves_both_paths_capped_at_top_k() {
        // AMC_RETRIEVAL=merge: the spread engine AND the chunk path run,
        // then fuse. Keys do not collide across the two (fact:/causal: vs
        // raw:), so the evidence usually appears twice — that is the arm's
        // known cost, and `top_k` still caps the response.
        let (base, _server, _tmp) =
            spawn_server_with_retrieval(WriteMode::Raw, None, RetrievalMode::Merge).await;
        let client = test_client();
        wait_ready(&client, &base).await;

        let resp = client
            .post(format!("{base}/add"))
            .json(&add_body(
                "iris",
                "s1",
                &[
                    ("assistant", "the billing job uses the mongoose ORM"),
                    ("assistant", "the ingest lag dropped to zero"),
                    ("assistant", "the retry test was fixed with jitter"),
                ],
            ))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "add failed");

        let resp = client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({
                "query": "mongoose ORM billing",
                "user_id": "iris",
                "top_k": 2,
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let data = resp["data"].as_array().unwrap();
        assert!(!data.is_empty(), "merge must return the evidence");
        assert!(data.len() <= 2, "top_k must cap the merged response");
        assert!(
            data.iter().any(|h| h["content"]
                .as_str()
                .unwrap_or_default()
                .contains("mongoose ORM")),
            "the fused arm's chunk text must survive the merge: {data:?}"
        );
    }
}
