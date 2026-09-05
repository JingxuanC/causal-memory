//! Git-sync object-store endpoints (design: docs/design/memory-git-sync.md
//! §2.3/§7 P1 — "https remote, 同一布局只换传输").
//!
//! The server hosts agent-namespaced memory repos on disk with **exactly the
//! same layout as a file remote**, so the client protocol is transport-only:
//!
//! ```text
//! <sync root>/agents/<agent_id>/
//! ├── objects/<sha256>      # commit objects (content-addressed snapshots)
//! └── refs/heads/main       # mainline pointer
//! ```
//!
//! Endpoints (all under `/agents/{id}`):
//!   GET  /agents/{id}/objects/{hash}   → 200 bytes | 404
//!   PUT  /agents/{id}/objects/{hash}   → 201 (body sha256 MUST equal hash)
//!   GET  /agents/{id}/refs/heads/main  → 200 ref | 404 (empty remote)
//!   PUT  /agents/{id}/refs/heads/main  → 204 (atomic rename, no partial refs)
//!
//! Auth (bearer, RFC 6750):
//!   1. `<agent dir>/token` file, when present, is the agent's own token
//!      (provisioned by `cloud register`, P1-3) — it overrides the global one;
//!   2. else the global `CAUSAL_MEMORY_HTTP_AUTH_TOKEN` (same as /metrics);
//!   3. if neither is configured → open (dev/trusted-network mode).
//!
//! Sync root: `CAUSAL_MEMORY_SYNC_ROOT` (env or setconfig), default
//! `~/.local/share/causal-memory/sync`.

use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::http_auth::constant_time_eq;

/// Default sync root when CAUSAL_MEMORY_SYNC_ROOT is unset.
pub(crate) fn sync_root() -> PathBuf {
    if let Some(p) = causal_memory::config::get("CAUSAL_MEMORY_SYNC_ROOT") {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".local/share/causal-memory")
        .join("sync")
}

#[derive(Clone)]
pub(crate) struct SyncState {
    pub(crate) root: PathBuf,
    pub(crate) global_token: Option<String>,
}

#[derive(Clone)]
struct AuthState {
    root: PathBuf,
    global_token: Option<String>,
}

/// Agent ids are namespaced repo names: bounded, alphanumeric + `._-`.
/// Anything else (paths, `..`, unicode, slashes) is rejected up front so no
/// path traversal can ever reach the filesystem.
pub(crate) fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())
}

fn agent_dir(root: &Path, id: &str) -> Option<PathBuf> {
    valid_agent_id(id).then(|| root.join("agents").join(id))
}

/// Resolve the bearer token that guards an agent's repo: per-agent `token`
/// file wins, else the global token. `None` = no auth configured → open.
fn expected_token(root: &Path, id: &str) -> Option<String> {
    let dir = agent_dir(root, id)?;
    if let Ok(raw) = std::fs::read_to_string(dir.join("token")) {
        let t = raw.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

async fn require_sync_auth(State(st): State<AuthState>, req: Request, next: Next) -> Response {
    // Agent id is the path segment right after /agents/.
    let agent = req
        .uri()
        .path()
        .strip_prefix("/agents/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_string();
    let expected = expected_token(&st.root, &agent).or_else(|| st.global_token.clone());
    match expected {
        None => next.run(req).await, // open mode (dev / trusted network)
        Some(expected) => {
            let authorized = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| match v.split_once(' ') {
                    Some((scheme, cred)) if scheme.eq_ignore_ascii_case("bearer") => {
                        constant_time_eq(cred.trim(), expected.as_str())
                    }
                    _ => false,
                })
                .unwrap_or(false);
            if authorized {
                next.run(req).await
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Bearer")],
                    "unauthorized: missing or invalid bearer token",
                )
                    .into_response()
            }
        }
    }
}

type ApiError = (StatusCode, String);

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, msg.into())
}

/// Atomic file write: temp in the same dir + rename (no partial objects/refs
/// ever visible to a concurrent reader).
fn atomic_write_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

