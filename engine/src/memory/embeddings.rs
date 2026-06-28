use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

use crate::error::AppError;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn dimension(&self) -> usize;
    async fn embed(&self, text: &str) -> Result<Vec<f32>, AppError>;
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, AppError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }
    fn clone_box(&self) -> Box<dyn EmbeddingProvider>;
}

pub struct OpenAiEmbedder {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiEmbedder {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            api_key,
            model: "text-embedding-3-small".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    #[must_use]
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Point the embedder at an OpenAI-compatible base URL (e.g. `GetAIBD`).
    #[must_use]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url)
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedder {
    fn dimension(&self) -> usize {
        if self.model.contains("3-small") {
            1536
        } else if self.model.contains("3-large") {
            3072
        } else {
            1536
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AppError> {
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let resp: serde_json::Value = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::ProviderError(format!("openai embed: {e}")))?
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("openai embed parse: {e}")))?;

        parse_openai_embedding(&resp)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, AppError> {
        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });

        let resp: serde_json::Value = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::ProviderError(format!("openai batch embed: {e}")))?
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("openai batch embed parse: {e}")))?;

        let data = resp["data"]
            .as_array()
            .ok_or_else(|| AppError::ProviderError("no data in embedding response".into()))?;

        data.iter()
            .map(|item| {
                item["embedding"]
                    .as_array()
                    .ok_or_else(|| AppError::ProviderError("missing embedding".into()))
                    .and_then(|arr| {
                        arr.iter()
                            .map(|v| {
                                json_f64_to_f32(v)
                                    .ok_or_else(|| AppError::ProviderError("bad float".into()))
                            })
                            .collect()
                    })
            })
            .collect()
    }

    fn clone_box(&self) -> Box<dyn EmbeddingProvider> {
        Box::new(Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
        })
    }
}

#[allow(clippy::cast_possible_truncation)]
fn json_f64_to_f32(v: &serde_json::Value) -> Option<f32> {
    v.as_f64().map(|f| f as f32)
}

fn parse_openai_embedding(resp: &serde_json::Value) -> Result<Vec<f32>, AppError> {
    resp["data"][0]["embedding"]
        .as_array()
        .ok_or_else(|| AppError::ProviderError("no embedding in response".into()))?
        .iter()
        .map(|v| {
            json_f64_to_f32(v)
                .ok_or_else(|| AppError::ProviderError("bad float in embedding".into()))
        })
        .collect()
}

pub struct OllamaEmbedder {
    client: Client,
    base_url: String,
    model: String,
}

impl OllamaEmbedder {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            base_url,
            model,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbedder {
    fn dimension(&self) -> usize {
        768
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AppError> {
        let body = serde_json::json!({
            "model": self.model,
            "prompt": text,
        });

        let resp: serde_json::Value = self
            .client
            .post(format!("{}/api/embeddings", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::ProviderError(format!("ollama embed: {e}")))?
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("ollama embed parse: {e}")))?;

        resp["embedding"]
            .as_array()
            .ok_or_else(|| AppError::ProviderError("no embedding in ollama response".into()))?
            .iter()
            .map(|v| json_f64_to_f32(v).ok_or_else(|| AppError::ProviderError("bad float".into())))
            .collect()
    }

    fn clone_box(&self) -> Box<dyn EmbeddingProvider> {
        Box::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
        })
    }
}

pub struct GeminiEmbedder {
    client: Client,
    api_key: String,
    model: String,
}

impl GeminiEmbedder {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            api_key,
            model: "text-embedding-004".to_string(),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbedder {
    fn dimension(&self) -> usize {
        768
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AppError> {
        let body = serde_json::json!({
            "model": format!("models/{}", self.model),
            "content": { "parts": [{ "text": text }] },
        });

        // The key goes in the `x-goog-api-key` header, NOT the URL query string:
        // `reqwest::Error`'s Display includes the request URL, so a key in the query
        // would leak into `AppError::ProviderError` (and logs) on any connect/timeout
        // error.
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:embedContent",
            self.model
        );

        let resp: serde_json::Value = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::ProviderError(format!("gemini embed: {e}")))?
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("gemini embed parse: {e}")))?;

        resp["embedding"]["values"]
            .as_array()
            .ok_or_else(|| AppError::ProviderError("no embedding in gemini response".into()))?
            .iter()
            .map(|v| json_f64_to_f32(v).ok_or_else(|| AppError::ProviderError("bad float".into())))
            .collect()
    }

    fn clone_box(&self) -> Box<dyn EmbeddingProvider> {
        Box::new(Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
        })
    }
}
