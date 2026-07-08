use mcp_universal::tools::command::RunCommand;
use mcp_universal::tools::env_manager::EnvManager;
use mcp_universal::tools::Tool;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

fn allowed_tool() -> (RunCommand, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let root = Arc::new(temp_dir.path().to_path_buf());
    let env_mgr = EnvManager::new(root.clone());
    let tool = RunCommand::new(root, env_mgr);
    (tool, temp_dir)
}

#[tokio::test]
async fn allowed_command_executes_successfully() {
    let (tool, _temp) = allowed_tool();

    let result = tool
        .execute(json!({
            "command": "echo",
            "args": ["hello"]
        }))
        .await
        .unwrap();

    assert_eq!(result["stdout"].as_str().unwrap().trim(), "hello");
    assert_eq!(result["exit_code"], 0);
}

#[tokio::test]
async fn full_command_line_is_split() {
    let (tool, _temp) = allowed_tool();

    // The whole line packed into `command` (no `args`) is split, not treated as one
    // binary name — otherwise the free-tier model can't run anything.
    let result = tool
        .execute(json!({ "command": "echo hello there" }))
        .await
        .unwrap();

    assert_eq!(result["stdout"].as_str().unwrap().trim(), "hello there");
    assert_eq!(result["exit_code"], 0);
}

#[tokio::test]
async fn command_with_args_works() {
    let (tool, _temp) = allowed_tool();

    let result = tool
        .execute(json!({
            "command": "echo",
            "args": ["foo", "bar", "baz"]
        }))
        .await
        .unwrap();

    assert_eq!(result["stdout"].as_str().unwrap().trim(), "foo bar baz");
}

#[tokio::test]
async fn command_captures_stdout_and_stderr() {
    let (tool, _temp) = allowed_tool();

    // stdout / stderr driven with simple binaries.
    let out = tool
        .execute(json!({ "command": "echo", "args": ["out"] }))
        .await
        .unwrap();
    assert!(out["stdout"].as_str().unwrap().contains("out"));

    // stderr via a command that fails on a missing path.
    let err = tool
        .execute(json!({ "command": "cat", "args": ["no_such_file_xyz"] }))
        .await
        .unwrap();
    assert!(!err["stderr"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn command_returns_exit_code() {
    let (tool, _temp) = allowed_tool();

    let ok = tool
        .execute(json!({ "command": "echo", "args": ["ok"] }))
        .await
        .unwrap();
    assert_eq!(ok["exit_code"], 0);

    // A missing file makes `cat` exit non-zero; the exact code is platform-dependent.
    let fail = tool
        .execute(json!({ "command": "cat", "args": ["no_such_file_xyz"] }))
        .await
        .unwrap();
    assert_ne!(fail["exit_code"], 0);
}

#[tokio::test]
async fn custom_working_directory_works() {
    let (tool, temp_dir) = allowed_tool();
    let subdir = temp_dir.path().join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("file.txt"), "contents").unwrap();

    let result = tool
        .execute(json!({
            "command": "cat",
            "args": ["file.txt"],
            "cwd": "subdir"
        }))
        .await
        .unwrap();

    assert_eq!(result["stdout"].as_str().unwrap().trim(), "contents");
}
