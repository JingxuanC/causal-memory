//! Multi-tenant bearer auth for the `/mcp` endpoint (opt-in).
//!
//! `CAUSAL_MEMORY_TOKENS_FILE` points to a JSON file mapping bearer tokens
//! to tenant names — or to a directory of such `*.json` files, which are
//! merged (the ops-managed `tokens.json` and the website-bridge
//! `cloud.json` pattern):
//!
//! ```json
//! { "<token-a>": "alice", "sha256:<hex-of-token-b>": "bob" }
//! ```
//!
//! Keys prefixed `sha256:` are matched against the SHA-256 of the presented
//! token, so issuers that only store hashes (the website dashboard: "we only
//! keep a hash") can bridge tokens in without plaintext ever touching disk.
//!
//! When the file is configured and loads as a non-empty map, `/mcp` requires
//! `Authorization: Bearer <token>`; the resolved tenant gets its own
//! `Memory` — one SQLite db per tenant under
//! `<db-dir>/tenants/<safe>.<fnv1a>.db` (the amc.rs per-user pattern) plus
//! the pooled hippocampus graph that goes with it — opened lazily on first
//! request and reused across requests (F1 pooling; see [`TenantStores`]).
//! Unknown or missing tokens get 401 and never create a store. When the file
//! is unset/empty/unreadable the server keeps the pre-existing behavior: no
//! `/mcp` auth, one shared store for everyone (`auth=open` — the startup log
//! states the mode either way).
//!
//! Hot-reload tradeoff: each source file is re-read when its mtime changes
//! (one `stat` per file per request, no full re-parse), so ops can
//! add/revoke tenants without a restart. A same-nanosecond rewrite could
//! theoretically slip past the mtime check — acceptable for files that
//! change by hand/deploy/bridge, and documented here. A source that
//! disappears or stops parsing keeps its last-good map (fail closed): a bad
//! edit must not silently open the endpoint or lock every tenant out. An
//! empty-but-valid `{}` clears that source's tokens (revocation by edit).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use causal_memory::memory::Memory;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tower::ServiceExt;

use crate::http_auth::constant_time_eq;
use crate::server::CausalMemoryServer;

/// Lock a Mutex ignoring poisoning — registry writes can't panic, so a
/// poisoned guard only means some other thread panicked elsewhere; the map
/// is still structurally valid (same pattern as amc.rs).
fn poison_lock<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

/// Parse the tokens file into a token → tenant map. Entries with an empty
/// token or tenant name are dropped (they can never authenticate anyway).
fn load_map(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let text = std::fs::read_to_string(path)?;
    let raw: HashMap<String, String> = serde_json::from_str(&text)?;
    Ok(raw
        .into_iter()
        .filter(|(token, tenant)| !token.trim().is_empty() && !tenant.trim().is_empty())
        .collect())
}

/// Token → tenant map with mtime-based hot reload.
/// SHA-256 hex of a presented bearer token, for matching `sha256:<hex>`
/// entries: bridge files from the website dashboard store only hashes (the
/// product's promise is "we never keep plaintext"), so the server hashes the
/// presented credential and compares digests instead.
fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub(crate) struct TenantTokens {
    /// File or directory. Directory mode merges every `*.json` inside (each
    /// holding the same token→tenant map): the ops-managed `tokens.json` and
    /// the website-bridge `cloud.json` can then live side by side without a
    /// shared-writer race on one file.
    path: PathBuf,
    inner: Mutex<TokensInner>,
}

#[derive(Default)]
struct TokensInner {
    /// Per-source-file last-known mtime (None = file was absent/unreadable).
    /// Compared with `!=`, not ordering: mtime can move backwards on a
    /// restore-from-backup and we still want the reload.
    stamps: HashMap<PathBuf, Option<std::time::SystemTime>>,
    /// Per-source-file last-good map. Fail closed per file: a broken edit to
    /// one source must not drop the others nor silently open anything.
    maps: HashMap<PathBuf, HashMap<String, String>>,
}

impl TokensInner {
    fn total_len(&self) -> usize {
        self.maps.values().map(|m| m.len()).sum()
    }
}

