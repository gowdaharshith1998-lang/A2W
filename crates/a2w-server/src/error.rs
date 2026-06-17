//! The API error type and its mapping onto HTTP responses.
//!
//! Every handler returns `Result<_, ApiError>`; [`ApiError`] implements
//! [`IntoResponse`] so a failure is serialized as a clean JSON body
//! (`{ "error": "..." }`) with the appropriate status code. This keeps handlers
//! free of `unwrap`/`expect`: fallible calls use `?` and the conversion does the
//! right thing.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use a2w_engine::EngineError;
use a2w_store::StoreError;

/// A handler-level error that knows how to become an HTTP response.
#[derive(Debug)]
pub enum ApiError {
    /// The requested resource (workflow or run) does not exist → `404`.
    NotFound(String),
    /// The request body or parameters were malformed/inconsistent → `400`.
    BadRequest(String),
    /// The engine rejected or failed the run (e.g. invalid workflow, node
    /// failure) → `422 Unprocessable Entity`.
    Unprocessable(String),
    /// A persistence-layer failure that is not the caller's fault → `500`.
    Internal(String),
}

impl ApiError {
    /// The HTTP status this error maps to.
    fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The human-readable message carried in the JSON body.
    fn message(&self) -> &str {
        match self {
            ApiError::NotFound(m)
            | ApiError::BadRequest(m)
            | ApiError::Unprocessable(m)
            | ApiError::Internal(m) => m,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = Json(json!({ "error": self.message() }));
        (status, body).into_response()
    }
}

/// Store failures are infrastructure failures from the caller's perspective:
/// map them to `500` with the underlying message.
impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        ApiError::Internal(err.to_string())
    }
}

/// Engine failures (invalid workflow, a node that failed under a Stop policy,
/// a missing executor) are about the *content* of the request, so they map to
/// `422` rather than `500`.
impl From<EngineError> for ApiError {
    fn from(err: EngineError) -> Self {
        ApiError::Unprocessable(err.to_string())
    }
}
