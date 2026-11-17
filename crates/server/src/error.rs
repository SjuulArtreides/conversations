//! Application error type and its HTTP/JSON representation.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, AppError>;

/// Errors surfaced from handlers and the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A requested resource was not found.
    #[error("not found")]
    NotFound,

    /// The request was malformed in a way the handler rejects.
    #[error("{0}")]
    BadRequest(String),

    /// The request body exceeds an accepted size cap (e.g. a pasted image). → 413.
    #[error("{0}")]
    PayloadTooLarge(String),

    /// Authentication failed (bad credentials, or no valid session).
    #[error("{0}")]
    Unauthorized(String),

    /// Authenticated, but not allowed to perform this action.
    #[error("{0}")]
    Forbidden(String),

    /// The request conflicts with existing state (e.g. account already exists).
    #[error("{0}")]
    Conflict(String),

    /// A dependency the request needs is not configured/available (e.g. the LLM
    /// features are requested but no `OPENROUTER_API_KEY` is set). → 503.
    #[error("{0}")]
    ServiceUnavailable(String),

    /// An upstream dependency returned an unusable response (e.g. the model
    /// returned non-JSON for a strict-JSON summary). → 502.
    #[error("{0}")]
    BadGateway(String),

    /// Storage / database failure.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// (De)serialization failure.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Anything else, surfaced as a 500.
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::Database(_) | AppError::Serde(_) | AppError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Log the full error server-side; only expose the display string to clients.
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        let body = Json(json!({ "error": self.to_string() }));
        (status, body).into_response()
    }
}
