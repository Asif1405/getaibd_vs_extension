use axum::extract::State;
use axum::Json;
use std::sync::Arc;

use crate::context::trim_to_context;
use crate::error::AppError;
use crate::models::{ChatRequest, ChatResponse};
use crate::retry::chat_with_retry;
use crate::state::AppState;

pub async fn chat_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, AppError> {
    if request.messages.is_empty() {
        return Err(AppError::InvalidRequest("messages array is empty".into()));
    }

    if let Err(retry_after) = state.rate_limiter.check(&request.provider).await {
        return Err(AppError::RateLimited(retry_after));
    }

    let mut request = request;

    let (trimmed_messages, was_trimmed) =
        trim_to_context(&request.messages, &request.model, &state.context_config);
    if was_trimmed {
        tracing::debug!(
            "Context trimmed: {} -> {} messages",
            request.messages.len(),
            trimmed_messages.len()
        );
    }
    request.messages = trimmed_messages;

    let provider = state
        .get_provider(&request.provider)
        .ok_or_else(|| AppError::UnknownProvider(request.provider.clone()))?;

    let response = chat_with_retry(&provider, &request).await?;

    Ok(Json(response))
}
