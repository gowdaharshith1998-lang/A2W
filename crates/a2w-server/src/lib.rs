//! # a2w-server
//!
//! Milestone **M7 — read-only REST + observability server**. A small [`axum`]
//! `0.8` application over [`a2w_store`] (persistence) and [`a2w_engine`]
//! (dry-run execution), exposing:
//!
//! - workflow CRUD-ish endpoints (`GET`/`PUT`/`DELETE /workflows/{id}`, list),
//! - run history (`GET /workflows/{id}/runs`, `GET /runs/{run_id}`),
//! - on-demand `validate` and `dry_run` of a stored workflow, and
//! - a dependency-free single-page HTML observability dashboard at `/`.
//!
//! The router is built by [`app`] from an [`AppState`], with no I/O of its own,
//! so it is exercised in tests with [`tower::ServiceExt::oneshot`] (no socket).
//! `main.rs` wires the store + listener around it.
//!
//! ## axum 0.8 notes
//! - Path parameters use the brace syntax in the route string (`/{id}`,
//!   `/{run_id}`) and are extracted with [`axum::extract::Path`].
//! - Handlers return `impl IntoResponse`; errors flow through [`ApiError`],
//!   which implements [`axum::response::IntoResponse`].

#![forbid(unsafe_code)]

mod auth;
mod dashboard;
mod error;
mod handlers;
mod metrics;
mod state;

pub use auth::AuthConfig;
pub use error::ApiError;
pub use metrics::MetricsHandle;
pub use state::AppState;

