//! Tools: the `Tool` trait, validation, dispatch, and the built-in set.

mod builtin;
mod command;
mod dispatch;
mod glob_tool;
mod graph_tool;
pub mod mcp;
mod mechanisms;
mod pattern_tool;
mod plan;
mod shell;
mod skill;
mod source;
mod subagent;
mod subtree_rules;
mod tool;
mod validate;
mod vcs;
mod verifier;
mod web;

pub use builtin::{Edit, GitTool, Grep, Read, Shell, hunk_diff};
pub use dispatch::{Dispatcher, DuplicateTool};
pub use glob_tool::GlobTool;
pub use graph_tool::GraphTool;
pub use mechanisms::{Mechanism, Mechanisms};
pub use pattern_tool::PatternTool;
pub use plan::PlanTool;
pub use shell::ShellSpec;
pub use skill::{Skill, SkillLookup};
pub use source::{Builtin, ToolSource};
pub use subagent::{SubagentRunner, SubagentTool};
pub use tool::{Concurrency, Tool, ToolSpec};
pub use vcs::{Git, Vcs};
pub use verifier::{DiagnosticSummary, verify_code_edits};
pub use web::{SearchConfig, WebFetch, WebSearch};
