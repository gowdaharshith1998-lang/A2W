//! Binary entry point for the A2W REST + observability server.
//!
//! Builds the [`AppState`] (with a credential [`Vault`] when `A2W_MASTER_KEY`
//! is set), the [`ServerConfig`] (auth gate + body / timeout limits, all from
//! env), binds `A2W_BIND`, logs the bound address, and serves the hardened
//! router from [`a2w_server::app_with_config`].
//!
//! ## Environment
//! - `A2W_DB_URL` — SQLite URL (default `sqlite://a2w.db?mode=rwc`).
//! - `A2W_BIND` — bind address (default `127.0.0.1:8080`).
//! - `A2W_MASTER_KEY` — base64 32-byte AES-256-GCM master key. When unset, the
//!   server starts in **no-credential mode**: HTTP/MCP nodes that name a
//!   `credential_ref` fail closed at execution time, and the `/credentials`
//!   endpoints return `503 Service Unavailable`.
//! - `A2W_API_KEY` — when set, every request (except `GET /health` and `GET /`)
//!   must carry `Authorization: Bearer <key>` or be rejected with `401`.
//! - `A2W_MAX_BODY_BYTES` — request body cap (default 1 MiB).
//! - `A2W_REQUEST_TIMEOUT_SECS` — per-request timeout (default 30 s).
//! - `RUST_LOG` — log filter (default `info`). Logs go to stdout as JSON when
//!   `A2W_LOG_JSON=true`, otherwise compact text.

#![forbid(unsafe_code)]

use std::error::Error;

use a2w_server::{app_with_config, AppState, ServerConfig};
use a2w_store::{Store, Vault};
use tracing_subscriber::EnvFilter;

/// Default SQLite URL (file-backed, created on demand).
const DEFAULT_DB_URL: &str = "sqlite://a2w.db?mode=rwc";
/// Default bind address.
const DEFAULT_BIND: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    init_tracing();

    let db_url = std::env::var("A2W_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
    let bind = std::env::var("A2W_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

    let store = Store::connect(&db_url).await?;

    // Optional vault. Misconfiguration of the key is a FATAL startup error so we
    // never silently fall back to no-credential mode when the operator intended
    // to enable encryption.
    let state = match std::env::var("A2W_MASTER_KEY") {
        Ok(_) => {
            let vault = Vault::from_env()?;
            tracing::info!("credential vault enabled (A2W_MASTER_KEY)");
            AppState::with_vault(store, vault)
        }
        Err(_) => {
            tracing::warn!(
                "credential vault disabled (A2W_MASTER_KEY not set); \
                 HTTP/MCP nodes that reference a credential_ref will fail closed"
            );
            AppState::new(store)
        }
    };

    let cfg = ServerConfig::from_env();
    if cfg.auth.is_enforced() {
        tracing::info!("API-key auth ENFORCED (A2W_API_KEY)");
    } else {
        tracing::warn!("API-key auth DISABLED (A2W_API_KEY not set); every request is permitted");
    }
    tracing::info!(
        max_body_bytes = cfg.max_body_bytes,
        request_timeout_secs = cfg.request_timeout.as_secs(),
        "request limits configured"
    );

    // R6 + R7 audit-fix: snapshot the TaskTracker + cancel + store BEFORE
    // moving `state` into the router. The cancel token is what makes
    // background backoff sleeps cancellation-aware on SIGTERM.
    let bg_tracker = state.bg_tasks.clone();
    let shutdown_cancel = state.shutdown.clone();
    let store_for_reaper = state.store.clone();
    // Startup reap: any commit-pending slots from a prior unclean
    // shutdown are finalized BEFORE we accept new requests.
    //
    // R7 M2 fix: by default this is FATAL — booting with stranded
    // slots and accepting new claims would allow adopter-double-fire.
    // Set `A2W_TOLERATE_STARTUP_REAP_FAILURE=true` (e.g. for dev) to
    // downgrade to a warning.
    //
    // R8 L1 fix: bounded by a 30 s timeout so a hung DB / NFS mount
    // can't render the process unkillable via graceful signals (kubelet
    // would otherwise have to SIGKILL after the grace period).
    let startup_reap = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        store_for_reaper.reap_stranded_idempotency_claims(),
    )
    .await;
    let reap_outcome = match startup_reap {
        Ok(inner) => inner.map_err(|e| format!("startup reap query failed: {e}")),
        Err(_) => Err("startup reap timed out after 30s (DB hung?)".to_string()),
    };
    if let Err(msg) = reap_outcome {
        let tolerate = std::env::var("A2W_TOLERATE_STARTUP_REAP_FAILURE")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);
        if tolerate {
            tracing::warn!(error = %msg, "startup reaper failed (tolerated by env)");
        } else {
            tracing::error!(error = %msg, "startup reaper failed — refusing to boot");
            return Err(msg.into());
        }
    }
    // Periodic reaper: every 60 s, finalize any in_progress idempotency
    // slot whose run_id is already in the runs table. R7 M1 fix: the
    // tick await is racing the close check — switch to tokio::select!
    // so cancel collapses the sleep immediately and the loop exits
    // before the 60 s budget elapses.
    let reaper_store = store_for_reaper.clone();
    let reaper_cancel = shutdown_cancel.clone();
    bg_tracker.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.tick().await; // burn the first immediate tick
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = reaper_cancel.cancelled() => break,
            }
            match reaper_store.reap_stranded_idempotency_claims().await {
                Ok(n) if n > 0 => {
                    tracing::info!(reaped = n, "periodic idempotency reaper finalized slots");
                    ::metrics::counter!("a2w_idempotency_reaped_total").increment(n);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "periodic idempotency reaper failed");
                }
            }
        }
    });

    let router = app_with_config(state, cfg);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!(address = %addr, db = %db_url, "a2w-server listening");
    println!("a2w-server listening on http://{addr} (db: {db_url})");

    // Wire SIGINT / SIGTERM. R7 H1 audit-fix: trip the CancellationToken
    // BEFORE closing the tracker so background commit-retries see the
    // signal mid-sleep and complete one final attempt; THEN await the
    // tracker drain with a budget that fits the worst-case commit
    // backoff (~106 s) plus headroom.
    let shutdown_for_signal = shutdown_cancel.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_for_signal.cancel();
        })
        .await?;
    bg_tracker.close();
    // 120 s budget > backoff sum 106 s + ~14 s headroom for the final
    // commit attempt + log flush. R8 L2 fix: a malformed env value used
    // to silently fall back to 120; now we surface the parse failure so
    // an operator's misconfig is visible.
    let drain_timeout = match std::env::var("A2W_SHUTDOWN_DRAIN_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    raw = %raw, error = %e,
                    "A2W_SHUTDOWN_DRAIN_SECS unparseable — falling back to 120s"
                );
                120
            }
        },
        Err(_) => 120,
    };
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(drain_timeout),
        bg_tracker.wait(),
    )
    .await;
    Ok(())
}

/// Initialise a JSON-or-text tracing subscriber once.
fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("A2W_LOG_JSON")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);
    if json {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .compact()
            .try_init();
    }
}

/// Block until SIGINT (or SIGTERM on Unix) is received, then return so axum can
/// gracefully drain in-flight requests before exiting.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received; draining...");
}
