use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use crate::error::AppError;

use super::{MemoryEntry, MemorySource, MemoryTier, RetrievedMemory};

const VECTOR_WEIGHT: f32 = 0.6;
const FTS_WEIGHT: f32 = 0.4;
const DECAY_HALF_LIFE_SECS: f64 = 86400.0 * 7.0;

#[derive(Clone)]
pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
    dimension: usize,
}

impl MemoryStore {
    pub fn open(path: &Path, dimension: usize) -> Result<Self, AppError> {
        let conn = Connection::open(path)
            .map_err(|e| AppError::ProviderError(format!("memory db open: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            dimension,
        };
        store.init_schema()?;
        Ok(store)
    }

    pub fn in_memory(dimension: usize) -> Result<Self, AppError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| AppError::ProviderError(format!("memory db: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            dimension,
        };
        store.init_schema()?;
        Ok(store)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, AppError> {
        self.conn
            .lock()
            .map_err(|e| AppError::ProviderError(format!("memory lock: {e}")))
    }

    fn init_schema(&self) -> Result<(), AppError> {
        self.lock()?
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS memories (
                    id TEXT PRIMARY KEY,
                    content TEXT NOT NULL,
                    embedding BLOB NOT NULL,
                    source TEXT NOT NULL,
                    tier TEXT NOT NULL DEFAULT 'short',
                    timestamp INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source);
                CREATE INDEX IF NOT EXISTS idx_memories_timestamp ON memories(timestamp);
                CREATE INDEX IF NOT EXISTS idx_memories_tier ON memories(tier);
                CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                    id UNINDEXED, content, tokenize='porter unicode61'
                );",
            )
            .map_err(|e| AppError::ProviderError(format!("memory schema: {e}")))?;
        Ok(())
    }

    pub fn insert(&self, entry: &MemoryEntry) -> Result<(), AppError> {
        let emb_blob = embedding_to_blob(&entry.embedding);
        let conn = self.lock()?;

        conn.execute("DELETE FROM memories_fts WHERE id = ?1", params![entry.id])
            .ok();

        conn.execute(
            "INSERT OR REPLACE INTO memories (id, content, embedding, source, tier, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.id,
                entry.content,
                emb_blob,
                entry.source.to_tag(),
                entry.tier.as_str(),
                entry.timestamp,
            ],
        )
        .map_err(|e| AppError::ProviderError(format!("memory insert: {e}")))?;

        conn.execute(
            "INSERT INTO memories_fts (id, content) VALUES (?1, ?2)",
            params![entry.id, entry.content],
        )
        .map_err(|e| AppError::ProviderError(format!("fts insert: {e}")))?;

        Ok(())
    }

    pub fn search(
        &self,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<RetrievedMemory>, AppError> {
        self.hybrid_search(query_embedding, "", top_k)
    }

    pub fn hybrid_search(
        &self,
        query_embedding: &[f32],
        query_text: &str,
        top_k: usize,
    ) -> Result<Vec<RetrievedMemory>, AppError> {
        let conn = self.lock()?;
        let now = current_timestamp();

        let fts_ids = if query_text.is_empty() {
            std::collections::HashMap::new()
        } else {
            Self::fts_search(&conn, query_text)?
        };

        let dim = self.dimension;
        let mut stmt = conn
            .prepare("SELECT id, content, embedding, source, tier, timestamp FROM memories")
            .map_err(|e| AppError::ProviderError(format!("memory query: {e}")))?;

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let emb_blob: Vec<u8> = row.get(2)?;
                let source_tag: String = row.get(3)?;
                let tier_str: String = row.get(4)?;
                let timestamp: i64 = row.get(5)?;
                Ok((id, content, emb_blob, source_tag, tier_str, timestamp))
            })
            .map_err(|e| AppError::ProviderError(format!("memory search: {e}")))?;

        let mut scored: Vec<RetrievedMemory> = Vec::new();
        for row in rows {
            let (id, content, emb_blob, source_tag, tier_str, timestamp) =
                row.map_err(|e| AppError::ProviderError(format!("memory row: {e}")))?;

            let emb = blob_to_embedding(&emb_blob, dim);
            let vector_score = cosine_similarity(query_embedding, &emb);

            let fts_score = fts_ids.get(&id).copied().unwrap_or(0.0);

            let raw = if fts_ids.is_empty() {
                vector_score
            } else {
                vector_score * VECTOR_WEIGHT + fts_score * FTS_WEIGHT
            };

            let decay = time_decay(now, timestamp);
            let tier_boost = MemoryTier::parse_tier(&tier_str).boost();
            let score = raw * decay * tier_boost;

            scored.push(RetrievedMemory {
                content,
                score,
                source: MemorySource::from_tag(&source_tag),
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);

        Ok(scored)
    }

    fn fts_search(
        conn: &Connection,
        query: &str,
    ) -> Result<std::collections::HashMap<String, f32>, AppError> {
        let escaped = query.replace('"', "\"\"");
        let fts_query = format!("\"{escaped}\"");

        let mut stmt = conn
            .prepare(
                "SELECT id, rank FROM memories_fts WHERE memories_fts MATCH ?1
                 ORDER BY rank LIMIT 100",
            )
            .map_err(|e| AppError::ProviderError(format!("fts query: {e}")))?;

        let rows = stmt
            .query_map(params![fts_query], |row| {
                let id: String = row.get(0)?;
                let rank: f64 = row.get(1)?;
                Ok((id, rank))
            })
            .map_err(|e| AppError::ProviderError(format!("fts search: {e}")))?;

        let mut scores = std::collections::HashMap::new();
        let mut max_rank = 0.0_f64;

        let entries: Vec<(String, f64)> = rows
            .filter_map(Result::ok)
            .map(|(id, rank)| {
                let abs = rank.abs();
                if abs > max_rank {
                    max_rank = abs;
                }
                (id, abs)
            })
            .collect();

        if max_rank > 0.0 {
            for (id, abs_rank) in entries {
                #[allow(clippy::cast_possible_truncation)]
                let normalized = (abs_rank / max_rank) as f32;
                scores.insert(id, normalized);
            }
        }

        Ok(scores)
    }

    pub fn delete_by_source(&self, source_tag: &str) -> Result<usize, AppError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM memories_fts WHERE id IN (
                SELECT id FROM memories WHERE source = ?1
            )",
            params![source_tag],
        )
        .ok();

