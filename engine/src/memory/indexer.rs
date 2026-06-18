use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::AppError;
use crate::models::ToolMessage;

use super::chunker::chunk_code_to_strings;
use super::embeddings::EmbeddingProvider;
use super::store::MemoryStore;
use super::{MemoryEntry, MemorySource, MemoryTier};

pub struct MemoryIndexer<'a> {
    store: &'a MemoryStore,
    embedder: &'a dyn EmbeddingProvider,
    chunk_size: usize,
    chunk_overlap: usize,
    max_code_lines: usize,
}

impl<'a> MemoryIndexer<'a> {
    pub fn new(store: &'a MemoryStore, embedder: &'a dyn EmbeddingProvider) -> Self {
        Self {
            store,
            embedder,
            chunk_size: 512,
            chunk_overlap: 64,
            max_code_lines: 80,
        }
    }

    #[must_use]
    pub fn with_chunk_size(mut self, size: usize, overlap: usize) -> Self {
        self.chunk_size = size;
        self.chunk_overlap = overlap;
        self
    }

    #[must_use]
    pub fn with_max_code_lines(mut self, max: usize) -> Self {
        self.max_code_lines = max;
        self
    }

    pub async fn index_file(&self, path: &Path, content: &str) -> Result<usize, AppError> {
        let path_str = path.to_string_lossy().to_string();
        self.store.delete_by_source(
            &MemorySource::File {
                path: path_str.clone(),
            }
            .to_tag(),
        )?;

        let is_code = is_code_file(path);
        let chunks = if is_code {
            chunk_code_to_strings(content, path, self.max_code_lines)
        } else {
            chunk_text(content, self.chunk_size, self.chunk_overlap)
        };

        if chunks.is_empty() {
            return Ok(0);
        }

        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let embeddings = self.embedder.embed_batch(&chunk_refs).await?;
        let now = unix_timestamp();

        for (i, (chunk, emb)) in chunks.iter().zip(embeddings).enumerate() {
            let entry = MemoryEntry {
                id: format!("file:{path_str}:{i}"),
                content: chunk.clone(),
                embedding: emb,
                source: MemorySource::File {
                    path: path_str.clone(),
                },
                tier: MemoryTier::Medium,
                timestamp: now,
            };
            self.store.insert(&entry)?;
        }

        Ok(chunks.len())
    }

    pub async fn index_session(
        &self,
        session_id: &str,
        messages: &[ToolMessage],
    ) -> Result<usize, AppError> {
        self.store.delete_by_source(
            &MemorySource::Session {
                session_id: session_id.to_string(),
            }
            .to_tag(),
        )?;

        let text = summarize_session(messages);
        let chunks = chunk_text(&text, self.chunk_size, self.chunk_overlap);

        if chunks.is_empty() {
            return Ok(0);
        }

        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let embeddings = self.embedder.embed_batch(&chunk_refs).await?;
        let now = unix_timestamp();

        for (i, (chunk, emb)) in chunks.iter().zip(embeddings).enumerate() {
            let entry = MemoryEntry {
                id: format!("session:{session_id}:{i}"),
                content: chunk.clone(),
                embedding: emb,
                source: MemorySource::Session {
                    session_id: session_id.to_string(),
                },
                tier: MemoryTier::Short,
                timestamp: now,
            };
            self.store.insert(&entry)?;
        }

        Ok(chunks.len())
    }

    pub async fn index_text(
        &self,
        label: &str,
        text: &str,
        source: MemorySource,
    ) -> Result<usize, AppError> {
        let chunks = chunk_text(text, self.chunk_size, self.chunk_overlap);

        if chunks.is_empty() {
            return Ok(0);
        }

        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let embeddings = self.embedder.embed_batch(&chunk_refs).await?;
        let now = unix_timestamp();

        for (i, (chunk, emb)) in chunks.iter().zip(embeddings).enumerate() {
            let entry = MemoryEntry {
                id: format!("{label}:{i}"),
                content: chunk.clone(),
                embedding: emb,
                source: source.clone(),
                tier: MemoryTier::Short,
                timestamp: now,
            };
            self.store.insert(&entry)?;
        }

        Ok(chunks.len())
    }

    pub async fn index_persistent_fact(
        &self,
        fact_id: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let emb = self.embedder.embed(content).await?;
        let entry = MemoryEntry {
            id: format!("fact:{fact_id}"),
            content: content.to_string(),
            embedding: emb,
            source: MemorySource::Manual,
            tier: MemoryTier::Long,
            timestamp: unix_timestamp(),
        };
        self.store.insert(&entry)
    }
}

fn is_code_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "go"
                | "java"
                | "kt"
                | "scala"
                | "rb"
                | "c"
                | "cpp"
                | "h"
                | "hpp"
                | "cs"
                | "swift"
                | "zig"
                | "lua"
                | "sh"
                | "bash"
                | "zsh"
                | "mjs"
        )
    )
}

fn summarize_session(messages: &[ToolMessage]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let role = &msg.role;
        if let Some(content) = &msg.content {
            if !content.is_empty() {
                parts.push(format!("[{role}]: {content}"));
            }
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                parts.push(format!(
                    "[{role} -> tool:{name}]: {args}",
                    name = call.name,
                    args = serde_json::to_string(&call.arguments).unwrap_or_default()
                ));
            }
        }
    }
    parts.join("\n")
}

pub fn chunk_text(text: &str, max_tokens: usize, overlap: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    if words.len() <= max_tokens {
        return vec![words.join(" ")];
    }

    let step = max_tokens.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < words.len() {
        let end = (start + max_tokens).min(words.len());
        chunks.push(words[start..end].join(" "));
        start += step;
    }

    chunks
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_short_text_single_chunk() {
        let chunks = chunk_text("hello world", 512, 64);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello world");
    }

    #[test]
    fn chunk_exact_boundary() {
        let text = (0..512)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk_text(&text, 512, 64);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn chunk_with_overlap() {
        let text = (0..100)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk_text(&text, 30, 10);
        assert!(chunks.len() > 1);
        assert!(chunks.len() <= 6);
    }

    #[test]
    fn chunk_empty_text() {
        let chunks = chunk_text("", 512, 64);
        assert!(chunks.is_empty());
    }

    #[test]
    fn is_code_detects_known_extensions() {
        assert!(is_code_file(Path::new("foo.rs")));
        assert!(is_code_file(Path::new("bar.ts")));
        assert!(is_code_file(Path::new("baz.py")));
        assert!(!is_code_file(Path::new("readme.md")));
        assert!(!is_code_file(Path::new("data.json")));
    }
}
