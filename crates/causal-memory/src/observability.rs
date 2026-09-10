//! In-process observability: a minimal hand-rolled metrics registry +
//! Prometheus text exposition. No prometheus/metrics/opentelemetry crates —
//! counters/gauges are atomics (or a Mutex-guarded map for the labeled
//! families), histograms are fixed-bucket, the text format is trivial.
//! Instrumentation is process-wide and free for stdio mode; only the HTTP
//! servers expose `/metrics`. OTLP export is deliberately deferred until a
//! collector actually exists.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Fixed buckets (seconds) for request-duration histograms.
const DURATION_BUCKETS: &[f64] = &[0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0];
/// Fixed buckets for the activated-nodes-per-recall histogram.
const ACTIVATED_BUCKETS: &[f64] = &[10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0];

#[derive(Default)]
struct Histogram {
    /// Cumulative counts per bucket (+ an implicit +Inf bucket).
    buckets: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn with_buckets(bounds: &[f64]) -> Self {
        Self {
            buckets: vec![0; bounds.len() + 1],
            sum: 0.0,
            count: 0,
        }
    }
    fn observe(&mut self, v: f64, bounds: &[f64]) {
        let idx = bounds.partition_point(|&b| v > b);
        self.buckets[idx] += 1;
        self.sum += v;
        self.count += 1;
    }
}

#[derive(Default)]
struct Inner {
    /// (server, tool, status) → count. RED: rate + errors.
    requests: HashMap<(String, String, String), u64>,
    /// (server, tool) → duration histogram. RED: duration.
    durations: HashMap<(String, String), Histogram>,
    /// Seed source ("bm25" | "semantic") → total seeds used.
    recall_seeds: HashMap<String, u64>,
    /// Activated-nodes-per-recall histogram.
    recall_activated_nodes: Histogram,
    /// (layer, source) → results returned, layer = facts|causal,
    /// source = seed|spread (provenance of the winning activation).
    recall_results: HashMap<(String, String), u64>,
    /// recall_audit write failures (the audit path never breaks retrieval).
    recall_audit_errors: u64,
}

/// Process-wide metrics registry.
pub struct Metrics {
    inner: Mutex<Inner>,
    started_unix: u64,
    uptime: AtomicU64, // cached at render time
}

/// The global registry singleton.
pub fn metrics() -> &'static Metrics {
    static M: OnceLock<Metrics> = OnceLock::new();
    M.get_or_init(|| Metrics {
        inner: Mutex::new(Inner {
            recall_activated_nodes: Histogram::with_buckets(ACTIVATED_BUCKETS),
            ..Inner::default()
        }),
        started_unix: chrono::Utc::now().timestamp() as u64,
        uptime: AtomicU64::new(0),
    })
}

impl Metrics {
    /// One completed tool call / HTTP request.
    pub fn record_request(&self, server: &str, tool: &str, status: &str, duration_secs: f64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.requests
            .entry((server.to_string(), tool.to_string(), status.to_string()))
            .or_insert(0) += 1;
        g.durations
            .entry((server.to_string(), tool.to_string()))
            .or_insert_with(|| Histogram::with_buckets(DURATION_BUCKETS))
            .observe(duration_secs, DURATION_BUCKETS);
    }

