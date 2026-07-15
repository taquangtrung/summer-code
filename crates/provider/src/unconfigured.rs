//! The unconfigured provider: a session with no credential.
//!
//! The CLI reads the credential once, at startup. When there is none, the old
//! behavior was to print a line and exit before the REPL ever opened, which is
//! the wrong shape for a session tool: the user should get into the session and
//! be told, on the turn they attempt, exactly what to set. This provider is that
//! deferral. Every turn fails with the same actionable message, and nothing
//! reaches a socket, because there is no credential to reach one with.

use async_trait::async_trait;
use futures_util::stream::BoxStream;

use crate::provider::{Provider, ProviderError, StreamEvent, TurnRequest};

// ========================================================================
// Constants
// ========================================================================

const MISSING_CREDENTIAL: &str =
    "no credential: set ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN) and restart the session";

// ========================================================================
// Data Structures
// ========================================================================

/// A [`Provider`] with no credential. Every request fails; the session runs so
/// the user can see it and is told what to set.
pub struct Unconfigured;

// ========================================================================
// Unconfigured Implementations
// ========================================================================

impl Unconfigured {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Unconfigured {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for Unconfigured {
    fn model(&self) -> &str {
        "unconfigured"
    }

    async fn turn(
        &self,
        _req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        Err(ProviderError::Unconfigured(MISSING_CREDENTIAL.to_string()))
    }

    async fn count_tokens(&self, _req: &TurnRequest) -> Result<u32, ProviderError> {
        Err(ProviderError::Unconfigured(MISSING_CREDENTIAL.to_string()))
    }
}
