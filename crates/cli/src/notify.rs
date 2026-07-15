//! Turn-completion notification: a terminal bell plus an OSC 9 desktop
//! notification, so a person who has alt-tabbed away does not have to keep
//! refocusing the terminal on a guess to see whether a turn finished.
//!
//! Both halves are inert where they are not understood. A terminal that does
//! not parse OSC 9 still consumes `ESC ] 9 ; ... BEL` as one unrecognized,
//! silently-dropped control sequence (standard OSC handling), which is why a
//! second, bare BEL follows it: the guaranteed fallback signal (a beep, or
//! whatever the terminal maps BEL to) for a terminal that never gets past the
//! first one.

use std::io::Write;

// ========================================================================
// Functions
// ========================================================================

/// Ring the terminal bell and request an OSC 9 desktop notification carrying
/// `message`.
///
/// `message` must be caller-controlled and short: it is embedded directly in
/// the OSC payload with no escaping, so it must never carry raw model or user
/// text.
pub fn ring(out: &mut impl Write, message: &str) -> std::io::Result<()> {
    write!(out, "\x1b]9;{message}\x07\x07")?;
    out.flush()
}
