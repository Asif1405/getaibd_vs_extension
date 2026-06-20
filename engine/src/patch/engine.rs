use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::AppError;
use crate::patch::diff::{EditOperation, EditResult, FileEdit, PatchResponse, PatchResult};
use crate::patch::prompt::extract_patch_json;

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
pub enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

pub fn parse_unified_diff(diff: &str) -> Result<Vec<Hunk>, AppError> {
    let mut hunks = Vec::new();
    let mut lines = diff.lines().peekable();

    while let Some(line) = lines.next() {
        if !line.starts_with("@@") {
            continue;
        }

        let header = line.split("@@").nth(1).unwrap_or("").trim();
        let (old_range, new_range) = parse_hunk_header(header)?;

        let mut hunk_lines = Vec::new();

        loop {
            match lines.peek() {
                Some(l) if l.starts_with("@@") || l.starts_with("diff ") => break,
                None => break,
                Some(_) => {}
            }
            let l = lines.next().unwrap();
            if l.starts_with('-') {
                hunk_lines.push(HunkLine::Remove(l[1..].to_string()));
            } else if l.starts_with('+') {
                hunk_lines.push(HunkLine::Add(l[1..].to_string()));
            } else if l.starts_with(' ') {
                hunk_lines.push(HunkLine::Context(l[1..].to_string()));
            } else if l.starts_with('\\') {
                continue;
            } else {
                hunk_lines.push(HunkLine::Context(l.to_string()));
            }
        }

        hunks.push(Hunk {
            old_start: old_range.0,
            old_count: old_range.1,
            new_start: new_range.0,
            new_count: new_range.1,
            lines: hunk_lines,
        });
    }

    Ok(hunks)
}

fn parse_hunk_header(header: &str) -> Result<((usize, usize), (usize, usize)), AppError> {
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(AppError::InvalidRequest(format!(
            "Invalid hunk header: {}",
            header
        )));
    }

    let old = parse_range(parts[0].trim_start_matches('-'))?;
    let new = parse_range(parts[1].trim_start_matches('+'))?;
    Ok((old, new))
}

fn parse_range(s: &str) -> Result<(usize, usize), AppError> {
    if let Some((start, count)) = s.split_once(',') {
        Ok((
            start
                .parse()
                .map_err(|_| AppError::InvalidRequest(format!("Bad range: {}", s)))?,
            count
                .parse()
                .map_err(|_| AppError::InvalidRequest(format!("Bad range: {}", s)))?,
        ))
    } else {
        let start = s
            .parse()
            .map_err(|_| AppError::InvalidRequest(format!("Bad range: {}", s)))?;
        Ok((start, 1))
    }
}

pub fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, AppError> {
    let original_lines: Vec<&str> = original.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut src_pos = 0usize;

    for hunk in hunks {
        let hunk_old_start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };

        if hunk_old_start > src_pos {
            for line in &original_lines[src_pos..hunk_old_start] {
                result.push(line.to_string());
            }
            src_pos = hunk_old_start;
        }

        for hunk_line in &hunk.lines {
            match hunk_line {
                HunkLine::Context(l) => {
                    if src_pos < original_lines.len() {
                        let actual = original_lines[src_pos];
                        if actual != l.as_str() {
                            return Err(AppError::InvalidRequest(format!(
                                "Context mismatch at line {}: expected {:?}, got {:?}",
                                src_pos + 1,
                                l,
                                actual
                            )));
                        }
                        result.push(l.clone());
                        src_pos += 1;
                    }
                }
                HunkLine::Remove(l) => {
                    if src_pos < original_lines.len() {
                        let actual = original_lines[src_pos];
                        if actual != l.as_str() {
                            return Err(AppError::InvalidRequest(format!(
                                "Remove mismatch at line {}: expected {:?}, got {:?}",
                                src_pos + 1,
                                l,
                                actual
                            )));
                        }
                        src_pos += 1;
                    }
                }
                HunkLine::Add(l) => {
                    result.push(l.clone());
                }
            }
        }
    }

    for line in &original_lines[src_pos..] {
        result.push(line.to_string());
    }

    let mut out = result.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

pub fn validate_diff(original: &str, diff: &str) -> Result<(), AppError> {
    let hunks = parse_unified_diff(diff)?;
    if hunks.is_empty() {
        return Err(AppError::InvalidRequest("Diff contains no hunks".into()));
    }
    apply_hunks(original, &hunks)?;
    Ok(())
}

