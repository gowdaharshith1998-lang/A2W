//! The request handlers backing the routes in [`crate::app`].
//!
//! Every handler returns `Result<impl IntoResponse, ApiError>` and uses `?` for
//! fallible calls, so there is no `unwrap`/`expect` in this module. axum 0.8
//! path params use the `/{id}` syntax in the route string and are extracted with
//! [`axum::extract::Path`].

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use a2w_engine::{ExecutionMode, MemoryEventLog, StepEvent};
use a2w_ir::Workflow;
use a2w_store::{workflow_fingerprint, ApprovalRecord, IdempotencyClaim, StoreResumeSource, Vault};

use crate::dashboard::DASHBOARD_HTML;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /` — the read-only HTML dashboard.
pub async fn dashboard() -> Html<String> {
    Html(DASHBOARD_HTML.to_string())
}

/// `GET /health` — liveness probe. Returns 200 unconditionally so a process is
/// declared "alive" as long as the HTTP runtime is up. For dependency checks,
/// use [`ready`] instead.
pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// `GET /ready` — readiness probe. Pokes the database with a trivial query so
/// the response is only `200` when the store is reachable. Returns `503` on a
/// DB failure with a small JSON body so a load balancer can drain the pod.
pub async fn ready(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // `list_workflows` is the cheapest read we have that exercises the pool.
    // We deliberately discard the result — we only care that the query
    // succeeded.
    state
        .store
        .list_workflows()
        .await
        .map_err(|e| ApiError::ServiceUnavailable(format!("database is not reachable: {e}")))?;
    Ok(Json(json!({ "status": "ready" })))
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
    // R3+R4 audit-fix: validate at save time so the operator can't store an
    // invalid (cyclic, self-referencing SubWorkflow, etc.) workflow that
    // would only fail at run time. R4: surface ALL findings in the body
    // (round-3 only surfaced the first), so the caller can fix them in one
    // round-trip.
    let report = a2w_validator::validate(&wf);
    if !report.is_valid {
        let summaries: Vec<String> = report
            .findings
            .iter()
            .filter(|f| f.severity == a2w_validator::Severity::Error)
            .map(|f| format!("{:?}: {}", f.code, f.message))
            .collect();
        return Err(ApiError::Unprocessable(format!(
            "workflow failed validation ({} error(s)): {}",
            summaries.len(),
            summaries.join(" | ")
        )));
    }
    // R4 audit-fix: cross-workflow SubWorkflow cycle detection. The
    // intra-workflow validator only catches direct self-reference; here we
    // walk the graph of stored workflows transitively (treating the
    // would-be-saved `wf` as already present at `id`) and refuse if a cycle
    // is reachable. Without this, A → B → A passes validation and only the
    // runtime depth cap defends.
    if let Err(cycle) = check_sub_workflow_cycle(&state, &id, &wf).await? {
        return Err(ApiError::Unprocessable(format!(
            "SubWorkflow cycle detected: {cycle}"
        )));
    }
    state.store.save_workflow(&wf).await?;
    Ok(Json(json!({ "saved": id })))
}

/// `DELETE /workflows/{id}` — delete a workflow (idempotent; `200` even if
/// absent, mirroring the store's no-op delete).
///
/// R4 audit-fix: refuses to delete a workflow that is referenced by any
/// other stored workflow's `sub_workflow` node. The caller must remove the
/// referring nodes first (returns 409 with the referrers in the body).
pub async fn delete_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // R5 H4 + H5 audit-fix: use the inverse-index. Previously this scanned
    // every workflow and parsed every IR — an authenticated DELETE-loop
    // could DoS the DB pool. The indexed lookup is O(refs-of-target).
    // The inverse-index is populated by save_workflow's
    // sub_workflow_references walk, which covers BOTH the inline form
    // and the workflow_id form — H4 in one shot.
    let referrers = state.store.referrers_of(&id).await?;
    let referrers: Vec<String> = referrers.into_iter().filter(|r| r != &id).collect();
    if !referrers.is_empty() {
        return Err(ApiError::Conflict(format!(
            "workflow '{id}' is referenced by {} other workflow(s): {} \
             — remove the referring sub_workflow nodes first",
            referrers.len(),
            referrers.join(", ")
        )));
    }
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

/// Optional body for `POST /workflows/{id}/dry_run` and
/// `POST /workflows/{id}/run`.
#[derive(Deserialize, Default)]
pub struct DryRunRequest {
    /// The trigger input item array. Defaults to a single empty object.
    #[serde(default)]
    trigger_input: Option<Vec<Value>>,
    /// Idempotency key (only honoured by the real-run endpoint). When supplied
    /// and a run was previously committed under this key for this workflow,
    /// the server returns the existing run instead of re-executing.
    #[serde(default)]
    idempotency_key: Option<String>,
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

/// `POST /workflows/{id}/run` — execute the workflow **for real** (HTTP nodes
/// make real outbound calls, MCP nodes spawn real children, etc.) and persist
/// the run. Returns the [`RunResult`].
///
/// Honours `idempotency_key` (in the request body): if a run was previously
/// committed under that key, returns the existing run unchanged. This makes
/// safe retries possible from upstream proxies.
pub async fn run_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<DryRunRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    let wf = state
        .store
        .get_workflow(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("workflow '{id}' not found")))?;

    let req = body.map(|Json(b)| b).unwrap_or_default();
    let trigger_input = req.trigger_input.unwrap_or_else(default_trigger_input);

    // Pre-mint the run id so the idempotency 2-phase claim can bind it BEFORE
    // we run (audit-2 fix for the binding-after-side-effect race).
    let run_id = mint_handler_run_id(&wf.id);

    // Idempotency 2-phase claim — scoped to (workflow_id, key). Closes both
    // the IDOR (round-2 audit) and the double-execute race (round-2 audit).
    let claim = if let Some(key) = req.idempotency_key.as_deref() {
        if key.len() > 200 {
            return Err(ApiError::BadRequest(
                "idempotency_key exceeds 200-byte limit".to_string(),
            ));
        }
        let ttl = std::env::var("A2W_IDEMPOTENCY_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(24 * 3600);
        let outcome = state
            .store
            .claim_idempotency_key(&id, key, &run_id, ttl)
            .await?;
        match &outcome {
            IdempotencyClaim::Completed(prior_run_id) => {
                if let Some(stored) = state.store.get_run(prior_run_id).await? {
                    return Ok(Json(serde_json::json!({
                        "run_id": stored.run_id,
                        "workflow_id": stored.workflow_id,
                        "status": stored.status,
                        "events": stored.events,
                        "idempotent_replay": true,
                    })));
                }
                // Race: row points to a run we can't read. Treat as conflict.
                return Err(ApiError::ServiceUnavailable(format!(
                    "idempotency key '{key}' points to a missing run '{prior_run_id}'; retry"
                )));
            }
            IdempotencyClaim::InProgress(_) => {
                // R3 audit-fix: error body must NOT echo the user-supplied
                // key or the prior run id. Both are existence oracles when
                // the caller doesn't already know them. Generic 409 instead.
                return Err(ApiError::Conflict(
                    "another run is in progress under this idempotency key; \
                     retry after the in-flight run completes"
                        .to_string(),
                ));
            }
            IdempotencyClaim::Acquired | IdempotencyClaim::Expired(_) => {
                // Proceed; we are the writer.
                Some(key.to_string())
            }
        }
    } else {
        None
    };

    let log = MemoryEventLog::new();
    let result = state
        .engine
        .run_with_id(
            &wf,
            run_id.clone(),
            trigger_input,
            ExecutionMode::Run,
            &log,
            None,
        )
        .await;

    // On engine error: release our claim so a retry can get a fresh Acquired.
    // R5 H1 fix: the release is run_id-guarded — we pre-minted `run_id`
    // before the engine ran, so the slot still bears our id and is safe
    // to release.
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            if let Some(key) = &claim {
                let _ = state.store.release_idempotency_key(&id, key, &run_id).await;
            }
            return Err(e.into());
        }
    };

    // R3 audit-fix: if save_run fails AFTER the engine ran side effects,
    // release the claim so a retry can get a fresh Acquired and re-run.
    //
    // R5 audit-fix: release_idempotency_key is now run_id-guarded so it
    // ONLY deletes the row if it still points at our run_id — protecting
    // an in-flight adopter from a stale release.
    let fp = workflow_fingerprint(&wf);
    if let Err(e) = state.store.save_run_full(&wf, &fp, &result).await {
        if let Some(key) = &claim {
            let _ = state
                .store
                .release_idempotency_key(&id, key, &result.run_id)
                .await;
        }
        return Err(e.into());
    }
    // Phase-2 commit of the idempotency claim.
    //
    // R4 audit-fix: we MUST NOT release the in_progress slot on commit
    // failure — releasing lets a correct client retry re-`Acquired` and
    // re-execute the engine, double-firing every side effect. Instead we
    // leave the slot in_progress (which makes subsequent retries return
    // 409 — correct), and surface 502 so the operator knows the run
    // committed to `runs` but the idempotency book-keeping needs reaping.
    //
    // A background reaper that finalizes (runs.run_id ↔ idempotency_keys
    // in_progress) pairs is the long-term fix; the TTL also recovers
    // stranded slots after `A2W_IDEMPOTENCY_TTL_SECS`.
    //
    // Retries use exponential backoff with jitter to avoid the
    // thundering-herd against an already-failing DB.
    // R5 audit-fix: commit-pending semantics.
    // - On `Ok(true)`: we are canonical; normal response.
    // - On `Ok(false)`: an adopter has already finalized the slot under a
    //   DIFFERENT run_id. Log the divergence + return audit_warning in
    //   the body so downstream systems can dedupe; the response still
    //   returns OUR run_id since the side effects of THIS request did
    //   fire (we don't lie to the caller about what they observed).
    // - On persistent error: return 200 with `idempotency_warning =
    //   "commit_pending"` + run_id, and SPAWN a background commit-retry
    //   so the slot is eventually finalized without a TTL-adoption
    //   double-fire. (R5 H3 fix: previously this returned 502 and
    //   wedged the slot for the full TTL.)
    let mut audit_warning: Option<String> = None;
    let mut commit_pending = false;
    if let Some(key) = &claim {
        let mut last_err = None;
        let mut committed_ok = false;
        for attempt in 0..5 {
            match state
                .store
                .complete_idempotency_key(&id, key, &result.run_id)
                .await
            {
                Ok(true) => {
                    committed_ok = true;
                    break;
                }
                Ok(false) => {
                    // Adopter took over and committed. R6 H2 audit-fix: log
                    // canonical run_id but do NOT echo it in the response
                    // body — that would leak a peer's run id to the losing
                    // caller and (combined with the unauthenticated
                    // GET /runs/{run_id} surface) enable full event-stream
                    // disclosure.
                    let canonical = state
                        .store
                        .get_idempotency_key(&id, key)
                        .await
                        .ok()
                        .flatten();
                    tracing::error!(
                        workflow = %id,
                        our_run = %result.run_id,
                        adopter_run = ?canonical,
                        "idempotency adoption conflict: an expired-claim adopter \
                         finalized this slot while our run was in flight. Both \
                         runs side-effected; caller observes ours, audit-trail \
                         canonical is the adopter's."
                    );
                    ::metrics::counter!("a2w_idempotency_adoption_conflicts_total").increment(1);
                    audit_warning = Some(
                        "adoption_conflict: another caller is canonical for this \
                         idempotency key; both runs side-effected. Operator \
                         should reconcile via server logs (the canonical run_id \
                         is intentionally NOT exposed in this response — see \
                         tracing log for details)."
                            .to_string(),
                    );
                    committed_ok = true;
                    break;
                }
                Err(e) => last_err = Some(e),
            }
            if attempt + 1 < 5 {
                // Exp backoff with REAL random jitter (R5 M2 fix: previous
                // jitter was deterministic per run_id, so all retries from
                // the same client landed at the same instant).
                //
                // R8 M2 audit-fix: cancellation-aware. If SIGTERM arrives
                // mid-loop on a DB-stalled run, break out immediately
                // instead of sleeping — the background commit-retry path
                // will be registered with bg_tasks so the slot finalizes
                // there.
                let base_ms = 20u64 << attempt;
                let jitter = {
                    use std::collections::hash_map::RandomState;
                    use std::hash::BuildHasher;
                    let raw = RandomState::new().hash_one((attempt, result.run_id.as_str()));
                    raw % (base_ms / 2 + 1)
                };
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(base_ms + jitter)) => {}
                    _ = state.shutdown.cancelled() => break,
                }
            }
        }
        if !committed_ok {
            // R5 H3 fix: do NOT return 502. The run committed; the slot
            // is left in_progress (correct: retries 409 instead of
            // re-firing) and a background task continues retrying the
            // 2-phase commit asynchronously. Operator alerted via metric.
            ::metrics::counter!("a2w_idempotency_commit_pending_total").increment(1);
            tracing::warn!(
                workflow = %id, run = %result.run_id,
                error = ?last_err,
                "idempotency phase-2 commit failed after retries; spawning \
                 background reaper-retry; slot remains in_progress"
            );
            commit_pending = true;
            let store_arc = state.store.clone();
            let id_owned = id.clone();
            let key_owned = key.to_string();
            let run_owned = result.run_id.clone();
            // R6 H1 / R7 H1 audit-fix: register with the AppState
            // TaskTracker so graceful shutdown awaits this future, AND
            // wrap each sleep in tokio::select! against the shutdown
            // CancellationToken so SIGTERM collapses any pending backoff
            // and triggers one final immediate commit attempt before
            // exit — instead of letting a 10-minute sleep get aborted at
            // its await point, leaving the slot stranded in_progress.
            //
            // Backoff tightened to [1, 5, 10, 30, 60] = 106 s total so
            // worst-case fits comfortably inside the 120 s shutdown
            // budget (see main.rs).
            let cancel = state.shutdown.clone();
            state.bg_tasks.spawn(async move {
                let backoffs = [1u64, 5, 10, 30, 60];
                let mut shutting_down = false;
                for sec in backoffs {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(sec)) => {}
                        _ = cancel.cancelled() => {
                            shutting_down = true;
                            // Skip the rest of the sleep; one final
                            // immediate attempt before shutdown.
                        }
                    }
                    match store_arc
                        .complete_idempotency_key(&id_owned, &key_owned, &run_owned)
                        .await
                    {
                        Ok(true) => {
                            tracing::info!(
                                workflow = %id_owned, run = %run_owned,
                                after_secs = sec, shutting_down,
                                "background idempotency commit-retry succeeded"
                            );
                            return;
                        }
                        Ok(false) => {
                            // R8 M1 audit-fix: mirror the sync path's
                            // adoption-conflict observability. An adopter
                            // finalized the slot under a different run_id
                            // while we were backing off — both runs
                            // side-effected. The synchronous path emits an
                            // error log + adoption_conflicts_total +
                            // audit_warning; the background path used to
                            // collapse this to an info-level success.
                            ::metrics::counter!(
                                "a2w_idempotency_background_adoption_conflicts_total"
                            )
                            .increment(1);
                            tracing::error!(
                                workflow = %id_owned, run = %run_owned,
                                after_secs = sec,
                                "deferred idempotency adoption conflict: an \
                                 expired-claim adopter finalized this slot \
                                 while we were retrying; both runs \
                                 side-effected; client was already given a \
                                 commit_pending response so cannot dedupe \
                                 via API — operator must reconcile via logs"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(
                                workflow = %id_owned, run = %run_owned,
                                after_secs = sec, error = %e,
                                "background idempotency commit-retry attempt failed"
                            );
                        }
                    }
                    if shutting_down {
                        // No more retries on shutdown — the periodic
                        // reaper on the NEXT boot will finalize the
                        // slot before TTL adoption can fire.
                        break;
                    }
                }
                ::metrics::counter!("a2w_idempotency_commit_abandoned_total").increment(1);
                tracing::error!(
                    workflow = %id_owned, run = %run_owned,
                    "idempotency commit-retry abandoned; reaper must \
                     finalize this slot before TTL adoption re-fires"
                );
            });
            audit_warning = Some(
                "commit_pending: idempotency phase-2 commit is being retried in \
                 the background; subsequent retries with the same key will \
                 return 409 until either the commit succeeds or the slot \
                 expires (then adoption will re-fire — operator should reap)"
                    .into(),
            );
        }
    }

    let status_label = match result.status {
        a2w_engine::RunStatus::Completed => "completed",
        a2w_engine::RunStatus::Failed => "failed",
    };
    ::metrics::counter!("a2w_runs_total", "status" => status_label).increment(1);

    let mut body = serde_json::json!({
        "run_id": result.run_id,
        "workflow_id": id,
        "status": result.status,
        "events": result.events,
        "node_outputs": result.node_outputs,
        "idempotent_replay": false,
    });
    if let Some(w) = audit_warning {
        body["audit_warning"] = serde_json::Value::String(w);
    }
    if commit_pending {
        body["idempotency_commit_pending"] = serde_json::Value::Bool(true);
    }
    Ok(Json(body))
}

