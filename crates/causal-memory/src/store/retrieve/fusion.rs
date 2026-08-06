//! Shared reciprocal-rank fusion (used by the harnesses and servers).

use crate::store::CausalEntry;

pub fn rrf_merge_many(lists: &[&[CausalEntry]], limit: usize) -> Vec<CausalEntry> {
    use std::collections::HashMap;
    let k = 60.0;
    let mut scores: HashMap<i64, f64> = HashMap::new(); // edge_id → Σ 1/(k+rank+1)

    for list in lists {
        for (i, entry) in list.iter().enumerate() {
            let s = 1.0 / (k + i as f64 + 1.0);
            *scores.entry(entry.edge_id).or_insert(0.0) += s;
        }
    }

    let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut result = Vec::new();
    for (edge_id, _) in ranked.into_iter().take(limit) {
        for list in lists {
            if let Some(entry) = list.iter().find(|e| e.edge_id == edge_id) {
                result.push(entry.clone());
                break;
            }
        }
    }
    result
}
