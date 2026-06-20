use axum::http::StatusCode;
use axum::response::IntoResponse;
use mcp_universal::error::AppError;

fn extract_status(err: AppError) -> StatusCode {
    err.into_response().status()
}

async fn extract_body(err: AppError) -> serde_json::Value {
    let response = err.into_response();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn invalid_request_returns_400() {
    let err = AppError::InvalidRequest("bad input".into());
    assert_eq!(extract_status(err), StatusCode::BAD_REQUEST);
}

#[test]
fn unknown_provider_returns_400() {
    let err = AppError::UnknownProvider("foobar".into());
    assert_eq!(extract_status(err), StatusCode::BAD_REQUEST);
}

#[test]
fn provider_unavailable_returns_502() {
    let err = AppError::ProviderUnavailable("ollama".into());
    assert_eq!(extract_status(err), StatusCode::BAD_GATEWAY);
}

#[test]
fn provider_error_returns_502() {
    let err = AppError::ProviderError("openai".into());
    assert_eq!(extract_status(err), StatusCode::BAD_GATEWAY);
}

#[test]
fn provider_timeout_returns_504() {
    let err = AppError::ProviderTimeout("gemini".into());
    assert_eq!(extract_status(err), StatusCode::GATEWAY_TIMEOUT);
}

#[test]
fn retryable_errors() {
    assert!(AppError::ProviderUnavailable("x".into()).is_retryable());
    assert!(AppError::ProviderTimeout("x".into()).is_retryable());
    assert!(!AppError::ProviderError("x".into()).is_retryable());
    assert!(!AppError::InvalidRequest("x".into()).is_retryable());
    assert!(!AppError::UnknownProvider("x".into()).is_retryable());
}

#[tokio::test]
async fn error_body_has_correct_structure() {
    let err = AppError::UnknownProvider("foobar".into());
    let body = extract_body(err).await;

    assert!(body.get("error").is_some());
    let error = &body["error"];
    assert_eq!(error["code"], "UNKNOWN_PROVIDER");
    assert_eq!(error["provider"], "foobar");
    assert!(error["message"].as_str().unwrap().contains("foobar"));
}

#[tokio::test]
async fn invalid_request_has_no_provider_field() {
    let err = AppError::InvalidRequest("missing messages".into());
    let body = extract_body(err).await;

    let error = &body["error"];
    assert_eq!(error["code"], "INVALID_REQUEST");
    assert!(error.get("provider").is_none());
}

#[tokio::test]
async fn provider_unavailable_includes_provider_name() {
    let err = AppError::ProviderUnavailable("gemini".into());
    let body = extract_body(err).await;

    let error = &body["error"];
    assert_eq!(error["code"], "PROVIDER_UNAVAILABLE");
    assert_eq!(error["provider"], "gemini");
}

#[tokio::test]
async fn provider_error_includes_provider_name() {
    let err = AppError::ProviderError("claude".into());
    let body = extract_body(err).await;

    let error = &body["error"];
    assert_eq!(error["code"], "PROVIDER_ERROR");
    assert_eq!(error["provider"], "claude");
}

#[tokio::test]
async fn provider_timeout_includes_provider_name() {
    let err = AppError::ProviderTimeout("openai".into());
    let body = extract_body(err).await;

    let error = &body["error"];
    assert_eq!(error["code"], "PROVIDER_TIMEOUT");
    assert_eq!(error["provider"], "openai");
}
