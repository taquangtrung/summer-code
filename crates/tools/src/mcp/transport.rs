//! The MCP transport: a request/response seam over JSON-RPC.
//!
//! Kept deliberately narrow so a real transport (stdio, a socket) is a thin
//! adapter: it carries a method name and a JSON `params` object in, and a JSON
//! `result` (or an error) out. The client and server never see raw bytes.

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

// ========================================================================
// Traits
// ========================================================================

/// One round-trip to an MCP peer: send `method` with `params`, get `result`.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;
}

// ========================================================================
// Errors
// ========================================================================

/// A transport- or protocol-level failure. A tool that errored is not this: that
/// is an [`journal::Outcome::Err`] returned in a successful response.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("the peer returned an error: {message}")]
    Remote { code: i64, message: String },
    #[error("malformed response: {0}")]
    Malformed(String),
    #[error("transport: {0}")]
    Transport(String),
}
