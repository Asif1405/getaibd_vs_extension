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
    // Note: network/exfiltration tools (curl, wget, …) are intentionally NOT here and
    // are additionally hard-blocked in `is_blocked_command` so a custom allowlist
    // can't re-enable them. The command tool is approval-gated, not a sandbox — these
    // restrictions are defense-in-depth on top of the human approval prompt.
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

/// Commands that are always rejected even if an operator adds them to the allowlist:
/// network/exfiltration utilities and arbitrary-shell launchers. A model under prompt
/// injection could otherwise use these to exfiltrate the workspace or bypass the
/// allowlist entirely (`sh -c '<anything>'`).
fn is_blocked_command(base: &str) -> bool {
    matches!(
        base,
        "curl"
            | "wget"
            | "nc"
            | "ncat"
            | "netcat"
            | "telnet"
            | "ssh"
            | "scp"
            | "sftp"
            | "ftp"
            | "rsync"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "env"
            | "xargs"
            | "eval"
    )
}

/// Interpreters that can run arbitrary inline code via an eval flag. For these we
/// reject inline-code flags (below) so the allowlist isn't trivially bypassed:
/// `python -c "..."` / `node -e "..."` is equivalent to "run anything".
fn is_interpreter(base: &str) -> bool {
    matches!(
        base,
        "python" | "python3" | "node" | "bun" | "deno" | "ruby" | "perl" | "php"
    )
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

/// Inline-eval flags that turn an interpreter into an arbitrary-code runner.
fn is_inline_eval_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-c" | "-e" | "--eval" | "-p" | "--print" | "-r" | "--exec"
    )
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
        "Execute a shell command. Read-only inspection commands (grep, rg, find, ls, cat, \
         head, tail, wc) run WITHOUT an approval prompt — use them freely to locate and read \
         code. Prefer dedicated git_* tools for git operations (git_status, git_add, \
         git_commit, git_push, git_reset). Only allowlisted commands are permitted. \
         Commands run in a persistent terminal pool: each result reports the `terminal_id` \
         it ran in; long-running processes (dev servers/watchers) are left running in their \
         own terminal and the agent is released to continue. Pass `terminal_id` to target a \
         specific idle terminal, and use `read_terminal` to read earlier output."
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

        let base = program
            .split('/')
            .next_back()
            .unwrap_or(program)
            .split_whitespace()
            .next()
            .unwrap_or(program);

        if is_blocked_command(base) {
            return Err(AppError::InvalidRequest(format!(
                "Command '{base}' is blocked for security (network/exfiltration or shell \
                 launcher). Use a dedicated tool, or write a script file and run it with an \
                 allowed interpreter."
            )));
        }

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

        // Defense-in-depth: an allowlisted interpreter must not be used to eval
        // arbitrary inline code (that would make the allowlist meaningless). The model
        // can still run interpreters on real script files.
        if is_interpreter(base) && args.iter().any(|a| is_inline_eval_flag(a)) {
            return Err(AppError::InvalidRequest(format!(
                "Inline code execution (e.g. -c/-e/--eval) is disabled for '{base}'. Write the \
                 code to a file and run that file instead."
            )));
        }

        // Resolve `cwd` strictly inside the project root. `PathBuf::join` with an
        // absolute path silently discards the root, so an unvalidated `cwd` of `/` or
        // `/etc` would run the command anywhere on the host — route it through the same
        // containment check the file tools use.
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

        let mut cmd = Command::new(program);
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
        RunCommand::new(root.clone(), default_allowlist(), EnvManager::new(root))
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

    // C-1: network/exfiltration and shell-launcher commands are blocked even though a
    // user could try to add them to a custom allowlist.
    #[tokio::test]
    async fn blocks_network_and_shell_commands() {
        for cmd in ["curl", "wget", "sh", "bash", "nc", "ssh"] {
            let mut list = default_allowlist();
            list.insert(cmd.to_string()); // even if explicitly allowed…
            let root = Arc::new(std::env::temp_dir());
            let tool = RunCommand::new(root.clone(), list, EnvManager::new(root));
            let err = tool
                .execute(json!({ "command": cmd, "args": ["http://evil/"] }))
                .await
                .expect_err("network/shell command must be blocked");
            assert!(format!("{err:?}").contains("blocked"), "{cmd} not blocked");
        }
    }

    // C-1: an allowlisted interpreter cannot be used to eval arbitrary inline code.
    #[tokio::test]
    async fn blocks_interpreter_inline_eval() {
        for (cmd, flag) in [("python3", "-c"), ("node", "-e"), ("node", "-p")] {
            let err = tool()
                .execute(json!({ "command": cmd, "args": [flag, "print(1)"] }))
                .await
                .expect_err("inline eval must be rejected");
            assert!(
                format!("{err:?}").contains("Inline code execution"),
                "{cmd} {flag} not rejected"
            );
        }
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
