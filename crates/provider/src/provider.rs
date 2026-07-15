//! The provider seam and the provider-neutral request/stream types.
//!
//! [`Block`], [`Usage`] and [`StopReason`] are `journal`'s, not twins of
//! them: this crate depends on the journal, so it uses the schema types
//! directly. The Anthropic wire shapes are private to `anthropic/` and convert
//! at the edge, which is what stops an API change becoming a schema migration.

use std::time::Duration;

use futures_util::stream::BoxStream;
use journal::{CallId, StopReason, Usage};
use serde_json::Value;

use crate::config::Effort;
use thiserror::Error;

// ========================================================================
// Constants
// ========================================================================

/// What the providers' error bodies say when a request overflows the context
/// window, lowercased. See [`ProviderError::is_context_overflow`].
const CONTEXT_OVERFLOW_PHRASES: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "prompt is too long",
];

// ========================================================================
// Traits
// ========================================================================

/// A model transport. One turn returns a stream of [`StreamEvent`]s. Retry lives
/// behind the implementation, so the loop never sees a 429. The stream is
/// `'static`: it owns the response, so the loop can mutate the engine while it
/// lives without a borrow conflict.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError>;

    /// The exact model id this provider talks to, for `SessionStarted.model`.
    ///
    /// On the trait rather than read off a concrete config, because the engine
    /// holds a `dyn Provider` and the id has to reach the journal from whichever
    /// one is wired. The offline implementations answer with what they are: a
    /// recording replayed by `Recorded` was made against some model, and saying
    /// so is more useful to a reader than an empty string.
    fn model(&self) -> &str;

    /// Exact, server-side token count for a request. There is no official Rust
    /// tokenizer, and `tiktoken` is OpenAI's: it undercounts Claude by 15-20% on
    /// prose and worse on code, so it is not an option here.
    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError>;

    /// Take the transport failures recorded since the last drain.
    ///
    /// The engine is the only journal writer, so the provider cannot append a
    /// `ProviderFailed` itself; it records attempts and the engine drains them
    /// around each turn. This is the same shape as the broker's permission
    /// records, and for the same reason.
    ///
    /// Implementations that cannot fail (the mock, the Phase 3 recorded
    /// provider) inherit the empty default.
    fn drain_attempts(&self) -> Vec<Attempt> {
        Vec::new()
    }

    /// Change the reasoning effort this provider sends, live, without rebuilding
    /// the transport. `None` disables it (some models 400 on `output_config.effort`).
    /// Default no-op so the offline providers (mock, recorded) ignore it; the live
    /// ones override it to mutate their config.
    fn set_effort(&mut self, _effort: Option<Effort>) {}

    /// The most output a turn may ask for: the `max_tokens` this provider
    /// sends. The prompt and this together must fit the model's context window,
    /// which the API checks, so the window budget leaves this much room. `None`
    /// for the offline providers, which send nothing.
    fn max_output_tokens(&self) -> Option<u32> {
        None
    }

    /// Construct a clone of this provider wired to a different model id
    /// (e.g. for multi-model subagent routing or cheap tier fallback).
    fn with_model(&self, _model: &str) -> Option<Box<dyn Provider>> {
        None
    }
}

// ========================================================================
// Data Structures
// ========================================================================

/// A single turn request: system prompt, tools, and the conversation so far.
pub struct TurnRequest {
    pub messages: Vec<TurnMessage>,
    pub system: String,
    pub tools: Vec<TurnTool>,
}

/// A tool definition handed to the model. Provider-neutral; the engine builds
/// these from `tools::ToolSpec`.
#[derive(Clone)]
pub struct TurnTool {
    pub description: String,
    pub input_schema: Value,
    pub name: String,
}

/// One message in the conversation.
#[derive(Clone)]
pub struct TurnMessage {
    pub content: Vec<TurnBlock>,
    pub role: Role,
}

#[derive(Clone, Copy)]
pub enum Role {
    Assistant,
    User,
}

/// One block of message content.
///
/// `Thinking` is here because a thinking block must be echoed back on the next
/// turn of the same conversation, byte-identical: `signature` is opaque and the
/// API rejects a modified block.
#[derive(Clone)]
pub enum TurnBlock {
    Text(String),
    Thinking(TurnThinking),
    ToolResult(TurnToolResult),
    ToolUse(TurnToolUse),
}

#[derive(Clone)]
pub struct TurnThinking {
    pub signature: String,
    pub text: String,
}

#[derive(Clone)]
pub struct TurnToolResult {
    pub call: CallId,
    pub content: String,
    pub is_error: bool,
}

#[derive(Clone)]
pub struct TurnToolUse {
    pub args: Value,
    pub call: CallId,
    pub name: String,
}

