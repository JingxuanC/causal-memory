//! Schema migration for the causal store.
//!
//! Uses `PRAGMA user_version` as the version marker, with `PRAGMA table_info`
//! as a fallback probe (v0.6 DBs have the temporal columns but no marker).
//! Every step checks actual column existence before altering, so migrations
//! are idempotent and safe to re-run.
//!
//! Version history:
//! - v0/v1: pre-v0.6 schema — `causal_edges` lacks `event_time` /
//!   `discovered_at` / `valid_to` (may have `created_at` instead), and a
//!   bare `meta_causal_edges` may lack `discovered_at` / `valid_from` /
//!   `valid_to`. All variants are patched column-by-column.
//! - v2 (v0.6): adds the three temporal columns to `causal_edges`.
//! - v3: adds `access_count` / `last_accessed_at` to `causal_edges`,
//!   the `edge_embeddings` table, and meta-edge indexes.
//! - v4: adds `outcome_polarity` to `causal_edges` (write-time LLM/heuristic
//!   judgment: positive / negative / mixed / neutral; NULL for legacy rows).

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};

use crate::store::CAUSAL_SCHEMA_SQL;

/// Current schema version. Bump when adding a new migration step.
pub const SCHEMA_VERSION: u32 = 4;

/// Bring `conn` up to `SCHEMA_VERSION`. Runs in a single transaction:
/// any failure rolls everything back.
///
/// Order matters: column migrations run before `CAUSAL_SCHEMA_SQL` because
/// the v3 indexes reference columns that older DBs don't have yet.
pub fn migrate(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;

    let version = detect_version(&tx)?;
    if version > SCHEMA_VERSION {
        return Err(anyhow!(
            "DB schema version {version} is newer than supported {SCHEMA_VERSION}"
        ));
    }
    if version < 2 {
        migrate_to_v2(&tx)?;
    }
    if version < 3 {
        migrate_to_v3(&tx)?;
    }
    if version < 4 {
        migrate_to_v4(&tx)?;
    }

    // Creates any missing tables/indexes at v3 (no-op for existing ones).
    tx.execute_batch(CAUSAL_SCHEMA_SQL)?;

    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

/// Detect the schema version: trust `PRAGMA user_version` when set,
/// otherwise fall back to probing actual columns (unmarked v0.6 DBs).
fn detect_version(conn: &Connection) -> Result<u32> {
    let marked: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if marked > 0 {
        return Ok(marked as u32);
    }
    if !table_exists(conn, "causal_edges")? {
        // No marker and no table: fresh DB, will be created at v3 below.
        return Ok(SCHEMA_VERSION);
    }
    let cols = table_columns(conn, "causal_edges")?;
    if cols.contains("outcome_polarity") {
        Ok(4)
    } else if cols.contains("access_count") {
        Ok(3)
    } else if cols.contains("event_time") {
        Ok(2)
    } else {
        Ok(1)
    }
}

/// v1 → v2: add the v0.6 temporal columns, backfilling existing rows from
/// `created_at` when that legacy column exists, else with the current time.
fn migrate_to_v2(conn: &Connection) -> Result<()> {
    let cols = table_columns(conn, "causal_edges")?;
    let now = chrono::Utc::now().timestamp();
    let has_created_at = cols.contains("created_at");

    for (col, ddl) in [
        (
            "event_time",
            "ALTER TABLE causal_edges ADD COLUMN event_time INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "discovered_at",
            "ALTER TABLE causal_edges ADD COLUMN discovered_at INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        if cols.contains(col) {
            continue;
        }
        conn.execute_batch(ddl)?;
        // Only rows that just received the default (0) need backfilling.
        if has_created_at {
            conn.execute(
                &format!("UPDATE causal_edges SET {col} = created_at WHERE {col} = 0"),
                [],
            )?;
        } else {
            conn.execute(
                &format!("UPDATE causal_edges SET {col} = ?1 WHERE {col} = 0"),
                params![now],
            )?;
        }
    }

    if !cols.contains("valid_to") {
        conn.execute_batch("ALTER TABLE causal_edges ADD COLUMN valid_to INTEGER")?;
    }

    // The legacy column is NOT NULL with no default: keeping it would break
    // every post-migration INSERT (which only writes v3 columns). The v3
    // schema has no `created_at` on causal_edges, so drop it after backfill.
    if has_created_at {
        conn.execute_batch("ALTER TABLE causal_edges DROP COLUMN created_at")?;
    }

    // Pre-v0.6 DBs may also carry a bare `meta_causal_edges` table lacking
    // the temporal columns — the v3 indexes on (to_id, valid_to) and every
    // meta-edge INSERT need them. Patch column-by-column, same as above.
    if table_exists(conn, "meta_causal_edges")? {
        let meta_cols = table_columns(conn, "meta_causal_edges")?;
        if !meta_cols.contains("discovered_at") {
            conn.execute_batch(
                "ALTER TABLE meta_causal_edges ADD COLUMN discovered_at INTEGER NOT NULL DEFAULT 0",
            )?;
            conn.execute(
                "UPDATE meta_causal_edges SET discovered_at = ?1 WHERE discovered_at = 0",
                params![now],
            )?;
        }
        if !meta_cols.contains("valid_from") {
            conn.execute_batch("ALTER TABLE meta_causal_edges ADD COLUMN valid_from INTEGER")?;
        }
        if !meta_cols.contains("valid_to") {
            conn.execute_batch("ALTER TABLE meta_causal_edges ADD COLUMN valid_to INTEGER")?;
        }
    }
    Ok(())
}

/// v2 → v3: access-tracking columns. The `edge_embeddings` table and the
/// meta-edge indexes are created (IF NOT EXISTS) by `CAUSAL_SCHEMA_SQL`.
fn migrate_to_v3(conn: &Connection) -> Result<()> {
    let cols = table_columns(conn, "causal_edges")?;
    if !cols.contains("access_count") {
        conn.execute_batch(
            "ALTER TABLE causal_edges ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !cols.contains("last_accessed_at") {
        conn.execute_batch("ALTER TABLE causal_edges ADD COLUMN last_accessed_at INTEGER")?;
    }
    Ok(())
}

/// v3 → v4: write-time outcome polarity. Existing rows stay NULL (read paths
/// fall back to the signal-word heuristic); the `polarity` CLI subcommand
/// backfills them on demand.
fn migrate_to_v4(conn: &Connection) -> Result<()> {
    let cols = table_columns(conn, "causal_edges")?;
    if !cols.contains("outcome_polarity") {
        conn.execute_batch(
            "ALTER TABLE causal_edges ADD COLUMN outcome_polarity TEXT
             CHECK(outcome_polarity IN ('positive','negative','mixed','neutral'))",
        )?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CausalStore;

    /// Build a pre-v0.6 (v1) DB: no temporal columns, no access columns,
    /// no user_version marker. Two edges with a legacy `created_at` column.
    fn build_v1_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE chunks (
                id TEXT PRIMARY KEY,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE causal_edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.5,
                discovered_by TEXT NOT NULL DEFAULT 'llm_inferred',
                task_tag TEXT,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO chunks (id, text, created_at) VALUES
                ('d1', 'decision one', 1000),
                ('o1', 'outcome one', 1000),
                ('d2', 'decision two', 2000),
                ('o2', 'outcome two', 2000);
            INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, task_tag, created_at)
            VALUES
                ('d1', 'o1', 'caused', 0.8, 'llm_inferred', 'tag-a', 1000),
                ('d2', 'o2', 'caused', 0.9, 'user_feedback', NULL, 2000);",
        )
        .unwrap();
        conn
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_migrate_v1_db() {
        let conn = build_v1_db();
        migrate(&conn).unwrap();

        assert_eq!(user_version(&conn), 4);

        let cols = table_columns(&conn, "causal_edges").unwrap();
        for col in [
            "event_time",
            "discovered_at",
            "valid_to",
            "access_count",
            "last_accessed_at",
            "outcome_polarity",
        ] {
            assert!(cols.contains(col), "missing column {col}");
        }

        // Old rows backfilled from created_at, data intact.
        let rows: Vec<(i64, i64, i64, i64)> = conn
            .prepare(
                "SELECT id, event_time, discovered_at, access_count FROM causal_edges ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (1, 1000, 1000, 0));
        assert_eq!(rows[1], (2, 2000, 2000, 0));

        // valid_to stays NULL for old rows.
        let null_valid_to: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE valid_to IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_valid_to, 2);

        // Legacy rows get NULL polarity (read paths fall back to heuristic).
        let null_polarity: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE outcome_polarity IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_polarity, 2);

        assert!(table_exists(&conn, "edge_embeddings").unwrap());
    }

    /// Reproduces a real-world pre-v0.6 DB shape (found in the wild):
    /// `causal_edges` already has `discovered_at` (no `created_at`), and a
    /// bare `meta_causal_edges` without any temporal columns.
    #[test]
    fn test_migrate_real_world_v1_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE chunks (
                id TEXT PRIMARY KEY,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE causal_edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.5,
                discovered_by TEXT NOT NULL DEFAULT 'llm_inferred',
                discovered_at INTEGER NOT NULL,
                task_tag TEXT
            );
            CREATE TABLE meta_causal_edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                pattern TEXT,
                confidence REAL NOT NULL DEFAULT 0.5
            );
            INSERT INTO chunks (id, text, created_at) VALUES ('d1', 'decision', 1000), ('o1', 'outcome', 1000);
            INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_at)
                VALUES ('d1', 'o1', 'caused', 0.8, 1000);
            INSERT INTO meta_causal_edges (from_id, to_id, relation, pattern, confidence)
                VALUES ('d1', 'd1', 'similar_to', 'test pattern', 0.6);",
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(user_version(&conn), 4);

        let edge_cols = table_columns(&conn, "causal_edges").unwrap();
        for col in [
            "event_time",
            "discovered_at",
            "valid_to",
            "access_count",
            "last_accessed_at",
            "outcome_polarity",
        ] {
            assert!(edge_cols.contains(col), "causal_edges missing {col}");
        }
        // Existing discovered_at preserved, event_time backfilled non-zero.
        let (event_time, discovered_at): (i64, i64) = conn
            .query_row(
                "SELECT event_time, discovered_at FROM causal_edges WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(discovered_at, 1000);
        assert!(event_time > 0);

        let meta_cols = table_columns(&conn, "meta_causal_edges").unwrap();
        for col in ["discovered_at", "valid_from", "valid_to"] {
            assert!(meta_cols.contains(col), "meta_causal_edges missing {col}");
        }
        // Meta row backfilled, indexes on the new columns now work.
        let meta_discovered: i64 = conn
            .query_row(
                "SELECT discovered_at FROM meta_causal_edges WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(meta_discovered > 0);
        conn.execute_batch("SELECT 1 FROM meta_causal_edges INDEXED BY idx_meta_valid LIMIT 1")
            .unwrap();
    }

    #[test]
    fn test_migrate_idempotent() {
        let conn = build_v1_db();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        assert_eq!(user_version(&conn), 4);
        let snapshot: Vec<(i64, i64, i64, i64)> = conn
            .prepare(
                "SELECT id, event_time, discovered_at, access_count FROM causal_edges ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(snapshot, vec![(1, 1000, 1000, 0), (2, 2000, 2000, 0)]);
    }

    #[test]
    fn test_fresh_db_is_v4() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .with_conn(|conn| {
                assert_eq!(user_version(conn), 4);
                assert!(table_exists(conn, "edge_embeddings")?);
                let cols = table_columns(conn, "causal_edges")?;
                assert!(cols.contains("access_count"));
                assert!(cols.contains("last_accessed_at"));
                assert!(cols.contains("outcome_polarity"));
                Ok(())
            })
            .unwrap();
    }

    /// v3 → v4: a DB at the v3 shape (marker set, no polarity column) gains
    /// `outcome_polarity` with NULL on existing rows; re-running is a no-op.
    #[test]
    fn test_migrate_v3_to_v4() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(CAUSAL_SCHEMA_SQL).unwrap();
        // Roll the v4 column back out to simulate a real v3 DB.
        conn.execute_batch("ALTER TABLE causal_edges DROP COLUMN outcome_polarity")
            .unwrap();
        conn.execute_batch("PRAGMA user_version = 3").unwrap();
        conn.execute_batch(
            "INSERT INTO chunks (id, text, created_at) VALUES ('d1', 'decision', 1000), ('o1', 'outcome', 1000);
             INSERT INTO causal_edges (from_id, to_id, relation, confidence, event_time, discovered_at)
             VALUES ('d1', 'o1', 'caused', 0.8, 1000, 1000);",
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(user_version(&conn), 4);

        let cols = table_columns(&conn, "causal_edges").unwrap();
        assert!(cols.contains("outcome_polarity"));
        let polarity: Option<String> = conn
            .query_row(
                "SELECT outcome_polarity FROM causal_edges WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(polarity, None, "existing rows stay NULL (no backfill)");

        // The CHECK constraint rejects values outside the enum, allows NULL.
        assert!(conn
            .execute(
                "UPDATE causal_edges SET outcome_polarity = 'bogus' WHERE id = 1",
                [],
            )
            .is_err());
        conn.execute(
            "UPDATE causal_edges SET outcome_polarity = 'mixed' WHERE id = 1",
            [],
        )
        .unwrap();

        // Idempotent.
        migrate(&conn).unwrap();
        assert_eq!(user_version(&conn), 4);
        let polarity: Option<String> = conn
            .query_row(
                "SELECT outcome_polarity FROM causal_edges WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(polarity.as_deref(), Some("mixed"));
    }

    #[test]
    fn test_access_tracking() {
        let store = CausalStore::open_in_memory().unwrap();
        store
            .record_decision(
                "enabled cache without TTL",
                "memory grew until OOM",
                "caused",
                Some("caching"),
                0.9,
                "llm_inferred",
            )
            .unwrap();

        let hits = store.search_causal(Some("caching"), None).unwrap();
        assert_eq!(hits.len(), 1);
        let edge_id = hits[0].edge_id;

        let (count, last): (i64, Option<i64>) = store
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT access_count, last_accessed_at FROM causal_edges WHERE id = ?1",
                    params![edge_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert!(last.is_some());

        // trace_cause bumps the same counter again.
        let causes = store.trace_cause("OOM").unwrap();
        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].edge_id, edge_id);
        let count: i64 = store
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT access_count FROM causal_edges WHERE id = ?1",
                    params![edge_id],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 2);
    }
}
