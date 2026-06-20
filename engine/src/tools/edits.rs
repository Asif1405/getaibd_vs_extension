use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Tracks the original content of files the first time they're edited during a
/// task. Lets `git_diff` show a real diff even when the workspace isn't a git
/// repository (the agent relies on `git_diff` to verify its own edits).
#[derive(Clone, Default)]
pub struct EditTracker {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl EditTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores the pre-edit content for `path` (only the first time it's seen).
    pub fn record_baseline(&self, path: &str, original: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.entry(path.to_string())
                .or_insert_with(|| original.to_string());
        }
    }

    /// Returns the captured `(path, baseline)` pairs, sorted by path.
    pub fn snapshot(&self) -> Vec<(String, String)> {
        let Ok(map) = self.inner.lock() else {
            return Vec::new();
        };
        let mut entries: Vec<(String, String)> =
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
}
