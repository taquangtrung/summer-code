//! The append-only record tree for a session.
//!
//! The journal is the source of truth: session state is a fold over a walk of
//! it, not a separate store. This crate owns the schema ([`Record`]), the JSONL
//! codec (`codec`), the tree traversal with compaction-aware
//! drop application ([`Walk`]), and the [`JournalSink`] seam where a real file
//! or socket appears.
//!
//! It is a contract crate: serde plus `ulid` only, no async runtime, no I/O.
//! It serializes to and from `impl Read` / `impl Write` and never opens a file.
//!
//! **The unit is a [`Record`], not an event, because three other things in this
//! workspace are events and none of them is durable.** `engine::UiEvent` is a
//! frame for the screen, `provider::StreamEvent` is a chunk off the wire, and
//! crossterm's `Event` is a keystroke: all three are in flight and gone, while
//! this one is the thing a later session reads back. Leaving the bare word to
//! the transient meanings and naming the durable one for what it is means no
//! file has to hold two `Event`s and no reader has to work out which is meant.
//!
//! The cost of the name is one adjacency worth knowing about:
//! `sandbox::PermissionRecord` is the broker's decision, and it *becomes* a
//! `Record` here with `RecordKind::Permission`. They meet in exactly one place,
//! `engine::Engine::drain_permissions`, where the broker's side is named
//! `decided` so that `record` keeps one meaning.

mod codec;
mod error;
mod id;
mod record;
mod sink;
/// `SystemTime` as RFC 3339 millis UTC, for `#[serde(with = ...)]`.
///
/// Public because the wire discipline it enforces is the crate's, not this
/// module's: the snapshot manifest is a sibling append-only JSONL file written
/// by `engine`, and letting it fall back to serde's default `SystemTime` shape
/// would put two different time formats in one session's records.
pub mod timestamp;
mod version;
mod walk;

pub use codec::{read_records, write_record, write_records};
pub use error::JournalError;
pub use id::{CallId, RecordId, SessionId};
pub use record::{
    Access, AccessConnect, AccessExec, AccessListen, Block, BlockThinking, BlockToolUse, Compacted,
    Decision, Delegated, Folded, ModelTurn, Outcome, Permission, ProviderFailed, Record,
    RecordKind, Scope, SessionStarted, StopReason, ToolCall, ToolResult, Usage,
};
pub use sink::{JournalSink, WriterSink};
pub use version::{SCHEMA_VERSION, check_version};
pub use walk::{Journal, Walk};

/// Result alias for fallible journal operations.
pub type Result<T> = std::result::Result<T, JournalError>;