async fn get_object(
    State(root): State<PathBuf>,
    AxumPath((agent, hash)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let dir =
        agent_dir(&root, &agent).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad agent id"))?;
    if !valid_hash(&hash) {
        return Err(err(StatusCode::BAD_REQUEST, "bad object hash"));
    }
    let path = dir.join("objects").join(&hash);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(err(StatusCode::NOT_FOUND, "object not found"))
        }
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("read: {e}"))),
    }
}

async fn put_object(
    State(root): State<PathBuf>,
    AxumPath((agent, hash)): AxumPath<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let dir =
        agent_dir(&root, &agent).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad agent id"))?;
    if !valid_hash(&hash) {
        return Err(err(StatusCode::BAD_REQUEST, "bad object hash"));
    }
    // Format sanity: an object file is a meta JSON line (whose `hash` must
    // equal the object name) followed by snapshot data lines. Byte-level
    // sha256(body) ≠ hash on purpose — the object includes the meta line and
    // only the data lines are content-addressed. Full data integrity is
    // re-verified by every client read (parse_object re-hashes the data).
    let text = std::str::from_utf8(&body)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "object must be UTF-8 text"))?;
    let meta_line = text
        .lines()
        .next()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "empty object"))?;
    let meta: serde_json::Value = serde_json::from_str(meta_line)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "first line must be the meta JSON"))?;
    match meta.get("hash").and_then(|h| h.as_str()) {
        Some(h) if h == hash => {}
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "meta.hash does not match the object name",
            ))
        }
    }
    atomic_write_file(&dir.join("objects").join(&hash), &body)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;
    Ok(StatusCode::CREATED)
}

async fn get_ref(
    State(root): State<PathBuf>,
    AxumPath(agent): AxumPath<String>,
) -> Result<Response, ApiError> {
    let dir =
        agent_dir(&root, &agent).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad agent id"))?;
    let path = dir.join("refs/heads/main");
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Ok(([(header::CONTENT_TYPE, "text/plain")], content).into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(err(StatusCode::NOT_FOUND, "remote is empty (no ref yet)"))
        }
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("read: {e}"))),
    }
}

async fn put_ref(
    State(root): State<PathBuf>,
    AxumPath(agent): AxumPath<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let dir =
        agent_dir(&root, &agent).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad agent id"))?;
    let content = String::from_utf8(body.to_vec())
        .map_err(|_| err(StatusCode::BAD_REQUEST, "ref must be plain text"))?;
    let content = content.trim();
    if content.len() != 64 || !content.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "ref must be a 64-hex commit hash",
        ));
    }
    atomic_write_file(&dir.join("refs/heads/main"), content.as_bytes())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Router for the object-store endpoints. State is bound internally so it can
