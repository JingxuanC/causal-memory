//! v17 migration e2e: a v16 database (causal_edges without the bias_flag
//! column) gains the column on open — data preserved, stamps work, and
//! re-opening is a no-op (idempotent).

use causal_memory::store::CausalStore;

/// v16 causal_edges: the full post-rebuild shape (v16 CHECK included), no
/// bias_flag column.
const V16_SCHEMA_AND_DATA: &str = r#"
CREATE TABLE chunks (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    q_value REAL NOT NULL DEFAULT 0.5,
    sparse_code TEXT
);
CREATE TABLE causal_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK(relation IN ('caused','enabled','prevented','no_effect','co_occurrence')),
    confidence REAL NOT NULL DEFAULT 0.5,
    discovered_by TEXT NOT NULL DEFAULT 'llm_inferred',
    event_time INTEGER NOT NULL,
    discovered_at INTEGER NOT NULL,
    valid_to INTEGER,
    task_tag TEXT,
    access_count INTEGER NOT NULL DEFAULT 0,
    last_accessed_at INTEGER,
    outcome_polarity TEXT CHECK(outcome_polarity IN ('positive','negative','mixed','neutral')),
    superseded_by INTEGER,
    context_fingerprint TEXT,
    context_text TEXT,
    FOREIGN KEY (from_id) REFERENCES chunks(id),
    FOREIGN KEY (to_id) REFERENCES chunks(id)
);
INSERT INTO chunks (id, text, created_at) VALUES
    ('d1', 'restart broker during deploy window', 1000),
    ('o1', 'webhook delivery failures observed', 1000);
INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by,
                          event_time, discovered_at, task_tag, outcome_polarity)
VALUES ('d1', 'o1', 'caused', 0.7, 'llm_inferred', 1000, 1000, 'legacy', 'negative');
PRAGMA user_version = 16;
"#;

#[test]
fn migration_from_v16_adds_bias_flag_column() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("v16.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(V16_SCHEMA_AND_DATA).unwrap();
    }

    let store = CausalStore::open(&db_path).unwrap();
    store
        .with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, i64::from(causal_memory::migrate::SCHEMA_VERSION));
            // Legacy row survived.
            let kept: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE relation = 'caused'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(kept, 1);
            // The new column exists and starts empty.
            let flags: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE bias_flag IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(flags, 0);
            Ok(())
        })
        .unwrap();

    // Stamps work end-to-end…
    let edge = store.all_valid_edges().unwrap().pop().unwrap();
    store
        .set_bias_flag(edge.edge_id, "polarity_skew:legacy")
        .unwrap();
    let flagged = store.bias_flagged_edges().unwrap();
    assert_eq!(flagged.len(), 1);
    assert_eq!(flagged[0].flag, "polarity_skew:legacy");

    // …and re-opening is a clean no-op (column-exists guard).
    drop(store);
    let reopened = CausalStore::open(&db_path).unwrap();
    assert_eq!(reopened.bias_flagged_edges().unwrap().len(), 1);
}
