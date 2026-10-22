//! The slash-command registry, and the completions it drives.
//!
//! **One registry, three consumers, so they cannot drift.** [`COMMANDS`] is the
//! single source for parsing a typed line, for the completion menu, and for
//! `/help`. A command added to the array appears in all three; a command added to
//! the parser alone used to appear in none of the others.
//!
//! Separate from the line editor because none of this is editing. The editor
//! owns a buffer and a cursor and knows nothing about what a `/` means; this
//! module turns text into a [`Command`] and offers rows to complete it with. The
//! `@file` completions live here for the same reason: they answer "what could
//! this partial word become", which is the menu's question, not the buffer's.

use std::borrow::Cow;

use provider::PROFILES;

use crate::tui::file_index;
use crate::tui::input::CompletionRow;

// ========================================================================
// Constants
// ========================================================================

/// The canonical command registry. Order is the menu and `/help` order; aliases
/// (`?`, `exit`, `q`, `usage`, `permissions`) still parse but are not listed.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: Command::Help,
        name: "help",
        description: "show this command list",
    },
    CommandSpec {
        command: Command::Status,
        name: "status",
        description: "show session status (model, sandbox, session file)",
    },
    CommandSpec {
        command: Command::Policy,
        name: "policy",
        description: "show the effective policy",
    },
    CommandSpec {
        command: Command::Sessions,
        name: "sessions",
        description: "list past sessions",
    },
    CommandSpec {
        command: Command::Resume,
        name: "resume",
        description: "resume a past session: /resume <session-id>",
    },
    CommandSpec {
        command: Command::Login,
        name: "login",
        description: "log in to a provider: /login <provider> [api-key]",
    },
    CommandSpec {
        command: Command::Model,
        name: "model",
        description: "pick a provider and model",
    },
    CommandSpec {
        command: Command::Mode,
        name: "mode",
        description: "switch execution mode: /mode <auto|interactive|plan> (or Shift+Tab)",
    },
    CommandSpec {
        command: Command::Thinking,
        name: "thinking",
        description: "toggle expanded thinking blocks",
    },
    CommandSpec {
        command: Command::Effort,
        name: "effort",
        description: "set reasoning effort: /effort <low|medium|high|xhigh|max|off>",
    },
    CommandSpec {
        command: Command::Tools,
        name: "tools",
        description: "toggle expanded tool output",
    },
    CommandSpec {
        command: Command::Diff,
        name: "diff",
        description: "show uncommitted code diffs in workspace",
    },
    CommandSpec {
        command: Command::Compact,
        name: "compact",
        description: "drop older turns from the context window, keeping a summary",
    },
    CommandSpec {
        command: Command::Keymap,
        name: "keymap",
        description: "set input editor keymap: /keymap <emacs|vim>",
    },
    CommandSpec {
        command: Command::Rewind,
        name: "rewind",
        description: "undo the agent's file changes from the last n turns (default 1)",
    },
    CommandSpec {
        command: Command::Undo,
        name: "undo",
        description: "revert workspace files to state before last turn",
    },
    CommandSpec {
        command: Command::Checkpoint,
        name: "checkpoint",
        description: "save a manual checkpoint of workspace files",
    },
    CommandSpec {
        command: Command::Usage,
        name: "usage",
        description: "show token usage dashboard",
    },
    CommandSpec {
        command: Command::Agent,
        name: "agent",
        description: "delegate a sub-task to a subagent: /agent <task>",
    },
    CommandSpec {
        command: Command::Review,
        name: "review",
        description: "run automated pre-commit code review on workspace diff",
    },
    CommandSpec {
        command: Command::Statusline,
        name: "statusline",
        description: "configure statusline format template: /statusline <template|reset>",
    },
    CommandSpec {
        command: Command::Copy,
        name: "copy",
        description: "copy the last reply to the clipboard: /copy [code]",
    },
    CommandSpec {
        command: Command::Export,
        name: "export",
        description: "write the transcript to a Markdown file: /export [path]",
    },
    CommandSpec {
        command: Command::Clear,
        name: "clear",
        description: "clear the transcript",
    },
    CommandSpec {
        command: Command::Quit,
        name: "quit",
        description: "exit the session (also Ctrl-D)",
    },
];

// ========================================================================
// Data Structures
// ========================================================================

