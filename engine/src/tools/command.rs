use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

use crate::error::AppError;
use crate::tools::Tool;

use super::env_manager::EnvManager;

/// Filename prefix for detached background-command logs written to the OS temp
/// dir by `run_in_background`.
const BG_LOG_PREFIX: &str = "getaibd-bg-";

/// Delete background-command log files older than `max_age` from the OS temp
/// directory. Background dev servers are intentionally detached and their logs
/// are never removed, so without this they accumulate for the machine's
/// lifetime. Best-effort; returns the number removed.
pub async fn cleanup_stale_bg_logs(max_age: Duration) -> usize {
    let dir = std::env::temp_dir();
    let mut removed = 0;
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_bg_log = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(BG_LOG_PREFIX) && n.ends_with(".log"));
        if !is_bg_log {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if stale && tokio::fs::remove_file(&path).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Optional terminal sandbox config from `.getaibd/sandbox.json` (project) or
/// `~/.getaibd/sandbox.json`. Off unless `enabled` is true, so default behaviour
/// is unchanged. `writable_paths` grants extra write access (e.g. `~/.cargo`).
#[derive(Debug, Default, serde::Deserialize)]
struct SandboxCfg {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    writable_paths: Vec<String>,
}

fn load_sandbox_cfg(root: &Path) -> Option<SandboxCfg> {
    let read = |p: PathBuf| -> Option<SandboxCfg> {
        serde_json::from_str::<SandboxCfg>(&std::fs::read_to_string(p).ok()?).ok()
    };
    let cfg = read(crate::paths::project_config_dir(root).join("sandbox.json"))
        .or_else(|| crate::paths::global_dir().and_then(|g| read(g.join("sandbox.json"))))?;
    cfg.enabled.then_some(cfg)
}

#[cfg(target_os = "macos")]
fn seatbelt_profile(root: &Path, extra: &[String]) -> String {
    let esc = |p: &str| p.replace('"', "\\\"");
    let mut writes = format!("(subpath \"{}\")", esc(&root.to_string_lossy()));
    for p in extra {
        writes.push_str(&format!(" (subpath \"{}\")", esc(p)));
    }
    // Allow everything (reads/exec/network), then deny writes globally, then re-allow
    // writes only under the workspace, temp dirs, and configured extra paths.
    format!(
        "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write* {writes} \
         (subpath \"/tmp\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\") \
         (subpath \"/dev\"))\n"
    )
}

#[cfg(target_os = "linux")]
fn which_exists(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// When a sandbox is enabled, wrap the command so it cannot write outside the repo
/// (+ temp dirs + configured `writable_paths`). macOS uses Seatbelt (`sandbox-exec`);
/// Linux uses bubblewrap (`bwrap`) when present. Otherwise the command is returned
/// unchanged. Network access is left open so installs/builds keep working.
fn maybe_wrap_sandbox(binary: &str, args: &[String], root: &Path) -> (String, Vec<String>) {
    let Some(_cfg) = load_sandbox_cfg(root) else {
        return (binary.to_string(), args.to_vec());
    };
    #[cfg(target_os = "macos")]
    {
        let profile = seatbelt_profile(root, &_cfg.writable_paths);
        let mut a = vec!["-p".to_string(), profile, binary.to_string()];
        a.extend(args.iter().cloned());
        (String::from("sandbox-exec"), a)
    }
    #[cfg(target_os = "linux")]
    {
        if !which_exists("bwrap") {
            return (binary.to_string(), args.to_vec());
        }
        let root_s = root.to_string_lossy().to_string();
        let mut a: Vec<String> = vec![
            "--ro-bind".into(), "/".into(), "/".into(),
            "--dev".into(), "/dev".into(),
            "--proc".into(), "/proc".into(),
            "--tmpfs".into(), "/tmp".into(),
            "--bind".into(), root_s.clone(), root_s,
        ];
        for p in &_cfg.writable_paths {
            a.push("--bind".into());
            a.push(p.clone());
            a.push(p.clone());
        }
        a.push("--".into());
        a.push(binary.to_string());
        a.extend(args.iter().cloned());
        (String::from("bwrap"), a)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (binary.to_string(), args.to_vec())
    }
}

pub fn default_allowlist() -> HashSet<String> {
    // Note: network/exfiltration tools (curl, wget, …) are intentionally NOT here and
    // are additionally hard-blocked in `is_blocked_command` so a custom allowlist
    // can't re-enable them. The command tool is approval-gated, not a sandbox — these
    // restrictions are defense-in-depth on top of the human approval prompt.
    [
        // language toolchains / runtimes
        "cargo", "rustc", "rustup", "node", "deno", "bun", "bunx", "python", "python3", "go",
        "ruby", "php", "java", "javac", "kotlin", "kotlinc", "scala", "dotnet", "dart", "flutter",
        "elixir", "mix", "erl", "swift", "swiftc", "lua", "perl", "Rscript",
        // package managers
        "npm", "npx", "yarn", "pnpm", "pip", "pip3", "pipx", "poetry", "pipenv", "uv", "uvx",
        "pixi", "hatch", "pdm", "conda", "mamba", "gem", "bundle", "composer", "cabal", "stack",
        "nuget", "brew",
        // build systems / bundlers / task runners
        "make", "cmake", "ninja", "meson", "bazel", "buck", "gradle", "gradlew", "mvn", "ant",
        "webpack", "rspack", "rsbuild", "vite", "rollup", "esbuild", "parcel", "turbo", "nx",
        "lerna", "grunt", "gulp", "rake", "just", "task", "tsc", "tsx", "ts-node", "swc",
        // web / app frameworks (project-local CLIs)
        "next", "nuxt", "astro", "remix", "gatsby", "ng", "vue", "svelte", "vite-node", "expo",
        "react-native", "rails", "django-admin", "flask", "uvicorn", "gunicorn", "hypercorn",
        "celery", "streamlit", "dash", "trunk", "wasm-pack", "dx",
        // test / lint / format / typecheck
        "pytest", "tox", "nox", "mypy", "ruff", "black", "isort", "flake8", "pylint", "bandit",
        "jest", "vitest", "mocha", "playwright", "cypress", "eslint", "prettier", "biome",
        "stylelint", "clippy", "rustfmt", "gofmt", "golangci-lint", "phpunit", "rspec",
        "alembic", "jupyter", "ipython",
        // file / text inspection (most also auto-approved as read-only)
        "ls", "cat", "grep", "rg", "find", "wc", "head", "tail", "sort", "uniq", "cut", "nl", "tr",
        "diff", "cmp", "sed", "awk", "tree", "stat", "file", "pwd", "which", "realpath", "readlink",
        "dirname", "basename", "du", "df", "date", "whoami", "uname", "hostname", "printf", "jq",
        "column", "tee",
        // file manipulation (approval-gated)
        "echo", "mkdir", "cp", "mv", "rm", "touch", "chmod", "ln", "tar", "zip", "unzip", "gzip",
        "gunzip", "sleep",
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

/// Long-running / never-exiting commands (dev servers, watchers) that must NOT be
/// waited on: they never return, so blocking until the timeout (up to 240s) freezes
/// the agent mid-run. These are started in the background instead and their output
/// captured to a log file the model can `cat`/`tail` later. Matches the VS Code
/// extension's "background dev-server release" behavior.
fn is_long_running_command(binary: &str, args: &[String]) -> bool {
    let base = binary.rsplit(['/', '\\']).next().unwrap_or(binary);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let has = |needle: &str| a.iter().any(|t| *t == needle);
    let first = a.first().copied().unwrap_or("");
    match base {
        "uvicorn" | "gunicorn" | "hypercorn" | "daphne" | "granian" | "streamlit"
        | "http-server" | "live-server" | "serve" | "watchmedo" => true,
        "flask" => has("run"),
        "celery" => has("worker") || has("beat") || has("flower"),
        "manage.py" => has("runserver") || has("runserver_plus"),
        "python" | "python3" | "py" => {
            a.iter().any(|t| matches!(*t, "runserver" | "runserver_plus"))
                || (has("-m")
                    && a.iter().any(|t| {
                        matches!(
                            *t,
                            "http.server" | "uvicorn" | "gunicorn" | "flask" | "streamlit"
                        )
                    }))
        }
        "npm" | "pnpm" | "yarn" | "bun" | "bunx" | "npx" | "deno" => {
            a.iter().any(|t| matches!(*t, "dev" | "start" | "serve" | "watch"))
        }
        "next" | "nuxt" | "vite" | "astro" | "remix" | "gatsby" | "ng" | "expo"
        | "react-scripts" | "vite-node" => {
            matches!(first, "dev" | "serve" | "start") || a.is_empty()
        }
        "rails" => has("server") || has("s"),
        "php" => has("-S"),
        "webpack" => has("serve"),
        "webpack-dev-server" | "webpack-serve" => true,
        "cargo" => has("watch") || first == "watch",
        "dotnet" => has("watch"),
        "nodemon" | "watchexec" | "air" | "reflex" => true,
        "trunk" => has("serve"),
        "tail" => has("-f") || has("-F"),
        "docker" | "docker-compose" => {
            (first == "up" || (has("compose") && has("up"))) && !has("-d") && !has("--detach")
        }
        _ => a.iter().any(|t| matches!(*t, "--watch")),
    }
}

/// Keep only the last `max` characters of a string (background logs can grow large;
/// the tail holds the most recent, most relevant output).
fn tail_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let start = s.chars().count() - max;
    s.chars().skip(start).collect()
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

    /// Start a never-exiting command (dev server/watcher) detached: pipe its output to
    /// a log file, wait a short grace period to capture startup output (and catch an
    /// immediate crash), then leave it running and return its pid + log path. The agent
    /// can `cat`/`tail` the log later instead of blocking on a process that never ends.
    async fn run_in_background(
        &self,
        binary: &str,
        args: &[String],
        work_dir: &Path,
    ) -> Result<Value, AppError> {
        use std::process::Stdio;

        let log_path = std::env::temp_dir().join(format!(
            "{BG_LOG_PREFIX}{}-{}.log",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ));
        let log = std::fs::File::create(&log_path)
            .map_err(|e| AppError::InvalidRequest(format!("background log create failed: {e}")))?;
        let log_err = log
            .try_clone()
            .map_err(|e| AppError::InvalidRequest(format!("background log clone failed: {e}")))?;

        let (program, spawn_args) = maybe_wrap_sandbox(binary, args, self.root.as_ref());
        let mut cmd = Command::new(&program);
        cmd.args(&spawn_args)
            .current_dir(work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            // Do NOT kill on drop: the whole point is that this keeps running after we
            // return so the user's dev server stays up for the rest of the session.
            .kill_on_drop(false);
        self.env_mgr.prepare_command(&mut cmd, work_dir).await;
        // Let shells detect an agent session (e.g. to skip heavy prompts/themes).
        cmd.env("GETAIBD_AGENT", "1");

        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::InvalidRequest(format!("spawn failed: {e}")))?;
        let pid = child.id().unwrap_or(0);

        // Grace period: long enough to capture a server's startup banner or a fast
        // failure, short enough that the agent isn't stalled.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Did it crash on startup? Report that like a normal (failed) command instead of
        // pretending a dead server is "running in the background".
        if let Ok(Some(status)) = child.try_wait() {
            let out = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Ok(json!({
                "stdout": tail_chars(&out, 8000),
                "stderr": "",
                "exit_code": status.code().unwrap_or(-1),
                "background": false,
                "note": "This command exited almost immediately (it was treated as a background \
                         server but did not stay running). See stdout for why.",
            }));
        }

        // Still running → detach (dropping the handle does not kill it, kill_on_drop=false).
        drop(child);
        let partial = std::fs::read_to_string(&log_path).unwrap_or_default();
        Ok(json!({
            "background": true,
            "pid": pid,
            "log_file": log_path.to_string_lossy(),
            "stdout": tail_chars(&partial, 4000),
            "stderr": "",
            "exit_code": 0,
            "note": format!(
                "Started in the background (pid {pid}); it is still running and will keep \
                 running. Its output is being appended to {log}. Do NOT wait on it — continue \
                 with the next step. To read its latest output later, run `cat {log}` (or `tail \
                 -n 50 {log}`).",
                log = log_path.display()
            ),
        }))
    }
}

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command. This tool is ALWAYS available — it is never blocked or \
         disabled for any language (python, node, etc.); just call it. To LOCATE code, prefer \
         the dedicated search tools (search_files for instant grep, search_code, semantic_search, \
         find_symbol) over shelling out to grep/rg/find here — they are faster and return \
         structured results. Read-only inspection commands (grep, rg, find, ls, cat, head, tail, \
         wc) do run WITHOUT an approval prompt when you do need them. For any other command the app AUTOMATICALLY \
         shows the user an approval prompt and handles permission — you do NOT need to ask the \
         user to run it or to approve it (dangerous commands such as rm/sudo/curl always \
         re-prompt). \
         Dev servers and watchers (runserver, npm run dev, uvicorn, vite, …) are detected \
         automatically (or set background:true) and started detached: the call returns \
         immediately with the process pid and a log_file to cat/tail — never wait on them. \
         Prefer dedicated git_* tools for git operations (git_status, git_add, git_commit, \
         git_push, git_reset)."
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
                "background": { "type": "boolean", "description": "Run without waiting for the process to exit. Use for dev servers/watchers (runserver, npm run dev, uvicorn, vite, …) that never return — the tool returns immediately with the process pid and a log_file path you can `cat`/`tail` to read output. Such commands are auto-detected even if this is omitted." },
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

        // No allowlist: the user (not a static list) decides what may run via the
        // approval prompt. Only genuine safety rails remain — the `cwd` containment
        // check below and the vendored-dependency read guard here.
        // Tolerate every `args` shape models emit. Dropping it silently would turn
        // e.g. `{command:"grep", args:"-rn foo src"}` into a bare `grep` that just
        // prints its usage banner.
        let mut args: Vec<String> = cmd_tokens[1..].to_vec();
        match &input["args"] {
            Value::Array(arr) => {
                for v in arr {
                    match v {
                        Value::String(s) => args.push(s.clone()),
                        Value::Null => {}
                        // Stringify scalars (numbers/bools) rather than skipping them.
                        other => args.push(other.to_string()),
                    }
                }
            }
            // A single string holds the whole argument tail; split it into argv
            // tokens (this is exec, not a shell, so it can't word-split for us).
            Value::String(s) if !s.trim().is_empty() => {
                args.extend(split_command_line(s));
            }
            _ => {}
        }

        if command_targets_vendored_deps(&binary, &args) {
            return Err(AppError::InvalidRequest(
                "Cannot grep/cat/read inside vendored dependency trees (.venv, node_modules, \
                 site-packages). Search project source with search_files/semantic_search, or use \
                 web_search for third-party library documentation."
                    .into(),
            ));
        }

        // Resolve `cwd` strictly inside the project root. `PathBuf::join` with an
        // absolute path silently discards the root, so an unvalidated `cwd` of `/` or
        // `/etc` would run the command anywhere on the host — route it through the same
        // containment check the file tools use.
        let work_dir = match input["cwd"].as_str() {
            None | Some("") => self.root.as_ref().clone(),
            Some(p) => super::workspace::resolve_command_cwd(self.root.as_ref(), p)?,
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

        // Dev servers / watchers never exit, so waiting on them (up to the 240s cap)
        // freezes the agent mid-run. Detect them (or honor an explicit `background`
        // flag), start them detached, capture output to a log file, and return right
        // away so the agent keeps going — mirroring the VS Code extension.
        let background =
            input["background"].as_bool().unwrap_or(false) || is_long_running_command(&binary, &args);
        if background {
            return self
                .run_in_background(&binary, &args, &work_dir)
                .await;
        }

        let (program, spawn_args) = maybe_wrap_sandbox(&binary, &args, self.root.as_ref());
        let mut cmd = Command::new(&program);
        cmd.args(&spawn_args)
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
        // Let shells detect an agent session (e.g. to skip heavy prompts/themes).
        cmd.env("GETAIBD_AGENT", "1");

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

    // A full command line packed into `command` (no `args` array) must still run —
    // the free-tier model does this, and prior to splitting it failed with ENOENT.
    #[tokio::test]
    async fn runs_full_command_line_in_command_field() {
        let out = tool()
            .execute(json!({ "command": "echo hello world", "timeout_secs": 5 }))
            .await
            .expect("full command line should run");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "hello world");
    }

    // A model that packs the argument tail into `args` as a single STRING (instead
    // of an array) must still run with those args — otherwise `grep`/`rg`/etc. would
    // run bare and just print a usage banner.
    #[tokio::test]
    async fn runs_string_shaped_args() {
        let out = tool()
            .execute(json!({ "command": "echo", "args": "hello world", "timeout_secs": 5 }))
            .await
            .expect("string args should run");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "hello world");
    }

    // Non-string scalar args (numbers) are stringified rather than dropped.
    #[tokio::test]
    async fn stringifies_scalar_args() {
        let out = tool()
            .execute(json!({ "command": "echo", "args": ["a", 1, true], "timeout_secs": 5 }))
            .await
            .expect("scalar args should run");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "a 1 true");
    }

    #[test]
    fn split_command_line_handles_quotes_and_spaces() {
        assert_eq!(
            split_command_line("python3 manage.py migrate"),
            vec!["python3", "manage.py", "migrate"]
        );
        assert_eq!(
            split_command_line("echo \"a b\" c"),
            vec!["echo", "a b", "c"]
        );
        assert_eq!(split_command_line("  ls   -la "), vec!["ls", "-la"]);
    }

    #[tokio::test]
    async fn blocks_grep_in_vendored_tree() {
        let err = tool()
            .execute(json!({
                "command": "grep",
                "args": ["-r", "foo", ".venv/lib"]
            }))
            .await
            .expect_err("grep in .venv must be blocked");
        assert!(
            format!("{err:?}").contains("vendored"),
            "expected vendored block, got {err:?}"
        );
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
