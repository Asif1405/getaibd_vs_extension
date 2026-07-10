use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::AppError;
use crate::tools::Tool;

/// Builds the diff payload surfaced to the UI. Skips bodies for very large files.
fn edit_payload(path: &str, old_content: &str, new_content: &str) -> Value {
    const MAX_DIFF_BYTES: usize = 512 * 1024;
    if old_content.len() > MAX_DIFF_BYTES || new_content.len() > MAX_DIFF_BYTES {
        return json!({ "path": path, "too_large": true });
    }
    json!({
        "path": path,
        "old_content": old_content,
        "new_content": new_content,
    })
}

/// Resolve a user/agent-supplied path.
///
/// This is a local, user-invoked CLI (gated by the folder-trust prompt and the
/// same shell the user already controls via `run_command`), so paths are *not*
/// jailed to the project root: absolute paths and `~` are honored as-is, and
/// relative paths resolve against the project root as a convenient default.
pub(crate) fn resolve_path(root: &Path, relative: &str) -> Result<PathBuf, AppError> {
    // Tilde expansion so "~/Desktop/foo" works like it does in a shell.
    let expanded: PathBuf = if relative == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(relative))
    } else if let Some(rest) = relative.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(relative),
        }
    } else {
        PathBuf::from(relative)
    };

    if expanded.is_absolute() {
        return Ok(expanded);
    }

    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    Ok(root_canonical.join(expanded))
}

/// Working directory for `run_command` — must stay inside the project root even
/// though file tools allow absolute paths elsewhere on disk.
pub(crate) fn resolve_command_cwd(root: &Path, cwd: &str) -> Result<PathBuf, AppError> {
    if cwd.contains("..") {
        return Err(AppError::InvalidRequest(format!(
            "cwd '{cwd}' escapes project root"
        )));
    }
    let resolved = if Path::new(cwd).is_absolute() {
        PathBuf::from(cwd)
    } else {
        let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        root_canonical.join(cwd)
    };
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let resolved_canonical = resolved.canonicalize().unwrap_or(resolved);
    if !resolved_canonical.starts_with(&root_canonical) {
        return Err(AppError::InvalidRequest(format!(
            "cwd '{cwd}' escapes project root"
        )));
    }
    Ok(resolved_canonical)
}

/// Dependency / build directories the agent must not read or list — use `web_search`
/// for third-party library docs instead of digging through vendored source.
pub fn is_vendored_dependency_path(path: &str) -> bool {
    let p = path.replace('\\', "/").to_lowercase();
    const SEGMENTS: &[&str] = &[
        "/.venv/",
        "/venv/",
        "/node_modules/",
        "/site-packages/",
        "/vendor/",
        "/.tox/",
        "/__pycache__/",
        "/dist-packages/",
        "/target/",
    ];
    if SEGMENTS.iter().any(|s| p.contains(s)) {
        return true;
    }
    p.starts_with(".venv/")
        || p.starts_with("venv/")
        || p.starts_with("node_modules/")
        || p.starts_with("target/")
        || p == ".venv"
        || p == "venv"
        || p == "node_modules"
        || p == "target"
}

pub(crate) fn is_vendored_path(path: &Path) -> bool {
    is_vendored_dependency_path(&path.to_string_lossy())
}

pub(crate) fn reject_vendored_path(path: &Path, rel: &str) -> Result<(), AppError> {
    if is_vendored_dependency_path(rel) || is_vendored_path(path) {
        return Err(AppError::InvalidRequest(format!(
            "{rel} is inside a vendored dependency tree (.venv/node_modules/site-packages). \
             Read project source instead — use search_files/semantic_search on the repo, or \
             web_search for third-party library docs."
        )));
    }
    Ok(())
}

pub struct ReadFile {
    root: Arc<PathBuf>,
}

