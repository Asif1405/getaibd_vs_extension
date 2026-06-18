use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

use crate::error::AppError;
use crate::state::AppState;

pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let Some(ref expected) = state.auth_token else {
        return Ok(next.run(req).await);
    };

    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == expected => Ok(next.run(req).await),
        _ => Err(AppError::Unauthorized(
            "Missing or invalid Bearer token".to_string(),
        )),
    }
}
