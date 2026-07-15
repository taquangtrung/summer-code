//! The engine: the agent loop, limits, and the session.
//!
//! Phase 1a lands the loop closing: a serial dispatch turn cycle with enforced
//! [`Limits`], journaling every record. Context windowing/compaction (Phase 4),
//! resume (Phase 3), and the query engine (Phase 5) are wired in later; this
//! crate exists now so the loop runs end to end.
//!
//! `engine` is a harness crate: it opens the journal file, so it carries
//! `#![allow(clippy::disallowed_methods)]` as the reviewable exemption.

#![allow(clippy::disallowed_methods)]

mod accumulate;
mod context;
mod digest;
mod engine;
mod hook;
mod limits;
pub mod monitor;
mod permission;
mod prompt;
mod recorder;
mod session;
mod skill_gate;
mod snapshot;
mod ui_event;

pub use context::{CompactionPolicy, ContextManager, Oversized};
pub use digest::{prompt_digest, tools_digest};
pub use engine::{
    DelegatedRun, Engine, EngineBuilder, EngineError, GoalSummary, Stop, TurnSummary,
};
pub use hook::{HookEvent, HookSet, HookVerdict};
pub use limits::{LimitKind, Limits, Spend};
pub use monitor::Violation;
pub use prompt::{CHEAP_MODEL_SYSTEM_PROMPT, SYSTEM_PROMPT};
// From `tools`, whose shared position is what lets one value name an experiment
// across the engine's gates and the dispatcher's tool set at once.
pub use recorder::Recorder;
pub use session::ResumeError;
pub use skill_gate::SkillGate;
pub use snapshot::{Restored, Snapshots};
pub use tools::{Mechanism, Mechanisms};
pub use ui_event::{Observer, UiEvent};

// The facade: an explicit re-export list, never `pub use *`. Frontends depend on
// `engine` only, and what crosses this line is an API decision. Nothing
// internal to `sandbox` is here.
pub use policy::{Effect, ListenRule, Policy, PolicyError};
pub use sandbox::{
    Broker, BrokerBuilder, Grant, Harness, NoneBackend, PermissionRecord, Prompter, SandboxBackend,
    SnapshotSink, Unenforceable, for_platform,
};
