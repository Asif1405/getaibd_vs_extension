//! `explore` — a read-only subagent that investigates the codebase in its own
//! isolated context and returns only a concise summary. It runs the normal agent
//! tool-loop against a fresh `Session` with a discovery-only toolset, so it can
//! chain many searches/reads without bloating the parent conversation. Its own
//! transcript is discarded; only the final summary flows back to the caller.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::runtime::{run_agent_with_memory, AgentOptions, MemoryContext};
use crate::agent::session::Session;
use crate::agent::thinking;
use crate::error::AppError;
use crate::memory::{EmbeddingProvider, MemoryStore};
use crate::providers::Provider;
use crate::tools::{Tool, ToolRegistry};

/// Read-only discovery tools the explore subagent may use. Deliberately excludes
/// all mutations (write/patch/delete/command) and `explore` itself (no recursion).
pub const EXPLORE_TOOLS: &[&str] = &[
    "read_file",
    "list_directory",
    "search_files",
    "search_code",
    "semantic_search",
    "find_symbol",
    "find_references",
    "document_symbols",
    "patch_graph",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_blame",
    "git_branch",
    "github_pr_list",
    "web_search",
    "web_fetch",
];

const EXPLORE_SYSTEM_PROMPT: &str = "You are an Explore subagent: a fast, read-only \
codebase investigator. Your ONLY job is to answer the given question by searching and \
reading the code, then return a tight summary for another agent to act on.\n\n\
Rules:\n\
- You are strictly read-only. Never write, edit, delete, or run commands.\n\
- Prefer search_code / semantic_search / find_symbol / find_references to locate \
things, then read_file only the relevant ranges. Don't dump whole files.\n\
- Be efficient: a handful of targeted searches, not an exhaustive crawl.\n\
- Finish with a concise summary (bullet points) grounded in concrete evidence: cite \
`path:line` for every claim. State clearly if something was not found.\n\
- Do not ask questions; investigate and report.";

pub struct ExploreCodebase {
    provider: Arc<dyn Provider>,
    model: String,
    project_root: PathBuf,
    /// Read-only registry (EXPLORE_TOOLS subset, no `explore` tool) for the sub-run.
    registry: Arc<ToolRegistry>,
    memory: Option<(MemoryStore, Arc<dyn EmbeddingProvider>)>,
    max_iterations: u32,
}

impl ExploreCodebase {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        project_root: PathBuf,
        registry: Arc<ToolRegistry>,
        memory: Option<(MemoryStore, Arc<dyn EmbeddingProvider>)>,
        max_iterations: u32,
    ) -> Self {
        Self {
            provider,
            model,
            project_root,
            registry,
            memory,
            max_iterations,
        }
    }
}

#[async_trait]
impl Tool for ExploreCodebase {
    fn name(&self) -> &'static str {
        "explore"
    }

    fn description(&self) -> &'static str {
        "Spawn a read-only Explore subagent that investigates the codebase in its own \
         context and returns a concise, evidence-cited summary (with path:line refs). Use \
         it for broad 'where/how does X work?' questions so your own context stays focused \
         — it runs many searches/reads for you and reports back only the findings. It \
         cannot modify files or run commands."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to investigate, in natural language (e.g. 'how are auth redirects handled?')."
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

        let mut session = Session::new(
            self.provider.id(),
            self.model.clone(),
            self.project_root.clone(),
        )
        .with_system_prompt(EXPLORE_SYSTEM_PROMPT)
        .with_max_iterations(self.max_iterations);

        let options = AgentOptions {
            enable_thinking: thinking::model_uses_reasoning(&self.model),
            // Explore answers and yields — it must not loop to "task completion".
            auto_complete: false,
            tool_timeout_secs: 120,
            ..Default::default()
        };

        let mem_ctx = self.memory.as_ref().map(|(store, embedder)| MemoryContext {
            store,
            embedder: embedder.as_ref(),
            top_k: 10,
            max_entries: 50,
            analysis_cache: None,
        });

        // The subagent's events (tool calls, tokens) stay inside this run; only the
        // final summary is returned, keeping the parent conversation lean.
        let mut sink = |_ev: crate::agent::runtime::AgentEvent| {};
        let result = run_agent_with_memory(
            &mut session,
            &query,
            &self.provider,
            self.registry.as_ref(),
            mem_ctx.as_ref(),
            Some(&options),
            &mut sink,
        )
        .await?;

        Ok(json!({
            "summary": result.final_response,
            "iterations": result.iterations,
        }))
    }
}