/// List the token source files: the path itself when it is a plain file, or
/// every `*.json` directly inside it when it is a directory (sorted for
/// deterministic merge order).
fn source_files(path: &Path) -> Vec<PathBuf> {
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    if !is_dir {
        return vec![path.to_path_buf()];
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

impl TenantTokens {
    /// Load from `CAUSAL_MEMORY_TOKENS_FILE` (env or config file). Returns
    /// None — open mode — when the key is unset, or the file/dir is missing,
    /// unparseable, or holds no usable entries (each case warns loudly on
    /// stderr; the last one is a misconfiguration an operator must see).
    pub(crate) fn from_env() -> Option<Self> {
        let raw = causal_memory::config::get("CAUSAL_MEMORY_TOKENS_FILE")?;
        let path = PathBuf::from(raw.trim());
        let mut inner = TokensInner::default();
        let mut loaded_any_source = false;
        for file in source_files(&path) {
            let stamp = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
            if let Ok(map) = load_map(&file) {
                inner.maps.insert(file.clone(), map);
                loaded_any_source = true;
            }
            inner.stamps.insert(file, stamp);
        }
        if !loaded_any_source {
            eprintln!(
                "WARNING: CAUSAL_MEMORY_TOKENS_FILE={} could not be loaded; /mcp runs with auth=open",
                path.display()
            );
            return None;
        }
        if inner.total_len() == 0 {
            eprintln!(
                "WARNING: CAUSAL_MEMORY_TOKENS_FILE={} has no usable token entries; /mcp runs with auth=open",
                path.display()
            );
            return None;
        }
        Some(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// Number of configured tokens across all sources (startup log).
    pub(crate) fn len(&self) -> usize {
        poison_lock(&self.inner).total_len()
    }

    /// Re-read sources whose mtime changed (or that appeared/disappeared).
    /// Fail closed per file: a missing/unparseable source keeps its last-good
    /// map; the stamped mtime means it is retried on its next change, not
    /// every request. An empty-but-valid map clears that source's tokens
    /// (that is how revocation-by-edit works).
    fn reload_if_changed(&self) {
        let files = source_files(&self.path);
        let mut inner = poison_lock(&self.inner);
        let file_set: std::collections::HashSet<&PathBuf> = files.iter().collect();
        let stale = inner.stamps.keys().any(|k| !file_set.contains(k))
            || files.iter().any(|f| {
                let stamp = std::fs::metadata(f).and_then(|m| m.modified()).ok();
                inner.stamps.get(f).copied().flatten() != stamp
            });
        if !stale {
            return;
        }
        // Drop sources that disappeared (their tenants are revoked).
        inner.maps.retain(|k, _| file_set.contains(k));
        let mut new_stamps = HashMap::new();
        let mut changed = 0usize;
        for file in files {
            let stamp = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
            if inner.stamps.get(&file).copied().flatten() != stamp {
                changed += 1;
                match load_map(&file) {
                    Ok(map) => {
                        inner.maps.insert(file.clone(), map);
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %file.display(),
                            "tenant tokens source unreadable ({e:#}) — keeping last-good map (fail closed)"
                        );
                    }
                }
            }
            new_stamps.insert(file, stamp);
        }
        inner.stamps = new_stamps;
        if changed > 0 {
            tracing::info!(
                sources = inner.stamps.len(),
                tokens = inner.total_len(),
                "reloaded tenant tokens"
            );
        }
    }

    /// Resolve the request's bearer credential to a tenant name. The scan is
    /// constant-time per entry rather than a HashMap lookup: `HashMap<String>`
    /// hashing is not a constant-time operation and would give a timing
    /// oracle on which token prefix matched. Keys prefixed `sha256:` are
    /// matched against the SHA-256 of the presented token, so hash-only
    /// bridge files work without ever storing plaintext.
    pub(crate) fn resolve(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| match v.split_once(' ') {
                Some((scheme, cred)) if scheme.eq_ignore_ascii_case("bearer") => Some(cred.trim()),
                _ => None,
            })?;
        self.reload_if_changed();
        let presented_hash = sha256_hex(presented);
        poison_lock(&self.inner)
            .maps
            .values()
            .flat_map(|m| m.iter())
            .find(|(token, _)| match token.strip_prefix("sha256:") {
                Some(hash) => constant_time_eq(hash, &presented_hash),
                None => constant_time_eq(token, presented),
            })
            .map(|(_, tenant)| tenant.clone())
    }
}

/// Default cap on pooled tenant `Memory` instances, overridden by
/// `CAUSAL_MEMORY_TENANT_POOL`. The cap bounds **RAM**, not connections:
/// each resident instance holds a whole hippocampus graph
/// (measured ~0.12 GB at 50k nodes, 1.6 GB at 1M, 7.5 GB at 5M —
/// enterprise-scaling §3), so raising it multiplies that footprint by the
/// number of tenants that stay hot. A miss re-opens the tenant's db
/// (migrations are idempotent) and rebuilds its graph on the first query, so
/// a cap that is too small costs latency, never correctness.
const DEFAULT_TENANT_POOL_CAP: usize = 64;

/// Env (or config-file) override for the pool cap.
const TENANT_POOL_ENV: &str = "CAUSAL_MEMORY_TENANT_POOL";

/// The configured cap, clamped to at least 1 (a 0 cap would evict the
/// instance it just opened, i.e. rebuild a graph per request).
fn pool_cap_from_env() -> usize {
    causal_memory::config::get(TENANT_POOL_ENV)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_TENANT_POOL_CAP)
        .max(1)
}

/// tenant → `Memory` registry, opened lazily on first sight. One SQLite db
/// file per tenant — physical isolation, the same retrieval boundary the AMC
/// server uses per user_id.
///
/// F1: the pool holds `Memory` instances, not `CausalStore`s. A `Memory` is
/// what remembers the hippocampus graph, and building that graph is O(store)
/// (measured 4.9 s at 50k nodes, 107 s at 1M): a per-request instance rebuilt
/// it on *every* tool call. Pooling moves that cost to the first request per
/// tenant, and — because an instance now lives long enough to see them — makes
/// D1's co-activation flush and the P7 bypass-write catch-up meaningful
/// (see `Memory::flush` / `Memory::ensure_graph_current`).
///
/// Eviction is LRU by instance count, on `get` only (no background thread:
/// the pool is touched by requests anyway, and a reaper thread would have to
/// fight the registry lock for nothing). An evicted instance is flushed
/// before it is dropped — its buffered co-activation pairs would otherwise
/// die with it, since an unpooled instance never reaches a graph rebuild.
pub(crate) struct TenantStores {
    /// `<db-dir>/tenants` — created on first store open.
    root: PathBuf,
    /// Maximum resident instances (see [`DEFAULT_TENANT_POOL_CAP`]).
    cap: usize,
    inner: Mutex<TenantPool>,
}

/// LRU pool state. `tick` is a monotonic touch counter rather than a clock:
/// two touches inside the same millisecond must still order, and a clock that
/// steps backwards (NTP) would poison the ordering.
#[derive(Default)]
struct TenantPool {
    entries: HashMap<String, PoolEntry>,
    tick: u64,
}

struct PoolEntry {
    memory: Arc<Memory>,
    touched: u64,
}

impl TenantPool {
    /// Insert `memory` for `tenant` as the most recently used instance, then
    /// evict least-recently-used entries until the pool fits `cap`.
    fn insert(&mut self, tenant: &str, memory: Arc<Memory>, cap: usize) {
        self.tick += 1;
        let touched = self.tick;
        self.entries
            .insert(tenant.to_string(), PoolEntry { memory, touched });
        self.evict_over_cap(cap);
    }

    /// Evict LRU instances until `entries.len() <= cap`.
    ///
    /// Each victim is flushed **before** it is dropped: the D1 buffer only
    /// flushes at graph rebuild, and an evicted instance will never rebuild —
    /// without this the learning it buffered since the last rebuild is lost
    /// exactly at the eviction point. `access_buffer` needs nothing here: it
    /// lives inside `CausalStore` and is released with it (the pending
    /// access-count bumps are dropped, as they always have been).
    ///
    /// Dropping the pool's `Arc` does not necessarily drop the `Memory`: a
    /// request in flight holds its own clone and keeps it alive until done.
    /// That is fine — the entry is out of the pool (so it cannot grow back
    /// into the working set) and already flushed.
    fn evict_over_cap(&mut self, cap: usize) {
        while self.entries.len() > cap {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(tenant, _)| tenant.clone())
            else {
                return;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                entry.memory.flush();
                tracing::info!(tenant = %victim, "evicted tenant memory (flushed + dropped)");
            }
        }
    }
}

