//! Git-style memory versioning + sync (design: docs/design/memory-git-sync.md).
//!
//! Memory = repo, agent_id = repo name. Commands:
//!   commit [-m <msg>] [--db P]          snapshot the whole store (full truth)
//!   log [--oneline] [--limit N] [--db P] walk the parent chain, no DB open
//!   push [<remote|path>] [--db P]        upload local-only commits (ff-checked)
//!   pull [<remote|path>] [--db P]        import remote commits (idempotent 只增)
//!   clone <path|remote> [--db P]         fresh DB from a remote + set origin
//!   checkout <hash|HEAD|HEAD~N> [--db P] hard-reset DB to a snapshot (backup first)
//!   remote add|list|remove <name> [url]  named-remote config (.cm/config.json)
//!
//! Object layout (one self-contained file per commit):
//!   line 1            meta JSON (hash/parent/message/agent_id/created_at/counts)
//!   lines 2..N        snapshot data lines — export_jsonl output WITHOUT its
//!                     header line (exported_at would pollute content addressing)
//! hash = sha256(data lines joined with "\n"); meta line & header line excluded.
//! Local state lives in `<db>.cm/` (config.json, HEAD, refs/heads/main,
//! objects/, backups/); a remote root is the same objects/ + refs layout.
//!
//! WAL note: the store keeps pooled connections open, so file-level backups of
//! a live DB must checkpoint first (PRAGMA wal_checkpoint(TRUNCATE)). checkout
//! never opens the target DB — it imports into a temp file and renames — so its
//! backup copy is taken from a closed, consistent file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::commands::io::{
    export_jsonl, import_jsonl, import_jsonl_aligned, ExportFilters, ExportStats,
};
use crate::get_db_path;
use causal_memory::store::CausalStore;

const FORMAT_VERSION: i64 = 1;
const HEAD_REF: &str = "ref: refs/heads/main";

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Counts {
    edges: usize,
    meta_edges: usize,
    chunks: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct CommitMeta {
    format_version: i64,
    hash: String,
    parent: Option<String>,
    message: String,
    agent_id: String,
    created_at: i64,
    counts: Counts,
}

/// `<db path>.cm/` sidecar dir for a store file.
fn cm_dir_for(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(".cm");
    PathBuf::from(s)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    // digest 0.11 finalize() returns an Array (not GenericArray); hex manually.
    let mut s = String::with_capacity(out.len() * 2);
    for b in out.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hash of canonical snapshot data: the data lines joined with "\n".
fn hash_of_data_lines(lines: &[String]) -> String {
    sha256_hex(lines.join("\n").as_bytes())
}

// ─── .cm state helpers ────────────────────────────────────────────────

fn ensure_cm(cm: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(cm.join("objects"))?;
    std::fs::create_dir_all(cm.join("refs/heads"))?;
    std::fs::create_dir_all(cm.join("backups"))?;
    Ok(())
}

/// Atomic write: temp file in the same dir + rename.
/// Sensitive content (refs, HEAD, and above all .cm/config.json which holds
/// cloud bearer tokens) must not be world-readable — force 0600 regardless
/// of umask (review finding: token/config were 0644).
fn atomic_write(path: &Path, content: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("path has no parent dir")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), rand_suffix()));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos}")
}

fn read_ref(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_local_refs(cm: &Path, hash: &str) -> anyhow::Result<()> {
    // HEAD is static (git convention, room for branches later).
    let head = cm.join("HEAD");
    if !head.exists() {
        atomic_write(&head, HEAD_REF)?;
    }
    atomic_write(&cm.join("refs/heads/main"), hash)
}

fn read_config(cm: &Path) -> anyhow::Result<serde_json::Value> {
    let p = cm.join("config.json");
    if !p.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(&p)?;
    Ok(serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({})))
}

fn write_config(cm: &Path, cfg: &serde_json::Value) -> anyhow::Result<()> {
    atomic_write(&cm.join("config.json"), &serde_json::to_string_pretty(cfg)?)
}

fn remotes_of(cfg: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(rem) = cfg.get("remotes").and_then(|r| r.as_object()) {
        for (name, v) in rem {
            if let Some(url) = v.get("url").and_then(|u| u.as_str()) {
                out.insert(name.clone(), url.to_string());
            }
        }
    }
    out
}

/// One named remote's full config: url + optional per-remote bearer token
/// (set by `cloud register`, P1-3).
fn remote_entry(cfg: &serde_json::Value, name: &str) -> Option<(String, Option<String>)> {
    let v = cfg.get("remotes")?.get(name)?;
    let url = v.get("url")?.as_str()?.to_string();
    let token = v.get("token").and_then(|t| t.as_str()).map(String::from);
    Some((url, token))
}

/// A remote memory repo. Two transports, one layout (design §2.3):
/// a local directory (file remote) or an https object-store whose base URL
/// is the agent namespace (e.g. `https://cm.example.com/agents/athena`).
enum Remote {
    File(PathBuf),
    Http { base: String, token: Option<String> },
}

fn is_http_url(t: &str) -> bool {
    t.starts_with("http://") || t.starts_with("https://")
}

fn looks_like_path(t: &str) -> bool {
    !is_http_url(t)
        && (t.starts_with("file://")
            || t.contains('/')
            || t == "."
            || t == ".."
            || t.starts_with("./")
            || t.starts_with("../")
            || t.starts_with('~')
            || Path::new(t).exists())
}

fn normalize_path(t: &str) -> PathBuf {
    let t = t.strip_prefix("file://").unwrap_or(t);
    if let Some(rest) = t.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(t)
}

fn env_auth_token() -> Option<String> {
    std::env::var("CAUSAL_MEMORY_HTTP_AUTH_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Build a Remote from a resolved url + optional configured token. HTTP urls
/// fall back to the shared env token; file urls carry no token.
fn remote_from_url(url: &str, token: Option<String>) -> Remote {
    if is_http_url(url) {
        Remote::Http {
            base: url.trim_end_matches('/').to_string(),
            token: token.or_else(env_auth_token),
        }
    } else {
        Remote::File(normalize_path(url))
    }
}

impl Remote {
    fn display(&self) -> String {
        match self {
            Remote::File(p) => p.display().to_string(),
            Remote::Http { base, .. } => base.clone(),
        }
    }

    fn http() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|e| panic!("failed to build HTTP client: {e}"))
    }

    fn auth(
        req: reqwest::blocking::RequestBuilder,
        token: &Option<String>,
    ) -> reqwest::blocking::RequestBuilder {
        match token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// Current mainline ref; None = unborn remote (no refs/heads/main yet).
    fn read_ref(&self) -> anyhow::Result<Option<String>> {
        match self {
            Remote::File(p) => Ok(read_ref(&p.join("refs/heads/main"))),
            Remote::Http { base, token } => {
                let resp = Self::auth(Self::http().get(format!("{base}/refs/heads/main")), token)
                    .send()
                    .context("sync server unreachable")?;
                match resp.status().as_u16() {
                    200 => {
                        let body = resp.text()?;
                        let t = body.trim();
                        if t.is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some(t.to_string()))
                        }
                    }
                    404 => Ok(None), // empty remote
                    s => bail!(
                        "GET refs → HTTP {s}: {}",
                        body_snippet(&resp.text().await_ok())
                    ),
                }
            }
        }
    }

    fn write_ref(&self, hash: &str) -> anyhow::Result<()> {
        match self {
            Remote::File(p) => atomic_write(&p.join("refs/heads/main"), hash),
            Remote::Http { base, token } => {
                let resp = Self::auth(
                    Self::http()
                        .put(format!("{base}/refs/heads/main"))
                        .body(hash.to_string()),
                    token,
                )
                .send()
                .context("sync server unreachable")?;
                if !resp.status().is_success() {
                    bail!(
                        "PUT refs → HTTP {}: {}",
                        resp.status().as_u16(),
                        body_snippet(&resp.text().await_ok())
                    );
                }
                Ok(())
            }
        }
    }

    /// Raw commit object text; errors mention the remote so users know which
    /// side is missing/corrupt.
    fn read_object_raw(&self, hash: &str) -> anyhow::Result<String> {
        match self {
            Remote::File(p) => {
                std::fs::read_to_string(p.join("objects").join(hash)).with_context(|| {
                    format!(
                        "commit {:.8} not found on remote (pull first?)",
                        short(hash)
                    )
                })
            }
            Remote::Http { base, token } => {
                let resp = Self::auth(Self::http().get(format!("{base}/objects/{hash}")), token)
                    .send()
                    .context("sync server unreachable")?;
                match resp.status().as_u16() {
                    200 => Ok(resp.text()?),
                    404 => bail!(
                        "commit {:.8} not found on remote (pull first?)",
                        short(hash)
                    ),
                    s => bail!(
                        "GET object → HTTP {s}: {}",
                        body_snippet(&resp.text().await_ok())
                    ),
                }
            }
        }
    }

    fn write_object(&self, hash: &str, content: &str) -> anyhow::Result<()> {
        match self {
            Remote::File(p) => atomic_write(&p.join("objects").join(hash), content),
            Remote::Http { base, token } => {
                let resp = Self::auth(
                    Self::http()
                        .put(format!("{base}/objects/{hash}"))
                        .body(content.to_string()),
                    token,
                )
                .send()
                .context("sync server unreachable")?;
                if !resp.status().is_success() {
                    bail!(
                        "PUT object → HTTP {}: {}",
                        resp.status().as_u16(),
                        body_snippet(&resp.text().await_ok())
                    );
                }
                Ok(())
            }
        }
    }
}

