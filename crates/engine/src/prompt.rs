//! The agent's default system prompt.
//!
//! **It is a file, not a string literal, because it is a tuning surface.** The
//! wording is what the model routes on, and `bench` records a
//! `prompt_digest` alongside every score precisely so a rate can say which
//! prompt produced it. Checked in, a prompt change is a diff anyone can read;
//! inline, it is buried in a code change and the digest moves for a reason no
//! reviewer can recover.
//!
//! `include_str!` rather than a runtime read, because the frontend enters the
//! sandbox before it builds an engine: a read here would happen after the root
//! is closed, and fail for a path the user never chose to deny.

// ========================================================================
// Constants
// ========================================================================

/// The default system prompt.
///
/// Frontends may append to it (the CLI adds its skills catalog) but the base
/// text is one artifact with one digest, so the CLI and the bench agree by
/// construction rather than by two copies staying in step.
pub const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

/// System prompt tailored for cheap models (Gemini Flash, Haiku, etc.).
pub const CHEAP_MODEL_SYSTEM_PROMPT: &str = include_str!("../prompts/cheap_model_system.md");

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use tools::{Builtin, ToolSource};

    use super::*;

    #[test]
    fn the_prompt_names_every_built_in_tool() {
        for tool in Builtin::new().tools() {
            let name = &tool.spec().name;
            assert!(
                SYSTEM_PROMPT.contains(&format!("`{name}`")),
                "the prompt never names the `{name}` tool"
            );
            assert!(
                CHEAP_MODEL_SYSTEM_PROMPT.contains(&format!("`{name}`")),
                "the cheap model prompt never names the `{name}` tool"
            );
        }
    }
}
