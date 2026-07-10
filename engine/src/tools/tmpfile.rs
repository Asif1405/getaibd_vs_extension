//! Spill large, externally-sourced text (diffs, PRs, fetched pages) to a temp
//! file under the project instead of dumping it into the model's context.
//!
//! Tools that pull data from "outside" — `web_fetch`, `git_diff`, `git_show`,
//! `git_blame` — write the full payload to `.getaibd/tmp/` and hand back a path
//! plus a small preview, so the agent reads only the ranges it needs (via
//! `read_file`/`search_code`) rather than memorizing the whole blob.

use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Project-relative temp directory (reachable by `read_file`, which is rooted at
/// the project).
pub const TMP_DIR: &str = ".getaibd/tmp";

/// Payloads at or below this size are still echoed inline (a temp copy is written
/// too) — a round-trip `read_file` would cost more than it saves. Anything larger
/// is returned as path + preview only.
const INLINE_LIMIT: usize = 6_000;

/// How many leading lines to show as a preview for large payloads.
const PREVIEW_LINES: usize = 60;

/// Delete spilled temp files older than `max_age`. These are agent scratch
/// artifacts (diffs, fetched pages) that are never cleaned up otherwise, so on a
/// long-lived session `.getaibd/tmp/` would grow without bound. Best-effort: any
/// I/O error on an individual file is ignored. Returns the number removed.
pub async fn cleanup_stale(root: &Path, max_age: Duration) -> usize {
    let dir = root.join(TMP_DIR);
    let mut removed = 0;
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return 0;
    };
    let now = SystemTime::now();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        // Preserve the `.gitignore` marker we write into the dir.
        if path.file_name().and_then(|n| n.to_str()) == Some(".gitignore") {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
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

/// Persist `content` to `.getaibd/tmp/<kind>-<ts>.<ext>` and return a JSON
/// descriptor. Small payloads keep an inline `content` copy; large ones return
/// `saved_to` + `preview` and a note steering the agent to read from disk.
pub async fn stash(root: &Path, kind: &str, ext: &str, content: &str) -> Value {
    let lines = content.lines().count();
    let bytes = content.len();

    let dir = root.join(TMP_DIR);
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return json!({ "content": content, "lines": lines, "bytes": bytes });
    }
    // Keep spilled temp files out of version control.
    let gitignore = dir.join(".gitignore");
    if tokio::fs::metadata(&gitignore).await.is_err() {
        let _ = tokio::fs::write(&gitignore, "*\n").await;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let file_name = format!("{kind}-{ts}.{ext}");
    let abs = dir.join(&file_name);
    let rel = format!("{TMP_DIR}/{file_name}");

    if tokio::fs::write(&abs, content).await.is_err() {
        // Read-only workspace or low disk — fall back to inline so the caller
        // still gets the data.
        return json!({ "content": content, "lines": lines, "bytes": bytes });
    }

    if content.chars().count() <= INLINE_LIMIT {
        json!({
            "saved_to": rel,
            "lines": lines,
            "bytes": bytes,
            "content": content,
        })
    } else {
        let preview: String = content
            .lines()
            .take(PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        json!({
            "saved_to": rel,
            "lines": lines,
            "bytes": bytes,
            "preview": preview,
            "note": format!(
                "Full content ({lines} lines) saved to {rel} to keep it out of context. \
                 Read specific ranges with read_file (offset/limit) or search it with \
                 search_code/search_files on that path — do not rely on memory of it."
            ),
        })
    }
}
