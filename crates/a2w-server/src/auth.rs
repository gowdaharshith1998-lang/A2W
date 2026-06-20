//! Per-request authentication middleware.
//!
//! When the server is started with the `A2W_API_KEY` environment variable, every
//! request other than the explicitly public set must carry an `Authorization:
//! Bearer <key>` header whose key matches exactly (constant-time compared).
//!
//! When the variable is **unset**, the middleware permits every request — the
//! server logs a startup warning. The fail-closed behaviour is therefore opt-in
//! via `A2W_API_KEY`; this matches the contract documented on
//! [`a2w_server::main`].
//!
//! ## Public paths
//! - `GET /` — the dashboard HTML (purely informational; it cannot drive the
//!   API without the same key, since every API call will be rejected).
//! - `GET /health` — Kubernetes / load-balancer liveness probe.
//!
//! All other routes are gated.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Shared auth configuration, cloned into the middleware closure.
#[derive(Clone, Default)]
pub struct AuthConfig {
    /// The expected API key, wrapped in an `Arc` so cloning is cheap and the
    /// key bytes are not duplicated. `None` means "no auth required".
    pub api_key: Option<Arc<String>>,
}

impl AuthConfig {
    /// Build a config from the `A2W_API_KEY` environment variable.
    ///
    /// An empty value is treated as "no key configured" rather than "empty key
    /// permitted" — that would let a misconfigured server accept any request.
    #[must_use]
    pub fn from_env() -> Self {
        let api_key = std::env::var("A2W_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(Arc::new);
        Self { api_key }
    }

    /// `true` when an API key is configured and the middleware will enforce it.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        self.api_key.is_some()
    }
}

/// Path-prefix denylist used to skip the auth check for public endpoints. The
/// check is exact-match on the path (no query string), so `/health?check=1`
/// is also public.
///
/// Audit-fix: `/ready` and `/metrics` are also public — the readiness probe
/// must always run (Kubernetes drains pods that fail it) and Prometheus
/// scrapers don't pass an Authorization header. Lock down Prometheus at the
/// network layer if needed.
fn is_public_path(path: &str) -> bool {
    matches!(path, "/" | "/health" | "/ready" | "/metrics")
}

/// Constant-time equality of two byte slices.
///
/// We avoid `subtle` and friends to keep the dep tree small: a single XOR/OR
/// loop is enough for the API-key compare and gives the standard
/// timing-independent property (length is leaked, key bytes are not).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Axum middleware that enforces [`AuthConfig`] on every request that is not a
/// public path. To be used with [`axum::middleware::from_fn_with_state`].
///
/// The expected `Authorization` header value is `Bearer <key>` (case-sensitive
/// on `Bearer`).
///
/// Errors are returned as `401` with a JSON body that mirrors the rest of the
/// API: `{ "error": "..." }`.
pub async fn require_api_key(
    State(cfg): State<AuthConfig>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = cfg.api_key.as_ref() else {
        // No key configured → pass everything through.
        return next.run(req).await;
    };

    if is_public_path(req.uri().path()) {
        return next.run(req).await;
    }

    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    // Audit-fix: the bare-key fallback widens the parse surface for no
    // operational benefit (curl accepts -H 'Authorization: Bearer KEY' just
    // fine). Require a Bearer prefix, case-insensitively, with at least one
    // space between scheme and credential.
    let presented = match header_value
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
    {
        Some((_, key)) => key.trim().to_string(),
        None => {
            return unauthorized(
                "missing or malformed Authorization header (expected `Bearer <key>`)",
            );
        }
    };
    if presented.is_empty() {
        return unauthorized("empty Bearer credential");
    }

    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return unauthorized("invalid API key");
    }

    next.run(req).await
}

/// Build a `401` response that matches the rest of the API's error shape.
fn unauthorized(msg: &str) -> Response {
    let body = Json(json!({ "error": msg }));
    (StatusCode::UNAUTHORIZED, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_correctness() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn auth_config_from_env_empty_is_none() {
        std::env::remove_var("A2W_API_KEY");
        assert!(AuthConfig::from_env().api_key.is_none());

        std::env::set_var("A2W_API_KEY", "   ");
        assert!(
            AuthConfig::from_env().api_key.is_none(),
            "whitespace-only value must be treated as unset"
        );

        std::env::set_var("A2W_API_KEY", "secret");
        let cfg = AuthConfig::from_env();
        assert_eq!(cfg.api_key.as_deref().map(String::as_str), Some("secret"));
        std::env::remove_var("A2W_API_KEY");
    }

    #[test]
    fn public_paths_are_classified() {
        assert!(is_public_path("/"));
        assert!(is_public_path("/health"));
        assert!(!is_public_path("/workflows"));
        assert!(!is_public_path("/credentials"));
    }
}
