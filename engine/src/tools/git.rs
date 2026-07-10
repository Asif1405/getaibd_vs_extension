use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;

use crate::error::AppError;
use crate::tools::{tmpfile, Tool};

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
        "Show file changes. Set staged=true for staged changes. Pass `ref` to diff the working \
         tree against a commit/branch/tag, or `base`+`head` to diff two refs (base..head)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean", "description": "Show staged changes" },
                "path": { "type": "string", "description": "Specific file to diff" },
                "ref": { "type": "string", "description": "Diff working tree against this commit/branch/tag" },
                "base": { "type": "string", "description": "Left side of a two-ref diff (base..head)" },
                "head": { "type": "string", "description": "Right side of a two-ref diff (base..head); defaults to HEAD" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Ok(self.diff_from_tracked_edits(input["path"].as_str()).await);
        }
        let staged = input["staged"].as_bool().unwrap_or(false);
        let mut args: Vec<String> = vec!["diff".into()];
        if staged {
            args.push("--cached".into());
        }
        if let Some(base) = input["base"].as_str() {
            let head = input["head"].as_str().unwrap_or("HEAD");
            args.push(format!("{base}..{head}"));
        } else if let Some(r) = input["ref"].as_str() {
            args.push(r.to_string());
        }
        if let Some(p) = input["path"].as_str() {
            args.push("--".into());
            args.push(p.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_git(&self.root, &arg_refs).await?;
        Ok(tmpfile::stash(&self.root, "git-diff", "diff", &output).await)
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
        "Show commit history. Filter by `path` (commits touching a file/dir) or `author`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "description": "Number of commits (default: 10)" },
                "oneline": { "type": "boolean", "description": "One-line format" },
                "path": { "type": "string", "description": "Only commits that touched this file or directory" },
                "author": { "type": "string", "description": "Only commits by this author (substring match)" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Ok(json!({ "log": "", "note": NOT_A_REPO }));
        }
        let count = input["count"].as_u64().unwrap_or(10);
        let oneline = input["oneline"].as_bool().unwrap_or(true);
        let mut args: Vec<String> = vec!["log".into(), format!("-{count}")];
        if oneline {
            args.push("--oneline".into());
        }
        if let Some(author) = input["author"].as_str() {
            args.push(format!("--author={author}"));
        }
        if let Some(path) = input["path"].as_str() {
            args.push("--".into());
            args.push(path.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_git(&self.root, &arg_refs).await?;
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

// ---------------------------------------------------------------------------
// Read-only enrichments: show / blame / branch
// ---------------------------------------------------------------------------

pub struct GitShow {
    root: Arc<PathBuf>,
}

impl GitShow {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitShow {
    fn name(&self) -> &'static str {
        "git_show"
    }

    fn description(&self) -> &'static str {
        "Inspect git history. With `ref` only: show a commit — message, author, date, full diff \
         (set stat=true for a changed-files summary); use for 'what changed in <sha>'. With \
         `path` set: fetch that file's FULL contents as of `ref` (a past or deleted version, \
         i.e. `git show <ref>:<path>`). Either way the payload is saved to a temp file you then \
         read with read_file / search_code — this is the correct way to view a historical file; \
         never run `git show <ref>:<file>` in a terminal and scrape the printed output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "description": "Commit SHA, branch, tag, or ref (default: HEAD)" },
                "path": { "type": "string", "description": "Fetch this file's contents as of `ref` (git show <ref>:<path>), saved to a temp file to read — for historical or deleted files" },
                "stat": { "type": "boolean", "description": "Show a changed-files summary instead of the full diff (commit mode only)" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let gref = input["ref"].as_str().unwrap_or("HEAD");
        // `path` set → show that file's *contents* as of the ref. Spill to a temp file the
        // agent reads with read_file, preserving the source extension for syntax/search —
        // never `git show <ref>:<path> | head` in a terminal and scrape the output.
        if let Some(path) = input["path"].as_str() {
            let spec = format!("{gref}:{path}");
            let output = run_git(&self.root, &["show", &spec]).await?;
            let ext = std::path::Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("txt");
            return Ok(tmpfile::stash(&self.root, "git-show-file", ext, &output).await);
        }
        let mut args: Vec<String> = vec!["show".into()];
        if input["stat"].as_bool().unwrap_or(false) {
            args.push("--stat".into());
        }
        args.push(gref.to_string());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_git(&self.root, &arg_refs).await?;
        Ok(tmpfile::stash(&self.root, "git-show", "diff", &output).await)
    }
}

pub struct GitBlame {
    root: Arc<PathBuf>,
}

impl GitBlame {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitBlame {
    fn name(&self) -> &'static str {
        "git_blame"
    }

    fn description(&self) -> &'static str {
        "Show line-by-line authorship for a file (which commit last changed each line). \
         Pass start/end to limit to a line range for large files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to blame" },
                "start": { "type": "integer", "description": "First line (1-based) of the range" },
                "end": { "type": "integer", "description": "Last line of the range" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let path = input["path"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("path is required".into()))?;
        let mut args: Vec<String> = vec!["blame".into(), "--date=short".into()];
        if let (Some(s), Some(e)) = (input["start"].as_u64(), input["end"].as_u64()) {
            args.push("-L".into());
            args.push(format!("{s},{e}"));
        }
        args.push("--".into());
        args.push(path.to_string());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_git(&self.root, &arg_refs).await?;
        Ok(tmpfile::stash(&self.root, "git-blame", "txt", &output).await)
    }
}

pub struct GitBranch {
    root: Arc<PathBuf>,
}

impl GitBranch {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitBranch {
    fn name(&self) -> &'static str {
        "git_branch"
    }

    fn description(&self) -> &'static str {
        "List branches with the current one marked and their upstream tracking. Set all=true \
         to include remote-tracking branches."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "all": { "type": "boolean", "description": "Include remote-tracking branches" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let mut args = vec!["branch", "-vv"];
        if input["all"].as_bool().unwrap_or(false) {
            args.push("--all");
        }
        let output = run_git(&self.root, &args).await?;
        let current = run_git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(json!({ "branches": output.trim(), "current": current }))
    }
}

pub struct GitCheckout {
    root: Arc<PathBuf>,
}

impl GitCheckout {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitCheckout {
    fn name(&self) -> &'static str {
        "git_checkout"
    }

    fn description(&self) -> &'static str {
        "Switch to an existing branch, or create a new one (create=true, optionally from a \
         base ref). Only when the user asked to change branches."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "branch": { "type": "string", "description": "Branch name to switch to or create" },
                "create": { "type": "boolean", "description": "Create the branch (git checkout -b)" },
                "from": { "type": "string", "description": "Base ref for a newly created branch" }
            },
            "required": ["branch"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        if !is_git_repo(&self.root).await {
            return Err(AppError::InvalidRequest(NOT_A_REPO.into()));
        }
        let branch = input["branch"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("branch is required".into()))?;
        let mut args: Vec<String> = vec!["checkout".into()];
        if input["create"].as_bool().unwrap_or(false) {
            args.push("-b".into());
        }
        args.push(branch.to_string());
        if let Some(from) = input["from"].as_str() {
            args.push(from.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_git(&self.root, &arg_refs).await?;
        Ok(json!({ "output": output.trim(), "branch": branch }))
    }
}

// ---------------------------------------------------------------------------
// GitHub pull-request ops via the authenticated `gh` CLI (works on private repos)
// ---------------------------------------------------------------------------

/// Resolve the `gh` binary, falling back to common install paths.
fn gh_bin() -> &'static str {
    for p in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"] {
        if std::path::Path::new(p).exists() {
            return p;
        }
    }
    "gh"
}

async fn run_gh(root: &PathBuf, args: &[&str]) -> Result<String, AppError> {
    let output = Command::new(gh_bin())
        .args(args)
        .current_dir(root)
        .output()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("failed to run gh (is it installed?): {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(AppError::InvalidRequest(format!("gh error: {}", stderr.trim())))
    }
}

pub struct GitPrList {
    root: Arc<PathBuf>,
}

impl GitPrList {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitPrList {
    fn name(&self) -> &'static str {
        "github_pr_list"
    }

    fn description(&self) -> &'static str {
        "List GitHub pull requests via the authenticated gh CLI (works on private repos). \
         Defaults to the current repo; pass `repo` (owner/name) for another. To read a single \
         PR's body and diff, use web_fetch with the PR URL instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "owner/name (default: current repo)" },
                "state": { "type": "string", "description": "open | closed | merged | all (default: open)" },
                "limit": { "type": "integer", "description": "Max PRs to list (default: 20)" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let state = input["state"].as_str().unwrap_or("open");
        let limit = input["limit"].as_u64().unwrap_or(20).to_string();
        let mut args = vec![
            "pr", "list", "--state", state, "--limit", &limit, "--json",
            "number,title,state,author,headRefName,baseRefName,url,isDraft",
        ];
        if let Some(repo) = input["repo"].as_str() {
            args.push("--repo");
            args.push(repo);
        }
        let output = run_gh(&self.root, &args).await?;
        Ok(json!({ "pull_requests": output.trim() }))
    }
}

pub struct GitPrCreate {
    root: Arc<PathBuf>,
}

impl GitPrCreate {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitPrCreate {
    fn name(&self) -> &'static str {
        "github_pr_create"
    }

    fn description(&self) -> &'static str {
        "Open a GitHub pull request for the current branch via the authenticated gh CLI. \
         Only when the user asked to open a PR. Push the branch first if it has no upstream."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "PR title" },
                "body": { "type": "string", "description": "PR description (markdown)" },
                "base": { "type": "string", "description": "Base branch to merge into (default: repo default)" },
                "draft": { "type": "boolean", "description": "Open as a draft PR" }
            },
            "required": ["title", "body"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let title = input["title"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("title is required".into()))?;
        let body = input["body"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("body is required".into()))?;
        let mut args: Vec<String> = vec![
            "pr".into(), "create".into(),
            "--title".into(), title.to_string(),
            "--body".into(), body.to_string(),
        ];
        if let Some(base) = input["base"].as_str() {
            args.push("--base".into());
            args.push(base.to_string());
        }
        if input["draft"].as_bool().unwrap_or(false) {
            args.push("--draft".into());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_gh(&self.root, &arg_refs).await?;
        Ok(json!({ "output": output.trim() }))
    }
}

pub struct GitPrCheckout {
    root: Arc<PathBuf>,
}

impl GitPrCheckout {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GitPrCheckout {
    fn name(&self) -> &'static str {
        "github_pr_checkout"
    }

    fn description(&self) -> &'static str {
        "Check out a GitHub pull request locally by number via the authenticated gh CLI, so you \
         can review or run it. Only when the user asked."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "PR number to check out" },
                "repo": { "type": "string", "description": "owner/name (default: current repo)" }
            },
            "required": ["number"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let number = input["number"]
            .as_u64()
            .ok_or_else(|| AppError::InvalidRequest("number is required".into()))?
            .to_string();
        let mut args = vec!["pr", "checkout", &number];
        if let Some(repo) = input["repo"].as_str() {
            args.push("--repo");
            args.push(repo);
        }
        let output = run_gh(&self.root, &args).await?;
        Ok(json!({ "output": output.trim() }))
    }
}