impl TenantStores {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self::with_capacity(root, pool_cap_from_env())
    }

    /// Explicit-capacity constructor (tests pin the cap instead of fighting
    /// over the process-global env).
    fn with_capacity(root: PathBuf, cap: usize) -> Self {
        Self {
            root,
            cap: cap.max(1),
            inner: Mutex::new(TenantPool::default()),
        }
    }

    /// Filesystem-safe db name per tenant (defensive: tenant names come from
    /// an ops-managed file but are still treated as external input; never let
    /// them escape the tenants dir). Same scheme as amc.rs: readable prefix
    /// + FNV-1a hash to keep distinct names from colliding after sanitizing.
    fn db_path(&self, tenant: &str) -> PathBuf {
        let safe: String = tenant
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let hashed = format!("{:x}", fnv1a(tenant.as_bytes()));
        self.root.join(format!("{safe}.{hashed}.db"))
    }

    /// Get (or lazily open) the tenant's pooled memory. Only ever called with
    /// a tenant that authenticated, so an unknown token can never materialize
    /// a db file.
    ///
    /// The open runs under the registry lock, matching the pre-pooling
    /// double-check: two racing first requests for the same tenant must not
    /// migrate the same db file concurrently (SQLite would serialize them at
    /// best, and a half-applied migration at worst). The lock is held across a
    /// migration and, on eviction, a co-activation flush — both are
    /// sub-millisecond on tenant-sized stores, and both happen on a miss.
    pub(crate) fn get(&self, tenant: &str) -> anyhow::Result<Arc<Memory>> {
        let mut pool = poison_lock(&self.inner);
        pool.tick += 1;
        let touched = pool.tick;
        if let Some(entry) = pool.entries.get_mut(tenant) {
            entry.touched = touched;
            return Ok(Arc::clone(&entry.memory));
        }
        std::fs::create_dir_all(&self.root)?;
        let path = self.db_path(tenant);
        let memory = Arc::new(Memory::new_with_label(
            causal_memory::store::CausalStore::open(&path)?,
            "mcp-http",
        ));
        tracing::info!(tenant, db = %path.display(), "opened tenant memory");
        // F1: a pooled instance is long-lived, so the F2 rule that forbids
        // prewarming per-request instances does not apply — building its graph
        // now keeps the first request from paying the O(store) build on the
        // request thread. Single-flight makes this race with that first
        // request harmlessly (whoever wins builds, the other serves).
        memory.spawn_prewarm();
        pool.insert(tenant, Arc::clone(&memory), self.cap);
        Ok(memory)
    }
}

