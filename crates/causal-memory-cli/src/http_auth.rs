//! Opt-in bearer-token auth for the observability endpoints.
//!
//! `CAUSAL_MEMORY_HTTP_AUTH_TOKEN` (env or `setconfig`; empty/unset =
//! disabled, today's open behavior) gates `/metrics` and `/debug/*` on the
//! MCP HTTP server and `/metrics` on the AMC server. Health probes
//! (`/health`, `/healthz`, `/readyz`) stay open on purpose: kubelet
//! liveness/readiness probes cannot attach bearer headers, and those
//! endpoints leak nothing beyond an "ok"/DB-up status. The `/mcp` route is
//! out of scope — MCP-client auth deserves its own design (rmcp 2.2.0
//! already restricts it to loopback Host headers by default).

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

/// Constant-time string equality (best effort, safe code — the workspace
/// denies `unsafe`). The XOR fold has no early exit on mismatch and the
/// accumulator is `black_box`ed so the optimizer cannot turn it into one;
/// differing lengths return false immediately (length is not a secret).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Middleware: require `Authorization: Bearer <token>` (scheme
/// case-insensitive). Only reads the header — body and query (e.g.
/// `/debug/recall?query=...`) pass through untouched. 401 carries
/// `WWW-Authenticate: Bearer` per RFC 6750 and a plain-text body (the obs
/// handlers already answer in plain text; there is no JSON error
/// convention to match).
pub async fn require_bearer(State(token): State<String>, req: Request, next: Next) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| match v.split_once(' ') {
            Some((scheme, cred)) if scheme.eq_ignore_ascii_case("bearer") => {
                constant_time_eq(cred.trim(), token.as_str())
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

/// Apply bearer auth to a router when a token is configured; return it
/// unchanged otherwise (opt-in: unset token = every route open, exactly the
/// pre-auth behavior). `route_layer` only touches routes registered on
/// `router` before this call, so open routes (health probes, `/mcp`) never
/// sit behind it. Generic over the router's state type so callers can layer
/// it before `with_state`.
pub fn protected<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    token: Option<String>,
) -> Router<S> {
    match token {
        Some(token) => {
            router.route_layer(axum::middleware::from_fn_with_state(token, require_bearer))
        }
        None => router,
    }
}

/// Resolve the auth token from config (env wins over the config file; the
/// value is trimmed — config-file entries can carry trailing whitespace).
/// Empty/unset = auth disabled.
pub fn token_from_config() -> Option<String> {
    let t = causal_memory::config::get("CAUSAL_MEMORY_HTTP_AUTH_TOKEN")?;
    let t = t.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_cases() {
        assert!(constant_time_eq("token", "token"));
        assert!(!constant_time_eq("token", "tokem"));
        assert!(!constant_time_eq("token", "tokenn"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }
}
