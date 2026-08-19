//! MCP server handler — exposes 14 tools for unified agent memory.
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
}

impl CausalMemoryServer {
    pub fn new(store: CausalStore) -> Self {
        Self {
            memory: Memory::new(store),
        }
    }
}

pub mod tools;

#[cfg(test)]
mod tests;

// Stable public paths for cross-module consumers (bench_tokens).
#[allow(unused_imports)]
pub(crate) use causal_memory::memory::format::{format_entry_layered, rrf_fuse};
