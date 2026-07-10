//! Git worktree tools (`worktree_list`, `worktree_create`, `worktree_remove`).
//!
//! Worktrees let the agent work in an isolated checkout — installs, builds, and
//! risky refactors stay off the user's main branch. `worktree_create` also runs
//! the setup commands from `.getaibd/worktrees.json` (checked in the new worktree
//! first, then the project root), mirroring Cursor's `.cursor/worktrees.json`.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;

use crate::error::AppError;
use crate::tools::Tool;

/// Run `git <args>` in `cwd`, returning (stdout, stderr, exit_code).
async fn git(cwd: &Path, args: &[&str]) -> Result<(String, String, i32), AppError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("git spawn failed: {e}")))?;
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    ))
}

/// Filesystem-safe slug for a branch name (`feature/x` -> `feature-x`).
fn slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

// ---------- worktree_list ----------

pub struct WorktreeList {
    root: Arc<PathBuf>,
}

impl WorktreeList {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for WorktreeList {
    fn name(&self) -> &'static str {
        "worktree_list"
    }

    fn description(&self) -> &'static str {
        "List all git worktrees for this repository (path, branch, HEAD). Use before \
         creating or removing a worktree."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<Value, AppError> {
        let (stdout, stderr, code) =
            git(self.root.as_ref(), &["worktree", "list", "--porcelain"]).await?;
        if code != 0 {
            return Ok(json!({ "error": stderr.trim(), "worktrees": [] }));
        }
        let mut worktrees: Vec<Value> = Vec::new();
        let mut cur = json!({});
        for line in stdout.lines() {
            if line.is_empty() {
                if cur.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                    worktrees.push(std::mem::replace(&mut cur, json!({})));
                }
                continue;
            }
            if let Some(p) = line.strip_prefix("worktree ") {
                cur["path"] = json!(p);
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                cur["head"] = json!(h);
            } else if let Some(b) = line.strip_prefix("branch ") {
                cur["branch"] = json!(b.strip_prefix("refs/heads/").unwrap_or(b));
            } else if line == "detached" {
                cur["branch"] = json!("(detached)");
            }
        }
        if cur.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            worktrees.push(cur);
        }
        Ok(json!({ "count": worktrees.len(), "worktrees": worktrees }))
    }
}

// ---------- worktree_create ----------

pub struct WorktreeCreate {
    root: Arc<PathBuf>,
}

impl WorktreeCreate {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for WorktreeCreate {
    fn name(&self) -> &'static str {
        "worktree_create"
    }

    fn description(&self) -> &'static str {
        "Create an isolated git worktree on a new (or existing) branch, then run the \
         setup commands from .getaibd/worktrees.json inside it. Returns the worktree \
         path so you can run/build/test there without touching the main checkout."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "branch": { "type": "string", "description": "Branch to check out in the worktree (created if it doesn't exist)." },
                "path": { "type": "string", "description": "Optional worktree location. Default: a sibling '<repo>-worktrees/<branch>' directory." },
                "base": { "type": "string", "description": "Optional base ref for a new branch (default: current HEAD)." }
            },
            "required": ["branch"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let branch = input["branch"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::InvalidRequest("branch is required".into()))?
            .to_string();

        let path: PathBuf = match input["path"].as_str().filter(|s| !s.is_empty()) {
            Some(p) => {
                let pb = PathBuf::from(p);
                if pb.is_absolute() { pb } else { self.root.join(pb) }
            }
            None => {
                let parent = self.root.parent().unwrap_or(self.root.as_ref());
                let name = self
                    .root
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".into());
                parent.join(format!("{name}-worktrees")).join(slug(&branch))
            }
        };
        let path_str = path.to_string_lossy().to_string();

        // Does the branch already exist?
        let (_, _, exists_code) =
            git(self.root.as_ref(), &["rev-parse", "--verify", &format!("refs/heads/{branch}")]).await?;
        let branch_exists = exists_code == 0;

        let base = input["base"].as_str().filter(|s| !s.is_empty());
        let mut args: Vec<&str> = vec!["worktree", "add"];
        if !branch_exists {
            args.push("-b");
            args.push(&branch);
            args.push(&path_str);
            if let Some(b) = base {
                args.push(b);
            }
        } else {
            args.push(&path_str);
            args.push(&branch);
        }

        let (_stdout, stderr, code) = git(self.root.as_ref(), &args).await?;
        if code != 0 {
            return Err(AppError::InvalidRequest(format!(
                "git worktree add failed: {}",
                stderr.trim()
            )));
        }

        let setup = run_worktree_setup(self.root.as_ref(), &path).await;

        Ok(json!({
            "path": path_str,
            "branch": branch,
            "created_branch": !branch_exists,
            "setup": setup,
        }))
    }
}