/// FNV-1a — tiny stable hash for collision-resistant file names (copied from
/// amc.rs, which is a separate bin target and cannot be imported).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Axum state for the multi-tenant `/mcp` route.
pub(crate) struct McpTenantState {
    tokens: TenantTokens,
    stores: TenantStores,
    /// Shared rmcp config (stateless mode, json responses, allowed hosts);
    /// cloned into each per-request service.
    config: StreamableHttpServerConfig,
}

impl McpTenantState {
    pub(crate) fn new(
        tokens: TenantTokens,
        stores: TenantStores,
        config: StreamableHttpServerConfig,
    ) -> Self {
        Self {
            tokens,
            stores,
            config,
        }
    }

    /// The `/mcp` route with tenant auth, ready to merge into the server app.
    pub(crate) fn router(self) -> axum::Router {
        axum::Router::new()
            .route("/mcp", axum::routing::any(mcp_tenant_handler))
            .with_state(Arc::new(self))
    }
}

/// Per-request dispatch: authenticate → resolve tenant → serve the MCP
/// request against that tenant's pooled `Memory`. A fresh
/// `StreamableHttpService` is still built per request — its service factory
/// takes no request context, so capturing the tenant in the factory closure
/// is the per-request binding point — but the service is now a thin shell
/// over an `Arc<Memory>` the pool owns, not a new memory: the per-request
/// construction cost that mattered was the O(store) graph build inside, and
/// that instance is gone (F1).
async fn mcp_tenant_handler(State(state): State<Arc<McpTenantState>>, req: Request) -> Response {
    let Some(tenant) = state.tokens.resolve(req.headers()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized: missing or unknown bearer token",
        )
            .into_response();
    };
    let memory = match state.stores.get(&tenant) {
        Ok(memory) => memory,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("tenant memory unavailable: {e:#}"),
            )
                .into_response();
        }
    };
    let service = StreamableHttpService::new(
        move || {
            Ok(CausalMemoryServer::from_memory(
                Arc::clone(&memory),
                "mcp-http",
            ))
        },
        Arc::new(NeverSessionManager::default()),
        state.config.clone(),
    );
    // StreamableHttpService::Error = Infallible.
    let resp = service.oneshot(req).await.unwrap_or_else(|e| match e {});
    resp.map(axum::body::Body::new)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test invariant: panicking on failure is desired"
)]
mod tests {
    use super::*;

