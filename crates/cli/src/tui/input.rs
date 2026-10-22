//! The input editor and the slash-command set.
//!
//! A pure unit: keystrokes in, buffer and cursor out. The driver reads crossterm
//! key events and feeds them here; the terminal never touches this code, which is
//! what makes the cursor, edit, and command-completion behavior unit-testable.
//!
//! While the buffer is a `/`-prefixed command prefix, or `/login`'s first
//! argument, a completion menu is live: Up/Down move the selection (history
//! recall waits until it closes), Tab fills the highlighted row, and Enter
//! fills it and submits -- unless the row still leaves the line incomplete
//! (`/login` or `/resume`, which need an argument `Command` does not carry),
//! in which case Enter fills it, adds a trailing space, and keeps editing
//! instead. A provider id is a complete line: submitting `/login <provider>`
//! opens the masked key entry downstream, so Enter on a provider row
//! submits rather than waiting for a clear-text token. The menu is a pure
//! projection of the buffer over [`crate::tui::slash::COMMANDS`] or the provider
//! registry, so
//! the view renders it without touching the editor.
//!
//! **Multi-line, with three ways in, because no single one works everywhere.**
//! Alt-Enter and Ctrl-J insert a newline on any terminal; Shift-Enter does too,
//! but only where the terminal reports the modifier at all (crossterm cannot
//! invent it), so a trailing `\` before Enter is the fallback that needs no
//! terminal support. Plain Enter still submits, which is the key people press
//! most and the one that must not change meaning.
//!
//! The fourth way in is a paste, and it is the one that made this necessary: a
//! bracketed paste arrives as [`InputEditor::insert_str`] with its newlines
//! intact. Without bracketed paste enabled, the same paste arrives as a stream
//! of key events whose Enters each submit a turn, so pasting a five-line stack
//! trace started five turns.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::slash::{
    at_file_prefix, command_completions, command_prefix, command_takes_arguments, file_completions,
    login_provider_prefix, provider_completions,
};

// ========================================================================
// Constants
// ========================================================================

/// A trailing character that turns the next Enter into a newline.
///
/// The shell continuation, chosen because it is the one people already reach
/// for, and because it travels through terminals that report no modifier on
/// Enter at all.
const CONTINUATION: char = '\\';

/// Commands that throw away work, which the completion menu will fill in but
/// never run on the Enter that completed them.
const DISCARDING_COMMANDS: &[&str] = &["clear", "rewind", "undo"];

/// How long after a first Ctrl-C on an empty line a second one exits. Two
/// presses, as in Claude Code: one press was a reflex that ended the session.
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

// ========================================================================
// Data Structures
// ========================================================================

/// The editor mode: Emacs/Readline (default) or Vim modal editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditMode {
    /// Emacs / Readline editing (default): standard typing inserts text, standard readline chords active.
    #[default]
    Emacs,
    /// Vim Normal mode: motions, deletions, yanking, mode transitions.
    VimNormal,
    /// Vim Insert mode: typing inserts text, Esc transitions to Normal mode.
    VimInsert,
}

/// The text under edit, with a cursor. May hold several lines.
pub struct InputEditor {
    buffer: String,
    /// Byte offset of the cursor. Always on a char boundary and within
    /// `[0, buffer.len()]`.
    cursor: usize,
    /// Whether Esc closed the menu the buffer would otherwise still show.
    /// Cleared on the next edit, so dismissing is per-suggestion, not
    /// per-session -- typing further can open a new one.
    dismissed: bool,
    /// Submitted lines, oldest first. Recalled with Up/Down when no command menu
    /// is open.
    history: Vec<String>,
    /// The history slot being edited, or `None` when composing a fresh line.
    /// `Some(0)` is the most recent submission.
    recall: Option<usize>,
    /// The line being composed when history recall began, restored on coming
    /// back down past the newest entry. One Up used to discard it.
    draft: String,
    /// When an empty-line Ctrl-C asked for a second press to exit.
    exit_armed_at: Option<Instant>,
    /// The highlighted row in the live command menu. Reset to `0` whenever the
    /// buffer changes; clamped to the match list when read.
    selection: usize,
    /// The most recent kill (Ctrl-K/U/W), for Ctrl-Y to yank back. A single
    /// slot rather than emacs's full ring: the prompt and the picker's filter
    /// box need yank, not ring rotation.
    last_kill: String,
    /// Custom keymap bindings, if loaded from `keybindings.json`.
    keymap: Option<crate::keybindings::Keymap>,
    /// The user's saved-prompt commands, as menu rows.
    ///
    /// Held here rather than looked up through a registry reference, because
    /// [`completion`](Self::completion) is a `&self` projection the view calls
    /// every frame: giving the editor a borrow of the registry would put a
    /// lifetime on `InputEditor` and thread it through every view function that
    /// touches one. The rows are built once at startup and never change.
    user_commands: Vec<CompletionRow>,
    /// Active edit mode (Emacs vs Vim modal).
    edit_mode: EditMode,
    /// Pending multi-key Vim operator (e.g. 'd', 'y', 'c').
    pending_vim_op: Option<char>,
    /// The Ctrl-R history search, while one is open. It owns every key until
    /// it is accepted or cancelled.
    search: Option<HistorySearch>,
}

/// A Ctrl-R reverse search through history, readline's `reverse-i-search`.
///
/// The match is shown in the buffer itself, so the box renders it the way it
/// renders any line, and accepting it is just closing the search.
struct HistorySearch {
    /// The line being composed when the search opened, restored on cancel.
    draft: String,
    /// The cursor in `draft`.
    draft_cursor: usize,
    /// The history index (oldest first) of the entry shown, or `None` while
    /// nothing has matched yet.
    matched: Option<usize>,
    /// What has been typed to search for.
    query: String,
}

/// What a keystroke meant at the input edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    /// Consumed by the editor (an insert, a move, a completion); the driver just
    /// redraws.
    Edited,
    /// Enter: the buffer is the submitted line. The driver takes it and starts a
    /// turn. An empty submit is a no-op (returns `Edited`) so Enter does not
    /// start a blank turn.
    Submit,
    /// Ctrl-D, or a second Ctrl-C on an empty line: leave the session.
    Exit,
    /// A first Ctrl-C on an empty line: the driver says a second one exits.
    ConfirmExit,
    /// PageUp: show older transcript. The editor owns key mapping but not the
    /// transcript, so it names the intent and the driver applies it against a
    /// viewport height only the driver knows.
    ScrollUp,
    /// PageDown: back towards the newest transcript.
    ScrollDown,
    /// Shift-Tab (or /mode): switch session execution mode (auto, interactive, plan).
    SwitchMode,
    /// A bare `?` on an empty line: show the keybindings cheat sheet, Claude
    /// Code's convention (distinct from `/help`'s slash-command list).
    ShowKeybindings,
    /// Ignored (an unmapped key).
    None,
}

/// One completion menu row: a name to accept and the text shown beside it.
/// Shared by the command-name menu and the `/login` provider-argument menu, so
/// One completion menu row: a name to accept and the text shown beside it.
#[derive(Clone, Debug)]
pub struct CompletionRow {
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
}

/// What accepting a completion row replaces: the whole buffer for a command
/// name, or just the partial word after `/login ` for a provider id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    LoginProvider,
    File,
}

/// The live completion menu: the matches for the buffer's current position and
/// the highlighted row. The matches are cheap `&'static str` pairs, so this
/// owns no editor state and is cheap to drop.
pub struct Completion {
    pub kind: CompletionKind,
    pub matches: Vec<CompletionRow>,
    pub selected: usize,
}

// ========================================================================
// InputEditor Implementations
// ========================================================================

