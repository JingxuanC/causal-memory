//! Export / import subcommands.

use crate::get_db_path;
use causal_memory::store::CausalStore;
use rusqlite::OptionalExtension;
use std::path::PathBuf;

fn fnv1a(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("imp{h:016x}")
}

/// Best-effort secret redaction (no regex dep; documented as best-effort in
/// --help): sk-… tokens, Bearer tokens, password-style assignments, and
/// private-key headers are replaced with [REDACTED]. Returns the redacted
/// text and the number of redactions applied.
pub(crate) fn redact(text: &str) -> (String, usize) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut redacted_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut count = 0;

    for (i, w) in words.iter().enumerate() {
        // sk-<secret> (API keys)
        if let Some(rest) = w.strip_prefix("sk-") {
            let key: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if key.len() >= 8 {
                redacted_idx.insert(i);
                count += 1;
                continue;
            }
        }
        // Bearer <token>
        if *w == "Bearer" {
            if let Some(next) = words.get(i + 1) {
                if next.chars().filter(|c| c.is_alphanumeric()).count() >= 4 {
                    redacted_idx.insert(i + 1);
                    count += 1;
                }
            }
            continue;
        }
        // password=… / password: … (case-insensitive, also passwd). When the
        // value is in the NEXT word ("password: hunter2"), redact it too.
        let lower = w.to_lowercase();
        for prefix in ["password", "passwd"] {
            if let Some(pos) = lower.find(prefix) {
                let after = &w[pos + prefix.len()..];
                if after.starts_with('=') || after.starts_with(':') {
                    redacted_idx.insert(i);
                    count += 1;
                    if after.len() == 1 {
                        if let Some(next) = words.get(i + 1) {
                            if !next.is_empty() {
                                redacted_idx.insert(i + 1);
                            }
                        }
                    }
                }
                break;
            }
        }
        // -----BEGIN … PRIVATE KEY----- (split on whitespace, so the closing
        // word is just "…KEY-----")
        if *w == "-----BEGIN" {
            for j in i + 1..=(i + 3).min(words.len().saturating_sub(1)) {
                if j < words.len() && words[j].ends_with("KEY-----") {
                    for k in i..=j {
                        redacted_idx.insert(k);
                    }
                    count += 1;
                    break;
                }
            }
        }
    }

    if count == 0 {
        return (text.to_string(), 0);
    }
    let mut out_words: Vec<&str> = Vec::with_capacity(words.len());
    for (i, w) in words.iter().enumerate() {
        if redacted_idx.contains(&i) {
            out_words.push("[REDACTED]");
        } else {
            out_words.push(w);
        }
    }
    (out_words.join(" "), count)
}

pub(crate) struct ExportFilters {
    pub(crate) task_tag: Option<String>,
    pub(crate) min_confidence: f64,
    pub(crate) since: i64,
    pub(crate) include_invalidated: bool,
    pub(crate) redact: bool,
}

#[derive(Default, Debug)]
pub(crate) struct ExportStats {
    pub(crate) chunks: usize,
    pub(crate) edges: usize,
    pub(crate) meta_edges: usize,
    pub(crate) redacted: usize,
}

