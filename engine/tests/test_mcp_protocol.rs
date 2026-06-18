use mcp_universal::mcp::handler::McpHandler;
use mcp_universal::mcp::protocol::{
    JsonRpcRequest, JSONRPC_VERSION, METHOD_NOT_FOUND, PROTOCOL_VERSION,
};
use mcp_universal::tools::ToolRegistry;
use serde_json::json;
use tempfile::TempDir;

fn make_handler() -> (McpHandler, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let registry = ToolRegistry::build_default(temp_dir.path());
    let handler = McpHandler::new(registry);
    (handler, temp_dir)
}

#[tokio::test]
async fn initialize_returns_server_info_with_protocol_version() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 1,
        "method": "initialize",
        "params": null
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(result["serverInfo"]["name"], "mcp-universal");
    assert!(result["serverInfo"]["version"].as_str().is_some());
}

#[tokio::test]
async fn initialized_notification_returns_none() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": "initialized",
        "params": null
    }))
    .unwrap();

    let resp = handler.handle(req).await;

    assert!(resp.is_none());
}

#[tokio::test]
async fn tools_list_returns_list_of_tool_definitions() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 2,
        "method": "tools/list",
        "params": null
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    let tools = result["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"run_command"));
}

#[tokio::test]
async fn tools_call_with_valid_tool_executes_tool() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": "echo",
                "args": ["hello"]
            }
        }
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.contains("hello"));
}

#[tokio::test]
async fn tools_call_with_unknown_tool_returns_error() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "nonexistent_tool",
            "arguments": {}
        }
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, METHOD_NOT_FOUND);
    assert!(err.message.contains("Unknown tool"));
}

#[tokio::test]
async fn tools_call_with_missing_params_returns_error() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 5,
        "method": "tools/call",
        "params": null
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("Missing params"));
}

#[tokio::test]
async fn ping_returns_empty_success() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 6,
        "method": "ping",
        "params": null
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    assert!(result.as_object().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_method_returns_method_not_found_error() {
    let (handler, _temp) = make_handler();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": 7,
        "method": "unknown/method",
        "params": null
    }))
    .unwrap();

    let resp = handler.handle(req).await.unwrap();

    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, METHOD_NOT_FOUND);
    assert!(err.message.contains("Method not found"));
}
