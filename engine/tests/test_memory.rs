use async_trait::async_trait;
use mcp_universal::error::AppError;
use mcp_universal::memory::embeddings::EmbeddingProvider;
use mcp_universal::memory::indexer::{chunk_text, MemoryIndexer};
use mcp_universal::memory::store::MemoryStore;
use mcp_universal::memory::{
    format_context, retrieve_context, MemoryEntry, MemorySource, MemoryTier, RetrievedMemory,
};
use mcp_universal::models::ToolMessage;

struct FakeEmbedder;

#[async_trait]
impl EmbeddingProvider for FakeEmbedder {
    fn dimension(&self) -> usize {
        4
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, AppError> {
        let hash = text
            .bytes()
            .fold(0u32, |acc, b| acc.wrapping_add(u32::from(b)));
        let base = f32::from(u16::try_from(hash % 100).unwrap_or(0)) / 100.0;
        Ok(vec![base, 1.0 - base, base * 0.5, 0.3])
    }

    fn clone_box(&self) -> Box<dyn EmbeddingProvider> {
        Box::new(FakeEmbedder)
    }
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

fn make_entry(id: &str, content: &str, embedding: Vec<f32>, source: MemorySource) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        content: content.to_string(),
        embedding,
        source,
        tier: MemoryTier::Short,
        timestamp: now_ts(),
    }
}

