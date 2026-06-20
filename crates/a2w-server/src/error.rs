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
    /// The caller is unauthenticated (`401`). Returned by the API-key middleware
    /// when `A2W_API_KEY` is set but the request omits or mismatches it.
    Unauthorized(String),
    /// The request asked for a feature that the server is not configured for
    /// (e.g. credential endpoints when `A2W_MASTER_KEY` is unset) → `503`.
    ServiceUnavailable(String),
    /// Resource state prevents fulfilment (e.g. idempotency key is in
    /// progress, approval already decided) → `409`.
    Conflict(String),
    /// A downstream/persistence dependency partly succeeded and partly
    /// failed; the caller should not treat the request as cleanly
    /// completed → `502`. Used by the idempotency 2-phase commit when the
    /// engine + save_run succeeded but the bookkeeping update failed.
    BadGateway(String),
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
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The human-readable message carried in the JSON body.
    fn message(&self) -> &str {
        match self {
            ApiError::NotFound(m)
            | ApiError::BadRequest(m)
            | ApiError::Unprocessable(m)
            | ApiError::Unauthorized(m)
            | ApiError::ServiceUnavailable(m)
            | ApiError::Conflict(m)
            | ApiError::BadGateway(m)
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

/// Map engine failures onto HTTP-appropriate status codes.
///
/// R4 audit-fix: `EngineError::Internal` is a server-side invariant violation
/// (corrupt step_records on resume, scheduler bug) and MUST surface as 500 —
/// previously we returned 422 which incorrectly told the caller "fix your
/// request". The other variants are caller-facing and stay 422.
impl From<EngineError> for ApiError {
    fn from(err: EngineError) -> Self {
        match &err {
            EngineError::Internal(_) => ApiError::Internal(err.to_string()),
            EngineError::Invalid(_)
            | EngineError::NoExecutorForKind(_)
            | EngineError::NodeFailed { .. } => ApiError::Unprocessable(err.to_string()),
        }
    }
}