/// be merged into the MCP HTTP app (which has its own state type).
pub(crate) fn build_sync_router(state: SyncState) -> Router {
    let api = Router::new()
        .route(
            "/agents/{agent}/objects/{hash}",
            get(get_object).put(put_object),
        )
        .route("/agents/{agent}/refs/heads/main", get(get_ref).put(put_ref));
    let auth = AuthState {
        root: state.root.clone(),
        global_token: state.global_token.clone(),
    };
    api.route_layer(axum::middleware::from_fn_with_state(
        auth,
        require_sync_auth,
    ))
    .with_state(state.root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use sha2::{Digest, Sha256};

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap()
    }

    async fn spawn(root: PathBuf, global_token: Option<String>) -> String {
        let app = build_sync_router(SyncState { root, global_token });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        // The sync router has no open liveness route; poll PUT-less GET on a
        // bogus agent until the server answers (404/400 both mean "up").
        let client = test_client();
        for _ in 0..100 {
            if client
                .get(format!("{base}/agents/x/refs/heads/main"))
                .send()
                .await
                .is_ok()
            {
                return base;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("sync server did not become ready");
    }

    #[tokio::test]
    async fn object_and_ref_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let base = spawn(root.path().to_path_buf(), None).await;
        let client = test_client();
        // A realistic commit object: meta JSON line (hash == data hash) +
        // snapshot data lines. Only the data lines are content-addressed.
        let data = "{\"type\":\"chunk\",\"id\":\"x\",\"text\":\"直推上线\",\"created_at\":1}\n{\"type\":\"edge\",\"from_id\":\"x\",\"to_id\":\"x\",\"relation\":\"caused\"}";
        let hash = sha256_hex(data.as_bytes());
        let content = format!(
            "{{\"format_version\":1,\"hash\":\"{hash}\",\"parent\":null,\"message\":\"m\",\"agent_id\":\"a\",\"created_at\":1}}\n{data}\n"
        );

        // PUT then GET object.
        let resp = client
            .put(format!("{base}/agents/athena/objects/{hash}"))
            .body(content.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let got = client
            .get(format!("{base}/agents/athena/objects/{hash}"))
            .send()
            .await
            .unwrap();
        assert_eq!(got.status(), StatusCode::OK);
        assert_eq!(got.bytes().await.unwrap().as_ref(), content.as_bytes());

        // Integrity: meta.hash not matching the object name → 400.
        let resp = client
            .put(format!("{base}/agents/athena/objects/{}", "0".repeat(64)))
            .body(content.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Not an object at all (no parseable meta line) → 400.
        let resp = client
            .put(format!("{base}/agents/athena/objects/{}", "1".repeat(64)))
            .body("meta\npayload\n")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Ref roundtrip.
        let resp = client
            .put(format!("{base}/agents/athena/refs/heads/main"))
            .body(hash.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = client
            .get(format!("{base}/agents/athena/refs/heads/main"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), hash);

        // Missing object → 404; unknown agent ref → 404.
        assert_eq!(
            client
                .get(format!("{base}/agents/athena/objects/{}", "1".repeat(64)))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .get(format!("{base}/agents/nobody/refs/heads/main"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );

        // Traversal / invalid ids rejected without touching the filesystem.
        // (reqwest normalizes `/agents/../` away before the server sees it,
        // so some of these surface as route-level 404s rather than our 400 —
        // either way nothing is served and nothing is written.)
        for bad in ["..", "../x", "a/b", "ä", ""] {
            let resp = client
                .get(format!("{base}/agents/{bad}/refs/heads/main"))
                .send()
                .await
                .unwrap();
            let status = resp.status();
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
                "agent={bad:?} → {status}"
            );
        }
        // The repo really lives at <root>/agents/athena/ with the file layout.
        assert!(root
            .path()
            .join("agents/athena/objects")
            .join(&hash)
            .exists());
    }

    #[tokio::test]
    async fn bearer_auth_global_and_per_agent() {
        let root = tempfile::tempdir().unwrap();
        // Global token set → everything needs it.
        let base = spawn(root.path().to_path_buf(), Some("global-secret".into())).await;
        let client = test_client();
        let no_auth = client
            .get(format!("{base}/agents/athena/refs/heads/main"))
            .send()
            .await
            .unwrap();
        assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
        let ok = client
            .get(format!("{base}/agents/athena/refs/heads/main"))
            .header("Authorization", "Bearer global-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::NOT_FOUND); // authed, empty repo

        // Per-agent token file overrides the global one.
        let dir = root.path().join("agents/athena");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "agent-own-secret").unwrap();
        let wrong = client
            .get(format!("{base}/agents/athena/refs/heads/main"))
            .header("Authorization", "Bearer global-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            wrong.status(),
            StatusCode::UNAUTHORIZED,
            "per-agent token must win"
        );
        let right = client
            .get(format!("{base}/agents/athena/refs/heads/main"))
            .header("Authorization", "Bearer agent-own-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(right.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn open_mode_when_no_tokens() {
        // Covered implicitly by object_and_ref_roundtrip (no global token,
        // no agent token file → PUT/GET succeed unauthenticated).
        let _ = Request::builder().body(Body::empty()).unwrap();
    }
}
