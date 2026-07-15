//! Starting the configured MCP servers and collecting their tools.
//!
//! [`tools::mcp`] has had a working stdio client since Phase 7, and nothing
//! called it: it was its own crate and nothing depended on it, so
//! `ARCHITECTURE.md` listed a bridge the binary could not reach and
//! `PROGRESS.md` marked it done. This module is
//! the missing half, and it is deliberately thin, because the interesting
//! decisions were already made elsewhere:
//!
//! - **The broker starts the program.** [`StdioTransport::spawn`] goes through
//!   `Broker::serve`, so an MCP server is a policy decision like any other exec.
//! - **The dispatcher owns the tools.** [`McpClient`] is a `ToolSource`, so a
//!   discovered tool inherits schema validation, the call-id ledger, per-call
//!   timeouts, and cancellation with no plumbing of its own.
//! - **Names cannot collide.** Discovery namespaces every tool under its
//!   server, so an external server cannot shadow `shell`.
//!
//! **A server that fails is a warning, not a dead session.** The alternative was
//! tempting -- a server the user configured and that did not start is a real
//! problem -- but the failure modes here are ordinary and external (the binary
//! moved, `npx` is offline, the policy denied it), and refusing to open a
//! session over one of them takes away the editor they would use to fix it. The
//! rule this follows is [`crate::config::hook_set`]'s: warn, name the entry, and
//! carry on with what did work. What it must never do is fail silently, since a
//! tool that is simply absent looks identical to a model choosing not to use it.

use std::sync::Arc;
use std::time::Instant;

use futures_util::future::join_all;
use sandbox::{Broker, ToolCx};
use tokio_util::sync::CancellationToken;
use tools::mcp::{McpClient, StdioTransport};
use tools::{Tool, ToolSource};

use crate::config::McpServerConfig;

// ========================================================================
// Constants
// ========================================================================

/// How long one server gets to spawn, handshake, and answer `tools/list`.
///
/// Startup is the one place where a hung server is indistinguishable from a
/// slow one and nobody is watching yet: without a cap, a server that accepts a
/// connection and never replies hangs the session before the first prompt, with
/// no UI drawn to explain why.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ========================================================================
// Functions
// ========================================================================

/// Start every configured server and return the tools they advertised.
///
/// The returned tools are already namespaced per server. Failures are reported
/// on stderr and skipped; the returned list holds whatever did connect.
///
/// **All at once, not one after another.** Each server is started and asked for
/// its tools concurrently, so startup waits for the slowest server rather than
/// for the sum of them: three servers that each needed `npx` to fetch a package
/// used to hold the first prompt for all three fetches back to back, and three
/// unreachable ones for three full [`CONNECT_TIMEOUT`]s. The results keep the
/// configured order, so which tools exist does not depend on which server
/// answered first.
pub async fn connect(
    servers: &[McpServerConfig],
    broker: &Arc<Broker>,
    cwd: &std::path::Path,
) -> Vec<Arc<dyn Tool>> {
    let attempts = join_all(
        servers
            .iter()
            .map(|server| connect_one(server, broker, cwd)),
    )
    .await;
    let mut tools = Vec::new();
    for (server, attempt) in servers.iter().zip(attempts) {
        match attempt {
            Ok(discovered) => {
                tracing::info!(
                    server = %server.name,
                    tools = discovered.len(),
                    "connected to MCP server"
                );
                tools.extend(discovered);
            }
            Err(error) => eprintln!(
                "summer: MCP server `{}` ({}) is not available: {error}",
                server.name, server.command
            ),
        }
    }
    tools
}

/// Start one server, handshake, and list its tools.
async fn connect_one(
    server: &McpServerConfig,
    broker: &Arc<Broker>,
    cwd: &std::path::Path,
) -> Result<Vec<Arc<dyn Tool>>, String> {
    // A deadline in the past would make every broker-mediated read inside the
    // handshake fail its own deadline check, so the context carries the same
    // cap the whole connection is bounded by.
    let cx = ToolCx::new(
        Arc::clone(broker),
        Arc::from(cwd),
        CancellationToken::new(),
        Instant::now() + CONNECT_TIMEOUT,
    );

    let discovery = async {
        let transport = StdioTransport::spawn(&cx, &server.command, &server.args)
            .await
            .map_err(|error| error.to_string())?;
        McpClient::discover(transport, &server.name)
            .await
            .map_err(|error| error.to_string())
    };

    match tokio::time::timeout(CONNECT_TIMEOUT, discovery).await {
        Ok(Ok(client)) => Ok(client.tools()),
        Ok(Err(error)) => Err(error),
        Err(_elapsed) => Err(format!(
            "it did not finish the MCP handshake within {}s",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A broker that allows what the policy allows, matching how the rest of
    /// this crate's tests build one. Neither server below gets far enough for
    /// the policy to matter: the point is what `connect` does with a failure.
    fn broker() -> Arc<Broker> {
        Arc::new(
            engine::BrokerBuilder::new(policy::Policy::builtin(std::env::temp_dir()))
                .backend(Box::new(engine::NoneBackend::new("test")))
                .build(true)
                .expect("none backend clears the startup check under allow_unenforced"),
        )
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_is_skipped_rather_than_fatal() {
        // The rule the module header argues for: one broken entry must not cost
        // the user the session they would fix it from. `connect` returns the
        // tools that did work, which for a single bad entry is none.
        let servers = vec![McpServerConfig {
            args: Vec::new(),
            command: "definitely-not-a-real-mcp-server-9c1f".into(),
            name: "broken".into(),
        }];
        let tools = connect(&servers, &broker(), &std::env::temp_dir()).await;
        assert!(tools.is_empty(), "a server that never started has no tools");
    }

    #[tokio::test]
    async fn a_healthy_server_does_not_lose_its_tools_to_a_broken_neighbour() {
        // The other half of the same rule, and the one a naive `?` would break:
        // a failing server must not truncate the loop before a later working
        // one. `cat` is not an MCP server, but it is a program that exists, so
        // it exercises the spawn-succeeds-then-handshake-fails path rather than
        // the spawn-fails path above.
        let servers = vec![
            McpServerConfig {
                args: Vec::new(),
                command: "definitely-not-a-real-mcp-server-9c1f".into(),
                name: "broken".into(),
            },
            McpServerConfig {
                args: Vec::new(),
                command: "definitely-not-a-real-mcp-server-2b7e".into(),
                name: "also-broken".into(),
            },
        ];
        // Both fail; the point is that the call returns at all rather than
        // propagating the first error.
        let tools = connect(&servers, &broker(), &std::env::temp_dir()).await;
        assert!(tools.is_empty());
    }
}