trait RespTextFallback {
    fn await_ok(self) -> String;
}
impl RespTextFallback for std::result::Result<String, reqwest::Error> {
    fn await_ok(self) -> String {
        self.unwrap_or_default()
    }
}

fn body_snippet(body: &str) -> String {
    let b: String = body.chars().take(200).collect();
    if b.is_empty() {
        "(empty body)".to_string()
    } else {
        b
    }
}

/// Resolve a push/pull/clone target to a [`Remote`]. Resolution order: named
/// remote from config (or default "origin") > http(s) URL > literal path.
/// Anything else that looks like a bare agent_id is the cloud registry's job
/// (P1-3).
fn resolve_remote(cm: &Path, target: Option<&str>, default_name: &str) -> anyhow::Result<Remote> {
    let cfg = read_config(cm)?;
    let remotes = remotes_of(&cfg);
    let (url, token) = match target {
        None => {
            let (url, token) = remote_entry(&cfg, default_name).context(format!(
                "no remote named '{default_name}' configured (remote add {default_name} <path|url> or pass one)"
            ))?;
            (url, token)
        }
        Some(t) => {
            if let Some((url, token)) = remote_entry(&cfg, t) {
                (url, token)
            } else if is_http_url(t) || looks_like_path(t) {
                (t.to_string(), None)
            } else {
                bail!(
                    "'{t}' is neither a configured remote ({}) nor a path/URL — \
                     run `cloud register {t} <server-url>` first",
                    remotes.keys().cloned().collect::<Vec<_>>().join(", "),
                )
            }
        }
    };
    Ok(remote_from_url(&url, token))
}

/// Mirror commit object files from a remote into the local `.cm/objects` so
/// `log` / `checkout` work offline afterwards (git fetch semantics: refs only
/// point at objects you actually hold).
fn mirror_objects(remote: &Remote, dst_dir: &Path, hashes: &[String]) -> anyhow::Result<()> {
    for h in hashes {
        let dst = dst_dir.join("objects").join(h);
        if !dst.exists() {
            let raw = remote.read_object_raw(h)?;
            atomic_write(&dst, &raw)?;
        }
    }
    Ok(())
}

/// Parse a commit object's raw text (meta line + data lines). Verifies
/// sha256(data) == meta.hash == the requested hash.
fn parse_object(raw: &str, hash: &str) -> anyhow::Result<(CommitMeta, Vec<String>)> {
    if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.len() != 64 {
        bail!("invalid commit hash: {hash}");
    }
    let mut lines = raw.lines();
    let meta_line = lines.next().context("empty commit object")?;
    let meta: CommitMeta = serde_json::from_str(meta_line)
        .context("commit object meta line is not valid JSON (corrupt object)")?;
    let data: Vec<String> = lines.map(String::from).collect();
    let want = hash_of_data_lines(&data);
    if want != meta.hash || want != hash {
        bail!(
            "commit {:.8} failed integrity check (hash mismatch: object corrupted)",
            short(hash)
        );
    }
    Ok((meta, data))
}

/// Read a commit object from a local `.cm/objects` dir (the local side always
/// stores raw files — this is not a [`Remote`]).
fn read_object(cm: &Path, hash: &str) -> anyhow::Result<(CommitMeta, Vec<String>)> {
    let path = cm.join("objects").join(hash);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("commit {:.8} not found locally (pull first?)", short(hash)))?;
    parse_object(&raw, hash)
}

/// Resolve `hash | HEAD | HEAD~N` to a full 64-hex hash, or an unambiguous
/// >=8-char prefix of a local object.
fn resolve_commit(cm: &Path, target: &str) -> anyhow::Result<String> {
    let head = read_ref(&cm.join("refs/heads/main"));
    if let Some(stripped) = target.strip_prefix("HEAD") {
        if stripped.is_empty() {
            return head.context("no commits yet (HEAD is unborn)");
        }
        let steps: u32 = stripped
            .strip_prefix('~')
            .context("expected HEAD or HEAD~N")?
            .parse()
            .context("expected HEAD or HEAD~N")?;
        let mut cur = head.context("no commits yet (HEAD is unborn)")?;
        for _ in 0..steps {
            let (meta, _) = read_object(cm, &cur)?;
            cur = meta.parent.context("HEAD~N walks past the first commit")?;
        }
        return Ok(cur);
    }
    if target.len() == 64 {
        return Ok(target.to_string());
    }
    if target.len() < 8 {
        bail!("commit reference too short: '{target}' (use >=8 hex chars, HEAD, or HEAD~N)");
    }
    // Prefix match over local objects.
    let dir = cm.join("objects");
    let mut hits: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.len() == 64 && name.starts_with(target) {
                hits.push(name);
            }
        }
    }
    match hits.len() {
        0 => bail!("commit '{}' not found locally (pull first?)", short(target)),
        1 => Ok(hits.remove(0)),
        _ => bail!(
            "ambiguous commit prefix '{}': matches {}",
            short(target),
            hits.len()
        ),
    }
}

fn short(hash: &str) -> String {
    hash.chars().take(8).collect()
}

fn fmt_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

// ─── snapshot production ──────────────────────────────────────────────

/// Full-truth snapshot data lines (export minus header). Deterministic for a
/// given DB state — this is what makes "nothing to commit" and push dedup work.
fn snapshot_data_lines(store: &CausalStore) -> anyhow::Result<(Vec<String>, ExportStats)> {
    let f = ExportFilters {
        task_tag: None,
        min_confidence: 0.0,
        since: 0,
        include_invalidated: true,
        redact: false,
    };
    let (mut lines, stats) = export_jsonl(store, &f)?;
    // Line 0 is the header ("exported_at": now) — exclude it from the
    // snapshot (a timestamp in the hashed content would break
    // nothing-to-commit and push dedup).
    lines.remove(0);
    Ok((lines, stats))
}

fn counts_of(stats: &ExportStats) -> Counts {
    Counts {
        edges: stats.edges,
        meta_edges: stats.meta_edges,
        chunks: stats.chunks,
    }
}

/// Full truth (valid + invalidated) counts for the whole DB — used by
/// checkout output and pull "working tree updated" summary.
fn db_counts(store: &CausalStore) -> anyhow::Result<Counts> {
    let (_, stats) = snapshot_data_lines(store)?;
    Ok(counts_of(&stats))
}

