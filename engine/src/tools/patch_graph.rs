//! `patch_graph` — a deterministic structural map of the current uncommitted
//! changes.
//!
//! Instead of handing the model a massive raw `git diff` (where it hallucinates
//! gaps and false positives), this parses the diff, maps each changed hunk to the
//! exact symbol it touches via tree-sitter, and attaches reference evidence (who
//! calls each changed symbol) via ripgrep. The result is a small, factual graph of
//! "what changed and what it affects" that the model can reason about directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::lsp::{regex_escape, ripgrep};
use super::Tool;
use crate::error::AppError;
use crate::memory::chunker::{symbol_ranges, SymbolRange};

/// Cap the number of changed symbols we resolve references for, so a huge diff
/// can't fan out into thousands of ripgrep calls.
const MAX_SYMBOLS_RESOLVED: usize = 40;
/// Sample of reference sites reported per changed symbol.
const MAX_REFS_PER_SYMBOL: usize = 12;

pub struct PatchGraph {
    root: Arc<PathBuf>,
}

impl PatchGraph {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for PatchGraph {
    fn name(&self) -> &'static str {
        "patch_graph"
    }

    fn description(&self) -> &'static str {
        "Get a deterministic STRUCTURAL map of the current uncommitted changes instead of \
         reading a huge raw diff. Returns, per file, its status and the exact symbols \
         (functions/classes/methods) that were touched, each with the line range and a sample \
         of who references it across the repo. Prefer this over `git_diff` when reasoning about \
         what changed and its blast radius."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": {
                    "type": "boolean",
                    "description": "Only staged changes (default false: all uncommitted changes vs HEAD)."
                }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let staged = input.get("staged").and_then(Value::as_bool).unwrap_or(false);

        let statuses = git_name_status(&self.root, staged).await;
        let hunks = git_hunks(&self.root, staged).await;

        if statuses.is_empty() && hunks.is_empty() {
            return Ok(json!({
                "base": "HEAD",
                "staged": staged,
                "files": [],
                "summary": { "files": 0, "changed_symbols": 0 },
                "note": "No uncommitted changes."
            }));
        }

        let mut files = Vec::new();
        let mut total_symbols = 0usize;
        let mut resolved = 0usize;

        // Union of paths from both sources, stable order.
        let mut paths: Vec<String> = statuses.keys().cloned().collect();
        for p in hunks.keys() {
            if !paths.contains(p) {
                paths.push(p.clone());
            }
        }
        paths.sort();

        for path in paths {
            let status = statuses.get(&path).cloned().unwrap_or_else(|| "M".into());
            let file_hunks = hunks.get(&path).cloned().unwrap_or_default();

            if status == "D" {
                files.push(json!({ "path": path, "status": "deleted", "changed_symbols": [] }));
                continue;
            }

            let content = tokio::fs::read_to_string(self.root.join(&path))
                .await
                .unwrap_or_default();
            let ranges = symbol_ranges(&content, Path::new(&path));

            // Innermost symbol touched by each hunk (dedup, source order).
            let mut touched: Vec<SymbolRange> = Vec::new();
            for (start, len) in &file_hunks {
                let (hs, he) = if *len == 0 {
                    ((*start).max(1), (*start).max(1))
                } else {
                    (*start, start + len - 1)
                };
                if let Some(sym) = innermost(&ranges, hs, he) {
                    if !touched
                        .iter()
                        .any(|t| t.name == sym.name && t.start_line == sym.start_line)
                    {
                        touched.push(sym.clone());
                    }
                }
            }

            let mut changed_symbols = Vec::new();
            for sym in &touched {
                total_symbols += 1;
                let refs = if resolved < MAX_SYMBOLS_RESOLVED {
                    resolved += 1;
                    references_for(&self.root, &sym.name, &path, sym.start_line).await
                } else {
                    Value::Null
                };
                changed_symbols.push(json!({
                    "name": sym.name,
                    "kind": sym.kind,
                    "lines": format!("{}-{}", sym.start_line, sym.end_line),
                    "references": refs,
                }));
            }

            let status_str = match status.chars().next() {
                Some('A') => "added",
                Some('R') => "renamed",
                _ => "modified",
            };
            let mut file_obj = json!({
                "path": path,
                "status": status_str,
                "changed_symbols": changed_symbols,
            });
            // A code file whose hunks fell outside any symbol (imports, top-level).
            if touched.is_empty() && !file_hunks.is_empty() {
                file_obj["note"] = json!("Changes are outside any named symbol (imports / top-level / non-code).");
            }
            files.push(file_obj);
        }

        Ok(json!({
            "base": "HEAD",
            "staged": staged,
            "files": files,
            "summary": {
                "files": files.len(),
                "changed_symbols": total_symbols,
            }
        }))
    }
}

