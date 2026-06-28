use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use crate::error::AppError;

use super::embeddings::EmbeddingProvider;
use super::indexer::MemoryIndexer;
use super::store::MemoryStore;

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

const MAX_FILE_SIZE: u64 = 100_000;

#[derive(Clone)]
pub struct MerkleIndex {
    conn: Arc<Mutex<Connection>>,
}

impl MerkleIndex {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        let conn = Connection::open(path)
            .map_err(|e| AppError::ProviderError(format!("merkle db open: {e}")))?;
        let idx = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        idx.init_schema()?;
        Ok(idx)
    }

    pub fn in_memory() -> Result<Self, AppError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| AppError::ProviderError(format!("merkle db: {e}")))?;
        let idx = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        idx.init_schema()?;
        Ok(idx)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, AppError> {
        self.conn
            .lock()
            .map_err(|e| AppError::ProviderError(format!("merkle lock: {e}")))
    }

    fn init_schema(&self) -> Result<(), AppError> {
        self.lock()?
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS file_hashes (
                    path TEXT PRIMARY KEY,
                    hash TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS dir_hashes (
                    path TEXT PRIMARY KEY,
                    hash TEXT NOT NULL
                );",
            )
            .map_err(|e| AppError::ProviderError(format!("merkle schema: {e}")))?;
        Ok(())
    }

    pub fn get_file_hash(&self, path: &str) -> Result<Option<String>, AppError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT hash FROM file_hashes WHERE path = ?1")
            .map_err(|e| AppError::ProviderError(format!("merkle get: {e}")))?;
        let result = stmt.query_row(params![path], |row| row.get(0)).ok();
        Ok(result)
    }

    pub fn set_file_hash(&self, path: &str, hash: &str) -> Result<(), AppError> {
        self.lock()?
            .execute(
                "INSERT OR REPLACE INTO file_hashes (path, hash) VALUES (?1, ?2)",
                params![path, hash],
            )
            .map_err(|e| AppError::ProviderError(format!("merkle set: {e}")))?;
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<(), AppError> {
        self.lock()?
            .execute("DELETE FROM file_hashes WHERE path = ?1", params![path])
            .map_err(|e| AppError::ProviderError(format!("merkle remove: {e}")))?;
        Ok(())
    }

    pub fn get_dir_hash(&self, path: &str) -> Result<Option<String>, AppError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT hash FROM dir_hashes WHERE path = ?1")
            .map_err(|e| AppError::ProviderError(format!("merkle dir get: {e}")))?;
        let result = stmt.query_row(params![path], |row| row.get(0)).ok();
        Ok(result)
    }

    pub fn set_dir_hash(&self, path: &str, hash: &str) -> Result<(), AppError> {
        self.lock()?
            .execute(
                "INSERT OR REPLACE INTO dir_hashes (path, hash) VALUES (?1, ?2)",
                params![path, hash],
            )
            .map_err(|e| AppError::ProviderError(format!("merkle dir set: {e}")))?;
        Ok(())
    }

    pub fn all_file_paths(&self) -> Result<Vec<String>, AppError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT path FROM file_hashes")
            .map_err(|e| AppError::ProviderError(format!("merkle list: {e}")))?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| AppError::ProviderError(format!("merkle list: {e}")))?;
        Ok(rows.filter_map(Result::ok).collect())
    }
}