#[test]
fn store_insert_and_count() {
    let store = MemoryStore::in_memory(4).unwrap();
    assert_eq!(store.count().unwrap(), 0);

    store
        .insert(&make_entry(
            "a",
            "hello",
            vec![1.0, 0.0, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    assert_eq!(store.count().unwrap(), 1);

    store
        .insert(&make_entry(
            "b",
            "world",
            vec![0.0, 1.0, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    assert_eq!(store.count().unwrap(), 2);
}

#[test]
fn store_upsert_replaces() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&make_entry(
            "a",
            "first",
            vec![1.0, 0.0, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "a",
            "updated",
            vec![1.0, 0.0, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();

    assert_eq!(store.count().unwrap(), 1);
    let results = store.search(&[1.0, 0.0, 0.0, 0.0], 10).unwrap();
    assert_eq!(results[0].content, "updated");
}

#[test]
fn store_search_returns_most_similar() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&make_entry(
            "a",
            "similar",
            vec![0.9, 0.1, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "b",
            "different",
            vec![0.0, 0.0, 1.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "c",
            "also_similar",
            vec![0.8, 0.2, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();

    let results = store.search(&[1.0, 0.0, 0.0, 0.0], 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content, "similar");
    assert_eq!(results[1].content, "also_similar");
    assert!(results[0].score > results[1].score);
}

#[test]
fn store_search_top_k_limits_results() {
    let store = MemoryStore::in_memory(4).unwrap();

    for i in 0..10 {
        store
            .insert(&make_entry(
                &format!("e{i}"),
                &format!("entry_{i}"),
                vec![1.0, 0.0, 0.0, 0.0],
                MemorySource::Manual,
            ))
            .unwrap();
    }

    let results = store.search(&[1.0, 0.0, 0.0, 0.0], 3).unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn store_delete_by_source() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&make_entry(
            "s1",
            "session_data",
            vec![1.0, 0.0, 0.0, 0.0],
            MemorySource::Session {
                session_id: "abc".to_string(),
            },
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "m1",
            "manual_data",
            vec![1.0, 0.0, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();

    assert_eq!(store.count().unwrap(), 2);
    let deleted = store.delete_by_source("session:abc").unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn store_prune_oldest() {
    let store = MemoryStore::in_memory(4).unwrap();

    for i in 0..10 {
        store
            .insert(&MemoryEntry {
                id: format!("e{i}"),
                content: format!("entry_{i}"),
                embedding: vec![1.0, 0.0, 0.0, 0.0],
                source: MemorySource::Manual,
                tier: MemoryTier::Short,
                timestamp: now_ts() - 100 + i64::from(i),
            })
            .unwrap();
    }

    let pruned = store.prune_oldest(5).unwrap();
    assert_eq!(pruned, 5);
    assert_eq!(store.count().unwrap(), 5);
}

#[test]
fn store_prune_noop_when_under_limit() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&make_entry(
            "a",
            "only_one",
            vec![1.0, 0.0, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();

    let pruned = store.prune_oldest(100).unwrap();
    assert_eq!(pruned, 0);
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn memory_source_roundtrip() {
    let sources = vec![
        MemorySource::Session {
            session_id: "s123".to_string(),
        },
        MemorySource::File {
            path: "/foo/bar.rs".to_string(),
        },
        MemorySource::Manual,
    ];

    for src in sources {
        let tag = src.to_tag();
        let back = MemorySource::from_tag(&tag);
        assert_eq!(src.to_tag(), back.to_tag());
    }
}

#[tokio::test]
async fn retrieve_context_with_fake_embedder() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;

    store
        .insert(&make_entry(
            "a",
            "rust programming",
            vec![0.5, 0.5, 0.25, 0.3],
            MemorySource::Manual,
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "b",
            "cooking recipe",
            vec![0.0, 0.0, 1.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();

    let results = retrieve_context(&store, &embedder, "rust", 2)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn format_context_empty() {
    let empty: Vec<RetrievedMemory> = vec![];
    assert!(format_context(&empty).is_empty());
}

#[test]
fn format_context_with_entries() {
    let memories = vec![RetrievedMemory {
        content: "some context".to_string(),
        score: 0.95,
        source: MemorySource::Manual,
    }];
    let formatted = format_context(&memories);
    assert!(formatted.contains("Relevant context from memory"));
    assert!(formatted.contains("some context"));
    assert!(formatted.contains("0.95"));
}

#[test]
fn chunk_text_basic() {
    let chunks = chunk_text("a b c d e", 3, 1);
    assert!(!chunks.is_empty());
    assert!(chunks[0].contains('a'));
}

#[test]
fn chunk_text_empty() {
    let chunks = chunk_text("", 10, 2);
    assert!(chunks.is_empty());
}

#[test]
fn chunk_text_single_word() {
    let chunks = chunk_text("hello", 10, 2);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], "hello");
}

#[tokio::test]
async fn indexer_index_text() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    let count = indexer
        .index_text("test", "hello world from the indexer", MemorySource::Manual)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(store.count().unwrap(), 1);
}

#[tokio::test]
async fn indexer_index_session() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    let messages = vec![
        ToolMessage::user("Fix the bug in main.rs".to_string()),
        ToolMessage::system("I'll look at that file now.".to_string()),
    ];

    let count = indexer.index_session("sess1", &messages).await.unwrap();
    assert!(count >= 1);
    assert!(store.count().unwrap() >= 1);
}

#[tokio::test]
async fn indexer_reindex_replaces_old() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    let messages = vec![ToolMessage::user("first session".to_string())];
    indexer.index_session("sess1", &messages).await.unwrap();
    let count1 = store.count().unwrap();

    let messages2 = vec![ToolMessage::user("updated session".to_string())];
    indexer.index_session("sess1", &messages2).await.unwrap();
    let count2 = store.count().unwrap();

    assert_eq!(count1, count2);
}

#[tokio::test]
async fn indexer_index_file() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    let content = "fn main() {\n    println!(\"Hello, world!\");\n}";
    let count = indexer
        .index_file(std::path::Path::new("src/main.rs"), content)
        .await
        .unwrap();
    assert!(count >= 1);
    assert!(store.count().unwrap() >= 1);
}

#[tokio::test]
async fn full_index_retrieve_flow() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    indexer
        .index_text(
            "t1",
            "rust error handling with Result",
            MemorySource::Manual,
        )
        .await
        .unwrap();
    indexer
        .index_text("t2", "javascript fetch API usage", MemorySource::Manual)
        .await
        .unwrap();

    let results = retrieve_context(&store, &embedder, "rust errors", 5)
        .await
        .unwrap();
    assert!(!results.is_empty());
}

// --- Hybrid search (FTS5 + vector) tests ---

#[test]
fn hybrid_search_fts_boosts_keyword_matches() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&make_entry(
            "a",
            "rust error handling with Result type",
            vec![0.5, 0.5, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "b",
            "python exception catching try except",
            vec![0.5, 0.5, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();

    let results = store
        .hybrid_search(&[0.5, 0.5, 0.0, 0.0], "rust error", 2)
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].content.contains("rust"));
}

#[test]
fn hybrid_search_empty_query_text_uses_vector_only() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&make_entry(
            "a",
            "close vector",
            vec![0.9, 0.1, 0.0, 0.0],
            MemorySource::Manual,
        ))
        .unwrap();
    store
        .insert(&make_entry(
            "b",
            "far vector",
            vec![0.0, 0.0, 0.9, 0.1],
            MemorySource::Manual,
        ))
        .unwrap();

    let results = store.hybrid_search(&[1.0, 0.0, 0.0, 0.0], "", 2).unwrap();
    assert_eq!(results[0].content, "close vector");
}

// --- Tiered memory tests ---

#[test]
fn tier_boost_long_scores_higher() {
    let store = MemoryStore::in_memory(4).unwrap();
    let ts = now_ts();

    store
        .insert(&MemoryEntry {
            id: "short".to_string(),
            content: "short term data".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Short,
            timestamp: ts,
        })
        .unwrap();
    store
        .insert(&MemoryEntry {
            id: "long".to_string(),
            content: "long term data".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Long,
            timestamp: ts,
        })
        .unwrap();

    let results = store.search(&[1.0, 0.0, 0.0, 0.0], 2).unwrap();
    assert_eq!(results[0].content, "long term data");
}

#[test]
fn count_by_tier() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&MemoryEntry {
            id: "s1".to_string(),
            content: "short".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Short,
            timestamp: now_ts(),
        })
        .unwrap();
    store
        .insert(&MemoryEntry {
            id: "l1".to_string(),
            content: "long".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Long,
            timestamp: now_ts(),
        })
        .unwrap();

    assert_eq!(store.count_by_tier(MemoryTier::Short).unwrap(), 1);
    assert_eq!(store.count_by_tier(MemoryTier::Long).unwrap(), 1);
    assert_eq!(store.count_by_tier(MemoryTier::Medium).unwrap(), 0);
}

#[test]
fn delete_by_tier() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&MemoryEntry {
            id: "s1".to_string(),
            content: "short".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Short,
            timestamp: now_ts(),
        })
        .unwrap();
    store
        .insert(&MemoryEntry {
            id: "l1".to_string(),
            content: "long".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Long,
            timestamp: now_ts(),
        })
        .unwrap();

    store.delete_by_tier(MemoryTier::Short).unwrap();
    assert_eq!(store.count().unwrap(), 1);
    assert_eq!(store.count_by_tier(MemoryTier::Long).unwrap(), 1);
}

#[test]
fn promote_tier() {
    let store = MemoryStore::in_memory(4).unwrap();

    store
        .insert(&MemoryEntry {
            id: "s1".to_string(),
            content: "promotable".to_string(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            source: MemorySource::Manual,
            tier: MemoryTier::Short,
            timestamp: now_ts(),
        })
        .unwrap();

    assert_eq!(store.count_by_tier(MemoryTier::Short).unwrap(), 1);
    store.promote_tier("s1", MemoryTier::Long).unwrap();
    assert_eq!(store.count_by_tier(MemoryTier::Short).unwrap(), 0);
    assert_eq!(store.count_by_tier(MemoryTier::Long).unwrap(), 1);
}

// --- AST chunking tests ---

#[test]
fn ast_chunker_rust_splits_on_functions() {
    use mcp_universal::memory::chunker::{chunk_code, ChunkKind};
    let code = "\
use std::io;

fn first() -> i32 {
    1
}

fn second() -> i32 {
    2
}
";
    let chunks = chunk_code(code, std::path::Path::new("lib.rs"), 50);
    let fn_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Function)
        .collect();
    assert_eq!(fn_chunks.len(), 2);
    assert!(fn_chunks[0].content.contains("fn first"));
    assert!(fn_chunks[1].content.contains("fn second"));
}

#[test]
fn ast_chunker_python_splits_classes() {
    use mcp_universal::memory::chunker::chunk_code;
    let code = "\
class Foo:
    def bar(self):
        pass

class Baz:
    def qux(self):
        pass
";
    let chunks = chunk_code(code, std::path::Path::new("app.py"), 50);
    assert!(chunks.len() >= 2);
}

#[test]
fn ast_chunker_respects_max_lines() {
    use mcp_universal::memory::chunker::chunk_code;
    let code = (0..200)
        .map(|i| format!("let x{i} = {i};"))
        .collect::<Vec<_>>()
        .join("\n");
    let full = format!("fn big() {{\n{code}\n}}");
    let chunks = chunk_code(&full, std::path::Path::new("big.rs"), 10);
    for chunk in &chunks {
        let line_count = chunk.content.lines().count();
        assert!(line_count <= 10, "chunk has {line_count} lines, max is 10");
    }
}

// --- Persistent MEMORY.md tests ---

#[test]
fn persistent_memory_add_and_load() {
    use mcp_universal::memory::persistent::PersistentMemory;

    let dir = tempfile::TempDir::new().unwrap();
    let mem = PersistentMemory::new(dir.path());

    assert!(!mem.exists());
    mem.add_fact("Preferences", "Use 4-space indentation")
        .unwrap();
    assert!(mem.exists());

    let facts = mem.load_facts().unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].category, "Preferences");
    assert!(facts[0].content.contains("4-space"));
}

#[test]
fn persistent_memory_multiple_categories() {
    use mcp_universal::memory::persistent::PersistentMemory;

    let dir = tempfile::TempDir::new().unwrap();
    let mem = PersistentMemory::new(dir.path());

    mem.add_fact("Preferences", "Dark mode").unwrap();
    mem.add_fact("Patterns", "Always run tests before commit")
        .unwrap();
    mem.add_fact("Preferences", "Use Rust").unwrap();

    let facts = mem.load_facts().unwrap();
    assert_eq!(facts.len(), 3);
}

#[test]
fn persistent_memory_remove_fact() {
    use mcp_universal::memory::persistent::PersistentMemory;

    let dir = tempfile::TempDir::new().unwrap();
    let mem = PersistentMemory::new(dir.path());

    mem.add_fact("Preferences", "Use tabs").unwrap();
    mem.add_fact("Preferences", "Dark mode").unwrap();

    let removed = mem.remove_fact("Preferences", "tabs").unwrap();
    assert!(removed);

    let facts = mem.load_facts().unwrap();
    assert_eq!(facts.len(), 1);
    assert!(facts[0].content.contains("Dark"));
}

#[tokio::test]
async fn indexer_uses_ast_chunking_for_code() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    let code = "\
fn foo() {
    println!(\"foo\");
}

fn bar() {
    println!(\"bar\");
}
";
    let count = indexer
        .index_file(std::path::Path::new("lib.rs"), code)
        .await
        .unwrap();
    assert!(count >= 2);
}

#[tokio::test]
async fn indexer_persistent_fact_is_long_tier() {
    let store = MemoryStore::in_memory(4).unwrap();
    let embedder = FakeEmbedder;
    let indexer = MemoryIndexer::new(&store, &embedder);

    indexer
        .index_persistent_fact("pref1", "Always use Result for errors")
        .await
        .unwrap();

    assert_eq!(store.count_by_tier(MemoryTier::Long).unwrap(), 1);
}
