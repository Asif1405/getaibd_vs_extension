use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use super::embeddings::EmbeddingProvider;
use super::index_status::IndexStatus;
use super::indexer::MemoryIndexer;
use super::merkle::{content_hash, MerkleIndex};
use super::store::MemoryStore;

const DEBOUNCE_MS: u64 = 2000;
const MAX_FILE_SIZE: u64 = 100_000;

const IGNORE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "build",
    "__pycache__",
    ".next",
    ".cache",
    "vendor",
    ".cargo",
];

pub fn spawn_watcher(
    root: &Path,
    store: MemoryStore,
    embedder: Arc<dyn EmbeddingProvider>,
    merkle_path: Option<PathBuf>,
    status: Arc<IndexStatus>,
    max_entries: usize,
) -> Option<notify::RecommendedWatcher> {
    let pending: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let pending_clone = pending.clone();
    let root_owned = root.to_path_buf();
    // Open a dedicated merkle connection for live hash updates (best-effort:
    // failures just mean the file re-embeds on next startup, no correctness loss).
    let merkle = merkle_path.and_then(|p| MerkleIndex::open(&p).ok());

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        if let Ok(event) = res {
            let dominated_by_ignored = |p: &Path| {
                p.components()
                    .any(|c| IGNORE_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
            };

            for path in &event.paths {
                if dominated_by_ignored(path) {
                    continue;
                }
                if !is_indexable(path) {
                    continue;
                }
                // notify runs this callback on its OWN (non-Tokio) thread, so we must
                // NOT touch a Tokio runtime here. A std Mutex insert is cheap and
                // avoids the "no reactor running" panic that killed the watcher.
                if let Ok(mut set) = pending_clone.lock() {
                    set.insert(path.clone());
                }
            }
        }
    })
    .ok()?;

    if watcher.watch(root, RecursiveMode::Recursive).is_err() {
        tracing::warn!("Failed to start file watcher on {}", root.display());
        return None;
    }

    tracing::info!("File watcher started on {}", root.display());

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(DEBOUNCE_MS));
        loop {
            interval.tick().await;
            let paths: Vec<PathBuf> = match pending.lock() {
                Ok(mut set) => set.drain().collect(),
                Err(_) => continue,
            };

            if paths.is_empty() {
                continue;
            }

            // Don't write the embeddings DB onto a nearly-full disk — it can tip
            // the machine into a swap-thrash freeze. Drop this batch; a later edit
            // (once space is freed) re-triggers indexing for these files.
            if crate::disk::is_low(&root_owned) {
                tracing::warn!(
                    "Skipping re-index of {} file(s): low disk space",
                    paths.len()
                );
                continue;
            }

            let indexer = MemoryIndexer::new(&store, embedder.as_ref());
            let mut count = 0;
            for path in &paths {
                let Ok(meta) = std::fs::metadata(path) else {
                    // Deleted/renamed: drop from the store and merkle so it stops
                    // surfacing in search and isn't re-embedded on restart. Path form
                    // matches `indexer::index_file` / `merkle::collect_files` exactly.
                    let rel = path.strip_prefix(&root_owned).unwrap_or(path);
                    let rel_str = rel.to_string_lossy().to_string();
                    let _ = store.delete_by_source(&format!("file:{rel_str}"));
                    if let Some(m) = &merkle {
                        let _ = m.remove_file(&rel_str);
                    }
                    continue;
                };
                if meta.len() > MAX_FILE_SIZE {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(path) else {
                    continue;
                };
                let rel = path.strip_prefix(&root_owned).unwrap_or(path);
                if indexer.index_file(rel, &content).await.is_ok() {
                    count += 1;
                    if let Some(m) = &merkle {
                        let rel_str = rel.to_string_lossy().to_string();
                        let _ = m.set_file_hash(&rel_str, &content_hash(content.as_bytes()));
                    }
                }
            }
            if count > 0 {
                status.note_watcher(count);
                tracing::debug!("Re-indexed {count} changed file(s)");
                // Keep the on-disk index bounded: live edits can push the row count
                // over the configured cap between agent turns, so prune here too.
                let prune_store = store.clone();
                if let Ok(Ok(pruned)) =
                    tokio::task::spawn_blocking(move || prune_store.prune_oldest(max_entries)).await
                {
                    if pruned > 0 {
                        tracing::debug!("Pruned {pruned} memory entries over cap ({max_entries})");
                    }
                }
            }
        }
    });

    Some(watcher)
}

fn is_indexable(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "go"
            | "java"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "rb"
            | "swift"
            | "zig"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "md"
            | "txt"
    )
}
