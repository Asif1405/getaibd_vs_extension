use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::AppError;
use crate::models::ToolMessage;

use super::chunker::chunk_code;
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

    /// Runs the blocking `SQLite` writes (optional delete-by-source, then inserts) on
    /// the blocking pool. `rusqlite` is synchronous, so doing this inline would stall
    /// a tokio worker for the whole batch; `spawn_blocking` keeps the async runtime
    /// responsive (matches the retrieval path and `workspace.rs`). Deleting alongside
    /// the inserts (rather than up-front) also avoids wiping old entries when an
    /// embedding call fails midway.
    async fn write_entries(
        &self,
        delete_tag: Option<String>,
        entries: Vec<MemoryEntry>,
    ) -> Result<(), AppError> {
        if delete_tag.is_none() && entries.is_empty() {
            return Ok(());
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            if let Some(tag) = delete_tag {
                store.delete_by_source(&tag)?;
            }
            for entry in &entries {
                store.insert(entry)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| AppError::ProviderError(format!("memory index join: {e}")))?
    }

    pub async fn index_file(&self, path: &Path, content: &str) -> Result<usize, AppError> {
        let path_str = path.to_string_lossy().to_string();
        let delete_tag = MemorySource::File {
            path: path_str.clone(),
        }
        .to_tag();

        let is_code = is_code_file(path);
        let chunks = if is_code {
            // Prefix each code chunk with a one-line locator (path, symbol, line
            // range). This gives the embedding and the retrieved context explicit
            // symbol-level attribution without a separate DB column.
            chunk_code(content, path, self.max_code_lines)
                .into_iter()
                .map(|c| {
                    let head = match &c.symbol {
                        Some(sym) => format!(
                            "[{path_str}] {} {} (lines {}-{})",
                            c.kind.as_str(),
                            sym,
                            c.start_line,
                            c.end_line
                        ),
                        None => format!(
                            "[{path_str}] (lines {}-{})",
                            c.start_line, c.end_line
                        ),
                    };
                    format!("{head}\n{}", c.content)
                })
                .collect()
        } else {
            chunk_text(content, self.chunk_size, self.chunk_overlap)
        };

        if chunks.is_empty() {
            // Still clear any stale entries for this file.
            self.write_entries(Some(delete_tag), Vec::new()).await?;
            return Ok(0);
        }

        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let embeddings = self.embedder.embed_batch(&chunk_refs).await?;
        let now = unix_timestamp();

        let entries: Vec<MemoryEntry> = chunks
            .iter()
            .zip(embeddings)
            .enumerate()
            .map(|(i, (chunk, emb))| MemoryEntry {
                id: format!("file:{path_str}:{i}"),
                content: chunk.clone(),
                embedding: emb,
                source: MemorySource::File {
                    path: path_str.clone(),
                },
                tier: MemoryTier::Medium,
                timestamp: now,
            })
            .collect();
        let n = entries.len();
        self.write_entries(Some(delete_tag), entries).await?;
        Ok(n)
    }

    pub async fn index_session(
        &self,
        session_id: &str,
        messages: &[ToolMessage],
    ) -> Result<usize, AppError> {
        let delete_tag = MemorySource::Session {
            session_id: session_id.to_string(),
        }
        .to_tag();

        let text = summarize_session(messages);
        let chunks = chunk_text(&text, self.chunk_size, self.chunk_overlap);

        if chunks.is_empty() {
            self.write_entries(Some(delete_tag), Vec::new()).await?;
            return Ok(0);
        }

        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let embeddings = self.embedder.embed_batch(&chunk_refs).await?;
        let now = unix_timestamp();

        let entries: Vec<MemoryEntry> = chunks
            .iter()
            .zip(embeddings)
            .enumerate()
            .map(|(i, (chunk, emb))| MemoryEntry {
                id: format!("session:{session_id}:{i}"),
                content: chunk.clone(),
                embedding: emb,
                source: MemorySource::Session {
                    session_id: session_id.to_string(),
                },
                tier: MemoryTier::Short,
                timestamp: now,
            })
            .collect();
        let n = entries.len();
        self.write_entries(Some(delete_tag), entries).await?;
        Ok(n)
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

        let entries: Vec<MemoryEntry> = chunks
            .iter()
            .zip(embeddings)
            .enumerate()
            .map(|(i, (chunk, emb))| MemoryEntry {
                id: format!("{label}:{i}"),
                content: chunk.clone(),
                embedding: emb,
                source: source.clone(),
                tier: MemoryTier::Short,
                timestamp: now,
            })
            .collect();
        let n = entries.len();
        self.write_entries(None, entries).await?;
        Ok(n)
    }

    /// Stores one distilled episodic memory for a completed task, replacing any
    /// prior record for the same session.
    pub async fn index_episode(&self, session_id: &str, episode: &str) -> Result<(), AppError> {
        let source = MemorySource::Session {
            session_id: session_id.to_string(),
        };
        let delete_tag = source.to_tag();

        if episode.trim().is_empty() {
            return self.write_entries(Some(delete_tag), Vec::new()).await;
        }

        let emb = self.embedder.embed(episode).await?;
        let entry = MemoryEntry {
            id: format!("session:{session_id}:episode"),
            content: episode.to_string(),
            embedding: emb,
            source,
            tier: MemoryTier::Medium,
            timestamp: unix_timestamp(),
        };
        self.write_entries(Some(delete_tag), vec![entry]).await
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
        self.write_entries(None, vec![entry]).await
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
