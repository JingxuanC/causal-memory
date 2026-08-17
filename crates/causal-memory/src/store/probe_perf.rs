// TEMP perf probe — measures entity-search latency before/after cache warm.
// Run: cargo test --release -p causal-memory --lib probe_entity_cache -- --ignored --nocapture
use crate::store::CausalStore;

#[test]
#[ignore]
fn probe_entity_cache() {
    let store = CausalStore::open_in_memory().unwrap();
    // 5k synthetic edges with entity-bearing text.
    for i in 0..5000 {
        let person = ["Kim", "Nate", "Joanna", "Priya", "Sam"][i % 5];
        store
            .record_decision_at(
                &format!("{person} deployed Service{} to region{}", i % 40, i % 7),
                &format!("incident ticket {i} filed by {person}"),
                "caused",
                Some("perf"),
                0.8,
                "rule",
                i as i64,
            )
            .unwrap();
    }
    let query = "what did Kim do about Service3";
    // Cold: first query tokenizes everything.
    let t0 = std::time::Instant::now();
    let r1 = store.search_causal_entity(query, 10).unwrap();
    let cold = t0.elapsed();
    // Warm: five more queries, all cache hits.
    let t1 = std::time::Instant::now();
    for _ in 0..5 {
        let _ = store.search_causal_entity(query, 10).unwrap();
    }
    let warm = t1.elapsed() / 5;
    println!("COLD first query: {cold:?}; WARM avg per query: {warm:?}; hits={}", r1.len());
    println!("speedup: {:.1}x", cold.as_secs_f64() / warm.as_secs_f64());
}