impl ReadFile {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read file contents. Optionally specify line_start and line_end for a range."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to project root" },
                "line_start": { "type": "integer", "description": "Start line (1-indexed, inclusive)" },
                "line_end": { "type": "integer", "description": "End line (1-indexed, inclusive)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let rel = input["path"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("path is required".into()))?;
        if is_vendored_dependency_path(rel) {
            return Err(AppError::InvalidRequest(format!(
                "{rel} is inside a vendored dependency tree (.venv/node_modules/site-packages). \
                 Read project source instead — use search_files/semantic_search on the repo, or \
                 web_search for third-party library docs."
            )));
        }
        let path = resolve_path(&self.root, rel)?;
        reject_vendored_path(&path, rel)?;
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_dir() => {
                let hint = immediate_entries_hint(&path).await;
                return Err(AppError::InvalidRequest(format!(
                    "{rel} is a directory, not a file — use list_directory to view it, then read_file on a specific file inside{hint}"
                )));
            }
            Err(e) => {
                return Err(AppError::InvalidRequest(format!("Cannot read {rel}: {e}")));
            }
            _ => {}
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot read {rel}: {e}")))?;

        let line_start = input["line_start"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        let line_end = input["line_end"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());

        let result = match (line_start, line_end) {
            (Some(start), Some(end)) => {
                let lines: Vec<&str> = content.lines().collect();
                let s = start.saturating_sub(1);
                let e = end.min(lines.len());
                lines[s..e]
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("{}|{l}", s + i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => content,
        };

        Ok(json!({ "content": result }))
    }
}

pub struct WriteFile {
    root: Arc<PathBuf>,
    edits: crate::tools::edits::EditTracker,
}

impl WriteFile {
    pub fn new(root: Arc<PathBuf>, edits: crate::tools::edits::EditTracker) -> Self {
        Self { root, edits }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write content to a file. Creates parent directories if create_dirs is true."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to project root" },
                "content": { "type": "string", "description": "Content to write" },
                // Accept boolean OR string: some providers (e.g. Groq) strictly validate the
                // model's tool-call output against this schema and reject a stringy "true",
                // which would otherwise fail the whole turn. We coerce it in execute().
                "create_dirs": { "type": ["boolean", "string"], "description": "Create parent directories if missing (true/false)" }
            },
            "required": ["path", "content"]
        })
    }

    fn requires_approval(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let rel = input["path"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("path is required".into()))?;
        let content = input["content"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("content is required".into()))?;
        let create_dirs = match &input["create_dirs"] {
            Value::Bool(b) => *b,
            Value::String(s) => matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes"
            ),
            _ => false,
        };

        let path = resolve_path(&self.root, rel)?;
        reject_vendored_path(&path, rel)?;

        if create_dirs {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| AppError::InvalidRequest(format!("Cannot create dirs: {e}")))?;
            }
        }

        let old_content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        self.edits.record_baseline(rel, &old_content);

        let bytes = content.len();
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot write {rel}: {e}")))?;

        Ok(json!({
            "written": rel,
            "bytes": bytes,
            "_edit": edit_payload(rel, &old_content, content)
        }))
    }
}

pub struct PatchFile {
    root: Arc<PathBuf>,
    edits: crate::tools::edits::EditTracker,
}

impl PatchFile {
    pub fn new(root: Arc<PathBuf>, edits: crate::tools::edits::EditTracker) -> Self {
        Self { root, edits }
    }
}

#[async_trait]
impl Tool for PatchFile {
    fn name(&self) -> &'static str {
        "patch_file"
    }

    fn description(&self) -> &'static str {
        "Apply a search-and-replace edit to a file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to project root" },
                "old_text": { "type": "string", "description": "Text to find" },
                "new_text": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn requires_approval(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let rel = input["path"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("path is required".into()))?;
        let old_text = input["old_text"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("old_text is required".into()))?;
        let new_text = input["new_text"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("new_text is required".into()))?;

        let path = resolve_path(&self.root, rel)?;
        reject_vendored_path(&path, rel)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot read {rel}: {e}")))?;

        if !content.contains(old_text) {
            return Err(AppError::InvalidRequest(format!(
                "old_text not found in {rel}"
            )));
        }

        self.edits.record_baseline(rel, &content);
        let updated = content.replacen(old_text, new_text, 1);
        tokio::fs::write(&path, &updated)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot write {rel}: {e}")))?;

        Ok(json!({
            "patched": rel,
            "_edit": edit_payload(rel, &content, &updated)
        }))
    }
}

pub struct ListDirectory {
    root: Arc<PathBuf>,
}