/// Pick the innermost (smallest-span) symbol whose range overlaps `[start, end]`.
pub(crate) fn innermost(ranges: &[SymbolRange], start: usize, end: usize) -> Option<&SymbolRange> {
    ranges
        .iter()
        .filter(|r| r.overlaps(start, end))
        .min_by_key(|r| r.span())
}

/// References to `name` across the repo (word-boundary), minus the definition line
/// itself. Returns `{ count, truncated, sample: [{path, line, text}] }`.
pub(crate) async fn references_for(
    root: &Path,
    name: &str,
    def_path: &str,
    def_line: usize,
) -> Value {
    let pattern = format!(r"\b{}\b", regex_escape(name));
    let matches = ripgrep(root, &pattern).await;
    let usages: Vec<_> = matches
        .into_iter()
        .filter(|(p, line, _)| !(p == def_path && *line == def_line))
        .collect();
    let sample: Vec<Value> = usages
        .iter()
        .take(MAX_REFS_PER_SYMBOL)
        .map(|(p, line, text)| json!({ "path": p, "line": line, "text": text }))
        .collect();
    json!({
        "count": usages.len(),
        "truncated": usages.len() > MAX_REFS_PER_SYMBOL,
        "sample": sample,
        "source": "ripgrep",
    })
}

async fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// `path -> status letter` (A/M/D/R…) for uncommitted changes.
async fn git_name_status(root: &Path, staged: bool) -> BTreeMap<String, String> {
    let mut args = vec!["diff", "--name-status"];
    if staged {
        args.push("--staged");
    } else {
        args.push("HEAD");
    }
    let mut out = BTreeMap::new();
    let Some(text) = run_git(root, &args).await else {
        return out;
    };
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else { continue };
        // Renames list old\tnew; key on the new path.
        let path = parts.last().unwrap_or("").to_string();
        if !path.is_empty() {
            out.insert(path, status.to_string());
        }
    }
    out
}

/// `path -> [(new_start_line, new_len)]` from a zero-context diff.
async fn git_hunks(root: &Path, staged: bool) -> BTreeMap<String, Vec<(usize, usize)>> {
    let mut args = vec!["diff", "-U0"];
    if staged {
        args.push("--staged");
    } else {
        args.push("HEAD");
    }
    let mut out: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let Some(text) = run_git(root, &args).await else {
        return out;
    };
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            current = if rest == "/dev/null" {
                None
            } else {
                Some(rest.trim_start_matches("b/").to_string())
            };
        } else if line.starts_with("@@ ") {
            if let (Some(path), Some(hunk)) = (current.as_ref(), parse_hunk(line)) {
                out.entry(path.clone()).or_default().push(hunk);
            }
        }
    }
    out
}

/// Parse `@@ -a,b +c,d @@` → new side `(c, d)` (d defaults to 1).
fn parse_hunk(line: &str) -> Option<(usize, usize)> {
    let plus = line.split_whitespace().find(|t| t.starts_with('+'))?;
    let spec = plus.trim_start_matches('+');
    let mut it = spec.split(',');
    let start: usize = it.next()?.parse().ok()?;
    let len: usize = match it.next() {
        Some(l) => l.parse().ok()?,
        None => 1,
    };
    Some((start, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunk_headers() {
        assert_eq!(parse_hunk("@@ -1,2 +3,4 @@"), Some((3, 4)));
        assert_eq!(parse_hunk("@@ -1 +5 @@"), Some((5, 1)));
        assert_eq!(parse_hunk("@@ -1,0 +5,0 @@ fn foo"), Some((5, 0)));
        assert_eq!(parse_hunk("garbage"), None);
    }

    #[test]
    fn innermost_prefers_smallest_span() {
        let ranges = vec![
            SymbolRange { name: "Big".into(), kind: "class".into(), start_line: 1, end_line: 100 },
            SymbolRange { name: "method".into(), kind: "fn".into(), start_line: 10, end_line: 20 },
        ];
        let sym = innermost(&ranges, 12, 12).unwrap();
        assert_eq!(sym.name, "method");
    }
}
