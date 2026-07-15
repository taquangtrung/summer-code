//! The console: the one input source the REPL and the permission prompt share.
//!
//! At a terminal both draw from stdin, and that sharing is the whole reason the
//! deadlock existed. The old loop held a `StdinLock` for its entire life (via
//! `.lines()`), and a turn, mid-flight, asked a permission question whose answer
//! `TerminalPrompter` reads from stdin on a blocking thread: `StdinLock` is
//! reentrant only for the thread that holds it, so the prompt waited forever on
//! a lock the loop would not release until the turn it was blocked on returned.
//!
//! Making the reader a seam fixes it two ways. Each `read_line` acquires and
//! releases, so no lock spans a turn and the reentrancy cannot recur. And a test
//! can drive the exact interleaving that reproduces the bug (a command, then the
//! `y` that answers the prompt it triggers) without a pty.

use std::io;

// ========================================================================
// Traits
// ========================================================================

/// A line source shared by the REPL and the permission prompt.
///
/// `read_line` returns `None` at end of input. `Send + Sync` because the prompt
/// reads it from a `spawn_blocking` thread while the REPL reads it from the main
/// task, which is the same contention that, over a held lock, was the deadlock.
pub trait Console: Send + Sync {
    fn read_line(&self) -> io::Result<Option<String>>;

    /// Read a secret (an API key) in answer to `prompt`, without echoing the
    /// characters. Returns `None` when the entry was cancelled (Esc, Ctrl-C)
    /// or input ended. The default falls through to [`Console::read_line`]:
    /// masking is something only a terminal can do, and a scripted or piped
    /// console has nothing to hide.
    fn read_secret(&self, prompt: &str) -> io::Result<Option<String>> {
        let _ = prompt;
        self.read_line()
    }
}

// ========================================================================
// Data Structures
// ========================================================================

/// Standard input, one lock acquired and released per read.
pub struct Stdin;

// ========================================================================
// Stdin Implementations
// ========================================================================

impl Console for Stdin {
    fn read_line(&self) -> io::Result<Option<String>> {
        let mut line = String::new();
        // `Stdin::read_line` locks for the call and unlocks on return, unlike a
        // `StdinLock` kept in a loop variable. That difference is the fix.
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }

    /// Masked entry at a terminal: raw mode, one `*` echoed per typed
    /// character, Enter to finish, Backspace to delete, Esc or Ctrl-C to
    /// cancel. A piped stdin has no keyboard to hide anything from, so there
    /// the prompt is printed and the line read (echoed) instead.
    ///
    /// crossterm through `ratatui`, per the workspace rule: the CLI keeps one
    /// crossterm version, the one the TUI backend builds against.
    fn read_secret(&self, prompt: &str) -> io::Result<Option<String>> {
        use std::io::Write;

        use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
        use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use ratatui::crossterm::tty::IsTty;

        if !io::stdin().is_tty() {
            print!("{prompt}");
            io::stdout().flush()?;
            return self.read_line();
        }
        enable_raw_mode()?;
        let answer = (|| {
            let mut out = io::stdout();
            let mut secret = String::new();
            write!(out, "{prompt}")?;
            out.flush()?;
            loop {
                let event = read().map_err(io::Error::other)?;
                let Event::Key(key) = event else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Enter => {
                        writeln!(out)?;
                        return Ok(Some(secret));
                    }
                    KeyCode::Esc => {
                        writeln!(out)?;
                        return Ok(None);
                    }
                    // Cancel, not EOF: the caller reports a cancelled login,
                    // the same answer Esc gives, so one message covers both.
                    KeyCode::Char('c') if ctrl => {
                        writeln!(out)?;
                        return Ok(None);
                    }
                    // Ctrl-D on an empty buffer is EOF, the read_line convention.
                    KeyCode::Char('d') if ctrl && secret.is_empty() => {
                        writeln!(out)?;
                        return Ok(None);
                    }
                    KeyCode::Backspace => {
                        if secret.pop().is_some() {
                            write!(out, "\u{8} \u{8}")?;
                        }
                    }
                    // Any other control chord is a line edit this reader does
                    // not have; swallow it rather than injecting its letter.
                    KeyCode::Char(ch) if !ctrl => {
                        secret.push(ch);
                        write!(out, "*")?;
                    }
                    _ => {}
                }
                out.flush()?;
            }
        })();
        disable_raw_mode()?;
        answer
    }
}
