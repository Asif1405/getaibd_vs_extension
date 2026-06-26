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

/// True when `root` is inside a git work tree. Used so the git tools can degrade
/// gracefully (instead of surfacing a raw "Not a git repository" error) when the
/// workspace isn't version-controlled.
async fn is_git_repo(root: &PathBuf) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const NOT_A_REPO: &str = "This folder isn't a git repository. Run `git init` first to enable version control.";

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
        "Show git working tree status with branch, staged, unstaged, and untracked file lists."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Ok(json!({
                "status": "",
                "branch": "",
                "staged": [],
                "unstaged": [],
                "untracked": [],
                "note": NOT_A_REPO
            }));
        }
        let branch = run_git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        let output = run_git(&self.root, &["status", "--porcelain"]).await?;
        let mut staged = Vec::new();
        let mut unstaged = Vec::new();
        let mut untracked = Vec::new();
        for line in output.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let path = line.get(3..).unwrap_or(line).trim();
            if line.starts_with("??") {
                untracked.push(path.to_string());
            } else {
                let x = line.as_bytes().first().copied().unwrap_or(b' ');
                let y = line.as_bytes().get(1).copied().unwrap_or(b' ');
                if x != b' ' {
                    staged.push(path.to_string());
                }
                if y != b' ' {
                    unstaged.push(path.to_string());
                }
            }
        }
        Ok(json!({
            "status": output.trim(),
            "branch": branch,
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked,
        }))
    }
}

pub struct GitDiff {
    root: Arc<PathBuf>,
    edits: crate::tools::edits::EditTracker,
}

impl GitDiff {
    pub fn new(root: Arc<PathBuf>, edits: crate::tools::edits::EditTracker) -> Self {
        Self { root, edits }
    }

    /// Diff from the session's tracked edits, for workspaces without git.
    async fn diff_from_tracked_edits(&self, filter: Option<&str>) -> Value {
        let mut combined = String::new();
        for (path, baseline) in self.edits.snapshot() {
            if filter.is_some_and(|f| f != path) {
                continue;
            }
            let current = tokio::fs::read_to_string(self.root.join(&path))
                .await
                .unwrap_or_default();
            if current == baseline {
                continue;
            }
            if let Some(d) = unified_diff(&path, &baseline, &current) {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&d);
            }
        }
        if combined.is_empty() {
            json!({
                "diff": "",
                "note": format!("{NOT_A_REPO} No edits have been made this session yet.")
            })
        } else {
            json!({
                "diff": combined.trim_end(),
                "note": "Not a git repository — showing edits made this session."
            })
        }
    }
}

/// Builds a unified diff between two buffers using libgit2 (no repo required).
fn unified_diff(path: &str, old: &str, new: &str) -> Option<String> {
    let as_path = std::path::Path::new(path);
    let mut patch = git2::Patch::from_buffers(
        old.as_bytes(),
        Some(as_path),
        new.as_bytes(),
        Some(as_path),
        None,
    )
    .ok()?;
    let buf = patch.to_buf().ok()?;
    let text = buf.as_str()?;
    if text.is_empty() {
        return None;
    }
    Some(format!("diff --git a/{path} b/{path}\n{text}"))
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
        if !is_git_repo(&self.root).await {
            return Ok(self.diff_from_tracked_edits(input["path"].as_str()).await);
        }
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
        if !is_git_repo(&self.root).await {
            return Ok(json!({ "log": "", "note": NOT_A_REPO }));
        }
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
        "Stage specific files for commit. List explicit paths only — do NOT use [\".\"] or \
         [\"-A\"] unless the user asked to stage everything. For \"commit staged\" tasks, skip \
         staging and use git_commit directly."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Specific file paths to stage (never use \".\" unless user asked to stage all)"
                }
            },
            "required": ["paths"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
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
        "Commit already-staged changes only. Does not stage new files — use git_add first only \
         when the user asked to stage specific paths."
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
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let message = input["message"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("message is required".into()))?;
        let output = run_git(&self.root, &["commit", "-m", message]).await?;
        Ok(json!({ "output": output.trim() }))
    }
}

pub struct GitReset {
    root: Arc<PathBuf>,
}

impl GitReset {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitReset {
    fn name(&self) -> &'static str {
        "git_reset"
    }

    fn description(&self) -> &'static str {
        "Unstage files (git reset HEAD). Use to undo staging when the user wants only staged \
         files committed or rejected a broad git add."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files to unstage; omit or use [\".\"] to unstage all"
                }
            }
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let paths = input.get("paths").and_then(|p| p.as_array());
        let mut args: Vec<String> = vec!["reset".into(), "HEAD".into()];
        if let Some(paths) = paths {
            if !paths.is_empty() {
                args.push("--".into());
                for p in paths {
                    if let Some(s) = p.as_str() {
                        args.push(s.into());
                    }
                }
            }
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&self.root, &arg_refs).await?;
        Ok(json!({ "unstaged": true }))
    }
}

pub struct GitPush {
    root: Arc<PathBuf>,
}

impl GitPush {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitPush {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn description(&self) -> &'static str {
        "Push committed changes to the remote. Only when the user asked to push."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "remote": { "type": "string", "description": "Remote name (default: origin)" },
                "branch": { "type": "string", "description": "Branch to push (default: current)" },
                "set_upstream": { "type": "boolean", "description": "Use -u on first push" }
            }
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let remote = input["remote"].as_str().unwrap_or("origin");
        let set_upstream = input["set_upstream"].as_bool().unwrap_or(false);
        let mut args = vec!["push"];
        if set_upstream {
            args.push("-u");
        }
        args.push(remote);
        if let Some(branch) = input["branch"].as_str() {
            args.push(branch);
        }
        let output = run_git(&self.root, &args).await?;
        Ok(json!({ "output": output.trim() }))
    }
}