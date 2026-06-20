//! Prometheus-style metrics export.
//!
//! A single [`PrometheusHandle`] is installed at startup; the `/metrics`
//! handler then renders it on every scrape. Counters live on the `metrics`
//! façade and can be incremented from anywhere in the workspace without taking
//! a hard dependency on the recorder (it's a global thread-local installed via
//! `metrics-exporter-prometheus`).
//!
//! ## Conventions
//! - `a2w_http_requests_total{method, path, status}` — request volume
//! - `a2w_http_request_duration_seconds{method, path}` — wall-clock latency
//! - `a2w_runs_total{status}` — engine runs persisted (`completed|failed`)
//! - `a2w_credentials_total{op}` — credential CRUD counts (`store|delete|read`)

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Shared metrics handle: cheap to clone, holds the renderer.
#[derive(Clone)]
pub struct MetricsHandle {
    inner: Arc<PrometheusHandle>,
}

impl MetricsHandle {
    /// Install the global recorder (idempotent across calls; we log and ignore
    /// the second-installation error). Returns a handle for the `/metrics`
    /// renderer.
    #[must_use]
    pub fn install() -> Self {
        // PrometheusBuilder::install_recorder is the canonical setup for
        // process-wide metrics. A second call returns Err; tolerate that so
        // tests can install the recorder multiple times without panicking.
        let inner = match PrometheusBuilder::new().install_recorder() {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "metrics recorder already installed; reusing the existing one"
                );
                // Build a no-op handle by installing again *to a sink*; but
                // `metrics-exporter-prometheus` doesn't expose the existing
                // handle. Fall back to a fresh, NOT-installed recorder, used
                // only to render an "empty" /metrics on duplicate-install. In
                // practice this branch only fires in tests.
                PrometheusBuilder::new().build_recorder().handle()
            }
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Render the current snapshot in Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        self.inner.render()
    }
}

/// `GET /metrics` — Prometheus scrape endpoint. Returns the text-format
/// snapshot rendered by the recorder.
pub async fn handler(State(handle): State<MetricsHandle>) -> Response {
    let body = handle.render();
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "text/plain; version=0.0.4; charset=utf-8".parse().unwrap(),
    );
    resp
}

/// Axum middleware that records `a2w_http_requests_total` and
/// `a2w_http_request_duration_seconds` for every request that reaches a
/// handler.
pub async fn track_requests(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    let elapsed = start.elapsed().as_secs_f64();

    metrics::counter!(
        "a2w_http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "a2w_http_request_duration_seconds",
        "method" => method,
        "path" => path,
    )
    .record(elapsed);

    resp
}