/// The fixed slash-command set, parsed at the input edge and never sent to the
/// model. Unknown `/x` is an inline error, not a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    Policy,
    Sessions,
    /// `/resume <session-id>`: the id is parsed from the raw line, not carried
    /// on this variant, since [`parse_command`] only classifies the name.
    Resume,
    /// `/login <provider> [api-key]`: arguments are parsed from the raw
    /// line, not carried on this variant, since [`parse_command`] only
    /// classifies the name.
    Login,
    Model,
    /// `/mode [auto|interactive|plan]`: switch execution mode.
    Mode,
    /// Toggle whether thinking blocks render expanded.
    Thinking,
    /// `/effort [level]`: set the reasoning effort for the current model.
    Effort,
    /// Toggle whether tool bodies render expanded.
    ///
    /// **Without this the bodies were unreachable.** `ToolView.collapsed` was set
    /// at construction and nothing ever cleared it, so the `!collapsed` branch in
    /// `tool_text` was dead code in the running app: tool output, `Denied`
    /// reasons and error messages never rendered, and the user saw
    /// `✗ shell <args> denied` with no way to find out why.
    Tools,
    Diff,
    Compact,
    /// `/keymap <emacs|vim>`: switch input editor keymap mode.
    Keymap,
    /// `/rewind [n]`: restore the workspace to before the last `n` turns.
    Rewind,
    Undo,
    Checkpoint,
    Usage,
    Agent,
    Review,
    Statusline,
    /// `/copy [code]`: the last reply, or its last code block, to the clipboard.
    Copy,
    /// `/export [path]`: the transcript as a Markdown file.
    Export,
    Clear,
    Quit,
}

/// One slash command's [`Command`], canonical name, and a one-line description.
/// The registry is the single source for parsing, completion, and `/help`, so the
/// three never drift.
pub struct CommandSpec {
    pub command: Command,
    pub name: &'static str,
    pub description: &'static str,
}

// ========================================================================
// Functions
// ========================================================================

/// Parse a submitted line as a command.
///
/// Returns `None` for a line that is not a command (no leading `/` or `?`); `Ok` for a
/// recognized command; `Err(name)` for an unrecognized `/x` or `?x`, so the driver can
/// surface it inline rather than sending it to the model.
pub fn parse_command(line: &str) -> Option<Result<Command, String>> {
    let trimmed = line.trim();
    let (prefix, rest) = if let Some(rest) = trimmed.strip_prefix('/') {
        ('/', rest)
    } else if let Some(rest) = trimmed.strip_prefix('?') {
        ('?', rest)
    } else {
        return None;
    };

    // A bare `?` is the help command.
    if prefix == '?' && rest.trim().is_empty() {
        return Some(Ok(Command::Help));
    }

    // A bare `/` is not a command.
    let name = rest.split_whitespace().next()?;
    let command = match name {
        // Aliases that are not in the canonical registry.
        "?" => Command::Help,
        "exit" | "q" => Command::Quit,
        "permissions" => Command::Policy,
        other => match COMMANDS.iter().find(|spec| spec.name == other) {
            Some(spec) => spec.command,
            None => return Some(Err(format!("{prefix}{name}"))),
        },
    };
    Some(Ok(command))
}

/// The `/` or `?` prefix being completed, or `None` when the buffer is not a command
/// prefix. Once a space appears the command name is finished and the menu closes.
pub fn command_prefix(buffer: &str) -> Option<&str> {
    let rest = buffer
        .strip_prefix('/')
        .or_else(|| buffer.strip_prefix('?'))?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Commands whose canonical name starts with `prefix`. Empty prefix yields the
/// whole set.
pub fn command_completions(prefix: &str) -> Vec<CompletionRow> {
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(prefix))
        .map(|spec| CompletionRow {
            name: Cow::Borrowed(spec.name),
            description: Cow::Borrowed(spec.description),
        })
        .collect()
}

/// Whether `name` is a command whose name alone is not a runnable line: it
/// needs an argument beyond the name, the same set [`Command::Resume`] and
/// [`Command::Login`] carry no data for.
pub(super) fn command_takes_arguments(name: &str) -> bool {
    matches!(
        COMMANDS
            .iter()
            .find(|spec| spec.name == name)
            .map(|spec| spec.command),
        Some(Command::Resume) | Some(Command::Login)
    )
}

