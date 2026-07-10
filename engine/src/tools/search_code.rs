//! `search_code` — a deterministic search router that mirrors how modern coding
//! agents pick a strategy: a concrete symbol/string routes to grep (or a
//! definition lookup), a natural-language concept routes to semantic search. It
//! falls back to the complementary strategy when the primary finds nothing, so
//! one call reliably surfaces the relevant code regardless of how the user
//! phrased the query.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::AppError;
use crate::memory::{retrieve_context, EmbeddingProvider, MemoryStore};
use crate::tools::lsp::{regex_escape, ripgrep};
use crate::tools::Tool;

#[derive(Clone, Copy, PartialEq)]
enum Strategy {
    Grep,
    Symbol,
    Semantic,
}

impl Strategy {
    fn as_str(self) -> &'static str {
        match self {
            Strategy::Grep => "grep",
            Strategy::Symbol => "symbol",
            Strategy::Semantic => "semantic",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "grep" | "text" | "exact" => Some(Strategy::Grep),
            "symbol" | "definition" | "def" => Some(Strategy::Symbol),
            "semantic" | "meaning" | "concept" => Some(Strategy::Semantic),
            _ => None,
        }
    }
}

/// True when `s` contains a lowercase→uppercase boundary (camelCase / PascalCase),
/// a strong signal the query names a code identifier rather than prose.
fn has_camel_case(s: &str) -> bool {
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            return true;
        }
        prev_lower = c.is_ascii_lowercase();
    }
    false
}

/// Route a query: a bare identifier → definition lookup; a short code-ish token →
/// grep; a multi-word phrase → semantic. Deterministic so behaviour is predictable.
fn classify(query: &str) -> Strategy {
    let q = query.trim();
    let words = q.split_whitespace().count();
    let codeish = q.contains("::")
        || q.contains('(')
        || q.contains('_')
        || q.contains('/')
        || q.contains('.')
        || q.chars().any(|c| "{}[]<>|$^\\*".contains(c))
        || has_camel_case(q);

    if words <= 1 {
        if !q.is_empty() && q.chars().all(|c| c.is_alphanumeric() || c == '_') {
            Strategy::Symbol
        } else {
            Strategy::Grep
        }
    } else if words <= 3 && codeish {
        Strategy::Grep
    } else {
        Strategy::Semantic
    }
}

pub struct SearchCode {
    root: Arc<PathBuf>,
    /// Present only when memory/embeddings are configured; enables the semantic path.
    memory: Option<(MemoryStore, Arc<dyn EmbeddingProvider>)>,
    default_top_k: usize,
}

impl SearchCode {
    pub fn new(
        root: Arc<PathBuf>,
        memory: Option<(MemoryStore, Arc<dyn EmbeddingProvider>)>,
        default_top_k: usize,
    ) -> Self {
        Self {
            root,
            memory,
            default_top_k: default_top_k.clamp(1, 25),
        }
    }

    async fn grep(&self, query: &str, symbol: bool, max: usize) -> Vec<Value> {
        let pattern = if symbol {
            let kws = "fn|def|func|function|class|struct|enum|trait|interface|type|impl|const|var|let|module|mod|package|record";
            format!(r"\b(?:{kws})\s+{}\b", regex_escape(query))
        } else {
            regex_escape(query)
        };
        ripgrep(&self.root, &pattern)
            .await
            .into_iter()
            .take(max)
            .map(|(path, line, text)| json!({ "path": path, "line": line, "text": text }))
            .collect()
    }

    async fn semantic(&self, query: &str, top_k: usize) -> Result<Vec<Value>, AppError> {
        let Some((store, embedder)) = &self.memory else {
            return Ok(Vec::new());
        };
        let results = retrieve_context(store, embedder.as_ref(), query, top_k).await?;
        Ok(results
            .iter()
            .map(|m| json!({ "source": m.source.to_tag(), "score": m.score, "content": m.content }))
            .collect())
    }
}

#[async_trait]
impl Tool for SearchCode {
    fn name(&self) -> &'static str {
        "search_code"
    }

    fn description(&self) -> &'static str {
        "Unified code search that auto-routes to the best strategy: exact text/regex \
         (grep), definition lookup (symbol), or meaning-based (semantic) search. Pass a \
         natural-language question to find WHERE behaviour lives, or a concrete symbol/\
         string to find exact matches — one call handles both and falls back \
         automatically. Set `strategy` to force grep|symbol|semantic."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A symbol/string for exact search, or a natural-language description for concept search."
                },
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "grep", "symbol", "semantic"],
                    "description": "Force a strategy. Default 'auto' picks based on the query."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max matches to return (default 20, max 100)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("query is required".into()))?
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(AppError::InvalidRequest("query is required".into()));
        }
        let max = input["max_results"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(20)
            .clamp(1, 100);
        let top_k = self.default_top_k;

        let mut strategy = input["strategy"]
            .as_str()
            .and_then(Strategy::parse)
            .unwrap_or_else(|| classify(&query));
        // Semantic needs an index; degrade to grep when memory is off.
        if strategy == Strategy::Semantic && self.memory.is_none() {
            strategy = Strategy::Grep;
        }

        let (matches, used) = match strategy {
            Strategy::Semantic => {
                let m = self.semantic(&query, top_k).await?;
                if m.is_empty() {
                    (self.grep(&query, false, max).await, Strategy::Grep)
                } else {
                    (m, Strategy::Semantic)
                }
            }
            Strategy::Symbol => {
                let defs = self.grep(&query, true, max).await;
                if !defs.is_empty() {
                    (defs, Strategy::Symbol)
                } else {
                    let g = self.grep(&query, false, max).await;
                    if g.is_empty() && self.memory.is_some() {
                        (self.semantic(&query, top_k).await?, Strategy::Semantic)
                    } else {
                        (g, Strategy::Grep)
                    }
                }
            }
            Strategy::Grep => {
                let g = self.grep(&query, false, max).await;
                if g.is_empty() && self.memory.is_some() {
                    (self.semantic(&query, top_k).await?, Strategy::Semantic)
                } else {
                    (g, Strategy::Grep)
                }
            }
        };

        let count = matches.len();
        Ok(json!({ "strategy": used.as_str(), "matches": matches, "count": count }))
    }
}
