use mcp_universal::tools::git::{GitAdd, GitCommit, GitDiff, GitLog, GitStatus};
use mcp_universal::tools::Tool;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

fn setup_git_repo() -> (TempDir, Arc<PathBuf>) {
    let dir = TempDir::new().unwrap();
    let root_path = dir.path().to_path_buf();

    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&root_path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&root_path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&root_path)
        .output()
        .unwrap();

    std::fs::write(root_path.join("hello.txt"), "hello world\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&root_path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&root_path)
        .output()
        .unwrap();

    (dir, Arc::new(root_path))
}

#[tokio::test]
async fn git_status_shows_clean() {
    let (_dir, root) = setup_git_repo();
    let tool = GitStatus::new(root);
    let result = tool.execute(json!({})).await.unwrap();
    let text = result["status"].as_str().unwrap();
    assert!(
        text.is_empty()
            || text.contains("nothing to commit")
            || text.contains("working tree clean"),
        "unexpected status: {text}"
    );
}

#[tokio::test]
async fn git_status_shows_modified() {
    let (_dir, root) = setup_git_repo();
    std::fs::write(root.join("hello.txt"), "changed\n").unwrap();
    let tool = GitStatus::new(root);
    let result = tool.execute(json!({})).await.unwrap();
    let text = result["status"].as_str().unwrap();
    assert!(text.contains("hello.txt"));
}

#[tokio::test]
async fn git_diff_shows_changes() {
    let (_dir, root) = setup_git_repo();
    std::fs::write(root.join("hello.txt"), "changed\n").unwrap();
    let tool = GitDiff::new(root);
    let result = tool.execute(json!({})).await.unwrap();
    let text = result["diff"].as_str().unwrap();
    assert!(text.contains("-hello world"));
    assert!(text.contains("+changed"));
}

#[tokio::test]
async fn git_diff_staged() {
    let (_dir, root) = setup_git_repo();
    std::fs::write(root.join("hello.txt"), "staged\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "hello.txt"])
        .current_dir(root.as_path())
        .output()
        .unwrap();

    let tool = GitDiff::new(root);
    let result = tool.execute(json!({ "staged": true })).await.unwrap();
    let text = result["diff"].as_str().unwrap();
    assert!(text.contains("+staged"));
}

#[tokio::test]
async fn git_log_shows_initial_commit() {
    let (_dir, root) = setup_git_repo();
    let tool = GitLog::new(root);
    let result = tool.execute(json!({})).await.unwrap();
    let text = result["log"].as_str().unwrap();
    assert!(text.contains("initial"));
}

#[tokio::test]
async fn git_log_respects_max_count() {
    let (_dir, root) = setup_git_repo();
    std::fs::write(root.join("second.txt"), "two\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root.as_path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "second commit"])
        .current_dir(root.as_path())
        .output()
        .unwrap();

    let tool = GitLog::new(root);
    let result = tool.execute(json!({ "count": 1 })).await.unwrap();
    let text = result["log"].as_str().unwrap();
    assert!(text.contains("second commit"));
}

#[tokio::test]
async fn git_add_stages_file() {
    let (_dir, root) = setup_git_repo();
    std::fs::write(root.join("new.txt"), "new file\n").unwrap();
    let tool = GitAdd::new(root.clone());
    let result = tool.execute(json!({ "paths": ["new.txt"] })).await.unwrap();
    assert!(result["staged"].as_bool().unwrap());

    let status = GitStatus::new(root);
    let status_result = status.execute(json!({})).await.unwrap();
    let text = status_result["status"].as_str().unwrap();
    assert!(text.contains("new.txt"));
}

#[tokio::test]
async fn git_commit_creates_commit() {
    let (_dir, root) = setup_git_repo();
    std::fs::write(root.join("commit_me.txt"), "data\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root.as_path())
        .output()
        .unwrap();

    let tool = GitCommit::new(root.clone());
    let result = tool
        .execute(json!({ "message": "test commit msg" }))
        .await
        .unwrap();
    let text = result["output"].as_str().unwrap();
    assert!(text.contains("test commit msg"));

    let log = GitLog::new(root);
    let log_result = log.execute(json!({})).await.unwrap();
    let log_text = log_result["log"].as_str().unwrap();
    assert!(log_text.contains("test commit msg"));
}

#[tokio::test]
async fn git_tools_report_requires_approval() {
    let (_dir, root) = setup_git_repo();

    assert!(!GitStatus::new(root.clone()).requires_approval());
    assert!(!GitDiff::new(root.clone()).requires_approval());
    assert!(!GitLog::new(root.clone()).requires_approval());
    assert!(GitAdd::new(root.clone()).requires_approval());
    assert!(GitCommit::new(root).requires_approval());
}
