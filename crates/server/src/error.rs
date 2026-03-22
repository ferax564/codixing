use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use codixing_core::CodixingError;

/// API error type that converts to an HTTP response.
#[derive(Debug)]
#[allow(dead_code)]
pub enum ApiError {
    /// A codixing engine error.
    Engine(CodixingError),
    /// A bad request (client error).
    BadRequest(String),
    /// Internal server error.
    Internal(String),
}

impl From<CodixingError> for ApiError {
    fn from(e: CodixingError) -> Self {
        ApiError::Engine(e)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::Engine(CodixingError::IndexNotFound { .. }) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            ApiError::Engine(CodixingError::EmbeddingNotEnabled) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            ApiError::Engine(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };

        let body = json!({ "error": message });
        (status, axum::Json(body)).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Engine(e) => write!(f, "{e}"),
            ApiError::BadRequest(m) | ApiError::Internal(m) => write!(f, "{m}"),
        }
    }
}
