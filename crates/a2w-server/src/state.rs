//! The shared application state handed to every handler.

use std::sync::Arc;

use a2w_engine::Engine;
use a2w_store::{
    Store, StoreApprovalGate, StoreCredentialResolver, StoreSubWorkflowResolver, Vault,
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Shared, cheaply-cloneable application state.
///
/// `axum` clones the state into each request, so the expensive members live
/// behind [`Arc`]: the [`Store`] (a pooled DB handle), the [`Engine`] (which
/// owns the node registry + optional credential resolver), and the [`Vault`]
/// (which guards the AES-256-GCM master key).
///
/// The vault is **optional**. When `A2W_MASTER_KEY` is not set, the engine is
/// built without a credential resolver and the credential endpoints return
/// `503 Service Unavailable` — fail-closed so a misconfigured server never
/// silently runs HTTP nodes against an inert resolver.
#[derive(Clone)]
pub struct AppState {
    /// The persistence handle (workflows + run history).
    pub store: Arc<Store>,
    /// The execution engine, preloaded with the default node registry and (when
    /// `vault` is `Some`) a vault-backed [`StoreCredentialResolver`].
    pub engine: Arc<Engine>,
    /// The credential vault, when configured via `A2W_MASTER_KEY`. `None` means
    /// credential writes are rejected with `503`.
    pub vault: Option<Arc<Vault>>,
    /// R6 audit-fix: tracker for background tasks (idempotency commit-retry
    /// spawns). Graceful shutdown awaits `wait()` after `close()` so a
    /// stranded slot doesn't outlive the process and trigger an
    /// adopter-double-fire on the next request.
    pub bg_tasks: TaskTracker,
    /// R7 audit-fix: cancellation token cloned into every background
    /// task. SIGTERM cancels it so backoff sleeps (which can be up to 10
    /// minutes) collapse immediately and trigger one final commit
    /// attempt before exit, rather than being aborted mid-sleep and
    /// leaving the idempotency slot stranded.
    pub shutdown: CancellationToken,
}

impl AppState {
    /// Build the state from a connected [`Store`] *without* a credential vault.
    /// The engine has no credential resolver and HTTP nodes that reference
    /// `credential_ref` will fail closed.
    #[must_use]
    pub fn new(store: Store) -> Self {
        let store = Arc::new(store);
        let sub_resolver = Arc::new(StoreSubWorkflowResolver::new(Arc::clone(&store)));
        let approval_gate = Arc::new(StoreApprovalGate::new(Arc::clone(&store)));
        let engine = Engine::new(a2w_nodes::default_registry())
            .with_sub_workflows(sub_resolver)
            .with_approvals(approval_gate);
        Self {
            store,
            engine: Arc::new(engine),
            vault: None,
            bg_tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
        }
    }

    /// Build the state with a vault wired through a
    /// [`StoreCredentialResolver`]: HTTP / MCP nodes that name a
    /// `credential_ref` resolve through this vault at run time, and the
    /// `/credentials` endpoints become available.
    #[must_use]
    pub fn with_vault(store: Store, vault: Vault) -> Self {
        let store = Arc::new(store);
        let vault = Arc::new(vault);
        let resolver = Arc::new(StoreCredentialResolver::new(
            Arc::clone(&store),
            Arc::clone(&vault),
        ));
        let sub_resolver = Arc::new(StoreSubWorkflowResolver::new(Arc::clone(&store)));
        let approval_gate = Arc::new(StoreApprovalGate::new(Arc::clone(&store)));
        let engine = Engine::new(a2w_nodes::default_registry())
            .with_credentials(resolver)
            .with_sub_workflows(sub_resolver)
            .with_approvals(approval_gate);
        Self {
            store,
            engine: Arc::new(engine),
            vault: Some(vault),
            bg_tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
        }
    }
}
