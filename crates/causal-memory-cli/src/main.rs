//! Causal Memory MCP Server — thin binary shell over the library
//! dispatcher (src/lib.rs), so the cargo binary and the pip console
//! script share one code path.

fn main() {
    std::process::exit(causal_memory_cli::run(
        &std::env::args().skip(1).collect::<Vec<_>>(),
    ));
}
