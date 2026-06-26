//! MCP client: spawn stdio servers from `.getaibd/mcp.json` and call their tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::error::AppError;
use crate::mcp::protocol::{
    JsonRpcRequest, JsonRpcResponse, McpToolInfo, ToolCallParams, ToolCallResult,
    ToolListResult, PROTOCOL_VERSION,
};

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Deserialize)]
struct McpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// One connected MCP server subprocess.
pub struct McpServerHandle {
    pub name: String,
    pub tools: Vec<McpToolInfo>,
    conn: Arc<Mutex<McpServerConnection>>,
    _child: Arc<Mutex<Child>>,
}

struct McpServerConnection {
    stdin: ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: AtomicU64,
}

impl McpServerConnection {
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, AppError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(id)),
            method: method.into(),
            params,
        };
        let line = serde_json::to_string(&req)
            .map_err(|e| AppError::ProviderError(format!("MCP encode: {e}")))?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AppError::ProviderError(format!("MCP write: {e}")))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| AppError::ProviderError(format!("MCP write: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| AppError::ProviderError(format!("MCP flush: {e}")))?;

        loop {
            let mut response_line = String::new();
            self.reader
                .read_line(&mut response_line)
                .await
                .map_err(|e| AppError::ProviderError(format!("MCP read: {e}")))?;
            let trimmed = response_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp: JsonRpcResponse = serde_json::from_str(trimmed)
                .map_err(|e| AppError::ProviderError(format!("MCP parse: {e}")))?;
            if resp.id.as_ref().is_some_and(|rid| rid == &json!(id)) {
                if let Some(err) = resp.error {
                    return Err(AppError::ProviderError(format!(
                        "MCP {}: {}",
                        err.code, err.message
                    )));
                }
                return resp
                    .result
                    .ok_or_else(|| AppError::ProviderError("MCP empty result".into()));
            }
        }
    }
}

fn substitute_env(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("${env:").and_then(|s| s.strip_suffix('}')) {
        return std::env::var(rest).unwrap_or_default();
    }
    value.to_string()
}

fn mcp_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".getaibd").join("mcp.json")
}

/// Connect all MCP servers declared in `.getaibd/mcp.json`. Failures are logged and skipped.
pub async fn connect_all(project_root: &Path) -> Vec<McpServerHandle> {
    let path = mcp_config_path(project_root);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_str::<McpConfigFile>(&raw) else {
        tracing::warn!("Invalid .getaibd/mcp.json — skipping MCP servers");
        return Vec::new();
    };

    let mut handles = Vec::new();
    for (name, cfg) in config.mcp_servers {
        match connect_server(&name, &cfg).await {
            Ok(handle) => handles.push(handle),
            Err(e) => tracing::warn!("MCP server '{name}' failed to start: {e}"),
        }
    }
    handles
}

async fn connect_server(name: &str, cfg: &McpServerConfig) -> Result<McpServerHandle, AppError> {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    for (k, v) in &cfg.env {
        cmd.env(k, substitute_env(v));
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::ProviderError(format!("MCP spawn {name}: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::ProviderError("MCP stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::ProviderError("MCP stdout unavailable".into()))?;

    let mut conn = McpServerConnection {
        stdin,
        reader: BufReader::new(stdout),
        next_id: AtomicU64::new(1),
    };

    let init_params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "getaibd-agent", "version": env!("CARGO_PKG_VERSION") }
    });
    conn.request("initialize", Some(init_params)).await?;

    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let line = serde_json::to_string(&initialized).unwrap_or_default();
    conn.stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| AppError::ProviderError(format!("MCP notify: {e}")))?;
    conn.stdin
        .write_all(b"\n")
        .await
        .map_err(|e| AppError::ProviderError(format!("MCP notify: {e}")))?;
    conn.stdin
        .flush()
        .await
        .map_err(|e| AppError::ProviderError(format!("MCP notify: {e}")))?;

    let list = conn.request("tools/list", None).await?;
    let tools: Vec<McpToolInfo> = serde_json::from_value::<ToolListResult>(list)
        .map(|r| r.tools)
        .unwrap_or_default();

    Ok(McpServerHandle {
        name: name.to_string(),
        tools,
        conn: Arc::new(Mutex::new(conn)),
        _child: Arc::new(Mutex::new(child)),
    })
}

impl McpServerHandle {
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value, AppError> {
        let params = ToolCallParams {
            name: tool_name.to_string(),
            arguments,
        };
        let result = self
            .conn
            .lock()
            .await
            .request("tools/call", Some(serde_json::to_value(params).unwrap_or_default()))
            .await?;
        let call_result: ToolCallResult = serde_json::from_value(result)
            .map_err(|e| AppError::ProviderError(format!("MCP tool result: {e}")))?;
        let text = call_result
            .content
            .first()
            .map(|c| c.text.clone())
            .unwrap_or_default();
        if call_result.is_error.unwrap_or(false) {
            return Err(AppError::ProviderError(text));
        }
        Ok(json!({ "output": text }))
    }
}

/// Sanitized registry name: `mcp_{server}_{tool}`.
pub fn mcp_tool_registry_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("mcp_{}_{}", sanitize(server), sanitize(tool))
}