    /// Seeds used by one recall, per source ("bm25" | "semantic").
    pub fn record_recall_seeds(&self, source: &str, n: usize) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.recall_seeds.entry(source.to_string()).or_insert(0) += n as u64;
    }

    /// Activated node count of one spread recall.
    pub fn record_activated_nodes(&self, n: usize) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.recall_activated_nodes
            .observe(n as f64, ACTIVATED_BUCKETS);
    }

    /// One returned result, by layer and provenance source (seed|spread).
    pub fn record_recall_result(&self, layer: &str, source: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.recall_results
            .entry((layer.to_string(), source.to_string()))
            .or_insert(0) += 1;
    }

    /// A recall-audit write failure (retrieval itself was unaffected).
    pub fn record_audit_error(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.recall_audit_errors += 1;
    }

    /// Prometheus text exposition. Store gauges are computed at scrape
    /// time (`store` is None for endpoints without a store handle).
    pub fn render_prometheus(&self, store: Option<&crate::store::CausalStore>) -> String {
        let mut out = String::with_capacity(4096);
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        out.push_str("# TYPE causal_memory_requests_total counter\n");
        let mut requests: Vec<_> = g.requests.iter().collect();
        requests.sort();
        for ((server, tool, status), n) in requests {
            out.push_str(&format!(
                "causal_memory_requests_total{{server=\"{server}\",tool=\"{tool}\",status=\"{status}\"}} {n}\n"
            ));
        }

        out.push_str("# TYPE causal_memory_request_duration_seconds histogram\n");
        let mut durations: Vec<_> = g.durations.iter().collect();
        durations.sort_by(|a, b| a.0.cmp(b.0));
        for ((server, tool), h) in durations {
            let labels = format!("server=\"{server}\",tool=\"{tool}\"");
            let mut cumulative = 0u64;
            for (i, le) in DURATION_BUCKETS.iter().enumerate() {
                cumulative += h.buckets[i];
                out.push_str(&format!(
                    "causal_memory_request_duration_seconds_bucket{{{labels},le=\"{le}\"}} {cumulative}\n"
                ));
            }
            cumulative += h.buckets[DURATION_BUCKETS.len()];
            out.push_str(&format!(
                "causal_memory_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {cumulative}\n"
            ));
            out.push_str(&format!(
                "causal_memory_request_duration_seconds_sum{{{labels}}} {:.6}\n",
                h.sum
            ));
            out.push_str(&format!(
                "causal_memory_request_duration_seconds_count{{{labels}}} {}\n",
                h.count
            ));
        }

        out.push_str("# TYPE causal_memory_recall_seeds_total counter\n");
        let mut seeds: Vec<_> = g.recall_seeds.iter().collect();
        seeds.sort();
        for (source, n) in seeds {
            out.push_str(&format!(
                "causal_memory_recall_seeds_total{{source=\"{source}\"}} {n}\n"
            ));
        }

        out.push_str("# TYPE causal_memory_recall_activated_nodes histogram\n");
        let h = &g.recall_activated_nodes;
        let mut cumulative = 0u64;
        for (i, le) in ACTIVATED_BUCKETS.iter().enumerate() {
            cumulative += h.buckets[i];
            out.push_str(&format!(
                "causal_memory_recall_activated_nodes_bucket{{le=\"{le}\"}} {cumulative}\n"
            ));
        }
        cumulative += h.buckets[ACTIVATED_BUCKETS.len()];
        out.push_str(&format!(
            "causal_memory_recall_activated_nodes_bucket{{le=\"+Inf\"}} {cumulative}\n"
        ));
        out.push_str(&format!(
            "causal_memory_recall_activated_nodes_sum {:.1}\n",
            h.sum
        ));
        out.push_str(&format!(
            "causal_memory_recall_activated_nodes_count {}\n",
            h.count
        ));

        out.push_str("# TYPE causal_memory_recall_results_total counter\n");
        let mut results: Vec<_> = g.recall_results.iter().collect();
        results.sort();
        for ((layer, source), n) in results {
            out.push_str(&format!(
                "causal_memory_recall_results_total{{layer=\"{layer}\",source=\"{source}\"}} {n}\n"
            ));
        }

        out.push_str("# TYPE causal_memory_recall_audit_errors_total counter\n");
        out.push_str(&format!(
            "causal_memory_recall_audit_errors_total {}\n",
            g.recall_audit_errors
        ));

        drop(g);
        if let Some(store) = store {
            out.push_str("# TYPE causal_memory_store_edges gauge\n");
            out.push_str(&format!(
                "causal_memory_store_edges {}\n",
                store.count_edges().unwrap_or(0)
            ));
            out.push_str("# TYPE causal_memory_store_facts gauge\n");
            out.push_str(&format!(
                "causal_memory_store_facts {}\n",
                store.count_facts().unwrap_or(0)
            ));
            out.push_str("# TYPE causal_memory_store_chunks gauge\n");
            out.push_str(&format!(
                "causal_memory_store_chunks {}\n",
                store.count_chunks().unwrap_or(0)
            ));
        }

        let uptime = chrono::Utc::now()
            .timestamp()
            .saturating_sub(self.started_unix as i64) as u64;
        self.uptime.store(uptime, Ordering::Relaxed);
        out.push_str("# TYPE causal_memory_uptime_seconds gauge\n");
        out.push_str(&format!("causal_memory_uptime_seconds {uptime}\n"));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_text_format_is_valid() {
        let m = Metrics {
            inner: Mutex::new(Inner {
                recall_activated_nodes: Histogram::with_buckets(ACTIVATED_BUCKETS),
                ..Inner::default()
            }),
            started_unix: 1000,
            uptime: AtomicU64::new(0),
        };
        m.record_request("mcp-http", "search_causal", "ok", 0.042);
        m.record_request("mcp-http", "search_causal", "ok", 0.20);
        m.record_request("mcp-http", "search_causal", "error", 6.5);
        m.record_recall_seeds("bm25", 7);
        m.record_activated_nodes(120);
        m.record_recall_result("causal", "spread");
        m.record_audit_error();

        let text = m.render_prometheus(None);
        // Counters with full label sets.
        assert!(text.contains(
            "causal_memory_requests_total{server=\"mcp-http\",tool=\"search_causal\",status=\"ok\"} 2"
        ));
        assert!(text.contains("status=\"error\"} 1"));
        // Histogram: cumulative buckets, +Inf == count, sum present.
        assert!(text.contains(
            "causal_memory_request_duration_seconds_bucket{server=\"mcp-http\",tool=\"search_causal\",le=\"0.05\"} 1"
        ));
        assert!(text.contains("le=\"+Inf\"} 3"));
        assert!(text.contains("_count{server=\"mcp-http\",tool=\"search_causal\"} 3"));
        // Recall metrics.
        assert!(text.contains("causal_memory_recall_seeds_total{source=\"bm25\"} 7"));
        assert!(text.contains("causal_memory_recall_activated_nodes_bucket{le=\"500\"} 1"));
        assert!(text
            .contains("causal_memory_recall_results_total{layer=\"causal\",source=\"spread\"} 1"));
        assert!(text.contains("causal_memory_recall_audit_errors_total 1"));
        // Every metric line is `name{labels} value` — no empty metric names.
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            assert!(line.contains(' ') || line.contains('{'), "bad line: {line}");
        }
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let mut h = Histogram::with_buckets(DURATION_BUCKETS);
        h.observe(0.001, DURATION_BUCKETS); // first bucket
        h.observe(0.07, DURATION_BUCKETS); // le=0.1 bucket
        h.observe(9.0, DURATION_BUCKETS); // +Inf
        assert_eq!(h.buckets[0], 1);
        assert_eq!(h.buckets[3], 1); // 0.1 bucket
        assert_eq!(h.buckets[DURATION_BUCKETS.len()], 1);
        assert_eq!(h.count, 3);
    }
}
