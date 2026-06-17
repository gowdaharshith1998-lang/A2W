//! The shared application state handed to every handler.

use std::sync::Arc;

use a2w_engine::Engine;
use a2w_store::Store;

/// Shared, cheaply-cloneable application state.
///
/// `axum` clones the state into each request, so the expensive members live
/// behind [`Arc`]: the [`Store`] (a pooled DB handle) and the [`Engine`] (which
/// owns the node registry). Build the engine once with
/// [`a2w_nodes::default_registry`] in [`AppState::new`].
#[derive(Clone)]
pub struct AppState {
    /// The persistence handle (workflows + run history).
    pub store: Arc<Store>,
    /// The execution engine, preloaded with the default node registry.
    pub engine: Arc<Engine>,
}

impl AppState {
    /// Build the state from a connected [`Store`], constructing the engine over
    /// the default node registry.
    #[must_use]
    pub fn new(store: Store) -> Self {
        let engine = Engine::new(a2w_nodes::default_registry());
        Self {
            store: Arc::new(store),
            engine: Arc::new(engine),
        }
    }
}
