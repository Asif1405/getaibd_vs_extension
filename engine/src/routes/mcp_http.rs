use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::JsonRpcRequest;
use crate::state::AppState;
use crate::tools::ToolRegistry;

pub async fn mcp_message_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let registry = ToolRegistry::build_default(&state.project_root);
    let handler = McpHandler::new(registry);

    match handler.handle(request).await {
        Some(response) => (
            StatusCode::OK,
            Json(serde_json::to_value(response).unwrap_or_default()),
        )
            .into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}
