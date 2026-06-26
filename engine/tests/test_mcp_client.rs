use mcp_universal::mcp::client::{connect_all, mcp_tool_registry_name};
use mcp_universal::tools::ToolRegistry;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test]
fn mcp_tool_registry_name_sanitizes() {
    assert_eq!(
        mcp_tool_registry_name("github", "create_issue"),
        "mcp_github_create_issue"
    );
    assert_eq!(
        mcp_tool_registry_name("my-server", "tool.name"),
        "mcp_my_server_tool_name"
    );
}

#[tokio::test]
async fn connect_all_missing_config_returns_empty() {
    let dir = TempDir::new().unwrap();
    assert!(connect_all(dir.path()).await.is_empty());
}

#[tokio::test]
async fn connect_all_invalid_json_is_skipped() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".getaibd")).unwrap();
    fs::write(dir.path().join(".getaibd/mcp.json"), "not json").unwrap();
    assert!(connect_all(dir.path()).await.is_empty());
}

#[tokio::test]
async fn build_for_session_survives_bad_mcp_server() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".getaibd")).unwrap();
    fs::write(
        dir.path().join(".getaibd/mcp.json"),
        json!({
            "mcpServers": {
                "broken": {
                    "command": "/nonexistent/mcp-server",
                    "args": []
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let registry = ToolRegistry::build_for_session(dir.path()).await;
    let names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"run_command".to_string()));
    assert!(names.contains(&"fetch_skill".to_string()));
    assert!(!names.iter().any(|n| n.starts_with("mcp_broken_")));
}

#[tokio::test]
async fn mock_stdio_mcp_server_registers_proxy_tool() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("mock_mcp.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    rid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        out = {"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}
    elif method == "tools/list":
        out = {"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object","properties":{}}}]}}
    else:
        continue
    print(json.dumps(out), flush=True)
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fs::create_dir_all(dir.path().join(".getaibd")).unwrap();
    fs::write(
        dir.path().join(".getaibd/mcp.json"),
        json!({
            "mcpServers": {
                "mock": {
                    "command": "python3",
                    "args": [script.to_string_lossy()]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let registry = ToolRegistry::build_for_session(dir.path()).await;
    let names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"mcp_mock_ping".to_string()));
}
