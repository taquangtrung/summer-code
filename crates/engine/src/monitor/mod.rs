//! Runtime properties over the journal.
//!
//! This module consumes journal records and policies and nothing else, which is
//! why one implementation serves both paths: the engine feeds records as it
//! writes them, and [`Monitor::check_journal`] replays a finished file after
//! the fact. Phase 9 runs one per tenant.
//!
//! It reads nothing from the rest of `engine`, so an auditor that only wants to
//! check a finished journal against a policy can use `engine::monitor` without
//! touching the loop.
//!
//! **What these properties claim, and what they do not.** They are about the
//! check being reached and answered consistently with the declared policy, not
//! about the policy being a good one: a permissive policy satisfies all of them.
//! They are also not a substitute for the typestate, which is the actual
//! enforcement. A monitor reads the journal, so it cannot see an access that
//! never reached the broker, and it cannot see the OS failing to enforce what
//! the broker decided.

mod check;
mod properties;
mod property;

#[cfg(test)]
mod testing;

pub use check::Monitor;
pub use properties::{GrantOrder, InwardResolution, OutsideRoot, UnauditedSuccess};
pub use property::{Property, Violation};
