//! Model transport.
//!
//! `Block`, `Usage` and `StopReason` are [`journal`]'s: this crate
//! depends on the journal, so it uses the schema types directly rather than
//! defining twins. The Anthropic wire shapes are private to `anthropic` and
//! convert at the edge, which is what stops an API change from becoming a
//! schema migration.
//!
//! Retry lives behind [`Provider`], so the loop never sees a 429.

mod anthropic;
mod config;
mod copilot;
mod mock;
mod openai;
mod profile;
mod provider;
mod recorded;
mod retry;
mod unconfigured;

pub use anthropic::AnthropicProvider;
pub use config::{
    ANTHROPIC_VERSION, Auth, AuthError, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, Display, Effort,
    OAUTH_BETA, ProviderConfig, Thinking,
};
pub use copilot::{
    ACCESS_TOKEN_URL, CopilotAuthError, CopilotModel, CopilotProvider, DEVICE_CODE_URL, DeviceCode,
    PollOutcome, SESSION_TOKEN_URL, SessionToken, exchange_session_token, fetch_models,
    poll_device_flow, start_device_flow,
};
pub use mock::{MockProvider, ScriptedTurn};
pub use openai::OpenAiProvider;
pub use profile::{
    AuthKind, BadBaseUrl, EnvCredential, ModelInfo, PROFILES, ProviderId, ProviderProfile,
    UnknownProviderId, context_window_for, transport,
};
pub use provider::{
    Attempt, Provider, ProviderError, Role, StreamEvent, ThinkingDelta, ToolCallStart, TurnBlock,
    TurnEnd, TurnMessage, TurnRequest, TurnThinking, TurnTool, TurnToolResult, TurnToolUse,
};
pub use recorded::{Recorded, ReplayError};
pub use unconfigured::Unconfigured;