impl ListDirectory {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ListDirectory {
    fn name(&self) -> &'static str {
        "list_directory"
    }

    fn description(&self) -> &'static str {
        "List files and directories at the given path."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path relative to project root (default: '.')" },
                "recursive": { "type": "boolean", "description": "List recursively" }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let rel = input["path"].as_str().unwrap_or(".");
        let recursive = input["recursive"].as_bool().unwrap_or(false);
        let path = resolve_path(&self.root, rel)?;
        reject_vendored_path(&path, rel)?;
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => {
                return Err(AppError::InvalidRequest(format!(
                    "{rel} is a file, not a directory — use read_file to read it, or list_directory on its parent folder"
                )));
            }
            Err(e) => {
                return Err(AppError::InvalidRequest(format!("Cannot list {rel}: {e}")));
            }
            _ => {}
        }

        let mut entries = Vec::new();
        collect_entries(&path, &path, recursive, &mut entries).await?;
        Ok(json!({ "entries": entries }))
    }
}

/// Short preview of a directory's immediate entries, used to make
/// "you read a directory" errors self-correcting (so the agent can pick the
/// right child file without an extra round-trip).
async fn immediate_entries_hint(dir: &Path) -> String {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return String::new();
    };
    let mut names: Vec<String> = Vec::new();
    let mut truncated = false;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let is_dir = entry.metadata().await.map(|m| m.is_dir()).unwrap_or(false);
        let mut name = entry.file_name().to_string_lossy().to_string();
        if is_dir {
            name.push('/');
        }
        names.push(name);
        if names.len() >= 50 {
            truncated = true;
            break;
        }
    }
    if names.is_empty() {
        return " (the directory is empty)".to_string();
    }
    names.sort();
    if truncated {
        names.push("…".to_string());
    }
    format!(" — it contains: {}", names.join(", "))
}

async fn collect_entries(
    base: &Path,
    dir: &Path,
    recursive: bool,
    out: &mut Vec<Value>,
) -> Result<(), AppError> {
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| AppError::InvalidRequest(format!("Cannot read dir: {e}")))?;

    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("readdir: {e}")))?
    {
        let meta = entry.metadata().await.ok();
        let name = entry
            .path()
            .strip_prefix(base)
            .unwrap_or(&entry.path())
            .to_string_lossy()
            .to_string();
        let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
        let size = meta.as_ref().map_or(0, std::fs::Metadata::len);

        out.push(json!({
            "name": name,
            "type": if is_dir { "directory" } else { "file" },
            "size": size,
        }));

        if recursive && is_dir {
            Box::pin(collect_entries(base, &entry.path(), true, out)).await?;
        }
    }

    Ok(())
}

pub struct SearchFiles {
    root: Arc<PathBuf>,
}

impl SearchFiles {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for SearchFiles {
    fn name(&self) -> &'static str {
        "search_files"
    }

    fn description(&self) -> &'static str {
        "Fast, instant grep over the workspace — your DEFAULT way to locate code by an exact \
         string, symbol, or regex. Runs in-process (ripgrep) and returns matching file paths \
         with line numbers, so it is faster and cleaner than shelling out to `grep`/`rg` via \
         run_command (don't do that for code search). Supports full regex and word boundaries: \
         search the task's key terms plus close synonyms via alternation (e.g. \
         `login|signin|authenticate`, `\\bPaymentService\\b`), then read only the files that \
         match. For a meaning-based question use semantic_search / search_code instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (text or regex). Use alternation to cover synonyms, e.g. 'redirect|handle_no_permission'" },
                "path": { "type": "string", "description": "Directory to search (default: '.')" },
                "glob": { "type": "string", "description": "File glob filter (e.g. '*.rs')" },
                "max_results": { "type": "integer", "description": "Max matches to return (default: 50)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let pattern = input["pattern"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("pattern is required".into()))?;
        let search_path = input["path"].as_str().unwrap_or(".");
        let max = usize::try_from(input["max_results"].as_u64().unwrap_or(50)).unwrap_or(50);

        let dir = resolve_path(&self.root, search_path)?;
        reject_vendored_path(&dir, search_path)?;
        let mut results = Vec::new();
        search_recursive(&dir, &dir, pattern, max, &mut results).await?;

        Ok(json!({ "matches": results, "count": results.len() }))
    }
}

