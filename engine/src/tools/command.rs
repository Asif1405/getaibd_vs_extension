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

/// Legacy config helper only. The command tool no longer enforces an allowlist:
/// the *user* decides what may run via the approval prompt (dangerous programs are
/// flagged and always re-prompt even when `run_command` is set to "always allow").
/// Kept so existing `command_allowlist` config keys still deserialize.
pub fn default_allowlist() -> HashSet<String> {
    [
        "cargo", "rustc", "npm", "npx", "bun", "bunx", "node", "python", "python3", "pip", "git",
        "ls", "cat", "grep", "rg", "find", "wc", "head", "tail", "sort", "uniq", "echo", "mkdir",
        "cp", "mv", "rm", "touch", "docker", "make", "cmake", "pip3", "pytest", "mypy", "ruff",
        "black", "poetry", "pipenv", "sleep",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Read-only inspection commands that are safe to run without an approval prompt.
/// `find` is allowed only when it carries none of its mutating actions
/// (`-delete`/`-exec`/…). Everything else still goes through the human approval gate.
pub fn is_auto_approved(input: &Value) -> bool {
    let Some(program) = input["command"].as_str() else {
        return false;
    };
    let base = program
        .split('/')
        .next_back()
        .unwrap_or(program)
        .split_whitespace()
        .next()
        .unwrap_or(program);

    // Every token the command will actually run with: inline tokens after the base in
    // `command`, plus the explicit `args` array.
    let mut tokens: Vec<&str> = program.split_whitespace().skip(1).collect();
    if let Some(arr) = input["args"].as_array() {
        tokens.extend(arr.iter().filter_map(serde_json::Value::as_str));
    }

    match base {
        "grep" | "rg" | "ls" | "cat" | "head" | "tail" | "wc" | "tree" | "pwd" | "which"
        | "stat" | "file" | "nl" | "cut" | "uniq" => true,
        "find" => !tokens.iter().any(|t| {
            matches!(
                *t,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprintf"
                    | "-fprint0"
                    | "-fls"
            )
        }),
        _ => false,
    }
}

/// Read-only shell commands that inspect file contents/paths.
fn is_read_inspection_command(base: &str) -> bool {
    matches!(
        base,
        "grep" | "rg" | "cat" | "head" | "tail" | "less" | "more" | "sed" | "awk" | "find"
    )
}

/// Reject grep/cat/etc. aimed at vendored dependency trees — the model should search
/// project source or use web_search for library docs, not read site-packages.
fn command_targets_vendored_deps(program: &str, args: &[String]) -> bool {
    if !is_read_inspection_command(
        program
            .split('/')
            .next_back()
            .unwrap_or(program)
            .split_whitespace()
            .next()
            .unwrap_or(program),
    ) {
        return false;
    }
    let mut tokens: Vec<&str> = program.split_whitespace().skip(1).collect();
    tokens.extend(args.iter().map(String::as_str));
    tokens
        .iter()
        .any(|t| super::workspace::is_vendored_dependency_path(t))
}

/// Split a command line into argv-style tokens, honoring single/double quotes so
/// paths/args containing spaces survive intact.
///
/// The `command` field of a tool call is *supposed* to hold just the program,
/// with the `args` array carrying the rest — but many models (especially the
/// free-tier default) pack the entire line into `command`
/// (`"python3 manage.py migrate"`). Without splitting, `Command::new` would try
/// to exec a binary literally named `"python3 manage.py migrate"` and fail with
/// ENOENT ("spawn failed"), so the agent appears unable to run *any* command.
fn split_command_line(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            '\\' if in_double => {
                if let Some(&n) = chars.peek() {
                    if n == '"' || n == '\\' {
                        cur.push(n);
                        chars.next();
                        continue;
                    }
                }
                cur.push('\\');
                has_token = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    tokens.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(cur);
    }
    tokens
}

pub struct RunCommand {
    root: Arc<PathBuf>,
    env_mgr: EnvManager,
}

impl RunCommand {
    pub fn new(root: Arc<PathBuf>, env_mgr: EnvManager) -> Self {
        Self { root, env_mgr }
    }
}

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command. Read-only inspection commands (grep, rg, find, ls, cat, \
         head, tail, wc) run WITHOUT an approval prompt — use them freely to locate and read \
         code. Prefer dedicated git_* tools for git operations (git_status, git_add, \
         git_commit, git_push, git_reset). Any command may be run; the user approves each one \
         (destructive/network commands always require explicit approval). Commands run in a \
         persistent terminal pool: each result reports the `terminal_id` it ran in; \
         long-running processes (dev servers/watchers) are left running in their own terminal \
         and the agent is released to continue. Pass `terminal_id` to target a specific idle \
         terminal, and use `read_terminal` to read earlier output."
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
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 30)" },
                "terminal_id": { "type": "string", "description": "Optional: reuse a specific terminal from a prior run's terminal_id. Ignored if that terminal is busy; a new one is used instead. Omit to auto-pick an idle terminal." }
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

        // Parse `command` into argv tokens: it may be just the program or a full
        // command line (see `split_command_line`). The first token is the binary;
        // any remaining tokens are leading arguments.
        let cmd_tokens = split_command_line(program);
        let binary = cmd_tokens
            .first()
            .cloned()
            .ok_or_else(|| AppError::InvalidRequest("command is empty".into()))?;

        // No allowlist/blocklist: the user (not a static list) decides what may run via
        // the approval prompt. Only genuine safety rails remain — the vendored-dependency
        // read guard here and the `cwd` containment check below.
        let mut args: Vec<String> = cmd_tokens[1..].to_vec();
        if let Some(arr) = input["args"].as_array() {
            args.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
        }

        if command_targets_vendored_deps(&binary, &args) {
            return Err(AppError::InvalidRequest(
                "Cannot grep/cat/read inside vendored dependency trees (.venv, node_modules, \
                 site-packages). Search project source with search_files/semantic_search, or use \
                 web_search for third-party library documentation."
                    .into(),
            ));
        }

        // Resolve `cwd` strictly inside the project root.
        let work_dir = match input["cwd"].as_str() {
            None | Some("") => self.root.as_ref().clone(),
            Some(p) => super::workspace::resolve_path(self.root.as_ref(), p)?,
        };

        // Clamp the model-supplied timeout: default 30s, hard cap 240s so a single
        // command can never wedge the agent for minutes (the outer tool wrapper is a
        // 300s backstop above this).
        const MAX_TIMEOUT_SECS: u64 = 240;
        let timeout = Duration::from_secs(
            input["timeout_secs"]
                .as_u64()
                .unwrap_or(30)
                .clamp(1, MAX_TIMEOUT_SECS),
        );

        let mut cmd = Command::new(&binary);
        cmd.args(&args)
            .current_dir(&work_dir)
            // Detach stdin so a command that reads input (a bare REPL, an interactive
            // prompt, an editor opened by git) gets EOF immediately instead of hanging
            // until the timeout. kill_on_drop ensures a timed-out command is actually
            // terminated rather than leaking as an orphaned process holding the pipes.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Inject project-env variables (VIRTUAL_ENV, PATH, etc.)
        self.env_mgr.prepare_command(&mut cmd, &work_dir).await;

        let child = cmd
            .spawn()
            .map_err(|e| AppError::InvalidRequest(format!("spawn failed: {e}")))?;

        // On timeout the future (and the Child it owns) is dropped; kill_on_drop then
        // terminates the process so nothing is left running.
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(res) => res.map_err(|e| AppError::InvalidRequest(format!("exec failed: {e}")))?,
            Err(_) => {
                return Ok(json!({
                    "stdout": "",
                    "stderr": format!("command timed out after {}s and was terminated", timeout.as_secs()),
                    "exit_code": -1,
                    "timed_out": true,
                }));
            }
        };

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn tool() -> RunCommand {
        let root = Arc::new(std::env::temp_dir());
        RunCommand::new(root.clone(), EnvManager::new(root))
    }

    // A command that reads stdin (cat with no args) must NOT hang: stdin is detached,
    // so it sees EOF and exits immediately instead of waiting for the timeout.
    #[tokio::test]
    async fn stdin_is_detached_no_hang() {
        let started = Instant::now();
        let out = tool()
            .execute(json!({ "command": "cat", "timeout_secs": 5 }))
            .await
            .expect("cat should run");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cat hung waiting on stdin"
        );
        assert_eq!(out["exit_code"], 0);
    }

    // A command that exceeds its timeout returns a structured timeout result quickly
    // (and the process is killed via kill_on_drop) rather than blocking.
    #[tokio::test]
    async fn timeout_terminates_promptly() {
        let started = Instant::now();
        let out = tool()
            .execute(json!({ "command": "sleep", "args": ["30"], "timeout_secs": 1 }))
            .await
            .expect("should return timeout result, not error");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout did not fire promptly"
        );
        assert_eq!(out["timed_out"], true);
        assert_eq!(out["exit_code"], -1);
    }

    // A full command line packed into `command` (as the free-tier model often does)
    // is split into program + args instead of failing with ENOENT.
    #[tokio::test]
    async fn splits_full_command_line() {
        let out = tool()
            .execute(json!({ "command": "echo hello world", "timeout_secs": 5 }))
            .await
            .expect("echo should run");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "hello world");
    }

    // C-2: a cwd that escapes the project root (absolute path or ../) is rejected
    // instead of running the command outside the workspace.
    #[tokio::test]
    async fn rejects_cwd_escape() {
        for cwd in ["/", "/etc", "../../.."] {
            let err = tool()
                .execute(json!({ "command": "ls", "cwd": cwd }))
                .await
                .expect_err("cwd escape must be rejected");
            assert!(
                format!("{err:?}").contains("escapes project root"),
                "cwd {cwd} not rejected"
            );
        }
    }

    // Read-only inspection commands skip the approval prompt; mutating ones don't.
    #[test]
    fn auto_approves_read_only_commands() {
        assert!(is_auto_approved(&json!({ "command": "grep", "args": ["-rn", "foo", "."] })));
        assert!(is_auto_approved(&json!({ "command": "rg", "args": ["foo"] })));
        assert!(is_auto_approved(&json!({ "command": "ls" })));
        assert!(is_auto_approved(&json!({ "command": "cat", "args": ["src/main.rs"] })));
        assert!(is_auto_approved(&json!({ "command": "find", "args": [".", "-name", "*.rs"] })));
    }

    #[test]
    fn does_not_auto_approve_mutating_or_unknown() {
        // find with a mutating action must still be approved.
        assert!(!is_auto_approved(&json!({ "command": "find", "args": [".", "-delete"] })));
        assert!(!is_auto_approved(&json!({ "command": "find", "args": [".", "-exec", "rm", "{}", ";"] })));
        // Writers / arbitrary commands are not auto-approved.
        assert!(!is_auto_approved(&json!({ "command": "rm", "args": ["-rf", "x"] })));
        assert!(!is_auto_approved(&json!({ "command": "python", "args": ["script.py"] })));
        assert!(!is_auto_approved(&json!({ "command": "git", "args": ["push"] })));
    }
}