/// Walk the SubWorkflow reference graph rooted at `(new_id, wf)` against
/// stored workflows. Returns `Ok(Ok(()))` when no cycle; `Ok(Err(path))`
/// when a cycle is reachable (path is a `A → B → C → A` description); and
/// propagates store errors otherwise.
async fn check_sub_workflow_cycle(
    state: &AppState,
    new_id: &str,
    new_wf: &Workflow,
) -> Result<Result<(), String>, ApiError> {
    use std::collections::{HashMap, HashSet};
    // R5 H5+M4 fix: bound depth + use the indexed
    // `referenced_workflows_of` for stored workflows (O(refs) per node
    // instead of O(all-workflows-and-parse-IRs)). The DFS bound matches
    // the runtime SubWorkflow recursion cap.
    const MAX_CYCLE_CHECK_DEPTH: usize = a2w_engine::DEFAULT_MAX_SUB_WORKFLOW_DEPTH as usize * 4;
    let mut visited: HashMap<String, Vec<String>> = HashMap::new();
    visited.insert(
        new_id.to_string(),
        a2w_validator::sub_workflow_references(new_wf),
    );
    let mut path: Vec<String> = vec![new_id.to_string()];
    let mut on_path: HashSet<String> = HashSet::new();
    on_path.insert(new_id.to_string());
    let mut stack: Vec<(String, usize)> = vec![(new_id.to_string(), 0)];
    while let Some((cur, idx)) = stack.last().cloned() {
        if path.len() > MAX_CYCLE_CHECK_DEPTH {
            // R6 L7 audit-fix: distinguish "depth-cap exceeded on a
            // legitimate linear graph" from "cycle detected". The
            // engine's runtime DEFAULT_MAX_SUB_WORKFLOW_DEPTH still
            // caps recursion; here we surface the unusual depth as
            // configuration guidance rather than implying a cycle.
            return Ok(Err(format!(
                "SubWorkflow reference graph deeper than {MAX_CYCLE_CHECK_DEPTH} \
                 levels during static cycle-check — either a true cycle the \
                 walker hasn't closed yet, OR an unusually deep legitimate \
                 chain. Trimmed path: {}",
                path.join(" → ")
            )));
        }
        let refs = match visited.get(&cur) {
            Some(r) => r.clone(),
            None => {
                // R5 H5: use the indexed lookup instead of re-parsing the IR.
                let r = state.store.referenced_workflows_of(&cur).await?;
                visited.insert(cur.clone(), r.clone());
                r
            }
        };
        if idx >= refs.len() {
            stack.pop();
            path.pop();
            on_path.remove(&cur);
            continue;
        }
        if let Some(last) = stack.last_mut() {
            last.1 = idx + 1;
        }
        let child = refs[idx].clone();
        if on_path.contains(&child) {
            path.push(child);
            return Ok(Err(path.join(" → ")));
        }
        on_path.insert(child.clone());
        path.push(child.clone());
        stack.push((child, 0));
    }
    Ok(Ok(()))
}

