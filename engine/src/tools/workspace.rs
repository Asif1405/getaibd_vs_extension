use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
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

pub(crate) fn resolve_path(root: &Path, relative: &str) -> Result<PathBuf, AppError> {
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let candidate = if Path::new(relative).is_absolute() {
        PathBuf::from(relative)
    } else {
        root_canonical.join(relative)
    };
    if let Ok(p) = candidate.canonicalize() {
        if !p.starts_with(&root_canonical) {
            return Err(AppError::InvalidRequest(format!(
                "Path escapes project root: {relative}"
            )));
        }
        return Ok(candidate);
    }
    let mut p = root_canonical.clone();
    for comp in Path::new(relative).components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err(AppError::InvalidRequest(format!(
                    "Path escapes project root: {relative}"
                )));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !p.pop() {
                    return Err(AppError::InvalidRequest(format!(
                        "Path escapes project root: {relative}"
                    )));
                }
            }
            Component::Normal(c) => p.push(c),
        }
    }
    if !p.starts_with(&root_canonical) {
        return Err(AppError::InvalidRequest(format!(
            "Path escapes project root: {relative}"
        )));
    }
    Ok(candidate)
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
        let path = resolve_path(&self.root, rel)?;
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

        let mut entries = Vec::new();
        collect_entries(&path, &path, recursive, &mut entries).await?;
        Ok(json!({ "entries": entries }))
    }
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
        "Secondary code-search fallback: search file contents for a pattern (plain text or \
         regex), returning matching file paths with line numbers. Prefer `grep`/`rg` via \
         run_command to locate code; use this only when a shell grep isn't available or \
         convenient. Search the task's key terms plus close synonyms via regex alternation \
         (e.g. `login|signin|authenticate`), then read only the files that match."
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
            if name.starts_with('.') || name == "node_modules" || name == "target" {
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
