//! Voyage AI embeddings client (#104). Anthropic-recommended provider;
//! API key stored in the macOS Keychain (`voyage-api-key` account).

use async_trait::async_trait;
use serde::Deserialize;

const ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";

pub const MODEL: &str = "voyage-3";
pub const VEC_DIM: usize = 1024;

/// Voyage uses asymmetric input_type — `document` for the corpus,
/// `query` for retrieval queries — to improve recall by ~5-10%.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputType {
    Document,
    Query,
}

impl InputType {
    fn as_str(&self) -> &'static str {
        match self {
            InputType::Document => "document",
            InputType::Query => "query",
        }
    }
}

/// Trait so the worker and retriever can be unit-tested against a fake
/// embedder without hitting the network.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_batch(
        &self,
        texts: &[String],
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>, String>;
}

pub struct VoyageClient {
    api_key: String,
    client: reqwest::Client,
}

impl VoyageClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Deserialize)]
struct VoyageResponse {
    data: Vec<VoyageDataItem>,
}

#[derive(Deserialize)]
struct VoyageDataItem {
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct VoyageError {
    detail: Option<String>,
}

#[async_trait]
impl Embedder for VoyageClient {
    async fn embed_batch(
        &self,
        texts: &[String],
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({
            "input": texts,
            "model": MODEL,
            "input_type": input_type.as_str(),
        });
        let resp = self
            .client
            .post(ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("voyage request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let raw = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<VoyageError>(&raw)
                .ok()
                .and_then(|e| e.detail)
                .unwrap_or_else(|| raw.clone());
            return Err(match status.as_u16() {
                401 => "voyage: invalid API key".to_string(),
                429 => format!("voyage: rate limited — {detail}"),
                _ => format!("voyage HTTP {status}: {detail}"),
            });
        }

        let parsed: VoyageResponse = resp
            .json()
            .await
            .map_err(|e| format!("voyage response parse failed: {e}"))?;
        if parsed.data.len() != texts.len() {
            return Err(format!(
                "voyage returned {} embeddings for {} inputs",
                parsed.data.len(),
                texts.len()
            ));
        }
        let vectors: Vec<Vec<f32>> = parsed.data.into_iter().map(|d| d.embedding).collect();
        for v in &vectors {
            if v.len() != VEC_DIM {
                return Err(format!(
                    "voyage returned vector of dim {} (expected {VEC_DIM})",
                    v.len()
                ));
            }
        }
        Ok(vectors)
    }
}

/// Pack a Vec<f32> into a tightly-packed little-endian byte buffer the
/// shape vec0 expects (`float[N]` = `4 * N` bytes).
pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}