/// Mint a handler-side run id. Distinct from the engine's `mint_run_id` so
/// the handler can pre-claim before the engine runs. R3 audit-fix: includes
/// PID + ns-resolution timestamp + process-unique hasher salt so cross-
/// replica collisions are vanishingly unlikely.
fn mint_handler_run_id(wf_id: &str) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let salt = RandomState::new().hash_one((pid, nanos, n, wf_id));
    format!("run_{wf_id}_{nanos:x}_{n}_{salt:x}")
}

/// `POST /runs/{run_id}/resume` — resume a previously-crashed run from its
/// last `Finished` step. Nodes already completed are not re-executed; only
/// the unfinished frontier (and downstream) runs. Returns the new
/// (completed) [`RunResult`].
pub async fn resume_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // Look up the stored run to find its workflow_id.
    let stored = state
        .store
        .get_run(&run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("run '{run_id}' not found")))?;
    let wf = state
        .store
        .get_workflow(&stored.workflow_id)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!("workflow '{}' not found", stored.workflow_id))
        })?;

    // R4 audit-fix: workflow fingerprint check. If the stored run was
    // committed against an older IR than the current `wf`, the persisted
    // outputs we'd hydrate may reference fields / structures that no longer
    // exist. Refuse the resume with a 409 so the operator can either revert
    // the IR or start a fresh run.
    let current_fp = workflow_fingerprint(&wf);
    if let Some(prior_fp) = stored.workflow_fingerprint.as_deref() {
        if prior_fp != current_fp {
            return Err(ApiError::Conflict(
                "workflow IR has changed since this run was first persisted; \
                 refuse to resume — start a fresh run with the new IR instead"
                    .to_string(),
            ));
        }
    }

    let resume = StoreResumeSource::new(state.store.clone());
    let log = MemoryEventLog::new();
    // The trigger input is empty on resume — the trigger's output is
    // hydrated from step_records. (Engine rejects non-empty trigger_input
    // combined with resume_from, R4 audit-fix.)
    let result = state
        .engine
        .run_with_id(
            &wf,
            run_id.clone(),
            Vec::new(),
            ExecutionMode::Run,
            &log,
            Some(&resume),
        )
        .await?;

    state.store.save_run_full(&wf, &current_fp, &result).await?;

    Ok(Json(serde_json::json!({
        "run_id": result.run_id,
        "workflow_id": stored.workflow_id,
        "status": result.status,
        "events": result.events,
        "node_outputs": result.node_outputs,
        "resumed": true,
    })))
}

