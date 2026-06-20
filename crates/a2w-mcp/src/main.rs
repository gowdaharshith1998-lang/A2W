//! Stdio entry point for the A2W MCP server.
//!
//! Running `cargo run -p a2w-mcp` starts an MCP server speaking the protocol
//! over stdin/stdout, which an agent (e.g. Claude) connects to. The server
//! exposes the `wf_*` tools defined in [`a2w_mcp`].
//!
//! ## Environment
//! - `A2W_DB_URL` — optional SQLite URL (default `sqlite://a2w.db?mode=rwc`).
//!   Read only when `A2W_MASTER_KEY` is also set (otherwise the server runs
//!   stateless, with credential tools disabled).
//! - `A2W_MASTER_KEY` — base64 32-byte master key. When set, a credential
//!   [`Vault`] is wired into the engine so HTTP nodes can resolve
//!   `credential_ref`s, and the `wf_*_credential` tools become live.
//! - `A2W_MCP_ALLOW_RUN` — set to `true` to allow `wf_run` to execute real
//!   side effects. Default: rejected.
//! - `A2W_MCP_ALLOW_LLM` — set to `true` to allow
//!   `generate_workflow_from_prompt` (costs LLM budget). Default: rejected.
//! - `A2W_MCP_ALLOW_CREDENTIAL_WRITES` — set to `true` to allow
//!   `wf_store_credential` and `wf_delete_credential` to mutate the vault.
//!   Reading the credential listing is always allowed once the vault is
//!   configured.
//!
//! ## Threat model
//! The MCP stdio transport is **local-trust**: any process that can spawn
//! `a2w-mcp` has full access to whichever tools the policy permits. The flags
//! above are an *operator* gate — they prevent accidental writes, but they do
//! not authenticate the calling agent. Treat the process boundary as the trust
//! boundary.
//!
//! Logging goes to **stderr** only: stdout is the MCP transport and must carry
//! nothing but protocol frames.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use a2w_mcp::{A2wServer, McpPolicy};
use a2w_store::{Store, Vault};
use rmcp::transport::stdio;
use rmcp::ServiceExt;

/// Default SQLite URL when `A2W_DB_URL` is unset.
const DEFAULT_DB_URL: &str = "sqlite://a2w.db?mode=rwc";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("a2w-mcp: fatal error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve the A2W MCP server over stdio until the client disconnects.
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("a2w-mcp: starting MCP stdio server (wf_* tools)");

    let policy = McpPolicy::from_env();
    eprintln!(
        "a2w-mcp: policy: allow_run={} allow_llm={} allow_credential_writes={} \
         (override with A2W_MCP_ALLOW_RUN / A2W_MCP_ALLOW_LLM / \
         A2W_MCP_ALLOW_CREDENTIAL_WRITES)",
        policy.allow_run, policy.allow_llm, policy.allow_credential_writes
    );

    // If the operator has set A2W_MASTER_KEY, wire a vault-backed credential
    // resolver into the engine. Misconfiguration (e.g. wrong-sized key) is a
    // FATAL startup error rather than a silent fallback so the operator never
    // ships a server that quietly ignores the key they configured.
    let server = match std::env::var("A2W_MASTER_KEY") {
        Ok(_) => {
            let db_url =
                std::env::var("A2W_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
            let store = Arc::new(Store::connect(&db_url).await?);
            let vault = Arc::new(Vault::from_env()?);
            eprintln!("a2w-mcp: credential vault enabled (db: {db_url})");
            A2wServer::with_vault_and_policy(store, vault, policy)
        }
        Err(_) => {
            eprintln!(
                "a2w-mcp: WARNING credential vault disabled (A2W_MASTER_KEY not set); \
                 HTTP/MCP nodes that reference a credential_ref will fail closed, \
                 and wf_*_credential tools return an error"
            );
            A2wServer::with_policy(policy)
        }
    };

    // Build the server (one shared Engine + registry) and serve it over the
    // stdio transport. `serve` performs the MCP initialize handshake; `waiting`
    // blocks until the peer closes the connection.
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    eprintln!("a2w-mcp: client disconnected, shutting down");
    Ok(())
}
