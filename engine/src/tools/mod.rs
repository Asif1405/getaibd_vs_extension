pub mod approval;
pub mod ask;
pub mod ask_gate;
pub mod command;
pub mod editor_gate;
pub mod edits;
pub mod env_manager;
pub mod explore;
pub mod git;
pub mod lsp;
pub mod mcp_proxy;
pub mod patch_graph;
pub mod plan;
pub mod search_code;
pub mod semantic;
pub mod skill;
pub mod worktree;
pub mod terminal;
pub mod terminal_gate;
pub mod tmpfile;
pub mod web_fetch;
pub mod web_search;
pub mod workspace;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::error::AppError;
use crate::mcp::client;
use crate::models::ToolDefinition;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn execute(&self, input: Value) -> Result<Value, AppError>;

    fn requires_approval(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Subset of tools whose names pass `pred`.
    pub fn filter<F>(&self, mut pred: F) -> Self
    where
        F: FnMut(&str) -> bool,
    {
        let mut registry = Self::new();
        for (name, tool) in &self.tools {
            if pred(name) {
                registry.register(Arc::clone(tool));
            }
        }
        registry
    }

    /// Built-in tools for MCP HTTP/stdio and non-agent routes (no external MCP servers).
    pub fn build_default(project_root: &Path) -> Self {
        let mut registry = Self::new();
        Self::register_builtin(&mut registry, project_root);
        registry
    }

    /// Register session tools that need runtime `AppState` (codebase semantic search and
    /// web search). Shared by every agent route so the basic and orchestrated paths expose
    /// the same toolset. Each tool self-gates on the state it needs being configured.
    pub fn register_session_state_tools(&mut self, state: &crate::state::AppState) {
        if let (Some(store), Some(embedder)) = (&state.memory_store, &state.embedder) {
            self.register(Arc::new(semantic::SemanticSearch::new(
                store.clone(),
                embedder.clone(),
                state.memory_top_k,
            )));
        }
        // Unified search router — grep always works; the semantic path lights up
        // only when memory/embeddings are configured.
        let memory = Self::memory_handles(state);
        self.register(Arc::new(search_code::SearchCode::new(
            Arc::new(state.project_root.clone()),
            memory,
            state.memory_top_k,
        )));
        if let (Some(key), Some(base)) = (&state.getaibd_api_key, &state.getaibd_base_url) {
            self.register(Arc::new(web_search::WebSearch::new(base.clone(), key.clone())));
        }
    }

    /// Owned `(store, embedder)` handles when memory is configured, for tools that
    /// need their own copies (search router, explore subagent).
    fn memory_handles(
        state: &crate::state::AppState,
    ) -> Option<(
        crate::memory::MemoryStore,
        Arc<dyn crate::memory::EmbeddingProvider>,
    )> {
        match (&state.memory_store, &state.embedder) {
            (Some(s), Some(e)) => Some((s.clone(), e.clone())),
            _ => None,
        }
    }

    /// Register the `explore` subagent tool. It runs a bounded, read-only sub-run
    /// against its own filtered registry (no mutations, no MCP, no `explore` itself,
    /// so it can never recurse). Requires the request's provider + model.
    pub fn register_explore_tool(
        &mut self,
        state: &crate::state::AppState,
        provider: Arc<dyn crate::providers::Provider>,
        model: String,
    ) {
        let mut sub = Self::build_default(&state.project_root);
        sub.register_session_state_tools(state);
        let sub = sub.filter(|name| explore::EXPLORE_TOOLS.contains(&name));
        self.register(Arc::new(explore::ExploreCodebase::new(
            provider,
            model,
            state.project_root.clone(),
            Arc::new(sub),
            Self::memory_handles(state),
            14,
        )));
    }

    /// Agent session registry: built-ins + optional MCP servers from `.getaibd/mcp.json`.
    pub async fn build_for_session(project_root: &Path) -> Self {
        let mut registry = Self::build_default(project_root);
        for server in client::connect_all(project_root).await {
            let server = Arc::new(server);
            for tool in &server.tools {
                registry.register(Arc::new(mcp_proxy::McpProxyTool::new(
                    server.clone(),
                    tool.name.clone(),
                    tool.description.clone(),
                    tool.input_schema.clone(),
                )));
            }
        }
        registry
    }

    fn register_builtin(registry: &mut Self, project_root: &Path) {
        let root = Arc::new(project_root.to_path_buf());
        let env_mgr = env_manager::EnvManager::new(root.clone());
        let edits = edits::EditTracker::new();

        registry.register(Arc::new(workspace::ReadFile::new(root.clone())));
        registry.register(Arc::new(workspace::WriteFile::new(root.clone(), edits.clone())));
        registry.register(Arc::new(workspace::PatchFile::new(root.clone(), edits.clone())));
        registry.register(Arc::new(workspace::ListDirectory::new(root.clone())));
        registry.register(Arc::new(workspace::SearchFiles::new(root.clone())));
        registry.register(Arc::new(workspace::MoveFile::new(root.clone())));
        registry.register(Arc::new(workspace::DeleteFile::new(root.clone())));
        registry.register(Arc::new(git::GitStatus::new(root.clone())));
        registry.register(Arc::new(git::GitDiff::new(root.clone(), edits.clone())));
        registry.register(Arc::new(git::GitLog::new(root.clone())));
        registry.register(Arc::new(git::GitShow::new(root.clone())));
        registry.register(Arc::new(git::GitBlame::new(root.clone())));
        registry.register(Arc::new(git::GitBranch::new(root.clone())));
        registry.register(Arc::new(git::GitAdd::new(root.clone())));
        registry.register(Arc::new(git::GitCommit::new(root.clone())));
        registry.register(Arc::new(git::GitReset::new(root.clone())));
        registry.register(Arc::new(git::GitCheckout::new(root.clone())));
        registry.register(Arc::new(git::GitPush::new(root.clone())));
        registry.register(Arc::new(git::GitPrList::new(root.clone())));
        registry.register(Arc::new(git::GitPrCreate::new(root.clone())));
        registry.register(Arc::new(git::GitPrCheckout::new(root.clone())));
        registry.register(Arc::new(command::RunCommand::new(
            root.clone(),
            env_mgr.clone(),
        )));
        registry.register(Arc::new(env_manager::ManageEnv::new(env_mgr)));
        registry.register(Arc::new(ask::AskQuestion::new()));
        registry.register(Arc::new(plan::UpdatePlan::new()));
        registry.register(Arc::new(plan::WritePlan::new()));
        registry.register(Arc::new(plan::AttemptCompletion::new()));
        registry.register(Arc::new(skill::FetchSkill::new(root.clone())));
        registry.register(Arc::new(terminal::ReadTerminal::new()));
        registry.register(Arc::new(lsp::FindSymbol::new(root.clone())));
        registry.register(Arc::new(lsp::FindReferences::new(root.clone())));
        registry.register(Arc::new(lsp::DocumentSymbols::new(root.clone())));
        registry.register(Arc::new(patch_graph::PatchGraph::new(root.clone())));
        registry.register(Arc::new(worktree::WorktreeList::new(root.clone())));
        registry.register(Arc::new(worktree::WorktreeCreate::new(root.clone())));
        registry.register(Arc::new(worktree::WorktreeRemove::new(root.clone())));
        registry.register(Arc::new(web_fetch::WebFetch::new(root.clone())));
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