    fn write_tokens(dir: &Path, json: &str) -> PathBuf {
        let path = dir.join("tokens.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    impl TenantTokens {
        /// Test constructor: load from an explicit path (from_env reads the
        /// process-global env, which tests must not fight over).
        fn from_path_for_test(path: &Path) -> Self {
            let mut inner = TokensInner::default();
            for file in source_files(path) {
                let stamp = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
                if let Ok(map) = load_map(&file) {
                    inner.maps.insert(file.clone(), map);
                }
                inner.stamps.insert(file, stamp);
            }
            Self {
                path: path.to_path_buf(),
                inner: Mutex::new(inner),
            }
        }
    }

    #[test]
    fn tokens_parse_and_constant_time_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(dir.path(), r#"{"tok-alice": "alice", "tok-bob": "bob"}"#);
        let tokens = TenantTokens::from_path_for_test(&path);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer tok-alice".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("alice"));
        headers.insert(header::AUTHORIZATION, "bearer tok-bob".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("bob"));
        headers.insert(header::AUTHORIZATION, "Bearer tok-carol".parse().unwrap());
        assert_eq!(tokens.resolve(&headers), None);
        headers.remove(header::AUTHORIZATION);
        assert_eq!(tokens.resolve(&headers), None);
    }

    #[test]
    fn tokens_hot_reload_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(dir.path(), r#"{"tok-alice": "alice"}"#);
        let tokens = TenantTokens::from_path_for_test(&path);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer tok-alice".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("alice"));

        // Rewrite: alice revoked, bob added. Sleep past coarse-mtime
        // filesystems (some CI mounts report 1s granularity).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, r#"{"tok-bob": "bob"}"#).unwrap();
        assert_eq!(tokens.resolve(&headers), None, "revoked token must fail");
        headers.insert(header::AUTHORIZATION, "Bearer tok-bob".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("bob"));

        // Broken file: last-good map stays (fail closed).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("bob"));
    }

    #[test]
    fn tenant_db_path_sanitizes_and_isolates() {
        let stores = TenantStores::new(PathBuf::from("/tmp/x/tenants"));
        let p = stores.db_path("../evil/../tenant");
        assert!(p.starts_with("/tmp/x/tenants"), "{p:?}");
        assert!(p.extension().is_some_and(|e| e == "db"), "{p:?}");
        // Distinct names stay distinct after sanitizing (hash disambiguates).
        assert_ne!(stores.db_path("a/b"), stores.db_path("a_b"));
    }

    // ─── F1: tenant-level Memory pooling ──────────────────────────────────

    /// Pool misses prewarm the tenant's graph on a background thread (the F2
    /// rule that forbids prewarming applies to per-request instances, not to
    /// pooled ones). These tests assert on the pooled instance's graph
    /// behaviour, so wait that build out first: single-flight makes the first
    /// query serve the store-only path while a build is in flight, which is
    /// correct in production and a race in a test.
    fn wait_for_graph(memory: &Memory) {
        for _ in 0..1000 {
            if memory.graph_version() > 0 {
                // The prewarm thread's last act after installing the graph is
                // the D1 flush; let it finish so the buffer assertions below
                // measure this test's queries, not that flush.
                std::thread::sleep(std::time::Duration::from_millis(20));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("tenant graph was never built");
    }

    /// Committed co-occurrence rows in a tenant db, read through a fresh store
    /// (another connection — proves the flush reached the file, not a buffer).
    fn cooc_rows(path: &Path) -> usize {
        causal_memory::store::CausalStore::open(path)
            .expect("open tenant db")
            .load_cooccurrences()
            .expect("load co-occurrences")
            .len()
    }

    /// P1's fix at the tenant boundary: the second request for a tenant must
    /// land on the same `Memory` — and therefore on the graph the first
    /// request built — instead of building a fresh one (and rebuilding the
    /// graph) per request.
    #[test]
    fn pooled_tenant_reuses_instance_and_keeps_graph() {
        let dir = tempfile::tempdir().unwrap();
        let stores = TenantStores::with_capacity(dir.path().join("tenants"), 4);
        let first = stores.get("alice").unwrap();
        wait_for_graph(&first);
        first.record_decision(
            "sharded the ingest pipeline by tenant",
            "the nightly ingest stopped starving the writers",
            "caused",
            "ingest",
            None,
            None,
            None,
        );
        let (hits, mode) = first.search_memory_entries("nightly ingest writers", None, None, 10);
        assert_eq!(mode, "spread", "{hits:?}");
        let version = first.graph_version();
        assert!(version > 0, "the first query must have a graph");

        let second = stores.get("alice").unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second request must reuse the pooled instance"
        );
        let _ = second.search_memory_entries("nightly ingest writers", None, None, 10);
        assert_eq!(
            second.graph_version(),
            version,
            "the second search must not rebuild the graph (F1's whole point)"
        );
    }

    /// Eviction is the one point where a pooled instance goes away, and its
    /// D1 co-activation buffer has no other flush before that (the rebuild
    /// cadence is 900 s or 512 writes). Evicting without flushing would throw
    /// away the learning the pool was introduced to keep.
    #[test]
    fn lru_eviction_flushes_cooccurrences_before_dropping() {
        let dir = tempfile::tempdir().unwrap();
        // One slot: the second tenant overflows it and evicts the first.
        let stores = TenantStores::with_capacity(dir.path().join("tenants"), 1);
        let alice = stores.get("alice").unwrap();
        wait_for_graph(&alice);
        for (decision, outcome) in [
            (
                "moved the retry loop into the queue worker",
                "duplicate jobs on every deploy",
            ),
            (
                "moved the queue worker into its own process",
                "duplicate jobs again on the next deploy",
            ),
        ] {
            alice.record_decision(decision, outcome, "caused", "queue", None, None, None);
        }
        let (hits, mode) =
            alice.search_memory_entries("moved the queue worker deploy", None, None, 10);
        assert_eq!(mode, "spread", "{hits:?}");
        assert!(
            hits.iter().filter(|h| h.key.starts_with("causal:")).count() >= 2,
            "need ≥2 co-activated chunks for the buffer to hold anything: {hits:?}"
        );

        // Buffered, not written: no rebuild has run since the graph was built
        // (searches are reads, and two writes are far below the threshold).
        let path = stores.db_path("alice");
        assert_eq!(
            cooc_rows(&path),
            0,
            "pairs stay buffered until a flush point (rebuild or eviction)"
        );

        let bob = stores.get("bob").unwrap();
        assert!(
            cooc_rows(&path) > 0,
            "eviction must flush the D1 buffer before dropping the instance"
        );
        drop(bob);

        // The evicted instance is really gone from the pool: the next get
        // reopens the db and rebuilds from it.
        let alice_again = stores.get("alice").unwrap();
        assert!(
            !Arc::ptr_eq(&alice, &alice_again),
            "an evicted tenant must not come back as the same instance"
        );
    }

    #[test]
    fn tokens_sha256_hashed_keys_match_without_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let hash = sha256_hex("cm_cloud-token-1");
        let path = write_tokens(
            dir.path(),
            &format!(r#"{{"sha256:{hash}": "alice", "tok-plain": "bob"}}"#),
        );
        let tokens = TenantTokens::from_path_for_test(&path);
        let mut headers = axum::http::HeaderMap::new();
        // Presented plaintext matches its sha256: entry.
        headers.insert(
            header::AUTHORIZATION,
            "Bearer cm_cloud-token-1".parse().unwrap(),
        );
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("alice"));
        // Plaintext entries still work side by side.
        headers.insert(header::AUTHORIZATION, "Bearer tok-plain".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("bob"));
        // The hash itself is NOT a credential: presenting the digest fails.
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {hash}").parse().unwrap(),
        );
        assert_eq!(tokens.resolve(&headers), None);
    }

