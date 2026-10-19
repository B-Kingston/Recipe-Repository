//! Client for the EmbeddingGemma 2 sidecar: a llama.cpp `llama-server
//! --embeddings` process with the Q8_0 text model and its vision/audio
//! projector. It serves the OpenAI-style `/v1/embeddings` endpoint and
//! returns L2-normalised 768-dimensional vectors.
//!
//! Text, JPEG images, and WAV audio map into one vector space, so a step's
//! text can be compared with an audio window or a video frame. The server
//! fixes the vision budget (70 soft tokens per image) through its own flags.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::env;
use std::time::Duration;

/// Native EmbeddingGemma 2 output size.
pub(crate) const DIMENSIONS: usize = 768;
/// Task prefix for retrieval queries (library search, recipe steps matched
/// against video moments, dish-photo prompts).
pub(crate) const QUERY_PREFIX: &str = "task: search result | query: ";
/// Inputs per HTTP request. The server embeds them one after another, so this
/// only bounds each request's duration and body size.
const REQUEST_BATCH: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Formats a retrieval document the way EmbeddingGemma 2 was trained.
pub(crate) fn document(title: &str, body: &str) -> String {
    let title = title.trim();
    let title = if title.is_empty() { "none" } else { title };
    format!("title: {title} | text: {body}")
}

/// Formats a retrieval query.
pub(crate) fn query(text: &str) -> String {
    format!("{QUERY_PREFIX}{}", text.trim())
}

/// One input to embed.
pub(crate) enum Input {
    Text(String),
    /// JPEG bytes.
    Image(Vec<u8>),
    /// WAV bytes (16 kHz mono PCM).
    Audio(Vec<u8>),
}

impl Input {
    fn to_json(&self) -> Value {
        match self {
            Input::Text(text) => Value::String(text.clone()),
            Input::Image(jpeg) => json!({"content": [{
                "type": "image_url",
                "image_url": {"url": format!("data:image/jpeg;base64,{}", BASE64.encode(jpeg))},
            }]}),
            Input::Audio(wav) => json!({"content": [{
                "type": "input_audio",
                "input_audio": {"data": BASE64.encode(wav), "format": "wav"},
            }]}),
        }
    }
}

/// HTTP client for the sidecar. Cheap to clone: reqwest shares its pool.
#[derive(Clone)]
pub(crate) struct Embedder {
    client: reqwest::Client,
    endpoint: String,
}

impl Embedder {
    /// The sidecar named by `EMBEDDING_URL` (for example
    /// `http://embeddings:8080`); `None` when unset, which turns the
    /// embedding features off.
    pub(crate) fn from_env() -> Option<Self> {
        let base = env::var("EMBEDDING_URL").ok()?;
        let base = base.trim();
        (!base.is_empty()).then(|| Self::new(base))
    }

    pub(crate) fn new(base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            endpoint: format!("{}/v1/embeddings", base_url.trim_end_matches('/')),
        }
    }

    /// Embeds every input, in order, as unit-length vectors.
    pub(crate) async fn embed(&self, inputs: &[Input]) -> Result<Vec<Vec<f32>>, String> {
        let mut vectors = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(REQUEST_BATCH) {
            vectors.extend(self.embed_batch(batch).await?);
        }
        Ok(vectors)
    }

    async fn embed_batch(&self, inputs: &[Input]) -> Result<Vec<Vec<f32>>, String> {
        let body = json!({
            "input": inputs.iter().map(Input::to_json).collect::<Vec<_>>(),
            "encoding_format": "float",
        });
        let response = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| format!("embedding service unreachable: {error}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("embedding service response failed: {error}"))?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "embedding service returned {status}: {}",
                detail.chars().take(300).collect::<String>()
            ));
        }
        parse_response(&bytes, inputs.len())
    }

    /// A handle for blocking media threads (inside `spawn_blocking`). Must be
    /// created on a runtime thread.
    pub(crate) fn blocking(&self) -> BlockingEmbedder {
        BlockingEmbedder {
            embedder: self.clone(),
            handle: tokio::runtime::Handle::current(),
        }
    }
}

/// Synchronous wrapper used by the ffmpeg pipelines, which run on blocking
/// threads. Never call it from an async task: `block_on` would panic there.
#[derive(Clone)]
pub(crate) struct BlockingEmbedder {
    embedder: Embedder,
    handle: tokio::runtime::Handle,
}

impl BlockingEmbedder {
    pub(crate) fn embed(&self, inputs: &[Input]) -> Result<Vec<Vec<f32>>, String> {
        self.handle.block_on(self.embedder.embed(inputs))
    }
}

fn parse_response(bytes: &[u8], expected: usize) -> Result<Vec<Vec<f32>>, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("embedding service sent invalid JSON: {error}"))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("embedding service response has no data")?;
    let mut rows: Vec<(usize, Vec<f32>)> = data
        .iter()
        .map(|row| {
            let index = row.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let vector = row
                .get("embedding")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_f64)
                        .map(|v| v as f32)
                        .collect()
                })
                .unwrap_or_default();
            (index, vector)
        })
        .collect();
    rows.sort_by_key(|(index, _)| *index);
    if rows.len() != expected || rows.iter().any(|(_, v)| v.len() != DIMENSIONS) {
        return Err(format!(
            "embedding service returned {} vectors for {expected} inputs",
            rows.len()
        ));
    }
    Ok(rows
        .into_iter()
        .map(|(_, mut vector)| {
            normalize(&mut vector);
            vector
        })
        .collect())
}

/// Scales a vector to unit length (a zero vector stays zero).
pub(crate) fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|v| *v /= norm);
    }
}

/// Cosine similarity of two unit vectors.
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Little-endian f32 bytes for a BLOB column.
pub(crate) fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Reads a stored vector; `None` when the BLOB is not a full vector.
pub(crate) fn from_blob(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != DIMENSIONS * 4 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}