// ---------- worktree_remove ----------

pub struct WorktreeRemove {
    root: Arc<PathBuf>,
}

impl WorktreeRemove {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for WorktreeRemove {
    fn name(&self) -> &'static str {
        "worktree_remove"
    }

    fn description(&self) -> &'static str {
        "Remove a git worktree created earlier. Use `force: true` to discard \
         uncommitted changes in that worktree."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Worktree path to remove (from worktree_list)." },
                "force": { "type": "boolean", "description": "Discard uncommitted changes (default false)." }
            },
            "required": ["path"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let path = input["path"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::InvalidRequest("path is required".into()))?;
        let force = input["force"].as_bool().unwrap_or(false);

        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path);
        let (_stdout, stderr, code) = git(self.root.as_ref(), &args).await?;
        if code != 0 {
            return Err(AppError::InvalidRequest(format!(
                "git worktree remove failed: {}",
                stderr.trim()
            )));
        }
        Ok(json!({ "removed": path }))
    }
}

// ---------- worktrees.json setup ----------

#[derive(Debug, Default, serde::Deserialize)]
struct WorktreesCfg {
    #[serde(default, rename = "setup-worktree")]
    setup: Option<Value>,
    #[serde(default, rename = "setup-worktree-unix")]
    setup_unix: Option<Value>,
    #[serde(default, rename = "setup-worktree-windows")]
    setup_windows: Option<Value>,
}

fn read_worktrees_cfg(dir: &Path) -> Option<WorktreesCfg> {
    let text = std::fs::read_to_string(dir.join(".getaibd").join("worktrees.json")).ok()?;
    serde_json::from_str::<WorktreesCfg>(&text).ok()
}

/// Turn the OS-specific `setup-worktree*` value (array of commands OR a script path)
/// into a list of shell command strings to run in the worktree.
fn resolve_setup_commands(cfg: &WorktreesCfg, worktree: &Path) -> Vec<String> {
    let os_specific = if cfg!(target_os = "windows") {
        cfg.setup_windows.as_ref()
    } else {
        cfg.setup_unix.as_ref()
    };
    let value = os_specific.or(cfg.setup.as_ref());
    match value {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(script)) => {
            // A relative script path lives next to worktrees.json (.getaibd/).
            let p = PathBuf::from(script);
            let full = if p.is_absolute() { p } else { worktree.join(".getaibd").join(script) };
            let s = full.to_string_lossy();
            if cfg!(target_os = "windows") {
                vec![format!("powershell -File \"{s}\"")]
            } else {
                vec![format!("sh \"{s}\"")]
            }
        }
        _ => Vec::new(),
    }
}

async fn run_worktree_setup(root: &Path, worktree: &Path) -> Vec<Value> {
    // Worktree-local config wins over the project root's, matching Cursor.
    let cfg = read_worktrees_cfg(worktree).or_else(|| read_worktrees_cfg(root));
    let Some(cfg) = cfg else {
        return Vec::new();
    };
    let commands = resolve_setup_commands(&cfg, worktree);
    let mut results = Vec::new();
    for cmd in commands {
        let (shell, flag) = if cfg!(target_os = "windows") {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let out = Command::new(shell)
            .arg(flag)
            .arg(&cmd)
            .current_dir(worktree)
            .env("ROOT_WORKTREE_PATH", root.to_string_lossy().to_string())
            .env("GETAIBD_AGENT", "1")
            .output()
            .await;
        match out {
            Ok(o) => results.push(json!({
                "command": cmd,
                "exit_code": o.status.code().unwrap_or(-1),
                "stderr": String::from_utf8_lossy(&o.stderr).chars().take(500).collect::<String>(),
            })),
            Err(e) => results.push(json!({ "command": cmd, "error": e.to_string() })),
        }
    }
    results
}