// ===========================================================================
// Approvals
// ===========================================================================

/// `GET /approvals?status=pending` — list approvals (optionally filtered).
pub async fn list_approvals(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ApprovalListQuery>,
) -> Result<Json<Vec<ApprovalSummary>>, ApiError> {
    let rows = state.store.list_approvals(params.status.as_deref()).await?;
    Ok(Json(rows.into_iter().map(ApprovalSummary::from).collect()))
}

/// `GET /approvals/{id}` — fetch one approval.
pub async fn get_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApprovalSummary>, ApiError> {
    let row = state
        .store
        .get_approval(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("approval '{id}' not found")))?;
    Ok(Json(ApprovalSummary::from(row)))
}

/// `POST /approvals/{id}` — decide a pending approval.
pub async fn decide_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DecideApprovalRequest>,
) -> Result<Json<Value>, ApiError> {
    let decision = req.decision.trim().to_ascii_lowercase();
    if decision != "approved" && decision != "rejected" {
        return Err(ApiError::BadRequest(
            "decision must be 'approved' or 'rejected'".into(),
        ));
    }
    let decided = state
        .store
        .decide_approval(&id, &decision, req.decided_by.as_deref())
        .await?;
    if !decided {
        return Err(ApiError::Conflict(format!(
            "approval '{id}' is not pending (already decided or absent)"
        )));
    }
    Ok(Json(json!({ "decided": id, "decision": decision })))
}