impl InputEditor {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            dismissed: false,
            history: Vec::new(),
            recall: None,
            draft: String::new(),
            exit_armed_at: None,
            selection: 0,
            last_kill: String::new(),
            keymap: None,
            user_commands: Vec::new(),
            edit_mode: EditMode::Emacs,
            pending_vim_op: None,
            search: None,
        }
    }

    // --------------------------------------------------------------------
    // Configuring the editor
    // --------------------------------------------------------------------

    pub fn edit_mode(&self) -> EditMode {
        self.edit_mode
    }

    pub fn set_edit_mode(&mut self, mode: EditMode) {
        self.edit_mode = mode;
        self.pending_vim_op = None;
    }

    #[cfg(test)]
    pub fn with_edit_mode(mut self, mode: EditMode) -> Self {
        self.edit_mode = mode;
        self
    }

    pub fn with_keymap(mut self, keymap: crate::keybindings::Keymap) -> Self {
        if let Some(style) = keymap.style() {
            if style.eq_ignore_ascii_case("vim") {
                self.edit_mode = EditMode::VimNormal;
            } else if style.eq_ignore_ascii_case("emacs") {
                self.edit_mode = EditMode::Emacs;
            }
        }
        self.keymap = Some(keymap);
        self
    }

    /// Start with `entries` as recall history, oldest first: the lines earlier
    /// sessions submitted.
    pub fn with_history(mut self, entries: Vec<String>) -> Self {
        self.history = entries;
        self
    }

    /// Show the user's saved-prompt commands in the completion menu.
    pub fn with_user_commands(mut self, commands: &crate::command::Registry) -> Self {
        self.user_commands = commands
            .completions("")
            .into_iter()
            .map(|(name, description)| CompletionRow {
                name: name.into(),
                description: description.into(),
            })
            .collect();
        self
    }

    // --------------------------------------------------------------------
    // Reading the buffer
    // --------------------------------------------------------------------

    /// The current text. May contain newlines.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Byte offset of the cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor's row, counting from the first line of the buffer.
    #[cfg(test)]
    pub fn cursor_row(&self) -> usize {
        self.buffer[..self.cursor].matches('\n').count()
    }

    /// The cursor's column within its own line, in characters (for placing the
    /// terminal cursor).
    #[cfg(test)]
    pub fn cursor_column(&self) -> usize {
        self.buffer[self.line_start()..self.cursor].chars().count()
    }

    /// Insert `text` at the cursor, newlines and all.
    ///
    /// The bracketed-paste path. It goes through one call rather than a loop
    /// over [`Self::handle`] because a pasted `\n` is text, and `handle` maps
    /// Enter to submit: replaying a paste as keystrokes is what started a turn
    /// per pasted line.
    pub fn insert_str(&mut self, text: &str) {
        // A paste mid-search lands on the match, as a typed key that is not
        // part of the query would.
        self.accept_search();
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.buffer.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.reset_completion_state();
    }

    /// The live completion menu, if the buffer is a `/`-prefix (command names)
    /// or `/login`'s first argument (provider ids) with at least one match,
    /// and Esc has not dismissed it since. `selected` is clamped to the
    /// match list.
    pub fn completion(&self) -> Option<Completion> {
        if self.dismissed {
            return None;
        }
        if let Some(prefix) = command_prefix(&self.buffer) {
            let mut matches = command_completions(prefix);
            // Built-ins first, then the user's own: a session command and a
            // saved prompt are different kinds of thing, and the ordering says
            // which is which without a second label.
            matches.extend(
                self.user_commands
                    .iter()
                    .filter(|row| row.name.starts_with(prefix))
                    .cloned(),
            );
            if !matches.is_empty() {
                let selected = self.selection.min(matches.len() - 1);
                return Some(Completion {
                    kind: CompletionKind::Command,
                    matches,
                    selected,
                });
            }
        }
        if let Some(prefix) = login_provider_prefix(&self.buffer) {
            let matches = provider_completions(prefix);
            if !matches.is_empty() {
                let selected = self.selection.min(matches.len() - 1);
                return Some(Completion {
                    kind: CompletionKind::LoginProvider,
                    matches,
                    selected,
                });
            }
        }
        if let Some(prefix) = at_file_prefix(&self.buffer, self.cursor) {
            let matches = file_completions(prefix);
            if !matches.is_empty() {
                let selected = self.selection.min(matches.len() - 1);
                return Some(Completion {
                    kind: CompletionKind::File,
                    matches,
                    selected,
                });
            }
        }
        None
    }

    // --------------------------------------------------------------------
    // Handling a key
    // --------------------------------------------------------------------

    /// Apply one key. Returns what the driver should do next.
    pub fn handle(&mut self, key: KeyEvent) -> InputAction {
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        if let Some(keymap) = &self.keymap
            && let Some(action) = keymap.lookup(&key)
        {
            return match action {
                crate::keybindings::KeyAction::CursorLineStart => {
                    self.cursor = self.line_start();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::CursorLineEnd => {
                    self.cursor = self.line_end();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::MoveWordForward => {
                    self.move_word_forward();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::MoveWordBack => {
                    self.move_word_back();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::KillToEnd => {
                    self.kill_to_end();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::KillToStart => {
                    self.kill_to_start();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::KillWordBack => {
                    self.kill_word_back();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::KillWordForward => {
                    self.kill_word_forward();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::TransposeWords => {
                    self.transpose_words();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::TransposeChars => {
                    self.transpose();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::UpcaseWord => {
                    self.upcase_word();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::DowncaseWord => {
                    self.downcase_word();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::CapitalizeWord => {
                    self.capitalize_word();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::Yank => {
                    self.yank();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::Newline => {
                    self.insert('\n');
                    InputAction::Edited
                }
                crate::keybindings::KeyAction::Exit => InputAction::Exit,
                crate::keybindings::KeyAction::ScrollUp => InputAction::ScrollUp,
                crate::keybindings::KeyAction::ScrollDown => InputAction::ScrollDown,
                crate::keybindings::KeyAction::SwitchMode => InputAction::SwitchMode,
            };
        }

        // A bare `?` on an empty line shows the keybindings cheat sheet,
        // Claude Code's convention: a lone `?` is a shortcut, not the start of
        // a message, so only an empty buffer claims it -- a question that
        // happens to start with "?" still types normally. Checked ahead of
        // the Vim Normal dispatch below so it works in both edit modes;
        // excluded from Vim Insert, where `?` is a character being typed.
        if key.code == KeyCode::Char('?')
            && key.modifiers.is_empty()
            && self.buffer.is_empty()
            && self.completion().is_none()
            && self.edit_mode != EditMode::VimInsert
        {
            return InputAction::ShowKeybindings;
        }

        // Shift-Tab / BackTab switches execution modes (or moves up in completion menu)
        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            if self.completion().is_some() {
                return self.navigate(-1);
            }
            return InputAction::SwitchMode;
        }

        // Handle Vim Normal mode motions & commands
        if self.edit_mode == EditMode::VimNormal {
            return self.handle_vim_normal(key);
        }

        // In Vim Insert mode, Esc returns to Vim Normal mode (when no completion menu is open)
        if self.edit_mode == EditMode::VimInsert
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.completion().is_none()
        {
            self.edit_mode = EditMode::VimNormal;
            self.pending_vim_op = None;
            return InputAction::Edited;
        }

        // Alt-Enter and Shift-Enter insert a newline. Shift-Enter only arrives
        // on terminals that report the modifier at all, which is why it is not
        // the only way in.
        if matches!(key.code, KeyCode::Enter)
            && key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            self.insert('\n');
            return InputAction::Edited;
        }

        // Alt / Meta keys: standard Readline / Emacs bindings
        if key.modifiers.contains(KeyModifiers::ALT) {
            return match key.code {
                KeyCode::Char('f' | 'F') | KeyCode::Right => {
                    self.move_word_forward();
                    InputAction::Edited
                }
                KeyCode::Char('b' | 'B') | KeyCode::Left => {
                    self.move_word_back();
                    InputAction::Edited
                }
                KeyCode::Char('d' | 'D') | KeyCode::Delete => {
                    self.kill_word_forward();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('h' | 'H') | KeyCode::Backspace => {
                    self.kill_word_back();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('t' | 'T') => {
                    self.transpose_words();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('u' | 'U') => {
                    self.upcase_word();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('l' | 'L') => {
                    self.downcase_word();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('c' | 'C') => {
                    self.capitalize_word();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('r' | 'R') => {
                    self.kill_line();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('<') => {
                    self.cursor = 0;
                    InputAction::Edited
                }
                KeyCode::Char('>') => {
                    self.cursor = self.buffer.len();
                    InputAction::Edited
                }
                _ => InputAction::None,
            };
        }

        // Ctrl keys: standard Readline / Emacs bindings
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                // Ctrl-D is delete-forward, the emacs chord. On an empty buffer
                // it is EOF instead -- the shell convention -- so it still leaves
                // the session there (and closes a picker, where "leave" means
                // cancel rather than quit).
                KeyCode::Char('d') => {
                    if self.buffer.is_empty() {
                        InputAction::Exit
                    } else {
                        self.delete();
                        self.reset_completion_state();
                        InputAction::Edited
                    }
                }
                KeyCode::Char('c') => self.interrupt_line(),
                KeyCode::Char('r') => {
                    self.start_search();
                    InputAction::Edited
                }
                // The rest of the emacs/readline table, uniform across every
                // editable surface (the prompt and the picker's filter box):
                // caret motion, deletion, and a single-slot kill/yank.
                KeyCode::Char('a') => {
                    self.cursor = self.line_start();
                    InputAction::Edited
                }
                KeyCode::Char('e') => {
                    self.cursor = self.line_end();
                    InputAction::Edited
                }
                KeyCode::Char('b') => {
                    self.move_left();
                    InputAction::Edited
                }
                KeyCode::Char('f') => {
                    self.move_right();
                    InputAction::Edited
                }
                KeyCode::Char('h') => {
                    self.backspace();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('k') => {
                    self.kill_to_end();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('u') => {
                    self.kill_to_start();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('w') => {
                    self.kill_word_back();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('y') => {
                    self.yank();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Char('t') => {
                    self.transpose();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                // Ctrl-J / Ctrl-M is the newline that reaches every terminal: it is
                // literally the line-feed / carriage-return control code.
                KeyCode::Char('j' | 'm') => {
                    self.insert('\n');
                    InputAction::Edited
                }
                // Ctrl-N / Ctrl-P move the selection through the command menu
                // (or recall history when it is closed), the same as Down / Up.
                KeyCode::Char('n' | 'N') => self.navigate(1),
                KeyCode::Char('p' | 'P') => self.navigate(-1),
                KeyCode::Char('l' | 'L') => InputAction::Edited,
                KeyCode::Left => {
                    self.move_word_back();
                    InputAction::Edited
                }
                KeyCode::Right => {
                    self.move_word_forward();
                    InputAction::Edited
                }
                KeyCode::Backspace => {
                    self.kill_word_back();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                KeyCode::Delete => {
                    self.kill_word_forward();
                    self.reset_completion_state();
                    InputAction::Edited
                }
                _ => InputAction::None,
            };
        }
        // While the command menu is open it owns Tab, Enter, Esc, and Up/Down.
        let menu_open = self.completion().is_some();
        match key.code {
            KeyCode::Tab if menu_open => {
                self.accept_completion();
                InputAction::Edited
            }
            // Esc dismisses the popup without touching the buffer, the same
            // way it does in most editors' autocomplete: the text typed so
            // far is kept, only the suggestions go away. `dismissed` clears
            // on the next edit, so typing further can open a new one.
            KeyCode::Esc if menu_open => {
                self.dismissed = true;
                InputAction::Edited
            }
            // Accepting a row that still needs an argument typed after it
            // (a command like `/login` or `/resume`, whose arguments
            // `Command` does not carry) must not submit: that would run the
            // bare, argument-less form and only ever show its usage error. A
            // trailing space is inserted instead, so completing `/login`
            // immediately opens the provider menu rather than dead-ending
            // on the usage error.
            KeyCode::Enter if menu_open && self.completion_needs_more_typing() => {
                self.accept_completion();
                self.insert(' ');
                InputAction::Edited
            }
            // Enter runs the highlighted command, as in Claude Code, with two
            // exceptions that only fill the row in and wait for a second
            // Enter. A file row is part of a message still being written, so
            // `@main.rs` swapping in some other `main.rs` must not also send
            // it. And a command that discards work runs only when typed in
            // full: `/u` highlighted `/undo` ahead of `/usage` and ran it,
            // reverting files with no question asked.
            KeyCode::Enter if menu_open => {
                let typed = self.buffer.clone();
                let kind = self.completion().map(|completion| completion.kind);
                self.accept_completion();
                let completed_by_menu = self.buffer != typed;
                let discards_work = DISCARDING_COMMANDS
                    .iter()
                    .any(|name| self.buffer == format!("/{name}"));
                match kind {
                    Some(CompletionKind::File) => InputAction::Edited,
                    _ if completed_by_menu && discards_work => InputAction::Edited,
                    _ => InputAction::Submit,
                }
            }
            // A trailing backslash continues the line. The fallback that needs
            // nothing from the terminal, and the only one available where a
            // terminal reports neither Alt nor Shift on Enter.
            KeyCode::Enter if self.buffer.ends_with(CONTINUATION) => {
                self.buffer.pop();
                self.cursor = self.buffer.len();
                self.insert('\n');
                InputAction::Edited
            }
            KeyCode::Enter => {
                if self.buffer.trim().is_empty() {
                    InputAction::Edited
                } else {
                    InputAction::Submit
                }
            }
            // Any buffer change resets the menu selection and un-dismisses it,
            // so a suggestion Esc closed can reopen once it no longer matches
            // what Esc dismissed.
            KeyCode::Char(ch) => {
                self.insert(ch);
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Backspace => {
                self.backspace();
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Delete => {
                self.delete();
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Left => {
                self.move_left();
                InputAction::Edited
            }
            KeyCode::Right => {
                self.move_right();
                InputAction::Edited
            }
            // Up / Down navigate the command menu (or history) via the shared
            // helper, the same path Ctrl-N / Ctrl-P take.
            KeyCode::Up => self.navigate(-1),
            KeyCode::Down => self.navigate(1),
            // Line-scoped, as in every editor: on a multi-line buffer, Home
            // meaning "the very start" would jump the cursor out of the line
            // being edited.
            KeyCode::Home => {
                self.cursor = self.line_start();
                InputAction::Edited
            }
            KeyCode::End => {
                self.cursor = self.line_end();
                InputAction::Edited
            }
            KeyCode::PageUp => InputAction::ScrollUp,
            KeyCode::PageDown => InputAction::ScrollDown,
            _ => InputAction::None,
        }
    }

    /// Take the submitted line and reset for the next one.
    pub fn take(&mut self) -> String {
        let line = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.reset_completion_state();
        let trimmed = line.trim().to_string();
        // A `/login` line can carry an API key, and recall would show it to
        // anyone who presses Up.
        if !trimmed.is_empty() && crate::tui::history::is_recordable(&trimmed) {
            // Dedup consecutive duplicates so recalling does not echo the same
            // line twice in a row.
            if self.history.last().is_some_and(|last| last != &trimmed) || self.history.is_empty() {
                self.history.push(trimmed.clone());
            }
        }
        self.recall = None;
        trimmed
    }

    /// Reset the buffer and cursor without recording a submission. The picker
    /// reuses the editor as a filter query, and a typed filter is not a line
    /// that belongs in history recall.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.recall = None;
        self.reset_completion_state();
    }

    // --------------------------------------------------------------------
    // The completion menu
    // --------------------------------------------------------------------

    /// Clear the menu selection and un-dismiss it. Called on every edit (a
    /// keystroke, a paste, accepting a row, or submitting), so a stale
    /// highlight or an Esc from an earlier suggestion never carries into a
    /// different one.
    fn reset_completion_state(&mut self) {
        self.selection = 0;
        self.dismissed = false;
    }

    /// Whether accepting the open menu's highlighted row leaves the command
    /// incomplete: a command whose name is `/login` or `/resume` (both need
    /// arguments `Command` does not carry). A provider id does not:
    /// submitting `/login <provider>` opens the masked key entry downstream,
    /// so the row is already a runnable line.
    fn completion_needs_more_typing(&self) -> bool {
        let Some(completion) = self.completion() else {
            return false;
        };
        match completion.kind {
            CompletionKind::Command => completion
                .matches
                .get(completion.selected)
                .is_some_and(|row| command_takes_arguments(&row.name)),
            CompletionKind::LoginProvider => false,
            CompletionKind::File => false,
        }
    }

    /// Fill the buffer with the menu's selected row and reset the selection.
    /// The menu closes for the next redraw unless the buffer changes again.
    fn accept_completion(&mut self) {
        if let Some(completion) = self.completion()
            && let Some(row) = completion.matches.get(completion.selected).cloned()
        {
            match completion.kind {
                CompletionKind::Command => {
                    self.buffer.clear();
                    self.buffer.push('/');
                    self.buffer.push_str(&row.name);
                    self.cursor = self.buffer.len();
                }
                CompletionKind::LoginProvider => {
                    self.buffer.clear();
                    self.buffer.push_str("/login ");
                    self.buffer.push_str(&row.name);
                    self.cursor = self.buffer.len();
                }
                CompletionKind::File => {
                    if let Some(at_idx) = self.buffer[..self.cursor].rfind('@') {
                        self.buffer
                            .replace_range(at_idx..self.cursor, &format!("@{}", row.name));
                        self.cursor = at_idx + 1 + row.name.len();
                        if !self.buffer[self.cursor..].starts_with(' ') {
                            self.buffer.insert(self.cursor, ' ');
                            self.cursor += 1;
                        }
                    }
                }
            }
        }
        self.reset_completion_state();
    }

    /// Move the menu selection by `delta` rows, wrapping at the ends so Up/Down
    /// never dead-end.
    fn move_selection(&mut self, delta: i32) {
        if let Some(completion) = self.completion() {
            let len = completion.matches.len() as i32;
            if len > 0 {
                self.selection = (completion.selected as i32 + delta).rem_euclid(len) as usize;
            }
        }
    }

    /// A wheel notch over the prompt: the command menu's selection when it is
    /// open, else the cursor one line. Never history, unlike Up: a notch is
    /// easily turned by accident, and recalling an entry replaces the draft.
    /// Returns whether there was anything to move, so the caller can give an
    /// unmoved notch to the transcript instead.
    pub fn wheel(&mut self, up: bool) -> bool {
        let delta = if up { -1 } else { 1 };
        if self.completion().is_some() {
            self.move_selection(delta);
            return true;
        }
        self.move_line(delta)
    }

    /// Move up (`-1`) or down (`+1`): through the command menu when it is open,
    /// then within a multi-line buffer, and only then through history. The arrow
    /// keys and Ctrl-N / Ctrl-P share this.
    ///
    /// **A multi-line buffer keeps Up/Down even at its edges**, which is the one
    /// case worth arguing. Falling through to history at the top line would
    /// replace a message the user is part-way through composing, and there is no
    /// undo; a key that does nothing is a much smaller cost than a key that
    /// destroys work.
    fn navigate(&mut self, delta: i32) -> InputAction {
        if self.completion().is_some() {
            self.move_selection(delta);
        } else if self.move_line(delta) || self.buffer.contains('\n') {
            // Moved within the buffer, or held at its edge.
        } else if delta < 0 {
            self.recall_older();
        } else {
            self.recall_newer();
        }
        InputAction::Edited
    }

    // --------------------------------------------------------------------
    // Moving the cursor
    // --------------------------------------------------------------------

    /// Move the cursor one line up (`-1`) or down (`+1`), keeping its column
    /// where the target line is long enough. Returns whether there was a line to
    /// move to.
    fn move_line(&mut self, delta: i32) -> bool {
        let start = self.line_start();
        let column = self.buffer[start..self.cursor].chars().count();
        let (target_start, target_end) = if delta < 0 {
            if start == 0 {
                return false;
            }
            // `start - 1` is the newline ending the previous line.
            let previous = self.buffer[..start - 1].rfind('\n').map_or(0, |at| at + 1);
            (previous, start - 1)
        } else {
            let end = self.line_end();
            if end == self.buffer.len() {
                return false;
            }
            let next = end + 1;
            let next_end = self.buffer[next..]
                .find('\n')
                .map_or(self.buffer.len(), |at| next + at);
            (next, next_end)
        };
        self.cursor = offset_at_column(&self.buffer, target_start, target_end, column);
        true
    }

    /// Byte offset of the start of the line the cursor is on.
    fn line_start(&self) -> usize {
        self.buffer[..self.cursor]
            .rfind('\n')
            .map_or(0, |at| at + 1)
    }

    /// Byte offset of the end of the line the cursor is on: its newline, or the
    /// end of the buffer.
    fn line_end(&self) -> usize {
        self.buffer[self.cursor..]
            .find('\n')
            .map_or(self.buffer.len(), |at| self.cursor + at)
    }

    // --------------------------------------------------------------------
    // Editing the buffer
    // --------------------------------------------------------------------

    fn insert(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some((index, _)) = self.buffer[..self.cursor].char_indices().next_back() {
            self.buffer.replace_range(index..self.cursor, "");
            self.cursor = index;
        }
    }

    fn delete(&mut self) {
        // Delete the char to the right of the cursor.
        let rest = &self.buffer[self.cursor..];
        if let Some((len, _)) = rest.char_indices().nth(1) {
            // `nth(1)` is the second char's byte offset, which is the byte length
            // of the first char: the range to remove.
            let end = self.cursor + len;
            self.buffer.replace_range(self.cursor..end, "");
        } else if !rest.is_empty() {
            self.buffer.replace_range(self.cursor.., "");
        }
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.buffer[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some((index, _)) = self.buffer[self.cursor..].char_indices().nth(1) {
            self.cursor += index;
        } else {
            self.cursor = self.buffer.len();
        }
    }

    /// Kill from the cursor to the end of the line (Ctrl-K). The killed text
    /// replaces the kill ring for a later Ctrl-Y.
    fn kill_to_end(&mut self) {
        let end = self.line_end();
        let end = if end == self.cursor && end < self.buffer.len() {
            end + 1
        } else {
            end
        };
        let killed: String = self.buffer.drain(self.cursor..end).collect();
        if !killed.is_empty() {
            self.last_kill = killed;
        }
    }

    /// Kill from the start of the line to the cursor (Ctrl-U).
    fn kill_to_start(&mut self) {
        let start = self.line_start();
        let killed: String = self.buffer.drain(start..self.cursor).collect();
        self.cursor = start;
        if !killed.is_empty() {
            self.last_kill = killed;
        }
    }

    /// Kill the word before the cursor (Ctrl-W): trailing whitespace first, then
    /// the run of non-whitespace -- the same argument boundary a shell's Ctrl-W
    /// deletes.
    fn kill_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before: Vec<char> = self.buffer[..self.cursor].chars().collect();
        let mut end = before.len();
        while end > 0 && before[end - 1].is_whitespace() {
            end -= 1;
        }
        while end > 0 && !before[end - 1].is_whitespace() {
            end -= 1;
        }
        let byte_end = before[..end].iter().map(|c| c.len_utf8()).sum();
        let killed: String = self.buffer.drain(byte_end..self.cursor).collect();
        self.cursor = byte_end;
        if !killed.is_empty() {
            self.last_kill = killed;
        }
    }

    /// Yank the last kill back at the cursor (Ctrl-Y).
    fn yank(&mut self) {
        if self.last_kill.is_empty() {
            return;
        }
        let at = self.cursor;
        let text = self.last_kill.clone();
        self.buffer.insert_str(at, &text);
        self.cursor += text.len();
    }

    /// Transpose the two characters immediately before the cursor (Ctrl-T). A
    /// deliberate slice of readline -- not the cursor-advancing, end-of-line
    /// form -- because the before-cursor pair is the case that fixes a typo in
    /// place, and the cursor's byte offset is re-derived so swapping chars of
    /// different widths cannot land it mid-codepoint.
    fn transpose(&mut self) {
        let mut chars: Vec<char> = self.buffer.chars().collect();
        let pos = self.buffer[..self.cursor].chars().count();
        if pos < 2 {
            return;
        }
        chars.swap(pos - 2, pos - 1);
        self.buffer = chars.iter().collect();
        self.cursor = chars[..pos].iter().map(|c| c.len_utf8()).sum();
    }

    // --------------------------------------------------------------------
    // History recall
    // --------------------------------------------------------------------

    /// Ctrl-C at the prompt. With text, clear it but keep it for Ctrl-Y, so
    /// a slip loses nothing; on an empty line, exit only on a second press.
    fn interrupt_line(&mut self) -> InputAction {
        if !self.buffer.is_empty() {
            self.last_kill = std::mem::take(&mut self.buffer);
            self.cursor = 0;
            self.reset_completion_state();
            return InputAction::Edited;
        }
        let now = Instant::now();
        if self
            .exit_armed_at
            .is_some_and(|armed| now.duration_since(armed) <= EXIT_CONFIRM_WINDOW)
        {
            return InputAction::Exit;
        }
        self.exit_armed_at = Some(now);
        InputAction::ConfirmExit
    }

    /// Recall the next older submitted line (Up). Stays at the oldest if already
    /// there.
    fn recall_older(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.recall.is_none() {
            self.draft = self.buffer.clone();
        }
        let next = self.recall.map_or(0, |slot| slot + 1);
        if next < self.history.len() {
            self.recall = Some(next);
            self.buffer = self.history[self.history.len() - 1 - next].clone();
            self.cursor = self.buffer.len();
        }
    }

    /// Recall the next newer line (Down). Past the most recent, returns to a
    /// fresh empty line.
    fn recall_newer(&mut self) {
        match self.recall {
            Some(0) => {
                self.recall = None;
                self.buffer = std::mem::take(&mut self.draft);
                self.cursor = self.buffer.len();
            }
            Some(slot) => {
                let newer = slot - 1;
                self.recall = Some(newer);
                self.buffer = self.history[self.history.len() - 1 - newer].clone();
                self.cursor = self.buffer.len();
            }
            None => {}
        }
    }

    // --------------------------------------------------------------------
    // Reverse history search
    // --------------------------------------------------------------------

    /// Whether a Ctrl-R search is open and owns the keys.
    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    /// What the input box shows while a search is open, in readline's words:
    /// `(reverse-i-search)`query'`, or `failing` once nothing matches it.
    pub fn search_label(&self) -> Option<String> {
        let search = self.search.as_ref()?;
        let query = search.query.to_lowercase();
        let failing = !query.is_empty()
            && search
                .matched
                .is_none_or(|index| !self.history[index].to_lowercase().contains(&query));
        let name = if failing {
            "failing reverse-i-search"
        } else {
            "reverse-i-search"
        };
        Some(format!("({name})`{}'", search.query))
    }

    fn start_search(&mut self) {
        self.search = Some(HistorySearch {
            draft: self.buffer.clone(),
            draft_cursor: self.cursor,
            matched: None,
            query: String::new(),
        });
    }

    /// A key while a search is open. Typing narrows it, Ctrl-R steps to the
    /// next older match, Esc or Ctrl-C puts the draft back. Enter accepts the
    /// match into the buffer *without* sending it: an old prompt starts a
    /// turn only when someone has read it and pressed Enter again. Any other
    /// key accepts and then acts on the match, readline's behavior, so Ctrl-E
    /// or an arrow goes straight to editing it.
    fn handle_search_key(&mut self, key: KeyEvent) -> InputAction {
        let Some(search) = self.search.as_mut() else {
            return self.handle(key);
        };
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let len = self.history.len();
        match key.code {
            KeyCode::Char('r') if !plain => {
                let before = search.matched.unwrap_or(len);
                self.search_before(before);
            }
            KeyCode::Char('c') if !plain => self.cancel_search(),
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter => self.accept_search(),
            KeyCode::Backspace => {
                search.query.pop();
                self.search_before(len);
            }
            KeyCode::Char(ch) if plain => {
                search.query.push(ch);
                // The entry on screen still matches a longer query more often
                // than not, so the search resumes from it rather than above it.
                let from = search.matched.map_or(len, |index| index + 1);
                self.search_before(from);
            }
            _ => {
                self.accept_search();
                return self.handle(key);
            }
        }
        InputAction::Edited
    }

    /// Show the newest entry older than `before` containing the query, case
    /// folded. With no match the entry already shown stays, as in readline;
    /// with an empty query the draft comes back.
    fn search_before(&mut self, before: usize) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if search.query.is_empty() {
            search.matched = None;
            self.buffer = search.draft.clone();
            self.cursor = search.draft_cursor;
            return;
        }
        let query = search.query.to_lowercase();
        if let Some(found) = self.history[..before.min(self.history.len())]
            .iter()
            .rposition(|entry| entry.to_lowercase().contains(&query))
        {
            search.matched = Some(found);
            self.buffer = self.history[found].clone();
            self.cursor = self.buffer.len();
        }
    }

    /// Close the search, keeping the match in the buffer to edit or send.
    fn accept_search(&mut self) {
        if self.search.take().is_some() {
            self.recall = None;
            self.reset_completion_state();
        }
    }

    /// Close the search and put back the line it replaced.
    fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.buffer = search.draft;
            self.cursor = search.draft_cursor;
            self.reset_completion_state();
        }
    }

    // --------------------------------------------------------------------
    // Vim normal mode
    // --------------------------------------------------------------------

    fn handle_vim_normal(&mut self, key: KeyEvent) -> InputAction {
        // Handle pending operator (dd, dw, yy, cw, cc)
        if let Some(op) = self.pending_vim_op.take() {
            match (op, key.code) {
                ('d', KeyCode::Char('d')) => {
                    self.kill_line();
                    self.reset_completion_state();
                    return InputAction::Edited;
                }
                ('d', KeyCode::Char('w')) => {
                    self.vim_kill_word_forward();
                    self.reset_completion_state();
                    return InputAction::Edited;
                }
                ('y', KeyCode::Char('y')) => {
                    let start = self.line_start();
                    let end = self.line_end();
                    self.last_kill = self.buffer[start..end].to_string();
                    return InputAction::Edited;
                }
                ('c', KeyCode::Char('w')) => {
                    self.vim_kill_word_forward();
                    self.reset_completion_state();
                    self.edit_mode = EditMode::VimInsert;
                    return InputAction::Edited;
                }
                ('c', KeyCode::Char('c')) => {
                    self.kill_line();
                    self.reset_completion_state();
                    self.edit_mode = EditMode::VimInsert;
                    return InputAction::Edited;
                }
                _ => {}
            }
        }

        // Ctrl-C or Ctrl-D in normal mode
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => self.interrupt_line(),
                KeyCode::Char('d') => {
                    if self.buffer.is_empty() {
                        InputAction::Exit
                    } else {
                        self.navigate(4)
                    }
                }
                KeyCode::Char('u') => self.navigate(-4),
                KeyCode::Char('r') => {
                    self.start_search();
                    InputAction::Edited
                }
                _ => InputAction::None,
            };
        }

        match key.code {
            KeyCode::Char('i') => {
                self.edit_mode = EditMode::VimInsert;
                InputAction::Edited
            }
            KeyCode::Char('a') => {
                self.move_right();
                self.edit_mode = EditMode::VimInsert;
                InputAction::Edited
            }
            KeyCode::Char('I') => {
                self.cursor = self.line_start();
                self.edit_mode = EditMode::VimInsert;
                InputAction::Edited
            }
            KeyCode::Char('A') => {
                self.cursor = self.line_end();
                self.edit_mode = EditMode::VimInsert;
                InputAction::Edited
            }
            KeyCode::Char('o') => {
                self.cursor = self.line_end();
                self.insert('\n');
                self.edit_mode = EditMode::VimInsert;
                InputAction::Edited
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.move_left();
                InputAction::Edited
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.move_right();
                InputAction::Edited
            }
            KeyCode::Char('k') | KeyCode::Up => self.navigate(-1),
            KeyCode::Char('j') | KeyCode::Down => self.navigate(1),
            KeyCode::Char('w') => {
                self.vim_move_word_forward();
                InputAction::Edited
            }
            KeyCode::Char('b') => {
                self.move_word_back();
                InputAction::Edited
            }
            KeyCode::Char('0') | KeyCode::Char('^') | KeyCode::Home => {
                self.cursor = self.line_start();
                InputAction::Edited
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.cursor = self.line_end();
                InputAction::Edited
            }
            KeyCode::Char('x') => {
                self.delete();
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Char('D') => {
                self.kill_to_end();
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Char('C') => {
                self.kill_to_end();
                self.reset_completion_state();
                self.edit_mode = EditMode::VimInsert;
                InputAction::Edited
            }
            KeyCode::Char('d' | 'y' | 'c') => {
                if let KeyCode::Char(c) = key.code {
                    self.pending_vim_op = Some(c);
                }
                InputAction::Edited
            }
            KeyCode::Char('p') => {
                self.yank();
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Char('P') => {
                self.move_left();
                self.yank();
                self.reset_completion_state();
                InputAction::Edited
            }
            KeyCode::Char('u') => InputAction::Edited,
            KeyCode::Enter => {
                if self.buffer.trim().is_empty() {
                    InputAction::Edited
                } else {
                    InputAction::Submit
                }
            }
            KeyCode::Esc => {
                self.pending_vim_op = None;
                InputAction::Edited
            }
            KeyCode::PageUp => InputAction::ScrollUp,
            KeyCode::PageDown => InputAction::ScrollDown,
            _ => InputAction::None,
        }
    }

    // --------------------------------------------------------------------
    // Killing and yanking
    // --------------------------------------------------------------------

    fn kill_line(&mut self) {
        let start = self.line_start();
        let end = self.line_end();
        let next = if end < self.buffer.len() && self.buffer.as_bytes()[end] == b'\n' {
            end + 1
        } else {
            end
        };
        let killed: String = self.buffer.drain(start..next).collect();
        self.cursor = start.min(self.buffer.len());
        if !killed.is_empty() {
            self.last_kill = killed;
        }
    }

    /// Kill from cursor to the end of the current or next word (Alt-D, Ctrl-Delete).
    fn kill_word_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let old_cursor = self.cursor;
        self.move_word_forward();
        let end = self.cursor;
        self.cursor = old_cursor;
        let killed: String = self.buffer.drain(old_cursor..end).collect();
        if !killed.is_empty() {
            self.last_kill = killed;
        }
    }

    /// Vim 'w': Move cursor forward to the start of the next word.
    fn vim_move_word_forward(&mut self) {
        let rest = &self.buffer[self.cursor..];
        let mut in_word = false;
        for (i, ch) in rest.char_indices() {
            if i == 0 {
                in_word = !ch.is_whitespace();
                continue;
            }
            if in_word && ch.is_whitespace() {
                in_word = false;
            } else if !in_word && !ch.is_whitespace() {
                self.cursor += i;
                return;
            }
        }
        self.cursor = self.buffer.len();
    }

    /// Vim 'dw': Kill from cursor to the start of the next word.
    fn vim_kill_word_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let old_cursor = self.cursor;
        self.vim_move_word_forward();
        let end = self.cursor;
        self.cursor = old_cursor;
        let killed: String = self.buffer.drain(old_cursor..end).collect();
        if !killed.is_empty() {
            self.last_kill = killed;
        }
    }

    fn is_word_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    // --------------------------------------------------------------------
    // Word motions
    // --------------------------------------------------------------------

    /// Move cursor forward to the end of the current or next word (Alt-F, Ctrl-Right).
    fn move_word_forward(&mut self) {
        let rest = &self.buffer[self.cursor..];
        let mut in_word = false;
        for (i, ch) in rest.char_indices() {
            let is_word = Self::is_word_char(ch);
            if !in_word && is_word {
                in_word = true;
            } else if in_word && !is_word {
                self.cursor += i;
                return;
            }
        }
        self.cursor = self.buffer.len();
    }

    /// Move cursor backward to the start of the current or previous word (Alt-B, Ctrl-Left).
    fn move_word_back(&mut self) {
        let prefix = &self.buffer[..self.cursor];
        let mut in_word = false;
        let mut target = 0;
        for (i, ch) in prefix.char_indices().rev() {
            let is_word = Self::is_word_char(ch);
            if !in_word && is_word {
                in_word = true;
                target = i;
            } else if in_word && is_word {
                target = i;
            } else if in_word && !is_word {
                break;
            }
        }
        self.cursor = target;
    }

    /// Transpose the word before (or at) cursor with the previous word (Alt-T).
    fn transpose_words(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let cur = self.cursor.min(self.buffer.len());

        // Find end of second word (w2_end)
        let mut w2_end = cur;
        while w2_end < self.buffer.len() {
            let next_char = self.buffer[w2_end..].chars().next().unwrap();
            if Self::is_word_char(next_char) {
                w2_end += next_char.len_utf8();
            } else {
                break;
            }
        }
        // Find start of second word (w2_start)
        let mut w2_start = w2_end;
        while w2_start > 0 {
            let prev_char = self.buffer[..w2_start].chars().next_back().unwrap();
            if Self::is_word_char(prev_char) {
                w2_start -= prev_char.len_utf8();
            } else {
                break;
            }
        }
        // Skip non-word chars backwards to find end of first word (w1_end)
        let mut w1_end = w2_start;
        while w1_end > 0 {
            let prev_char = self.buffer[..w1_end].chars().next_back().unwrap();
            if !Self::is_word_char(prev_char) {
                w1_end -= prev_char.len_utf8();
            } else {
                break;
            }
        }
        // Find start of first word (w1_start)
        let mut w1_start = w1_end;
        while w1_start > 0 {
            let prev_char = self.buffer[..w1_start].chars().next_back().unwrap();
            if Self::is_word_char(prev_char) {
                w1_start -= prev_char.len_utf8();
            } else {
                break;
            }
        }

        if w1_start < w1_end && w2_start < w2_end {
            let w1 = self.buffer[w1_start..w1_end].to_string();
            let mid = self.buffer[w1_end..w2_start].to_string();
            let w2 = self.buffer[w2_start..w2_end].to_string();
            let replacement = format!("{w2}{mid}{w1}");
            self.buffer.replace_range(w1_start..w2_end, &replacement);
            self.cursor = w1_start + replacement.len();
        }
    }

    fn word_forward_range(&self) -> Option<(usize, usize)> {
        let rest = &self.buffer[self.cursor..];
        let mut start = None;
        let mut end = None;
        for (i, ch) in rest.char_indices() {
            let is_word = Self::is_word_char(ch);
            if start.is_none() && is_word {
                start = Some(self.cursor + i);
            } else if start.is_some() && !is_word {
                end = Some(self.cursor + i);
                break;
            }
        }
        if let Some(s) = start {
            let e = end.unwrap_or(self.buffer.len());
            Some((s, e))
        } else {
            None
        }
    }

    // --------------------------------------------------------------------
    // Case conversion
    // --------------------------------------------------------------------

    /// Uppercase the word from cursor forward (Alt-U).
    fn upcase_word(&mut self) {
        if let Some((start, end)) = self.word_forward_range() {
            let uppercased = self.buffer[start..end].to_uppercase();
            self.buffer.replace_range(start..end, &uppercased);
            self.cursor = start + uppercased.len();
        }
    }

    /// Lowercase the word from cursor forward (Alt-L).
    fn downcase_word(&mut self) {
        if let Some((start, end)) = self.word_forward_range() {
            let lowercased = self.buffer[start..end].to_lowercase();
            self.buffer.replace_range(start..end, &lowercased);
            self.cursor = start + lowercased.len();
        }
    }

    /// Capitalize the word from cursor forward (Alt-C).
    fn capitalize_word(&mut self) {
        if let Some((start, end)) = self.word_forward_range() {
            let slice = &self.buffer[start..end];
            let mut result = String::with_capacity(slice.len());
            let mut first = true;
            for ch in slice.chars() {
                if first && Self::is_word_char(ch) {
                    for u in ch.to_uppercase() {
                        result.push(u);
                    }
                    first = false;
                } else {
                    for l in ch.to_lowercase() {
                        result.push(l);
                    }
                }
            }
            self.buffer.replace_range(start..end, &result);
            self.cursor = start + result.len();
        }
    }
}

impl Default for InputEditor {
    fn default() -> Self {
        Self::new()
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The byte offset `column` characters into `text[start..end]`, or `end` when
/// the line is shorter than that. Keeps a vertical move on a char boundary.
fn offset_at_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(at, _)| start + at)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use provider::PROFILES;

    use super::*;
    use crate::tui::slash::COMMANDS;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn type_str(editor: &mut InputEditor, text: &str) {
        for ch in text.chars() {
            editor.handle(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn typing_and_submitting_yields_the_line() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "hello");
        assert_eq!(editor.buffer(), "hello");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        assert_eq!(editor.take(), "hello");
        assert!(editor.buffer().is_empty());
    }

    #[test]
    fn backspace_and_cursor_movement_edit_in_place() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "abc");
        editor.handle(key(KeyCode::Left));
        editor.handle(key(KeyCode::Backspace));
        assert_eq!(editor.buffer(), "ac");
        editor.handle(key(KeyCode::End));
        editor.handle(key(KeyCode::Backspace));
        assert_eq!(editor.buffer(), "a");
    }

    #[test]
    fn inserting_in_the_middle_shifts_the_tail() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "ac");
        editor.handle(key(KeyCode::Home));
        editor.handle(key(KeyCode::Right));
        editor.handle(key(KeyCode::Char('b')));
        assert_eq!(editor.buffer(), "abc");
    }

    #[test]
    fn an_empty_submit_is_a_noop() {
        let mut editor = InputEditor::new();
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Edited);
    }

    #[test]
    fn ctrl_d_exits_and_ctrl_c_exits_on_a_second_press() {
        // One reflexive Ctrl-C used to end the session; it now asks first.
        let mut editor = InputEditor::new();
        assert_eq!(editor.handle(ctrl('d')), InputAction::Exit);
        assert_eq!(editor.handle(ctrl('c')), InputAction::ConfirmExit);
        assert_eq!(editor.handle(ctrl('c')), InputAction::Exit);
    }

    #[test]
    fn ctrl_c_clears_the_line_but_ctrl_y_brings_it_back() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "a long carefully written prompt");
        assert_eq!(editor.handle(ctrl('c')), InputAction::Edited);
        assert_eq!(editor.buffer(), "");
        editor.handle(ctrl('y'));
        assert_eq!(editor.buffer(), "a long carefully written prompt");
    }

    #[test]
    fn browsing_history_keeps_the_draft_being_written() {
        // One Up used to overwrite the draft with no way back.
        let mut editor = InputEditor::new();
        type_str(&mut editor, "sent earlier");
        editor.take();
        type_str(&mut editor, "half-written");
        editor.handle(key(KeyCode::Up));
        assert_eq!(editor.buffer(), "sent earlier");
        editor.handle(key(KeyCode::Down));
        assert_eq!(editor.buffer(), "half-written");
    }

    #[test]
    fn the_cursor_column_tracks_characters_not_bytes() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "é"); // two bytes, one char
        assert_eq!(editor.cursor_column(), 1);
    }

    fn searchable() -> InputEditor {
        InputEditor::new().with_history(vec![
            "fix the Parser bug".into(),
            "run the tests".into(),
            "fix the lexer".into(),
        ])
    }

    #[test]
    fn ctrl_r_finds_the_newest_match_and_steps_to_older_ones() {
        let mut editor = searchable();
        editor.handle(ctrl('r'));
        type_str(&mut editor, "fix");
        assert_eq!(editor.buffer(), "fix the lexer", "the newest match first");
        editor.handle(ctrl('r'));
        assert_eq!(
            editor.buffer(),
            "fix the Parser bug",
            "again: the next older"
        );
        editor.handle(ctrl('r'));
        assert_eq!(
            editor.buffer(),
            "fix the Parser bug",
            "no older match stays put"
        );
        assert_eq!(
            editor.search_label().as_deref(),
            Some("(reverse-i-search)`fix'")
        );
    }

    #[test]
    fn ctrl_r_narrows_case_blind_and_reports_a_failing_query() {
        let mut editor = searchable();
        editor.handle(ctrl('r'));
        type_str(&mut editor, "PARSER");
        assert_eq!(editor.buffer(), "fix the Parser bug");
        type_str(&mut editor, "x");
        assert_eq!(
            editor.buffer(),
            "fix the Parser bug",
            "the last match stays"
        );
        assert_eq!(
            editor.search_label().as_deref(),
            Some("(failing reverse-i-search)`PARSERx'")
        );
        editor.handle(key(KeyCode::Backspace));
        assert_eq!(
            editor.search_label().as_deref(),
            Some("(reverse-i-search)`PARSER'")
        );
    }

    #[test]
    fn enter_accepts_a_search_match_without_sending_it() {
        // Sending an old prompt on the Enter that picked it starts a turn the
        // user has not reread.
        let mut editor = searchable();
        editor.handle(ctrl('r'));
        type_str(&mut editor, "tests");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Edited);
        assert!(!editor.is_searching());
        assert_eq!(editor.buffer(), "run the tests");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
    }

    #[test]
    fn esc_cancels_a_search_and_restores_the_draft() {
        let mut editor = searchable();
        type_str(&mut editor, "half a thought");
        editor.handle(key(KeyCode::Left));
        editor.handle(ctrl('r'));
        type_str(&mut editor, "lexer");
        assert_eq!(editor.buffer(), "fix the lexer");
        editor.handle(key(KeyCode::Esc));
        assert!(!editor.is_searching());
        assert_eq!(editor.buffer(), "half a thought");
        assert_eq!(editor.cursor(), "half a though".len(), "the cursor too");
    }

    #[test]
    fn an_editing_key_accepts_the_match_and_then_edits_it() {
        let mut editor = searchable();
        editor.handle(ctrl('r'));
        type_str(&mut editor, "run");
        editor.handle(ctrl('a'));
        assert!(!editor.is_searching());
        type_str(&mut editor, "now ");
        assert_eq!(editor.buffer(), "now run the tests");
    }

    #[test]
    fn the_wheel_moves_within_the_draft_but_never_recalls_history_over_it() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "earlier");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        editor.take();
        type_str(&mut editor, "draft");
        assert!(!editor.wheel(true), "one line: nothing to move");
        assert_eq!(editor.buffer(), "draft", "the draft survives the notch");

        editor.insert_str("\nsecond line");
        assert!(editor.wheel(true), "a line above to move to");
        assert_eq!(editor.buffer(), "draft\nsecond line");
    }

    #[test]
    fn up_down_recalls_history_and_returns_to_a_fresh_line() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "first");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        assert_eq!(editor.take(), "first");
        type_str(&mut editor, "second");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        assert_eq!(editor.take(), "second");

        // Up recalls the most recent (second), then the older (first).
        editor.handle(key(KeyCode::Up));
        assert_eq!(editor.buffer(), "second");
        editor.handle(key(KeyCode::Up));
        assert_eq!(editor.buffer(), "first");
        // A third Up stays at the oldest.
        editor.handle(key(KeyCode::Up));
        assert_eq!(editor.buffer(), "first");
        // Down walks back to second, then to a fresh empty line.
        editor.handle(key(KeyCode::Down));
        assert_eq!(editor.buffer(), "second");
        editor.handle(key(KeyCode::Down));
        assert!(
            editor.buffer().is_empty(),
            "past the newest is a fresh line"
        );
    }

    #[test]
    fn consecutive_duplicate_submissions_dedup_in_history() {
        let mut editor = InputEditor::new();
        for _ in 0..2 {
            type_str(&mut editor, "same");
            editor.handle(key(KeyCode::Enter));
            editor.take();
        }
        editor.handle(key(KeyCode::Up));
        editor.handle(key(KeyCode::Up));
        // Only one "same" in history, so a second Up stays put.
        assert_eq!(editor.buffer(), "same");
    }

    #[test]
    fn every_newline_key_inserts_rather_than_submits() {
        // Three ways in because no one of them works everywhere: Shift-Enter
        // only arrives where the terminal reports the modifier, and a terminal
        // that reports neither Alt nor Shift on Enter leaves the backslash as
        // the only route. Each is asserted because each covers a terminal the
        // others do not.
        for key in [
            modified(KeyCode::Enter, KeyModifiers::ALT),
            modified(KeyCode::Enter, KeyModifiers::SHIFT),
            ctrl('j'),
        ] {
            let mut editor = InputEditor::new();
            type_str(&mut editor, "first");
            assert_eq!(editor.handle(key), InputAction::Edited, "{key:?} submitted");
            type_str(&mut editor, "second");
            assert_eq!(editor.buffer(), "first\nsecond", "{key:?}");
        }
    }

    #[test]
    fn a_trailing_backslash_continues_the_line() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "first\\");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Edited);
        type_str(&mut editor, "second");
        assert_eq!(
            editor.buffer(),
            "first\nsecond",
            "the backslash is consumed, not sent to the model"
        );
        // A plain Enter on a line that does not end in one still submits.
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
    }

    #[test]
    fn a_pasted_block_arrives_as_one_line_with_its_newlines() {
        // The bug this closes: without bracketed paste the same text arrived as
        // keystrokes, and each embedded Enter submitted, so pasting a five-line
        // stack trace started five turns against the model.
        let mut editor = InputEditor::new();
        editor.insert_str("line one\nline two\nline three");
        assert_eq!(editor.cursor_row(), 2);
        assert_eq!(editor.buffer().split('\n').count(), 3);
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        assert_eq!(editor.take(), "line one\nline two\nline three");
    }

    #[test]
    fn pasted_crlf_and_cr_are_normalized_to_maintain_newlines() {
        let mut editor = InputEditor::new();
        editor.insert_str("first\r\nsecond\rthird\nfourth");
        assert_eq!(editor.buffer(), "first\nsecond\nthird\nfourth");
        assert_eq!(editor.cursor_row(), 3);
        assert_eq!(editor.take(), "first\nsecond\nthird\nfourth");
    }

    #[test]
    fn ctrl_k_on_multiline_buffer_kills_only_to_line_end() {
        let mut editor = InputEditor::new();
        editor.insert_str("first line\nsecond line\nthird line");
        // Move to start of line 2
        editor.handle(key(KeyCode::Up));
        editor.handle(ctrl('a'));
        assert_eq!(editor.cursor_row(), 1);
        assert_eq!(editor.cursor_column(), 0);

        // Kill line 2 content
        editor.handle(ctrl('k'));
        assert_eq!(editor.buffer(), "first line\n\nthird line");

        // Kill the newline itself
        editor.handle(ctrl('k'));
        assert_eq!(editor.buffer(), "first line\nthird line");
    }

    #[test]
    fn up_and_down_move_between_lines_and_keep_the_column() {
        let mut editor = InputEditor::new();
        editor.insert_str("abcdef\nxy\nghijkl");
        // Cursor at the end of line 3, column 6. Up lands on line 2, which is
        // only two columns wide, so it clamps to the end of that line.
        editor.handle(key(KeyCode::Up));
        assert_eq!((editor.cursor_row(), editor.cursor_column()), (1, 2));
        // And there it stays: there is no sticky goal column, so a second Up
        // carries column 2 rather than restoring the 6 it started from. Worth
        // pinning, because it is the behaviour someone would otherwise "fix"
        // into a regression while chasing the editor feel.
        editor.handle(key(KeyCode::Up));
        assert_eq!((editor.cursor_row(), editor.cursor_column()), (0, 2));
        editor.handle(key(KeyCode::Down));
        assert_eq!(editor.cursor_row(), 1);
    }

    #[test]
    fn a_multi_line_buffer_never_loses_itself_to_history() {
        // Up at the top line of a half-composed message must not recall
        // history over it: there is no undo, and the message is gone.
        let mut editor = InputEditor::new();
        type_str(&mut editor, "an old line");
        editor.handle(key(KeyCode::Enter));
        editor.take();

        editor.insert_str("draft line one\ndraft line two");
        editor.handle(key(KeyCode::Up));
        editor.handle(key(KeyCode::Up));
        assert_eq!(
            editor.buffer(),
            "draft line one\ndraft line two",
            "history overwrote a draft"
        );
    }

    #[test]
    fn home_and_end_are_scoped_to_the_cursors_own_line() {
        let mut editor = InputEditor::new();
        editor.insert_str("first\nsecond");
        editor.handle(key(KeyCode::Home));
        assert_eq!((editor.cursor_row(), editor.cursor_column()), (1, 0));
        editor.handle(key(KeyCode::End));
        assert_eq!((editor.cursor_row(), editor.cursor_column()), (1, 6));
    }

    #[test]
    fn a_buffer_ending_in_a_newline_puts_the_cursor_on_a_new_row() {
        // The view sizes the input box from the buffer's `split('\n')` count and
        // places the caret at `cursor_row`. The two have to agree, and
        // `str::lines` would drop the trailing empty line, drawing the caret a
        // row above where the next character lands.
        let mut editor = InputEditor::new();
        editor.insert_str("one\n");
        assert_eq!(editor.buffer().split('\n').count(), 2);
        assert_eq!(editor.cursor_row(), 1);
        assert_eq!(editor.cursor_column(), 0);
    }

    #[test]
    fn page_keys_ask_the_driver_to_scroll() {
        let mut editor = InputEditor::new();
        assert_eq!(editor.handle(key(KeyCode::PageUp)), InputAction::ScrollUp);
        assert_eq!(
            editor.handle(key(KeyCode::PageDown)),
            InputAction::ScrollDown
        );
    }

    #[test]
    fn a_slash_opens_the_menu_and_tab_completes_the_selection() {
        let mut editor = InputEditor::new();
        editor.handle(key(KeyCode::Char('/')));
        let menu = editor.completion().expect("menu opens on a bare slash");
        assert_eq!(menu.matches.len(), COMMANDS.len());
        assert_eq!(menu.selected, 0);
        // Down moves the selection, not history: the buffer is unchanged.
        editor.handle(key(KeyCode::Down));
        assert_eq!(editor.completion().unwrap().selected, 1);
        assert_eq!(editor.buffer(), "/");
        // Tab fills the highlighted command (index 1 in COMMANDS is `status`).
        editor.handle(key(KeyCode::Tab));
        assert_eq!(editor.buffer(), "/status");
    }

    #[test]
    fn a_bare_question_mark_shows_keybindings_instead_of_typing() {
        // Claude Code's convention: a lone `?` on an empty line is the
        // keybindings shortcut, not the start of a message or a command-menu
        // filter, so it is consumed rather than inserted.
        let mut editor = InputEditor::new();
        assert_eq!(
            editor.handle(key(KeyCode::Char('?'))),
            InputAction::ShowKeybindings
        );
        assert!(
            editor.buffer().is_empty(),
            "the `?` was consumed, not typed"
        );
        assert!(editor.completion().is_none(), "no menu opens");
    }

    #[test]
    fn esc_dismisses_the_menu_without_touching_the_buffer() {
        let mut editor = InputEditor::new();
        editor.handle(key(KeyCode::Char('/')));
        assert!(editor.completion().is_some(), "menu opens on a bare slash");
        assert_eq!(editor.handle(key(KeyCode::Esc)), InputAction::Edited);
        assert!(
            editor.completion().is_none(),
            "Esc closes the menu the buffer would otherwise still show"
        );
        assert_eq!(editor.buffer(), "/", "Esc does not touch what was typed");
    }

    #[test]
    fn typing_after_a_dismissal_can_reopen_the_menu() {
        let mut editor = InputEditor::new();
        editor.handle(key(KeyCode::Char('/')));
        editor.handle(key(KeyCode::Esc));
        assert!(editor.completion().is_none());
        editor.handle(key(KeyCode::Char('h')));
        assert!(
            editor.completion().is_some(),
            "a further edit un-dismisses the menu"
        );
    }

    #[test]
    fn entering_login_from_the_command_menu_opens_the_provider_menu_instead_of_submitting() {
        // The exact sequence the review named: typing `/login` and pressing
        // Enter (not Tab) used to submit the bare command immediately, since
        // Enter on an open menu both fills and submits -- landing on the
        // "usage: /login <provider> <token>" error before the provider menu
        // was ever seen.
        let mut editor = InputEditor::new();
        for ch in "/login".chars() {
            editor.handle(key(KeyCode::Char(ch)));
        }
        assert_eq!(
            editor.handle(key(KeyCode::Enter)),
            InputAction::Edited,
            "an incomplete `/login` must not submit"
        );
        assert_eq!(editor.buffer(), "/login ");
        let menu = editor
            .completion()
            .expect("the provider menu opens right after Enter");
        assert_eq!(menu.kind, CompletionKind::LoginProvider);
    }

    #[test]
    fn entering_a_provider_from_its_menu_submits_the_login_line() {
        // Accepting a provider row is a complete line now: `/login
        // <provider>` with no token opens the masked key entry downstream,
        // so Enter submits it instead of sitting open waiting for a
        // clear-text token that would land in recallable history.
        let mut editor = InputEditor::new();
        for ch in "/login anth".chars() {
            editor.handle(key(KeyCode::Char(ch)));
        }
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        assert_eq!(editor.take(), "/login anthropic");
    }

    #[test]
    fn login_provider_menu_lists_every_provider_before_any_letter_is_typed() {
        let mut editor = InputEditor::new();
        for ch in "/login ".chars() {
            editor.handle(key(KeyCode::Char(ch)));
        }
        let menu = editor
            .completion()
            .expect("the trailing space alone should open the provider menu");
        assert_eq!(menu.kind, CompletionKind::LoginProvider);
        let names: Vec<&str> = menu.matches.iter().map(|row| row.name.as_ref()).collect();
        let expected: Vec<&str> = PROFILES.iter().map(|profile| profile.id.as_str()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn login_provider_argument_completes_from_the_registry() {
        let mut editor = InputEditor::new();
        for ch in "/login anth".chars() {
            editor.handle(key(KeyCode::Char(ch)));
        }
        let menu = editor.completion().expect("anth matches anthropic");
        assert_eq!(menu.kind, CompletionKind::LoginProvider);
        assert_eq!(menu.matches.len(), 1);
        assert_eq!(menu.matches[0].name, "anthropic");
        editor.handle(key(KeyCode::Tab));
        assert_eq!(editor.buffer(), "/login anthropic");
        // A second space starts the token argument, closing the provider menu.
        editor.handle(key(KeyCode::Char(' ')));
        assert!(
            editor.completion().is_none(),
            "the provider menu only owns the first argument"
        );
    }

    #[test]
    fn enter_on_an_open_menu_runs_the_selected_command() {
        let mut editor = InputEditor::new();
        editor.handle(key(KeyCode::Char('/')));
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Submit);
        assert_eq!(editor.take(), "/help");
    }

    #[test]
    fn the_menu_intercepts_up_down_instead_of_history() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "first");
        editor.handle(key(KeyCode::Enter));
        editor.take();
        type_str(&mut editor, "/");
        // With the menu open, Up moves the selection, not history.
        editor.handle(key(KeyCode::Up));
        assert_eq!(editor.buffer(), "/", "history was not recalled");
        assert_eq!(editor.completion().unwrap().selected, COMMANDS.len() - 1);
    }

    #[test]
    fn ctrl_n_and_p_navigate_the_command_menu() {
        let mut editor = InputEditor::new();
        editor.handle(key(KeyCode::Char('/')));
        assert_eq!(editor.completion().unwrap().selected, 0);
        // Ctrl-N moves down the menu (like Down); the buffer is unchanged.
        editor.handle(ctrl('n'));
        assert_eq!(editor.completion().unwrap().selected, 1);
        assert_eq!(editor.buffer(), "/");
        // Ctrl-P moves back up (like Up).
        editor.handle(ctrl('p'));
        assert_eq!(editor.completion().unwrap().selected, 0);
    }

    #[test]
    fn ctrl_n_and_p_recall_history_when_no_menu() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "first");
        editor.handle(key(KeyCode::Enter));
        editor.take();
        // No slash menu: Ctrl-P recalls the most recent submission.
        editor.handle(ctrl('p'));
        assert_eq!(editor.buffer(), "first");
        // Ctrl-N returns to a fresh empty line.
        editor.handle(ctrl('n'));
        assert!(editor.buffer().is_empty());
    }

    #[test]
    fn emacs_chords_move_edit_kill_and_yank() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "hello");
        // Ctrl-A / Ctrl-E move to the line start / end.
        editor.handle(ctrl('a'));
        assert_eq!(editor.cursor_column(), 0);
        editor.handle(ctrl('e'));
        // Ctrl-U kills from the cursor back to the line start; Ctrl-Y yanks it.
        editor.handle(ctrl('u'));
        assert_eq!(editor.buffer(), "");
        editor.handle(ctrl('y'));
        assert_eq!(editor.buffer(), "hello");
        // Ctrl-K kills to the end.
        editor.handle(ctrl('a'));
        editor.handle(ctrl('k'));
        assert_eq!(editor.buffer(), "");
    }

    #[test]
    fn ctrl_w_kills_the_word_before_the_cursor() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "foo bar baz");
        editor.handle(ctrl('w'));
        assert_eq!(editor.buffer(), "foo bar ");
        editor.handle(ctrl('w'));
        assert_eq!(editor.buffer(), "foo ");
        // The killed word is on the kill ring for Ctrl-Y.
        editor.handle(ctrl('y'));
        assert_eq!(editor.buffer(), "foo bar ");
    }

    #[test]
    fn ctrl_t_transposes_the_two_chars_before_the_cursor() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "ab");
        editor.handle(ctrl('t'));
        assert_eq!(editor.buffer(), "ba");
    }

    #[test]
    fn ctrl_b_and_ctrl_f_move_the_caret_one_char() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "abc");
        editor.handle(ctrl('b'));
        assert_eq!(editor.cursor_column(), 2);
        editor.handle(ctrl('a'));
        editor.handle(ctrl('f'));
        assert_eq!(editor.cursor_column(), 1);
    }

    #[test]
    fn ctrl_d_deletes_forward_and_ctrl_h_backspaces() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "abc");
        editor.handle(ctrl('a'));
        assert_eq!(editor.handle(ctrl('d')), InputAction::Edited);
        assert_eq!(editor.buffer(), "bc");
        editor.handle(ctrl('e'));
        editor.handle(ctrl('h'));
        assert_eq!(editor.buffer(), "b");
    }

    #[test]
    fn custom_keybinding_overrides_default_behavior() {
        let mut keymap = crate::keybindings::Keymap::new();
        keymap.bind(
            KeyModifiers::CONTROL,
            KeyCode::Char('h'),
            crate::keybindings::KeyAction::CursorLineStart,
        );

        let mut editor = InputEditor::new().with_keymap(keymap);
        type_str(&mut editor, "hello");
        editor.handle(ctrl('h'));
        assert_eq!(editor.cursor_column(), 0);
    }

    #[test]
    fn default_edit_mode_is_emacs() {
        let editor = InputEditor::new();
        assert_eq!(editor.edit_mode(), EditMode::Emacs);
    }

    #[test]
    fn vim_mode_normal_and_insert_transitions() {
        let mut editor = InputEditor::new().with_edit_mode(EditMode::VimNormal);
        assert_eq!(editor.edit_mode(), EditMode::VimNormal);

        // 'i' transitions to Insert mode
        editor.handle(key(KeyCode::Char('i')));
        assert_eq!(editor.edit_mode(), EditMode::VimInsert);

        // Typing inserts text in Insert mode
        type_str(&mut editor, "hello world");
        assert_eq!(editor.buffer(), "hello world");

        // Esc transitions back to Normal mode
        editor.handle(key(KeyCode::Esc));
        assert_eq!(editor.edit_mode(), EditMode::VimNormal);

        // Normal mode motions: '0' jumps to line start, '$' jumps to end
        editor.handle(key(KeyCode::Char('0')));
        assert_eq!(editor.cursor_column(), 0);
        editor.handle(key(KeyCode::Char('$')));
        assert_eq!(editor.cursor_column(), 11);

        // 'b' jumps back one word, 'w' jumps forward
        editor.handle(key(KeyCode::Char('b')));
        assert_eq!(editor.cursor_column(), 6); // start of "world"
        editor.handle(key(KeyCode::Char('w')));
        assert_eq!(editor.cursor_column(), 11);

        // 'x' deletes char under cursor
        editor.handle(key(KeyCode::Char('0')));
        editor.handle(key(KeyCode::Char('x')));
        assert_eq!(editor.buffer(), "ello world");

        // 'dw' deletes word forward
        editor.handle(key(KeyCode::Char('d')));
        editor.handle(key(KeyCode::Char('w')));
        assert_eq!(editor.buffer(), "world");

        // 'dd' deletes line
        editor.handle(key(KeyCode::Char('d')));
        editor.handle(key(KeyCode::Char('d')));
        assert_eq!(editor.buffer(), "");

        // 'p' pastes last kill
        editor.handle(key(KeyCode::Char('p')));
        assert_eq!(editor.buffer(), "world");
    }

    #[test]
    fn emacs_mode_word_motions_and_deletions() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "hello world test");
        assert_eq!(editor.cursor_column(), 16);

        // Alt-B moves backward one word
        editor.handle(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(editor.cursor_column(), 12); // start of "test"

        editor.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(editor.cursor_column(), 6); // start of "world"

        // Alt-F moves forward one word
        editor.handle(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(editor.cursor_column(), 11); // end of "world"

        editor.handle(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(editor.cursor_column(), 16); // end of "test"

        // Alt-Backspace / Ctrl-Backspace kills word backward
        editor.handle(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(editor.buffer(), "hello world ");

        // Alt-D kills word forward
        editor.handle(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)); // start of line
        assert_eq!(editor.cursor_column(), 0);
        editor.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(editor.buffer(), " world ");

        // Ctrl-Y yanks back last kill ("hello")
        editor.handle(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(editor.buffer(), "hello world ");
    }

    #[test]
    fn emacs_mode_case_modifications_and_transposition() {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "hello world");
        editor.handle(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)); // cursor at 0

        // Alt-U uppercases word forward
        editor.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT));
        assert_eq!(editor.buffer(), "HELLO world");
        assert_eq!(editor.cursor_column(), 5);

        // Alt-L lowercases word forward
        editor.handle(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        editor.handle(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT));
        assert_eq!(editor.buffer(), "hello world");

        // Alt-C capitalizes word forward
        editor.handle(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        editor.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT));
        assert_eq!(editor.buffer(), "Hello world");

        // Alt-T transposes words
        editor.handle(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)); // cursor at end
        editor.handle(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(editor.buffer(), "world Hello");

        // Ctrl-T transposes characters
        editor.handle(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(editor.buffer(), "world Helol");
    }

    #[test]
    fn shift_tab_switches_mode() {
        let mut editor = InputEditor::new();
        let action = editor.handle(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(action, InputAction::SwitchMode);

        let action2 = editor.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(action2, InputAction::SwitchMode);
    }

    #[test]
    fn a_partial_command_never_runs_one_that_discards_work() {
        // `/u` highlighted `/undo` ahead of `/usage`, and Enter reverted the
        // workspace. Completing it is fine; running it needs a second Enter.
        let mut editor = InputEditor::new();
        type_str(&mut editor, "/u");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Edited);
        assert_eq!(editor.buffer(), "/undo");
    }

    #[test]
    fn enter_in_the_file_menu_completes_the_path_without_sending() {
        // Completion walks the working directory, which under `cargo test` is
        // this crate's root.
        let mut editor = InputEditor::new();
        type_str(&mut editor, "see @Cargo.to");
        assert_eq!(editor.handle(key(KeyCode::Enter)), InputAction::Edited);
        assert!(
            editor.buffer().contains("Cargo.toml"),
            "{}",
            editor.buffer()
        );
    }
}
