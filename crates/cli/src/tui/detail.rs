//! The detail pager: Ctrl+O over the selected transcript item.
//!
//! Its own module because it is a second, modal input surface. While it is open
//! the driver routes keys here instead of to the input editor, and the motions it
//! accepts are the pager's (line, half-page, page, top, bottom) under both
//! keymaps, which is a different mapping problem from editing a line of text.

use std::error::Error;

use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::DriverCx;
use crate::tui::state::{Detail, TuiState};

// ========================================================================
// Data Structures
// ========================================================================

/// Whether a detail-pager keystroke asked to leave the pager.
enum DetailKey {
    Stay,
    Exit,
}

// ========================================================================
// Functions
// ========================================================================

/// Flip the detail pager on or off. Opening pins it to the newest content (the
/// bottom of the transcript), the view a streaming turn or an idle prompt was
/// just showing; closing returns control to whichever loop toggled it.
pub(super) fn toggle_detail(cx: &mut DriverCx<'_>) {
    if cx.state.detail.is_some() {
        cx.state.detail = None;
        return;
    }
    let (height, width) = cx
        .terminal
        .size()
        .map(|rect| (rect.height, rect.width))
        .unwrap_or((1, 1));
    let offset = detail_max_offset(cx.state, height, width);
    cx.state.detail = Some(Detail {
        offset,
        pending_g: false,
    });
}

/// The largest scroll offset that still leaves the viewport full: the wrapped
/// line count minus the pager's body height (one row reserved for the footer).
/// Reused by [`toggle_detail`] (open at the bottom) and [`handle_detail_key`]
/// (clamp motions), so the two agree on where "the bottom" is.
fn detail_max_offset(state: &TuiState, height: u16, width: u16) -> usize {
    // The expanded rendering, because that is what the pager draws: computing
    // the bottom against the collapsed height would open the pager short of
    // the real last row and clamp scrolls against a stale maximum.
    let text = crate::tui::view::transcript_text_expanded(&state.transcript, state.streaming);
    let body = height.saturating_sub(1) as usize;
    crate::tui::view::wrapped_height(&text, width).saturating_sub(body)
}

/// One key in an open detail pager: read the viewport size, delegate the offset
/// math to the terminal-free [`detail_motion`], and close the pager when it
/// returns [`DetailKey::Exit`].
pub(super) async fn handle_detail_key(
    cx: &mut DriverCx<'_>,
    key: &ratatui::crossterm::event::KeyEvent,
) -> Result<(), Box<dyn Error>> {
    let (height, width) = cx
        .terminal
        .size()
        .map(|rect| (rect.height, rect.width))
        .unwrap_or((1, 1));
    let body = height.saturating_sub(1) as usize;
    let max = detail_max_offset(cx.state, height, width);
    let Some(detail) = cx.state.detail.as_mut() else {
        return Ok(());
    };
    if matches!(detail_motion(detail, key, body, max), DetailKey::Exit) {
        cx.state.detail = None;
    }
    Ok(())
}

/// Move an open pager by `delta` rows, clamped to its content: the mouse
/// wheel's way in, which has no key to hand [`detail_motion`].
pub(super) fn scroll_detail(cx: &mut DriverCx<'_>, delta: i64) {
    let (height, width) = cx
        .terminal
        .size()
        .map(|rect| (rect.height, rect.width))
        .unwrap_or((1, 1));
    let max = detail_max_offset(cx.state, height, width);
    if let Some(detail) = cx.state.detail.as_mut() {
        detail.offset = (detail.offset as i64 + delta).clamp(0, max as i64) as usize;
    }
}

