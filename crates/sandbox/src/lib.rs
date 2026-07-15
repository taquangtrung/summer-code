//! The sandbox: the only crate that touches fs/process/net for agent-driven
//! work.
//!
//! **The typestate is the enforcement; the lint is defense in depth.** These are
//! not two checks of equal standing. A tool cannot perform agent-driven I/O
//! because it cannot construct an [`AuthorizedRead`]: the fields are private,
//! there is no public constructor, and the only source is a [`Broker`] method
//! that ran the policy evaluation first. The compiler checks that on every
//! build, it needs no configuration, and it cannot be forgotten. The CI lint
//! exists only to stop someone bypassing this crate altogether by reaching for
//! `std::fs` directly.
//!
//! `sandbox` is that lint's subject: the one crate that legitimately names
//! `std::fs`, `std::process`, and sockets. The crate-level
//! `#![allow(clippy::disallowed_methods)]` is the reviewable exemption that
//! admits it; every crate outside the named harness set gets none.
//!
//! **The decision itself is the user's, so every layer of it is extensible.**
//! Policies are data, so a permissive policy is a configuration and not a bypass:
//! "every access was checked" stays true. [`Prompter`] lets the answer come from
//! a terminal, a daemon's client, or a test. [`SandboxBackend`] covers both OS
//! mechanisms and stronger isolation.

#![allow(clippy::disallowed_methods)]
// The other half of that exemption. `unsafe_code` is denied workspace-wide; the
// two sites here are the Landlock ABI probe (the landlock crate keeps runtime
// ABI detection private, and `capabilities()` has to answer before `enter`) and
// the `openat2` syscall that makes `RESOLVE_BENEATH` available at all. Both are
// the reason this crate exists, and both carry their own `SAFETY` comment.
#![allow(unsafe_code)]

mod backend;
mod broker;
mod callsite;
mod canon;
mod ctx;
mod handle;
#[cfg(target_os = "linux")]
mod openat;
mod prompt;
mod remembered;
mod snapshot;

pub use backend::{
    Capabilities, Enforcement, Gap, Harness, NoneBackend, SandboxBackend, for_platform,
};
pub use broker::{
    Broker, BrokerBuilder, DecisionSource, Denied, OpenError, PermissionRecord, Supplied,
    Unenforceable, resolve,
};
pub use callsite::{current_call, tag_call};
pub use canon::canonicalize;
pub use ctx::ToolCx;
pub use handle::{
    AuthorizedAddrs, AuthorizedCommand, AuthorizedDir, AuthorizedListener, AuthorizedRead,
    AuthorizedService, AuthorizedSocket, AuthorizedWrite, CapturedStream, CommandOutcome,
    CommandOutput,
};
pub use prompt::{AutoDeny, Grant, Prompter, WritePreview};
pub use remembered::Remembrance;
pub use snapshot::{NoSnapshots, SnapshotSink};

#[cfg(target_os = "linux")]
pub use backend::LandlockBackend;

#[cfg(target_os = "macos")]
pub use backend::SeatbeltBackend;
