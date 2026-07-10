//! Run-mode approval policy, loosely modelled on Cursor's Run Modes / `permissions.json`.
//!
//! Loaded from `<repo>/.getaibd/permissions.json` (project) merged over
//! `~/.getaibd/permissions.json` (user). When neither file exists this returns
//! `None` and the built-in approval logic is used unchanged — so there is zero
//! behaviour change without an explicit policy.
//!
//! Schema:
//! ```json
//! {
//!   "mode": "auto-review" | "allowlist" | "run-everything",
//!   "allow": ["git status", "cargo *", "read_file"],
//!   "ask":   ["git push*", "*sudo*"],
//!   "deny":  ["rm -rf /*", "*curl*"],
//!   "protect": { "file_deletion": true, "external_files": true }
//! }
//! ```
//! Patterns match a tool name exactly, or wildcard-match the "subject": for
//! `run_command` that's the full command line (`git push origin main`); for other
//! tools it's the tool name.

use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

/// Outcome of a policy lookup. `None` from `decide` means "no opinion — use the
/// built-in approval logic".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run without prompting.
    Allow,
    /// Force an approval prompt (even if the tool would normally auto-run).
    Ask,
    /// Hard-block; the tool never runs.
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    AutoReview,
    Allowlist,
    RunEverything,
}

impl Mode {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "allowlist" | "allow-list" => Mode::Allowlist,
            "run-everything" | "run-all" | "auto-run" => Mode::RunEverything,
            _ => Mode::AutoReview,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawProtect {
    #[serde(default)]
    file_deletion: bool,
    #[serde(default)]
    external_files: bool,
}

#[derive(Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    protect: RawProtect,
}

#[derive(Debug, Clone)]
pub struct Permissions {
    mode: Mode,
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
    protect_file_deletion: bool,
    protect_external_files: bool,
}

fn read_raw(path: &Path) -> Option<Raw> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Raw>(&text) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!("Ignoring malformed {}: {e}", path.display());
            None
        }
    }
}

impl Permissions {
    /// Load + merge policy files. `None` when no policy is configured.
    pub fn load(project_root: &Path) -> Option<Self> {
        let mut raws: Vec<Raw> = Vec::new();
        // Global first (lower priority), then project (overrides mode/protect).
        if let Some(g) = crate::paths::global_dir() {
            if let Some(r) = read_raw(&g.join("permissions.json")) {
                raws.push(r);
            }
        }
        if let Some(r) = read_raw(&crate::paths::project_config_dir(project_root).join("permissions.json"))
        {
            raws.push(r);
        }
        if raws.is_empty() {
            return None;
        }

        let mut perm = Permissions {
            mode: Mode::AutoReview,
            allow: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
            protect_file_deletion: false,
            protect_external_files: false,
        };
        for r in raws {
            if let Some(m) = r.mode.as_deref() {
                perm.mode = Mode::parse(m);
            }
            perm.protect_file_deletion |= r.protect.file_deletion;
            perm.protect_external_files |= r.protect.external_files;
            perm.allow.extend(r.allow);
            perm.ask.extend(r.ask);
            perm.deny.extend(r.deny);
        }
        Some(perm)
    }

    /// Decide how a tool call should be handled. `None` = defer to built-in logic.
    pub fn decide(&self, tool: &str, args: &Value, project_root: &Path) -> Option<Decision> {
        if self.mode == Mode::RunEverything {
            return Some(Decision::Allow);
        }
        let subject = subject_string(tool, args);

        if any_match(&self.deny, tool, &subject) {
            return Some(Decision::Deny);
        }
        if self.protect_file_deletion && is_deletion(tool, &subject) {
            return Some(Decision::Ask);
        }
        if self.protect_external_files && is_external_write(tool, args, project_root) {
            return Some(Decision::Ask);
        }
        if any_match(&self.ask, tool, &subject) {
            return Some(Decision::Ask);
        }
        if any_match(&self.allow, tool, &subject) {
            return Some(Decision::Allow);
        }
        match self.mode {
            // Allowlist: anything not explicitly allowed must be approved.
            Mode::Allowlist => Some(Decision::Ask),
            // Auto-review: no explicit rule — let the built-in heuristics decide.
            Mode::AutoReview => None,
            Mode::RunEverything => Some(Decision::Allow),
        }
    }
}

