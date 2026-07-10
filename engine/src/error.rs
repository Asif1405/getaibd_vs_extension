#[cfg(feature = "server")]
use axum::http::StatusCode;
#[cfg(feature = "server")]
use axum::response::{IntoResponse, Response};
#[cfg(feature = "server")]
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Unknown provider: {0}")]
    UnknownProvider(String),

    #[error("Provider unavailable: {0}")]
    ProviderUnavailable(String),

    #[error("Provider error: {0}")]
    ProviderError(String),

    #[error("Provider timeout: {0}")]
    ProviderTimeout(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Rate limited: retry after {0}s")]
    RateLimited(u64),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Quota exceeded: {0}")]
    QuotaExceeded(String),

    #[error("Conflict: {0}")]
    Conflict(String),
}

#[cfg(feature = "server")]
#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

impl AppError {
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::UnknownProvider(_) => "UNKNOWN_PROVIDER",
            Self::ProviderUnavailable(_) => "PROVIDER_UNAVAILABLE",
            Self::ProviderError(_) => "PROVIDER_ERROR",
            Self::ProviderTimeout(_) => "PROVIDER_TIMEOUT",
            Self::Unauthorized(_) => "UNAUTHORIZED",
            Self::RateLimited(_) => "RATE_LIMITED",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::QuotaExceeded(_) => "QUOTA_EXCEEDED",
            Self::Conflict(_) => "CONFLICT",
        }
    }

    #[cfg(feature = "server")]
    fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) | Self::UnknownProvider(_) => StatusCode::BAD_REQUEST,
            Self::ProviderUnavailable(_) | Self::ProviderError(_) => StatusCode::BAD_GATEWAY,
            Self::ProviderTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::RateLimited(_) | Self::QuotaExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    fn provider_name(&self) -> Option<String> {
        match self {
            Self::ProviderUnavailable(p)
            | Self::ProviderError(p)
            | Self::UnknownProvider(p)
            | Self::ProviderTimeout(p) => Some(p.clone()),
            Self::InvalidRequest(_)
            | Self::Unauthorized(_)
            | Self::RateLimited(_)
            | Self::Forbidden(_)
            | Self::QuotaExceeded(_)
            | Self::Conflict(_) => None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ProviderUnavailable(_) | Self::ProviderTimeout(_)
        )
    }
}

#[cfg(feature = "server")]
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code().to_string(),
                message: self.to_string(),
                provider: self.provider_name(),
            },
        };
        (status, axum::Json(body)).into_response()
    }
}
