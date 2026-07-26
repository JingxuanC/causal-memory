//! Causal Memory MCP Server — entry point.
//!
//! Run: causal-memory
//! Connects via stdio (standard MCP transport). The host agent spawns this
//! as a child process and communicates via JSON-RPC over stdin/stdout.
//!
//! Data path: ~/.local/share/causal-memory/causal.db (or CAUSAL_MEMORY_DB env var)

use std::path::PathBuf;

use causal_memory::{server::CausalMemoryServer, store::CausalStore};

fn get_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("CAUSAL_MEMORY_DB") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".local/share/causal-memory")
        .join("causal.db")
}

fn main() -> anyhow::Result<()> {
    // Logging goes to stderr only (stdout is reserved for MCP protocol)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    tracing::info!("Opening causal memory DB at {}", db_path.display());
    let store = CausalStore::open(&db_path)?;
    let edge_count = store.count_edges().unwrap_or(0);
    tracing::info!("Causal memory ready: {} existing edges", edge_count);

    let server = CausalMemoryServer::new(store);

    // MCP stdio transport: read from stdin, write to stdout
    use rmcp::ServiceExt;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let transport = (tokio::io::stdin(), tokio::io::stdout());
        server
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;
        tracing::info!("MCP server shut down");
        Ok(())
    })
}