/// Serialize the store to JSONL lines (header first, then chunks, edges,
/// meta edges). Pure read path; fully testable without a file.
pub(crate) fn export_jsonl(
    store: &CausalStore,
    f: &ExportFilters,
) -> anyhow::Result<(Vec<String>, ExportStats)> {
    let mut stats = ExportStats::default();
    let mut lines = Vec::new();
    let mut chunks: std::collections::HashMap<String, (String, i64)> =
        std::collections::HashMap::new();

    lines.push(
        serde_json::json!({
            "type": "header",
            "format_version": 1,
            "exported_at": chrono::Utc::now().timestamp(),
            "source": "causal-memory",
        })
        .to_string(),
    );

    // ── causal edges (filtered) ──
    let mut sql = String::from(
        "SELECT cf.id, cf.text, cf.created_at, ct.id, ct.text, ct.created_at,
                ce.relation, ce.confidence, ce.task_tag, ce.event_time,
                ce.discovered_at, ce.valid_to, ce.discovered_by, ce.outcome_polarity
         FROM causal_edges ce
         JOIN chunks cf ON cf.id = ce.from_id
         JOIN chunks ct ON ct.id = ce.to_id
         WHERE ce.confidence >= ?1 AND ce.event_time >= ?2",
    );
    let mut bind: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(f.min_confidence), Box::new(f.since)];
    if !f.include_invalidated {
        sql.push_str(" AND ce.valid_to IS NULL");
    }
    if let Some(t) = &f.task_tag {
        sql.push_str(" AND ce.task_tag = ?3");
        bind.push(Box::new(t.clone()));
    }
    sql.push_str(" ORDER BY ce.id");

    store.with_conn(|conn| {
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, f64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
            ))
        })?;
        for row in rows {
            let (fid, ftext, fcat, tid, ttext, tcat, rel, conf, tag, et, dat, vto, dby, pol) = row?;
            chunks.entry(fid.clone()).or_insert((ftext, fcat));
            chunks.entry(tid.clone()).or_insert((ttext, tcat));
            lines.push(
                serde_json::json!({
                    "type": "edge",
                    "from_id": fid, "to_id": tid,
                    "relation": rel, "confidence": conf, "task_tag": tag,
                    "event_time": et, "discovered_at": dat, "valid_to": vto,
                    "discovered_by": dby, "outcome_polarity": pol,
                })
                .to_string(),
            );
            stats.edges += 1;
        }

        // ── meta edges (only the invalidated filter applies) ──
        let meta_sql = if f.include_invalidated {
            "SELECT m.from_id, cf.text, cf.created_at, m.to_id, ct.text, ct.created_at,
                    m.relation, m.pattern, m.confidence, m.discovered_at, m.valid_to,
                    m.strata_count, m.strata, m.confounded, m.simpson
             FROM meta_causal_edges m
             JOIN chunks cf ON cf.id = m.from_id
             JOIN chunks ct ON ct.id = m.to_id
             ORDER BY m.id"
        } else {
            "SELECT m.from_id, cf.text, cf.created_at, m.to_id, ct.text, ct.created_at,
                    m.relation, m.pattern, m.confidence, m.discovered_at, m.valid_to,
                    m.strata_count, m.strata, m.confounded, m.simpson
             FROM meta_causal_edges m
             JOIN chunks cf ON cf.id = m.from_id
             JOIN chunks ct ON ct.id = m.to_id
             WHERE m.valid_to IS NULL
             ORDER BY m.id"
        };
        let mut stmt = conn.prepare(meta_sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, f64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<bool>>(13)?,
                row.get::<_, Option<bool>>(14)?,
            ))
        })?;
        for row in rows {
            let (fid, ftext, fcat, tid, ttext, tcat, rel, pat, conf, dat, vto, sc, s, cfd, sim) =
                row?;
            chunks.entry(fid.clone()).or_insert((ftext, fcat));
            chunks.entry(tid.clone()).or_insert((ttext, tcat));
            lines.push(
                serde_json::json!({
                    "type": "meta_edge",
                    "from_id": fid, "to_id": tid,
                    "relation": rel, "pattern": pat, "confidence": conf,
                    "discovered_at": dat, "valid_to": vto,
                    "strata_count": sc, "strata": s,
                    "confounded": cfd, "simpson": sim,
                })
                .to_string(),
            );
            stats.meta_edges += 1;
        }
        Ok(())
    })?;

    // ── chunks (deduped, redacted) — emitted before edges reference them? ──
    // Edges reference chunk ids only, so order is free; chunks go right after
    // the header for readability. Insert at position 1.
    let mut chunk_lines = Vec::new();
    let mut sorted: Vec<_> = chunks.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, (text, created_at)) in sorted {
        let (text, n) = if f.redact { redact(&text) } else { (text, 0) };
        stats.redacted += n;
        chunk_lines.push(
            serde_json::json!({
                "type": "chunk", "id": id, "text": text, "created_at": created_at,
            })
            .to_string(),
        );
        stats.chunks += 1;
    }
    let header = lines.remove(0);
    let mut out = vec![header];
    out.extend(chunk_lines);
    out.extend(lines);
    Ok((out, stats))
}