/// The partial provider id being typed as `/login`'s first argument, or
/// `None` when the buffer is not in that position: not a `/login` line, or
/// past the argument already (a second space appeared).
pub(super) fn login_provider_prefix(buffer: &str) -> Option<&str> {
    let rest = buffer.strip_prefix("/login")?.strip_prefix(' ')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Provider ids whose canonical name starts with `prefix`, read straight from
/// [`PROFILES`] so a new registry entry autocompletes without this list
/// needing to be kept in sync by hand.
pub(super) fn provider_completions(prefix: &str) -> Vec<CompletionRow> {
    PROFILES
        .iter()
        .filter(|profile| profile.id.as_str().starts_with(prefix))
        .map(|profile| CompletionRow {
            name: Cow::Borrowed(profile.id.as_str()),
            description: Cow::Borrowed(profile.label),
        })
        .collect()
}

/// The `@file` prefix being typed in the buffer, or `None` if not in `@file` position.
pub(super) fn at_file_prefix(buffer: &str, cursor: usize) -> Option<&str> {
    let before_cursor = &buffer[..cursor];
    let at_idx = before_cursor.rfind('@')?;
    let slice = &before_cursor[at_idx + 1..];
    if slice.contains(' ') || slice.contains('\n') {
        return None;
    }
    Some(slice)
}

/// The files and directories `@prefix` completes to, best match first; see
/// [`file_index`].
pub(super) fn file_completions(prefix: &str) -> Vec<CompletionRow> {
    file_index::matches(prefix)
        .into_iter()
        .map(|entry| CompletionRow {
            name: Cow::Owned(entry.path),
            description: Cow::Borrowed(if entry.is_dir { "dir" } else { "file" }),
        })
        .collect()
}

/// The `/help` text, generated from [`COMMANDS`] so it never drifts from the
/// registry.
pub fn help_text() -> String {
    let mut out = String::from("commands:");
    for spec in COMMANDS {
        out.push_str(&format!("\n  /{:<8}  {}", spec.name, spec.description));
    }
    out
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_commands_parse_at_the_input_edge() {
        assert_eq!(parse_command("/help"), Some(Ok(Command::Help)));
        assert_eq!(parse_command("  /quit  "), Some(Ok(Command::Quit)));
        assert_eq!(parse_command("/model extra"), Some(Ok(Command::Model)));
        assert_eq!(
            parse_command("hello"),
            None,
            "no leading slash is not a command"
        );
        assert_eq!(parse_command("/"), None, "a bare slash is not a command");
        match parse_command("/nope") {
            Some(Err(name)) => assert_eq!(name, "/nope"),
            other => panic!("unknown command is an inline error, got {other:?}"),
        }
    }

    #[test]
    fn new_commands_and_aliases_parse() {
        assert_eq!(parse_command("/status"), Some(Ok(Command::Status)));
        assert_eq!(parse_command("/usage"), Some(Ok(Command::Usage)));
        assert_eq!(parse_command("/policy"), Some(Ok(Command::Policy)));
        assert_eq!(
            parse_command("/permissions"),
            Some(Ok(Command::Policy)),
            "permissions aliases policy"
        );
        assert_eq!(parse_command("/sessions"), Some(Ok(Command::Sessions)));
    }

    #[test]
    fn argument_taking_commands_still_go_through_the_shared_registry() {
        // `/login` and `/resume` are dispatched by hand at the driver edge
        // (they need arguments `Command` does not carry), which once meant
        // they were missing from `COMMANDS` entirely: invisible to both Tab
        // completion and `/help`. Registering them fixes that only if
        // `parse_command` and the completion menu both still key off the one
        // registry, which is what this pins down.
        assert_eq!(
            parse_command("/login anthropic sk-test"),
            Some(Ok(Command::Login))
        );
        assert_eq!(parse_command("/resume abc123"), Some(Ok(Command::Resume)));
        let comps = command_completions("");
        let names: Vec<&str> = comps.iter().map(|spec| spec.name.as_ref()).collect();
        assert!(names.contains(&"login"), "{names:?}");
        assert!(names.contains(&"resume"), "{names:?}");
        assert!(help_text().contains("login"));
        assert!(help_text().contains("resume"));
    }

    #[test]
    fn command_completions_filter_by_prefix() {
        assert_eq!(command_completions("").len(), COMMANDS.len());
        let help = command_completions("h");
        assert_eq!(help.len(), 1);
        assert_eq!(help[0].name, "help");
        assert!(command_completions("xyz").is_empty());
    }

    #[test]
    fn command_prefix_closes_once_a_space_is_typed() {
        assert_eq!(command_prefix("/he"), Some("he"));
        assert_eq!(command_prefix("/"), Some(""));
        assert_eq!(command_prefix("hello"), None);
        assert_eq!(
            command_prefix("/help x"),
            None,
            "a space finishes the command name"
        );
    }

    #[test]
    fn question_mark_prefixed_forms_still_parse_as_help() {
        // `parse_command` and `command_prefix` are pure string parsers, used
        // for a buffer built some other way (a paste, a resumed line) than the
        // live `?` keybinding above, which no longer builds one by typing.
        assert_eq!(parse_command("?"), Some(Ok(Command::Help)));
        assert_eq!(parse_command("  ?  "), Some(Ok(Command::Help)));
        assert_eq!(parse_command("?help"), Some(Ok(Command::Help)));
        assert_eq!(parse_command("?status"), Some(Ok(Command::Status)));
        assert_eq!(command_prefix("?"), Some(""));
        assert_eq!(command_prefix("?he"), Some("he"));
    }
}
