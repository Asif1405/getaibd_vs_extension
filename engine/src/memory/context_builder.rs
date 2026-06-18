use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::AppError;

use super::embeddings::EmbeddingProvider;
use super::project_graph::ProjectGraph;
use super::recent_tracker;
use super::store::MemoryStore;
use super::{retrieve_context, MemorySource, RetrievedMemory};

#[derive(Debug, Clone)]
pub struct ContextWindow {
    pub relevant_files: Vec<FileContext>,
    pub relevant_symbols: Vec<SymbolContext>,
    pub recent_edits: Vec<EditContext>,
    pub conversation_summary: Option<String>,
    pub codebase_facts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FileContext {
    pub path: String,
    pub content: String,
    pub relevance_score: f32,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SymbolContext {
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub definition: String,
    pub relevance_score: f32,
}

#[derive(Debug, Clone)]
pub struct EditContext {
    pub file_path: String,
    pub timestamp: i64,
    pub summary: String,
}

pub struct ContextBuilder<'a> {
    store: &'a MemoryStore,
    embedder: &'a dyn EmbeddingProvider,
    #[allow(dead_code)]
    project_root: PathBuf,
    max_context_chars: usize,
    min_relevance_score: f32,
    project_graph: Option<ProjectGraph>,
    recent_changes: Option<Vec<recent_tracker::RecentChange>>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(
        store: &'a MemoryStore,
        embedder: &'a dyn EmbeddingProvider,
        project_root: PathBuf,
    ) -> Self {
        Self {
            store,
            embedder,
            project_root,
            max_context_chars: 50_000,
            min_relevance_score: 0.3,
            project_graph: None,
            recent_changes: None,
        }
    }

    pub fn with_project_graph(mut self, graph: ProjectGraph) -> Self {
        self.project_graph = Some(graph);
        self
    }

    pub fn with_recent_changes(mut self, changes: Vec<recent_tracker::RecentChange>) -> Self {
        self.recent_changes = Some(changes);
        self
    }

    pub fn with_max_context_chars(mut self, max: usize) -> Self {
        self.max_context_chars = max;
        self
    }

    pub fn with_min_relevance_score(mut self, score: f32) -> Self {
        self.min_relevance_score = score;
        self
    }

    pub async fn build_context(
        &self,
        query: &str,
        current_files: &[String],
    ) -> Result<ContextWindow, AppError> {
        let mut window = ContextWindow {
            relevant_files: Vec::new(),
            relevant_symbols: Vec::new(),
            recent_edits: Vec::new(),
            conversation_summary: None,
            codebase_facts: Vec::new(),
        };

        let memories = retrieve_context(self.store, self.embedder, query, 20).await?;

        let relevant_memories = self.filter_by_relevance(&memories);
        let mut file_contexts = self.extract_file_contexts(&relevant_memories, current_files);
        let symbol_contexts = self.extract_symbol_contexts(&relevant_memories);

        self.boost_with_project_graph(&mut file_contexts, current_files);
        self.boost_with_recent_changes(&mut file_contexts);

        if let Some(changes) = &self.recent_changes {
            window.recent_edits = changes
                .iter()
                .take(10)
                .map(|c| EditContext {
                    file_path: c.file_path.clone(),
                    timestamp: c.timestamp,
                    summary: c.message.clone(),
                })
                .collect();
        }

        window.relevant_files = self.deduplicate_and_rank_files(file_contexts);
        window.relevant_symbols = self.deduplicate_and_rank_symbols(symbol_contexts);

        self.trim_to_context_limit(&mut window);

        Ok(window)
    }

    fn filter_by_relevance<'b>(&self, memories: &'b [RetrievedMemory]) -> Vec<&'b RetrievedMemory> {
        memories
            .iter()
            .filter(|m| m.score >= self.min_relevance_score)
            .collect()
    }

    fn extract_file_contexts(
        &self,
        memories: &[&RetrievedMemory],
        current_files: &[String],
    ) -> Vec<FileContext> {
        let mut file_map: HashMap<String, FileContext> = HashMap::new();

        for memory in memories {
            if let Some(file_path) = self.extract_file_path(&memory.source) {
                if current_files.contains(&file_path) {
                    continue;
                }

                file_map
                    .entry(file_path.clone())
                    .and_modify(|ctx| {
                        ctx.relevance_score = ctx.relevance_score.max(memory.score);
                        if ctx.content.len() < self.max_context_chars / 10 {
                            ctx.content.push_str("\n\n");
                            ctx.content.push_str(&memory.content);
                        }
                    })
                    .or_insert_with(|| FileContext {
                        path: file_path,
                        content: memory.content.clone(),
                        relevance_score: memory.score,
                        line_start: None,
                        line_end: None,
                    });
            }
        }

        file_map.into_values().collect()
    }

    fn extract_symbol_contexts(&self, memories: &[&RetrievedMemory]) -> Vec<SymbolContext> {
        let mut symbols = Vec::new();

        for memory in memories {
            if let Some((symbol_name, kind)) = self.extract_symbol_info(&memory.content) {
                if let Some(file_path) = self.extract_file_path(&memory.source) {
                    symbols.push(SymbolContext {
                        name: symbol_name,
                        kind,
                        file_path,
                        definition: memory.content.clone(),
                        relevance_score: memory.score,
                    });
                }
            }
        }

        symbols
    }

    fn boost_with_project_graph(
        &self,
        file_contexts: &mut [FileContext],
        current_files: &[String],
    ) {
        if let Some(graph) = &self.project_graph {
            for current_file in current_files {
                let related = graph.find_related_files(current_file, 2);

                for file_ctx in file_contexts.iter_mut() {
                    if related.contains(&file_ctx.path) {
                        file_ctx.relevance_score *= 1.5;
                    }

                    let dependencies = graph.get_file_dependencies(&file_ctx.path);
                    let dependents = graph.get_file_dependents(&file_ctx.path);

                    if dependencies.iter().any(|d| current_files.contains(d)) {
                        file_ctx.relevance_score *= 1.3;
                    }
                    if dependents.iter().any(|d| current_files.contains(d)) {
                        file_ctx.relevance_score *= 1.3;
                    }
                }
            }
        }
    }

    fn boost_with_recent_changes(&self, file_contexts: &mut [FileContext]) {
        if let Some(changes) = &self.recent_changes {
            let mut activity_map: HashMap<String, f32> = HashMap::new();

            for change in changes {
                let score = activity_map.entry(change.file_path.clone()).or_insert(0.0);
                *score += 1.0;
            }

            for file_ctx in file_contexts.iter_mut() {
                if let Some(activity_score) = activity_map.get(&file_ctx.path) {
                    file_ctx.relevance_score *= 1.0 + (activity_score * 0.2);
                }
            }
        }
    }

    fn extract_file_path(&self, source: &MemorySource) -> Option<String> {
        match source {
            MemorySource::File { path } => Some(path.clone()),
            _ => None,
        }
    }

    fn extract_symbol_info(&self, content: &str) -> Option<(String, String)> {
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return None;
        }

        let first_line = lines[0].trim();

        if first_line.starts_with("fn ") || first_line.starts_with("pub fn ") {
            if let Some(name) = first_line.split('(').next() {
                let name = name.replace("pub", "").replace("fn", "").trim().to_string();
                return Some((name, "function".to_string()));
            }
        }

        if first_line.starts_with("struct ") || first_line.starts_with("pub struct ") {
            if let Some(name) = first_line.split_whitespace().nth(2) {
                return Some((name.to_string(), "struct".to_string()));
            }
        }

        if first_line.starts_with("impl ") {
            if let Some(name) = first_line.split_whitespace().nth(1) {
                return Some((name.to_string(), "impl".to_string()));
            }
        }

        None
    }

    fn deduplicate_and_rank_files(&self, mut files: Vec<FileContext>) -> Vec<FileContext> {
        files.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut seen_paths = HashSet::new();
        files.retain(|f| seen_paths.insert(f.path.clone()));

        files
    }

    fn deduplicate_and_rank_symbols(&self, mut symbols: Vec<SymbolContext>) -> Vec<SymbolContext> {
        symbols.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut seen_symbols = HashSet::new();
        symbols.retain(|s| seen_symbols.insert(format!("{}:{}", s.file_path, s.name)));

        symbols
    }

    fn trim_to_context_limit(&self, window: &mut ContextWindow) {
        let mut total_chars = 0;

        window.relevant_files.retain(|f| {
            if total_chars + f.content.len() <= self.max_context_chars {
                total_chars += f.content.len();
                true
            } else {
                false
            }
        });

        let remaining_budget = self.max_context_chars.saturating_sub(total_chars);
        window.relevant_symbols.retain(|s| {
            if total_chars + s.definition.len() <= remaining_budget {
                total_chars += s.definition.len();
                true
            } else {
                false
            }
        });
    }

    pub fn format_context_window(&self, window: &ContextWindow) -> String {
        let mut output = String::new();

        if !window.codebase_facts.is_empty() {
            output.push_str("## Codebase Facts\n\n");
            for fact in &window.codebase_facts {
                output.push_str(&format!("- {}\n", fact));
            }
            output.push_str("\n");
        }

        if !window.relevant_files.is_empty() {
            output.push_str("## Relevant Files\n\n");
            for file in &window.relevant_files {
                output.push_str(&format!(
                    "### {} (relevance: {:.2})\n",
                    file.path, file.relevance_score
                ));
                output.push_str("```\n");
                output.push_str(&file.content);
                output.push_str("\n```\n\n");
            }
        }

        if !window.relevant_symbols.is_empty() {
            output.push_str("## Relevant Symbols\n\n");
            for symbol in &window.relevant_symbols {
                output.push_str(&format!(
                    "### {} ({}) from {}\n",
                    symbol.name, symbol.kind, symbol.file_path
                ));
                output.push_str("```\n");
                output.push_str(&symbol.definition);
                output.push_str("\n```\n\n");
            }
        }

        if let Some(summary) = &window.conversation_summary {
            output.push_str("## Conversation Summary\n\n");
            output.push_str(summary);
            output.push_str("\n\n");
        }

        output
    }
}

pub async fn build_smart_context(
    store: &MemoryStore,
    embedder: &dyn EmbeddingProvider,
    project_root: &Path,
    query: &str,
    current_files: &[String],
) -> Result<String, AppError> {
    let builder = ContextBuilder::new(store, embedder, project_root.to_path_buf())
        .with_max_context_chars(50_000)
        .with_min_relevance_score(0.3);

    let window = builder.build_context(query, current_files).await?;
    Ok(builder.format_context_window(&window))
}
