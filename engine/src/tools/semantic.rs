use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::memory::{retrieve_context, EmbeddingProvider, MemoryStore};
use crate::tools::Tool;

/// Meaning-based search over the indexed codebase and prior memory. Complements the
/// lexical `search_files` (exact regex): the agent uses this to find WHERE behavior
/// lives when it doesn't know the exact symbol, then confirms with
/// `search_files` / `read_file`.
///
/// Backed by the same `MemoryStore` hybrid (vector + keyword) ranking that already
/// powers passive context injection, so it never needs a separate index.
pub struct SemanticSearch {
    store: MemoryStore,
    embedder: Arc<dyn EmbeddingProvider>,
    default_top_k: usize,
}

impl SemanticSearch {
    pub fn new(
        store: MemoryStore,
        embedder: Arc<dyn EmbeddingProvider>,
        default_top_k: usize,
    ) -> Self {
        Self {
            store,
            embedder,
            default_top_k: default_top_k.clamp(1, 25),
        }
    }
}

#[async_trait]
impl Tool for SemanticSearch {
    fn name(&self) -> &'static str {
        "semantic_search"
    }

    fn description(&self) -> &'static str {
        "Semantic (meaning-based) search over the indexed codebase and prior memory. \
         Give a natural-language query (e.g. 'where are login redirects handled?') and get \
         the most relevant code/notes via hybrid vector + keyword ranking. Use it to find \
         WHERE behavior lives when you don't know the exact symbol, then confirm with \
         search_files / read_file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language description of what you're looking for"
                },
                "top_k": {
                    "type": "integer",
                    "description": "Number of results to return (default 8, max 25)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("query is required".into()))?;
        let top_k = input["top_k"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(self.default_top_k)
            .clamp(1, 25);

        let results = retrieve_context(&self.store, self.embedder.as_ref(), query, top_k).await?;
        let matches: Vec<Value> = results
            .iter()
            .map(|m| {
                json!({
                    "source": m.source.to_tag(),
                    "score": m.score,
                    "content": m.content,
                })
            })
            .collect();

        Ok(json!({ "matches": matches, "count": matches.len() }))
    }
}