    #[test]
    fn tokens_directory_mode_merges_and_revokes_per_file() {
        let dir = tempfile::tempdir().unwrap();
        write_tokens(dir.path(), r#"{"tok-alice": "alice"}"#);
        std::fs::write(dir.path().join("cloud.json"), r#"{"tok-carol": "carol"}"#).unwrap();
        // Non-json files in the dir are ignored.
        std::fs::write(dir.path().join("README.txt"), "not tokens").unwrap();
        let tokens = TenantTokens::from_path_for_test(dir.path());
        let mut headers = axum::http::HeaderMap::new();
        for (token, tenant) in [("tok-alice", "alice"), ("tok-carol", "carol")] {
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            assert_eq!(tokens.resolve(&headers).as_deref(), Some(tenant));
        }

        // Website bridge adds a token to cloud.json: picked up on next request.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            dir.path().join("cloud.json"),
            r#"{"tok-carol": "carol", "tok-dave": "dave"}"#,
        )
        .unwrap();
        headers.insert(header::AUTHORIZATION, "Bearer tok-dave".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("dave"));

        // Deleting cloud.json revokes its tenants; tokens.json tenants stay.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_file(dir.path().join("cloud.json")).unwrap();
        headers.insert(header::AUTHORIZATION, "Bearer tok-carol".parse().unwrap());
        assert_eq!(tokens.resolve(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer tok-alice".parse().unwrap());
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("alice"));
    }

    // ─── End-to-end: real MCP JSON-RPC over HTTP against the tenant router ───

    fn test_client() -> reqwest::Client {
        // No keep-alive reuse: on the current-thread tokio runtime the reqwest
        // pool can reset a reused idle connection between requests (amc.rs's
        // test_client note).
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap()
    }

    async fn spawn_tenant_server(dir: &Path) -> String {
        let tokens_path = write_tokens(dir, r#"{"tok-alice": "alice", "tok-bob": "bob"}"#);
        let tokens = TenantTokens::from_path_for_test(&tokens_path);
        let stores = TenantStores::new(dir.join("tenants"));
        let config = StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true);
        let app = McpTenantState::new(tokens, stores, config).router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        // Readiness probe: /mcp answers 401 (not connection-refused) once up.
        let client = test_client();
        for _ in 0..100 {
            if client
                .post(format!("{base}/mcp"))
                .send()
                .await
                .map(|r| r.status() == StatusCode::UNAUTHORIZED)
                .unwrap_or(false)
            {
                return base;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("tenant server did not become ready");
    }

    /// One MCP JSON-RPC call; returns (status, concatenated result text).
    async fn mcp_call(
        client: &reqwest::Client,
        base: &str,
        token: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> (StatusCode, String) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let mut req = client
            .post(format!("{base}/mcp"))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(body.to_string());
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        // JSON-direct mode: {"result":{"content":[{"type":"text","text":...}]}}
        let out = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v["result"]["content"].as_array().map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            })
            .unwrap_or(text);
        (status, out)
    }

    fn tool_call(name: &str, arguments: serde_json::Value) -> (String, serde_json::Value) {
        (
            "tools/call".to_string(),
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
    }

    // multi_thread: the Memory facade uses block_in_place on writes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_tenant_isolation_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_tenant_server(dir.path()).await;
        let client = test_client();

        // Missing and unknown tokens: 401, and no store is materialized.
        for token in [None, Some("tok-unknown")] {
            let (method, params) =
                tool_call("record_fact", serde_json::json!({"key": "x", "value": "x"}));
            let (status, _) = mcp_call(&client, &base, token, &method, params).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
        }
        assert!(
            !dir.path().join("tenants").exists(),
            "rejected requests must not create tenant dbs"
        );

        // Alice records a fact and a causal decision; bob records his own.
        let (status, _) = mcp_call(
            &client,
            &base,
            Some("tok-alice"),
            "tools/call",
            serde_json::json!({"name": "record_fact", "arguments": {
                "key": "tech_stack", "value": "alice runs dragonfruit-db-9000 in prod",
            }}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = mcp_call(
            &client,
            &base,
            Some("tok-alice"),
            "tools/call",
            serde_json::json!({"name": "record_decision", "arguments": {
                "decision": "alice deployed skyhook-rollback-7777 before the freeze",
                "outcome": "the freeze passed without incidents",
                "relation": "caused",
                "task_tag": "release",
            }}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = mcp_call(
            &client,
            &base,
            Some("tok-bob"),
            "tools/call",
            serde_json::json!({"name": "record_fact", "arguments": {
                "key": "tech_stack", "value": "bob runs persimmon-db-4242 in staging",
            }}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Facts: each tenant sees only their own, both directions.
        let (_, alice_facts) = mcp_call(
            &client,
            &base,
            Some("tok-alice"),
            "tools/call",
            serde_json::json!({"name": "search_facts", "arguments": {"query": "dragonfruit"}}),
        )
        .await;
        assert!(alice_facts.contains("dragonfruit-db-9000"), "{alice_facts}");
        let (_, bob_facts) = mcp_call(
            &client,
            &base,
            Some("tok-bob"),
            "tools/call",
            serde_json::json!({"name": "search_facts", "arguments": {"query": "dragonfruit"}}),
        )
        .await;
        assert!(
            !bob_facts.contains("dragonfruit-db-9000"),
            "bob must not see alice's fact: {bob_facts}"
        );
        let (_, bob_own) = mcp_call(
            &client,
            &base,
            Some("tok-bob"),
            "tools/call",
            serde_json::json!({"name": "search_facts", "arguments": {"query": "persimmon"}}),
        )
        .await;
        assert!(bob_own.contains("persimmon-db-4242"), "{bob_own}");

        // Causal layer: bob cannot reach alice's decision either.
        let (_, alice_causal) = mcp_call(
            &client,
            &base,
            Some("tok-alice"),
            "tools/call",
            serde_json::json!({"name": "search_causal", "arguments": {"query": "skyhook"}}),
        )
        .await;
        assert!(
            alice_causal.contains("skyhook-rollback-7777"),
            "{alice_causal}"
        );
        let (_, bob_causal) = mcp_call(
            &client,
            &base,
            Some("tok-bob"),
            "tools/call",
            serde_json::json!({"name": "search_causal", "arguments": {"query": "skyhook"}}),
        )
        .await;
        assert!(
            !bob_causal.contains("skyhook-rollback-7777"),
            "bob must not see alice's decision: {bob_causal}"
        );

        // Physical isolation: one db file per tenant under tenants/.
        let mut dbs: Vec<_> = std::fs::read_dir(dir.path().join("tenants"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".db"))
            .collect();
        dbs.sort();
        assert_eq!(dbs.len(), 2, "{dbs:?}");
        assert!(dbs.iter().any(|n| n.starts_with("alice.")), "{dbs:?}");
        assert!(dbs.iter().any(|n| n.starts_with("bob.")), "{dbs:?}");
    }
}