        let count = conn
            .execute(
                "DELETE FROM memories WHERE source = ?1",
                params![source_tag],
            )
            .map_err(|e| AppError::ProviderError(format!("memory delete: {e}")))?;
        Ok(count)
    }

    pub fn delete_by_tier(&self, tier: MemoryTier) -> Result<usize, AppError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM memories_fts WHERE id IN (
                SELECT id FROM memories WHERE tier = ?1
            )",
            params![tier.as_str()],
        )
        .ok();

        let count = conn
            .execute(
                "DELETE FROM memories WHERE tier = ?1",
                params![tier.as_str()],
            )
            .map_err(|e| AppError::ProviderError(format!("tier delete: {e}")))?;
        Ok(count)
    }

    pub fn prune_oldest(&self, keep: usize) -> Result<usize, AppError> {
        let conn = self.lock()?;
        let total: usize = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .map_err(|e| AppError::ProviderError(format!("memory count: {e}")))?;

        if total <= keep {
            return Ok(0);
        }

        let to_delete = total - keep;

        conn.execute(
            "DELETE FROM memories_fts WHERE id IN (
                SELECT id FROM memories ORDER BY timestamp ASC LIMIT ?1
            )",
            params![to_delete],
        )
        .ok();

        let count = conn
            .execute(
                "DELETE FROM memories WHERE id IN (
                    SELECT id FROM memories ORDER BY timestamp ASC LIMIT ?1
                )",
                params![to_delete],
            )
            .map_err(|e| AppError::ProviderError(format!("memory prune: {e}")))?;
        Ok(count)
    }

    pub fn promote_tier(&self, id: &str, new_tier: MemoryTier) -> Result<(), AppError> {
        self.lock()?
            .execute(
                "UPDATE memories SET tier = ?1 WHERE id = ?2",
                params![new_tier.as_str(), id],
            )
            .map_err(|e| AppError::ProviderError(format!("tier promote: {e}")))?;
        Ok(())
    }

    pub fn count(&self) -> Result<usize, AppError> {
        self.lock()?
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .map_err(|e| AppError::ProviderError(format!("memory count: {e}")))
    }

    pub fn count_by_tier(&self, tier: MemoryTier) -> Result<usize, AppError> {
        self.lock()?
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE tier = ?1",
                params![tier.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| AppError::ProviderError(format!("tier count: {e}")))
    }
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_embedding(blob: &[u8], dimension: usize) -> Vec<f32> {
    let mut result = Vec::with_capacity(dimension);
    for chunk in blob.chunks_exact(4) {
        let bytes: [u8; 4] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        result.push(f32::from_le_bytes(bytes));
    }
    result
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for i in 0..len {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

fn time_decay(now: i64, timestamp: i64) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let age_secs = ((now - timestamp).max(0)) as f64;
    let decay = (-age_secs * (2.0_f64.ln()) / DECAY_HALF_LIFE_SECS).exp();
    #[allow(clippy::cast_possible_truncation)]
    let result = decay as f32;
    result
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let s = cosine_similarity(&v, &v);
        assert!((s - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < 0.001);
    }

    #[test]
    fn cosine_opposite_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let s = cosine_similarity(&a, &b);
        assert!((s + 1.0).abs() < 0.001);
    }

    #[test]
    fn blob_roundtrip() {
        let emb = vec![0.1, 0.2, 0.3, 0.4];
        let blob = embedding_to_blob(&emb);
        let back = blob_to_embedding(&blob, 4);
        assert_eq!(emb, back);
    }

    #[test]
    fn time_decay_recent_is_near_one() {
        let now = current_timestamp();
        let d = time_decay(now, now);
        assert!((d - 1.0).abs() < 0.01);
    }

    #[test]
    fn time_decay_old_is_less() {
        let now = current_timestamp();
        let week_ago = now - 604_800;
        let d = time_decay(now, week_ago);
        assert!(d < 0.6);
        assert!(d > 0.4);
    }
}
