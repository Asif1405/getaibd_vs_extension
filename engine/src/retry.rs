use std::sync::Arc;
use std::time::Duration;

use crate::circuit_breaker::CircuitBreaker;
use crate::error::AppError;
use crate::models::{ChatRequest, ChatResponse, ToolChatRequest, ToolChatResponse};
use crate::providers::Provider;

pub async fn chat_with_retry(
    provider: &Arc<dyn Provider>,
    request: &ChatRequest,
) -> Result<ChatResponse, AppError> {
    let max = provider.max_retries();
    let mut last_err = None;

    for attempt in 0..=max {
        match provider.chat(request).await {
            Ok(resp) => return Ok(resp),
            Err(e) if e.is_retryable() && attempt < max => {
                let backoff_ms = 100 * 2u64.saturating_pow(attempt);
                tracing::warn!(
                    provider = provider.id(),
                    attempt = attempt + 1,
                    max_retries = max,
                    backoff_ms,
                    "Retrying after error: {e}",
                );
                let backoff = Duration::from_millis(backoff_ms);
                tokio::time::sleep(backoff).await;
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        AppError::ProviderError(format!("{}: all retries exhausted", provider.id()))
    }))
}

pub async fn chat_with_tools_retry(
    provider: &Arc<dyn Provider>,
    request: &ToolChatRequest,
) -> Result<ToolChatResponse, AppError> {
    chat_with_tools_retry_cb(provider, request, None).await
}

pub async fn chat_with_tools_retry_cb(
    provider: &Arc<dyn Provider>,
    request: &ToolChatRequest,
    circuit_breaker: Option<&CircuitBreaker>,
) -> Result<ToolChatResponse, AppError> {
    let provider_id = provider.id();

    // Check circuit breaker before attempting
    if let Some(cb) = circuit_breaker {
        if cb.is_open(&provider_id).await {
            return Err(AppError::ProviderError(format!(
                "{}: circuit breaker is open",
                provider_id
            )));
        }
    }

    let max = provider.max_retries();
    let mut last_err = None;

    for attempt in 0..=max {
        match provider.chat_with_tools(request).await {
            Ok(resp) => {
                // Record success
                if let Some(cb) = circuit_breaker {
                    cb.record_success(&provider_id).await;
                }
                return Ok(resp);
            }
            Err(e) if e.is_retryable() && attempt < max => {
                let backoff_ms = 100 * 2u64.saturating_pow(attempt);
                tracing::warn!(
                    provider = provider_id,
                    attempt = attempt + 1,
                    max_retries = max,
                    backoff_ms,
                    "Retrying tool chat after error: {e}",
                );

                // Record failure
                if let Some(cb) = circuit_breaker {
                    cb.record_failure(&provider_id).await;
                }

                let backoff = Duration::from_millis(backoff_ms);
                tokio::time::sleep(backoff).await;
                last_err = Some(e);
            }
            Err(e) => {
                // Non-retryable error, record and return
                if let Some(cb) = circuit_breaker {
                    cb.record_failure(&provider_id).await;
                }
                return Err(e);
            }
        }
    }

    // Record final failure
    if let Some(cb) = circuit_breaker {
        cb.record_failure(&provider_id).await;
    }

    Err(last_err.unwrap_or_else(|| {
        AppError::ProviderError(format!("{}: all tool retries exhausted", provider_id))
    }))
}
