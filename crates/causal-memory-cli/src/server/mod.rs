//! MCP server handler — exposes 16 tools for unified agent memory.
//!
//! Thin shell: every tool handler parses its rmcp parameters and delegates
//! to the shared library facade `causal_memory::memory::Memory`, which owns
//! all orchestration logic (see that module for the tool semantics). The
//! Python bindings (`causal-memory-py`) sit on the same facade.
//!
//! Tools: record_decision, search_causal, record_fact, search_facts,
//! search_memory, trace_cause, trace_cause_chain, invalidate_decision,
//! search_patterns, causal_directory, intervention_query,
//! counterfactual_query, reconstruct_lesson, remember.

use causal_memory::memory::Memory;
use causal_memory::store::CausalStore;

/// MCP-facing wrapper around the shared memory facade. Keeps the historical
/// name used by the CLI wiring (misc.rs) and the rmcp macros.
pub struct CausalMemoryServer {
    pub(crate) memory: Memory,
    /// Observability label: "mcp-stdio" (default) or "mcp-http".
    pub(crate) label: &'static str,
}

impl CausalMemoryServer {
    pub fn new(store: CausalStore) -> Self {
        Self::new_with_label(store, "mcp-stdio")
    }

    /// Same facade, labeled for observability (request metrics + recall
    /// audit rows carry the label).
    pub fn new_with_label(store: CausalStore, label: &'static str) -> Self {
        Self {
            memory: Memory::new_with_label(store, label),
            label,
        }
    }

    /// RED metrics + one tracing span per tool call. Status is derived from
    /// the facade's error marker (❌ prefix) — the facade's contract.
    pub(crate) fn timed(&self, tool: &'static str, f: impl FnOnce() -> String) -> String {
        let t0 = std::time::Instant::now();
        let out = f();
        let status = if out.starts_with('❌') {
            "error"
        } else {
            "ok"
        };
        let latency_ms = t0.elapsed().as_millis() as u64;
        causal_memory::observability::metrics().record_request(
            self.label,
            tool,
            status,
            t0.elapsed().as_secs_f64(),
        );
        tracing::info!(tool, status, latency_ms, server = self.label, "tool call");
        out
    }
}

pub mod tools;

#[cfg(test)]
mod tests;

// Stable public paths for cross-module consumers (bench_tokens).
#[allow(unused_imports)]
pub(crate) use causal_memory::memory::format::{format_entry_layered, rrf_fuse};