#[derive(Deserialize, Default)]
pub struct ApprovalListQuery {
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct DecideApprovalRequest {
    pub decision: String,
    #[serde(default)]
    pub decided_by: Option<String>,
}

#[derive(Serialize)]
pub struct ApprovalSummary {
    id: String,
    run_id: String,
    workflow_id: String,
    node_id: String,
    payload: Value,
    status: String,
    decided_by: Option<String>,
    decided_at: Option<i64>,
    created_at: i64,
}

impl From<ApprovalRecord> for ApprovalSummary {
    fn from(r: ApprovalRecord) -> Self {
        let payload = serde_json::from_str::<Value>(&r.payload_json).unwrap_or(Value::Null);
        Self {
            id: r.id,
            run_id: r.run_id,
            workflow_id: r.workflow_id,
            node_id: r.node_id,
            payload,
            status: r.status,
            decided_by: r.decided_by,
            decided_at: r.decided_at,
            created_at: r.created_at,
        }
    }
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

// ===========================================================================
// Credentials
//
// Backed by `a2w_store::Vault` (AES-256-GCM envelope encryption). All
// endpoints return `503 Service Unavailable` when the server was started
// without `A2W_MASTER_KEY` so misconfigured deployments fail closed instead of
// quietly persisting cleartext or running with an inert resolver.
// ===========================================================================

/// `POST /credentials` body — agent-supplied credential to store.
///
/// The secret is encrypted at rest and never returned by any later GET.
#[derive(Deserialize)]
pub struct StoreCredentialRequest {
    /// Stable identifier used as the `credential_ref` from a workflow.
    pub id: String,
    /// Human-readable display name (free text).
    pub name: String,
    /// Plaintext secret to be encrypted under the master key.
    pub secret: String,
}

/// One row of the `GET /credentials` listing — **no secret material**.
#[derive(Serialize)]
pub struct CredentialSummary {
    /// The credential id.
    id: String,
    /// The display name.
    name: String,
    /// Unix-seconds timestamp of the most recent write.
    created_at: i64,
}

/// Borrow the configured vault from state, or return `503` if absent.
fn require_vault(state: &AppState) -> Result<&Arc<Vault>, ApiError> {
    state.vault.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "credential endpoints disabled: server was started without A2W_MASTER_KEY \
             (set A2W_MASTER_KEY to a base64 32-byte key and restart)"
                .to_string(),
        )
    })
}

