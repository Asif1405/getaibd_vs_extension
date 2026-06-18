pub mod openai_compat;

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::error::AppError;
use crate::models::{
    ChatRequest, ChatResponse, ModelInfo, ProviderHealth, ToolChatRequest, ToolChatResponse,
    ToolStreamDelta,
};

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn max_retries(&self) -> u32 {
        3
    }
    fn supports_tool_calling(&self) -> bool {
        false
    }
    async fn health_check(&self) -> ProviderHealth;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError>;
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AppError>;
    fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>>;
    async fn chat_with_tools(
        &self,
        _request: &ToolChatRequest,
    ) -> Result<ToolChatResponse, AppError> {
        Err(AppError::ProviderError(format!(
            "{}: tool calling not supported",
            self.id()
        )))
    }

    fn supports_streaming_tools(&self) -> bool {
        false
    }

    fn chat_with_tools_stream(
        &self,
        _request: ToolChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ToolStreamDelta, AppError>> + Send>> {
        Box::pin(futures::stream::once(async { Ok(ToolStreamDelta::Done) }))
    }
}
