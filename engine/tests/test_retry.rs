use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use mcp_universal::error::AppError;
use mcp_universal::models::{ChatRequest, ChatResponse, Message, ModelInfo, ProviderHealth};
use mcp_universal::providers::Provider;
use mcp_universal::retry::chat_with_retry;

struct FlakyProvider {
    fail_count: AtomicU32,
    retries: u32,
}

impl FlakyProvider {
    fn new(fail_times: u32, retries: u32) -> Self {
        Self {
            fail_count: AtomicU32::new(fail_times),
            retries,
        }
    }
}

#[async_trait]
impl Provider for FlakyProvider {
    fn id(&self) -> &'static str {
        "flaky"
    }
    fn display_name(&self) -> &'static str {
        "Flaky Test Provider"
    }
    fn max_retries(&self) -> u32 {
        self.retries
    }
    async fn health_check(&self) -> ProviderHealth {
        ProviderHealth {
            provider: "flaky".into(),
            healthy: true,
            message: None,
        }
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        Ok(vec![])
    }
    async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, AppError> {
        let remaining = self.fail_count.fetch_sub(1, Ordering::SeqCst);
        if remaining > 0 {
            return Err(AppError::ProviderUnavailable(
                "flaky: connection reset".into(),
            ));
        }
        Ok(ChatResponse {
            provider: "flaky".into(),
            model: "test".into(),
            content: "success".into(),
            usage: None,
        })
    }
    fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>> {
        Box::pin(futures::stream::empty())
    }
}

fn test_request() -> ChatRequest {
    ChatRequest {
        provider: "flaky".into(),
        model: "test".into(),
        messages: vec![Message {
            role: "user".into(),
            content: "hi".into(),
        }],
        temperature: None,
        max_tokens: None,
        api_key: None,
    }
}

#[tokio::test]
async fn succeeds_after_retries() {
    let provider: Arc<dyn Provider> = Arc::new(FlakyProvider::new(2, 3));
    let result = chat_with_retry(&provider, &test_request()).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().content, "success");
}

#[tokio::test]
async fn succeeds_immediately_without_retries() {
    let provider: Arc<dyn Provider> = Arc::new(FlakyProvider::new(0, 3));
    let result = chat_with_retry(&provider, &test_request()).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn fails_after_all_retries_exhausted() {
    let provider: Arc<dyn Provider> = Arc::new(FlakyProvider::new(5, 2));
    let result = chat_with_retry(&provider, &test_request()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn non_retryable_error_fails_immediately() {
    struct AlwaysBadRequest;

    #[async_trait]
    impl Provider for AlwaysBadRequest {
        fn id(&self) -> &'static str {
            "bad"
        }
        fn display_name(&self) -> &'static str {
            "Bad"
        }
        fn max_retries(&self) -> u32 {
            3
        }
        async fn health_check(&self) -> ProviderHealth {
            ProviderHealth {
                provider: "bad".into(),
                healthy: false,
                message: None,
            }
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
            Ok(vec![])
        }
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, AppError> {
            Err(AppError::ProviderError("bad: permanent failure".into()))
        }
        fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>> {
            Box::pin(futures::stream::empty())
        }
    }

    let provider: Arc<dyn Provider> = Arc::new(AlwaysBadRequest);
    let result = chat_with_retry(&provider, &test_request()).await;
    assert!(result.is_err());
}