pub fn content_hash(content: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn dir_hash(child_hashes: &mut [(String, String)]) -> String {
    child_hashes.sort_by(|a, b| a.0.cmp(&b.0));
    let mut combined = String::new();
    for (name, hash) in child_hashes.iter() {
        let _ = write!(combined, "{name}:{hash};");
    }
    content_hash(combined.as_bytes())
}

fn is_indexable(name: &str) -> bool {
    let Some(ext) = name.rsplit('.').next() else {
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

pub struct IncrementalResult {
    pub indexed: usize,
    pub removed: usize,
    pub unchanged: usize,
}

pub async fn incremental_index(
    root: &Path,
    merkle: &MerkleIndex,
    store: &MemoryStore,
    embedder: &dyn EmbeddingProvider,
) -> Result<IncrementalResult, AppError> {
    let mut current_files: HashMap<String, String> = HashMap::new();
    collect_files(root, root, &mut current_files)?;

    let previous_paths = merkle.all_file_paths()?;

    let indexer = MemoryIndexer::new(store, embedder);
    let mut reindexed = 0;
    let mut unchanged = 0;
    let mut removed = 0;

    for (rel_path, hash) in &current_files {
        let prev_hash = merkle.get_file_hash(rel_path)?;
        if prev_hash.as_deref() == Some(hash.as_str()) {
            unchanged += 1;
            continue;
        }

        let abs_path = root.join(rel_path);
        let Ok(content) = std::fs::read_to_string(&abs_path) else {
            continue;
        };
        let rel = Path::new(rel_path);
        if indexer.index_file(rel, &content).await.is_ok() {
            merkle.set_file_hash(rel_path, hash)?;
            reindexed += 1;
        }
    }

    for prev_path in &previous_paths {
        if !current_files.contains_key(prev_path) {
            let source_tag = format!("file:{prev_path}");
            // Offload the blocking rusqlite delete off the async runtime (H-3).
            let store = store.clone();
            tokio::task::spawn_blocking(move || store.delete_by_source(&source_tag))
                .await
                .map_err(|e| AppError::ProviderError(format!("memory delete join: {e}")))??;
            merkle.remove_file(prev_path)?;
            removed += 1;
        }
    }

    if reindexed > 0 || removed > 0 {
        compute_dir_hashes(root, root, merkle)?;
    }

    Ok(IncrementalResult {
        indexed: reindexed,
        removed,
        unchanged,
    })
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut HashMap<String, String>,
) -> Result<(), AppError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() {
            if IGNORE_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            collect_files(root, &path, out)?;
        } else if path.is_file() && is_indexable(&name) {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > MAX_FILE_SIZE {
                continue;
            }
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            out.insert(rel, content_hash(&content));
        }
    }

    Ok(())
}

fn compute_dir_hashes(root: &Path, dir: &Path, merkle: &MerkleIndex) -> Result<String, AppError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(String::new());
    };

    let mut child_hashes: Vec<(String, String)> = Vec::new();

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() {
            if IGNORE_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            let h = compute_dir_hashes(root, &path, merkle)?;
            if !h.is_empty() {
                child_hashes.push((name, h));
            }
        } else if path.is_file() && is_indexable(&name) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if let Some(h) = merkle.get_file_hash(&rel)? {
                child_hashes.push((name, h));
            }
        }
    }

    if child_hashes.is_empty() {
        return Ok(String::new());
    }

    let hash = dir_hash(&mut child_hashes);
    let rel = dir
        .strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .to_string();
    let dir_key = if rel.is_empty() { ".".to_string() } else { rel };
    merkle.set_dir_hash(&dir_key, &hash)?;

    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn merkle_index_stores_and_retrieves() {
        let idx = MerkleIndex::in_memory().unwrap();
        assert!(idx.get_file_hash("foo.rs").unwrap().is_none());
        idx.set_file_hash("foo.rs", "abc123").unwrap();
        assert_eq!(idx.get_file_hash("foo.rs").unwrap().unwrap(), "abc123");
    }

    #[test]
    fn merkle_index_remove_file() {
        let idx = MerkleIndex::in_memory().unwrap();
        idx.set_file_hash("bar.rs", "def456").unwrap();
        idx.remove_file("bar.rs").unwrap();
        assert!(idx.get_file_hash("bar.rs").unwrap().is_none());
    }

    #[test]
    fn merkle_index_lists_all_paths() {
        let idx = MerkleIndex::in_memory().unwrap();
        idx.set_file_hash("a.rs", "h1").unwrap();
        idx.set_file_hash("b.rs", "h2").unwrap();
        let paths = idx.all_file_paths().unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn collect_files_from_temp_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target").join("junk.rs"), "junk").unwrap();

        let mut files = HashMap::new();
        collect_files(dir.path(), dir.path(), &mut files).unwrap();
        assert!(files.contains_key("src/main.rs"));
        assert!(files.contains_key("Cargo.toml"));
        assert!(!files.contains_key("target/junk.rs"));
    }

    #[test]
    fn dir_hash_is_order_independent() {
        let mut a = vec![
            ("b.rs".to_string(), "hash_b".to_string()),
            ("a.rs".to_string(), "hash_a".to_string()),
        ];
        let mut b = vec![
            ("a.rs".to_string(), "hash_a".to_string()),
            ("b.rs".to_string(), "hash_b".to_string()),
        ];
        assert_eq!(dir_hash(&mut a), dir_hash(&mut b));
    }
}
