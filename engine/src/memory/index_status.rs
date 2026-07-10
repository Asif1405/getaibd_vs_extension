//! Lightweight, lock-free snapshot of the codebase index state, shared across the
//! startup indexer, the file watcher, and the `/index/status` route so clients can
//! show whether the semantic index is warm and fresh.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

#[derive(Default)]
pub struct IndexStatus {
    /// True while the startup full-repo pass is running.
    indexing: AtomicBool,
    /// Files embedded on the last full pass.
    indexed_files: AtomicUsize,
    /// Files skipped (hash unchanged) on the last full pass.
    unchanged_files: AtomicUsize,
    /// Files pruned (deleted) on the last full pass.
    removed_files: AtomicUsize,
    /// Cumulative files re-indexed by the live watcher since startup.
    watcher_reindexed: AtomicUsize,
    /// Unix seconds of the last index activity (0 = never).
    last_updated_unix: AtomicI64,
}

impl IndexStatus {
    pub fn begin(&self) {
        self.indexing.store(true, Ordering::Relaxed);
    }

    pub fn finish(&self, indexed: usize, unchanged: usize, removed: usize) {
        self.indexed_files.store(indexed, Ordering::Relaxed);
        self.unchanged_files.store(unchanged, Ordering::Relaxed);
        self.removed_files.store(removed, Ordering::Relaxed);
        self.indexing.store(false, Ordering::Relaxed);
        self.touch();
    }

    /// Clear the in-progress flag without recording counts (error / early-out paths).
    pub fn abort(&self) {
        self.indexing.store(false, Ordering::Relaxed);
    }

    pub fn note_watcher(&self, count: usize) {
        self.watcher_reindexed.fetch_add(count, Ordering::Relaxed);
        self.touch();
    }

    fn touch(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.last_updated_unix.store(now, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "indexing": self.indexing.load(Ordering::Relaxed),
            "indexed_files": self.indexed_files.load(Ordering::Relaxed),
            "unchanged_files": self.unchanged_files.load(Ordering::Relaxed),
            "removed_files": self.removed_files.load(Ordering::Relaxed),
            "watcher_reindexed": self.watcher_reindexed.load(Ordering::Relaxed),
            "last_updated_unix": self.last_updated_unix.load(Ordering::Relaxed),
        })
    }
}
