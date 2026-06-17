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

mod dashboard;
mod error;
mod handlers;
mod state;

pub use error::ApiError;
pub use state::AppState;

use axum::routing::{get, post};
use axum::Router;

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
///
/// `Router` is itself `#[must_use]`, so no extra attribute is needed here.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::dashboard))
        .route("/health", get(handlers::health))
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
        .route("/runs/{run_id}", get(handlers::get_run))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt; // for `oneshot`

    use a2w_store::Store;

    /// Build an `app` backed by a fresh in-memory store.
    async fn test_app() -> Router {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("connect in-memory store");
        app(AppState::new(store))
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
