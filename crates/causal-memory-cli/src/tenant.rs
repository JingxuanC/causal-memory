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
//! `CausalStore` (one SQLite db per tenant under
//! `<db-dir>/tenants/<safe>.<fnv1a>.db`, the amc.rs per-user pattern), opened
//! lazily on first request. Unknown or missing tokens get 401 and never
//! create a store. When the file is unset/empty/unreadable the server keeps
//! the pre-existing behavior: no `/mcp` auth, one shared store for everyone
//! (`auth=open` — the startup log states the mode either way).
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
        let stale = inner
            .stamps
            .keys()
            .any(|k| !file_set.contains(k))
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
            tracing::info!(sources = inner.stamps.len(), tokens = inner.total_len(), "reloaded tenant tokens");
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

/// tenant → `CausalStore` registry, opened lazily on first sight. One SQLite
/// db file per tenant — physical isolation, the same retrieval boundary the
/// AMC server uses per user_id.
pub(crate) struct TenantStores {
    /// `<db-dir>/tenants` — created on first store open.
    root: PathBuf,
    stores: Mutex<HashMap<String, Arc<causal_memory::store::CausalStore>>>,
}

impl TenantStores {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            stores: Mutex::new(HashMap::new()),
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

    /// Get (or lazily open) the tenant's store. Only ever called with a
    /// tenant that authenticated, so an unknown token can never materialize
    /// a db file.
    pub(crate) fn get(
        &self,
        tenant: &str,
    ) -> anyhow::Result<Arc<causal_memory::store::CausalStore>> {
        if let Some(store) = poison_lock(&self.stores).get(tenant) {
            return Ok(Arc::clone(store));
        }
        let mut guard = poison_lock(&self.stores);
        if let Some(store) = guard.get(tenant) {
            return Ok(Arc::clone(store));
        }
        std::fs::create_dir_all(&self.root)?;
        let path = self.db_path(tenant);
        let store = Arc::new(causal_memory::store::CausalStore::open(&path)?);
        tracing::info!(tenant, db = %path.display(), "opened tenant store");
        guard.insert(tenant.to_string(), Arc::clone(&store));
        Ok(store)
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
/// request against that tenant's store. A fresh `StreamableHttpService` is
/// built per request (construction is a handful of Arc clones) because its
/// service factory takes no request context — capturing the tenant's store
/// in the factory closure is the per-request binding point.
async fn mcp_tenant_handler(State(state): State<Arc<McpTenantState>>, req: Request) -> Response {
    let Some(tenant) = state.tokens.resolve(req.headers()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized: missing or unknown bearer token",
        )
            .into_response();
    };
    let store = match state.stores.get(&tenant) {
        Ok(store) => store,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("tenant store unavailable: {e:#}"),
            )
                .into_response();
        }
    };
    let tenant_store = (*store).clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(CausalMemoryServer::new_with_label(
                tenant_store.clone(),
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
        std::fs::write(
            dir.path().join("cloud.json"),
            r#"{"tok-carol": "carol"}"#,
        )
        .unwrap();
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
        headers.insert(
            header::AUTHORIZATION,
            "Bearer tok-dave".parse().unwrap(),
        );
        assert_eq!(tokens.resolve(&headers).as_deref(), Some("dave"));

        // Deleting cloud.json revokes its tenants; tokens.json tenants stay.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_file(dir.path().join("cloud.json")).unwrap();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer tok-carol".parse().unwrap(),
        );
        assert_eq!(tokens.resolve(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            "Bearer tok-alice".parse().unwrap(),
        );
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
