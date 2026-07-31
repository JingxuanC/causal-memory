//! Utility functions for the hippocampus module.

/// Maximum weight after LTP. Prevents weight drift from unbounded ×1.05.
pub(crate) const WEIGHT_CAP: f32 = 2.0;

/// SimHash for pattern separation (DG analog).
pub(crate) fn simhash(text: &str) -> u128 {
    let mut bits = [0_i32; 128];
    for token in text.to_lowercase().split_whitespace() {
        let hash = fnv1a_64(token);
        for (i, bit) in bits[..64].iter_mut().enumerate() {
            if (hash >> i) & 1 == 1 {
                *bit += 1;
            } else {
                *bit -= 1;
            }
        }
        let hash2 = fnv1a_64(&format!("{}#2", token));
        for i in 0..64 {
            if (hash2 >> i) & 1 == 1 {
                bits[i + 64] += 1;
            } else {
                bits[i + 64] -= 1;
            }
        }
    }
    let mut result: u128 = 0;
    for (i, bit) in bits.iter().enumerate() {
        if *bit > 0 {
            result |= 1u128 << i;
        }
    }
    result
}

pub(crate) fn fnv1a_64(s: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Simple Jaccard text similarity (token overlap).
/// LIMITATION: whitespace tokenization only; Chinese text needs bigram tokenizer.
pub(crate) fn text_jaccard_similarity(a: &str, b: &str) -> f32 {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    let set_a: std::collections::HashSet<&str> = a_lower.split_whitespace().collect();
    let set_b: std::collections::HashSet<&str> = b_lower.split_whitespace().collect();
    if set_a.is_empty() && set_b.is_empty() {
        return 1.0;
    }
    let intersection = set_a.intersection(&set_b).count() as f32;
    let union = set_a.union(&set_b).count() as f32;
    intersection / union
}

/// Deterministic xorshift PRNG (fixed seed for reproducible tests).
/// Production code should seed from system time or /dev/urandom.
pub(crate) fn rand_seed() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x1234567890ABCDEF) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}
