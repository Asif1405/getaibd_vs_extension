use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use notify::{RecursiveMode, Watcher};

use super::embeddings::EmbeddingProvider;
use super::indexer::MemoryIndexer;
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
) -> Option<notify::RecommendedWatcher> {
    let pending: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let pending_clone = pending.clone();
    let root_owned = root.to_path_buf();

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
                let pending = pending_clone.clone();
                let p = path.clone();
                tokio::spawn(async move {
                    pending.lock().await.insert(p);
                });
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
            let paths: Vec<PathBuf> = {
                let mut set = pending.lock().await;
                let drained: Vec<PathBuf> = set.drain().collect();
                drained
            };

            if paths.is_empty() {
                continue;
            }

            let indexer = MemoryIndexer::new(&store, embedder.as_ref());
            let mut count = 0;
            for path in &paths {
                let Ok(meta) = std::fs::metadata(path) else {
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
                }
            }
            if count > 0 {
                tracing::debug!("Re-indexed {count} changed file(s)");
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
