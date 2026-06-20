use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

use crate::context::trim_to_context;
use crate::error::AppError;
use crate::models::{ChatRequest, SseDoneEvent, SseErrorEvent, SseTokenEvent};
use crate::state::AppState;

#[allow(clippy::unused_async)]
pub async fn sse_chat_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, AppError> {
    if request.messages.is_empty() {
        return Err(AppError::InvalidRequest("messages array is empty".into()));
    }

    if let Err(retry_after) = state.rate_limiter.check(&request.provider).await {
        return Err(AppError::RateLimited(retry_after));
    }

    let provider = state
        .get_provider(&request.provider)
        .ok_or_else(|| AppError::UnknownProvider(request.provider.clone()))?;

    let mut request = request;
    let (trimmed, _) = trim_to_context(&request.messages, &request.model, &state.context_config);
    request.messages = trimmed;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);

    let stream = provider.chat_stream(request);

    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);

        while let Some(result) = stream.next().await {
            let event = match result {
                Ok(text) => {
                    let data =
                        serde_json::to_string(&SseTokenEvent { content: text }).unwrap_or_default();
                    Event::default().event("token").data(data)
                }
                Err(e) => {
                    let data = serde_json::to_string(&SseErrorEvent {
                        message: e.to_string(),
                        code: "PROVIDER_ERROR".to_string(),
                    })
                    .unwrap_or_default();
                    let event = Event::default().event("error").data(data);
                    let _ = tx.send(Ok(event)).await;
                    return;
                }
            };

            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }

        let done = serde_json::to_string(&SseDoneEvent { usage: None }).unwrap_or_default();
        let _ = tx.send(Ok(Event::default().event("done").data(done))).await;
    });

    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
