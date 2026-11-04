//! Scale probe: synthetic store at N nodes → measure the real cost centers.
//!
//! Usage: scale_probe <nodes> [fanout] [out_dir]
//!
//! Prints, for the given size:
//!   1. bulk insert wall time + resulting db file size
//!   2. CausalGraph::from_store build time (the resident graph load)
//!   3. cold full-candidate SQL scan (id index sweep) + cold full-text hydrate
//!   4. one spreading-activation query on the resident graph
//!
//! Wrap with `/usr/bin/time -l` to capture max RSS.

use std::time::Instant;

use causal_memory::hippocampus::CausalGraph;
use causal_memory::store::{CausalStore, CAUSAL_SCHEMA_SQL};
use rusqlite::{params, Connection};

const RELATIONS: [&str; 4] = ["caused", "enabled", "prevented", "no_effect"];

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let nodes: usize = args
        .get(1)
        .map(|s| s.parse().unwrap_or(1_000_000))
        .unwrap_or(1_000_000);
    let fanout: usize = args.get(2).map(|s| s.parse().unwrap_or(4)).unwrap_or(4);
    let dir = std::env::temp_dir().join(format!(
        "cm-scale-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("scale.db");
    println!("== scale probe: nodes={nodes} fanout={fanout} db={db_path:?}");

    // ---- 1. bulk insert (raw rows; faithful table shape, bypasses facade) ----
    let t0 = Instant::now();
    {
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(CAUSAL_SCHEMA_SQL)?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut ins_chunk = tx.prepare(
                "INSERT INTO chunks(id, text, created_at, q_value) VALUES (?1, ?2, ?3, 0.5)",
            )?;
            let base = 1_700_000_000i64;
            for i in 0..nodes {
                let text = format!(
                    "decision {i} shipped the feature flag rollout to production with a canary \
                     percentage ramp and monitored error rates before full release",
                );
                ins_chunk.execute(params![format!("c{i}"), text, base + i as i64])?;
            }
        }
        {
            let mut ins_edge = tx.prepare(
                "INSERT INTO causal_edges(from_id, to_id, relation, confidence, event_time, \
                 discovered_at, task_tag) VALUES (?1, ?2, ?3, 0.7, ?4, ?4, 'scale')",
            )?;
            let base = 1_700_000_000i64;
            let mut n = 0usize;
            for i in 0..nodes {
                for k in 0..fanout {
                    let to = (i + 1 + (i * 7 + k * 13) % nodes) % nodes;
                    if to == i {
                        continue;
                    }
                    ins_edge.execute(params![
                        format!("c{i}"),
                        format!("c{to}"),
                        RELATIONS[n % RELATIONS.len()],
                        base + n as i64,
                    ])?;
                    n += 1;
                }
            }
        }
        tx.commit()?;
    }
    let insert_s = t0.elapsed().as_secs_f64();
    let db_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    println!("1) insert: {insert_s:.1}s  db_size={}MB", db_bytes >> 20);

    // ---- 2. resident graph build ----
    let store = CausalStore::open(&db_path)?;
    let t1 = Instant::now();
    let mut graph = CausalGraph::from_store(&store)?;
    println!(
        "2) from_store: {:.1}s  nodes={} edges={}",
        t1.elapsed().as_secs_f64(),
        graph.num_nodes(),
        graph.num_edges(),
    );

    // ---- 3. cold full-candidate scan costs (raw SQL mirror of entity path) ----
    let t2 = Instant::now();
    let id_scan: usize = {
        let conn = Connection::open(&db_path)?;
        let mut stmt =
            conn.prepare("SELECT id FROM causal_edges WHERE valid_to IS NULL ORDER BY id")?;
        let mut n = 0usize;
        let mut rows = stmt.query([])?;
        while let Some(_r) = rows.next()? {
            n += 1;
        }
        n
    };
    println!(
        "3a) full id-scan ({id_scan} edges): {:.2}s",
        t2.elapsed().as_secs_f64()
    );

    let t3 = Instant::now();
    let text_rows: usize = {
        let conn = Connection::open(&db_path)?;
        let mut stmt = conn.prepare(
            "SELECT ce.id, cf.text, ct.text FROM causal_edges ce \
             JOIN chunks cf ON cf.id = ce.from_id JOIN chunks ct ON ct.id = ce.to_id",
        )?;
        let mut n = 0usize;
        let mut rows = stmt.query([])?;
        while let Some(_r) = rows.next()? {
            n += 1;
        }
        n
    };
    println!(
        "3b) cold full-text hydrate ({text_rows} edges): {:.2}s",
        t3.elapsed().as_secs_f64()
    );

    // ---- 4. one spreading-activation query on the resident graph ----
    let t4 = Instant::now();
    let hits = graph.spreading_activation(
        "feature flag canary rollout production release",
        None,
        false,
    );
    println!(
        "4) spreading_activation: {:.3}s  hits={}",
        t4.elapsed().as_secs_f64(),
        hits.len(),
    );

    println!("done");
    Ok(())
}
