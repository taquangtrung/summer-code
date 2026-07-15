//! Capability policy as data.
//!
//! A contract crate: serde plus `globset`, no async runtime, no I/O. It is split
//! out because the runtime monitor (Phase 2) and outside readers consume journals
//! and policies without linking the engine or the sandbox.

mod decision;
mod evaluator;
mod file;
mod policy;
mod request;
mod scope;
mod shell_line;
mod source;

#[cfg(test)]
mod testing;

pub use decision::Decision;
pub use evaluator::{PolicyEvaluator, RulesEvaluator, normalize, sole_unlisted_command};
pub use file::{
    CommandEntry, HostEntry, ListenEntry, ListenMode, NetworkEntry, PathEntry, PolicyFile,
};
pub use policy::{
    CommandRule, Effect, HostRule, ListenRule, NetworkRule, PathAccess, PathRule, Policy,
    PolicyError,
};
pub use request::Request;
pub use scope::{RuleSpec, Scope};
pub use shell_line::commands_of;
pub use source::{Source, merge};
