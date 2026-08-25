//! JSON config file support. Process environment variables always win over
//! the file; the file exists so `pip install` users can configure once
//! (`causal-memory setconfig KEY=VALUE`) instead of exporting env vars into
//! every agent process.
//!
//! Path resolution: `$CAUSAL_MEMORY_CONFIG` (explicit override) >
//! `~/.local/share/causal-memory/config.json` (default, next to the
//! default store).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Keys the config file is allowed to carry. `set` rejects anything else —
/// a typo'd key would be written but never read, a silent misconfiguration.
pub const ALLOWED_KEYS: &[&str] = &[
    "CAUSAL_MEMORY_EMBED_API",
    "CAUSAL_MEMORY_EMBED_KEY",
    "CAUSAL_MEMORY_EMBED_MODEL",
    "CAUSAL_MEMORY_LLM_API",
    "CAUSAL_MEMORY_LLM_KEY",
    "CAUSAL_MEMORY_LLM_MODEL",
    "CAUSAL_MEMORY_HTTP_TIMEOUT_SECS",
];

/// Resolved config-file path.
pub fn path() -> PathBuf {
    if let Ok(p) = std::env::var("CAUSAL_MEMORY_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/causal-memory/config.json")
}

/// Read a config value: process env first (empty counts as unset), then
/// the config file.
pub fn get(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    file_values().get(key).cloned()
}

/// Write key=value pairs into the config file (existing keys preserved,
/// empty value deletes the key, parent dirs created). Only ALLOWED_KEYS
/// are accepted — the whole batch is validated before anything is written.
pub fn set(pairs: &[(String, String)]) -> Result<(), String> {
    for (k, _) in pairs {
        if !ALLOWED_KEYS.contains(&k.as_str()) {
            return Err(format!(
                "unknown config key {k} (allowed: {})",
                ALLOWED_KEYS.join(", ")
            ));
        }
    }
    let path = path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut values = file_values();
    for (k, v) in pairs {
        if v.is_empty() {
            values.remove(k);
        } else {
            values.insert(k.clone(), v.clone());
        }
    }
    let json = serde_json::to_string_pretty(&values).map_err(|e| format!("encode config: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;
    store_cache(&path, values);
    Ok(())
}

/// The one shared cache cell (path, values) — a path change (e.g. tests
/// repointing CAUSAL_MEMORY_CONFIG) forces a reload.
fn cache_cell() -> &'static Mutex<(PathBuf, HashMap<String, String>)> {
    static CACHE: OnceLock<Mutex<(PathBuf, HashMap<String, String>)>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((PathBuf::new(), HashMap::new())))
}

/// File values with a per-path cache (the file is read once per process in
/// the common case).
fn file_values() -> HashMap<String, String> {
    let path = path();
    let mut guard = cache_cell().lock().unwrap_or_else(|e| e.into_inner());
    if guard.0 != path {
        guard.1 = load(&path);
        guard.0 = path;
    }
    guard.1.clone()
}

fn store_cache(path: &Path, values: HashMap<String, String>) {
    let mut guard = cache_cell().lock().unwrap_or_else(|e| e.into_inner());
    guard.0 = path.to_path_buf();
    guard.1 = values;
}

fn load(path: &Path) -> HashMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect()
}

const USAGE: &str = "causal-memory — config management
USAGE:
  causal-memory setconfig KEY=VALUE [KEY=VALUE...]   write config keys (empty VALUE deletes)
  causal-memory getconfig                            list configured values (*_KEY masked)
  causal-memory config-path                          print the config file path
Config file: $CAUSAL_MEMORY_CONFIG or ~/.local/share/causal-memory/config.json
Process env vars always override the file.";

