//! Turn an arbitrary unified diff (a fetched PR/commit diff) into a compact
//! structural graph so the model reviews a "what changed and what it touches"
//! map instead of a huge raw diff.
//!
//! Unlike `patch_graph` (which inspects the local working tree via `git diff`),
//! this works on diff TEXT that may describe a branch not checked out. It is
//! hybrid:
//!   * Always derives, per file: status, hunks, and the enclosing symbol — parsed
//!     straight from the diff (hunk headers include the enclosing definition), so
//!     it works with no checkout.
//!   * When the changed file exists locally, it upgrades to tree-sitter symbol
//!     resolution and attaches ripgrep reference/blast-radius evidence.

use std::path::Path;

use serde_json::{json, Value};

use super::patch_graph::{innermost, references_for};
use crate::memory::chunker::symbol_ranges;

/// Cap files/symbols so a giant PR can't fan out into thousands of ripgrep calls.
const MAX_FILES: usize = 80;
const MAX_SYMBOLS_RESOLVED: usize = 40;

#[derive(Default)]
struct FileDiff {
    path: String,
    status: &'static str,
    /// (new_start_line, new_len, enclosing-context string from the hunk header)
    hunks: Vec<(usize, usize, String)>,
}

/// Build a structural graph JSON from a unified diff. Never fails: on unparseable
/// input it returns an empty graph, so callers can attach it unconditionally.
pub async fn build_from_diff(root: &Path, diff: &str) -> Value {
    let files = parse_diff(diff);
    if files.is_empty() {
        return json!({ "source": "diff-graph", "files": [], "summary": { "files": 0, "changed_symbols": 0 } });
    }

    let mut out_files = Vec::new();
    let mut total_symbols = 0usize;
    let mut resolved = 0usize;

    for file in files.into_iter().take(MAX_FILES) {
        if file.status == "deleted" {
            out_files.push(json!({ "path": file.path, "status": "deleted", "changed_symbols": [] }));
            continue;
        }

        let abs = root.join(&file.path);
        let local = tokio::fs::read_to_string(&abs).await.ok();

        let mut changed_symbols = Vec::new();
        if let Some(content) = local.as_deref() {
            // Local file present: tree-sitter symbols + ripgrep references (accurate).
            let ranges = symbol_ranges(content, Path::new(&file.path));
            let mut seen: Vec<(String, usize)> = Vec::new();
            for (start, len, _) in &file.hunks {
                let (hs, he) = if *len == 0 {
                    ((*start).max(1), (*start).max(1))
                } else {
                    (*start, start + len - 1)
                };
                let Some(sym) = innermost(&ranges, hs, he) else {
                    continue;
                };
                if seen.iter().any(|(n, l)| n == &sym.name && *l == sym.start_line) {
                    continue;
                }
                seen.push((sym.name.clone(), sym.start_line));
                total_symbols += 1;
                let refs = if resolved < MAX_SYMBOLS_RESOLVED {
                    resolved += 1;
                    references_for(root, &sym.name, &file.path, sym.start_line).await
                } else {
                    Value::Null
                };
                changed_symbols.push(json!({
                    "name": sym.name,
                    "kind": sym.kind,
                    "lines": format!("{}-{}", sym.start_line, sym.end_line),
                    "references": refs,
                    "from": "tree-sitter",
                }));
            }
        } else {
            // No local file: derive the enclosing symbol from the hunk-header context.
            let mut seen: Vec<String> = Vec::new();
            for (start, len, ctx) in &file.hunks {
                let name = symbol_from_context(ctx).unwrap_or_else(|| "(top-level / non-symbol)".into());
                if seen.contains(&name) {
                    continue;
                }
                seen.push(name.clone());
                total_symbols += 1;
                let end = start + len.saturating_sub(1);
                changed_symbols.push(json!({
                    "name": name,
                    "kind": "context",
                    "lines": format!("{}-{}", start, end.max(*start)),
                    "references": Value::Null,
                    "from": "hunk-header",
                }));
            }
        }

        out_files.push(json!({
            "path": file.path,
            "status": file.status,
            "local": local.is_some(),
            "changed_symbols": changed_symbols,
        }));
    }

    json!({
        "source": "diff-graph",
        "files": out_files,
        "summary": { "files": out_files.len(), "changed_symbols": total_symbols },
        "note": "Structural map of the diff: each file -> the symbols its hunks touch \
                 (with references/blast-radius when the file is local). Review from this graph, \
                 then read specific ranges of the saved diff or the base file as needed.",
    })
}