pub struct PatchEngine {
    project_root: PathBuf,
}

impl PatchEngine {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }

    pub fn parse_llm_response(&self, response: &str) -> Result<PatchResponse, AppError> {
        let json_str = extract_patch_json(response).ok_or_else(|| {
            AppError::InvalidRequest("No valid JSON patch found in LLM response".into())
        })?;

        serde_json::from_str::<PatchResponse>(json_str)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to parse patch JSON: {}", e)))
    }

    pub async fn preview(&self, patch: &PatchResponse) -> Vec<EditPreview> {
        let mut previews = Vec::new();

        for edit in &patch.edits {
            let path = self.project_root.join(&edit.file);
            let preview = match &edit.operation {
                EditOperation::Create => EditPreview {
                    file: edit.file.clone(),
                    operation: "create".into(),
                    before: None,
                    after: edit.content.clone(),
                    valid: edit.content.is_some(),
                    error: None,
                },
                EditOperation::Delete => EditPreview {
                    file: edit.file.clone(),
                    operation: "delete".into(),
                    before: fs::read_to_string(&path).await.ok(),
                    after: None,
                    valid: path.exists(),
                    error: None,
                },
                EditOperation::Rename { to } => EditPreview {
                    file: edit.file.clone(),
                    operation: format!("rename → {}", to),
                    before: None,
                    after: None,
                    valid: path.exists(),
                    error: None,
                },
                EditOperation::Modify => match fs::read_to_string(&path).await {
                    Err(e) => EditPreview {
                        file: edit.file.clone(),
                        operation: "modify".into(),
                        before: None,
                        after: None,
                        valid: false,
                        error: Some(format!("Cannot read file: {}", e)),
                    },
                    Ok(original) => match edit.diff.as_deref() {
                        None => EditPreview {
                            file: edit.file.clone(),
                            operation: "modify".into(),
                            before: Some(original),
                            after: None,
                            valid: false,
                            error: Some("No diff provided for modify operation".into()),
                        },
                        Some(diff) => match apply_hunks_preview(&original, diff) {
                            Err(e) => EditPreview {
                                file: edit.file.clone(),
                                operation: "modify".into(),
                                before: Some(original),
                                after: None,
                                valid: false,
                                error: Some(e.to_string()),
                            },
                            Ok(after) => EditPreview {
                                file: edit.file.clone(),
                                operation: "modify".into(),
                                before: Some(original),
                                after: Some(after),
                                valid: true,
                                error: None,
                            },
                        },
                    },
                },
            };
            previews.push(preview);
        }

        previews
    }

    pub async fn apply(&self, patch: &PatchResponse) -> PatchResult {
        let mut applied = Vec::new();
        let mut failed = Vec::new();
        let mut snapshots: HashMap<String, Option<String>> = HashMap::new();

        for edit in &patch.edits {
            let result = self.apply_single(edit, &mut snapshots).await;
            if result.success {
                applied.push(result);
            } else {
                failed.push(result);
                break;
            }
        }

        let rollback_available = !snapshots.is_empty();

        PatchResult {
            success: failed.is_empty(),
            applied,
            failed,
            rollback_available,
        }
    }

    pub async fn apply_with_rollback(
        &self,
        patch: &PatchResponse,
    ) -> (PatchResult, Option<Snapshot>) {
        let mut snapshots: HashMap<String, Option<String>> = HashMap::new();
        let mut applied = Vec::new();
        let mut failed = Vec::new();

        for edit in &patch.edits {
            let result = self.apply_single(edit, &mut snapshots).await;
            if result.success {
                applied.push(result);
            } else {
                failed.push(result);
                break;
            }
        }

        let snapshot = if !snapshots.is_empty() {
            Some(Snapshot { files: snapshots })
        } else {
            None
        };

        let result = PatchResult {
            success: failed.is_empty(),
            applied,
            failed,
            rollback_available: snapshot.is_some(),
        };

        (result, snapshot)
    }

    async fn apply_single(
        &self,
        edit: &FileEdit,
        snapshots: &mut HashMap<String, Option<String>>,
    ) -> EditResult {
        let path = self.project_root.join(&edit.file);

        let original = fs::read_to_string(&path).await.ok();
        snapshots.insert(edit.file.clone(), original.clone());

        match &edit.operation {
            EditOperation::Create => {
                if let Some(content) = &edit.content {
                    if let Some(parent) = path.parent() {
                        if let Err(e) = fs::create_dir_all(parent).await {
                            return EditResult {
                                file: edit.file.clone(),
                                operation: edit.operation.clone(),
                                success: false,
                                error: Some(e.to_string()),
                                original_content: None,
                            };
                        }
                    }
                    match fs::write(&path, content).await {
                        Ok(_) => EditResult {
                            file: edit.file.clone(),
                            operation: edit.operation.clone(),
                            success: true,
                            error: None,
                            original_content: None,
                        },
                        Err(e) => EditResult {
                            file: edit.file.clone(),
                            operation: edit.operation.clone(),
                            success: false,
                            error: Some(e.to_string()),
                            original_content: None,
                        },
                    }
                } else {
                    EditResult {
                        file: edit.file.clone(),
                        operation: edit.operation.clone(),
                        success: false,
                        error: Some("Create operation requires content".into()),
                        original_content: None,
                    }
                }
            }

            EditOperation::Delete => match fs::remove_file(&path).await {
                Ok(_) => EditResult {
                    file: edit.file.clone(),
                    operation: edit.operation.clone(),
                    success: true,
                    error: None,
                    original_content: original,
                },
                Err(e) => EditResult {
                    file: edit.file.clone(),
                    operation: edit.operation.clone(),
                    success: false,
                    error: Some(e.to_string()),
                    original_content: None,
                },
            },

            EditOperation::Rename { to } => {
                let dest = self.project_root.join(to);
                match fs::rename(&path, &dest).await {
                    Ok(_) => EditResult {
                        file: edit.file.clone(),
                        operation: edit.operation.clone(),
                        success: true,
                        error: None,
                        original_content: original,
                    },
                    Err(e) => EditResult {
                        file: edit.file.clone(),
                        operation: edit.operation.clone(),
                        success: false,
                        error: Some(e.to_string()),
                        original_content: None,
                    },
                }
            }

            EditOperation::Modify => {
                let Some(original_content) = original else {
                    return EditResult {
                        file: edit.file.clone(),
                        operation: edit.operation.clone(),
                        success: false,
                        error: Some(format!("File not found: {}", edit.file)),
                        original_content: None,
                    };
                };

                let Some(diff) = &edit.diff else {
                    return EditResult {
                        file: edit.file.clone(),
                        operation: edit.operation.clone(),
                        success: false,
                        error: Some("Modify operation requires a diff".into()),
                        original_content: Some(original_content),
                    };
                };

                match apply_hunks_preview(&original_content, diff) {
                    Err(e) => EditResult {
                        file: edit.file.clone(),
                        operation: edit.operation.clone(),
                        success: false,
                        error: Some(e.to_string()),
                        original_content: Some(original_content),
                    },
                    Ok(new_content) => match fs::write(&path, &new_content).await {
                        Ok(_) => EditResult {
                            file: edit.file.clone(),
                            operation: edit.operation.clone(),
                            success: true,
                            error: None,
                            original_content: Some(original_content),
                        },
                        Err(e) => EditResult {
                            file: edit.file.clone(),
                            operation: edit.operation.clone(),
                            success: false,
                            error: Some(e.to_string()),
                            original_content: Some(original_content),
                        },
                    },
                }
            }
        }
    }
}

fn apply_hunks_preview(original: &str, diff: &str) -> Result<String, AppError> {
    let hunks = parse_unified_diff(diff)?;
    apply_hunks(original, &hunks)
}

pub struct Snapshot {
    pub files: HashMap<String, Option<String>>,
}

impl Snapshot {
    pub async fn rollback(&self, project_root: &Path) -> Vec<(String, Result<(), String>)> {
        let mut results = Vec::new();
        for (file, content) in &self.files {
            let path = project_root.join(file);
            let result = match content {
                None => fs::remove_file(&path).await.map_err(|e| e.to_string()),
                Some(c) => fs::write(&path, c).await.map_err(|e| e.to_string()),
            };
            results.push((file.clone(), result));
        }
        results
    }
}

#[derive(Debug, Clone)]
pub struct EditPreview {
    pub file: String,
    pub operation: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub valid: bool,
    pub error: Option<String>,
}