/// What a key does to an open pager, free of any terminal or driver state so
/// the motion table is unit-testable. Vim's Normal-mode set only: motions never
/// mutate the transcript, so browsing is always as safe as a read-only view.
fn detail_motion(
    detail: &mut Detail,
    key: &ratatui::crossterm::event::KeyEvent,
    body: usize,
    max: usize,
) -> DetailKey {
    // q and Esc leave (Ctrl+O, the primary toggle, is handled by the caller).
    if !key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
    {
        return DetailKey::Exit;
    }
    // `gg` / `G`: the two-key `g` prefix is tracked on `Detail` so the footer
    // can show a half-typed prefix the instant the first `g` lands.
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('g') => {
                if detail.pending_g {
                    detail.pending_g = false;
                    detail.offset = 0;
                } else {
                    detail.pending_g = true;
                }
                return DetailKey::Stay;
            }
            KeyCode::Char('G') => {
                detail.pending_g = false;
                detail.offset = max;
                return DetailKey::Stay;
            }
            _ => {}
        }
    }
    // Any other key abandons a half-typed `gg`.
    detail.pending_g = false;
    let half = body / 2;
    let delta: Option<i64> = if key.modifiers.contains(KeyModifiers::CONTROL) {
        // The vim scroll chords only (the navigation skill's Normal-mode Ctrl
        // set); a bare Ctrl+letter outside this set is inert here, never an
        // edit chord, since the pager is not an editable surface.
        match key.code {
            KeyCode::Char('d') => Some(half as i64),
            KeyCode::Char('u') => Some(-(half as i64)),
            KeyCode::Char('f') => Some(body as i64),
            KeyCode::Char('b') => Some(-(body as i64)),
            KeyCode::Char('e') => Some(1),
            KeyCode::Char('y') => Some(-1),
            _ => None,
        }
    } else {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => Some(1),
            KeyCode::Char('k') | KeyCode::Up => Some(-1),
            KeyCode::PageDown => Some(body as i64),
            KeyCode::PageUp => Some(-(body as i64)),
            KeyCode::Home => {
                detail.offset = 0;
                return DetailKey::Stay;
            }
            KeyCode::End => {
                detail.offset = max;
                return DetailKey::Stay;
            }
            _ => None,
        }
    };
    if let Some(delta) = delta {
        detail.offset = (detail.offset as i64 + delta).clamp(0, max as i64) as usize;
    }
    DetailKey::Stay
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyEvent;

    use super::*;

    #[test]
    fn detail_motions_move_half_page_and_jump() {
        // The motion table is the part worth pinning: line, half/full page,
        // and the two-key `gg`/`G` jumps, all clamped to [0, max]. A regression
        // here is a pager that scrolls past the end or dead-keys a motion.
        let max = 100;
        let body = 10;
        let mut d = Detail {
            offset: 50,
            pending_g: false,
        };
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            body,
            max,
        );
        assert_eq!(d.offset, 51);
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            body,
            max,
        );
        assert_eq!(d.offset, 50);
        // Ctrl-D / Ctrl-U move half a page (half of body 10 is 5).
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            body,
            max,
        );
        assert_eq!(d.offset, 55);
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            body,
            max,
        );
        assert_eq!(d.offset, 50);
        // G jumps to the bottom; gg (two keys) to the top.
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
            body,
            max,
        );
        assert_eq!(d.offset, max);
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            body,
            max,
        );
        assert!(d.pending_g, "the first g is held as a prefix");
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            body,
            max,
        );
        assert_eq!(d.offset, 0);
        assert!(!d.pending_g);
    }

    #[test]
    fn detail_motion_clamps_at_zero_and_max() {
        let mut d = Detail {
            offset: 0,
            pending_g: false,
        };
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            10,
            100,
        );
        assert_eq!(d.offset, 0, "k at the top stays at the top");
        d.offset = 100;
        detail_motion(
            &mut d,
            &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            10,
            100,
        );
        assert_eq!(d.offset, 100, "j past the bottom clamps to max");
    }

    #[test]
    fn detail_motion_exits_on_q_and_esc_and_ignores_unknown_keys() {
        let mut d = Detail {
            offset: 5,
            pending_g: false,
        };
        assert!(matches!(
            detail_motion(
                &mut d,
                &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                10,
                100
            ),
            super::DetailKey::Exit
        ));
        assert!(matches!(
            detail_motion(
                &mut d,
                &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                10,
                100
            ),
            super::DetailKey::Exit
        ));
        // An unmapped key is inert, and clears a half-typed prefix rather than
        // leaving a stale `g` that the next key would complete by accident.
        d.pending_g = true;
        assert!(matches!(
            detail_motion(
                &mut d,
                &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                10,
                100
            ),
            super::DetailKey::Stay
        ));
        assert!(!d.pending_g, "an unrelated key abandons a half-typed gg");
        assert_eq!(d.offset, 5, "offset untouched by an inert key");
    }
}
