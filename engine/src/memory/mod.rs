pub mod cache;
pub mod call_graph;
pub mod chunker;
pub mod context_builder;
pub mod embeddings;
pub mod indexer;
pub mod merkle;
pub mod persistent;
pub mod project_graph;
pub mod recent_tracker;
pub mod store;
pub mod summarizer;
pub mod watcher;

use std::fmt::Write;

use crate::error::AppError;

pub use embeddings::EmbeddingProvider;
pub use indexer::MemoryIndexer;
pub use store::MemoryStore;

pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub source: MemorySource,
    pub tier: MemoryTier,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    Short,
    Medium,
    Long,
}

impl MemoryTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Medium => "medium",
            Self::Long => "long",
        }
    }

    pub fn parse_tier(s: &str) -> Self {
        match s {
            "medium" => Self::Medium,
            "long" => Self::Long,
            _ => Self::Short,
        }
    }

    pub fn boost(self) -> f32 {
        match self {
            Self::Short => 1.0,
            Self::Medium => 1.15,
            Self::Long => 1.3,
        }
    }
}

#[derive(Debug, Clone)]
pub enum MemorySource {
    Session { session_id: String },
    File { path: String },
    Manual,
}

impl MemorySource {
    pub fn to_tag(&self) -> String {
        match self {
            Self::Session { session_id } => format!("session:{session_id}"),
            Self::File { path } => format!("file:{path}"),
            Self::Manual => "manual".to_string(),
        }
    }

    pub fn from_tag(tag: &str) -> Self {
        if let Some(rest) = tag.strip_prefix("session:") {
            Self::Session {
                session_id: rest.to_string(),
            }
        } else if let Some(rest) = tag.strip_prefix("file:") {
            Self::File {
                path: rest.to_string(),
            }
        } else {
            Self::Manual
        }
    }
}

pub struct RetrievedMemory {
    pub id: String,
    pub content: String,
    pub score: f32,
    pub source: MemorySource,
}

pub async fn retrieve_context(
    store: &MemoryStore,
    embedder: &dyn EmbeddingProvider,
    query: &str,
    top_k: usize,
) -> Result<Vec<RetrievedMemory>, AppError> {
    let query_emb = embedder.embed(query).await?;
    let results = store.hybrid_search(&query_emb, query, top_k)?;
    let ids: Vec<String> = results.iter().map(|m| m.id.clone()).collect();
    let _ = store.touch_many(&ids);
    Ok(results)
}

pub fn format_context(memories: &[RetrievedMemory]) -> String {
    if memories.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Relevant context from memory\n\n");
    for (i, mem) in memories.iter().enumerate() {
        let _ = write!(
            out,
            "### [{} | score: {:.2}]\n{}\n\n",
            i + 1,
            mem.score,
            mem.content
        );
    }
    out
}
