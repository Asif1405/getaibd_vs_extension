use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

use crate::error::AppError;
use crate::tools::Tool;

use super::env_manager::EnvManager;

pub fn default_allowlist() -> HashSet<String> {
    [
        "cargo", "rustc", "npm", "npx", "bun", "bunx", "node", "python", "python3", "pip", "git",
        "ls", "cat", "grep", "rg", "find", "wc", "head", "tail", "sort", "uniq", "echo", "mkdir",
        "cp", "mv", "rm", "touch", "curl", "wget", "docker", "make", "cmake", "pip3", "pytest",
        "mypy", "ruff", "black", "poetry", "pipenv",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub struct RunCommand {
    root: Arc<PathBuf>,
    allowlist: HashSet<String>,
    env_mgr: EnvManager,
}

impl RunCommand {
    pub fn new(root: Arc<PathBuf>, allowlist: HashSet<String>, env_mgr: EnvManager) -> Self {
        Self {
            root,
            allowlist,
            env_mgr,
        }
    }
}

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command. Only allowlisted commands are permitted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command to execute" },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command arguments"
                },
                "cwd": { "type": "string", "description": "Working directory (relative to project root)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 30)" }
            },
            "required": ["command"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let program = input["command"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("command is required".into()))?;

        let base = program
            .split('/')
            .next_back()
            .unwrap_or(program)
            .split_whitespace()
            .next()
            .unwrap_or(program);

        if !self.allowlist.contains(base) {
            return Err(AppError::InvalidRequest(format!(
                "Command not allowed: {base}"
            )));
        }

        let args: Vec<String> = input["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let work_dir = input["cwd"]
            .as_str()
            .map_or_else(|| self.root.as_ref().clone(), |p| self.root.join(p));

        let timeout = Duration::from_secs(input["timeout_secs"].as_u64().unwrap_or(30));

        let mut cmd = Command::new(program);
        cmd.args(&args)
            .current_dir(&work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Inject project-env variables (VIRTUAL_ENV, PATH, etc.)
        self.env_mgr.prepare_command(&mut cmd, &work_dir).await;

        let child = cmd
            .spawn()
            .map_err(|e| AppError::InvalidRequest(format!("spawn failed: {e}")))?;

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| AppError::ProviderTimeout("command timed out".into()))?
            .map_err(|e| AppError::InvalidRequest(format!("exec failed: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        }))
    }
}
