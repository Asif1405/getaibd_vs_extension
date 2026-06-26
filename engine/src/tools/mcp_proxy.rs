use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::error::AppError;
use crate::mcp::client::{McpServerHandle, mcp_tool_registry_name};
use crate::tools::Tool;

pub struct McpProxyTool {
    registry_name: &'static str,
    description: &'static str,
    server: Arc<McpServerHandle>,
    remote_name: String,
    input_schema: Value,
}

impl McpProxyTool {
    pub fn new(
        server: Arc<McpServerHandle>,
        remote_name: String,
        description: String,
        input_schema: Value,
    ) -> Self {
        let registry_name =
            Box::leak(mcp_tool_registry_name(&server.name, &remote_name).into_boxed_str());
        let description = Box::leak(description.into_boxed_str());
        Self {
            registry_name,
            description,
            server,
            remote_name,
            input_schema,
        }
    }
}

#[async_trait]
impl Tool for McpProxyTool {
    fn name(&self) -> &'static str {
        self.registry_name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        self.server.call_tool(&self.remote_name, input).await
    }
}
