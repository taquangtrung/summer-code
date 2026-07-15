//! MCP, client and server.
//!
//! The client is a [`ToolSource`](crate::ToolSource) and nothing more:
//! that one line is why external tools inherit schema validation, timeouts,
//! dedup, and the policy check — they go through the dispatcher the same way a
//! built-in does. The server exposes the engine's tools over the protocol so an
//! external client drives the same task path.
//!
//! Both directions live here. The transport seam is narrow enough that a real
//! transport (stdio, a socket) is a thin adapter over [`Transport`].
//!
//! MCP sits in `tools` rather than beside it because it is a source of tools
//! and nothing else: everything it produces reaches a caller through the
//! dispatcher in this crate, and the module was never usable on its own.

mod client;
mod server;
mod stdio;
mod transport;

pub use client::McpClient;
pub use server::{InProcessTransport, McpServer};
pub use stdio::StdioTransport;
pub use transport::{McpError, Transport};