use std::time::Duration;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceBuilder;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Header used to carry the request id (both incoming and outgoing).
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Tunable knobs for [`app_with_config`].
///
/// [`Default`] matches the unhardened M7 behaviour: no API-key enforcement,
/// 1 MiB body limit, 30 s request timeout, and request-id middleware enabled.
/// Production deployments should override `auth` via
/// [`AuthConfig::from_env`] and tune the limits to fit their threat model.
#[derive(Clone)]
pub struct ServerConfig {
    /// Auth gate. `is_enforced() == false` means every request is permitted.
    pub auth: AuthConfig,
    /// Maximum request body size in bytes (default 1 MiB).
    pub max_body_bytes: usize,
    /// Per-request timeout (default 30 s). Applies end-to-end including
    /// handler work and body streaming.
    pub request_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            auth: AuthConfig::default(),
            max_body_bytes: 1024 * 1024,
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl ServerConfig {
    /// Build a `ServerConfig` from environment variables. Falls back to the
    /// [`Default`] values for any var that is unset or unparseable.
    ///
    /// - `A2W_API_KEY` — when set, gates all non-public routes.
    /// - `A2W_MAX_BODY_BYTES` — request body size cap in bytes (default 1 MiB).
    /// - `A2W_REQUEST_TIMEOUT_SECS` — per-request timeout (default 30 s).
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let max_body_bytes = std::env::var("A2W_MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(defaults.max_body_bytes);
        let request_timeout = std::env::var("A2W_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(defaults.request_timeout);
        Self {
            auth: AuthConfig::from_env(),
            max_body_bytes,
            request_timeout,
        }
    }
}

/// Build the application router over `state`.
///
/// Pure construction — no binding or listening — so it can be driven directly
/// with `tower`'s `oneshot` in tests.
///
/// ## Route table
/// | Method   | Path                          | Handler                      |
/// |----------|-------------------------------|------------------------------|
/// | `GET`    | `/`                           | [`handlers::dashboard`]      |
/// | `GET`    | `/health`                     | [`handlers::health`]         |
/// | `GET`    | `/workflows`                  | [`handlers::list_workflows`] |
/// | `GET`    | `/workflows/{id}`             | [`handlers::get_workflow`]   |
/// | `PUT`    | `/workflows/{id}`             | [`handlers::put_workflow`]   |
/// | `DELETE` | `/workflows/{id}`             | [`handlers::delete_workflow`]|
/// | `GET`    | `/workflows/{id}/runs`        | [`handlers::list_runs`]      |
/// | `POST`   | `/workflows/{id}/validate`    | [`handlers::validate_workflow`] |
/// | `POST`   | `/workflows/{id}/dry_run`     | [`handlers::dry_run`]        |
/// | `GET`    | `/runs/{run_id}`              | [`handlers::get_run`]        |
/// | `GET`    | `/credentials`                | [`handlers::list_credentials`] |
/// | `POST`   | `/credentials`                | [`handlers::store_credential`] |
/// | `DELETE` | `/credentials/{id}`           | [`handlers::delete_credential`] |
///
/// `Router` is itself `#[must_use]`, so no extra attribute is needed here.
///
/// Equivalent to `app_with_config(state, ServerConfig::default())`.
pub fn app(state: AppState) -> Router {
    app_with_config(state, ServerConfig::default())
}

/// Build the application router with explicit hardening config.
///
/// Layers applied (outermost first):
/// 1. [`SetRequestIdLayer`] — accept an incoming `x-request-id`, otherwise mint
///    a UUIDv4. Carried into every downstream extension and back into the
///    response via [`PropagateRequestIdLayer`].
/// 2. [`TraceLayer`] — tracing spans per request (uses the request id).
/// 3. [`TimeoutLayer`] — request-scoped timeout. Excess requests get `408`.
/// 4. [`RequestBodyLimitLayer`] — caps body size. Excess gets `413 Payload Too
///    Large`.
/// 5. API-key middleware — see [`auth::require_api_key`].
pub fn app_with_config(state: AppState, cfg: ServerConfig) -> Router {
    app_with_config_and_metrics(state, cfg, MetricsHandle::install())
}

/// Variant of [`app_with_config`] that takes a pre-installed [`MetricsHandle`].
/// Useful in tests so the recorder install error path is not exercised on
/// every router build.
pub fn app_with_config_and_metrics(
    state: AppState,
    cfg: ServerConfig,
    metrics_handle: MetricsHandle,
) -> Router {
    let auth_cfg = cfg.auth.clone();
    // Layer ordering (outermost first — first `.layer()` call wraps last):
    //   SetRequestId → Trace → PropagateRequestId → RequestBodyLimit → Timeout
    //
    // `TimeoutLayer` must be INSIDE `RequestBodyLimitLayer` because Timeout
    // constructs a fallback 408 response that requires the inner service's
    // response body to implement `Default`; `RequestBodyLimit::Response::Body`
    // is `ResponseBody<_>`, which is not `Default`, but axum's `Body` is.
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::new(
            REQUEST_ID_HEADER.parse().expect("static header name parses"),
            MakeRequestUuid,
        ))
        .layer(
            TraceLayer::new_for_http().make_span_with(
                tower_http::trace::DefaultMakeSpan::new().include_headers(false),
            ),
        )
        .layer(PropagateRequestIdLayer::new(
            REQUEST_ID_HEADER.parse().expect("static header name parses"),
        ))
        .layer(RequestBodyLimitLayer::new(cfg.max_body_bytes))
        .layer(TimeoutLayer::with_status_code(
            http::StatusCode::REQUEST_TIMEOUT,
            cfg.request_timeout,
        ));

    Router::new()
        .route("/", get(handlers::dashboard))
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::ready))
        .route("/workflows", get(handlers::list_workflows))
        .route(
            "/workflows/{id}",
            get(handlers::get_workflow)
                .put(handlers::put_workflow)
                .delete(handlers::delete_workflow),
        )
        .route("/workflows/{id}/runs", get(handlers::list_runs))
        .route("/workflows/{id}/validate", post(handlers::validate_workflow))
        .route("/workflows/{id}/dry_run", post(handlers::dry_run))
        .route("/workflows/{id}/run", post(handlers::run_workflow))
        .route("/runs/{run_id}/resume", post(handlers::resume_run))
        .route("/approvals", get(handlers::list_approvals))
        .route(
            "/approvals/{id}",
            get(handlers::get_approval).post(handlers::decide_approval),
        )
        .route("/runs/{run_id}", get(handlers::get_run))
        .route(
            "/credentials",
            get(handlers::list_credentials).post(handlers::store_credential),
        )
        .route(
            "/credentials/{id}",
            axum::routing::delete(handlers::delete_credential),
        )
        // Auth wraps the routes only; SetRequestId / TimeoutLayer etc. wrap
        // everything (including the auth check itself) via the ServiceBuilder
        // below.
        .layer(from_fn_with_state(auth_cfg, auth::require_api_key))
        .layer(axum::middleware::from_fn(metrics::track_requests))
        .layer(middleware)
        .with_state(state)
        // /metrics needs its own state (the MetricsHandle); merge it as a
        // sibling router that doesn't carry AppState.
        .merge(
            Router::new()
                .route("/metrics", get(metrics::handler))
                .with_state(metrics_handle),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt; // for `oneshot`

    use a2w_store::{Store, Vault};

    /// Build an `app` backed by a fresh in-memory store, **without** a vault.
    async fn test_app() -> Router {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("connect in-memory store");
        app(AppState::new(store))
    }

    /// Build an `app` backed by a fresh in-memory store **with** a deterministic
    /// vault, so the `/credentials` endpoints are live.
    async fn test_app_with_vault() -> Router {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("connect in-memory store");
        let vault = Vault::new([42u8; 32]);
        app(AppState::with_vault(store, vault))
    }

    /// Read a response body fully into a JSON value.
    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body bytes");
        serde_json::from_slice(&bytes).expect("body is valid JSON")
    }

    /// Read a response body fully into a UTF-8 string.
    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body bytes");
        String::from_utf8(bytes.to_vec()).expect("body is valid UTF-8")
    }

    /// `GET /<path>` request helper.
    fn get(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("build GET request")
    }

    /// `<method> <path>` with a JSON body.
    fn json_req(method: &str, path: &str, body: &Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).expect("serialize body")))
            .expect("build JSON request")
    }

    /// `POST <path>` with no body.
    fn post_empty(path: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .expect("build POST request")
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = test_app().await;
        let resp = app.oneshot(get("/health")).await.expect("health response");
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn put_then_get_workflow_and_list() {
        let app = test_app().await;
        let wf = a2w_ir::sample_workflow();
        let wf_json = serde_json::to_value(&wf).expect("workflow to json");

        // PUT /workflows/wf_sample
        let resp = app
            .clone()
            .oneshot(json_req("PUT", "/workflows/wf_sample", &wf_json))
            .await
            .expect("put response");
        assert_eq!(resp.status(), StatusCode::OK);
        let saved = body_json(resp).await;
        assert_eq!(saved["saved"], "wf_sample");

        // GET /workflows contains it.
        let resp = app
            .clone()
            .oneshot(get("/workflows"))
            .await
            .expect("list response");
        assert_eq!(resp.status(), StatusCode::OK);
        let list = body_json(resp).await;
        let arr = list.as_array().expect("workflows is an array");
        assert!(
            arr.iter()
                .any(|w| w["id"] == "wf_sample" && w["name"] == wf.name),
            "listing must contain the saved workflow: {list:?}"
        );

        // GET /workflows/wf_sample returns it.
        let resp = app
            .oneshot(get("/workflows/wf_sample"))
            .await
            .expect("get response");
        assert_eq!(resp.status(), StatusCode::OK);
        let got = body_json(resp).await;
        assert_eq!(got["id"], "wf_sample");
        assert_eq!(got["name"], wf.name);
    }

    #[tokio::test]
    async fn dry_run_persists_and_is_retrievable() {
        let app = test_app().await;
        // The sample workflow's HttpRequest node carries empty params; its
        // dry_run still resolves the `url`, so give it one before storing so the
        // dry run can complete (rather than failing with BadParams).
        let mut wf = a2w_ir::sample_workflow();
        for node in &mut wf.nodes {
            if node.kind == a2w_ir::NodeKind::HttpRequest {
                node.params = serde_json::json!({ "url": "https://example.com/" });
            }
        }
        let wf_json = serde_json::to_value(&wf).expect("workflow to json");

        // Store the workflow first.
        let resp = app
            .clone()
            .oneshot(json_req("PUT", "/workflows/wf_sample", &wf_json))
            .await
            .expect("put response");
        assert_eq!(resp.status(), StatusCode::OK);

        // POST /workflows/wf_sample/dry_run (no body → default trigger input).
        let resp = app
            .clone()
            .oneshot(post_empty("/workflows/wf_sample/dry_run"))
            .await
            .expect("dry_run response");
        assert_eq!(resp.status(), StatusCode::OK);
        let result = body_json(resp).await;
        assert_eq!(result["status"], "completed");
        let run_id = result["run_id"]
            .as_str()
            .expect("run_id present")
            .to_string();

        // GET /workflows/wf_sample/runs is non-empty and contains the run id.
        let resp = app
            .clone()
            .oneshot(get("/workflows/wf_sample/runs"))
            .await
            .expect("runs response");
        assert_eq!(resp.status(), StatusCode::OK);
        let runs = body_json(resp).await;
        let runs_arr = runs.as_array().expect("runs is an array");
        assert!(!runs_arr.is_empty(), "runs must be non-empty");
        assert!(
            runs_arr.iter().any(|r| r == &Value::String(run_id.clone())),
            "runs must contain the new run id {run_id}: {runs:?}"
        );

        // GET /runs/{run_id} returns the persisted run.
        let resp = app
            .oneshot(get(&format!("/runs/{run_id}")))
            .await
            .expect("run response");
        assert_eq!(resp.status(), StatusCode::OK);
        let stored = body_json(resp).await;
        assert_eq!(stored["run_id"], run_id);
        assert_eq!(stored["workflow_id"], "wf_sample");
        assert_eq!(stored["status"], "completed");
        assert!(
            stored["events"].as_array().is_some_and(|e| !e.is_empty()),
            "persisted run must carry events: {stored:?}"
        );
    }

    #[tokio::test]
    async fn missing_resources_return_404() {
        let app = test_app().await;

        let resp = app
            .clone()
            .oneshot(get("/workflows/missing"))
            .await
            .expect("missing workflow response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .oneshot(get("/runs/missing"))
            .await
            .expect("missing run response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn credentials_without_vault_return_503() {
        let app = test_app().await;

        // POST is 503 when the vault is unconfigured.
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/credentials",
                &serde_json::json!({ "id": "k", "name": "K", "secret": "s" }),
            ))
            .await
            .expect("post response");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        // GET is 503 when the vault is unconfigured.
        let resp = app
            .clone()
            .oneshot(get("/credentials"))
            .await
            .expect("list response");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        // DELETE is 503 when the vault is unconfigured.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/credentials/k")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("delete response");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn credentials_post_then_list_and_delete_round_trip() {
        let app = test_app_with_vault().await;

        // POST a credential.
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/credentials",
                &serde_json::json!({ "id": "k1", "name": "Token", "secret": "topsecret" }),
            ))
            .await
            .expect("post response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["saved"], "k1");

        // GET the listing includes the new credential without leaking the secret.
        let resp = app
            .clone()
            .oneshot(get("/credentials"))
            .await
            .expect("list response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(
            body.contains("k1") && body.contains("Token"),
            "listing must contain id+name: {body}"
        );
        assert!(
            !body.contains("topsecret"),
            "listing must NEVER leak the plaintext secret: {body}"
        );

        // DELETE removes it.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/credentials/k1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("delete response");
        assert_eq!(resp.status(), StatusCode::OK);

        // GET after delete is empty.
        let resp = app
            .oneshot(get("/credentials"))
            .await
            .expect("list-after-delete response");
        assert_eq!(resp.status(), StatusCode::OK);
        let arr = body_json(resp).await;
        assert!(
            arr.as_array().is_some_and(|a| a.is_empty()),
            "credential listing must be empty after delete: {arr}"
        );
    }

    #[tokio::test]
    async fn credentials_post_rejects_empty_fields() {
        let app = test_app_with_vault().await;
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/credentials",
                &serde_json::json!({ "id": "", "name": "x", "secret": "y" }),
            ))
            .await
            .expect("post response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "empty id rejected");

        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/credentials",
                &serde_json::json!({ "id": "k", "name": "", "secret": "y" }),
            ))
            .await
            .expect("post response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "empty name rejected");

        let resp = app
            .oneshot(json_req(
                "POST",
                "/credentials",
                &serde_json::json!({ "id": "k", "name": "n", "secret": "" }),
            ))
            .await
            .expect("post response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "empty secret rejected");
    }

    #[tokio::test]
    async fn run_endpoint_persists_and_returns_result() {
        let app = test_app().await;

        let mut wf = a2w_ir::sample_workflow();
        // Replace the HttpRequest URL with example.com so dry_run/real_run can
        // resolve params. Real `Run` would actually hit the network here; for
        // the test we mark the http node as transform-only by removing it.
        wf.nodes.retain(|n| n.kind != a2w_ir::NodeKind::HttpRequest);
        wf.connections.retain(|c| c.from_node != "fetch" && c.to_node != "fetch");
        // Reconnect trigger -> shape directly.
        wf.connections.push(a2w_ir::Connection::new("trigger", 0, "shape"));

        let wf_json = serde_json::to_value(&wf).expect("wf to json");
        let resp = app
            .clone()
            .oneshot(json_req("PUT", "/workflows/wf_sample", &wf_json))
            .await
            .expect("put");
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(post_empty("/workflows/wf_sample/run"))
            .await
            .expect("run");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "completed");
        let run_id = body["run_id"].as_str().expect("run_id").to_string();

        // Persisted: listing must contain the new run id.
        let resp = app
            .oneshot(get("/workflows/wf_sample/runs"))
            .await
            .expect("runs");
        let runs = body_json(resp).await;
        assert!(
            runs.as_array()
                .unwrap()
                .iter()
                .any(|r| r == &serde_json::Value::String(run_id.clone())),
            "run must be persisted: {runs:?}"
        );
    }

    #[tokio::test]
    async fn run_endpoint_idempotency_short_circuits() {
        let app = test_app().await;

        let mut wf = a2w_ir::sample_workflow();
        wf.nodes.retain(|n| n.kind != a2w_ir::NodeKind::HttpRequest);
        wf.connections.retain(|c| c.from_node != "fetch" && c.to_node != "fetch");
        wf.connections.push(a2w_ir::Connection::new("trigger", 0, "shape"));
        let wf_json = serde_json::to_value(&wf).expect("wf to json");

        app.clone()
            .oneshot(json_req("PUT", "/workflows/wf_sample", &wf_json))
            .await
            .expect("put");

        let body = serde_json::json!({
            "trigger_input": [ { "id": 1 } ],
            "idempotency_key": "abc-123",
        });
        let resp = app
            .clone()
            .oneshot(json_req("POST", "/workflows/wf_sample/run", &body))
            .await
            .expect("first run");
        assert_eq!(resp.status(), StatusCode::OK);
        let first = body_json(resp).await;
        assert_eq!(first["idempotent_replay"], false);
        let first_run_id = first["run_id"].as_str().expect("run_id").to_string();

        // Second call with the same key: server returns the SAME run, marked
        // idempotent_replay=true, no new run committed.
        let resp = app
            .clone()
            .oneshot(json_req("POST", "/workflows/wf_sample/run", &body))
            .await
            .expect("second run");
        assert_eq!(resp.status(), StatusCode::OK);
        let second = body_json(resp).await;
        assert_eq!(second["idempotent_replay"], true);
        assert_eq!(second["run_id"], first_run_id);

        // Confirm only one run was persisted.
        let resp = app
            .oneshot(get("/workflows/wf_sample/runs"))
            .await
            .expect("runs");
        let runs = body_json(resp).await;
        assert_eq!(
            runs.as_array().unwrap().len(),
            1,
            "idempotency must NOT create a second run: {runs:?}"
        );
    }

    #[tokio::test]
    async fn ready_returns_ok_when_db_reachable() {
        let app = test_app().await;
        let resp = app.oneshot(get("/ready")).await.expect("ready response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "ready");
    }

    /// Build an app gated by the API key `"sekret"`.
    async fn test_app_with_api_key() -> Router {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("connect in-memory store");
        let cfg = ServerConfig {
            auth: AuthConfig {
                api_key: Some(std::sync::Arc::new("sekret".to_string())),
            },
            ..ServerConfig::default()
        };
        app_with_config(AppState::new(store), cfg)
    }

    #[tokio::test]
    async fn api_key_health_is_public() {
        let app = test_app_with_api_key().await;
        let resp = app.oneshot(get("/health")).await.expect("health response");
        assert_eq!(resp.status(), StatusCode::OK, "health must stay public");
    }

    #[tokio::test]
    async fn api_key_dashboard_is_public() {
        let app = test_app_with_api_key().await;
        let resp = app.oneshot(get("/")).await.expect("dashboard response");
        assert_eq!(resp.status(), StatusCode::OK, "dashboard must stay public");
    }

    #[tokio::test]
    async fn api_key_protected_route_without_header_is_401() {
        let app = test_app_with_api_key().await;
        let resp = app
            .oneshot(get("/workflows"))
            .await
            .expect("workflows response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_protected_route_with_wrong_key_is_401() {
        let app = test_app_with_api_key().await;
        let req = Request::builder()
            .uri("/workflows")
            .header("authorization", "Bearer wrong-key")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("workflows response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_protected_route_with_correct_key_is_200() {
        let app = test_app_with_api_key().await;
        let req = Request::builder()
            .uri("/workflows")
            .header("authorization", "Bearer sekret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("workflows response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_key_rejects_bare_key_without_bearer_prefix() {
        // Audit-fix: tightened parser requires the `Bearer ` scheme prefix.
        let app = test_app_with_api_key().await;
        let req = Request::builder()
            .uri("/workflows")
            .header("authorization", "sekret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("workflows response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_accepts_lowercase_bearer_prefix() {
        // Tolerant of curl-style lowercased scheme name.
        let app = test_app_with_api_key().await;
        let req = Request::builder()
            .uri("/workflows")
            .header("authorization", "bearer sekret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("workflows response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_endpoint_is_public_even_when_auth_enforced() {
        // Audit-fix: K8s readiness probe must NOT require the API key.
        let app = test_app_with_api_key().await;
        let resp = app.oneshot(get("/ready")).await.expect("ready response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn body_size_limit_rejects_oversized_payloads() {
        // Configure a tiny 16-byte limit and POST a body bigger than that.
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let cfg = ServerConfig {
            max_body_bytes: 16,
            ..ServerConfig::default()
        };
        let app = app_with_config(AppState::new(store), cfg);

        // PUT /workflows/xx with a body well over the limit.
        let huge = serde_json::json!({
            "schema_version": 1,
            "id": "xx",
            "name": "this name is intentionally long to exceed the body cap",
            "nodes": [],
            "connections": [],
        });
        let resp = app
            .oneshot(json_req("PUT", "/workflows/xx", &huge))
            .await
            .expect("put response");
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "oversized body must be rejected with 413"
        );
    }

    #[tokio::test]
    async fn request_id_is_set_on_response_when_absent_on_request() {
        let app = test_app().await;
        let resp = app.oneshot(get("/health")).await.expect("health response");
        assert!(
            resp.headers().get("x-request-id").is_some(),
            "x-request-id must be set on the response when the request did not carry one"
        );
    }

    #[tokio::test]
    async fn request_id_is_propagated_from_request_to_response() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/health")
            .header("x-request-id", "client-supplied-id-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.expect("health response");
        let echoed = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        assert_eq!(
            echoed.as_deref(),
            Some("client-supplied-id-123"),
            "client-supplied x-request-id must be preserved"
        );
    }

    #[tokio::test]
    async fn dashboard_is_served_and_mentions_a2w() {
        let app = test_app().await;
        let resp = app.oneshot(get("/")).await.expect("dashboard response");
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_text(resp).await;
        assert!(html.contains("A2W"), "dashboard must contain the text A2W");
        assert!(
            html.contains("/workflows"),
            "dashboard JS must fetch /workflows"
        );
    }
}
