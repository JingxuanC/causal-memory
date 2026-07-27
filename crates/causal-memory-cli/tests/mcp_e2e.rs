//! Phase 7 e2e: MCP over stdio — spawn the real `causal-memory` binary as an
//! MCP server child process and drive the full protocol with an rmcp client:
//! initialize handshake, tools/list, and a sequential session of tools/call
//! requests over one long-lived connection.
//!
//! Test isolation: the server picks its DB path from the CAUSAL_MEMORY_DB
//! env var (see main.rs `get_db_path`), pointed at a tempdir here. Embedding
//! env vars are removed from the child so search_causal takes the keyword
//! (LIKE) fallback — which is itself part of what this test verifies.

use std::collections::HashSet;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;

const EXPECTED_TOOLS: &[&str] = &[
    "record_decision",
    "search_causal",
    "trace_cause",
    "trace_cause_chain",
    "invalidate_decision",
    "search_patterns",
    "causal_directory",
    "intervention_query",
];

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn args(v: serde_json::Value) -> rmcp::model::JsonObject {
    v.as_object()
        .expect("arguments must be a JSON object")
        .clone()
}

#[tokio::test]
async fn mcp_stdio_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("mcp.db");

    // Seed a multi-hop chain (MCP's record_decision always creates disjoint
    // chunk pairs, so a bridged graph can only come from the library layer):
    //   X: global lock → deadlock error        (t=1000)
    //   bridge: X.outcome → Y.decision         (t=1500)
    //   Y: sharded locks → contention resolved (t=2000)
    {
        let store = causal_memory::store::CausalStore::open(&db_path).unwrap();
        store
            .record_decision_at(
                "used global lock for cache",
                "deadlock error under load",
                "caused",
                Some("locking"),
                0.9,
                "rule",
                1000,
            )
            .unwrap();
        store
            .record_decision_at(
                "replaced global lock with sharded locks",
                "lock contention resolved successfully",
                "caused",
                Some("locking"),
                0.9,
                "rule",
                2000,
            )
            .unwrap();
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO causal_edges
                         (from_id, to_id, relation, confidence, discovered_by, event_time, discovered_at, task_tag)
                     SELECT o.id, d.id, 'caused', 0.5, 'temporal', 1500, 1500, 'chain-link:temporal'
                     FROM chunks o, chunks d
                     WHERE o.text = 'deadlock error under load'
                       AND d.text = 'replaced global lock with sharded locks'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
    }

    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_causal-memory"));
    cmd.env("CAUSAL_MEMORY_DB", &db_path)
        // Force the keyword (LIKE) retrieval path in the child: no embedding
        // endpoint configured → EmbedConfig::from_env() returns None.
        .env_remove("CAUSAL_MEMORY_EMBED_API")
        .env_remove("CAUSAL_MEMORY_EMBED_KEY")
        .env_remove("CAUSAL_MEMORY_LLM_API")
        .env_remove("CAUSAL_MEMORY_LLM_KEY");
    let transport = TokioChildProcess::new(cmd).expect("spawn causal-memory server");
    let client = ().serve(transport).await.expect("MCP initialize handshake");

    // ── 1. Initialize handshake succeeded (regression: server must not exit
    //       right after init — everything below runs on this one connection).
    let info = client.peer_info().expect("server must report its info");
    assert!(!info.server_info.name.is_empty());

    // ── 2. tools/list: exactly the 8 documented tools.
    let tools = client.list_all_tools().await.unwrap();
    let names: HashSet<String> = tools.iter().map(|t| t.name.to_string()).collect();
    let expected: HashSet<String> = EXPECTED_TOOLS.iter().map(|s| s.to_string()).collect();
    assert_eq!(names, expected);

    // ── 3. record_decision ×2 (one failure, one success, same task_tag).
    let r1 = client
        .call_tool(
            CallToolRequestParams::new("record_decision").with_arguments(args(serde_json::json!({
                "decision": "skip backup before db migration",
                "outcome": "data loss error during migration rollback",
                "relation": "caused",
                "task_tag": "e2e-test",
                "confidence_source": "rule"
            }))),
        )
        .await
        .unwrap();
    assert!(text_of(&r1).contains("✅"), "record #1: {}", text_of(&r1));

    let r2 = client
        .call_tool(
            CallToolRequestParams::new("record_decision").with_arguments(args(serde_json::json!({
                "decision": "restored db from nightly snapshot",
                "outcome": "service recovered successfully",
                "relation": "caused",
                "task_tag": "e2e-test",
                "confidence_source": "rule"
            }))),
        )
        .await
        .unwrap();
    assert!(text_of(&r2).contains("✅"), "record #2: {}", text_of(&r2));

    // ── 4. search_causal with task_tag filter — keyword fallback path.
    let r = client
        .call_tool(
            CallToolRequestParams::new("search_causal").with_arguments(args(serde_json::json!({
                "task_tag": "e2e-test"
            }))),
        )
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(
        text.starts_with("[keyword]"),
        "no embed env → keyword fallback, got: {text}"
    );
    assert!(text.contains("skip backup before db migration"), "{text}");
    assert!(text.contains("restored db from nightly snapshot"), "{text}");

    // ── 5. trace_cause (single hop) + trace_cause_chain (multi-hop, seeded).
    let r = client
        .call_tool(
            CallToolRequestParams::new("trace_cause").with_arguments(args(serde_json::json!({
                "outcome_description": "data loss"
            }))),
        )
        .await
        .unwrap();
    assert!(
        text_of(&r).contains("skip backup before db migration"),
        "{}",
        text_of(&r)
    );

    let r = client
        .call_tool(
            CallToolRequestParams::new("trace_cause_chain").with_arguments(args(
                serde_json::json!({
                    "outcome_description": "contention resolved",
                    "max_depth": 5,
                    "min_confidence": 0.3
                }),
            )),
        )
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(text.contains("causal chain"), "{text}");
    assert!(
        text.contains("hop 3"),
        "Y → bridge → X is a 3-hop chain: {text}"
    );
    assert!(text.contains("used global lock for cache"), "{text}");

    // ── 6. causal_directory contains the recorded decisions.
    let r = client
        .call_tool(CallToolRequestParams::new("causal_directory"))
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(text.contains("[e2e-test]"), "{text}");
    assert!(text.contains("skip backup before db migration"), "{text}");

    // ── 7. intervention_query: forward chains with risk labels.
    let r = client
        .call_tool(
            CallToolRequestParams::new("intervention_query").with_arguments(args(
                serde_json::json!({
                    "action": "skip backup"
                }),
            )),
        )
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(text.contains("Chain 1"), "{text}");
    assert!(text.contains("DANGER"), "failure terminal outcome: {text}");

    let r = client
        .call_tool(
            CallToolRequestParams::new("intervention_query").with_arguments(args(
                serde_json::json!({
                    "action": "sharded locks"
                }),
            )),
        )
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(text.contains("Chain 1"), "{text}");
    assert!(text.contains("SAFE"), "success terminal outcome: {text}");

    // ── 8. search_patterns: normal response (empty is fine, no miner ran).
    let r = client
        .call_tool(CallToolRequestParams::new("search_patterns"))
        .await
        .unwrap();
    assert!(!r.is_error.unwrap_or(false));
    assert!(!text_of(&r).is_empty());

    // ── 9. invalidate_decision: the recorded failure disappears from search.
    // The MCP surface never exposes edge ids, so look it up in the DB file
    // (fresh DB owned by this test; server is idle between requests).
    let edge_id: i64 = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.query_row(
            "SELECT ce.id FROM causal_edges ce
             JOIN chunks cf ON cf.id = ce.from_id
             WHERE cf.text = 'skip backup before db migration'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let r = client
        .call_tool(
            CallToolRequestParams::new("invalidate_decision").with_arguments(args(
                serde_json::json!({"edge_id": edge_id, "reason": "e2e wrong lesson"}),
            )),
        )
        .await
        .unwrap();
    assert!(
        text_of(&r).contains("✅ Invalidated"),
        "invalidate: {}",
        text_of(&r)
    );

    let r = client
        .call_tool(
            CallToolRequestParams::new("search_causal").with_arguments(args(serde_json::json!({
                "task_tag": "e2e-test"
            }))),
        )
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(!text.contains("skip backup before db migration"), "{text}");
    assert!(text.contains("restored db from nightly snapshot"), "{text}");

    // ── 10. Graceful shutdown of the long-lived connection.
    client.cancel().await.unwrap();
}