fn agent_id() -> String {
    std::env::var("CAUSAL_MEMORY_AGENT_ID").unwrap_or_else(|_| "local".into())
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

/// Checkpoint a live WAL store so a file-level copy of the DB is consistent.
fn checkpoint_and_copy(store: &CausalStore, db_path: &Path, dst: &Path) -> anyhow::Result<()> {
    store.with_conn(|conn| {
        let _: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        Ok(())
    })?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(db_path, dst)?;
    Ok(())
}

fn backup_db(db_path: &Path, cm: &Path, tag: &str) -> anyhow::Result<Option<PathBuf>> {
    if !db_path.exists() {
        return Ok(None);
    }
    ensure_cm(cm)?;
    let dst = cm.join("backups").join(format!("{tag}-{}.db", now_ts()));
    // Target DB is not open in this process (checkout never opens it); a plain
    // copy of the closed file is consistent. Best-effort remove of stale
    // WAL sidecars from a previous unclean exit so they can't shadow the copy.
    for ext in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{}", db_path.display(), ext));
        if side.exists() {
            let _ = std::fs::remove_file(&side);
        }
    }
    std::fs::copy(db_path, &dst)?;
    Ok(Some(dst))
}

// ─── commands ─────────────────────────────────────────────────────────

pub(crate) fn run_commit(args: &[String]) -> anyhow::Result<()> {
    let mut msg: Vec<String> = Vec::new();
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--message" => {
                i += 1;
                let Some(m) = args.get(i) else {
                    bail!("-m requires a message");
                };
                msg.push(m.clone());
            }
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            other if other.starts_with("--") => bail!("unknown flag: {other}"),
            other => bail!("unexpected argument: {other}"),
        }
        i += 1;
    }
    let message = msg.join(" ");
    if message.trim().is_empty() {
        bail!("commit requires -m <message> (git philosophy: messages are human intent)");
    }
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);
    ensure_cm(&cm)?;
    let store = CausalStore::open(&db_path)?;
    let (data, stats) = snapshot_data_lines(&store)?;
    let hash = hash_of_data_lines(&data);
    let head = read_ref(&cm.join("refs/heads/main"));
    if head.as_deref() == Some(hash.as_str()) {
        println!("nothing to commit, working tree clean");
        return Ok(());
    }
    let meta = CommitMeta {
        format_version: FORMAT_VERSION,
        hash: hash.clone(),
        parent: head.clone(),
        message: message.trim().to_string(),
        agent_id: agent_id(),
        created_at: now_ts(),
        counts: counts_of(&stats),
    };
    let mut content = serde_json::to_string(&meta)?;
    content.push('\n');
    content.push_str(&data.join("\n"));
    content.push('\n');
    atomic_write(&cm.join("objects").join(&hash), &content)?;
    write_local_refs(&cm, &hash)?;
    // Δ vs parent (read meta only — no DB access).
    let (d_e, d_m, d_c) = match &head {
        Some(p) => {
            let (pmeta, _) = read_object(&cm, p)?;
            (
                stats.edges as i64 - pmeta.counts.edges as i64,
                stats.meta_edges as i64 - pmeta.counts.meta_edges as i64,
                stats.chunks as i64 - pmeta.counts.chunks as i64,
            )
        }
        None => (
            stats.edges as i64,
            stats.meta_edges as i64,
            stats.chunks as i64,
        ),
    };
    println!(
        "commit {} — {}  (Δ edges {d_e:+}, meta_edges {d_m:+}, chunks {d_c:+})",
        short(&hash),
        meta.message
    );
    Ok(())
}

pub(crate) fn run_log(args: &[String]) -> anyhow::Result<()> {
    let mut oneline = false;
    let mut limit: Option<usize> = None;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--oneline" => oneline = true,
            "--limit" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("--limit requires a number");
                };
                limit = Some(v.parse().context("--limit must be an integer")?);
            }
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            other if other.starts_with("--") => bail!("unknown flag: {other}"),
            other => bail!("unexpected argument: {other}"),
        }
        i += 1;
    }
    let cm = cm_dir_for(&db.unwrap_or_else(get_db_path));
    let mut cur = match read_ref(&cm.join("refs/heads/main")) {
        Some(h) => h,
        None => {
            println!("(no commits yet)");
            return Ok(());
        }
    };
    let mut n = 0usize;
    loop {
        let (meta, _) = read_object(&cm, &cur)?;
        // Δ vs the child snapshot (first commit shows absolute counts).
        let (d_e, d_m, d_c) = match &meta.parent {
            Some(p) => {
                let (pm, _) = read_object(&cm, p)?;
                (
                    meta.counts.edges as i64 - pm.counts.edges as i64,
                    meta.counts.meta_edges as i64 - pm.counts.meta_edges as i64,
                    meta.counts.chunks as i64 - pm.counts.chunks as i64,
                )
            }
            None => (
                meta.counts.edges as i64,
                meta.counts.meta_edges as i64,
                meta.counts.chunks as i64,
            ),
        };
        if oneline {
            println!(
                "{} {}  {}",
                short(&cur),
                fmt_ts(meta.created_at),
                meta.message
            );
        } else {
            println!("commit {} ({})", short(&cur), meta.message);
            println!("  Author: agent <{}>", meta.agent_id);
            println!("  Date:   {}", fmt_ts(meta.created_at));
            println!("  Δ edges {d_e:+}, meta_edges {d_m:+}, chunks {d_c:+}");
            println!();
        }
        n += 1;
        if limit.is_some_and(|l| n >= l) {
            break;
        }
        match &meta.parent {
            Some(p) => cur = p.clone(),
            None => break,
        }
    }
    Ok(())
}