/// One event in a model turn's stream.
///
/// **`TurnEnd` is the only thing that produces a `ModelTurn`.** A stream that
/// stops before `message_stop` yields no `TurnEnd`; the loop must not treat a
/// truncated stream as a completed turn.
pub enum StreamEvent {
    /// One content block closed. The accumulated partial JSON of an open
    /// `tool_use` is parsed here, and never before.
    BlockEnd,
    TextDelta(String),
    ThinkingDelta(ThinkingDelta),
    ToolCallDelta(String),
    ToolCallStart(ToolCallStart),
    TurnEnd(TurnEnd),
    /// The partial turn was discarded and the request re-sent whole.
    ///
    /// **Everything emitted since the stream began (or since the last
    /// `TurnRestarted`) must be dropped by the consumer.** A mid-stream failure
    /// cannot be resumed: there is no wire mechanism to continue an interrupted
    /// assistant message, so the provider re-sends and the model starts over.
    /// Resetting the decoder is not enough on its own, because the consumer has
    /// already been handed the blocks from the dead attempt; without this event
    /// it would append the second attempt's blocks to the first's and journal a
    /// turn the model never produced.
    TurnRestarted,
}

/// A thinking block's deltas. `signature` arrives once, at the end of the block;
/// `text` is empty unless the provider asked for `display: "summarized"`.
pub struct ThinkingDelta {
    pub signature: Option<String>,
    pub text: Option<String>,
}

pub struct ToolCallStart {
    pub call: CallId,
    pub name: String,
}

pub struct TurnEnd {
    pub stop: StopReason,
    pub usage: Usage,
}

/// One transport failure, for the journal. Recorded per attempt, so a retry
/// storm that ended in success is still visible.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub attempt: u32,
    pub failure: String,
}

// ========================================================================
// Errors
// ========================================================================

/// A model-transport failure. Retryable variants are handled inside the
/// provider; only `Fatal` reaches the loop.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// 400, 401, 403, 404, 413. The server understood the request and rejected
    /// it, so retrying is not a strategy. `message` carries the response body,
    /// which is where the API names the parameter that was wrong.
    ///
    /// **`status` is optional, and it used to be a sentinel.** A replay that
    /// diverged reported `status: 0` and a `count_tokens` response missing its
    /// field reported `status: 200`; neither is an HTTP status the server sent,
    /// and both render as one in the `Display` above -- "fatal: HTTP 200" reads
    /// as a contradiction and "HTTP 0" as corruption. Not every fatal failure
    /// comes from a response, so the type says so.
    #[error("fatal{}: {kind}: {message}", status.map(|code| format!(" (HTTP {code})")).unwrap_or_default())]
    Fatal {
        kind: String,
        message: String,
        status: Option<u16>,
    },
    /// 529.
    #[error("provider overloaded")]
    Overloaded,
    /// 429.
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },
    /// 5xx other than 529.
    #[error("server error: HTTP {status}")]
    Server { status: u16 },
    /// Bytes stopped before `message_stop`. The partial turn is discarded and
    /// the turn is re-sent whole: there is no wire mechanism to resume an
    /// interrupted assistant message.
    #[error("stream broke before message_stop: {0}")]
    StreamBroken(String),
    /// Connection reset, TLS, DNS, timeout.
    #[error("transport: {0}")]
    Transport(String),
    /// No credential is configured, so the session cannot reach the model. The
    /// CLI starts the session anyway and surfaces this on the first turn, rather
    /// than refusing to open at all.
    #[error("{0}")]
    Unconfigured(String),
}

// ========================================================================
// ProviderError Implementations
// ========================================================================

impl ProviderError {
    /// Whether the server refused the request because it does not fit the
    /// model's context window.
    ///
    /// Read from the response body, since no provider gives this its own
    /// status: Anthropic says "prompt is too long", OpenAI and the servers
    /// compatible with it say `context_length_exceeded` or "maximum context
    /// length". The engine compacts and retries once on it, which is the
    /// answer the local token estimate should have given before sending.
    pub fn is_context_overflow(&self) -> bool {
        let Self::Fatal { kind, message, .. } = self else {
            return false;
        };
        let text = format!("{kind} {message}").to_lowercase();
        CONTEXT_OVERFLOW_PHRASES
            .iter()
            .any(|phrase| text.contains(phrase))
    }

    /// Whether the provider should back off and try again. Only `Fatal` is
    /// terminal: the server understood it and said no.
    pub fn is_retryable(&self) -> bool {
        match self {
            // Retrying a missing credential just fails again; the fix is the
            // user's, not a backoff's.
            Self::Fatal { .. } | Self::Unconfigured(_) => false,
            Self::Overloaded
            | Self::RateLimited { .. }
            | Self::Server { .. }
            | Self::StreamBroken(_)
            | Self::Transport(_) => true,
        }
    }

    /// The `ProviderFailed.failure` string for this error. Stable, so Phase 8
    /// can count failures by category out of the journals.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Fatal { .. } => "fatal",
            Self::Overloaded => "overloaded",
            Self::RateLimited { .. } => "rate_limited",
            Self::Server { .. } => "server",
            Self::StreamBroken(_) => "stream_broken",
            Self::Transport(_) => "transport",
            Self::Unconfigured(_) => "unconfigured",
        }
    }
}
