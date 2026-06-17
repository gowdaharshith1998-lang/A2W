//! The request handlers backing the routes in [`crate::app`].
//!
//! Every handler returns `Result<impl IntoResponse, ApiError>` and uses `?` for
//! fallible calls, so there is no `unwrap`/`expect` in this module. axum 0.8
//! path params use the `/{id}` syntax in the route string and are extracted with
//! [`axum::extract::Path`].

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use a2w_engine::{ExecutionMode, MemoryEventLog, StepEvent};
use a2w_ir::Workflow;

use crate::dashboard::DASHBOARD_HTML;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /` — the read-only HTML dashboard.
pub async fn dashboard() -> Html<String> {
    Html(DASHBOARD_HTML.to_string())
}

/// `GET /health` — liveness probe.
pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// One row of the `GET /workflows` listing.
#[derive(Serialize)]
pub struct WorkflowSummary {
    /// The workflow id.
    id: String,
    /// The workflow's human-readable name.
    name: String,
}

/// `GET /workflows` — list stored workflows as `{id, name}` objects.
pub async fn list_workflows(
    State(state): State<AppState>,
) -> Result<Json<Vec<WorkflowSummary>>, ApiError> {
    let rows = state.store.list_workflows().await?;
    let summaries = rows
        .into_iter()
        .map(|(id, name)| WorkflowSummary { id, name })
        .collect();
    Ok(Json(summaries))
}

/// `GET /workflows/{id}` — the stored workflow JSON, or `404`.
pub async fn get_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Workflow>, ApiError> {
    let wf = state
        .store
        .get_workflow(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("workflow '{id}' not found")))?;
    Ok(Json(wf))
}

/// `PUT /workflows/{id}` — upsert a workflow.
///
/// The body is a full [`Workflow`]. We require the body's `id` to match the
/// path id (returning `400` otherwise) so the stored key is unambiguous.
pub async fn put_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(wf): Json<Workflow>,
) -> Result<Json<Value>, ApiError> {
    if wf.id != id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path id '{id}'",
            wf.id
        )));
    }
    state.store.save_workflow(&wf).await?;
    Ok(Json(json!({ "saved": id })))
}

/// `DELETE /workflows/{id}` — delete a workflow (idempotent; `200` even if
/// absent, mirroring the store's no-op delete).
pub async fn delete_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.store.delete_workflow(&id).await?;
    Ok(Json(json!({ "deleted": id })))
}

/// `GET /workflows/{id}/runs` — the run ids for a workflow.
///
/// Returns `404` if the workflow itself does not exist (so an empty array
/// unambiguously means "exists, no runs yet").
pub async fn list_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<String>>, ApiError> {
    ensure_workflow_exists(&state, &id).await?;
    let runs = state.store.list_runs(&id).await?;
    Ok(Json(runs))
}

/// `POST /workflows/{id}/validate` — validate the stored workflow.
pub async fn validate_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let wf = state
        .store
        .get_workflow(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("workflow '{id}' not found")))?;
    let report = a2w_validator::validate(&wf);
    Ok(Json(report))
}

/// Optional body for `POST /workflows/{id}/dry_run`.
#[derive(Deserialize, Default)]
pub struct DryRunRequest {
    /// The trigger input item array. Defaults to a single empty object.
    #[serde(default)]
    trigger_input: Option<Vec<Value>>,
}

/// `POST /workflows/{id}/dry_run` — load the stored workflow, run it in
/// [`ExecutionMode::DryRun`], persist the run, and return the [`RunResult`].
///
/// The body is an optional `{ "trigger_input": [..] }`; when omitted (or when
/// no body is sent) the trigger input defaults to `[{}]`. Engine failures map to
/// `422` via [`ApiError`].
pub async fn dry_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<DryRunRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    let wf = state
        .store
        .get_workflow(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("workflow '{id}' not found")))?;

    let trigger_input = body
        .and_then(|Json(req)| req.trigger_input)
        .unwrap_or_else(default_trigger_input);

    let log = MemoryEventLog::new();
    let result = state
        .engine
        .run(&wf, trigger_input, ExecutionMode::DryRun, &log)
        .await?;

    // Persist the run before returning it.
    state.store.save_run(&id, &result).await?;

    Ok(Json(result))
}

/// `GET /runs/{run_id}` — the persisted run record, or `404`.
///
/// `a2w_store::StoredRun` is not itself `Serialize`, so it is projected onto a
/// local [`StoredRunResponse`] for the JSON body.
pub async fn get_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<StoredRunResponse>, ApiError> {
    let stored = state
        .store
        .get_run(&run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("run '{run_id}' not found")))?;
    Ok(Json(StoredRunResponse::from(stored)))
}

/// JSON-serializable view of `a2w_store::StoredRun` (which is not `Serialize`).
#[derive(Serialize)]
pub struct StoredRunResponse {
    /// The run's id.
    run_id: String,
    /// The owning workflow's id.
    workflow_id: String,
    /// The terminal status string (`completed`/`failed`).
    status: String,
    /// The persisted step-event stream.
    events: Vec<StepEvent>,
}

impl From<a2w_store::StoredRun> for StoredRunResponse {
    fn from(s: a2w_store::StoredRun) -> Self {
        Self {
            run_id: s.run_id,
            workflow_id: s.workflow_id,
            status: s.status,
            events: s.events,
        }
    }
}

/// The default trigger input when a dry-run body omits it: a single empty item.
fn default_trigger_input() -> Vec<Value> {
    vec![json!({})]
}

/// Return `Ok(())` if a workflow with `id` exists, else `404`.
async fn ensure_workflow_exists(state: &AppState, id: &str) -> Result<(), ApiError> {
    if state.store.get_workflow(id).await?.is_some() {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("workflow '{id}' not found")))
    }
}