/// The string a pattern matches against: the full command line for `run_command`,
/// otherwise the tool name.
fn subject_string(tool: &str, args: &Value) -> String {
    if tool == "run_command" {
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = args.get("command").and_then(Value::as_str) {
            parts.push(c.to_string());
        }
        if let Some(arr) = args.get("args").and_then(Value::as_array) {
            for a in arr {
                if let Some(s) = a.as_str() {
                    parts.push(s.to_string());
                }
            }
        }
        parts.join(" ")
    } else {
        tool.to_string()
    }
}

fn any_match(patterns: &[String], tool: &str, subject: &str) -> bool {
    patterns
        .iter()
        .any(|p| p == tool || wildcard_match(p, subject) || wildcard_match(p, tool))
}

/// Deletion detection for the file-deletion protection.
fn is_deletion(tool: &str, subject: &str) -> bool {
    if tool == "delete_file" {
        return true;
    }
    if tool == "run_command" {
        let first = subject.split_whitespace().next().unwrap_or("");
        let base = first.rsplit(['/', '\\']).next().unwrap_or(first);
        return matches!(base, "rm" | "rmdir" | "unlink" | "shred")
            || subject.contains(" rm ")
            || subject.contains(" rmdir ");
    }
    false
}

/// External-write detection: a file tool whose path arg resolves outside the repo.
fn is_external_write(tool: &str, args: &Value, project_root: &Path) -> bool {
    if !matches!(tool, "write_file" | "patch_file" | "move_file" | "delete_file") {
        return false;
    }
    let mut paths: Vec<&str> = Vec::new();
    for key in ["path", "source", "destination", "to", "from"] {
        if let Some(p) = args.get(key).and_then(Value::as_str) {
            paths.push(p);
        }
    }
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    paths.iter().any(|p| {
        let path = Path::new(p);
        if path.is_absolute() {
            let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            !canon.starts_with(&root)
        } else {
            // Relative paths that climb out of the repo.
            p.split(['/', '\\']).filter(|s| *s == "..").count() > 0
                && !root.join(p).canonicalize().map(|c| c.starts_with(&root)).unwrap_or(true)
        }
    })
}

/// Minimal glob: `*` matches any run (including empty); everything else literal.
/// Case-insensitive to match how users write command patterns.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    // Two-pointer with backtracking on the last `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wildcard_basic() {
        assert!(wildcard_match("cargo *", "cargo build"));
        assert!(wildcard_match("*sudo*", "echo sudo rm"));
        assert!(wildcard_match("git push*", "git push origin main"));
        assert!(!wildcard_match("git push*", "git status"));
        assert!(wildcard_match("read_file", "read_file"));
    }

    fn perm(mode: Mode, allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
        Permissions {
            mode,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            protect_file_deletion: false,
            protect_external_files: false,
        }
    }

    #[test]
    fn deny_beats_allow() {
        let p = perm(Mode::AutoReview, &["*"], &[], &["*curl*"]);
        let root = std::path::Path::new("/tmp");
        let d = p.decide("run_command", &json!({"command":"curl","args":["http://x"]}), root);
        assert_eq!(d, Some(Decision::Deny));
    }

    #[test]
    fn allowlist_asks_for_unlisted() {
        let p = perm(Mode::Allowlist, &["git status"], &[], &[]);
        let root = std::path::Path::new("/tmp");
        assert_eq!(
            p.decide("run_command", &json!({"command":"git status"}), root),
            Some(Decision::Allow)
        );
        assert_eq!(
            p.decide("run_command", &json!({"command":"git push"}), root),
            Some(Decision::Ask)
        );
    }

    #[test]
    fn auto_review_defers_when_unmatched() {
        let p = perm(Mode::AutoReview, &["git status"], &[], &[]);
        let root = std::path::Path::new("/tmp");
        assert_eq!(p.decide("run_command", &json!({"command":"ls"}), root), None);
    }
}