pub struct MoveFile {
    root: Arc<PathBuf>,
}

impl MoveFile {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for MoveFile {
    fn name(&self) -> &'static str {
        "move_file"
    }

    fn description(&self) -> &'static str {
        "Move or rename a file or directory. Use this to relocate files (e.g. move SUMMARY.md into docs/) instead of recreating them."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string", "description": "Existing path relative to project root" },
                "to": { "type": "string", "description": "Destination path relative to project root" }
            },
            "required": ["from", "to"]
        })
    }

    fn requires_approval(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let from_rel = input["from"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("from is required".into()))?;
        let to_rel = input["to"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("to is required".into()))?;

        let from = resolve_path(&self.root, from_rel)?;
        let to = resolve_path(&self.root, to_rel)?;
        reject_vendored_path(&from, from_rel)?;
        reject_vendored_path(&to, to_rel)?;

        if !from.exists() {
            return Err(AppError::InvalidRequest(format!("Source not found: {from_rel}")));
        }
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AppError::InvalidRequest(format!("Cannot create dirs: {e}")))?;
        }
        tokio::fs::rename(&from, &to)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot move {from_rel} to {to_rel}: {e}")))?;

        Ok(json!({ "moved": from_rel, "to": to_rel }))
    }
}

pub struct DeleteFile {
    root: Arc<PathBuf>,
}

impl DeleteFile {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for DeleteFile {
    fn name(&self) -> &'static str {
        "delete_file"
    }

    fn description(&self) -> &'static str {
        "Delete a file. Set recursive=true to remove a directory and its contents."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to project root" },
                "recursive": { "type": "boolean", "description": "Remove a directory recursively" }
            },
            "required": ["path"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let rel = input["path"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("path is required".into()))?;
        let recursive = input["recursive"].as_bool().unwrap_or(false);
        let path = resolve_path(&self.root, rel)?;
        reject_vendored_path(&path, rel)?;

        if !path.exists() {
            return Err(AppError::InvalidRequest(format!("Not found: {rel}")));
        }
        if path.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|e| AppError::InvalidRequest(format!("Cannot remove {rel}: {e}")))?;
            } else {
                tokio::fs::remove_dir(&path)
                    .await
                    .map_err(|e| AppError::InvalidRequest(format!("Cannot remove {rel} (use recursive for non-empty dirs): {e}")))?;
            }
        } else {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| AppError::InvalidRequest(format!("Cannot delete {rel}: {e}")))?;
        }

        Ok(json!({ "deleted": rel }))
    }
}

async fn search_recursive(
    base: &Path,
    dir: &Path,
    pattern: &str,
    max: usize,
    out: &mut Vec<Value>,
) -> Result<(), AppError> {
    if out.len() >= max {
        return Ok(());
    }

    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return Ok(());
    };

    while let Ok(Some(entry)) = rd.next_entry().await {
        if out.len() >= max {
            break;
        }

        let path = entry.path();
        let meta = entry.metadata().await.ok();

        if meta.as_ref().is_some_and(std::fs::Metadata::is_dir) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "venv" | "site-packages" | "vendor" | "__pycache__"
                )
            {
                continue;
            }
            if is_vendored_dependency_path(&path.to_string_lossy()) {
                continue;
            }
            Box::pin(search_recursive(base, &path, pattern, max, out)).await?;
        } else if meta.as_ref().is_some_and(std::fs::Metadata::is_file) {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if is_vendored_dependency_path(&rel) {
                    continue;
                }
                for (i, line) in content.lines().enumerate() {
                    if out.len() >= max {
                        break;
                    }
                    if line.contains(pattern) {
                        out.push(json!({
                            "file": rel,
                            "line": i + 1,
                            "content": line.trim(),
                        }));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod vendored_tests {
    use super::*;

    #[test]
    fn blocks_venv_and_node_modules() {
        assert!(is_vendored_dependency_path("project/.venv/lib/python3.12/site.py"));
        assert!(is_vendored_dependency_path("node_modules/lodash/index.js"));
        assert!(is_vendored_dependency_path("target/debug/getaibd"));
        assert!(!is_vendored_dependency_path("src/main.rs"));
    }
}