pub(crate) fn run_push(args: &[String]) -> anyhow::Result<()> {
    let mut target: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            s if s.starts_with("--") => bail!("unknown flag: {s}"),
            positional => {
                if target.is_some() {
                    bail!("unexpected extra argument: {positional}");
                }
                target = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);
    let remote = resolve_remote(&cm, target.as_deref(), "origin")?;
    let local_head = match read_ref(&cm.join("refs/heads/main")) {
        Some(h) => h,
        None => {
            println!("nothing to push (no commits)");
            return Ok(());
        }
    };
    let remote_head = remote.read_ref()?;

    // Collect the local-only chain: walk parents from HEAD until we hit the
    // remote ref (fast-forward) or genesis. An empty remote (no ref yet) is
    // trivially fast-forward — nothing exists there to overwrite.
    let mut to_push: Vec<String> = Vec::new();
    let mut cur = local_head.clone();
    let mut ff_ok = remote_head.is_none();
    loop {
        if remote_head.as_deref() == Some(cur.as_str()) {
            ff_ok = true;
            break;
        }
        to_push.push(cur.clone());
        let (meta, _) = read_object(&cm, &cur)?;
        match meta.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    if to_push.is_empty() {
        println!("everything up-to-date");
        return Ok(());
    }
    if !ff_ok {
        bail!(
            "remote has commit(s) you don't (remote HEAD {}) — pull first (fast-forward check)",
            remote_head
                .as_deref()
                .map(short)
                .unwrap_or_else(|| "?".into())
        );
    }
    // Upload oldest → newest.
    for h in to_push.iter().rev() {
        let raw = std::fs::read_to_string(cm.join("objects").join(h))
            .with_context(|| format!("local object {:.8} missing (corrupt .cm?)", short(h)))?;
        remote.write_object(h, &raw)?;
    }
    remote.write_ref(&local_head)?;
    println!("pushed {} commit(s) → {}", to_push.len(), remote.display());
    Ok(())
}

/// Read + integrity-verify a commit object from any remote transport.
fn read_remote_object(remote: &Remote, hash: &str) -> anyhow::Result<(CommitMeta, Vec<String>)> {
    let raw = remote.read_object_raw(hash)?;
    parse_object(&raw, hash)
}

/// Import a chain of commit snapshots (oldest first) into `store`. Returns
/// cumulative ImportStats across all snapshots.
///
/// Uses **align-mode** import: replaying full-state snapshots over an existing
/// DB converges to the remote's state — a forget/supersede (valid_to set in a
/// newer snapshot) invalidates the local copy instead of being skipped, and a
/// re-validation (valid_to → NULL) re-activates it. This closes the 只增 gap
/// (design §4/§9 R6): pull is now real state alignment, not just additions.
fn import_chain(
    store: &CausalStore,
    remote: &Remote,
    chain_oldest_first: &[String],
) -> anyhow::Result<ImportStatsSum> {
    let mut sum = ImportStatsSum::default();
    for h in chain_oldest_first {
        let (meta, data) = read_remote_object(remote, h)?; // verify integrity
        if meta.format_version != FORMAT_VERSION {
            bail!("unsupported commit format_version {}", meta.format_version);
        }
        let stats = import_jsonl_aligned(store, &data.join("\n"), None, false)?;
        sum.imported += stats.imported;
        sum.aligned += stats.aligned;
        sum.skipped_duplicate += stats.skipped_duplicate;
        sum.skipped_invalid += stats.skipped_invalid;
    }
    Ok(sum)
}

#[derive(Default)]
struct ImportStatsSum {
    imported: usize,
    aligned: usize,
    skipped_duplicate: usize,
    skipped_invalid: usize,
}

pub(crate) fn run_pull(args: &[String]) -> anyhow::Result<()> {
    let mut target: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            s if s.starts_with("--") => bail!("unknown flag: {s}"),
            positional => {
                if target.is_some() {
                    bail!("unexpected extra argument: {positional}");
                }
                target = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);
    ensure_cm(&cm)?;
    let remote = resolve_remote(&cm, target.as_deref(), "origin")?;
    let remote_head = match remote.read_ref()? {
        Some(h) => h,
        None => {
            println!("nothing to pull (remote is empty)");
            return Ok(());
        }
    };
    let local_head = read_ref(&cm.join("refs/heads/main"));

    // Chain from remote HEAD back until local HEAD or genesis.
    let mut remote_chain: Vec<String> = Vec::new(); // oldest first
    let mut cur = remote_head.clone();
    let mut hit_local = false;
    loop {
        if local_head.as_deref() == Some(cur.as_str()) {
            hit_local = true;
            break;
        }
        remote_chain.push(cur.clone());
        let (meta, _) = read_remote_object(&remote, &cur)?;
        match meta.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    remote_chain.reverse();
    if remote_chain.is_empty() {
        println!("already up to date");
        return Ok(());
    }
    if !hit_local && local_head.is_some() {
        bail!(
            "histories diverged: local HEAD {:.8} is not on the remote's chain \
             (remote has {} commit(s) you lack). No merge in MVP — pull is \
             fast-forward only. Clone the remote into a fresh --db if you want \
             its state (your uncommitted records can be re-imported from a \
             backup or export).",
            short(local_head.as_deref().unwrap_or_default()),
            remote_chain.len()
        );
    }

    // git fetch semantics: hold every commit object locally before importing,
    // so log/checkout work offline afterwards.
    mirror_objects(&remote, &cm, &remote_chain)?;

    let store = CausalStore::open(&db_path)?;
    // Backup pre-import (WAL-safe: checkpoint first).
    let backup = {
        let dst = cm.join("backups").join(format!("pre-pull-{}.db", now_ts()));
        let _ = checkpoint_and_copy(&store, &db_path, &dst).map(|_| dst.clone());
        // checkpoint failure is non-fatal; import is idempotent + re-runnable.
        dst.exists().then_some(dst)
    };
    let sum = import_chain(&store, &remote, &remote_chain)?;
    let counts = db_counts(&store)?;
    write_local_refs(&cm, &remote_head)?;
    println!(
        "pulled {} commit(s) → working tree updated (edges {}, meta_edges {}, chunks {})",
        remote_chain.len(),
        counts.edges,
        counts.meta_edges,
        counts.chunks
    );
    println!(
        "  imported {} · aligned {} · skipped_duplicate {} · skipped_invalid {}",
        sum.imported, sum.aligned, sum.skipped_duplicate, sum.skipped_invalid
    );
    if let Some(b) = backup {
        println!("  pre-pull backup: {}", b.display());
    }
    if local_head.is_some() {
        println!(
            "  local-only uncommitted records (if any) are preserved; commit to snapshot them"
        );
    }
    Ok(())
}

pub(crate) fn run_clone(args: &[String]) -> anyhow::Result<()> {
    let mut target: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            s if s.starts_with("--") => bail!("unknown flag: {s}"),
            positional => {
                if target.is_some() {
                    bail!("unexpected extra argument: {positional}");
                }
                target = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let Some(target) = target else {
        bail!("clone requires a <path|remote> argument");
    };
    let db_path = db.unwrap_or_else(get_db_path);
    if db_path.exists() {
        bail!("refusing to clone over existing DB {}", db_path.display());
    }
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cm = cm_dir_for(&db_path);
    ensure_cm(&cm)?;
    // Fresh clone: target may be a path/URL, or a named remote / registered
    // agent_id (config holds agent remotes with their bearer token).
    let cfg = read_config(&cm)?;
    let remote = if is_http_url(&target) {
        remote_from_url(&target, env_auth_token())
    } else if let Some((url, token)) = remote_entry(&cfg, &target) {
        remote_from_url(&url, token.filter(|t| !t.is_empty()))
    } else if looks_like_path(&target) {
        remote_from_url(&target, None)
    } else {
        bail!(
            "'{target}' is neither a path/URL nor a registered agent — run `cloud register {target} <server-url>` first"
        )
    };
    let remote_head = match remote.read_ref()? {
        Some(h) => h,
        None => bail!("nothing to clone (remote is empty): {}", remote.display()),
    };
    let store = CausalStore::open(&db_path)?;
    let mut chain: Vec<String> = Vec::new();
    let mut cur = remote_head.clone();
    loop {
        chain.push(cur.clone());
        let (meta, _) = read_remote_object(&remote, &cur)?;
        match meta.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain.reverse();
    // git fetch semantics: hold commit objects locally (bootstrap summary,
    // log and checkout all read local objects afterwards).
    mirror_objects(&remote, &cm, &chain)?;
    let sum = import_chain(&store, &remote, &chain)?;
    let counts = db_counts(&store)?;
    write_local_refs(&cm, &remote_head)?;
    // Remember the source as origin (git clone semantics), including the
    // bearer token when this remote is HTTP-authenticated.
    let (origin_url, origin_token) = match &remote {
        Remote::File(p) => (format!("file://{}", p.display()), None),
        Remote::Http { base, token } => (base.clone(), token.clone()),
    };
    let mut cfg = read_config(&cm)?;
    cfg["remotes"]["origin"] = serde_json::json!({
        "url": origin_url,
        "token": origin_token,
    });
    write_config(&cm, &cfg)?;

    // Bootstrap summary: newest commit meta + last 3 valid lessons.
    let (head_meta, _) = read_remote_object(&remote, &remote_head)?;
    let lessons: Vec<(String, String)> = store.with_conn(|conn: &Connection| {
        let mut stmt = conn.prepare(
            "SELECT cf.text, ct.text FROM causal_edges ce
            JOIN chunks cf ON cf.id = ce.from_id
            JOIN chunks ct ON ct.id = ce.to_id
            WHERE ce.valid_to IS NULL
            ORDER BY ce.discovered_at DESC, ce.id DESC LIMIT 3",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))
    })?;
    println!(
        "Cloned agent {} · edges {} · meta_edges {} · chunks {}",
        head_meta.agent_id, counts.edges, counts.meta_edges, counts.chunks
    );
    println!(
        "  imported {} · HEAD {:.8}",
        sum.imported,
        short(&remote_head)
    );
    println!("  origin → {}", remote.display());
    if lessons.is_empty() {
        println!("  (no lessons yet)");
    } else {
        println!("  recent lessons:");
        for (d, o) in lessons {
            println!("    · {d} → {o}");
        }
    }
    Ok(())
}

pub(crate) fn run_checkout(args: &[String]) -> anyhow::Result<()> {
    let mut target: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            s if s.starts_with("--") => bail!("unknown flag: {s}"),
            positional => {
                if target.is_some() {
                    bail!("unexpected extra argument: {positional}");
                }
                target = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let Some(target) = target else {
        bail!("checkout requires a <commit> argument (hash, HEAD, or HEAD~N)");
    };
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);
    if !cm.join("refs/heads/main").exists() && !cm.join("objects").exists() {
        bail!(
            "not a memory repo (no .cm state next to {}); commit first",
            db_path.display()
        );
    }
    ensure_cm(&cm)?;
    let hash = resolve_commit(&cm, &target)?;
    let (meta, data) = read_object(&cm, &hash)?; // integrity check

    // 1. Backup current DB (file is closed — consistent copy).
    let backup = backup_db(&db_path, &cm, "pre-checkout")?;

    // 2. Rebuild into a temp DB next to the target, then atomically swap.
    let dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = db_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "causal.db".into());
    let tmp_path = dir.join(format!(".{file_name}.checkout-{}", std::process::id()));
    if tmp_path.exists() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    {
        let store = CausalStore::open(&tmp_path)?;
        let sum = import_jsonl(&store, &data.join("\n"), None, false)?;
        let counts = db_counts(&store)?;
        drop(store); // close pool → WAL checkpoint → temp is a clean single file
                     // Best-effort cleanup of temp WAL sidecars (last-close should already
                     // have removed them).
        for ext in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{}", tmp_path.display(), ext));
            if side.exists() {
                let _ = std::fs::remove_file(&side);
            }
        }
        // 3. Swap.
        std::fs::rename(&tmp_path, &db_path)?;
        write_local_refs(&cm, &hash)?;
        println!(
            "checkout {:.8} — {}  (edges {}, meta_edges {}, chunks {} restored)",
            short(&hash),
            meta.message,
            counts.edges,
            counts.meta_edges,
            counts.chunks
        );
        println!("  imported {}", sum.imported);
        if let Some(b) = &backup {
            println!("  pre-checkout backup: {}", b.display());
        }
        if sum.imported == 0 && !data.is_empty() {
            println!("  (snapshot restored; import reported 0 — re-import deduped against nothing on a fresh DB, check counts above)");
        }
    }
    Ok(())
}

/// `cloud register|list|revoke` — provision/rotate/revoke per-agent bearer
/// tokens on a sync server (P1-3), and record the agent as a named remote so
/// `push`/`pull`/`clone <agent_id>` resolve with the right token.
///
///   cloud register <agent_id> <server-url> [--db P]   mint token, save remote
///   cloud list     <server-url> [--db P]              list registered agents
///   cloud revoke   <agent_id> <server-url> [--db P]   revoke token + drop remote
///
/// Admin auth for the control plane: CAUSAL_MEMORY_ADMIN_TOKEN, else the
/// shared CAUSAL_MEMORY_HTTP_AUTH_TOKEN (the server applies the same rule).
pub(crate) fn run_cloud(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str =
        "Usage: causal-memory cloud register <agent_id> <server-url> [--db P]\n       \
         causal-memory cloud list <server-url> [--db P]\n       \
         causal-memory cloud revoke <agent_id> <server-url> [--db P]";
    let mut db: Option<PathBuf> = None;
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            s if s.starts_with("--") => bail!("unknown flag: {s}\n{USAGE}"),
            other => pos.push(other.to_string()),
        }
        i += 1;
    }
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let admin_token = std::env::var("CAUSAL_MEMORY_ADMIN_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(env_auth_token);

    let op = pos
        .first()
        .ok_or_else(|| anyhow::anyhow!("{USAGE}"))?
        .as_str();
    match op {
        "register" => {
            let (Some(agent), Some(server)) = (pos.get(1), pos.get(2)) else {
                bail!("Usage: causal-memory cloud register <agent_id> <server-url>\n{USAGE}");
            };
            let server = server.trim_end_matches('/');
            let mut req = client.post(format!("{server}/agents/{agent}/register"));
            if let Some(t) = &admin_token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().context("sync server unreachable")?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().unwrap_or_default();
                bail!(
                    "register failed → HTTP {status}: {} (server needs CAUSAL_MEMORY_ADMIN_TOKEN to mint tokens)",
                    body_snippet(&body)
                );
            }
            let v: serde_json::Value = resp.json()?;
            let token = v
                .get("token")
                .and_then(|t| t.as_str())
                .context("register response missing token")?
                .to_string();
            ensure_cm(&cm)?;
            let mut cfg = read_config(&cm)?;
            cfg["remotes"][agent] = serde_json::json!({
                "url": format!("{server}/agents/{agent}"),
                "token": token,
            });
            write_config(&cm, &cfg)?;
            println!(
                "registered agent '{agent}' → {server}/agents/{agent} (token saved; rotated: {})",
                v.get("rotated").and_then(|r| r.as_bool()).unwrap_or(false)
            );
            println!("  now: commit -m … && push {agent}");
        }
        "list" => {
            let Some(server) = pos.get(1) else {
                bail!("Usage: causal-memory cloud list <server-url>\n{USAGE}");
            };
            let server = server.trim_end_matches('/');
            let mut req = client.get(format!("{server}/agents"));
            if let Some(t) = &admin_token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().context("sync server unreachable")?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                bail!(
                    "list failed → HTTP {status}: {}",
                    body_snippet(&resp.text().unwrap_or_default())
                );
            }
            let v: serde_json::Value = resp.json()?;
            let agents = v
                .get("agents")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            if agents.is_empty() {
                println!("(no agents registered on {server})");
            } else {
                println!("agents on {server}:");
                for a in agents {
                    println!(
                        "  {} (token: {})",
                        a.get("agent_id").and_then(|x| x.as_str()).unwrap_or("?"),
                        if a.get("has_token")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false)
                        {
                            "provisioned"
                        } else {
                            "none"
                        }
                    );
                }
            }
        }
        "revoke" => {
            let (Some(agent), Some(server)) = (pos.get(1), pos.get(2)) else {
                bail!("Usage: causal-memory cloud revoke <agent_id> <server-url>\n{USAGE}");
            };
            let server = server.trim_end_matches('/');
            let mut req = client.delete(format!("{server}/agents/{agent}/token"));
            if let Some(t) = &admin_token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().context("sync server unreachable")?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                bail!(
                    "revoke failed → HTTP {status}: {}",
                    body_snippet(&resp.text().unwrap_or_default())
                );
            }
            // Drop the local named remote (url + token) for that agent.
            let mut cfg = read_config(&cm)?;
            if let Some(rem) = cfg.get_mut("remotes").and_then(|r| r.as_object_mut()) {
                rem.remove(agent);
            }
            write_config(&cm, &cfg)?;
            println!("revoked token for agent '{agent}' and dropped the local remote");
        }
        other => bail!("unknown cloud subcommand: {other} (register|list|revoke)\n{USAGE}"),
    }
    Ok(())
}

