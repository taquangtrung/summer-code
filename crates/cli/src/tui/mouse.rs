//! The mouse wheel: scroll whatever region the pointer is over.
//!
//! Only the wheel is wanted, but no terminal protocol reports the wheel alone.
//! The narrowest mode that does is X10-compatible button tracking (`?1000`),
//! which also reports clicks, so while it is on a plain drag no longer selects
//! text and Shift+drag does instead, the bypass every mainstream terminal
//! emulator offers. Motion tracking (`?1002`/`?1003`, which crossterm's
//! `EnableMouseCapture` also turns on) is left off: every pointer move would
//! wake the driver and redraw the frame for an event nothing here reads.

use std::fmt;

use ratatui::crossterm::Command;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::tui::DriverCx;
use crate::tui::detail::scroll_detail;
use crate::tui::view::input_area;

// ========================================================================
// Constants
// ========================================================================

/// Transcript rows one wheel notch moves, the step most terminals and editors
/// use for a notch of their own scrollback.
const WHEEL_ROWS: usize = 3;

/// Button tracking on, reported in SGR encoding (`?1006`), which carries
/// columns and rows past 223 that the legacy encoding cannot.
const ENABLE_WHEEL: &str = "\x1b[?1000h\x1b[?1006h";
const DISABLE_WHEEL: &str = "\x1b[?1006l\x1b[?1000l";

// ========================================================================
// Data Structures
// ========================================================================

/// Turn on the wheel reporting [`wheel`] reads.
pub(super) struct EnableWheel;

/// Give the mouse back to the terminal, undoing [`EnableWheel`].
pub(super) struct DisableWheel;

/// The region under the pointer.
#[derive(Debug, PartialEq, Eq)]
enum Region {
    /// The prompt: a notch moves its open menu's selection or its cursor a
    /// line (see [`InputEditor::wheel`]), and scrolls the transcript when there
    /// is neither to move.
    ///
    /// [`InputEditor::wheel`]: crate::tui::input::InputEditor::wheel
    Prompt,
    /// The conversation above the prompt.
    Transcript,
}

/// One wheel notch: where the pointer was, and which way it turned.
#[derive(Debug, PartialEq, Eq)]
struct Scroll {
    region: Region,
    up: bool,
}

// ========================================================================
// Command Implementations
// ========================================================================

impl Command for EnableWheel {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str(ENABLE_WHEEL)
    }

    /// The Windows console reports the mouse through its input records rather
    /// than escape sequences, so crossterm's own capture is the only switch
    /// there; it has no wheel-only mode to narrow it to.
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        ratatui::crossterm::event::EnableMouseCapture.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

impl Command for DisableWheel {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str(DISABLE_WHEEL)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        ratatui::crossterm::event::DisableMouseCapture.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Scroll the region under the pointer by one wheel notch. Any other mouse
/// event (a click, a release) is ignored.
///
/// The open detail pager fills the window, so it takes every notch wherever
/// the pointer is. An open API-key entry takes none: the prompt under it is
/// not what is being typed, and Up there would recall history into it.
pub(super) fn wheel(cx: &mut DriverCx<'_>, event: &MouseEvent) {
    if cx.state.pending_secret.is_some() {
        return;
    }
    let frame = match cx.terminal.size() {
        Ok(size) => ratatui::layout::Rect::new(0, 0, size.width, size.height),
        Err(_) => return,
    };
    let Some(Scroll { region, up }) = scroll_for(event, input_area(frame, cx.input).y) else {
        return;
    };
    if cx.state.detail.is_some() {
        let rows = WHEEL_ROWS as i64;
        scroll_detail(cx, if up { -rows } else { rows });
        return;
    }
    if region == Region::Prompt && cx.input.wheel(up) {
        return;
    }
    cx.state.scroll_up = if up {
        cx.state.scroll_up + WHEEL_ROWS
    } else {
        cx.state.scroll_up.saturating_sub(WHEEL_ROWS)
    };
}

/// A wheel notch as the arrow key that moves a list's selection the same way,
/// for the pickers, which own every key while they are open and so are moved
/// by keys rather than scrolled.
pub(super) fn wheel_key(event: &MouseEvent) -> Option<KeyEvent> {
    let code = match event.kind {
        MouseEventKind::ScrollUp => KeyCode::Up,
        MouseEventKind::ScrollDown => KeyCode::Down,
        _ => return None,
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Which region a wheel event scrolls, given the first row of the input box:
/// the prompt from there down (its rules and the status footer under it
/// included), the transcript above it. `None` for anything but the wheel.
fn scroll_for(event: &MouseEvent, input_top: u16) -> Option<Scroll> {
    let up = match event.kind {
        MouseEventKind::ScrollUp => true,
        MouseEventKind::ScrollDown => false,
        _ => return None,
    };
    let region = if event.row >= input_top {
        Region::Prompt
    } else {
        Region::Transcript
    };
    Some(Scroll { region, up })
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::MouseButton;

    use super::*;

    fn at_row(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn the_wheel_scrolls_the_region_under_the_pointer() {
        let input_top = 20;
        assert_eq!(
            scroll_for(&at_row(MouseEventKind::ScrollUp, 19), input_top),
            Some(Scroll {
                region: Region::Transcript,
                up: true
            }),
            "the row above the input box is the transcript's"
        );
        assert_eq!(
            scroll_for(&at_row(MouseEventKind::ScrollDown, 20), input_top),
            Some(Scroll {
                region: Region::Prompt,
                up: false
            }),
            "the input box's top rule is the prompt's"
        );
    }

    #[test]
    fn a_click_scrolls_nothing() {
        let click = at_row(MouseEventKind::Down(MouseButton::Left), 5);
        assert_eq!(scroll_for(&click, 20), None);
    }
}
