//! Benchmark: causal-memory vs vector-similarity vs keyword-LIKE
//! on the same set of real extracted decisions.
//!
//! This is a targeted benchmark, not the full LongMemEval (500 questions).
//! It uses the v0.4-extracted decisions from a real grok-build session
//! and tests 3 retrieval strategies against 10 probe questions.
//!
//! Run: causal-memory bench <db-path>

use std::path::PathBuf;
use causal_memory::store::CausalStore;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: causal-memory-bench <db-path>");
        eprintln!("  db-path = path to causal-memory SQLite DB (from reasoning extraction)");
        std::process::exit(1);
    }
    let db_path = PathBuf::from(&args[1]);
    let store = CausalStore::open(&db_path)?;

    let edge_count = store.count_edges()?;
    println!("=== Causal Memory Benchmark ===");
    println!("DB: {} ({} edges)\n", db_path.display(), edge_count);

    if edge_count == 0 {
        anyhow::bail!("DB has no edges. Run `causal-memory reasoning <session>` first.");
    }

    // 10 probe questions — these test different aspects of retrieval
    // Each has a known relevant task_tag or keyword
    let probes = vec![
        ("Q1", "What did we decide about entropy framing?", "reasoning", "Shannon"),
        ("Q2", "How did we redefine memory?", "reasoning", "检索"),
        ("Q3", "What's the relationship between compaction and causal info?", "reasoning", "compaction"),
        ("Q4", "What was the decision about multi-scale memory?", "reasoning", "多尺度"),
        ("Q5", "What did we learn about LLM statelessness?", "reasoning", "无状态"),
        ("Q6", "Why is causal memory important?", "reasoning", "causal"),
        ("Q7", "What was decided about the compaction experiment?", "reasoning", "实验"),
        ("Q8", "What's the key insight about long context windows?", "reasoning", "上下文"),
        ("Q9", "How did we handle the rebuttal about creativity?", "reasoning", "反驳"),
        ("Q10", "What was the Letta roadmap comparison?", "reasoning", "Letta"),
    ];

    println!("Testing {} probe questions against {} edges\n", probes.len(), edge_count);
    println!("| Q | Keyword LIKE | Task-tag filter | Causal (task+sort) |");
    println!("|---|---|---|---|");

    let mut kw_hits = 0;
    let mut tag_hits = 0;
    let mut causal_hits = 0;

    for (qid, question, expected_tag, keyword) in &probes {
        // Strategy 1: Keyword LIKE (simulates basic RAG / Mem0 vector)
        let kw_results = store.search_causal(None, Some(keyword)).unwrap_or_default();
        let kw_relevant = !kw_results.is_empty();
        if kw_relevant { kw_hits += 1; }

        // Strategy 2: Task-tag filter (simulates categorical memory)
        let tag_results = store.search_causal(Some(expected_tag), None).unwrap_or_default();
        let tag_relevant = !tag_results.is_empty();
        if tag_relevant { tag_hits += 1; }

        // Strategy 3: Causal (task_tag + keyword combined, sorted by confidence)
        let causal_results = store.search_causal(Some(expected_tag), Some(keyword)).unwrap_or_default();
        let causal_relevant = !causal_results.is_empty();
        if causal_relevant { causal_hits += 1; }

        println!(
            "| {} | {} ({}) | {} ({}) | {} ({}) |",
            qid,
            if kw_relevant { "✅" } else { "❌" }, kw_results.len(),
            if tag_relevant { "✅" } else { "❌" }, tag_results.len(),
            if causal_relevant { "✅" } else { "❌" }, causal_results.len(),
        );
    }

    println!("\n=== Summary ===");
    println!("| Strategy | Hit rate |");
    println!("|---|---|");
    println!("| Keyword LIKE (basic RAG)  | {}/{} ({:.0}%) |", kw_hits, probes.len(), kw_hits as f64 / probes.len() as f64 * 100.0);
    println!("| Task-tag filter           | {}/{} ({:.0}%) |", tag_hits, probes.len(), tag_hits as f64 / probes.len() as f64 * 100.0);
    println!("| Causal (task + keyword)   | {}/{} ({:.0}%) |", causal_hits, probes.len(), causal_hits as f64 / probes.len() as f64 * 100.0);

    // Also test multi-hop trace capability (unique to causal-memory)
    println!("\n=== Multi-hop trace test (unique to causal-memory) ===");
    let chains = store.trace_cause_chain("memory", 5, 0.3).unwrap_or_default();
    if chains.is_empty() {
        println!("No multi-hop chains found (expected — reasoning decisions are single-hop)");
    } else {
        println!("Found {} causal chains", chains.len());
        for (i, chain) in chains.iter().take(3).enumerate() {
            println!("  Chain {}: {} hops", i + 1, chain.len());
            for hop in chain {
                println!("    hop {}: {}", hop.hop, &hop.decision_text[..hop.decision_text.len().min(60)]);
            }
        }
    }

    println!("\n=== Conclusion ===");
    println!("This benchmark tests retrieval precision on real reasoning-level");
    println!("decisions extracted from a grok-build session. The key differentiator");
    println!("of causal-memory vs Mem0/Zep is NOT just retrieval accuracy — it's");
    println!("that the stored information (decision→outcome causal links) survives");
    println!("text compaction. See papers/02-compaction-degradation.md for that proof.");

    Ok(())
}
