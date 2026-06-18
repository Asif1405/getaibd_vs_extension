use mcp_universal::tools::workspace::{ListDirectory, PatchFile, ReadFile, SearchFiles, WriteFile};
use mcp_universal::tools::Tool;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn read_file_happy_path() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    let path = tmp.path().join("foo.txt");
    tokio::fs::write(&path, "hello world").await.unwrap();

    let tool = ReadFile::new(root);
    let result = tool.execute(json!({ "path": "foo.txt" })).await.unwrap();

    assert_eq!(result["content"], "hello world");
}

#[tokio::test]
async fn read_file_with_line_ranges() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    let path = tmp.path().join("lines.txt");
    tokio::fs::write(&path, "line1\nline2\nline3\nline4\nline5")
        .await
        .unwrap();

    let tool = ReadFile::new(root);
    let result = tool
        .execute(json!({ "path": "lines.txt", "line_start": 2, "line_end": 4 }))
        .await
        .unwrap();

    assert!(result["content"].as_str().unwrap().contains("line2"));
    assert!(result["content"].as_str().unwrap().contains("line3"));
    assert!(result["content"].as_str().unwrap().contains("line4"));
    assert!(!result["content"].as_str().unwrap().contains("line1"));
    assert!(!result["content"].as_str().unwrap().contains("line5"));
}

#[tokio::test]
async fn read_file_path_escaping_rejected() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = ReadFile::new(root);
    let result = tool.execute(json!({ "path": "../../../etc/passwd" })).await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Path escapes project root"));
}

#[tokio::test]
async fn write_file_happy_path() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = WriteFile::new(root);
    let result = tool
        .execute(json!({ "path": "out.txt", "content": "written content" }))
        .await
        .unwrap();

    assert_eq!(result["written"], "out.txt");
    assert_eq!(result["bytes"], 15);

    let content = tokio::fs::read_to_string(tmp.path().join("out.txt"))
        .await
        .unwrap();
    assert_eq!(content, "written content");
}

#[tokio::test]
async fn write_file_with_create_dirs() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = WriteFile::new(root);
    let result = tool
        .execute(json!({
            "path": "a/b/c/nested.txt",
            "content": "nested",
            "create_dirs": true
        }))
        .await
        .unwrap();

    assert_eq!(result["written"], "a/b/c/nested.txt");

    let content = tokio::fs::read_to_string(tmp.path().join("a/b/c/nested.txt"))
        .await
        .unwrap();
    assert_eq!(content, "nested");
}

#[tokio::test]
async fn write_file_path_escaping_rejected() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = WriteFile::new(root);
    let result = tool
        .execute(json!({
            "path": "../../../etc/pwned",
            "content": "evil",
            "create_dirs": true
        }))
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Path escapes project root"));
}

#[tokio::test]
async fn patch_file_happy_path() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    let path = tmp.path().join("patch.txt");
    tokio::fs::write(&path, "foo bar baz").await.unwrap();

    let tool = PatchFile::new(root);
    let result = tool
        .execute(json!({
            "path": "patch.txt",
            "old_text": "bar",
            "new_text": "qux"
        }))
        .await
        .unwrap();

    assert_eq!(result["patched"], "patch.txt");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "foo qux baz");
}

#[tokio::test]
async fn patch_file_old_text_not_found_errors() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    let path = tmp.path().join("patch.txt");
    tokio::fs::write(&path, "foo bar baz").await.unwrap();

    let tool = PatchFile::new(root);
    let result = tool
        .execute(json!({
            "path": "patch.txt",
            "old_text": "nonexistent",
            "new_text": "replacement"
        }))
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("old_text not found"));
}

#[tokio::test]
async fn patch_file_path_escaping_rejected() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = PatchFile::new(root);
    let result = tool
        .execute(json!({
            "path": "../../../etc/passwd",
            "old_text": "x",
            "new_text": "y"
        }))
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Path escapes project root"));
}

#[tokio::test]
async fn list_directory_happy_path() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    tokio::fs::write(tmp.path().join("f1.txt"), "")
        .await
        .unwrap();
    tokio::fs::write(tmp.path().join("f2.txt"), "")
        .await
        .unwrap();
    tokio::fs::create_dir(tmp.path().join("subdir"))
        .await
        .unwrap();

    let tool = ListDirectory::new(root);
    let result = tool.execute(json!({ "path": "." })).await.unwrap();

    let entries = result["entries"].as_array().unwrap();
    assert!(entries.len() >= 3);
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"f1.txt"));
    assert!(names.contains(&"f2.txt"));
    assert!(names.contains(&"subdir"));
}

#[tokio::test]
async fn list_directory_recursive() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    tokio::fs::create_dir_all(tmp.path().join("a/b/c"))
        .await
        .unwrap();
    tokio::fs::write(tmp.path().join("a/b/c/leaf.txt"), "")
        .await
        .unwrap();
    tokio::fs::write(tmp.path().join("a/top.txt"), "")
        .await
        .unwrap();

    let tool = ListDirectory::new(root);
    let result = tool
        .execute(json!({ "path": ".", "recursive": true }))
        .await
        .unwrap();

    let entries = result["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.iter().any(|n| n.contains("leaf.txt")));
    assert!(names.iter().any(|n| n.contains("top.txt")));
}

#[tokio::test]
async fn list_directory_path_escaping_rejected() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = ListDirectory::new(root);
    let result = tool.execute(json!({ "path": "../../../etc" })).await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Path escapes project root"));
}

#[tokio::test]
async fn search_files_with_matches() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    tokio::fs::write(
        tmp.path().join("has_needle.txt"),
        "line1\nneedle here\nline3",
    )
    .await
    .unwrap();
    tokio::fs::write(tmp.path().join("also_needle.txt"), "needle again")
        .await
        .unwrap();

    let tool = SearchFiles::new(root);
    let result = tool
        .execute(json!({ "pattern": "needle", "path": "." }))
        .await
        .unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(matches.len() >= 2);
    assert_eq!(result["count"], matches.len());
}

#[tokio::test]
async fn search_files_no_matches() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());
    tokio::fs::write(tmp.path().join("no_match.txt"), "only hay here")
        .await
        .unwrap();

    let tool = SearchFiles::new(root);
    let result = tool
        .execute(json!({ "pattern": "needle", "path": "." }))
        .await
        .unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 0);
    assert_eq!(result["count"], 0);
}

#[tokio::test]
async fn search_files_path_escaping_rejected() {
    let tmp = TempDir::new().unwrap();
    let root = Arc::new(tmp.path().to_path_buf());

    let tool = SearchFiles::new(root);
    let result = tool
        .execute(json!({ "pattern": "x", "path": "../../../etc" }))
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Path escapes project root"));
}