pub(crate) fn run_remote(args: &[String]) -> anyhow::Result<()> {
    let mut db: Option<PathBuf> = None;
    let mut op_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path");
                };
                db = Some(PathBuf::from(p));
            }
            other => op_args.push(other.to_string()),
        }
        i += 1;
    }
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);
    ensure_cm(&cm)?;
    let Some(op) = op_args.first() else {
        bail!("Usage: causal-memory remote add <name> <path|url> | list | remove <name>");
    };
    let mut cfg = read_config(&cm)?;
    match op.as_str() {
        "add" => {
            let (Some(name), Some(url)) = (op_args.get(1), op_args.get(2)) else {
                bail!("Usage: causal-memory remote add <name> <path|url>");
            };
            cfg["remotes"][name] = serde_json::json!({ "url": url });
            write_config(&cm, &cfg)?;
            println!("remote {name} → {url}");
        }
        "list" | "ls" => {
            let remotes = remotes_of(&cfg);
            if remotes.is_empty() {
                println!("(no remotes configured)");
            }
            for (name, url) in remotes {
                println!("{name}\t{url}");
            }
        }
        "remove" | "rm" => {
            let Some(name) = op_args.get(1) else {
                bail!("Usage: causal-memory remote remove <name>");
            };
            if let Some(rem) = cfg.get_mut("remotes").and_then(|r| r.as_object_mut()) {
                if rem.remove(name).is_some() {
                    write_config(&cm, &cfg)?;
                    println!("remote {name} removed");
                } else {
                    bail!("no remote named '{name}'");
                }
            } else {
                bail!("no remote named '{name}'");
            }
        }
        other => bail!("unknown remote subcommand: {other} (add|list|remove)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let n = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("cm-git-test-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TestDir(p)
        }
        fn db(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One valid lesson between two chunks.
    fn seed_jsonl(decision: &str, outcome: &str, valid_to: Option<i64>) -> String {
        let d = format!("d:{}", fnv64(decision));
        let o = format!("o:{}", fnv64(outcome));
        let mut s = format!(
            "{{\"type\":\"chunk\",\"id\":\"{d}\",\"text\":\"{}\",\"created_at\":1700000000}}\n",
            decision
        );
        s.push_str(&format!(
            "{{\"type\":\"chunk\",\"id\":\"{o}\",\"text\":\"{}\",\"created_at\":1700000000}}\n",
            outcome
        ));
        let vto = valid_to
            .map(|t| format!(",\"valid_to\":{t}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "{{\"type\":\"edge\",\"from_id\":\"{d}\",\"to_id\":\"{o}\",\"relation\":\"caused\",\"confidence\":0.9,\"task_tag\":null,\"event_time\":1700000000,\"discovered_at\":1700000000{vto},\"discovered_by\":\"test\",\"outcome_polarity\":null}}\n"
        ));
        s
    }

    fn fnv64(s: &str) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    fn export_edges(db: &str) -> (usize, usize) {
        // (valid edge count, invalidated edge count)
        let store = CausalStore::open(db).unwrap();
        store
            .with_conn(|conn| {
                let valid: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NULL",
                    [],
                    |r| r.get(0),
                )?;
                let inv: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NOT NULL",
                    [],
                    |r| r.get(0),
                )?;
                Ok((valid as usize, inv as usize))
            })
            .unwrap()
    }

    fn export_texts(db: &str) -> Vec<String> {
        let store = CausalStore::open(db).unwrap();
        store
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT text FROM chunks ORDER BY id").unwrap();
                let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>().unwrap())
            })
            .unwrap()
    }

    fn head_of(db: &str) -> Option<String> {
        read_ref(&cm_dir_for(Path::new(db)).join("refs/heads/main"))
    }

    fn objects_count(db: &str) -> usize {
        let dir = cm_dir_for(Path::new(db)).join("objects");
        std::fs::read_dir(&dir).map(|rd| rd.count()).unwrap_or(0)
    }

    fn commit_all(db: &str, msg: &str) -> anyhow::Result<()> {
        run_commit(&["-m".into(), msg.into(), "--db".into(), db.into()])
    }

    #[test]
    fn roundtrip_commit_clone_retrieves_same_lessons() {
        let td = TestDir::new("roundtrip");
        let db_a = td.db("a.db");
        let remote = td.db("remote");
        let db_b = td.db("b.db");

        // Seed machine A with one lesson via a shared-format import.
        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(
            &store,
            &seed_jsonl("直推上线", "生产挂了", None),
            None,
            false,
        )
        .unwrap();
        drop(store);
        commit_all(&db_a, "学会：不直推").unwrap();
        run_remote(&[
            "add".into(),
            "origin".into(),
            remote.clone(),
            "--db".into(),
            db_a.clone(),
        ])
        .unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();
        assert!(head_of(&db_a).is_some(), "push should leave a local HEAD");
        assert_eq!(objects_count(&db_a), 1, "exactly one commit object");

        // "New machine": clone.
        run_clone(&[remote.clone(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(export_texts(&db_b), export_texts(&db_a));
        let (valid, inv) = export_edges(&db_b);
        assert_eq!((valid, inv), (1, 0));
        // origin auto-recorded.
        let cfg = read_config(&cm_dir_for(Path::new(&db_b))).unwrap();
        assert!(cfg["remotes"].get("origin").is_some());

        // Nothing to commit on an identical second commit.
        run_commit(&["-m".into(), "空".into(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(objects_count(&db_b), 1);
    }

    #[test]
    fn pull_is_idempotent_and_preserves_local_records() {
        let td = TestDir::new("pull");
        let db_a = td.db("a.db");
        let remote = td.db("remote");
        let db_b = td.db("b.db");

        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(&store, &seed_jsonl("教训A", "结果A", None), None, false).unwrap();
        drop(store);
        commit_all(&db_a, "c1").unwrap();
        run_remote(&[
            "add".into(),
            "origin".into(),
            remote.clone(),
            "--db".into(),
            db_a.clone(),
        ])
        .unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();
        run_clone(&[remote.clone(), "--db".into(), db_b.clone()]).unwrap();

        // Local uncommitted record on B.
        let store = CausalStore::open(&db_b).unwrap();
        import_jsonl(
            &store,
            &seed_jsonl("本地独有", "本地结果", None),
            None,
            false,
        )
        .unwrap();
        drop(store);

        // A adds a second lesson and pushes.
        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(&store, &seed_jsonl("教训B", "结果B", None), None, false).unwrap();
        drop(store);
        commit_all(&db_a, "c2").unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();

        // B pulls: both remote lessons + local-only record present.
        run_pull(&["origin".into(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(head_of(&db_b), head_of(&db_a));
        let (valid, _) = export_edges(&db_b);
        assert_eq!(valid, 3); // 教训A + 教训B + 本地独有
        let texts = export_texts(&db_b);
        for t in ["教训A", "教训B", "本地独有"] {
            assert!(texts.iter().any(|x| x.contains(t)), "missing {t}");
        }

        // Second pull: no-op, state unchanged.
        let before = head_of(&db_b);
        run_pull(&["origin".into(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(head_of(&db_b), before);
        assert_eq!(export_edges(&db_b).0, 3);
    }

    #[test]
    fn push_rejects_non_fast_forward() {
        let td = TestDir::new("ffreject");
        let db_a = td.db("a.db");
        let remote = td.db("remote");
        let db_b = td.db("b.db");

        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(&store, &seed_jsonl("教训A", "结果A", None), None, false).unwrap();
        drop(store);
        commit_all(&db_a, "c1").unwrap();
        run_remote(&[
            "add".into(),
            "origin".into(),
            remote.clone(),
            "--db".into(),
            db_a.clone(),
        ])
        .unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();
        run_clone(&[remote.clone(), "--db".into(), db_b.clone()]).unwrap();

        // A advances remote to c2.
        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(&store, &seed_jsonl("教训A2", "结果A2", None), None, false).unwrap();
        drop(store);
        commit_all(&db_a, "c2").unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();

        // B commits locally on top of c1 (divergence).
        let store = CausalStore::open(&db_b).unwrap();
        import_jsonl(&store, &seed_jsonl("B本地", "B结果", None), None, false).unwrap();
        drop(store);
        commit_all(&db_b, "b1").unwrap();

        // B push → rejected (remote has c2 which B lacks).
        let err = run_push(&["origin".into(), "--db".into(), db_b.clone()]).unwrap_err();
        assert!(err.to_string().contains("pull first"), "got: {err:#}");

        // B pull → rejected (diverged; fast-forward only).
        let err = run_pull(&["origin".into(), "--db".into(), db_b.clone()]).unwrap_err();
        assert!(err.to_string().contains("diverged"), "got: {err:#}");
    }

    #[test]
    fn corrupt_object_is_rejected() {
        let td = TestDir::new("corrupt");
        let db_a = td.db("a.db");
        let remote = td.db("remote");
        let db_b = td.db("b.db");

        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(&store, &seed_jsonl("教训A", "结果A", None), None, false).unwrap();
        drop(store);
        commit_all(&db_a, "c1").unwrap();
        run_remote(&[
            "add".into(),
            "origin".into(),
            remote.clone(),
            "--db".into(),
            db_a.clone(),
        ])
        .unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();
        run_clone(&[remote.clone(), "--db".into(), db_b.clone()]).unwrap();

        // Corrupt the local copy of the commit object (append junk → hash
        // mismatch on read). checkout must reject it instead of restoring
        // from a corrupted snapshot.
        let hash = head_of(&db_a).unwrap();
        let lobj = cm_dir_for(Path::new(&db_b)).join("objects").join(&hash);
        let mut raw = std::fs::read_to_string(&lobj).unwrap();
        raw.push_str("junk");
        std::fs::write(&lobj, raw).unwrap();
        let err = run_checkout(&[hash.clone(), "--db".into(), db_b.clone()]).unwrap_err();
        assert!(err.to_string().contains("integrity"), "got: {err:#}");
    }

    #[test]
    fn checkout_rolls_back_including_invalidated_state() {
        let td = TestDir::new("checkout");
        let db = td.db("a.db");

        // c1: one valid lesson + one invalidated lesson (as if forgotten).
        let mut seed = seed_jsonl("教训V", "结果V", None);
        seed.push_str(&seed_jsonl("教训F", "结果F", Some(1700000100)));
        let store = CausalStore::open(&db).unwrap();
        import_jsonl(&store, &seed, None, false).unwrap();
        drop(store);
        commit_all(&db, "c1").unwrap();
        let c1 = head_of(&db).unwrap();

        // c2: add another valid lesson.
        let store = CausalStore::open(&db).unwrap();
        import_jsonl(&store, &seed_jsonl("教训X", "结果X", None), None, false).unwrap();
        drop(store);
        commit_all(&db, "c2").unwrap();
        assert_eq!(export_edges(&db), (2, 1));

        // Roll back to c1: 教训X gone; 教训F stays invalidated.
        run_checkout(&[c1.clone(), "--db".into(), db.clone()]).unwrap();
        assert_eq!(head_of(&db).as_deref(), Some(c1.as_str()));
        assert_eq!(export_edges(&db), (1, 1));
        let texts = export_texts(&db);
        assert!(texts.iter().any(|t| t.contains("教训V")));
        assert!(!texts.iter().any(|t| t.contains("教训X")));

        // Pre-checkout backup exists and still contains 教训X.
        let backups = cm_dir_for(Path::new(&db)).join("backups");
        let files: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(!files.is_empty(), "backup should exist");
        let (v, inv) = export_edges(files[0].to_str().unwrap());
        assert_eq!((v, inv), (2, 1));

        // Nothing to commit right after checkout (state == c1 snapshot).
        run_commit(&["-m".into(), "again".into(), "--db".into(), db.clone()]).unwrap();
        assert_eq!(objects_count(&db), 2);
    }

    #[test]
    fn head_n_resolution_and_log_walk() {
        let td = TestDir::new("headn");
        let db = td.db("a.db");
        for (i, (d, o)) in [("教训A", "结果A"), ("教训B", "结果B")].iter().enumerate() {
            let store = CausalStore::open(&db).unwrap();
            import_jsonl(&store, &seed_jsonl(d, o, None), None, false).unwrap();
            drop(store);
            commit_all(&db, &format!("commit-{i}")).unwrap();
        }
        let head = head_of(&db).unwrap();
        // HEAD~1 == first commit.
        let first = resolve_commit(&cm_dir_for(Path::new(&db)), "HEAD~1").unwrap();
        assert_ne!(first, head);
        assert!(run_checkout(&["HEAD~1".into(), "--db".into(), db.clone()]).is_ok());
        assert_eq!(export_edges(&db).0, 1);
        // Prefix resolution of HEAD itself.
        let prefix = &head[..10];
        let resolved = resolve_commit(&cm_dir_for(Path::new(&db)), prefix).unwrap();
        assert_eq!(resolved, head);
    }
    #[test]
    fn pull_propagates_state_changes() {
        // The P1 align-mode acceptance: a forget on A invalidates the lesson
        // on B after pull, and a re-validation re-activates it (只增 gap closed).
        let td = TestDir::new("align-pull");
        let db_a = td.db("a.db");
        let remote = td.db("remote");
        let db_b = td.db("b.db");

        // Helper: edge id by decision text.
        fn edge_id(db: &str, decision: &str) -> i64 {
            let store = CausalStore::open(db).unwrap();
            store
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT ce.id FROM causal_edges ce
                         JOIN chunks cf ON cf.id = ce.from_id
                         JOIN chunks ct ON ct.id = ce.to_id
                         WHERE cf.text = ?1 AND ct.text = ?2 LIMIT 1",
                        rusqlite::params![decision, outcome_of(decision)],
                        |r| r.get(0),
                    )
                    .map_err(|e| anyhow::anyhow!("{e}"))
                })
                .unwrap()
        }
        fn outcome_of(_d: &str) -> &str {
            "结果A"
        }
        fn set_valid_to(db: &str, id: i64, valid_to: Option<i64>) {
            let store = CausalStore::open(db).unwrap();
            store
                .with_conn(|conn| {
                    conn.execute(
                        "UPDATE causal_edges SET valid_to = ?1 WHERE id = ?2",
                        rusqlite::params![valid_to, id],
                    )?;
                    Ok(())
                })
                .unwrap();
        }

        // A: lesson valid, commit c1, push. B clones.
        let store = CausalStore::open(&db_a).unwrap();
        import_jsonl(&store, &seed_jsonl("教训A", "结果A", None), None, false).unwrap();
        drop(store);
        commit_all(&db_a, "c1: learned").unwrap();
        run_remote(&[
            "add".into(),
            "origin".into(),
            remote.clone(),
            "--db".into(),
            db_a.clone(),
        ])
        .unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();
        run_clone(&[remote.clone(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(export_edges(&db_b), (1, 0));

        // A forgets the lesson (valid_to = now) → commit c2 → push.
        let id = edge_id(&db_a, "教训A");
        set_valid_to(&db_a, id, Some(1700000100));
        commit_all(&db_a, "c2: forgotten").unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();

        // B pulls → the lesson is now invalidated locally (align, not skip).
        run_pull(&["origin".into(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(
            export_edges(&db_b),
            (0, 1),
            "forget must propagate via pull"
        );

        // A re-validates (e.g. restore edge) → commit c3 → push.
        set_valid_to(&db_a, id, None);
        commit_all(&db_a, "c3: revived").unwrap();
        run_push(&["origin".into(), "--db".into(), db_a.clone()]).unwrap();

        // B pulls → lesson valid again.
        run_pull(&["origin".into(), "--db".into(), db_b.clone()]).unwrap();
        assert_eq!(
            export_edges(&db_b),
            (1, 0),
            "re-validation must propagate via pull"
        );
        assert_eq!(head_of(&db_b), head_of(&db_a));
    }
}

/// Best-effort L0 one-liner for an end-of-session commit message (P2). Kept
/// single-line and ≤256 chars — it is a *fallback*: hosts that can produce a
/// real L0 summary (Hermes session title etc.) pass it via `-m`.
fn l0_message(stem: &str, decisions: usize, events: usize, lessons: usize) -> String {
    let msg =
        format!("session {stem}: {lessons} new lesson(s) ({decisions} decisions, {events} events)");
    msg.chars().take(256).collect()
}

/// Bounded excerpt of a parsed session for the LLM L0 summarizer (~3k chars,
/// keeps the per-session-end call cheap).
fn l0_excerpt(p: &causal_memory::session::ParsedSession) -> String {
    let mut out = String::new();
    for d in &p.decisions {
        out.push_str(&format!(
            "decision: {}\n",
            d.name.chars().take(160).collect::<String>()
        ));
        if out.chars().count() > 3000 {
            return out;
        }
    }
    for e in &p.events {
        out.push_str(&format!(
            "event: {} → {}\n",
            e.tool_name.chars().take(80).collect::<String>(),
            e.outcome.chars().take(80).collect::<String>()
        ));
        if out.chars().count() > 3000 {
            return out;
        }
    }
    for t in &p.assistant_texts {
        let one = t.chars().take(200).collect::<String>();
        out.push_str(&format!("assistant: {one}\n"));
        if out.chars().count() > 3000 {
            break;
        }
    }
    out
}

/// LLM one-line L0 summary (`--l0-llm`). `Ok(None)` = no LLM configured or no
/// session material (caller falls back to the heuristic); `Err` = the model
/// call failed — same fallback applies, so a flaky LLM never blocks the hook.
fn llm_l0(
    parsed: Option<&causal_memory::session::ParsedSession>,
    stem: &str,
) -> anyhow::Result<Option<String>> {
    use causal_memory::llm::{self, LlmConfig};
    let Some(cfg) = LlmConfig::from_env() else {
        return Ok(None);
    };
    let Some(p) = parsed else {
        return Ok(None);
    };
    let excerpt = l0_excerpt(p);
    if excerpt.trim().is_empty() {
        return Ok(None);
    }
    const SYS: &str = "You summarize an AI agent session into exactly ONE plain-text line \
                       (no quotes, no markdown, no emoji, ≤ 256 characters). Output only the summary.";
    let user = format!("Session: {stem}\n\nSession activity:\n{excerpt}\n\nOne-line summary:");
    let rt = tokio::runtime::Runtime::new()?;
    let content = rt.block_on(llm::chat(&cfg, SYS, &user, 70, 0.2))?;
    let one_line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = one_line.chars().take(256).collect();
    if capped.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(capped))
    }
}

/// `session-commit [<session-file|dir>]` — the `on_session_end` hook of P2
/// auto-commit: snapshot whatever lessons the session recorded (via `record`
/// / MCP record_decision) with a generated L0 message, and optionally push to
/// a remote so the cloud copy never goes stale.
///
///   session-commit [<session>] [--agent grok|claude|codex|kimi] [-m <msg>]
///                 [--l0-llm] [--push <remote>] [--db P]
///
/// The session argument is OPTIONAL: hosts that drive the hook themselves
/// (Hermes memory provider `on_session_end`) hold the conversation in memory,
/// not in a session file — they pass a real L0 via `-m` and no session path.
///
/// Message resolution order: `-m` (host-provided L0) > `--l0-llm` (LLM
/// one-line summary of the parsed session, ≤256 chars) > heuristic fallback
/// (`session <stem>: N new lesson(s) …`). The parse is advisory: an
/// unparseable/foreign session file does NOT block the commit — the hook must
/// never lose a session's recorded lessons to a format nit.
pub(crate) fn run_session_commit(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "Usage: causal-memory session-commit [<session-file|dir>] [--agent grok|claude|codex|kimi] [-m <msg>] [--l0-llm] [--push <remote>] [--db P]";
    let mut db: Option<PathBuf> = None;
    let mut push: Option<String> = None;
    let mut message: Option<String> = None;
    let mut agent: Option<String> = None;
    let mut l0_llm = false;
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--db requires a path\n{USAGE}")
                };
                db = Some(PathBuf::from(p));
            }
            "--push" => {
                i += 1;
                let Some(r) = args.get(i) else {
                    bail!("--push requires a remote name/path\n{USAGE}")
                };
                push = Some(r.clone());
            }
            "-m" | "--message" => {
                i += 1;
                let Some(m) = args.get(i) else {
                    bail!("-m requires a message\n{USAGE}")
                };
                message = Some(m.clone());
            }
            "--agent" => {
                i += 1;
                let Some(a) = args.get(i) else {
                    bail!("--agent requires a kind\n{USAGE}")
                };
                agent = Some(a.clone());
            }
            "--l0-llm" => l0_llm = true,
            s if s.starts_with("--") => bail!("unknown flag: {s}\n{USAGE}"),
            other => pos.push(other.to_string()),
        }
        i += 1;
    }
    if pos.len() > 1 {
        bail!("unexpected extra argument: {}\n{USAGE}", pos[1]);
    }
    let session_path: Option<&String> = pos.first();
    let db_path = db.unwrap_or_else(get_db_path);
    let cm = cm_dir_for(&db_path);

    // Best-effort parse (only when a session path was given) → parsed
    // material (for LLM L0) + counts (fallback msg). Host-driven commits
    // pass no path: nothing to parse, no warning.
    let stem = match session_path {
        Some(sp) => Path::new(sp)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| sp.clone()),
        None => "(auto)".to_string(),
    };
    let parsed: Option<causal_memory::session::ParsedSession> = match session_path {
        Some(sp) => {
            use causal_memory::session::{agent_kind_from_str, parser_for, SessionSource};
            let kind = agent
                .as_deref()
                .and_then(agent_kind_from_str)
                .unwrap_or(causal_memory::session::AgentKind::Grok);
            let src = if Path::new(sp).is_dir() {
                SessionSource::dir(sp)
            } else {
                SessionSource::file(sp)
            };
            match parser_for(kind).parse(&src) {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!(
                        "session-commit: parse warning ({e}); committing with fallback message"
                    );
                    None
                }
            }
        }
        None => None,
    };
    let (decisions, events) = parsed
        .as_ref()
        .map(|p| (p.decisions.len(), p.events.len()))
        .unwrap_or((0, 0));

    // How much uncommitted content does the working tree hold? Compare the
    // current export against the head snapshot. Snapshot scope = the causal
    // graph (edges + referenced chunk texts); orphan chunks and the fact
    // layer are intentionally not sync content — so an "empty" store here
    // means "no uncommitted causal content".
    let store = CausalStore::open(&db_path)?;
    let (lines, stats) = snapshot_data_lines(&store)?;
    let head = read_ref(&cm.join("refs/heads/main"));
    let new_lessons = match &head {
        Some(h) => read_object(&cm, h)
            .ok()
            .map(|(_, old)| stats.edges.saturating_sub(snapshot_edges(&old)))
            .unwrap_or(stats.edges),
        None => stats.edges,
    };
    fn snapshot_edges(old: &[String]) -> usize {
        old.iter()
            .filter(|l| l.contains("\"type\":\"edge\""))
            .count()
    }
    // A brand-new store must NOT get an empty genesis snapshot from the
    // auto-commit hook (review finding): a no-op first session would push a
    // meaningless empty commit (e3b0c442…) to the cloud. `commit` remains
    // the explicit way to baseline an empty store; the hook waits for the
    // first real lesson.
    if head.is_none() && lines.is_empty() {
        println!("session-commit: nothing to commit (store is empty — the first lesson will create the first snapshot)");
        return Ok(());
    }

    let msg = match message {
        Some(m) => m,
        None if l0_llm => match llm_l0(parsed.as_ref(), &stem) {
            Ok(Some(m)) => {
                eprintln!("session-commit: L0 via LLM");
                m
            }
            Ok(None) => {
                eprintln!("session-commit: --l0-llm requested but no LLM config / no parseable session — heuristic message");
                l0_message(&stem, decisions, events, new_lessons)
            }
            Err(e) => {
                eprintln!("session-commit: LLM L0 failed ({e:#}); heuristic message");
                l0_message(&stem, decisions, events, new_lessons)
            }
        },
        None => l0_message(&stem, decisions, events, new_lessons),
    };
    if msg.is_empty() {
        bail!("empty commit message");
    }

    run_commit(&[
        "-m".into(),
        msg.clone(),
        "--db".into(),
        db_path.to_string_lossy().into_owned(),
    ])?;
    if let Some(remote) = push {
        run_push(&[
            remote,
            "--db".into(),
            db_path.to_string_lossy().into_owned(),
        ])?;
    }
    println!("session-commit: {msg}");
    Ok(())
}

#[cfg(test)]
mod l0_tests {
    use super::*;

    #[test]
    fn l0_message_is_single_line_and_capped() {
        let m = l0_message(&"session-abc".to_string(), 123, 456, 3);
        assert!(m.chars().count() <= 256);
        assert!(!m.contains('\n'));
        assert!(m.contains("new lesson(s)"));
        // A pathological stem is still capped (phrase may be truncated away).
        let m2 = l0_message(&"x".repeat(400), 0, 0, 0);
        assert!(m2.chars().count() <= 256);
        assert!(!m2.contains('\n'));
    }

    #[test]
    fn l0_excerpt_bounded_and_empty_on_empty_session() {
        let empty = causal_memory::session::ParsedSession::default();
        assert_eq!(l0_excerpt(&empty), "");
        // A crowded session still stays bounded.
        let mut p = causal_memory::session::ParsedSession::default();
        for i in 0..50 {
            p.decisions.push(causal_memory::session::CandidateDecision {
                id: format!("d{i}"),
                name: format!("decision number {i} ").repeat(30),
                arguments: "{}".into(),
            });
        }
        p.assistant_texts = vec!["long reasoning text ".repeat(500)];
        let ex = l0_excerpt(&p);
        assert!(ex.chars().count() <= 3200, "len {}", ex.chars().count());
    }

    #[test]
    fn llm_l0_returns_none_without_session_material() {
        // Env-independent short circuits: no parsed session, or an empty one,
        // must never reach a network call.
        assert!(llm_l0(None, "s.jsonl").unwrap().is_none());
        let empty = causal_memory::session::ParsedSession::default();
        assert!(llm_l0(Some(&empty), "s.jsonl").unwrap().is_none());
    }
}
