use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::JsonRpcRequest;
use crate::state::AppState;
use crate::tools::ToolRegistry;

/// MCP HTTP transport with SSE streaming
#[allow(clippy::unused_async)]
pub async fn mcp_stream_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<JsonRpcRequest>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let registry = ToolRegistry::build_default(&state.project_root);
    let handler = McpHandler::new(registry);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);

    tokio::spawn(async move {
        if let Some(response) = handler.handle(request).await {
            let data = serde_json::to_string(&response).unwrap_or_default();
            let _ = tx
                .send(Ok(Event::default().event("message").data(data)))
                .await;
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
