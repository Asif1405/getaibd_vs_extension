use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;

use crate::error::AppError;
use crate::tools::Tool;

async fn run_git(root: &PathBuf, args: &[&str]) -> Result<String, AppError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("git exec failed: {e}")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(AppError::InvalidRequest(format!("git error: {stderr}")))
    }
}

pub struct GitStatus {
    root: Arc<PathBuf>,
}

impl GitStatus {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &'static str {
        "git_status"
    }

    fn description(&self) -> &'static str {
        "Show git working tree status."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<Value, AppError> {
        let output = run_git(&self.root, &["status", "--porcelain"]).await?;
        Ok(json!({ "status": output.trim() }))
    }
}

pub struct GitDiff {
    root: Arc<PathBuf>,
}

impl GitDiff {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &'static str {
        "Show file changes. Set staged=true for staged changes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean", "description": "Show staged changes" },
                "path": { "type": "string", "description": "Specific file to diff" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let staged = input["staged"].as_bool().unwrap_or(false);
        let mut args = vec!["diff"];
        if staged {
            args.push("--cached");
        }
        let path_str;
        if let Some(p) = input["path"].as_str() {
            args.push("--");
            path_str = p.to_string();
            args.push(&path_str);
        }
        let output = run_git(&self.root, &args).await?;
        Ok(json!({ "diff": output }))
    }
}

pub struct GitLog {
    root: Arc<PathBuf>,
}

impl GitLog {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitLog {
    fn name(&self) -> &'static str {
        "git_log"
    }

    fn description(&self) -> &'static str {
        "Show commit history."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "description": "Number of commits (default: 10)" },
                "oneline": { "type": "boolean", "description": "One-line format" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let count = input["count"].as_u64().unwrap_or(10);
        let oneline = input["oneline"].as_bool().unwrap_or(true);
        let count_str = format!("-{count}");
        let mut args = vec!["log", &count_str];
        if oneline {
            args.push("--oneline");
        }
        let output = run_git(&self.root, &args).await?;
        Ok(json!({ "log": output.trim() }))
    }
}

pub struct GitAdd {
    root: Arc<PathBuf>,
}

impl GitAdd {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitAdd {
    fn name(&self) -> &'static str {
        "git_add"
    }

    fn description(&self) -> &'static str {
        "Stage files for commit."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files to stage (use [\".\"] for all)"
                }
            },
            "required": ["paths"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let paths = input["paths"]
            .as_array()
            .ok_or_else(|| AppError::InvalidRequest("paths array is required".into()))?;
        let mut args: Vec<String> = vec!["add".into()];
        for p in paths {
            if let Some(s) = p.as_str() {
                args.push(s.into());
            }
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&self.root, &arg_refs).await?;
        Ok(json!({ "staged": true }))
    }
}

pub struct GitCommit {
    root: Arc<PathBuf>,
}

impl GitCommit {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &'static str {
        "git_commit"
    }

    fn description(&self) -> &'static str {
        "Commit staged changes with a message."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "Commit message" }
            },
            "required": ["message"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let message = input["message"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("message is required".into()))?;
        let output = run_git(&self.root, &["commit", "-m", message]).await?;
        Ok(json!({ "output": output.trim() }))
    }
}
