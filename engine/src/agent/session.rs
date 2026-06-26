use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

use crate::models::ToolMessage;

pub struct Session {
    pub id: String,
    pub provider_id: String,
    pub model: String,
    pub project_root: PathBuf,
    pub messages: Vec<ToolMessage>,
    pub max_iterations: u32,
    pub system_prompt: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Server-side tool-output compression on the GetAIBD platform API.
    pub compress: bool,
    /// Host OS + shell the client runs in, so generated commands match it.
    pub environment: Option<String>,
    /// Global user rules from VS Code settings.
    pub user_rules: Option<String>,
    /// Active working directory relative to project (for nested AGENTS.md).
    pub workspace_cwd: Option<PathBuf>,
    /// Structured task ledger (goal + checklist) the agent maintains via the
    /// `update_plan` tool. Re-injected verbatim every turn and never summarized
    /// away, so long runs keep their plan and place even after compression.
    pub task_ledger: Option<String>,
    /// Fingerprints of tool calls the user denied — not retried this run.
    pub denied_actions: HashSet<String>,
    /// Per-workspace chat key forwarded to GetAIBD for OpenRouter sticky routing.
    pub cache_session_id: Option<String>,
    /// User chose to stop (denials, "do nothing", etc.).
    pub user_stopped: bool,
    /// Consecutive approval denials without an approved action between.
    pub approval_denials: u32,
}

impl Session {
    pub fn new(
        provider_id: impl Into<String>,
        model: impl Into<String>,
        project_root: PathBuf,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            provider_id: provider_id.into(),
            model: model.into(),
            project_root,
            messages: Vec::new(),
            max_iterations: 25,
            system_prompt: None,
            reasoning_effort: None,
            compress: false,
            environment: None,
            user_rules: None,
            workspace_cwd: None,
            task_ledger: None,
            denied_actions: HashSet::new(),
            cache_session_id: None,
            user_stopped: false,
            approval_denials: 0,
        }
    }

    /// Stable key for denial cache. Git staging/commit/push map to canonical keys so
    /// `git_add` and `run_command` with `git add` share the same denial.
    pub fn action_fingerprint(tool: &str, arguments: &serde_json::Value) -> String {
        canonical_action_key(tool, arguments).unwrap_or_else(|| {
            serde_json::to_string(&(tool, arguments)).unwrap_or_else(|_| tool.to_string())
        })
    }

    pub fn is_action_denied(&self, tool: &str, arguments: &serde_json::Value) -> bool {
        self.denied_actions
            .contains(&Self::action_fingerprint(tool, arguments))
    }

    pub fn record_denied(&mut self, tool: &str, arguments: &serde_json::Value) {
        self.denied_actions
            .insert(Self::action_fingerprint(tool, arguments));
    }

    /// Human-readable summary of denied actions for the completion reviewer.
    pub fn denied_actions_summary(&self) -> String {
        if self.denied_actions.is_empty() {
            return "(none)".to_string();
        }
        let mut lines: Vec<String> = self.denied_actions.iter().cloned().collect();
        lines.sort();
        lines.join("\n")
    }

    pub fn with_workspace_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.workspace_cwd = cwd;
        self
    }

    pub fn with_user_rules(mut self, rules: Option<String>) -> Self {
        self.user_rules = rules.filter(|s| !s.trim().is_empty());
        self
    }

    #[must_use]
    pub fn with_environment(mut self, environment: Option<String>) -> Self {
        self.environment = environment;
        self
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }

    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    #[must_use]
    pub fn with_compress(mut self, compress: bool) -> Self {
        self.compress = compress;
        self
    }

    #[must_use]
    pub fn with_cache_session_id(mut self, id: Option<String>) -> Self {
        self.cache_session_id = id.filter(|s| !s.trim().is_empty());
        self
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn push_message(&mut self, msg: ToolMessage) {
        self.messages.push(msg);
    }

    /// Compress older messages into a summary, keeping the most recent half.
    pub fn compress_messages(&mut self) {
        if self.messages.len() <= 4 {
            return;
        }
        let keep_count = self.messages.len() / 2;
        let to_summarize = self.messages.len() - keep_count;

        let old: Vec<ToolMessage> = self.messages.drain(..to_summarize).collect();
        let summary = old
            .iter()
            .filter_map(|m| {
                let content = m.content.as_deref().unwrap_or("");
                if content.is_empty() {
                    None
                } else {
                    Some(format!("[{}] {}", m.role, truncate_str(content, 200)))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if !summary.is_empty() {
            self.messages.insert(
                0,
                ToolMessage::system(format!("Previous conversation summary:\n{summary}")),
            );
        }
    }
}

fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}

/// Maps equivalent git/shell actions to one key (e.g. `git add .` == `git_add` with `["."]`).
fn canonical_action_key(tool: &str, args: &serde_json::Value) -> Option<String> {
    match tool {
        "git_add" => {
            let paths = args.get("paths")?.as_array()?;
            Some(format!("git:stage:{}", normalize_stage_spec(paths)))
        }
        "git_commit" => Some("git:commit".to_string()),
        "git_push" => Some("git:push".to_string()),
        "git_reset" => {
            let paths = args.get("paths").and_then(|p| p.as_array());
            let spec = paths.map_or_else(|| "all".to_string(), |p| normalize_stage_spec(p));
            Some(format!("git:unstage:{spec}"))
        }
        "run_command" => {
            let base = args.get("command")?.as_str()?;
            if base != "git" {
                return None;
            }
            let git_args: Vec<&str> = args
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if git_args.is_empty() {
                return None;
            }
            match git_args[0] {
                "add" => Some(format!(
                    "git:stage:{}",
                    normalize_stage_spec_from_strs(&git_args[1..])
                )),
                "commit" => Some("git:commit".to_string()),
                "push" => Some("git:push".to_string()),
                "reset" => {
                    let rest = if git_args.len() > 1 && git_args[1] == "HEAD" {
                        &git_args[2..]
                    } else {
                        &git_args[1..]
                    };
                    Some(format!(
                        "git:unstage:{}",
                        normalize_stage_spec_from_strs(rest)
                    ))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn normalize_stage_spec(paths: &[serde_json::Value]) -> String {
    let specs: Vec<String> = paths
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    normalize_stage_spec_from_strs(
        &specs.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

fn normalize_stage_spec_from_strs(paths: &[&str]) -> String {
    if paths.is_empty() {
        return "all".to_string();
    }
    let mut specs: Vec<String> = paths.iter().map(|p| p.trim().to_string()).collect();
    if specs.iter().any(|p| is_broad_stage_path(p)) {
        return "all".to_string();
    }
    specs.sort();
    specs.join(",")
}

fn is_broad_stage_path(path: &str) -> bool {
    matches!(path, "." | ".." | "-A" | "-a" | "--all" | "*" | ":/")
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_add_all_equivalent_to_run_command() {
        let tool_fp = Session::action_fingerprint("git_add", &json!({ "paths": ["."] }));
        let shell_fp = Session::action_fingerprint(
            "run_command",
            &json!({ "command": "git", "args": ["add", "-A"] }),
        );
        assert_eq!(tool_fp, shell_fp);
        assert_eq!(tool_fp, "git:stage:all");
    }

    #[test]
    fn specific_paths_preserved() {
        let a = Session::action_fingerprint("git_add", &json!({ "paths": ["b.rs", "a.rs"] }));
        let b = Session::action_fingerprint(
            "run_command",
            &json!({ "command": "git", "args": ["add", "a.rs", "b.rs"] }),
        );
        assert_eq!(a, b);
        assert_eq!(a, "git:stage:a.rs,b.rs");
    }
}
