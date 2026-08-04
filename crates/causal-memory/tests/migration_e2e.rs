//! Phase 7 e2e: schema migration — open a pre-v0.6 (v1) database file and
//! verify the automatic migration to the current schema (v5) preserves data
//! and keeps every read/write path functional.
//!
//! v1 schema (reconstructed from migrate.rs docs): `causal_edges` has no
//! `event_time` / `discovered_at` / `valid_to` / `access_count` columns and
//! carries a legacy `created_at` column instead; no `user_version` marker.

use causal_memory::store::CausalStore;

const V1_SCHEMA_AND_DATA: &str = r#"
CREATE TABLE chunks (
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
);
INSERT INTO chunks (id, text, created_at) VALUES
    ('d1', 'skip backup before db migration', 1000),
    ('o1', 'data loss during deploy', 1000),
    ('d2', 'disable foreign key checks for speed', 2000);
INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by, task_tag, created_at)
VALUES
    ('d1', 'o1', 'caused', 0.8, 'llm_inferred', 'legacy', 1000),
    ('d2', 'o1', 'caused', 0.9, 'user_feedback', NULL, 2000);
"#;

#[test]
fn migration_from_v1_file_db() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("legacy.db");

    // Hand-build the v1 database, then close it before the store opens it.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(V1_SCHEMA_AND_DATA).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 0, "v1 DB carries no version marker");
    }

    // CausalStore::open runs the migration automatically.
    let store = CausalStore::open(&db_path).unwrap();

    store
        .with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, 8, "migrated to schema v8");
            // v8: the reversible-consolidation and recurrence-distill columns exist.
            let v8_columns: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('causal_edges') WHERE name = 'superseded_by'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(v8_columns, 1);
            let sl_columns: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('session_logs')
                 WHERE name IN ('embedding', 'distilled_at')",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(sl_columns, 2);
            // v6: the fact layer exists.
            let facts_tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'
                 AND name IN ('agent_facts', 'agent_facts_embeddings')",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(facts_tables, 2);
            // v4: the polarity column exists and legacy rows stay NULL
            // (read paths fall back to the signal-word heuristic).
            let null_polarity: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE outcome_polarity IS NULL",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(null_polarity, 2);
            Ok(())
        })
        .unwrap();

    // Old data intact: both legacy edges are queryable.
    let edges = store.all_valid_edges().unwrap();
    assert_eq!(edges.len(), 2);
    assert!(
        edges.iter().all(|e| e.event_time > 0),
        "event_time backfilled from created_at: {edges:?}"
    );
    assert!(
        edges
            .iter()
            .all(|e| e.discovered_at > 0 && e.valid_to.is_none()),
        "discovered_at backfilled, valid_to stays NULL"
    );
    let d1 = edges
        .iter()
        .find(|e| e.decision_text.contains("skip backup"))
        .unwrap();
    assert_eq!(d1.event_time, 1000, "backfill uses the legacy created_at");

    // Read paths work on migrated data.
    let hits = store.search_causal(Some("legacy"), None).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].decision_text.contains("skip backup"));

    let traced = store.trace_cause("data loss").unwrap();
    assert_eq!(
        traced.len(),
        2,
        "both edges point at the shared outcome chunk"
    );

    // Write path works: recording a new edge on the migrated DB.
    let id = store
        .record_decision(
            "run migration in a transaction",
            "deploy succeeded with zero data loss",
            "caused",
            Some("db"),
            0.7,
            "rule",
        )
        .unwrap();
    assert!(!id.is_empty());
    assert_eq!(store.count_edges().unwrap(), 3);
    let hits = store.search_causal(Some("db"), None).unwrap();
    assert_eq!(hits.len(), 1);

    // Re-opening the migrated file is a no-op (idempotent migration).
    drop(store);
    let reopened = CausalStore::open(&db_path).unwrap();
    assert_eq!(reopened.count_edges().unwrap(), 3);
}