/// `POST /credentials` — upsert a credential under its id.
///
/// The request body is `{ "id", "name", "secret" }`. Returns `{ "saved": id }`
/// on success.
pub async fn store_credential(
    State(state): State<AppState>,
    Json(req): Json<StoreCredentialRequest>,
) -> Result<Json<Value>, ApiError> {
    let vault = require_vault(&state)?;
    if req.id.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "`id` must be a non-empty string".into(),
        ));
    }
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "`name` must be a non-empty string".into(),
        ));
    }
    if req.secret.is_empty() {
        return Err(ApiError::BadRequest(
            "`secret` must be a non-empty string".into(),
        ));
    }
    vault
        .store_secret(&state.store, &req.id, &req.name, &req.secret)
        .await?;
    Ok(Json(json!({ "saved": req.id })))
}

/// `GET /credentials` — list stored credentials as `{id, name, created_at}`.
///
/// The plaintext secret is **never** returned.
pub async fn list_credentials(
    State(state): State<AppState>,
) -> Result<Json<Vec<CredentialSummary>>, ApiError> {
    // The vault gate is checked even on read so the API surface is consistent
    // across all credential endpoints (and we never leak listings from a
    // misconfigured server).
    let _ = require_vault(&state)?;
    let rows = Vault::list_credentials(&state.store).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name, created_at)| CredentialSummary {
                id,
                name,
                created_at,
            })
            .collect(),
    ))
}

/// `DELETE /credentials/{id}` — delete a credential (idempotent).
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let _ = require_vault(&state)?;
    Vault::delete_credential(&state.store, &id).await?;
    Ok(Json(json!({ "deleted": id })))
}
