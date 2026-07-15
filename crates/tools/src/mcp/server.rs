//! The MCP server: the engine's tools and resources over the protocol.
//!
//! A peer driving the agent sends `tools/list` to discover what is available and
//! `tools/call` to run one. The server wraps a [`ToolSource`] and answers both,
//! so an external MCP client drives the same tools the loop does.
//!
//! **It drives them through the [`Dispatcher`], not by calling them.** That one
//! choice is what makes "the same tools the loop does" true rather than nearly
//! true: schema validation, the per-call timeout, and panic isolation all live
//! on the dispatcher, so a server that called `tool.call` directly would offer a
//! peer a path into the tools with none of them. A malformed `tools/call` would
//! reach the tool as `Value::Null`, a panicking tool would unwind through the
//! server task instead of becoming an `Outcome::Err`, and a hung tool would run
//! unbounded.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use journal::{CallId, Outcome};
use sandbox::ToolCx;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::mcp::transport::{McpError, Transport};
use crate::{Dispatcher, DuplicateTool, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// Serves a set of tools over MCP. Built from any [`ToolSource`].
///
/// The `Mutex` is what lets a `&self` request handler reach a dispatcher that
/// takes `&mut self`, and it serializes calls, which matches the plan's serial
/// dispatch rather than working around it.
pub struct McpServer {
    calls: AtomicU64,
    dispatcher: Mutex<Dispatcher>,
}

// ========================================================================
// McpServer Implementations
// ========================================================================

impl McpServer {
    /// Build a server from a tool source.
    ///
    /// Fallible because two tools may claim one name, and the server is exactly
    /// where that matters: it is the surface a peer discovers tools through.
    pub fn from_source(source: &dyn ToolSource) -> Result<Self, DuplicateTool> {
        Ok(Self {
            calls: AtomicU64::new(0),
            dispatcher: Mutex::new(Dispatcher::new(source.tools())?),
        })
    }

    /// Answer one MCP request. `cx` is the capability a dispatched tool receives.
    pub async fn handle(
        &self,
        method: &str,
        params: Value,
        cx: &ToolCx,
    ) -> Result<Value, McpError> {
        match method {
            "tools/list" => self.list().await,
            "tools/call" => self.call(params, cx).await,
            _ => Err(McpError::Remote {
                code: -32601,
                message: format!("unknown method: {method}"),
            }),
        }
    }

    async fn list(&self) -> Result<Value, McpError> {
        let dispatcher = self.dispatcher.lock().await;
        let tools: Vec<Value> = dispatcher
            .specs()
            .into_iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "inputSchema": spec.input_schema,
                })
            })
            .collect();
        Ok(json!({ "tools": tools }))
    }

    async fn call(&self, params: Value, cx: &ToolCx) -> Result<Value, McpError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Malformed("tools/call requires `name`".into()))?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let dispatcher = self.dispatcher.lock().await;
        // An unknown tool is a protocol error, not a `tools/call` result with
        // `isError` set: the peer named something that does not exist, which is
        // invalid params. Only a tool that ran can report having failed.
        if !dispatcher.has(name) {
            return Err(McpError::Remote {
                code: -32602,
                message: format!("unknown tool: {name}"),
            });
        }
        // A peer supplies no call id, so one is minted per call. The dedup
        // ledger therefore never fires here, which is correct: a peer's two
        // calls are two calls, not a retry of one. Validation, the timeout and
        // panic isolation are what this path is going through the dispatcher
        // for.
        let call = CallId::new(format!(
            "mcp_{}",
            self.calls.fetch_add(1, Ordering::Relaxed)
        ));
        let outcome = dispatcher.dispatch(&call, name, args, cx).await;
        Ok(outcome_to_mcp(&outcome))
    }
}

/// Render an outcome as an MCP `tools/call` result: a text content block and an
/// `isError` flag.
fn outcome_to_mcp(outcome: &Outcome) -> Value {
    let (text, is_error) = match outcome {
        Outcome::Ok { content } => (content.clone(), false),
        Outcome::Err { message } => (message.clone(), true),
        Outcome::Denied { reason } => (reason.clone(), true),
        Outcome::TimedOut => ("timed out".to_string(), true),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

/// A [`Transport`] that calls an [`McpServer`] directly, in-process. For tests
/// and for an engine serving itself, with no bytes on a wire.
pub struct InProcessTransport {
    server: Arc<McpServer>,
    cx: ToolCx,
}

impl InProcessTransport {
    pub fn new(server: Arc<McpServer>, cx: ToolCx) -> Self {
        Self { server, cx }
    }
}

#[async_trait::async_trait]
impl Transport for InProcessTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.server.handle(method, params, &self.cx).await
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use sandbox::Broker;

    use super::*;
    use crate::Builtin;

    fn cx() -> ToolCx {
        ToolCx::new(
            Arc::new(Broker::permissive(std::env::temp_dir())),
            Arc::from(std::env::temp_dir().as_path()),
            tokio_util::sync::CancellationToken::new(),
            std::time::Instant::now(),
        )
    }

    #[tokio::test]
    async fn list_returns_every_tool_spec() {
        let server = McpServer::from_source(&Builtin::new()).expect("built-in names are unique");
        let result = server.handle("tools/list", json!({}), &cx()).await.unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "read"));
        assert!(tools.iter().any(|t| t["name"] == "edit"));
    }

    #[tokio::test]
    async fn an_unknown_method_is_an_error() {
        let server = McpServer::from_source(&Builtin::new()).expect("built-in names are unique");
        let err = server
            .handle("no/such", json!({}), &cx())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Remote { code: -32601, .. }));
    }
}
