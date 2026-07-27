//! Embedding client for semantic retrieval (Phase 6).
//!
//! When CAUSAL_MEMORY_EMBED_API + CAUSAL_MEMORY_EMBED_KEY are set (falling back
//! to the CAUSAL_MEMORY_LLM_* pair), edges get vector embeddings and
//! `search_causal` can rank by cosine similarity instead of LIKE matching.
//! Otherwise returns None and the caller falls back to the keyword path.
//!
//! Compatible with any OpenAI-style /v1/embeddings endpoint.
//!
//! HTTP style mirrors `llm.rs`: async `reqwest` (the store layer is sync, the
//! caller bridges via a tokio runtime), same header style, same
//! "unconfigured → None → caller falls back" contract.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Configuration for the embedding endpoint. Read from environment variables.
#[derive(Debug, Clone)]
pub struct EmbedConfig {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

impl EmbedConfig {
    /// Load from env. Returns None if not configured (semantic search unavailable).
    ///
    /// - CAUSAL_MEMORY_EMBED_API, default: CAUSAL_MEMORY_LLM_API
    /// - CAUSAL_MEMORY_EMBED_KEY, default: CAUSAL_MEMORY_LLM_KEY
    /// - CAUSAL_MEMORY_EMBED_MODEL, default: "text-embedding-3-small"
    pub fn from_env() -> Option<Self> {
        Self::resolve(
            std::env::var("CAUSAL_MEMORY_EMBED_API").ok().as_deref(),
            std::env::var("CAUSAL_MEMORY_EMBED_KEY").ok().as_deref(),
            std::env::var("CAUSAL_MEMORY_EMBED_MODEL").ok().as_deref(),
            std::env::var("CAUSAL_MEMORY_LLM_API").ok().as_deref(),
            std::env::var("CAUSAL_MEMORY_LLM_KEY").ok().as_deref(),
        )
    }

    /// Pure resolution logic, split from env access so tests never mutate
    /// process env (env writes race under `cargo test`'s parallel harness).
    pub fn resolve(
        embed_api: Option<&str>,
        embed_key: Option<&str>,
        embed_model: Option<&str>,
        llm_api: Option<&str>,
        llm_key: Option<&str>,
    ) -> Option<Self> {
        let api_base = embed_api.or(llm_api)?.to_string();
        let api_key = embed_key.or(llm_key)?.to_string();
        let model = embed_model.unwrap_or("text-embedding-3-small").to_string();
        Some(Self {
            api_base,
            api_key,
            model,
        })
    }
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

/// OpenAI-compatible embedding client.
pub struct Embedder {
    config: EmbedConfig,
    client: reqwest::Client,
}

impl Embedder {
    pub fn new(config: EmbedConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// The embedding model name (recorded alongside stored vectors).
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Embed a single text via the /embeddings endpoint → f32 vector.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.config.api_base.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&EmbedRequest {
                model: &self.config.model,
                input: text,
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Embedding API returned status {}", resp.status());
        }

        let embed_resp: EmbedResponse = resp.json().await?;
        embed_resp
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| anyhow::anyhow!("No embedding in API response"))
    }
}

/// Cosine similarity between two vectors.
/// Returns 0.0 for mismatched lengths, empty input, or a zero vector
/// (all cases where similarity is undefined or meaningless for ranking).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Serialize an f32 vector to a little-endian byte blob for SQLite BLOB storage.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize a little-endian blob back to an f32 vector.
/// Errors when the blob length is not a multiple of 4 (corrupt/foreign data).
pub fn blob_to_vec(b: &[u8]) -> Result<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        anyhow::bail!("embedding blob length {} is not a multiple of 4", b.len());
    }
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_identical() {
        let v = [1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_cosine_orthogonal() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn test_cosine_opposite() {
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_cosine_zero_vector() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn test_cosine_length_mismatch() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0, 2.0, 3.0]), 0.0);
    }

    #[test]
    fn test_blob_roundtrip() {
        let v = vec![1.5f32, -2.25, 0.0, 42.0, f32::MIN_POSITIVE];
        let blob = vec_to_blob(&v);
        assert_eq!(blob.len(), v.len() * 4);
        assert_eq!(blob_to_vec(&blob).unwrap(), v);
    }

    #[test]
    fn test_blob_bad_length() {
        assert!(blob_to_vec(&[0u8; 3]).is_err());
        assert!(blob_to_vec(&[0u8; 5]).is_err());
    }

    #[test]
    fn test_resolve_unconfigured() {
        assert!(EmbedConfig::resolve(None, None, None, None, None).is_none());
        // base without key, key without base — both incomplete
        assert!(EmbedConfig::resolve(Some("http://x/v1"), None, None, None, None).is_none());
        assert!(EmbedConfig::resolve(None, None, None, None, Some("k")).is_none());
    }

    #[test]
    fn test_resolve_defaults_and_fallbacks() {
        // Model defaults to text-embedding-3-small
        let c = EmbedConfig::resolve(Some("http://x/v1"), Some("k"), None, None, None).unwrap();
        assert_eq!(c.model, "text-embedding-3-small");

        // Falls back to the LLM pair
        let c = EmbedConfig::resolve(None, None, None, Some("http://llm/v1"), Some("lk")).unwrap();
        assert_eq!(c.api_base, "http://llm/v1");
        assert_eq!(c.api_key, "lk");

        // EMBED_* takes precedence over LLM_*
        let c = EmbedConfig::resolve(
            Some("http://e/v1"),
            Some("ek"),
            Some("bge-m3"),
            Some("http://llm/v1"),
            Some("lk"),
        )
        .unwrap();
        assert_eq!(c.api_base, "http://e/v1");
        assert_eq!(c.api_key, "ek");
        assert_eq!(c.model, "bge-m3");
    }
}
