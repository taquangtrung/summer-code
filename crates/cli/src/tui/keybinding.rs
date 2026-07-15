//! The keybinding reference table behind `/keymap`.
//!
//! A static table and its formatter, kept beside the editor rather than inside
//! it: what a key *does* is [`InputEditor`](crate::tui::input::InputEditor)'s,
//! and what the help *says* it does is this table's. They are two things that
//! have to agree, and a reader checking whether they do should not have to page
//! through a thousand lines of editing logic to find the claim.

// ========================================================================
// Constants
// ========================================================================

/// The keybindings [`keybindings_help_text`] shows. Curated, not exhaustive --
/// the full Vim motion table and every Alt-word chord live as doc comments
/// near their own handlers -- this is the cheat sheet a person actually wants
/// on first glance, the scope Claude Code's own `?` panel keeps to.
const KEYBINDINGS: &[KeyBindingSpec] = &[
    KeyBindingSpec {
        keys: "Enter",
        description: "submit; during a turn, queue for when it ends",
    },
    KeyBindingSpec {
        keys: "Alt+Enter / Ctrl+J / \\ then Enter",
        description: "insert a newline",
    },
    KeyBindingSpec {
        keys: "Esc",
        description: "interrupt the turn (closes an open menu first)",
    },
    KeyBindingSpec {
        keys: "Esc Esc",
        description: "rewind to an earlier message and edit it",
    },
    KeyBindingSpec {
        keys: "Ctrl+C",
        description: "interrupt; clear the line (Ctrl+Y restores); twice to exit",
    },
    KeyBindingSpec {
        keys: "1 / 2 / 3",
        description: "answer a permission prompt",
    },
    KeyBindingSpec {
        keys: "Ctrl+D",
        description: "exit, on an empty line between turns",
    },
    KeyBindingSpec {
        keys: "Shift+Tab",
        description: "cycle mode: auto / interactive / plan",
    },
    KeyBindingSpec {
        keys: "Ctrl+G",
        description: "write the prompt in $VISUAL / $EDITOR",
    },
    KeyBindingSpec {
        keys: "Ctrl+O",
        description: "toggle the detail pager",
    },
    KeyBindingSpec {
        keys: "PageUp / PageDown",
        description: "scroll the transcript",
    },
    KeyBindingSpec {
        keys: "Up / Down",
        description: "recall input history",
    },
    KeyBindingSpec {
        keys: "!command",
        description: "run a shell command; its output goes with your next message",
    },
    KeyBindingSpec {
        keys: "Ctrl+R",
        description: "search input history; again for an older match",
    },
    KeyBindingSpec {
        keys: "Tab",
        description: "accept a completion",
    },
    KeyBindingSpec {
        keys: "Ctrl+A/E, Ctrl+K/U/W, Ctrl+Y",
        description: "readline line editing",
    },
    KeyBindingSpec {
        keys: "/keymap vim",
        description: "switch to Vim modal editing",
    },
    KeyBindingSpec {
        keys: "?",
        description: "this list, on an empty line",
    },
];

// ========================================================================
// Data Structures
// ========================================================================

/// One entry in [`KEYBINDINGS`]: the chord as shown to the user, and what it does.
struct KeyBindingSpec {
    keys: &'static str,
    description: &'static str,
}

// ========================================================================
// Functions
// ========================================================================

/// The keybindings cheat sheet a bare `?` shows on an empty line, distinct
/// from [`crate::tui::slash::help_text`]'s slash-command list: Claude Code's `?`
/// shows
/// shortcuts, not commands.
pub fn keybindings_help_text() -> String {
    let mut out = String::from("keybindings:");
    for spec in KEYBINDINGS {
        out.push_str(&format!("\n  {:<30}  {}", spec.keys, spec.description));
    }
    out
}