/// Parse a unified diff (`diff --git` style) into per-file hunk records.
fn parse_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut f = FileDiff { status: "modified", ..Default::default() };
            // `a/path b/path` — take the b/ side as the path.
            if let Some(b) = rest.split(" b/").nth(1) {
                f.path = b.to_string();
            } else if let Some(a) = rest.strip_prefix("a/") {
                f.path = a.split_whitespace().next().unwrap_or("").to_string();
            }
            files.push(f);
        } else if line.starts_with("new file mode") {
            if let Some(f) = files.last_mut() {
                f.status = "added";
            }
        } else if line.starts_with("deleted file mode") {
            if let Some(f) = files.last_mut() {
                f.status = "deleted";
            }
        } else if let Some(to) = line.strip_prefix("rename to ") {
            if let Some(f) = files.last_mut() {
                f.status = "renamed";
                f.path = to.to_string();
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(f) = files.last_mut() {
                if rest == "/dev/null" {
                    f.status = "deleted";
                } else {
                    f.path = rest.trim_start_matches("b/").to_string();
                }
            }
        } else if line.starts_with("--- ") {
            if line == "--- /dev/null" {
                if let Some(f) = files.last_mut() {
                    f.status = "added";
                }
            }
        } else if line.starts_with("@@ ") {
            if let (Some(f), Some((start, len, ctx))) = (files.last_mut(), parse_hunk(line)) {
                f.hunks.push((start, len, ctx));
            }
        }
    }
    // Drop pure-metadata entries with no path.
    files.retain(|f| !f.path.is_empty());
    files
}

/// Parse `@@ -a,b +c,d @@ context` -> (new_start, new_len, context-after-@@).
fn parse_hunk(line: &str) -> Option<(usize, usize, String)> {
    let after = line.strip_prefix("@@ ")?;
    let close = after.find("@@")?;
    let spec = &after[..close];
    let ctx = after[close + 2..].trim().to_string();
    let plus = spec.split_whitespace().find(|t| t.starts_with('+'))?;
    let nums = plus.trim_start_matches('+');
    let mut it = nums.split(',');
    let start: usize = it.next()?.parse().ok()?;
    let len: usize = match it.next() {
        Some(l) => l.parse().ok()?,
        None => 1,
    };
    Some((start, len, ctx))
}

/// Best-effort symbol name from a hunk-header context (e.g. `def foo(...)`,
/// `fn bar()`, `class Baz:`), used when the file isn't available locally.
fn symbol_from_context(ctx: &str) -> Option<String> {
    if ctx.is_empty() {
        return None;
    }
    const KEYWORDS: &[&str] = &[
        "def", "fn", "func", "function", "class", "struct", "enum", "impl", "trait",
        "interface", "type", "const", "let", "var", "public", "private", "static",
    ];
    let tokens: Vec<&str> = ctx.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| !t.is_empty()).collect();
    // Prefer the identifier right after a definition keyword.
    for w in tokens.windows(2) {
        if KEYWORDS.contains(&w[0]) && !KEYWORDS.contains(&w[1]) {
            return Some(w[1].to_string());
        }
    }
    // Otherwise the first identifier-like token.
    tokens
        .into_iter()
        .find(|t| t.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_'))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/foo.py b/src/foo.py\n\
index 111..222 100644\n\
--- a/src/foo.py\n\
+++ b/src/foo.py\n\
@@ -10,3 +10,4 @@ def process(task_id):\n\
     pass\n\
diff --git a/src/new.py b/src/new.py\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/src/new.py\n\
@@ -0,0 +1,5 @@\n\
+print('hi')\n\
diff --git a/src/gone.py b/src/gone.py\n\
deleted file mode 100644\n\
--- a/src/gone.py\n\
+++ /dev/null\n\
@@ -1,3 +0,0 @@\n";

    #[test]
    fn parses_files_statuses_and_hunks() {
        let files = parse_diff(SAMPLE);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/foo.py");
        assert_eq!(files[0].status, "modified");
        assert_eq!(files[0].hunks[0].0, 10);
        assert_eq!(files[0].hunks[0].2, "def process(task_id):");
        assert_eq!(files[1].path, "src/new.py");
        assert_eq!(files[1].status, "added");
        assert_eq!(files[2].path, "src/gone.py");
        assert_eq!(files[2].status, "deleted");
    }

    #[test]
    fn extracts_symbol_from_context() {
        assert_eq!(symbol_from_context("def process(task_id):"), Some("process".into()));
        assert_eq!(symbol_from_context("fn build_graph() {"), Some("build_graph".into()));
        assert_eq!(symbol_from_context("class Foo:"), Some("Foo".into()));
        assert_eq!(symbol_from_context(""), None);
    }

    // No local files (temp root) -> graph still built from the diff alone, deleted
    // files carry no symbols, added/modified derive symbols from hunk context.
    #[tokio::test]
    async fn builds_graph_without_checkout() {
        let root = std::env::temp_dir().join("diff-graph-nonexistent-xyz");
        let graph = build_from_diff(&root, SAMPLE).await;
        assert_eq!(graph["source"], "diff-graph");
        let files = graph["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        let foo = &files[0];
        assert_eq!(foo["local"], false);
        assert_eq!(foo["changed_symbols"][0]["name"], "process");
        assert_eq!(foo["changed_symbols"][0]["from"], "hunk-header");
    }
}
