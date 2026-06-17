//! Stdio entry point for the A2W MCP server.
//!
//! Running `cargo run -p a2w-mcp` starts an MCP server speaking the protocol
//! over stdin/stdout, which an agent (e.g. Claude) connects to. The server
//! exposes the `wf_*` tools defined in [`a2w_mcp`].
//!
//! Logging goes to **stderr** only: stdout is the MCP transport and must carry
//! nothing but protocol frames.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use a2w_mcp::A2wServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

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

    // Build the server (one shared Engine + registry) and serve it over the
    // stdio transport. `serve` performs the MCP initialize handshake; `waiting`
    // blocks until the peer closes the connection.
    let service = A2wServer::new().serve(stdio()).await?;
    service.waiting().await?;

    eprintln!("a2w-mcp: client disconnected, shutting down");
    Ok(())
}
