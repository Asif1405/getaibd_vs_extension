pub mod approval;
pub mod ask;
pub mod ask_gate;
pub mod command;
pub mod edits;
pub mod env_manager;
pub mod git;
pub mod terminal_gate;
pub mod workspace;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::AppError;
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

    pub fn build_default(project_root: &std::path::Path) -> Self {
        let mut registry = Self::new();
        let root = Arc::new(project_root.to_path_buf());
        let env_mgr = env_manager::EnvManager::new(root.clone());
        // Shared across the edit + diff tools so git_diff can fall back to the
        // session's tracked edits when the workspace isn't a git repo.
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
        registry.register(Arc::new(git::GitAdd::new(root.clone())));
        registry.register(Arc::new(git::GitCommit::new(root.clone())));
        registry.register(Arc::new(command::RunCommand::new(
            root.clone(),
            command::default_allowlist(),
            env_mgr.clone(),
        )));
        registry.register(Arc::new(env_manager::ManageEnv::new(env_mgr)));
        registry.register(Arc::new(ask::AskQuestion::new()));

        registry
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
