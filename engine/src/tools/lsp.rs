//! Structural code-navigation tools (`find_symbol`, `find_references`,
//! `document_symbols`).
//!
//! When the editor is attached these calls are delegated to its language servers
//! for precise, semantics-aware results (see `runtime::delegate_editor`). The
//! `execute` bodies here are the headless fallback used when no editor is present
//! (CLI runs, tests): ripgrep for cross-file lookups and the tree-sitter chunker
//! for per-file outlines. Both paths return the same JSON shape so the model can't
//! tell which served the request.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::Tool;
use crate::error::AppError;

const MAX_MATCHES: usize = 100;

pub(crate) fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Run ripgrep in `root` and return `(rel_path, line, text)` matches. Empty when
/// ripgrep is missing or errors — callers surface a note in that case.
pub(crate) async fn ripgrep(root: &Path, pattern: &str) -> Vec<(String, usize, String)> {
    let output = tokio::process::Command::new("rg")
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color")
        .arg("never")
        .arg("--max-columns")
        .arg("300")
        .arg("-e")
        .arg(pattern)
        .arg(".")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .await;

    let Ok(output) = output else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();
    for line in stdout.lines() {
        // format: path:line:text
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(line_no), Some(text)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(line_no) = line_no.parse::<usize>() else {
            continue;
        };
        let rel = path.strip_prefix("./").unwrap_or(path).to_string();
        matches.push((rel, line_no, text.trim().to_string()));
        if matches.len() >= MAX_MATCHES {
            break;
        }
    }
    matches
}

fn matches_json(matches: &[(String, usize, String)]) -> Value {
    json!(matches
        .iter()
        .map(|(path, line, text)| json!({ "path": path, "line": line, "text": text }))
        .collect::<Vec<_>>())
}

/// `find_symbol` — where a function/type/etc. is defined.
pub struct FindSymbol {
    root: Arc<PathBuf>,
}

impl FindSymbol {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for FindSymbol {
    fn name(&self) -> &'static str {
        "find_symbol"
    }

    fn description(&self) -> &'static str {
        "Find where a symbol (function, class, struct, type, etc.) is DEFINED across the \
         workspace. Prefer this over reading whole files to locate a definition. Returns \
         file path, line, and the matching line of text."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Exact symbol name to locate the definition of."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let query = input
            .get("query")
            .or_else(|| input.get("symbol"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(AppError::InvalidRequest("query is required".into()));
        }
        let kws = "fn|def|func|function|class|struct|enum|trait|interface|type|impl|const|var|let|module|mod|package|record";
        let pattern = format!(r"\b(?:{kws})\s+{}\b", regex_escape(&query));
        let matches = ripgrep(&self.root, &pattern).await;
        Ok(json!({
            "symbol": query,
            "definitions": matches_json(&matches),
            "count": matches.len(),
            "source": "grep",
        }))
    }
}

/// `find_references` — where a symbol is used.
pub struct FindReferences {
    root: Arc<PathBuf>,
}

impl FindReferences {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for FindReferences {
    fn name(&self) -> &'static str {
        "find_references"
    }

    fn description(&self) -> &'static str {
        "Find all references / usages of a symbol across the workspace before you rename or \
         change it. Returns file path, line, and the matching line of text (capped)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "Exact symbol name to find usages of."
                }
            },
            "required": ["symbol"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let symbol = input
            .get("symbol")
            .or_else(|| input.get("query"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if symbol.is_empty() {
            return Err(AppError::InvalidRequest("symbol is required".into()));
        }
        let pattern = format!(r"\b{}\b", regex_escape(&symbol));
        let matches = ripgrep(&self.root, &pattern).await;
        let truncated = matches.len() >= MAX_MATCHES;
        Ok(json!({
            "symbol": symbol,
            "references": matches_json(&matches),
            "count": matches.len(),
            "truncated": truncated,
            "source": "grep",
        }))
    }
}

/// `document_symbols` — outline of a single file.
pub struct DocumentSymbols {
    root: Arc<PathBuf>,
}

impl DocumentSymbols {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for DocumentSymbols {
    fn name(&self) -> &'static str {
        "document_symbols"
    }

    fn description(&self) -> &'static str {
        "List the top-level symbols (functions, classes, types, etc.) defined in a file, with \
         their line numbers. Use this to understand a file's structure without reading all of it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file, relative to the workspace root."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let rel = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if rel.is_empty() {
            return Err(AppError::InvalidRequest("path is required".into()));
        }
        let full = if Path::new(&rel).is_absolute() {
            PathBuf::from(&rel)
        } else {
            self.root.join(&rel)
        };
        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("cannot read {rel}: {e}")))?;
        let symbols = crate::memory::chunker::symbol_outline(&content, &full);
        Ok(json!({
            "path": rel,
            "symbols": symbols
                .iter()
                .map(|s| json!({ "name": s.name, "kind": s.kind, "line": s.line }))
                .collect::<Vec<_>>(),
            "count": symbols.len(),
            "source": "tree-sitter",
        }))
    }
}
