//! Error type for the A2W IR crate.

use thiserror::Error;

/// Errors produced while (de)serializing or otherwise handling the IR.
///
/// For M1 this is a thin wrapper around [`serde_json::Error`]; additional
/// variants (e.g. unsupported schema version) can be added without breaking
/// the public surface because the enum is `#[non_exhaustive]`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IrError {
    /// A JSON (de)serialization error from `serde_json`.
    #[error("JSON (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}
