//! Stdio transport for Model Context Protocol (MCP) servers.
//!
//! **The server is started through the broker**, like every other process this
//! harness runs. An MCP server is a program named by configuration that then
//! runs with the session's privileges, so it is exactly the thing the policy
//! exists to decide about. It previously spawned with `tokio::process::Command`
//! directly, which was the one exec in the workspace no policy saw -- not by
//! design, but because a long-lived bidirectional child was a shape the broker
//! could not produce until `Broker::serve` existed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use sandbox::{AuthorizedService, ToolCx};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::mcp::transport::{McpError, Transport};

// ========================================================================
// Data Structures
// ========================================================================

pub struct StdioTransport {
    next_id: AtomicU64,
    /// **One lock over both directions, deliberately.** The streams used to be
    /// two mutexes, so a request released stdin before taking stdout: two
    /// concurrent calls could interleave and each read the other's reply. A
    /// JSON-RPC exchange is one critical section, not two.
    service: Mutex<AuthorizedService>,
}

// ========================================================================
// StdioTransport Implementations
// ========================================================================

impl StdioTransport {
    /// Start an MCP server through the broker and run the MCP handshake.
    ///
    /// A denial is an ordinary outcome: the policy may refuse the program, and
    /// the caller sees why rather than a spawn error.
    pub async fn spawn(cx: &ToolCx, cmd: &str, args: &[String]) -> Result<Arc<Self>, McpError> {
        let service = cx.broker.serve(cmd, args).await.map_err(|denied| {
            McpError::Transport(format!(
                "MCP server `{cmd}` was not started: {}",
                denied.reason
            ))
        })?;

        let transport = Arc::new(Self {
            next_id: AtomicU64::new(1),
            service: Mutex::new(service),
        });

        // Initialize MCP Protocol Handshake
        transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "summer-code",
                        "version": "0.1.0"
                    }
                }),
            )
            .await?;

        Ok(transport)
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let mut line = req.to_string();
        line.push('\n');

        // Held across both halves: see the field's own note on why splitting
        // this was a bug.
        let mut resp_line = String::new();
        {
            let mut service = self.service.lock().await;
            service
                .send(line.as_bytes())
                .await
                .map_err(|e| McpError::Transport(format!("Failed to write to stdin: {e}")))?;
            service
                .read_line(&mut resp_line)
                .await
                .map_err(|e| McpError::Transport(format!("Failed to read response line: {e}")))?;
        }

        let response: Value = serde_json::from_str(&resp_line)
            .map_err(|e| McpError::Malformed(format!("Invalid JSON-RPC response: {e}")))?;

        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown RPC error")
                .to_string();
            return Err(McpError::Remote { code, message });
        }

        response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Malformed("Response missing `result` field".into()))
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> ToolCx {
        ToolCx::new(
            Arc::new(sandbox::Broker::permissive(std::env::temp_dir())),
            Arc::from(std::env::temp_dir().as_path()),
            tokio_util::sync::CancellationToken::new(),
            std::time::Instant::now(),
        )
    }

    #[tokio::test]
    async fn fails_on_invalid_command() {
        let res = StdioTransport::spawn(&cx(), "non_existent_mcp_cmd_12345", &[]).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn a_server_the_policy_denies_is_reported_as_denied() {
        // The point of routing this through the broker: an MCP server named by
        // configuration is a program running with the session's privileges, and
        // the policy gets to decide. A permissive broker allows; a real one
        // asks, and an unattended session denies.
        let denying = ToolCx::new(
            Arc::new(
                sandbox::BrokerBuilder::new(policy::Policy::builtin(std::env::temp_dir()))
                    .build(true)
                    .expect("unenforced is allowed here"),
            ),
            Arc::from(std::env::temp_dir().as_path()),
            tokio_util::sync::CancellationToken::new(),
            std::time::Instant::now(),
        );
        let res = StdioTransport::spawn(&denying, "some-mcp-server", &[]).await;
        assert!(res.is_err(), "an unauthorized server must not start");
    }
}
