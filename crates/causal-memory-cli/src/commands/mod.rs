//! CLI subcommand implementations, split from main.rs (pure move).

pub mod distill;
pub mod git;
pub mod io;
pub mod maintenance;
pub mod misc;
pub mod wiki;

use std::path::PathBuf;

use causal_memory::session::{agent_kind_from_str, AgentKind};

/// Parse `--agent <name>` and the positional session path from CLI args.
pub(crate) fn parse_agent_path(args: &[String]) -> (AgentKind, PathBuf) {
    let mut path = PathBuf::new();
    let mut kind = AgentKind::Grok;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent" => {
                if let Some(v) = args.get(i + 1) {
                    match agent_kind_from_str(v) {
                        Some(k) => kind = k,
                        None => eprintln!("Unknown agent {v:?}; falling back to grok"),
                    }
                }
                i += 2;
            }
            s if s.starts_with('-') => i += 1,
            s => {
                if path.as_os_str().is_empty() {
                    path = PathBuf::from(s);
                }
                i += 1;
            }
        }
    }
    (kind, path)
}
