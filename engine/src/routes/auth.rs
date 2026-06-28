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
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err(AppError::Unauthorized(
            "Missing or invalid Bearer token".to_string(),
        )),
    }
}

/// Length-then-content comparison that does not short-circuit on the first
/// differing byte, so a remote caller cannot use response timing as an oracle to
/// recover the token byte-by-byte. The length itself is not secret (the token is a
/// fixed-length UUID), so an early length check is acceptable.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn equal_tokens_match() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
    }

    #[test]
    fn different_content_same_len_fails() {
        assert!(!constant_time_eq(b"abc123", b"abc124"));
    }

    #[test]
    fn different_length_fails() {
        assert!(!constant_time_eq(b"abc", b"abc123"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
