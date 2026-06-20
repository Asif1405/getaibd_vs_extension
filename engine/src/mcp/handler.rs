use serde_json::{json, Value};

use crate::tools::ToolRegistry;

use super::protocol::{
    InitializeResult, JsonRpcRequest, JsonRpcResponse, McpToolInfo, ServerCapabilities, ServerInfo,
    ToolCallParams, ToolCallResult, ToolListResult, ToolResultContent, ToolsCapability,
    INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, PROTOCOL_VERSION,
};

pub struct McpHandler {
    registry: ToolRegistry,
}

impl McpHandler {
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    pub async fn handle(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        match request.method.as_str() {
            "initialize" => Some(Self::handle_initialize(request.id)),
            "initialized" => None,
            "tools/list" => Some(self.handle_tools_list(request.id)),
            "tools/call" => Some(self.handle_tools_call(request.id, request.params).await),
            "ping" => Some(JsonRpcResponse::success(request.id, json!({}))),
            _ => Some(JsonRpcResponse::error(
                request.id,
                METHOD_NOT_FOUND,
                format!("Method not found: {}", request.method),
            )),
        }
    }

    fn handle_initialize(id: Option<Value>) -> JsonRpcResponse {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {
                    list_changed: false,
                },
            },
            server_info: ServerInfo {
                name: "mcp-universal".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };
        JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
    }

    fn handle_tools_list(&self, id: Option<Value>) -> JsonRpcResponse {
        let tools: Vec<McpToolInfo> = self
            .registry
            .definitions()
            .into_iter()
            .map(|d| McpToolInfo {
                name: d.name,
                description: d.description,
                input_schema: d.input_schema,
            })
            .collect();
        let result = ToolListResult { tools };
        JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
    }

    async fn handle_tools_call(&self, id: Option<Value>, params: Option<Value>) -> JsonRpcResponse {
        let Some(params) = params else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing params");
        };

        let Ok(call) = serde_json::from_value::<ToolCallParams>(params) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid params");
        };

        let Some(tool) = self.registry.get(&call.name) else {
            return JsonRpcResponse::error(
                id,
                METHOD_NOT_FOUND,
                format!("Unknown tool: {}", call.name),
            );
        };

        match tool.execute(call.arguments).await {
            Ok(result) => {
                let text = serde_json::to_string_pretty(&result).unwrap_or_default();
                let call_result = ToolCallResult {
                    content: vec![ToolResultContent {
                        content_type: "text".into(),
                        text,
                    }],
                    is_error: None,
                };
                JsonRpcResponse::success(id, serde_json::to_value(call_result).unwrap_or_default())
            }
            Err(e) => {
                let call_result = ToolCallResult {
                    content: vec![ToolResultContent {
                        content_type: "text".into(),
                        text: e.to_string(),
                    }],
                    is_error: Some(true),
                };
                JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    serde_json::to_string(&call_result).unwrap_or_default(),
                )
            }
        }
    }
}