pub(crate) fn run_export(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "Usage: causal-memory export <file.jsonl> [--db <PATH>] [--task-tag X] [--min-confidence 0.5] [--since <unix_ts>] [--include-invalidated] [--no-redact]
  Text is best-effort redacted (sk-…, Bearer, password=…, private-key headers) unless --no-redact.";
    let mut file: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut f = ExportFilters {
        task_tag: None,
        min_confidence: 0.0,
        since: 0,
        include_invalidated: false,
        redact: true,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--db requires a path\n{USAGE}")
                };
                db = Some(PathBuf::from(p));
            }
            "--task-tag" => {
                i += 1;
                let Some(t) = args.get(i) else {
                    anyhow::bail!("--task-tag requires a value\n{USAGE}")
                };
                f.task_tag = Some(t.clone());
            }
            "--min-confidence" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    anyhow::bail!("--min-confidence requires a number\n{USAGE}")
                };
                f.min_confidence = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--min-confidence must be a float, got: {v}"))?;
            }
            "--since" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    anyhow::bail!("--since requires a unix timestamp\n{USAGE}")
                };
                f.since = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--since must be an integer, got: {v}"))?;
            }
            "--include-invalidated" => f.include_invalidated = true,
            "--no-redact" => f.redact = false,
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}\n{USAGE}"),
            positional => {
                if file.is_some() {
                    anyhow::bail!("unexpected extra argument: {positional}\n{USAGE}");
                }
                file = Some(PathBuf::from(positional));
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("{USAGE}");
        std::process::exit(1);
    };

    let db_path = db.unwrap_or_else(get_db_path);
    let store = CausalStore::open(&db_path)?;
    let (lines, stats) = export_jsonl(&store, &f)?;
    std::fs::write(&file, lines.join("\n") + "\n")?;

    println!("=== Export complete ===");
    println!("DB:   {}", db_path.display());
    println!("File: {}", file.display());
    println!("  chunks:      {}", stats.chunks);
    println!("  edges:       {}", stats.edges);
    println!("  meta edges:  {}", stats.meta_edges);
    println!(
        "  redacted:    {}{}",
        stats.redacted,
        if f.redact {
            ""
        } else {
            " (redaction disabled)"
        }
    );
    Ok(())
}

#[derive(Default, Debug)]
pub(crate) struct ImportStats {
    pub(crate) imported: usize,
    pub(crate) skipped_duplicate: usize,
    pub(crate) skipped_invalid: usize,
}

