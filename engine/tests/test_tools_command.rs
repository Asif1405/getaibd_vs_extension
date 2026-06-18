use mcp_universal::tools::command::{default_allowlist, RunCommand};
use mcp_universal::tools::env_manager::EnvManager;
use mcp_universal::tools::Tool;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::TempDir;

fn allowed_tool() -> (RunCommand, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let root = Arc::new(temp_dir.path().to_path_buf());
    let env_mgr = EnvManager::new(root.clone());
    let tool = RunCommand::new(root, default_allowlist(), env_mgr);
    (tool, temp_dir)
}

fn disallowed_tool() -> (RunCommand, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let root = Arc::new(temp_dir.path().to_path_buf());
    let allowlist: HashSet<String> = ["ls", "cat"].into_iter().map(String::from).collect();
    let env_mgr = EnvManager::new(root.clone());
    let tool = RunCommand::new(root, allowlist, env_mgr);
    (tool, temp_dir)
}

fn tool_with_sh() -> (RunCommand, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let root = Arc::new(temp_dir.path().to_path_buf());
    let mut allowlist = default_allowlist();
    allowlist.insert("sh".to_string());
    let env_mgr = EnvManager::new(root.clone());
    let tool = RunCommand::new(root, allowlist, env_mgr);
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
async fn disallowed_command_returns_error() {
    let (tool, _temp) = disallowed_tool();

    let result = tool
        .execute(json!({
            "command": "echo",
            "args": ["hello"]
        }))
        .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not allowed"));
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
    let (tool, _temp) = tool_with_sh();

    let result = tool
        .execute(json!({
            "command": "sh",
            "args": ["-c", "echo out; echo err >&2"]
        }))
        .await
        .unwrap();

    assert!(result["stdout"].as_str().unwrap().contains("out"));
    assert!(result["stderr"].as_str().unwrap().contains("err"));
}

#[tokio::test]
async fn command_returns_exit_code() {
    let (tool, _temp) = tool_with_sh();

    let ok = tool
        .execute(json!({
            "command": "sh",
            "args": ["-c", "exit 0"]
        }))
        .await
        .unwrap();
    assert_eq!(ok["exit_code"], 0);

    let fail = tool
        .execute(json!({
            "command": "sh",
            "args": ["-c", "exit 42"]
        }))
        .await
        .unwrap();
    assert_eq!(fail["exit_code"], 42);
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
