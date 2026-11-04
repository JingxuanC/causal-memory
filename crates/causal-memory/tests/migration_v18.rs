//! v18 migration e2e: a v17 database (causal_edges without the
//! influenced_by column) gains the column on open — data preserved,
//! influence chains work, and re-opening is a no-op (idempotent).

use causal_memory::store::CausalStore;

/// v17 causal_edges: the full shape after the v16 rebuild + v17 bias_flag,
/// no influenced_by column.
const V17_SCHEMA_AND_DATA: &str = r#"
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
    bias_flag TEXT,
    FOREIGN KEY (from_id) REFERENCES chunks(id),
    FOREIGN KEY (to_id) REFERENCES chunks(id)
);
INSERT INTO chunks (id, text, created_at) VALUES
    ('d1', 'restart broker during deploy window', 1000),
    ('o1', 'webhook delivery failures observed', 1000);
INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by,
                          event_time, discovered_at, task_tag, outcome_polarity,
                          bias_flag)
VALUES ('d1', 'o1', 'caused', 0.7, 'llm_inferred', 1000, 1000, 'legacy',
        'negative', 'polarity_skew:legacy');
PRAGMA user_version = 17;
"#;

#[test]
fn migration_from_v17_adds_influenced_by_column() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("v17.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(V17_SCHEMA_AND_DATA).unwrap();
    }

    let store = CausalStore::open(&db_path).unwrap();
    store
        .with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, i64::from(causal_memory::migrate::SCHEMA_VERSION));
            // Legacy row survived, bias_flag (v17) intact.
            let kept: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE bias_flag = 'polarity_skew:legacy'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(kept, 1);
            // The new column exists and starts empty.
            let chains: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE influenced_by IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(chains, 0);
            Ok(())
        })
        .unwrap();

    // Influence chains work end-to-end…
    let legacy = store.all_valid_edges().unwrap().pop().unwrap();
    let (_, new_edge) = store
        .record_decision_full(
            "added retry to the webhook worker",
            "flap rate fell",
            "caused",
            Some("legacy"),
            0.8,
            "rule",
            2000,
            Some("positive"),
            None,
            Some(&[legacy.edge_id]),
        )
        .unwrap();
    assert_eq!(
        store.get_edge(new_edge).unwrap().unwrap().influenced_by,
        Some(vec![legacy.edge_id])
    );
    assert_eq!(store.influenced_decisions(legacy.edge_id).unwrap().len(), 1);

    // …and re-opening is a clean no-op (column-exists guard).
    drop(store);
    let reopened = CausalStore::open(&db_path).unwrap();
    assert_eq!(
        reopened.get_edge(new_edge).unwrap().unwrap().influenced_by,
        Some(vec![legacy.edge_id])
    );
}