/// `causal-memory` console entry point: config management subcommands.
/// Returns the process exit code (0 ok, 1 rejected, 2 usage error).
pub fn cli_main(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("setconfig") => {
            if args.len() < 2 {
                eprintln!("setconfig needs at least one KEY=VALUE pair\n{USAGE}");
                return 2;
            }
            let mut pairs = Vec::with_capacity(args.len() - 1);
            for a in &args[1..] {
                match a.split_once('=') {
                    Some((k, v)) if !k.is_empty() => {
                        pairs.push((k.to_string(), v.to_string()));
                    }
                    _ => {
                        eprintln!("invalid KEY=VALUE pair: {a}");
                        return 2;
                    }
                }
            }
            match set(&pairs) {
                Ok(()) => {
                    println!("wrote {} key(s) to {}", pairs.len(), path().display());
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Some("getconfig") => {
            println!("config file: {}", path().display());
            for key in ALLOWED_KEYS {
                match get(key) {
                    Some(v) => println!("{key}={}", display_value(key, &v)),
                    None => println!("{key}=(unset)"),
                }
            }
            0
        }
        Some("config-path") => {
            println!("{}", path().display());
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

/// `*_KEY` values are masked (first 4 chars + ***) so getconfig is safe to
/// paste into chats/issues.
fn display_value(key: &str, value: &str) -> String {
    if key.ends_with("_KEY") {
        let head: String = value.chars().take(4).collect();
        format!("{head}***")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test invariant: panicking on failure is desired"
)]
mod tests {
    use super::*;

    /// Env writes race under the parallel test harness — serialize this
    /// module's env-mutating tests and restore the var after each one.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard(&'static str, Option<String>);
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self(key, prev)
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self(key, prev)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }

    #[test]
    fn env_wins_over_file() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let _cfg = EnvGuard::set("CAUSAL_MEMORY_CONFIG", cfg.to_str().unwrap());
        let _key = EnvGuard::set("CAUSAL_MEMORY_LLM_API", "https://env.example");
        set(&[(
            "CAUSAL_MEMORY_LLM_API".to_string(),
            "https://file.example".to_string(),
        )])
        .unwrap();
        assert_eq!(
            get("CAUSAL_MEMORY_LLM_API").as_deref(),
            Some("https://env.example")
        );
    }

    #[test]
    fn set_get_roundtrip_and_delete() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Nested path: parent dirs must be created.
        let cfg = dir.path().join("sub/dir/config.json");
        let _cfg = EnvGuard::set("CAUSAL_MEMORY_CONFIG", cfg.to_str().unwrap());
        let _k1 = EnvGuard::unset("CAUSAL_MEMORY_LLM_MODEL");
        let _k2 = EnvGuard::unset("CAUSAL_MEMORY_EMBED_MODEL");

        set(&[
            (
                "CAUSAL_MEMORY_LLM_MODEL".to_string(),
                "deepseek-chat".to_string(),
            ),
            (
                "CAUSAL_MEMORY_EMBED_MODEL".to_string(),
                "bge-small".to_string(),
            ),
        ])
        .unwrap();
        assert!(cfg.exists());
        assert_eq!(
            get("CAUSAL_MEMORY_LLM_MODEL").as_deref(),
            Some("deepseek-chat")
        );

        // Second set preserves the existing keys; empty value deletes.
        set(&[("CAUSAL_MEMORY_LLM_MODEL".to_string(), String::new())]).unwrap();
        assert_eq!(get("CAUSAL_MEMORY_LLM_MODEL"), None);
        assert_eq!(
            get("CAUSAL_MEMORY_EMBED_MODEL").as_deref(),
            Some("bge-small")
        );

        // The file on disk is valid JSON carrying only the survivor.
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(on_disk.as_object().unwrap().len(), 1);
    }

    #[test]
    fn whitelist_rejects_unknown_keys() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let _cfg = EnvGuard::set("CAUSAL_MEMORY_CONFIG", cfg.to_str().unwrap());
        let err = set(&[("CAUSAL_MEMORY_BOGUS".to_string(), "x".to_string())]).unwrap_err();
        assert!(err.contains("unknown config key"), "{err}");
        assert!(!cfg.exists(), "rejected batch must not write the file");
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("nope.json");
        let _cfg = EnvGuard::set("CAUSAL_MEMORY_CONFIG", cfg.to_str().unwrap());
        let _k = EnvGuard::unset("CAUSAL_MEMORY_HTTP_TIMEOUT_SECS");
        assert_eq!(get("CAUSAL_MEMORY_HTTP_TIMEOUT_SECS"), None);
    }

    #[test]
    fn cli_main_exit_codes_and_roundtrip() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let _cfg = EnvGuard::set("CAUSAL_MEMORY_CONFIG", cfg.to_str().unwrap());
        let _k = EnvGuard::unset("CAUSAL_MEMORY_LLM_MODEL");

        assert_eq!(
            cli_main(&[
                "setconfig".to_string(),
                "CAUSAL_MEMORY_LLM_MODEL=deepseek-chat".to_string(),
            ]),
            0
        );
        assert_eq!(
            get("CAUSAL_MEMORY_LLM_MODEL").as_deref(),
            Some("deepseek-chat")
        );
        // Unknown key → non-zero; unknown command / missing pair → usage error.
        assert_eq!(
            cli_main(&["setconfig".to_string(), "BOGUS=1".to_string()]),
            1
        );
        assert_eq!(cli_main(&["setconfig".to_string()]), 2);
        assert_eq!(cli_main(&["frobnicate".to_string()]), 2);
        assert_eq!(cli_main(&["getconfig".to_string()]), 0);
        assert_eq!(cli_main(&["config-path".to_string()]), 0);
    }
}
