//! Binary entry point for the A2W read-only REST + observability server.
//!
//! Connects the store from `A2W_DB_URL` (default
//! `"sqlite://a2w.db?mode=rwc"`), builds the [`AppState`], binds `A2W_BIND`
//! (default `"127.0.0.1:8080"`), logs the bound address, and serves the router
//! from [`a2w_server::app`].

#![forbid(unsafe_code)]

use std::error::Error;

use a2w_server::{app, AppState};
use a2w_store::Store;

/// Default SQLite URL (file-backed, created on demand).
const DEFAULT_DB_URL: &str = "sqlite://a2w.db?mode=rwc";
/// Default bind address.
const DEFAULT_BIND: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let db_url = std::env::var("A2W_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
    let bind = std::env::var("A2W_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

    let store = Store::connect(&db_url).await?;
    let state = AppState::new(store);
    let router = app(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let addr = listener.local_addr()?;
    println!("a2w-server listening on http://{addr} (db: {db_url})");

    axum::serve(listener, router).await?;
    Ok(())
}
