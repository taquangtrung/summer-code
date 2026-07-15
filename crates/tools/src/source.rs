//! A `ToolSource`: where a set of tools comes from. Built-ins and (Phase 6) MCP
//! servers both implement this, so an MCP tool inherits validation, dispatch,
//! and the broker with no new plumbing.

use std::sync::Arc;

use crate::mechanisms::Mechanisms;
use crate::shell::ShellSpec;
use crate::tool::Tool;

// ========================================================================
// Traits
// ========================================================================

pub trait ToolSource: Send + Sync {
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

// ========================================================================
// Data Structures
// ========================================================================

/// The built-in tool set.
pub struct Builtin {
    tools: Vec<Arc<dyn Tool>>,
}

// ========================================================================
// Builtin Implementations
// ========================================================================

impl Builtin {
    pub fn new() -> Self {
        Self::with_shell(ShellSpec::platform_default())
    }

    /// The built-in set, running command lines through `shell`.
    ///
    /// The shell is a construction parameter rather than something the `shell`
    /// tool works out for itself, because choosing it needs to look at the
    /// user's config and at `PATH`, and this crate touches neither the
    /// filesystem nor the config. The frontend decides; the tool obeys.
    pub fn with_shell(shell: ShellSpec) -> Self {
        Self::with_mechanisms(Mechanisms::default(), shell)
    }

    /// The built-in set under `mechanisms`, which today means the `read` tool
    /// with or without its skeleton mode.
    ///
    /// The engine takes the same [`Mechanisms`] value for its own gates, so a
    /// caller names the whole experiment once and hands the same value to both:
    /// an ablation arm that turned the gate off in the engine but left the tool
    /// advertising the mode would be two different setups sharing a label.
    pub fn with_mechanisms(mechanisms: Mechanisms, shell: ShellSpec) -> Self {
        Self {
            tools: vec![
                Arc::new(crate::builtin::Read::with_skeleton(
                    mechanisms.skeleton_reads,
                )),
                Arc::new(crate::builtin::Edit::new()),
                Arc::new(crate::builtin::Grep::new()),
                Arc::new(crate::GlobTool::new()),
                Arc::new(crate::builtin::Shell::with_shell(shell)),
                Arc::new(crate::builtin::GitTool::new(Arc::new(crate::Git::new()))),
                Arc::new(crate::builtin::Symbols::new()),
                Arc::new(crate::GraphTool::new()),
                Arc::new(crate::PatternTool::new()),
                Arc::new(crate::PlanTool::new()),
                Arc::new(crate::WebFetch::new()),
                Arc::new(crate::WebSearch::new()),
            ],
        }
    }

    /// Add the `skill` tool, backed by a name → body lookup. Add it only when
    /// the frontend has skills; an empty catalog makes the tool useless.
    pub fn with_skill(mut self, skills: Arc<dyn crate::skill::SkillLookup>) -> Self {
        self.tools.push(Arc::new(crate::skill::Skill::new(skills)));
        self
    }

    /// Add the `subagent` tool, backed by something that can actually run a
    /// sub-session.
    ///
    /// **Not in the default set, and that is the whole design.** Running a
    /// sub-session needs an engine, which no caller of this crate is obliged to
    /// have. A frontend that cannot supply one leaves the tool out, and the
    /// model is offered a tool set without it. The alternative, shipping a
    /// version that answers plausibly without running anything, is what this
    /// tool used to do.
    pub fn with_subagent(mut self, runner: Arc<dyn crate::subagent::SubagentRunner>) -> Self {
        self.tools
            .push(Arc::new(crate::subagent::SubagentTool::new(runner)));
        self
    }
}

impl Default for Builtin {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSource for Builtin {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_loads_its_checked_in_schema_under_its_own_name() {
        // Two failure modes at once, both otherwise silent. A `tools/*.json`
        // that does not parse panics inside `ToolSpec::load`, which is why the
        // artifacts get a test at all; and a `name` typo in one registers the
        // tool under a name the model is never told about, so it is simply never
        // called and nothing anywhere reports a reason.
        let tools = Builtin::new().tools();
        let names: Vec<&str> = tools.iter().map(|tool| tool.spec().name.as_str()).collect();
        assert_eq!(
            names,
            [
                "read",
                "edit",
                "grep",
                "glob",
                "shell",
                "git",
                "symbols",
                "graph",
                "pattern",
                "plan",
                "web_fetch",
                "web_search"
            ]
        );
    }

    #[test]
    fn the_subagent_tool_is_offered_only_when_something_can_run_one() {
        // The rule this pins: a tool the model is told about must be able to do
        // what its description says. `subagent` needs an engine to run a
        // sub-session, so a frontend without one offers a tool set without it
        // rather than a tool that answers from nothing.
        struct NoRunner;

        #[async_trait::async_trait]
        impl crate::subagent::SubagentRunner for NoRunner {
            async fn run(&self, _prompt: &str, _role: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }

        let bare = Builtin::new().tools();
        assert!(!bare.iter().any(|tool| tool.spec().name == "subagent"));

        let wired = Builtin::new().with_subagent(Arc::new(NoRunner)).tools();
        assert!(wired.iter().any(|tool| tool.spec().name == "subagent"));
    }

    #[test]
    fn the_shell_description_names_the_real_shell() {
        // `shell.json` is the one schema with placeholders, so it is the one
        // that can reach the model unfilled. A description reading "write the
        // command in {program} syntax" tells the model nothing about which
        // syntax to write, and every command line it guesses fails for a reason
        // the error cannot express.
        let shell = ShellSpec::platform_default();
        let tools = Builtin::with_shell(shell.clone()).tools();
        let description = &tools
            .iter()
            .find(|tool| tool.spec().name == "shell")
            .expect("the built-in set has a shell tool")
            .spec()
            .description;

        assert!(description.contains(&shell.program), "{description}");
        assert!(!description.contains('{'), "a placeholder survived");
    }

    #[test]
    fn the_read_description_advertises_skeleton_only_while_it_runs() {
        // The same reachability as the shell test, plus the ablation claim: a
        // description that still advertises skeleton mode while the tool reads
        // plain text is a model guided into a mode that no longer does what it
        // says, and an ablation arm whose tool set quietly differs from its
        // label.
        let on =
            Builtin::with_mechanisms(Mechanisms::default(), ShellSpec::platform_default()).tools();
        let on = &on
            .iter()
            .find(|tool| tool.spec().name == "read")
            .expect("the built-in set has a read tool")
            .spec()
            .description;
        assert!(on.contains("skeleton"), "{on}");
        assert!(!on.contains('{'), "a placeholder survived");

        let off = Builtin::with_mechanisms(
            Mechanisms {
                skeleton_reads: false,
                ..Mechanisms::default()
            },
            ShellSpec::platform_default(),
        )
        .tools();
        let off = &off
            .iter()
            .find(|tool| tool.spec().name == "read")
            .expect("the built-in set has a read tool")
            .spec()
            .description;
        assert!(!off.contains("skeleton"), "{off}");
        assert!(!off.contains('{'), "a placeholder survived");
    }
}