/// Parse and import JSONL produced by export_jsonl. With `dry_run`, reads and
/// dedup checks run against `store` but nothing is written. Bad lines are
/// counted and skipped, never fatal. Dedup key for edges:
/// (from_text, to_text, relation, event_time); meta edges:
/// (from_text, to_text, relation); chunks: FNV-1a(text) id, INSERT OR IGNORE.
pub(crate) fn import_jsonl(
    store: &CausalStore,
    content: &str,
    task_tag_override: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<ImportStats> {
    let mut stats = ImportStats::default();
    // source chunk id → text (chunk lines precede their referrers in exports)
    let mut chunk_texts: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            stats.skipped_invalid += 1;
            continue;
        };
        match v["type"].as_str() {
            Some("header") => {
                if v["format_version"].as_i64() != Some(1) {
                    anyhow::bail!(
                        "unsupported format_version {:?} (expected 1)",
                        v["format_version"]
                    );
                }
            }
            Some("chunk") => {
                let (Some(id), Some(text)) = (v["id"].as_str(), v["text"].as_str()) else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                chunk_texts.insert(id.to_string(), text.to_string());
                if !dry_run {
                    let created_at = v["created_at"].as_i64().unwrap_or(0);
                    store.with_conn(|conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(text), text, created_at],
                        )?;
                        Ok(())
                    })?;
                }
            }
            Some("edge") => {
                let (Some(fid), Some(tid), Some(rel)) = (
                    v["from_id"].as_str(),
                    v["to_id"].as_str(),
                    v["relation"].as_str(),
                ) else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                let (Some(ftext), Some(ttext)) = (chunk_texts.get(fid), chunk_texts.get(tid))
                else {
                    stats.skipped_invalid += 1; // chunk line missing
                    continue;
                };
                let event_time = v["event_time"].as_i64().unwrap_or(0);
                let tag = task_tag_override
                    .map(String::from)
                    .or_else(|| v["task_tag"].as_str().map(String::from));

                // Dedup: (from_text, to_text, relation, event_time).
                let dup = store.with_conn(|conn| {
                    Ok(conn
                        .query_row(
                            "SELECT ce.id FROM causal_edges ce
                             JOIN chunks cf ON cf.id = ce.from_id
                             JOIN chunks ct ON ct.id = ce.to_id
                             WHERE cf.text = ?1 AND ct.text = ?2 AND ce.relation = ?3 AND ce.event_time = ?4
                             LIMIT 1",
                            rusqlite::params![ftext, ttext, rel, event_time],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?
                        .is_some())
                })?;
                if dup {
                    stats.skipped_duplicate += 1;
                    continue;
                }
                if !dry_run {
                    let confidence = v["confidence"].as_f64().unwrap_or(0.5);
                    let discovered_at = v["discovered_at"]
                        .as_i64()
                        .unwrap_or_else(|| chrono::Utc::now().timestamp());
                    let valid_to = v["valid_to"].as_i64();
                    let discovered_by = v["discovered_by"].as_str().unwrap_or("llm_inferred");
                    let polarity = v["outcome_polarity"].as_str();
                    store.with_conn(|conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ftext), ftext, event_time],
                        )?;
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ttext), ttext, event_time],
                        )?;
                        conn.execute(
                            "INSERT INTO causal_edges
                                 (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, valid_to, task_tag, outcome_polarity)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                            rusqlite::params![
                                fnv1a(ftext), fnv1a(ttext), rel, confidence,
                                discovered_by, event_time, discovered_at, valid_to, tag, polarity
                            ],
                        )?;
                        Ok(())
                    })?;
                }
                stats.imported += 1;
            }
            Some("meta_edge") => {
                let (Some(fid), Some(tid), Some(rel)) = (
                    v["from_id"].as_str(),
                    v["to_id"].as_str(),
                    v["relation"].as_str(),
                ) else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                let (Some(ftext), Some(ttext)) = (chunk_texts.get(fid), chunk_texts.get(tid))
                else {
                    stats.skipped_invalid += 1;
                    continue;
                };
                let dup = store.with_conn(|conn| {
                    Ok(conn
                        .query_row(
                            "SELECT m.id FROM meta_causal_edges m
                             JOIN chunks cf ON cf.id = m.from_id
                             JOIN chunks ct ON ct.id = m.to_id
                             WHERE cf.text = ?1 AND ct.text = ?2 AND m.relation = ?3
                             LIMIT 1",
                            rusqlite::params![ftext, ttext, rel],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?
                        .is_some())
                })?;
                if dup {
                    stats.skipped_duplicate += 1;
                    continue;
                }
                if !dry_run {
                    let confidence = v["confidence"].as_f64().unwrap_or(0.5);
                    let discovered_at = v["discovered_at"]
                        .as_i64()
                        .unwrap_or_else(|| chrono::Utc::now().timestamp());
                    let pattern = v["pattern"].as_str();
                    let valid_to = v["valid_to"].as_i64();
                    store.with_conn(|conn| {
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ftext), ftext, discovered_at],
                        )?;
                        conn.execute(
                            "INSERT OR IGNORE INTO chunks (id, text, created_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![fnv1a(ttext), ttext, discovered_at],
                        )?;
                        conn.execute(
                            "INSERT INTO meta_causal_edges
                                 (from_id, to_id, relation, pattern, confidence, discovered_at, valid_from, valid_to,
                                  strata_count, strata, confounded, simpson)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                            rusqlite::params![
                                fnv1a(ftext), fnv1a(ttext), rel, pattern, confidence,
                                discovered_at, discovered_at, valid_to,
                                v["strata_count"].as_i64(),
                                v["strata"].as_str(),
                                v["confounded"].as_bool(),
                                v["simpson"].as_bool(),
                            ],
                        )?;
                        Ok(())
                    })?;
                }
                stats.imported += 1;
            }
            _ => stats.skipped_invalid += 1,
        }
    }
    Ok(stats)
}

pub(crate) fn run_import(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str =
        "Usage: causal-memory import <file.jsonl> [--db <PATH>] [--dry-run] [--task-tag Y]
  --task-tag Y tags all imported edges (e.g. the source agent's name).";
    let mut file: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut tag: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--db requires a path\n{USAGE}")
                };
                db = Some(PathBuf::from(p));
            }
            "--dry-run" => dry_run = true,
            "--task-tag" => {
                i += 1;
                let Some(t) = args.get(i) else {
                    anyhow::bail!("--task-tag requires a value\n{USAGE}")
                };
                tag = Some(t.clone());
            }
            other if other.starts_with("--") => anyhow::bail!("unknown flag: {other}\n{USAGE}"),
            positional => {
                if file.is_some() {
                    anyhow::bail!("unexpected extra argument: {positional}\n{USAGE}");
                }
                file = Some(PathBuf::from(positional));
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("{USAGE}");
        std::process::exit(1);
    };

    let content = std::fs::read_to_string(&file)?;
    let db_path = db.unwrap_or_else(get_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = CausalStore::open(&db_path)?;
    let stats = import_jsonl(&store, &content, tag.as_deref(), dry_run)?;

    println!(
        "=== Import complete{} ===",
        if dry_run { " (DRY RUN)" } else { "" }
    );
    println!("  imported:          {}", stats.imported);
    println!("  skipped_duplicate: {}", stats.skipped_duplicate);
    println!("  skipped_invalid:   {}", stats.skipped_invalid);
    Ok(())
}
