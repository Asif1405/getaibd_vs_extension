pub mod approval;
pub mod ask;
pub mod ask_gate;
pub mod command;
pub mod edits;
pub mod env_manager;
pub mod git;
pub mod mcp_proxy;
pub mod plan;
pub mod semantic;
pub mod skill;
pub mod terminal;
pub mod terminal_gate;
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
        registry.register(Arc::new(command::RunCommand::new(
            root.clone(),
            command::default_allowlist(),
            env_mgr.clone(),
        )));
        registry.register(Arc::new(env_manager::ManageEnv::new(env_mgr)));
        registry.register(Arc::new(ask::AskQuestion::new()));
        registry.register(Arc::new(plan::UpdatePlan::new()));
        registry.register(Arc::new(skill::FetchSkill::new(root.clone())));
        registry.register(Arc::new(terminal::ReadTerminal::new()));
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
