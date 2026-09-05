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

use crate::commands::io::{export_jsonl, import_jsonl, ExportFilters, ExportStats};
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
fn atomic_write(path: &Path, content: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("path has no parent dir")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), rand_suffix()));
    std::fs::write(&tmp, content)?;
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

/// Resolve a push/pull/clone target to a local path. Resolution order:
/// named remote from config (or default "origin") > literal path (file://
/// stripped, "~" expanded). Anything else that looks like a bare agent_id
/// (no slash, not an existing path) is a P1 registry concern.
fn resolve_target(cm: &Path, target: Option<&str>, default_name: &str) -> anyhow::Result<PathBuf> {
    let cfg = read_config(cm)?;
    let remotes = remotes_of(&cfg);
    let t = match target {
        None => {
            let url = remotes.get(default_name).context(format!(
                "no remote named '{default_name}' configured (remote add {default_name} <path> or pass a path)"
            ))?;
            url.clone()
        }
        Some(t) => {
            if let Some(url) = remotes.get(t) {
                url.clone()
            } else if looks_like_path(t) {
                t.to_string()
            } else {
                bail!(
                    "'{t}' is neither a configured remote ({}) nor a path; \
                     bare agent_id resolution is a P1 registry feature (https)",
                    remotes.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            }
        }
    };
    Ok(normalize_path(&t))
}

fn looks_like_path(t: &str) -> bool {
    t.starts_with("file://")
        || t.contains('/')
        || t == "."
        || t == ".."
        || t.starts_with("./")
        || t.starts_with("../")
        || t.starts_with('~')
        || Path::new(t).exists()
}

fn normalize_path(t: &str) -> PathBuf {
    let t = t.strip_prefix("file://").unwrap_or(t);
    if let Some(rest) = t.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(t)
}

/// Mirror commit object files from a remote into the local `.cm/objects` so
/// `log` / `checkout` work offline afterwards (git fetch semantics: refs only
/// point at objects you actually hold).
fn mirror_objects(src_dir: &Path, dst_dir: &Path, hashes: &[String]) -> anyhow::Result<()> {
    for h in hashes {
        let dst = dst_dir.join("objects").join(h);
        if !dst.exists() {
            std::fs::copy(src_dir.join("objects").join(h), &dst)?;
        }
    }
    Ok(())
}

/// Read a commit object's (meta, data lines). Verifies hash == filename.
fn read_object(cm: &Path, hash: &str) -> anyhow::Result<(CommitMeta, Vec<String>)> {
    if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.len() != 64 {
        bail!("invalid commit hash: {hash}");
    }
    let path = cm.join("objects").join(hash);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("commit {:.8} not found locally (pull first?)", short(hash)))?;
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
    let remote = resolve_target(&cm, target.as_deref(), "origin")?;
    let local_head = match read_ref(&cm.join("refs/heads/main")) {
        Some(h) => h,
        None => {
            println!("nothing to push (no commits)");
            return Ok(());
        }
    };
    let remote_ref_file = remote.join("refs/heads/main");
    let remote_head = read_ref(&remote_ref_file);

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
    std::fs::create_dir_all(remote.join("objects"))?;
    std::fs::create_dir_all(
        remote_ref_file
            .parent()
            .context("remote ref has no parent")?,
    )?;
    for h in to_push.iter().rev() {
        let raw = std::fs::read_to_string(cm.join("objects").join(h))
            .with_context(|| format!("local object {:.8} missing (corrupt .cm?)", short(h)))?;
        atomic_write(&remote.join("objects").join(h), &raw)?;
    }
    atomic_write(&remote_ref_file, &local_head)?;
    println!("pushed {} commit(s) → {}", to_push.len(), remote.display());
    Ok(())
}

/// Import a chain of commit snapshots (oldest first) into `store`. Returns
/// cumulative ImportStats across all snapshots.
fn import_chain(
    store: &CausalStore,
    remote_dir: &Path,
    chain_oldest_first: &[String],
) -> anyhow::Result<ImportStatsSum> {
    let mut sum = ImportStatsSum::default();
    for h in chain_oldest_first {
        let (meta, data) = read_object(remote_dir, h)?; // verify integrity
        if meta.format_version != FORMAT_VERSION {
            bail!("unsupported commit format_version {}", meta.format_version);
        }
        let stats = import_jsonl(store, &data.join("\n"), None, false)?;
        sum.imported += stats.imported;
        sum.skipped_duplicate += stats.skipped_duplicate;
        sum.skipped_invalid += stats.skipped_invalid;
    }
    Ok(sum)
}

#[derive(Default)]
struct ImportStatsSum {
    imported: usize,
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
    let remote = resolve_target(&cm, target.as_deref(), "origin")?;
    if !remote.join("refs/heads/main").exists() {
        println!("nothing to pull (remote is empty)");
        return Ok(());
    }
    let remote_head = read_ref(&remote.join("refs/heads/main")).context("remote ref unreadable")?;
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
        let (meta, _) = read_object(&remote, &cur)?;
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
        "  imported {} · skipped_duplicate {} · skipped_invalid {}",
        sum.imported, sum.skipped_duplicate, sum.skipped_invalid
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
    // Named-remote or agent_id resolution happens against an empty config
    // (fresh clone) — only paths/URLs make sense pre-config.
    let remote = if let Some(url) = remotes_of(&read_config(&cm)?).get(&target) {
        normalize_path(url)
    } else if looks_like_path(&target) {
        normalize_path(&target)
    } else {
        bail!("'{target}' is not a path; bare agent_id resolution is a P1 registry feature (https)")
    };
    if !remote.join("refs/heads/main").exists() {
        bail!("nothing to clone (remote is empty): {}", remote.display());
    }
    let store = CausalStore::open(&db_path)?;
    let remote_head = read_ref(&remote.join("refs/heads/main")).context("remote ref unreadable")?;
    let mut chain: Vec<String> = Vec::new();
    let mut cur = remote_head.clone();
    loop {
        chain.push(cur.clone());
        let (meta, _) = read_object(&remote, &cur)?;
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
    // Remember the source as origin (git clone semantics).
    let mut cfg = read_config(&cm)?;
    cfg["remotes"]["origin"] = serde_json::json!({ "url": format!("file://{}", remote.display()) });
    write_config(&cm, &cfg)?;

    // Bootstrap summary: newest commit meta + last 3 valid lessons.
    let (head_meta, _) = read_object(&cm, &remote_head)?;
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
    println!("  origin → file://{}", remote.display());
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
}
