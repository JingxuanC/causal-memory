//! v16 migration e2e: a v15 database (causal_edges CHECK without
//! 'co_occurrence') migrates cleanly — data preserved, the widened CHECK
//! accepts co_occurrence writes, and the chain walk excludes them.

use causal_memory::store::CausalStore;

/// v15 causal_edges: same columns as v16 but the OLD 4-value relation CHECK.
const V15_SCHEMA_AND_DATA: &str = r#"
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
    relation TEXT NOT NULL CHECK(relation IN ('caused','enabled','prevented','no_effect')),
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
PRAGMA user_version = 15;
"#;

#[test]
fn migration_from_v15_widens_relation_check() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("v15.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(V15_SCHEMA_AND_DATA).unwrap();
    }

    let store = CausalStore::open(&db_path).unwrap();
    store
        .with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, i64::from(causal_memory::migrate::SCHEMA_VERSION));
            // Legacy row survived the table rebuild.
            let kept: i64 = conn.query_row(
                "SELECT COUNT(*) FROM causal_edges WHERE relation = 'caused'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(kept, 1);
            // Indexes rebuilt.
            let idx: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                 AND name LIKE 'idx_causal_%'",
                [],
                |r| r.get(0),
            )?;
            assert!(idx >= 5, "causal_edges indexes rebuilt, got {idx}");
            Ok(())
        })
        .unwrap();

    // The widened CHECK accepts co_occurrence writes…
    store
        .record_decision_full(
            "purge edge cache nightly",
            "elevated origin error rate on Tuesdays",
            "co_occurrence",
            Some("legacy"),
            0.5,
            "rule",
            2000,
            Some("negative"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.count_edges().unwrap(), 2);

    // …and the forward chain walk excludes the associational edge.
    let chains = store
        .trace_effect_chain("purge edge cache", 3, 0.3)
        .unwrap();
    assert!(
        chains.is_empty(),
        "co_occurrence edges must not appear in do()-style chains"
    );
    let causal_chains = store.trace_effect_chain("restart broker", 3, 0.3).unwrap();
    assert!(!causal_chains.is_empty(), "causal chains unaffected");

    // Idempotent re-open.
    drop(store);
    let reopened = CausalStore::open(&db_path).unwrap();
    assert_eq!(reopened.count_edges().unwrap(), 2);
}

/// v15 schema PLUS the two tables that hold FOREIGN KEY references into
/// causal_edges — the combination that broke production (athena tenant):
/// bundled SQLite enforces FK by default, and DROP TABLE causal_edges in
/// migrate_to_v16 fails with code 787 while any child row references it.
const V15_WITH_FK_CHILDREN: &str = r#"
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
    relation TEXT NOT NULL CHECK(relation IN ('caused','enabled','prevented','no_effect')),
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
CREATE TABLE decision_forks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_id_a INTEGER NOT NULL,
    edge_id_b INTEGER NOT NULL,
    discovered_at INTEGER NOT NULL,
    UNIQUE(edge_id_a, edge_id_b),
    FOREIGN KEY (edge_id_a) REFERENCES causal_edges(id),
    FOREIGN KEY (edge_id_b) REFERENCES causal_edges(id)
);
CREATE TABLE edge_embeddings (
    edge_id INTEGER PRIMARY KEY REFERENCES causal_edges(id),
    model TEXT NOT NULL,
    vector BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO chunks (id, text, created_at) VALUES
    ('d1', 'restart broker during deploy window', 1000),
    ('o1', 'webhook delivery failures observed', 1000),
    ('d2', 'switched to channel ownership', 1100),
    ('o2', 'race fixed, all tests pass', 1100);
INSERT INTO causal_edges (from_id, to_id, relation, confidence, discovered_by,
                          event_time, discovered_at, task_tag, outcome_polarity)
VALUES ('d1', 'o1', 'caused', 0.7, 'llm_inferred', 1000, 1000, 'legacy', 'negative'),
       ('d2', 'o2', 'caused', 0.9, 'user_feedback', 1100, 1100, 'legacy', 'positive');
INSERT INTO decision_forks (edge_id_a, edge_id_b, discovered_at) VALUES (1, 2, 1200);
INSERT INTO edge_embeddings (edge_id, model, vector, created_at) VALUES (1, 'shared', X'0000803F', 1200);
PRAGMA user_version = 15;
"#;

/// Production regression (athena tenant, 2026-10): a v15 store whose
/// decision_forks / edge_embeddings reference causal_edges must migrate —
/// DROP TABLE in migrate_to_v16 used to die on FK enforcement (code 787).
#[test]
fn migration_from_v15_preserves_fk_child_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("v15-fk.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(V15_WITH_FK_CHILDREN).unwrap();
    }

    let store = CausalStore::open(&db_path).unwrap();
    store
        .with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, i64::from(causal_memory::migrate::SCHEMA_VERSION));
            let edges: i64 =
                conn.query_row("SELECT COUNT(*) FROM causal_edges", [], |r| r.get(0))?;
            assert_eq!(edges, 2, "edge rows preserved through the rebuild");
            let forks: i64 =
                conn.query_row("SELECT COUNT(*) FROM decision_forks", [], |r| r.get(0))?;
            assert_eq!(forks, 1, "decision_forks row preserved");
            let embs: i64 =
                conn.query_row("SELECT COUNT(*) FROM edge_embeddings", [], |r| r.get(0))?;
            assert_eq!(embs, 1, "edge_embeddings row preserved");
            // Contents intact, not just counts.
            let (fa, fb): (i64, i64) =
                conn.query_row("SELECT edge_id_a, edge_id_b FROM decision_forks", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            assert_eq!((fa, fb), (1, 2));
            let model: String = conn.query_row(
                "SELECT model FROM edge_embeddings WHERE edge_id = 1",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(model, "shared");
            // Migration restores FK enforcement for normal operation.
            let fk_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            assert_eq!(fk_on, 1, "foreign_keys must be re-enabled after migrate");
            Ok(())
        })
        .unwrap();

    // Idempotent re-open.
    drop(store);
    let reopened = CausalStore::open(&db_path).unwrap();
    assert_eq!(reopened.count_edges().unwrap(), 2);
}
