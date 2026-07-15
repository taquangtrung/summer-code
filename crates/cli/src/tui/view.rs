//! View state -> ratatui widgets.
//!
//! [`view`] renders the fullscreen frame each redraw in the Claude Code / Pi
//! shape: the transcript fills the window above an input box bounded by top and
//! bottom rules, with a two-line status footer (the statusline, then the
//! editing mode) pinned to the very bottom. [`transcript_text`] stitches
//! [`transcript_text_expanded`] (the pager's rendering) share [`item_text_with`],
//! which renders one [`Item`] with or without forced expansion.
//!
//! Snapshot-tested against ratatui's `TestBackend`: the terminal is untestable,
//! so the one pure function that turns state into pixels is what gets the test.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::input::{CompletionKind, EditMode, InputEditor};
use crate::tui::markdown::{
    TAB_AS_SPACES, highlight_diff_line, render_markdown_cached, render_markdown_live,
};
use crate::tui::state::{
    Item, PickerStage, PlanDisplay, PlanProgress, SecretInput, ToolStatus, ToolView, TuiState,
    format_age, truncate,
};

// ========================================================================
// Constants
// ========================================================================

/// Maximum characters to show in the active session's preview string in the picker.
const ACTIVE_SESSION_PREVIEW_LIMIT: usize = 40;

/// The tallest the input box's content area grows before it scrolls internally.
///
/// A pasted stack trace is fifty lines, and an input box that grew to fit it
/// would leave no transcript to paste it *about*. Past this the box shows the
/// window ending at the cursor.
const MAX_INPUT_ROWS: u16 = 10;

/// The tallest the floating message banner grows before it clips. Generous
/// enough for a typical `/policy` or `/status` dump; past this the oldest
/// visible lines scroll out of the banner, though they stay in
/// `pending_messages` until `/clear`.
const MAX_MESSAGE_ROWS: u16 = 15;

/// The most diff lines a permission prompt draws under its question. The
/// options sit below the diff, so an uncapped diff for a new thousand-line file
/// would push the question itself off the top of the transcript.
const MAX_PROMPT_DIFF_LINES: usize = 30;

/// The most body lines a tool item shows in the transcript. The rest are a
/// count and a pointer to the Ctrl+O pager, which shows them all.
const MAX_INLINE_BODY_LINES: usize = 20;

/// How full, as a share of the compaction point, the context gets before the
/// transcript warns about it.
const CONTEXT_WARNING_PERCENT: u64 = 80;

const PERCENT: u64 = 100;

/// The keys the welcome screen names, two rows of four: the ones a first
/// session needs, not the cheat sheet `?` shows.
const WELCOME_TIPS: &[&[(&str, &str)]] = &[
    &[
        ("/help", "commands"),
        ("?", "shortcuts"),
        ("@", "attach a file"),
        ("!", "run a shell command"),
    ],
    &[
        ("Shift+Tab", "mode"),
        ("Ctrl+R", "history"),
        ("Ctrl+O", "details"),
        ("Ctrl+G", "edit in $EDITOR"),
    ],
];

/// The tool whose results are diffs, and which the transcript shows unfolded.
pub(crate) const EDIT_TOOL: &str = "edit";

// ========================================================================
// Data Structures
// ========================================================================

/// Where each region of the frame sits.
///
/// One function computes it so the render and the scroll clamp cannot disagree
/// about how tall the transcript is, which is the sort of drift that shows up as
/// a viewport that scrolls one row short.
struct Areas {
    input: Rect,
    status: Rect,
    transcript: Rect,
}

// ========================================================================
// Functions
// ========================================================================

/// Render the fullscreen frame in the Claude Code / Pi shape: the transcript
/// fills the window above a bordered input box, with a two-line status footer
/// (the statusline, then the editing mode) pinned to the very bottom. The
/// input box holds the editable prompt, a working indicator while a turn
/// streams, or a permission prompt while one is pending.
///
/// Takes the state mutably for one reason: it clamps `scroll_up` against the
/// content height, which is known only here, after wrapping. Doing it in the
/// driver would mean laying the frame out and wrapping the whole transcript
/// twice per keystroke; leaving it unclamped means holding PageUp past the top
/// banks an offset that as many PageDowns have to spend before the view moves.
pub fn view(state: &mut TuiState, input: &InputEditor, frame: &mut Frame) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    // The Ctrl+O detail pager is a full-screen overlay: it replaces the chat
    // layout entirely while it is up, so it never shares a redraw with the
    // input box or the status footer.
    if state.detail.is_some() {
        render_detail(state, area, frame);
        return;
    }
    let areas = Areas::of(area, input);

    if areas.transcript.height > 0 {
        let content = areas.transcript;
        let mut text = if state.transcript.is_empty() && !state.streaming {
            welcome_text(state)
        } else {
            transcript_text(&state.transcript, state.streaming)
        };
        if state.streaming {
            // The live progress line rides the tail of the conversation area,
            // the way Claude Code's spinner does, so "is this still working,
            // and for how long?" reads in the same place as the reply itself.
            let is_thinking = matches!(state.transcript.last(), Some(Item::Thinking { .. }));
            text.lines.push(progress_line(
                state.turn_started_at,
                state.turn_uploaded_tokens,
                state.turn_downloaded_tokens,
                is_thinking,
            ));
            // The plan under the spinner, as Claude Code pins its todo list:
            // where a long turn is, without scrolling back to the last `plan`
            // call to find out.
            if let Some(plan) = &state.plan {
                text.lines.extend(plan_lines(plan));
            }
        }
        // What is waiting to be sent, so a queued message reads as waiting
        // rather than as one that vanished on Enter.
        for queued in &state.queued {
            let first_line = queued.lines().next().unwrap_or_default();
            text.lines.push(Line::from(Span::styled(
                format!("  queued: {first_line}"),
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        if let Some(warning) = context_warning(state) {
            text.lines.push(warning);
        }
        if let Some(prompt) = &state.pending_prompt {
            // Print the question right below the current conversation during reasoning/turns
            text.lines.push(Line::from(Span::styled(
                format!("summer: {}", prompt.description),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            if let Some(diff) = &prompt.diff {
                text.lines.extend(prompt_diff_lines(diff));
            }
            let options = crate::tui::prompt::prompt_options(prompt.offer.as_deref());
            for (idx, (key_label, text_label)) in options.iter().enumerate() {
                let is_selected = idx == prompt.selected;
                if is_selected {
                    text.lines.push(Line::from(vec![
                        Span::styled(
                            "  ● ",
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("[{key_label}] "),
                            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            text_label.clone(),
                            Style::new().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                } else {
                    text.lines.push(Line::from(vec![
                        Span::styled("  ○ ", Style::new().fg(Color::DarkGray)),
                        Span::styled(format!("[{key_label}] "), Style::new().fg(Color::DarkGray)),
                        Span::styled(text_label.clone(), Style::new().fg(Color::Gray)),
                    ]));
                }
            }
        }
        // **Wrapped, because ratatui truncates what it cannot fit.** Model output
        // is typically one long line per paragraph, so on an 80-column terminal
        // everything past column 78 was simply invisible, with nothing on screen
        // to say so. `trim: false` keeps leading indentation, which matters for
        // the code the agent is showing.
        let wrap = Wrap { trim: false };
        // Only the lines that reach the screen are handed to ratatui, counted
        // up from the bottom. Wrapping the whole transcript every frame made
        // each streamed token cost as much as the session was long, and the
        // single scroll offset it needed overflowed `u16` in a long session.
        let window = tail_window(
            &text.lines,
            content.width,
            usize::from(content.height),
            state.scroll_up,
        );
        state.scroll_up = window.scroll_up;
        let visible = Text::from(text.lines.split_off(window.start));
        frame.render_widget(
            Paragraph::new(visible)
                .wrap(wrap)
                .scroll((window.scroll, 0)),
            content,
        );
    }

    render_completion_popup(input, area, areas.input, frame);
    // The acceptance line floats directly above the input box and the banner
    // floats above *it*, so the banner gives back the rows the line took:
    // drawn independently of each other, the two would land on the same row.
    let accepted_rows = u16::from(render_auto_accepted(state, area, areas.input, frame));
    render_pending_message(state, area, areas.input, accepted_rows, frame);
    render_picker_popup(state, input, area, areas.input, frame);

    render_input_box(state, input, areas.input, frame);

    if areas.status.height > 0 {
        frame.render_widget(state.statusline.render(&state.status), areas.status);
    }
}

/// The Ctrl+O detail pager: the same [`transcript_text`] the live view renders,
/// but filling the whole window above a one-line footer, scrolled by an offset
/// vim motions own rather than follow-the-tail. Shares [`transcript_text`] and
/// [`wrapped_height`] with the live view so an item looks identical in both.
fn render_detail(state: &mut TuiState, area: Rect, frame: &mut Frame) {
    // The pager's rendering expands thinking and tool bodies whatever the
    // transcript's own toggles say; see `transcript_text_expanded`.
    let text = transcript_text_expanded(&state.transcript, state.streaming);
    let body_height = area.height.saturating_sub(1);
    let body = Rect {
        height: body_height,
        ..area
    };
    let footer = Rect {
        y: area.y + body_height,
        height: area.height - body_height,
        ..area
    };
    let line_count = wrapped_height(&text, area.width);
    let max = line_count.saturating_sub(body_height as usize);
    // Clamp here, where the wrapped height and viewport are known, so a motion
    // that overshoots (or content that shrank) never leaves the offset past the
    // end. The paragraph then starts at the line holding the offset, so the
    // scroll it is given is under one line's height whatever the offset.
    let detail = state
        .detail
        .as_mut()
        .expect("render_detail is only called with the pager open");
    detail.offset = detail.offset.min(max);
    let (start, scroll) = head_window(&text.lines, area.width, detail.offset);
    let mut text = text;
    let text = Text::from(text.lines.split_off(start));
    let prefix = if detail.pending_g { "g" } else { "" };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        body,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{prefix} detail  j/k Ctrl-D/U Ctrl-F/B gg/G  (Ctrl-O or q to exit)"),
            Style::new().fg(Color::DarkGray),
        ))),
        footer,
    );
}

// ========================================================================
// Areas Implementations
// ========================================================================

impl Areas {
    /// Lay the frame out bottom-up: a two-line status footer (the statusline,
    /// then the editing mode), a bordered input box above it, and the
    /// transcript filling whatever is left.
    fn of(area: Rect, input: &InputEditor) -> Self {
        // Two footer rows when the frame can spare them, one when it cannot:
        // six is the smallest frame that still holds a usable chat — one
        // transcript row, three input rows (two rules and a line), two status
        // rows. Below that the mode line is what goes, not the statusline or
        // the input box's rules: the row with the session facts on it is the
        // one the footer exists for.
        let status_height = if area.height >= 6 { 2 } else { 1 };
        // The prompt doubles as the picker's filter box, so its height is
        // the buffer's wrapped line count either way; the picker list floats above
        // it (see [`render_picker_popup`]), not inside the box. A pending
        // permission prompt renders inline in the transcript right below the
        // current conversation, so the input box keeps its normal size.
        let content_width = (area.width as usize).saturating_sub(2).max(1);
        let (visual_lines, _, _) = wrap_prompt(input.buffer(), input.cursor(), content_width);
        let input_rows = (visual_lines.len() as u16).clamp(1, MAX_INPUT_ROWS);
        // Plus two for the top and bottom rules.
        let input_height = (input_rows + 2).min(area.height.saturating_sub(status_height));
        let transcript_height = area.height.saturating_sub(status_height + input_height);

        Self {
            input: Rect {
                y: area.y + transcript_height,
                height: input_height,
                ..area
            },
            status: Rect {
                y: area.y + transcript_height + input_height,
                height: status_height,
                ..area
            },
            transcript: Rect {
                height: transcript_height,
                ..area
            },
        }
    }
}

/// Render the input box: the editable prompt with the live cursor, or a
/// working indicator while a turn streams. Only a top and bottom rule is drawn
/// (no side borders), so the box reads as an open band; the top rule separates
/// it from the transcript above. Below three rows there is no room for the
/// rules plus a content line, so they are dropped and content renders straight
/// into the area. A pending permission question during reasoning renders
/// directly in the transcript below the conversation.
fn render_input_box(state: &TuiState, input: &InputEditor, area: Rect, frame: &mut Frame) {
    let inner = if area.height >= 3 {
        let block = match input.edit_mode() {
            EditMode::Emacs => Block::new().borders(Borders::TOP | Borders::BOTTOM),
            EditMode::VimNormal => Block::new()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title_top(Line::from(vec![Span::styled(
                    " NORMAL ",
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                )])),
            EditMode::VimInsert => Block::new()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title_top(Line::from(vec![Span::styled(
                    " INSERT ",
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )])),
        };
        let block = match input.search_label() {
            Some(label) => block.title_top(Line::from(Span::styled(
                format!(" {label} "),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))),
            None => block,
        };
        frame.render_widget(block, area);
        area.inner(Margin::new(0, 1))
    } else {
        area
    };
    if inner.height == 0 {
        return;
    }

    if let Some(status) = &state.pending_login {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                status.clone(),
                Style::new().fg(Color::DarkGray),
            ))),
            inner,
        );
    } else if let Some(secret) = &state.pending_secret {
        render_secret_input(secret, inner, frame);
    } else if state.streaming {
        // The progress line lives in the conversation area; the band still
        // renders the (editable) prompt, because keys during a turn compose the
        // next message and Enter queues it — an invisible buffer would read as
        // the keys doing nothing, which is what this replaced.
        render_prompt_lines(input, inner, frame);
    } else {
        render_prompt_lines(input, inner, frame);
    }
}

/// Render the editable buffer, one row per line, and place the terminal cursor.
///
/// Only the first line carries the `> ` marker; continuations are indented to
/// line up under it, so a pasted block reads as one message rather than as
/// several prompts.
fn render_prompt_lines(input: &InputEditor, inner: Rect, frame: &mut Frame) {
    let content_width = (inner.width as usize).saturating_sub(2).max(1);
    let (lines, cursor_row, cursor_col) =
        wrap_prompt(input.buffer(), input.cursor(), content_width);
    let rows = inner.height as usize;
    // The window ends at the cursor's row, so typing at the end of a block
    // longer than the box keeps the caret on screen instead of below it.
    let first = cursor_row.saturating_sub(rows.saturating_sub(1));

    let marker_style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
    let visible: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(index, line)| {
            let marker = if index == 0 { "> " } else { "  " };
            Line::from(vec![
                Span::styled(marker, marker_style),
                Span::raw(line.clone()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Past the marker and the characters before the cursor on its own line, so
    // inline edits land where a person expects.
    // Clamped to the box: a row filled to the last column puts the cursor one
    // cell past it, which drew the caret on the border.
    let right_edge = inner.x + inner.width.saturating_sub(1);
    let col = (inner.x + 2 + cursor_col as u16).min(right_edge);
    let row = inner.y + (cursor_row - first) as u16;
    frame.set_cursor_position(Position::new(col, row));
}

/// Render the masked API-key entry in place of the editable prompt: the
/// provider's label, one `*` per typed character, and the keys that finish
/// the entry. The key itself is never on screen, and the cursor sits just
/// after the mask so the box still reads as a live input.
fn render_secret_input(secret: &SecretInput, inner: Rect, frame: &mut Frame) {
    let prefix = format!("{} API key: ", secret.id.profile().label);
    let masked = secret.masked();
    let marker_style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
    let line = Line::from(vec![
        Span::styled(prefix.clone(), marker_style),
        Span::raw(masked.clone()),
        Span::styled(
            "  (Enter to save, Esc to cancel)",
            Style::new().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
    let col = inner.x + prefix.chars().count() as u16 + masked.chars().count() as u16;
    // A pasted key runs past the box's width; the stars clip at the edge, and
    // the cursor parks there rather than off-frame.
    let col = col.min(inner.x + inner.width.saturating_sub(1));
    frame.set_cursor_position(Position::new(col, inner.y));
}

/// Render the command-completion menu floating just above the input box, when the
/// buffer is a `/`-prefix. The menu overlays the bottom of the transcript (the
/// newest lines are transient while typing); no border, like Claude Code's menu.
fn render_completion_popup(input: &InputEditor, area: Rect, input_area: Rect, frame: &mut Frame) {
    let Some(completion) = input.completion() else {
        return;
    };
    // There must be at least one row above the input box for the menu.
    if input_area.y <= area.y {
        return;
    }
    let available = (input_area.y - area.y) as usize;
    let height = completion.matches.len().min(available) as u16;
    if height == 0 {
        return;
    }
    let popup_area = Rect {
        x: area.x,
        y: input_area.y - height,
        width: area.width,
        height,
    };
    let mut lines: Vec<Line> = Vec::with_capacity(completion.matches.len());
    for (index, row) in completion.matches.iter().enumerate() {
        let selected = index == completion.selected;
        let name_style = if selected {
            Style::new()
                .bg(Color::Blue)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)
        };
        let desc_style = if selected {
            Style::new().bg(Color::Blue).fg(Color::Black)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        // A command name replaces the whole buffer, so it is shown the way it
        // is typed, `/`-prefixed; a provider id fills an argument after
        // `/login `, which already has its own slash.
        let name = match completion.kind {
            CompletionKind::Command => format!("/{}", row.name),
            CompletionKind::LoginProvider => row.name.to_string(),
            CompletionKind::File => format!("@{}", row.name),
        };
        lines.push(Line::from(vec![
            Span::styled(name, name_style),
            Span::styled(format!("  {}", row.description), desc_style),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), popup_area);
}

/// How many terminal rows `text` occupies once word-wrapped to `width`.
///
/// ratatui's own `Paragraph::line_count` is gated behind an unstable feature, so
/// this reimplements the greedy word wrap it uses. It only feeds the tail-scroll
/// offset, so being a row out is cosmetic -- but being *systematically* short is
/// not, which is what using the unwrapped `Text::height` was.
pub(crate) fn wrapped_height(text: &Text<'_>, width: u16) -> usize {
    if width == 0 {
        return text.height();
    }
    text.lines
        .iter()
        .map(|line| wrapped_line_rows(line, width as usize))
        .sum()
}

/// Rows one logical line takes at `width`. An empty line still takes one.
///
/// Measured by ratatui's own wrapping, the code that draws the line. A copy of
/// its rules here counted characters rather than cells, so a reply in Chinese
/// or with emoji wrapped to more rows than were counted and its end could not
/// be scrolled into view.
fn wrapped_line_rows(line: &Line<'_>, width: usize) -> usize {
    let width = u16::try_from(width).unwrap_or(u16::MAX);
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

/// The part of a transcript the live view shows: from line `start`, scrolled
/// down `scroll` rows, with `scroll_up` clamped to what the content allows.
struct TailWindow {
    scroll: u16,
    scroll_up: usize,
    start: usize,
}

/// Find the lines that fill a `height`-row view sitting `scroll_up` rows above
/// the bottom, walking up from the last line and stopping once they are
/// covered. Work is proportional to how far up the view is, not to the length
/// of the transcript.
fn tail_window(lines: &[Line<'_>], width: u16, height: usize, scroll_up: usize) -> TailWindow {
    let wanted = height + scroll_up;
    let mut rows = 0usize;
    let mut start = lines.len();
    while start > 0 && rows < wanted {
        start -= 1;
        rows += wrapped_line_rows(&lines[start], usize::from(width));
    }
    // Ran out of lines first: the view is at the top, so the offset cannot be
    // more than the content above the bottom screenful.
    let scroll_up = scroll_up.min(rows.saturating_sub(height));
    let scroll = rows.saturating_sub(height + scroll_up);
    TailWindow {
        scroll: u16::try_from(scroll).unwrap_or(u16::MAX),
        scroll_up,
        start,
    }
}

/// The line an `offset`-rows-from-the-top view starts in, and the rows into
/// that line it starts at.
fn head_window(lines: &[Line<'_>], width: u16, offset: usize) -> (usize, u16) {
    let mut rows_before = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let rows = wrapped_line_rows(line, usize::from(width));
        if rows_before + rows > offset {
            let into = offset - rows_before;
            return (index, u16::try_from(into).unwrap_or(u16::MAX));
        }
        rows_before += rows;
    }
    (lines.len(), 0)
}

/// The braille spinner Claude Code uses, advanced one frame per
/// [`SPINNER_INTERVAL`] of wall time. The frame is derived from the turn's
/// elapsed time rather than a counter the driver mutates, so it is
/// deterministic and never desyncs from the elapsed seconds shown beside it.
///
/// `pub(crate)`: the plain REPL's live status line (`crate::repl`) redraws the
/// same spinner in place at the terminal cursor, and reuses this and
/// [`SPINNER_INTERVAL`] rather than keeping a second glyph set in sync by hand.
pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(crate) const SPINNER_INTERVAL: Duration = Duration::from_millis(100);

/// The spinner glyph for `elapsed` time into the animation.
pub(crate) fn spinner_frame(elapsed: Duration) -> &'static str {
    let ticks = elapsed.as_millis() / SPINNER_INTERVAL.as_millis();
    SPINNER_FRAMES[ticks as usize % SPINNER_FRAMES.len()]
}

/// The live progress line's text, without the spinner glyph: elapsed time
/// plus real-time token counts (↑ uploaded, ↓ generated), e.g.
/// `"working… 3s (↑1.2k, ↓340)"`.
///
/// Split from [`progress_line`] so the plain REPL's status line can share the
/// exact phrasing without depending on ratatui's `Line`/`Span` types.
pub(crate) fn progress_label(
    elapsed: Duration,
    uploaded_tokens: u64,
    downloaded_tokens: u64,
    is_thinking: bool,
) -> String {
    let prefix = if is_thinking {
        "thinking…"
    } else {
        "working…"
    };
    let mut label = format!("{prefix} {}s", elapsed.as_secs());

    let uploaded_str = if uploaded_tokens > 0 {
        Some(format!("↑{}", format_token_count(uploaded_tokens)))
    } else {
        None
    };

    let downloaded_str = if downloaded_tokens > 0 {
        Some(format!("↓{}", format_token_count(downloaded_tokens)))
    } else {
        None
    };

    match (uploaded_str, downloaded_str) {
        (Some(up), Some(down)) => {
            label.push_str(&format!(" ({up}, {down})"));
        }
        (Some(up), None) => {
            label.push_str(&format!(" ({up})"));
        }
        (None, Some(down)) => {
            label.push_str(&format!(" ({down})"));
        }
        (None, None) => {}
    }
    label
}

/// The live progress line shown at the tail of the conversation while a turn
/// streams: an animated spinner plus the turn's elapsed time and real-time
/// token count (uploaded and generated tokens).
fn progress_line(
    started: Option<Instant>,
    uploaded_tokens: u64,
    downloaded_tokens: u64,
    is_thinking: bool,
) -> Line<'static> {
    let elapsed = started.map(|t| t.elapsed()).unwrap_or_default();
    let glyph = Span::styled(
        format!("{} ", spinner_frame(elapsed)),
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    );
    let label = progress_label(elapsed, uploaded_tokens, downloaded_tokens, is_thinking);
    Line::from(vec![
        glyph,
        Span::styled(label, Style::new().fg(Color::DarkGray)),
    ])
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Stitch every transcript [`Item`]] into the body the view draws. A blank line
/// separates items so a new turn, tool, or error reads as its own block. Shares
/// `item_text` with the render, so an item looks the same the moment it
/// streams and after it closes.
///
/// `streaming` says the last item may still be growing, so a reply there is
/// rendered without the markdown cache; see [`render_markdown_cached`].
pub fn transcript_text(transcript: &[Item], streaming: bool) -> Text<'static> {
    transcript_text_with(transcript, false, streaming)
}

/// The pager's rendering: every thinking block and tool body expanded, no
/// matter how the transcript's own `/thinking` and `/tools` toggles are set.
/// The pager is the surface for *reading the detail*; opening it to inspect a
/// tool call and finding the same collapsed `ⓘ`/`✓` headers as the screen
/// behind would make it a second copy of that screen rather than a way in.
/// A rendering choice, not a state change: closing the pager leaves the
/// transcript's collapse preferences exactly as they were.
pub fn transcript_text_expanded(transcript: &[Item], streaming: bool) -> Text<'static> {
    transcript_text_with(transcript, true, streaming)
}

fn transcript_text_with(transcript: &[Item], expand: bool, streaming: bool) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, item) in transcript.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        let growing = streaming && index + 1 == transcript.len();
        match item {
            Item::Assistant(text) if growing => lines.extend(render_markdown_live(text)),
            _ => lines.extend(item_text_with(item, expand)),
        }
    }
    Text::from(lines)
}

/// Soft-wrap the prompt buffer to `width` columns and compute the visual cursor
/// row and column.
///
/// Returns `(visual_lines, cursor_visual_row, cursor_visual_col)`.
///
/// **Columns are display cells, not characters.** A CJK character or an emoji
/// takes two, a combining mark none, and counting characters wrapped a wide
/// line late (ratatui then cut it at the box edge) and put the cursor
/// half-way back along the text it was meant to follow.
pub(crate) fn wrap_prompt(
    buffer: &str,
    cursor: usize,
    width: usize,
) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    // Each row is an exact slice of the buffer, kept with its byte start, so
    // the cursor maps onto a row by offset alone.
    let mut rows: Vec<(String, usize)> = Vec::new();
    let mut offset = 0;
    for logical in buffer.split('\n') {
        wrap_prompt_line(logical, offset, width, &mut rows);
        offset += logical.len() + 1;
    }

    let mut cursor = cursor.min(buffer.len());
    while !buffer.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let row = rows
        .iter()
        .rposition(|(_, start)| *start <= cursor)
        .unwrap_or(0);
    let (text, start) = &rows[row];
    let mut into = (cursor - start).min(text.len());
    while !text.is_char_boundary(into) {
        into -= 1;
    }
    let column = UnicodeWidthStr::width(&text[..into]);
    (
        rows.into_iter().map(|(text, _)| text).collect(),
        row,
        column,
    )
}

/// Greedy word wrap of one logical line into `rows`, breaking a word wider
/// than the box at the last character that fits.
fn wrap_prompt_line(line: &str, base: usize, width: usize, rows: &mut Vec<(String, usize)>) {
    let mut current = String::new();
    let mut current_start = base;
    let mut used = 0usize;
    let mut position = base;
    for (index, word) in line.split(' ').enumerate() {
        let word_width = UnicodeWidthStr::width(word);
        let word_start = position;
        position += word.len() + 1;
        if index > 0 {
            if used + 1 + word_width <= width {
                current.push(' ');
                current.push_str(word);
                used += 1 + word_width;
                continue;
            }
            rows.push((std::mem::take(&mut current), current_start));
        }
        let mut rest = word;
        let mut rest_start = word_start;
        while UnicodeWidthStr::width(rest) > width {
            let cut = fitting_prefix(rest, width);
            rows.push((rest[..cut].to_string(), rest_start));
            rest_start += cut;
            rest = &rest[cut..];
        }
        current.push_str(rest);
        current_start = rest_start;
        used = UnicodeWidthStr::width(rest);
    }
    rows.push((current, current_start));
}

/// Bytes of the longest prefix of `text` that fits in `width` cells, never
/// less than one character, so a character wider than the box still advances.
fn fitting_prefix(text: &str, width: usize) -> usize {
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        let cells = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cells > width && index > 0 {
            return index;
        }
        used += cells;
    }
    text.len()
}

/// Render one [`Item`] to a [`Text`] block. Used by [`transcript_text`] and the
/// snapshot tests, so an item looks the same the moment it streams and after it
/// closes.
pub fn item_text_with(item: &Item, expand: bool) -> Text<'static> {
    match item {
        Item::User(text) => Text::from(user_lines(text)),
        Item::Assistant(text) => render_markdown_cached(text),
        Item::Thinking { text, collapsed } => {
            if *collapsed && !expand {
                Text::from(thinking_marker())
            } else {
                Text::from(
                    text.lines()
                        .map(|line| {
                            Line::from(Span::styled(
                                line.to_string(),
                                Style::new().fg(Color::DarkGray),
                            ))
                        })
                        .collect::<Vec<Line>>(),
                )
            }
        }
        Item::Tool(view) => tool_text_with(view, expand),
        // One row per line of the message: a span drops its newlines, which
        // ran a multi-line provider error together into one unreadable row.
        Item::Error(message) => Text::from(
            format!("error: {message}")
                .lines()
                .map(|line| Line::from(Span::styled(line.to_string(), Style::new().fg(Color::Red))))
                .collect::<Vec<_>>(),
        ),
    }
}

/// A user turn in the transcript: the `> ` marker on the first row, every
/// continuation row indented to line up under it, one buffer row per line.
/// The bug this replaces: the whole text went into one [`Line`], and ratatui
/// does not split a span on `\n`, so a pasted multi-line prompt rendered as
/// one mashed row with the newlines swallowed instead of as the block the
/// input box showed while it was being composed.
fn user_lines(text: &str) -> Vec<Line<'static>> {
    let marker_style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            let marker = if index == 0 { "> " } else { "  " };
            Line::from(vec![
                Span::styled(marker, marker_style),
                Span::raw(line.to_string()),
            ])
        })
        .collect()
}

fn thinking_marker() -> Line<'static> {
    Line::from(Span::styled("ⓘ thinking", Style::new().fg(Color::DarkGray)))
}

fn tool_text_with(view: &ToolView, expand: bool) -> Text<'static> {
    let (marker, color) = tool_glyph(view.status);
    let collapsed_with_body = view.body.is_some() && view.collapsed && !expand;
    let mut header = vec![
        Span::styled(
            format!("{marker} {}", view.name),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(view.args_summary.clone(), Style::new().fg(Color::Gray)),
        Span::raw(" "),
        Span::styled(status_label(view.status), Style::new().fg(color)),
    ];
    // A collapsed body that never says so is only discoverable by knowing
    // Ctrl+O exists ahead of time. This is the one hint that it does.
    if collapsed_with_body {
        header.push(Span::styled(
            " (Ctrl+O for details)",
            Style::new().fg(Color::DarkGray),
        ));
    }
    let mut lines = vec![Line::from(header)];
    // The body is collapsible and collapsed by default past a size threshold;
    // expanding it is a later pass. Rendered when present and expanded. Lines
    // that look like a diff (`+`/`-`) tint green/red, with the specific
    // changed words (`edit`'s own [-.-]/{+.+} markers) bolded on top, so a
    // change reads at a glance without opening a dedicated diff view.
    //
    // Diff colouring only for a diff: applied to every body, `ls -l` printed
    // each file red (`-rw-r--r--` reads as a removed line) and a markdown file
    // read back turned its bullets red. Tabs become spaces because ratatui
    // drops control characters, which flattened Go and Makefile indentation.
    if let Some(body) = &view.body
        && (!view.collapsed || expand)
    {
        let diff = view.name == EDIT_TOOL || is_unified_diff(body);
        // Capped on screen, whole in the pager: a test run or a long diff
        // used to push the conversation off the top of the screen, which is
        // the reading Claude Code's `+N lines` fold exists to prevent.
        let limit = if expand {
            usize::MAX
        } else {
            MAX_INLINE_BODY_LINES
        };
        let (body_lines, hidden) = body_lines(body, diff, limit);
        lines.extend(body_lines);
        if hidden > 0 {
            lines.push(more_lines(format!("  … +{hidden} lines (Ctrl+O for all)")));
        }
    }
    Text::from(lines)
}

/// A tool body or a diff, one row per line up to `limit`, and how many lines
/// were left out.
///
/// A diff's lines are coloured by sign with the changed words picked out, and
/// its hunk headers are cyan rather than the context lines' grey: they are
/// where the change is, which is the first thing a reader wants. Anything else
/// is grey. Tabs become spaces because ratatui drops control characters, which
/// flattened Go and Makefile indentation.
fn body_lines(body: &str, diff: bool, limit: usize) -> (Vec<Line<'static>>, usize) {
    let lines = body
        .lines()
        .take(limit)
        .map(|line| {
            let line = line.replace('\t', TAB_AS_SPACES);
            let mut spans = vec![Span::raw("  ")];
            if !diff {
                spans.push(Span::styled(line, Style::new().fg(Color::DarkGray)));
            } else if line.starts_with("@@ ") {
                spans.push(Span::styled(line, Style::new().fg(Color::Cyan)));
            } else {
                spans.extend(highlight_diff_line(&line));
            }
            Line::from(spans)
        })
        .collect();
    let hidden = body.lines().count().saturating_sub(limit);
    (lines, hidden)
}

/// The dim italic row that says how much of a body was folded away.
fn more_lines(text: String) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// A dim line once the conversation passes [`CONTEXT_WARNING_PERCENT`] of the
/// size at which the engine compacts it, so the compaction that summarizes
/// older turns away does not arrive unannounced. Measured against the
/// engine's budget, not the model's window: compaction comes well before the
/// window fills, since the reply needs room too.
fn context_warning(state: &TuiState) -> Option<Line<'static>> {
    let budget = state.context_budget.filter(|&budget| budget > 0)?;
    let percent = (state.status.context_tokens * PERCENT / budget).min(PERCENT);
    (percent >= CONTEXT_WARNING_PERCENT).then(|| {
        Line::from(Span::styled(
            format!(
                "  context {percent}% of the way to compaction: older turns will be summarized soon (/compact now to choose when)"
            ),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
        ))
    })
}

/// The first screen of a session, before anything has been said: what this
/// is, where it is pointed, and the handful of keys worth knowing, the way
/// Claude Code and Pi open. An empty transcript used to be a blank screen, and
/// everything below it had to be discovered by already knowing to press `?`.
fn welcome_text(state: &TuiState) -> Text<'static> {
    let accent = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" ✻ ", accent),
            Span::styled("summer-code", accent),
            Span::styled(format!(" {}", env!("CARGO_PKG_VERSION")), dim),
        ]),
        Line::from(Span::styled(format!("   {}", state.status.cwd), dim)),
        Line::from(vec![
            Span::styled("   model ", dim),
            Span::raw(state.status.model.clone()),
            Span::styled(" · mode ", dim),
            Span::raw(state.execution_mode.as_str()),
        ]),
        Line::raw(""),
    ];
    for row in WELCOME_TIPS {
        let mut spans = vec![Span::raw("   ")];
        for (key, what) in *row {
            spans.push(Span::styled(
                *key,
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(" {what}   "), dim));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

/// A plan as a checklist: a header counting the finished steps, then one row
/// per step, done steps checked and dimmed, the active one marked and bold.
///
/// A completed plan checks every step whatever its `active_step` says, and a
/// blocked one marks its active step as where it stalled.
fn plan_lines(plan: &PlanDisplay) -> Vec<Line<'static>> {
    let done = match plan.progress {
        PlanProgress::Completed => plan.steps.len(),
        PlanProgress::Blocked | PlanProgress::InProgress => {
            plan.active.map_or(0, |active| active.saturating_sub(1))
        }
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("  plan {done}/{}", plan.steps.len()),
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))];
    for (index, step) in plan.steps.iter().enumerate() {
        let (marker, style) = if index < done {
            (
                "☒",
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
            )
        } else if index == done && plan.progress == PlanProgress::Blocked {
            (
                "✗",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        } else if index == done && plan.active.is_some() {
            (
                "▶",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        } else {
            ("☐", Style::new())
        };
        lines.push(Line::from(vec![
            Span::styled(format!("    {marker} "), style),
            Span::styled(step.clone(), style),
        ]));
    }
    lines
}

/// A permission prompt's diff, capped at [`MAX_PROMPT_DIFF_LINES`] with a
/// count of what was cut. No pager hint: the prompt owns the keys while it is
/// open, so Ctrl+O would not reach the pager.
fn prompt_diff_lines(diff: &str) -> Vec<Line<'static>> {
    let (mut lines, hidden) = body_lines(diff, true, MAX_PROMPT_DIFF_LINES);
    if hidden > 0 {
        lines.push(more_lines(format!("  … {hidden} more lines")));
    }
    lines
}

/// Whether `body` is a unified diff: a hunk header or a file header line.
fn is_unified_diff(body: &str) -> bool {
    body.lines().any(|line| {
        line.starts_with("@@ ") || line.starts_with("diff --git ") || line.starts_with("+++ ")
    })
}

/// `TimedOut` shares red with `Denied`/`Failed`, not `Running`'s yellow: a
/// timeout is a failure the tool did not recover from, and coloring it like
/// an in-flight call read as "still going" at a glance. The glyph still
/// differs (`⏱` vs `✗`), so *which* failure it was stays legible up close.
fn tool_glyph(status: ToolStatus) -> (&'static str, Color) {
    match status {
        ToolStatus::Running => ("●", Color::Yellow),
        ToolStatus::Done => ("✓", Color::Green),
        ToolStatus::Denied | ToolStatus::Failed => ("✗", Color::Red),
        ToolStatus::TimedOut => ("⏱", Color::Red),
        ToolStatus::Pending => ("○", Color::DarkGray),
    }
}

fn status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Denied => "denied",
        ToolStatus::Failed => "failed",
        ToolStatus::TimedOut => "timed out",
        ToolStatus::Pending => "pending",
    }
}

/// Render an open `/model` picker: a title naming the stage, then one row per
/// choice with the selection highlighted the same way
/// [`render_completion_popup`] highlights its menu. The driver owns every key
/// while this is open; this only draws.
///
/// One row in `/model`'s Copilot list: the model's friendly name (or id when
/// the API sent none), with its context window in thousands when the API
/// reported one -- the same two facts a Copilot-aware editor surfaces beside
/// each model. The id is what the chat endpoint receives; the label is just
/// what the picker shows.
fn copilot_model_label(model: &provider::CopilotModel) -> String {
    let name = model.display_name().to_string();
    match model.max_context_window_tokens {
        Some(tokens) => format!("{name} ({})", format_context_tokens(tokens)),
        None => name,
    }
}

/// Compact context-window suffix: `128000` -> `128K`, `2000000` -> `2.0M`.
/// Shared by the model picker and the statusline's `{ctx}` variable, so the
/// window reads the same in both.
pub(crate) fn format_context_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else {
        format!("{}K", tokens / 1_000)
    }
}

/// The first row index a picker renders so `selected` stays inside the visible
/// `window` of rows without the window running past `total`. Pure so the
/// scroll-keeps-the-selection-visible behavior is unit-testable without a
/// frame; the failure mode it pins is a long Copilot catalog whose highlight
/// scrolls off-screen the moment Down moves past the first page.
fn picker_window_start(selected: usize, total: usize, window: usize) -> usize {
    if total <= window {
        0
    } else {
        (selected + 1).saturating_sub(window).min(total - window)
    }
}

/// Summary lines for the currently active session, rendered at the top of the
/// `/resume` session picker so the user sees their current session context
/// (session id, turns, token spend, first user message) before picking a past session.
fn active_session_summary_lines(state: &TuiState) -> (Line<'static>, Line<'static>) {
    let turns = state.usage.len().max(
        state
            .transcript
            .iter()
            .filter(|i| matches!(i, Item::User(_)))
            .count(),
    );
    let turns_label = if turns == 1 {
        "1 turn".to_string()
    } else {
        format!("{turns} turns")
    };
    let preview = state
        .transcript
        .iter()
        .find_map(|item| match item {
            Item::User(text) => {
                let line = text.lines().next().unwrap_or("").trim();
                if line.is_empty() {
                    None
                } else {
                    Some(truncate(line, ACTIVE_SESSION_PREVIEW_LIMIT))
                }
            }
            _ => None,
        })
        .unwrap_or_else(|| "new session".to_string());

    let session_display = if state.session_id.is_empty() {
        "current".to_string()
    } else {
        state.session_id.clone()
    };

    let age = state
        .turn_starts
        .first()
        .map(|&t| format_age(t))
        .unwrap_or_else(|| "just now".to_string());

    let summary = Line::from(vec![
        Span::styled(
            "  Active: ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(session_display),
        Span::styled(" • ", Style::new().fg(Color::DarkGray)),
        Span::styled(age, Style::new().fg(Color::DarkGray)),
        Span::styled(" • ", Style::new().fg(Color::DarkGray)),
        Span::raw(turns_label),
        Span::styled(" • ", Style::new().fg(Color::DarkGray)),
        Span::raw(format!("{} tokens", state.status.total_tokens)),
        Span::styled(" • ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!("\"{preview}\""), Style::new().fg(Color::DarkGray)),
    ]);

    let header = Line::from(Span::styled(
        "  Past sessions (select to resume):",
        Style::new().fg(Color::DarkGray),
    ));

    (summary, header)
}

/// The `/model` picker as a floating list above the input box -- the same place
/// [`render_completion_popup`] floats the command menu -- so the prompt at the
/// bottom stays the prompt (and the filter), and the choices read as a palette
/// stacked above it rather than replacing the prompt. No border, matching the
/// completion menu's look.
fn render_picker_popup(
    state: &TuiState,
    input: &InputEditor,
    area: Rect,
    input_area: Rect,
    frame: &mut Frame,
) {
    let Some(picker) = &state.pending_picker else {
        return;
    };
    // Need at least one row of clearance above the input box.
    if input_area.y <= area.y {
        return;
    }
    if let PickerStage::Effort { model, levels, .. } = &picker.stage {
        render_effort_slider(model, levels, picker.selected, area, input_area, frame);
        return;
    }
    let available = (input_area.y - area.y) as usize;
    let matching = picker.matching(input.buffer());
    let rows: Vec<String> = match &picker.stage {
        PickerStage::Models {
            profile,
            dynamic_models,
        } => match dynamic_models {
            Some(models) => matching
                .iter()
                .map(|&i| copilot_model_label(&models[i]))
                .collect(),
            None => matching
                .iter()
                .map(|&i| profile.models[i].name.to_string())
                .collect(),
        },
        PickerStage::Providers { profiles } => matching
            .iter()
            .map(|&i| profiles[i].label.to_string())
            .collect(),
        PickerStage::Sessions { rows } => matching
            .iter()
            .map(|&i| format!("{}  {:>10}  {}", rows[i].id, rows[i].age, rows[i].preview))
            .collect(),
        PickerStage::Rewind { rows } => matching
            .iter()
            .map(|&i| rows[i].lines().next().unwrap_or_default().to_string())
            .collect(),
        PickerStage::Effort { .. } => unreachable!("handled above"),
    };
    let total = rows.len();

    let (header_lines, header_height) = match &picker.stage {
        PickerStage::Sessions { .. } => {
            let (summary, header) = active_session_summary_lines(state);
            if available >= 3 {
                (vec![summary, header], 2usize)
            } else if available == 2 {
                (vec![summary], 1usize)
            } else {
                (Vec::new(), 0usize)
            }
        }
        _ => (Vec::new(), 0usize),
    };

    let list_available = available.saturating_sub(header_height);
    let list_height = total.min(list_available);
    let height = (header_height + list_height) as u16;
    if height == 0 {
        return;
    }
    let popup_area = Rect {
        x: area.x,
        y: input_area.y - height,
        width: area.width,
        height,
    };

    let mut lines: Vec<Line> = Vec::with_capacity(header_height + list_height);
    lines.extend(header_lines);

    if list_height > 0 {
        // Scroll the window to keep the selection visible within the popup.
        let window = list_height;
        let start = picker_window_start(picker.selected, total, window);
        let end = (start + window).min(total);
        for (offset, row) in rows[start..end].iter().enumerate() {
            let style = if start + offset == picker.selected {
                Style::new()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            lines.push(Line::from(Span::styled(format!("  {row}"), style)));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), popup_area);
}

/// Render a minimal reasoning effort selection slider widget above the input box.
fn render_effort_slider(
    model: &str,
    levels: &[String],
    selected: usize,
    area: Rect,
    input_area: Rect,
    frame: &mut Frame,
) {
    if levels.is_empty() {
        return;
    }
    let height = 2u16;
    if input_area.y < area.y + height {
        return;
    }
    let popup_area = Rect {
        x: area.x,
        y: input_area.y - height,
        width: area.width,
        height,
    };

    let active_level = levels.get(selected).map(|s| s.as_str()).unwrap_or("");

    let mut slider_spans = Vec::new();
    slider_spans.push(Span::raw("  "));

    for (i, level) in levels.iter().enumerate() {
        if i > 0 {
            let connector = if i <= selected {
                Span::styled("──", Style::new().fg(Color::Cyan))
            } else {
                Span::styled("──", Style::new().fg(Color::DarkGray))
            };
            slider_spans.push(connector);
        }

        let is_sel = i == selected;
        if is_sel {
            slider_spans.push(Span::styled(
                format!(" [ {level} ] "),
                Style::new()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            slider_spans.push(Span::styled(
                format!(" {level} "),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "  Effort: ",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("[ {active_level} ]"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ({model})"), Style::new().fg(Color::DarkGray)),
        ]),
        Line::from(slider_spans),
    ];

    frame.render_widget(Paragraph::new(Text::from(lines)), popup_area);
}

/// Render the auto-mode acceptance line: the newest command the mode answered
/// for, one row directly above the input box. Transient by design -- the
/// banner-shaped display it replaced stacked every acceptance of a turn into a
/// wall of `Auto-accepted:` lines held over the transcript for the whole turn;
/// one line, replaced by each newer acceptance and dropped
/// `AUTO_ACCEPT_TIMEOUT` later, keeps the fact visible without the wall.
/// Cyan, not the banner's yellow: the two sit one row apart above the prompt,
/// and a held error already claims yellow's alarm weight -- this line is
/// neither an error nor a result, just a running commentary on auto mode.
/// Reports whether a row was drawn, so
/// [`render_pending_message`] can float the banner above it.
fn render_auto_accepted(state: &TuiState, area: Rect, input_area: Rect, frame: &mut Frame) -> bool {
    let Some(description) = state.auto_accepted_line() else {
        return false;
    };
    // There must be one row above the input box for the line.
    if input_area.y <= area.y {
        return false;
    }
    let row = Rect {
        y: input_area.y - 1,
        height: 1,
        ..input_area
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("auto: ", Style::new().fg(Color::DarkGray)),
            Span::styled(description.to_string(), Style::new().fg(Color::Cyan)),
        ])),
        row,
    );
    true
}

/// Render the latest system message floating just above the input box, the
/// same technique [`render_completion_popup`] uses: it overlays the bottom of
/// the transcript rather than the input box itself, so the editable prompt
/// underneath is never displaced or hidden. Every command result, login-flow
/// update, and inline error passes through
/// [`crate::tui::state::TuiState::notify`] to get here, so nothing lands only
/// in the transcript where a busy screen could bury it before it is read. All
/// of them stay -- a message never overwrites the one before it, since two
/// shown back to back (the device flow's code, immediately followed by its
/// browser prompt) must both stay readable -- until `/clear` empties the
/// list, or Esc dismisses a banner an error is holding. A `Notice`'s own
/// `sticky` flag colors it apart from a plain one (see
/// [`crate::tui::state::TuiState::notify_error`]); only the most recent lines
/// that fit are drawn, with a count of how many are not past that. `reserved`
/// rows directly above the input box are already taken (the acceptance
/// line's, when one is showing); the banner floats above those rather than
/// over them.
fn render_pending_message(
    state: &TuiState,
    area: Rect,
    input_area: Rect,
    reserved: u16,
    frame: &mut Frame,
) {
    if state.pending_messages.is_empty() {
        return;
    }
    let baseline = input_area.y.saturating_sub(reserved);
    // There must be at least one row above it for the banner.
    if baseline <= area.y {
        return;
    }
    let available = baseline - area.y;
    let held = state.banner_sticky();
    let mut all_lines: Vec<(bool, &str)> = state
        .pending_messages
        .iter()
        .flat_map(|notice| {
            let sticky = notice.sticky;
            notice.text.lines().map(move |line| (sticky, line))
        })
        .collect();
    // A held banner says how it leaves, since no timeout will do it for the
    // user: without the hint, "it never goes away" reads as a bug rather
    // than a choice. It also says how long it has been waiting, so a notice
    // that has sat unread for ten minutes does not look identical to one
    // that just arrived.
    let held_hint = state
        .pending_messages
        .iter()
        .filter(|notice| notice.sticky)
        .map(|notice| notice.at.elapsed())
        .max()
        .map(crate::tui::state::format_duration_compact)
        .filter(|age| !age.is_empty())
        .map_or_else(
            || "(Esc to dismiss)".to_string(),
            |age| format!("(Esc to dismiss · held {age})"),
        );
    if held {
        all_lines.push((false, held_hint.as_str()));
    }
    let capacity = available.min(MAX_MESSAGE_ROWS);
    if capacity == 0 {
        return;
    }
    // A row given to "N more" is a row taken from content, so only spend it
    // when there is truly something hidden and a second row to show past it.
    let show_hidden_count = all_lines.len() as u16 > capacity && capacity > 1;
    let content_rows = capacity - u16::from(show_hidden_count);
    let visible_from = all_lines.len().saturating_sub(content_rows as usize);
    let height = content_rows + u16::from(show_hidden_count);
    let message_area = Rect {
        x: area.x,
        y: baseline - height,
        width: area.width,
        height,
    };
    let error_style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let info_style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let chrome_style = Style::new().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();
    if show_hidden_count {
        lines.push(Line::from(Span::styled(
            format!("▲ {visible_from} more"),
            chrome_style,
        )));
    }
    // `saturating_sub`, not `- 1`: a notice whose text is empty contributes no
    // line to `all_lines`, and no `held` hint is pushed unless a notice
    // already made it non-empty, but nothing else here guarantees that.
    let last_index = all_lines.len().saturating_sub(1);
    lines.extend(
        all_lines[visible_from..]
            .iter()
            .enumerate()
            .map(|(index, (sticky, line))| {
                // The hint is chrome, not message content, so it renders dim
                // rather than in either notice color.
                let is_hint = held && visible_from + index == last_index;
                let style = if is_hint {
                    chrome_style
                } else if *sticky {
                    error_style
                } else {
                    info_style
                };
                Line::from(Span::styled((*line).to_string(), style))
            }),
    );
    frame.render_widget(Text::from(lines), message_area);
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use journal::CallId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use provider::{PROFILES, ProviderId};

    use crate::tui::state::{Status, ToolView, reduce};

    fn status() -> Status {
        Status {
            model: "mock".into(),
            effort: None,
            execution_mode: crate::mode::ExecutionMode::Interactive,
            total_tokens: 0,
            context_tokens: 0,
            context_window: None,
            backend: "none".into(),
            cwd: "/repo".into(),
        }
    }

    #[test]
    fn a_streaming_turn_renders_its_text_and_a_working_indicator() {
        // While a turn streams, the conversation area shows the assistant text
        // and a working progress line at its tail, rather than an editable
        // prompt. Snapshot, not pixel-perfect: assert the streamed text and the
        // progress line both land in the buffer. The terminal is untestable;
        // this is the next best thing.
        let mut state = crate::tui::state::TuiState::new(status());
        for event in [
            engine::UiEvent::TurnStarted {
                user: "hello".into(),
            },
            engine::UiEvent::TextDelta {
                text: "streaming reply".into(),
            },
        ] {
            reduce(&mut state, event);
        }
        assert!(state.streaming);
        let input = InputEditor::new();

        // Tall enough that the transcript holds the user line, a blank, the
        // streamed reply, and the progress line all at once.
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("streaming reply"),
            "the streamed text is in the live region"
        );
        assert!(
            rendered.contains("working"),
            "the progress line rides the tail of the conversation: {rendered}"
        );
    }

    #[test]
    fn the_progress_line_shows_the_turns_elapsed_seconds() {
        // Pin the format Claude Code uses: a working glyph plus elapsed time.
        // Back-date the start three seconds so a whole second is unambiguous.
        let mut state = crate::tui::state::TuiState::new(status());
        state.streaming = true;
        state.turn_started_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(3));
        let input = InputEditor::new();

        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("working… 3s"),
            "elapsed seconds since the turn started: {rendered}"
        );
    }

    #[test]
    fn the_progress_line_shows_thinking_and_realtime_token_counts() {
        let mut state = crate::tui::state::TuiState::new(status());
        state.streaming = true;
        state.turn_started_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(4));
        state.transcript.push(crate::tui::state::Item::Thinking {
            text: "planning next step".into(),
            collapsed: true,
        });
        state.turn_uploaded_tokens = 1500;
        state.turn_downloaded_tokens = 80;
        let input = InputEditor::new();

        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("thinking… 4s (↑1.5k, ↓80)"),
            "thinking with real-time tokens: {rendered}"
        );
    }

    #[test]
    fn a_pending_login_shows_its_status_instead_of_the_editable_prompt() {
        // The bug this catches: the device-flow login's own key-reading loop
        // owns every keystroke while it runs, so if the prompt area kept
        // showing the normal editable `> ` prompt underneath it, typing would
        // look like it should do something and silently do nothing instead.
        let mut state = crate::tui::state::TuiState::new(status());
        state.pending_login = Some("waiting for GitHub…".into());
        let input = InputEditor::new();

        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("waiting for GitHub"), "{rendered}");
        assert!(
            !rendered.contains("> "),
            "the editable prompt marker must not also render: {rendered}"
        );
    }

    #[test]
    fn a_pending_secret_entry_renders_masked_not_the_key() {
        // The whole point of the entry box: the label and one `*` per
        // character render, the typed key never does, and the editable
        // prompt is not shown underneath (its keys would go to the box).
        let mut state = crate::tui::state::TuiState::new(status());
        let mut secret = crate::tui::state::SecretInput::new(provider::ProviderId::Anthropic);
        for ch in "sk-ant-maskme".chars() {
            secret.push(ch);
        }
        state.pending_secret = Some(secret);
        let input = InputEditor::new();

        let backend = TestBackend::new(48, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("Anthropic API key:"), "{rendered}");
        assert!(
            rendered.contains("*************"),
            "one star per character: {rendered}"
        );
        assert!(
            !rendered.contains("sk-ant-maskme"),
            "the key itself must never render: {rendered}"
        );
        assert!(
            !rendered.contains("> "),
            "the editable prompt marker must not also render: {rendered}"
        );
    }

    #[test]
    fn a_long_masked_line_clips_to_the_box_width() {
        // A pasted key is often 100+ characters; at a normal terminal width
        // the star run must clip inside the box (not wrap or push the hint
        // off), and the frame must still draw -- the cursor clamps to the
        // edge rather than off-frame.
        let mut state = crate::tui::state::TuiState::new(status());
        let mut secret = crate::tui::state::SecretInput::new(provider::ProviderId::OpenAi);
        for _ in 0..80 {
            secret.push('*');
        }
        state.pending_secret = Some(secret);
        let input = InputEditor::new();

        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .expect("a long mask draws rather than panics");

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("OpenAI API key:"), "{rendered}");
        assert!(
            !rendered.contains("*".repeat(20).as_str()),
            "an 80-star mask cannot fit a 30-column box: {rendered}"
        );
    }

    #[test]
    fn a_held_error_banner_says_how_it_leaves() {
        // An error banner stays until dismissed; without the hint that reads
        // as a stuck UI rather than a choice.
        let mut state = crate::tui::state::TuiState::new(status());
        state.notify_error("could not store credential: permission denied");
        let input = InputEditor::new();

        let backend = TestBackend::new(50, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("Could not store credential"),
            "{rendered}"
        );
        assert!(rendered.contains("(Esc to dismiss)"), "{rendered}");

        // An ordinary notice carries no hint: it leaves on its own.
        let mut info = crate::tui::state::TuiState::new(status());
        info.notify("logged in to Anthropic.");
        terminal
            .draw(|frame| view(&mut info, &input, frame))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(!rendered.contains("Esc to dismiss"), "{rendered}");
    }

    #[test]
    fn the_end_of_a_wide_character_reply_can_be_seen() {
        // Each CJK character takes two cells. Counted as one, the reply wrapped
        // to more rows than the scroll knew of, and its tail sat below the fold.
        let mut state = crate::tui::state::TuiState::new(status());
        let reply = format!("{}終わり", "漢字".repeat(120));
        state
            .transcript
            .push(crate::tui::state::Item::Assistant(reply));
        let input = InputEditor::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("終"),
            "the last characters are off screen"
        );
    }

    #[test]
    fn a_transcript_taller_than_u16_rows_still_shows_its_newest_line() {
        // The scroll offset was one `u16` for the whole transcript, and wrapped
        // past 65,535 rows, showing a slice from the middle of the session.
        let lines: Vec<Line<'_>> = (0..70_000).map(|n| Line::raw(format!("row {n}"))).collect();
        let window = tail_window(&lines, 80, 10, 0);
        assert_eq!(window.start, 70_000 - 10);
        assert_eq!(window.scroll, 0);
        let up = tail_window(&lines, 80, 10, 69_995);
        assert_eq!(up.scroll_up, 70_000 - 10, "clamped at the top");
        assert_eq!(up.start, 0);
    }

    #[test]
    fn only_a_real_diff_is_coloured_as_one() {
        // `ls -l` lines start with `-`, which the diff tint read as removals.
        assert!(!is_unified_diff("-rw-r--r-- 1 me me 10 a.rs\n+ not a hunk"));
        assert!(is_unified_diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new"));
    }

    #[test]
    fn a_tab_indented_body_keeps_its_indentation() {
        // ratatui drops control characters, so a tab vanished outright.
        let view = ToolView {
            call: CallId::new("c".into()),
            name: "shell".into(),
            args_summary: "cat Makefile".into(),
            status: ToolStatus::Done,
            body: Some("all:\n\tcargo build".into()),
            collapsed: false,
        };
        let rendered: String = tool_text_with(&view, false)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(rendered.contains("    cargo build"), "{rendered}");
    }

    #[test]
    fn every_queued_message_shows_as_waiting() {
        // A second queued message used to replace the first, and neither was
        // shown, so a message submitted mid-turn looked like it had vanished.
        let mut state = crate::tui::state::TuiState::new(status());
        state.queued = vec!["first follow-up".into(), "second\nwith more".into()];
        let input = InputEditor::new();
        let mut terminal = Terminal::new(TestBackend::new(46, 12)).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("queued: first follow-up"), "{rendered}");
        assert!(rendered.contains("queued: second"), "{rendered}");
    }

    #[test]
    fn an_empty_session_opens_on_the_welcome_screen_and_leaves_it_on_the_first_message() {
        let render = |state: &mut crate::tui::state::TuiState| -> String {
            let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();
            terminal
                .draw(|frame| view(state, &InputEditor::new(), frame))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol().to_string())
                .collect()
        };
        let mut state = crate::tui::state::TuiState::new(status());
        let screen = render(&mut state);
        assert!(screen.contains("summer-code"), "{screen}");
        assert!(screen.contains("run a shell command"), "the tips: {screen}");

        state.transcript.push(Item::User("hello".into()));
        let screen = render(&mut state);
        assert!(!screen.contains("run a shell command"), "{screen}");
    }

    #[test]
    fn the_context_warning_appears_near_the_compaction_point_not_the_window() {
        let mut state = crate::tui::state::TuiState::new(status());
        state.context_budget = Some(100_000);
        state.status.context_tokens = 79_000;
        assert!(context_warning(&state).is_none(), "79% is not close yet");
        state.status.context_tokens = 85_000;
        let line: String = context_warning(&state)
            .expect("85% of the way")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(line.contains("85%") && line.contains("/compact"), "{line}");
        state.context_budget = None;
        assert!(context_warning(&state).is_none(), "no budget, no guess");
    }

    #[test]
    fn a_plan_checklist_checks_what_is_done_and_marks_the_active_step() {
        let rows = |plan: &PlanDisplay| -> Vec<String> {
            plan_lines(plan)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        let mut plan = PlanDisplay {
            active: Some(2),
            progress: PlanProgress::InProgress,
            steps: vec!["read".into(), "edit".into(), "test".into()],
        };
        assert_eq!(
            rows(&plan),
            ["  plan 1/3", "    ☒ read", "    ▶ edit", "    ☐ test"]
        );

        plan.progress = PlanProgress::Completed;
        assert_eq!(
            rows(&plan),
            ["  plan 3/3", "    ☒ read", "    ☒ edit", "    ☒ test"],
            "a completed plan checks every step, whatever its active step says"
        );

        plan.progress = PlanProgress::Blocked;
        assert_eq!(rows(&plan)[2], "    ✗ edit", "where it stalled");
    }

    #[test]
    fn a_long_tool_body_folds_on_screen_and_unfolds_in_the_pager() {
        let view = ToolView {
            call: journal::CallId::new("toolu_x".into()),
            name: "shell".into(),
            args_summary: "cargo test".into(),
            status: ToolStatus::Done,
            body: Some((1..=50).map(|n| format!("line {n}\n")).collect()),
            collapsed: false,
        };
        let screen = tool_text_with(&view, false).to_string();
        assert!(
            screen.contains("line 20") && !screen.contains("line 21"),
            "{screen}"
        );
        assert!(screen.contains("… +30 lines (Ctrl+O for all)"), "{screen}");
        let pager = tool_text_with(&view, true).to_string();
        assert!(pager.contains("line 50") && !pager.contains("+30 lines"));
    }

    #[test]
    fn a_streaming_reply_never_enters_the_markdown_cache() {
        // Each frame of a growing reply is a new prefix. Cached, one reply
        // added an entry per token, filled the cache, and the clear that
        // followed threw away every finished rendering with it.
        let mut transcript = vec![Item::Assistant("finished reply".into())];
        let mut growing = String::new();
        transcript.push(Item::Assistant(String::new()));
        for n in 0..600 {
            growing.push_str(&format!("token{n} "));
            *transcript.last_mut().unwrap() = Item::Assistant(growing.clone());
            let _ = transcript_text(&transcript, true);
        }
        assert_eq!(
            crate::tui::markdown::render_cache_len(),
            1,
            "only the finished reply is cached"
        );

        let _ = transcript_text(&transcript, false);
        assert_eq!(
            crate::tui::markdown::render_cache_len(),
            2,
            "once the turn ends, the last reply is finished too"
        );
    }

    #[test]
    fn a_prompt_diff_marks_the_changed_word_and_caps_its_length() {
        let before: String = (1..=60).map(|n| format!("line {n}\n")).collect();
        let after = before.replace("line 2\n", "line two\n");
        let lines = prompt_diff_lines(&tools::hunk_diff(&before, &after));
        let header = &lines[0].spans[1];
        assert_eq!(header.style.fg, Some(Color::Cyan), "{header:?}");
        let marked: Vec<&Span<'_>> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style.bg.is_some())
            .collect();
        assert_eq!(
            marked
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            ["2", "two"],
            "only the changed word is backgrounded, on each side"
        );

        let all_new: String = (1..=45).map(|n| format!("row {n}\n")).collect();
        let lines = prompt_diff_lines(&tools::hunk_diff("", &all_new));
        assert_eq!(lines.len(), MAX_PROMPT_DIFF_LINES + 1);
        let last = &lines[MAX_PROMPT_DIFF_LINES].spans[0].content;
        assert_eq!(last, "  … 16 more lines", "45 rows and a header, 30 shown");
    }

    #[test]
    fn a_permission_prompt_renders_below_the_conversation_in_transcript() {
        let mut state = crate::tui::state::TuiState::new(status());
        state.transcript.push(crate::tui::state::Item::Assistant(
            "I will run tests now".into(),
        ));
        state.pending_prompt = Some(crate::tui::state::PromptDisplay {
            armed_at: std::time::Instant::now(),
            description: "run `sh -c make test`".into(),
            diff: None,
            offer: Some("allow run sh -c make test".into()),
            selected: 0,
        });
        let mut input = InputEditor::new();
        for ch in "draft".chars() {
            input.handle(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }

        let backend = TestBackend::new(46, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("I will run tests now"),
            "the message: {rendered}"
        );
        assert!(rendered.contains("summer: run"), "the ask: {rendered}");
        assert!(rendered.contains("[1]"), "the offer: {rendered}");
        assert!(rendered.contains("●"), "bullet item: {rendered}");
        assert!(
            rendered.contains("> draft"),
            "the prompt renders underneath: {rendered}"
        );

        let rows: Vec<String> = {
            let buf = terminal.backend().buffer();
            let width = buf.area.width as usize;
            buf.content()
                .chunks(width)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect()
        };
        let msg_row = rows
            .iter()
            .position(|r| r.contains("I will run tests now"))
            .expect("msg");
        let ask_row = rows
            .iter()
            .position(|r| r.contains("summer: run"))
            .expect("ask");
        let input_row = rows
            .iter()
            .position(|r| r.contains("> draft"))
            .expect("prompt");
        assert!(
            msg_row < ask_row,
            "the ask renders right below the conversation"
        );
        assert!(ask_row < input_row, "the ask is above the input box");
    }

    #[test]
    fn a_multi_line_user_prompt_renders_one_row_per_line() {
        // The paste case: a multi-line user turn must keep its shape in the
        // transcript, the same shape the input box showed while it was being
        // composed. The old renderer put the whole block in one Line, and
        // ratatui does not split a span on `\n`, so every pasted line mashed
        // into a single row with the newlines swallowed.
        let mut state = crate::tui::state::TuiState::new(status());
        state.transcript.push(crate::tui::state::Item::User(
            "line one\nline two\nline three".into(),
        ));
        let input = InputEditor::new();

        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("> line one"),
            "marker on the first row: {rendered}"
        );
        assert!(
            rendered.contains("  line two") && rendered.contains("  line three"),
            "continuations on their own indented rows: {rendered}"
        );
    }

    #[test]
    fn the_detail_pager_shows_thinking_and_tool_bodies_expanded() {
        // Ctrl+O is the surface for reading the detail: a collapsed transcript
        // (`ⓘ thinking`, a bare tool header) must expand inside the pager
        // without touching the transcript's own toggle state.
        let mut state = crate::tui::state::TuiState::new(status());
        state.transcript.push(crate::tui::state::Item::Thinking {
            text: "the model reasoned here".into(),
            collapsed: true,
        });
        state
            .transcript
            .push(crate::tui::state::Item::Tool(crate::tui::state::ToolView {
                call: journal::CallId::new("toolu_1".into()),
                name: "read".into(),
                args_summary: "src/main.rs".into(),
                status: crate::tui::state::ToolStatus::Done,
                body: Some("fn main() {}".into()),
                collapsed: true,
            }));
        state.detail = Some(crate::tui::state::Detail::default());
        let input = InputEditor::new();

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("the model reasoned here"), "{rendered}");
        assert!(rendered.contains("fn main() {}"), "{rendered}");
        assert!(rendered.contains("detail"), "the pager footer: {rendered}");
        // The state was not mutated: the transcript keeps its collapsed flags,
        // so closing the pager restores exactly what was there before.
        assert!(
            matches!(
                state.transcript.first(),
                Some(crate::tui::state::Item::Thinking {
                    collapsed: true,
                    ..
                })
            ),
            "expansion is a rendering choice, not a state change"
        );
    }

    #[test]
    fn an_idle_turn_renders_the_editable_prompt() {
        // When nothing streams, the prompt is editable: the `> ` marker and the
        // buffer are in the region, and the cursor is placed.
        let mut state = crate::tui::state::TuiState::new(status());
        let mut input = InputEditor::new();
        for ch in "hi".chars() {
            input.handle(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }

        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("> "), "the prompt marker is rendered");
        assert!(rendered.contains("hi"), "the input buffer is rendered");
    }

    #[test]
    fn the_status_footer_renders_the_statusline_then_the_editing_mode() {
        // Two footer rows: the statusline on the first, the editing mode on
        // the second. A frame too short to spare the second row drops the
        // mode line, not the statusline and not the input box's rules.
        let mut state = crate::tui::state::TuiState::new(status());
        let input = InputEditor::new();

        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rows: Vec<String> = {
            let buf = terminal.backend().buffer();
            let width = buf.area.width as usize;
            buf.content()
                .chunks(width)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect()
        };
        assert!(rows[6].contains("mock"), "the statusline row: {rows:?}");
        assert!(
            rows[7].contains("mode: interactive"),
            "the editing-mode row: {rows:?}"
        );

        // Five rows is below the threshold: the footer keeps its one row for
        // the statusline, and the mode line is the part that goes.
        let mut short = Terminal::new(TestBackend::new(40, 5)).unwrap();
        short.draw(|frame| view(&mut state, &input, frame)).unwrap();
        let rendered: String = short
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("mock"),
            "the statusline stays: {rendered}"
        );
        assert!(
            !rendered.contains("mode:"),
            "a short frame drops the mode line: {rendered}"
        );
    }

    #[test]
    fn the_auto_acceptance_line_renders_above_the_prompt_then_leaves() {
        // The redesign in one frame: an auto-mode acceptance is one row above
        // the input box (not a stack of `Auto-accepted:` banner messages held
        // for the whole turn), the banner floats above the note's row rather
        // than over it, and the note leaves once its timeout has passed while
        // the banner stays.
        let mut state = crate::tui::state::TuiState::new(status());
        state.note_auto_accepted("run `sh -c make check`".into());
        state.notify("switched to auto mode");
        let input = InputEditor::new();

        let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rows: Vec<String> = {
            let buf = terminal.backend().buffer();
            let width = buf.area.width as usize;
            buf.content()
                .chunks(width)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect()
        };
        let note_row = rows
            .iter()
            .position(|r| r.contains("auto: run `sh -c make check`"))
            .expect("the acceptance note is visible");
        let banner_row = rows
            .iter()
            .position(|r| r.contains("Switched to auto mode"))
            .expect("the banner is visible");
        let input_row = rows
            .iter()
            .position(|r| r.contains('─'))
            .expect("the input box's top rule");
        assert!(
            banner_row < note_row && note_row < input_row,
            "banner above the note, note above the input box: {rows:?}"
        );
        // Adjacent rows in near-identical yellow read as one wall of chrome;
        // the note's own color is what keeps it legible as a different kind
        // of line from the banner above it.
        let buf = terminal.backend().buffer();
        let width = buf.area.width as usize;
        let note_fg = buf.content()[note_row * width + "auto: ".len()].fg;
        assert_eq!(
            note_fg,
            Color::Cyan,
            "the acceptance note must not share the banner's alarm color"
        );

        // Back-date the note past its timeout, the way a long tool call would,
        // and redraw: the note leaves, the banner stays.
        let stale = Instant::now().checked_sub(Duration::from_secs(3));
        if let Some(note) = &mut state.auto_accepted {
            note.at = stale.expect("a timeout-sized step back fits an Instant");
        }
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            !rendered.contains("auto: run"),
            "an expired note does not render: {rendered}"
        );
        assert!(
            rendered.contains("Switched to auto mode"),
            "the banner outlives the note: {rendered}"
        );
    }

    #[test]
    fn a_pending_message_floats_above_the_input_box_without_hiding_the_prompt() {
        // notify() is the one path every command result, login update, and
        // inline error goes through; this pins that it actually lands where
        // a busy screen cannot bury it, not just in the transcript -- and
        // that it does so without displacing whatever the user was typing,
        // the same way the completion popup overlays rather than replaces.
        let mut state = crate::tui::state::TuiState::new(status());
        state.notify("stored a credential for Anthropic.\nRestart to use it.");
        let mut input = InputEditor::new();
        for ch in "unsent draft".chars() {
            input.handle(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("Stored a credential for Anthropic."),
            "{rendered}"
        );
        assert!(rendered.contains("Restart to use it."), "{rendered}");
        assert!(
            rendered.contains("unsent draft"),
            "the prompt stays visible and editable underneath the banner: {rendered}"
        );
    }

    #[test]
    fn a_notify_error_line_renders_red_and_a_plain_notify_line_renders_yellow() {
        // Before Notice tracked severity, an error and a routine result were
        // the same yellow bold line -- readable apart only by waiting to see
        // which one auto-cleared.
        let mut state = crate::tui::state::TuiState::new(status());
        state.notify("routine update");
        state.notify_error("something broke");
        let input = InputEditor::new();

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let buf = terminal.backend().buffer();
        let width = buf.area.width as usize;
        let rows: Vec<String> = buf
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let info_row = rows
            .iter()
            .position(|row| row.contains("Routine update"))
            .expect("the plain notice is visible");
        let error_row = rows
            .iter()
            .position(|row| row.contains("Something broke"))
            .expect("the error notice is visible");

        let fg_at = |row: usize| buf.content()[row * width].fg;
        assert_eq!(
            fg_at(info_row),
            Color::Yellow,
            "a plain notice stays yellow"
        );
        assert_eq!(
            fg_at(error_row),
            Color::Red,
            "an error notice must not read like ordinary chrome"
        );
    }

    #[test]
    fn an_overflowing_banner_says_how_many_lines_are_hidden() {
        let mut state = crate::tui::state::TuiState::new(status());
        for i in 0..8 {
            state.notify(format!("line {i}"));
        }
        let input = InputEditor::new();

        // Short enough that eight one-line notices cannot all fit.
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("more"),
            "an overflowing banner must say lines are hidden above it, not just cut them: \
             {rendered}"
        );
        assert!(
            rendered.contains("Line 7"),
            "the newest line is what stays visible: {rendered}"
        );
    }

    #[test]
    fn a_long_held_error_says_how_long_it_has_waited() {
        let mut state = crate::tui::state::TuiState::new(status());
        state.notify_error("could not store credential: keychain locked");
        // Back-date it well past the plain-notice timeout: a held error has
        // no timeout of its own, so this is purely about what the hint says,
        // not about anything expiring.
        state.pending_messages[0].at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(125))
            .unwrap();
        let input = InputEditor::new();

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("held 2m"),
            "a notice that has sat for two minutes must not look freshly posted: {rendered}"
        );
    }

    #[test]
    fn login_provider_completion_popup_renders_above_the_input_box() {
        let mut state = crate::tui::state::TuiState::new(status());
        let mut input = InputEditor::new();
        for ch in "/login ".chars() {
            input.handle(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(ch),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("anthropic"),
            "provider completion menu shows anthropic: {rendered}"
        );
        assert!(
            rendered.contains("zai"),
            "provider completion menu shows zai: {rendered}"
        );
    }

    #[test]
    fn a_provider_picker_renders_every_choice() {
        let mut state = crate::tui::state::TuiState::new(status());
        state.pending_picker = Some(crate::tui::state::PickerDisplay {
            selected: 0,
            stage: crate::tui::state::PickerStage::Providers {
                profiles: PROFILES.iter().collect(),
            },
        });
        let input = InputEditor::new();

        // Tall enough for a title row plus every registry entry, however many
        // that grows to, plus the status line and input box borders around it.
        let backend = TestBackend::new(40, PROFILES.len() as u16 + 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        // The list floats above the prompt; every provider label is in it.
        for profile in PROFILES {
            assert!(rendered.contains(profile.label), "{rendered}");
        }
    }

    #[test]
    fn a_model_picker_renders_the_chosen_providers_models() {
        let profile = &PROFILES[0];
        let mut state = crate::tui::state::TuiState::new(status());
        state.pending_picker = Some(crate::tui::state::PickerDisplay {
            selected: 0,
            stage: crate::tui::state::PickerStage::Models {
                profile,
                dynamic_models: None,
            },
        });
        let input = InputEditor::new();

        // Tall enough for the floating popup to show every model above the prompt.
        let backend = TestBackend::new(40, profile.models.len() as u16 + 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        for model in profile.models {
            assert!(rendered.contains(model.name), "{rendered}");
        }
    }

    #[test]
    fn a_model_picker_renders_live_copilot_models_with_their_context_window() {
        // The dynamic path is what GitHub Copilot actually uses: `/model`
        // fetches the account's real catalog online, and each row shows the
        // model's friendly name with its context window -- not the bare id
        // the chat endpoint wants, which lives only in the selection.
        let profile = ProviderId::GithubCopilot.profile();
        let dynamic = vec![
            provider::CopilotModel {
                id: "gpt-4o".into(),
                name: Some("GPT-4o".into()),
                multiplier: Some(0.0),
                max_context_window_tokens: Some(128_000),
                reasoning_effort: None,
            },
            provider::CopilotModel {
                id: "claude-opus-4-6".into(),
                name: None,
                multiplier: Some(3.0),
                max_context_window_tokens: Some(200_000),
                reasoning_effort: None,
            },
        ];
        let mut state = crate::tui::state::TuiState::new(status());
        state.pending_picker = Some(crate::tui::state::PickerDisplay {
            selected: 0,
            stage: crate::tui::state::PickerStage::Models {
                profile,
                dynamic_models: Some(dynamic),
            },
        });
        let input = InputEditor::new();

        let backend = TestBackend::new(48, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        // A named model shows its friendly name plus its context window.
        assert!(rendered.contains("GPT-4o (128K)"), "{rendered}");
        // A model the API sent no name for falls back to its id.
        assert!(rendered.contains("claude-opus-4-6 (200K)"), "{rendered}");
    }

    #[test]
    fn picker_window_keeps_the_selection_visible_as_it_scrolls() {
        // 44 models (the live Copilot catalog) in a 19-row window. The
        // selection starts at the top of the first page, then sticks to the
        // bottom of the window once it scrolls past it -- never off-screen,
        // which is the "Down moves a highlight no one can see" bug.
        assert_eq!(picker_window_start(0, 44, 19), 0);
        assert_eq!(picker_window_start(18, 44, 19), 0, "still on page one");
        assert_eq!(
            picker_window_start(19, 44, 19),
            1,
            "scrolls one as 19 reaches the edge"
        );
        assert_eq!(
            picker_window_start(43, 44, 19),
            25,
            "pinned to the last page"
        );
        // The selected row is always within [start, start + window).
        for sel in 0..44 {
            let start = picker_window_start(sel, 44, 19);
            assert!(
                start <= sel && sel < start + 19,
                "sel {sel} scrolled off: start {start}"
            );
        }
    }

    #[test]
    fn picker_window_starts_at_zero_when_everything_fits() {
        // Fewer rows than the window: no scrolling, start pinned to 0 so a short
        // provider list is not pushed down artificially.
        assert_eq!(picker_window_start(0, 3, 10), 0);
        assert_eq!(picker_window_start(2, 3, 10), 0);
    }

    #[test]
    fn picker_matching_filters_case_insensitively_on_name_or_id() {
        // The filter narrows the list the selection moves through: an empty query
        // keeps every row, a substring matches the display name or the id (so
        // `gpt-4o` finds models whose id carries it even if the name is
        // prettified), and matching is case-insensitive. Indices are positions
        // in the full stage list, which is what confirmation resolves.
        use crate::tui::state::{PickerDisplay, PickerStage};
        let dynamic = vec![
            provider::CopilotModel {
                id: "gpt-4o".into(),
                name: Some("GPT-4o".into()),
                multiplier: None,
                max_context_window_tokens: None,
                reasoning_effort: None,
            },
            provider::CopilotModel {
                id: "gpt-4o-mini".into(),
                name: Some("GPT-4o mini".into()),
                multiplier: None,
                max_context_window_tokens: None,
                reasoning_effort: None,
            },
            provider::CopilotModel {
                id: "claude-sonnet-4.6".into(),
                name: Some("Claude Sonnet 4.6".into()),
                multiplier: None,
                max_context_window_tokens: None,
                reasoning_effort: None,
            },
        ];
        let profile = ProviderId::GithubCopilot.profile();
        let picker = PickerDisplay {
            selected: 0,
            stage: PickerStage::Models {
                profile,
                dynamic_models: Some(dynamic),
            },
        };
        assert_eq!(picker.matching(""), vec![0, 1, 2]);
        assert_eq!(
            picker.matching("sonnet"),
            vec![2],
            "matches the display name"
        );
        assert_eq!(picker.matching("GPT"), vec![0, 1], "case-insensitive");
        assert_eq!(picker.matching("gpt-4o"), vec![0, 1], "matches the id too");
        assert!(picker.matching("zzz").is_empty(), "a miss matches nothing");
    }

    #[test]
    fn a_finished_tools_body_is_hidden_until_expanded() {
        let view = ToolView {
            call: CallId::new("c".into()),
            name: "read".into(),
            args_summary: "/a".into(),
            status: ToolStatus::Done,
            body: Some("secret body".into()),
            collapsed: true,
        };
        let text = item_text_with(&Item::Tool(view), false).to_string();
        assert!(!text.contains("secret body"), "collapsed hides the body");

        let mut expanded = ToolView {
            collapsed: false,
            ..ToolView {
                call: CallId::new("c".into()),
                name: "read".into(),
                args_summary: "/a".into(),
                status: ToolStatus::Done,
                body: Some("secret body".into()),
                collapsed: true,
            }
        };
        expanded.collapsed = false;
        let text = item_text_with(&Item::Tool(expanded), false).to_string();
        assert!(text.contains("secret body"), "expanded shows the body");
    }

    #[test]
    fn a_collapsed_tool_with_a_body_hints_at_ctrl_o() {
        let hidden = ToolView {
            call: CallId::new("c".into()),
            name: "read".into(),
            args_summary: "/a".into(),
            status: ToolStatus::Done,
            body: Some("secret body".into()),
            collapsed: true,
        };
        assert!(
            item_text_with(&Item::Tool(hidden), false)
                .to_string()
                .contains("Ctrl+O"),
            "a collapsed body with nothing to say so is undiscoverable"
        );

        let expanded = ToolView {
            collapsed: false,
            ..ToolView {
                call: CallId::new("c".into()),
                name: "read".into(),
                args_summary: "/a".into(),
                status: ToolStatus::Done,
                body: Some("secret body".into()),
                collapsed: true,
            }
        };
        assert!(
            !item_text_with(&Item::Tool(expanded), false)
                .to_string()
                .contains("Ctrl+O"),
            "nothing is hidden once expanded, so the hint has nothing to point at"
        );

        let no_body = ToolView {
            call: CallId::new("c".into()),
            name: "read".into(),
            args_summary: "/a".into(),
            status: ToolStatus::Running,
            body: None,
            collapsed: true,
        };
        assert!(
            !item_text_with(&Item::Tool(no_body), false)
                .to_string()
                .contains("Ctrl+O"),
            "there is nothing behind a still-running call to point at either"
        );
    }

    #[test]
    fn a_timed_out_tool_reads_as_a_failure_not_as_running() {
        let running = ToolView {
            call: CallId::new("c".into()),
            name: "shell".into(),
            args_summary: "sleep 30".into(),
            status: ToolStatus::Running,
            body: None,
            collapsed: true,
        };
        let timed_out = ToolView {
            status: ToolStatus::TimedOut,
            ..ToolView {
                call: CallId::new("c".into()),
                name: "shell".into(),
                args_summary: "sleep 30".into(),
                status: ToolStatus::Running,
                body: None,
                collapsed: true,
            }
        };
        let running_style = item_text_with(&Item::Tool(running), false).lines[0].spans[0].style;
        let timed_out_style = item_text_with(&Item::Tool(timed_out), false).lines[0].spans[0].style;
        assert_eq!(running_style.fg, Some(Color::Yellow));
        assert_eq!(
            timed_out_style.fg,
            Some(Color::Red),
            "a timeout is a failure, not a still-running call that happens to share its color"
        );
    }

    #[test]
    fn an_error_item_renders_red() {
        let text = item_text_with(&Item::Error("boom".into()), false);
        // The to_string loses style, so assert the text here and the style in
        // the span below. The two `let _ = ...` bindings that used to close this
        // test asserted nothing at all, despite a comment claiming an
        // end-to-end touch.
        assert!(text.to_string().contains("error: boom"));
        let style = text.lines[0].spans[0].style;
        assert_eq!(style.fg, Some(Color::Red), "an error must read as an error");
    }

    #[test]
    fn a_long_model_line_is_wrapped_rather_than_clipped() {
        // **The bug that made the interface unusable for real replies.** Model
        // output is one long line per paragraph, and ratatui truncates what it
        // cannot fit, so on an 80-column terminal everything past the width was
        // invisible with nothing on screen to say so.
        let mut state = crate::tui::state::TuiState::new(status());
        let tail = "SENTINEL_AT_THE_END";
        reduce(
            &mut state,
            engine::UiEvent::TextDelta {
                text: format!("{} {tail}", "filler ".repeat(30)),
            },
        );

        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("backend");
        let input = InputEditor::new();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .expect("draw");
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();

        assert!(
            rendered.contains(tail),
            "text past the terminal width was clipped:\n{rendered}"
        );
    }

    /// Draw one frame at `width` x `height` and return the buffer's symbols.
    fn render(
        state: &mut crate::tui::state::TuiState,
        input: &InputEditor,
        size: (u16, u16),
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).expect("backend");
        terminal
            .draw(|frame| view(state, input, frame))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect()
    }

    #[test]
    fn a_multi_line_buffer_renders_every_line_of_itself() {
        // The box is sized from the buffer, so a pasted block that renders only
        // its first line is indistinguishable from one that was truncated on the
        // way in, and the user sends what they cannot see.
        let mut state = crate::tui::state::TuiState::new(status());
        let mut input = InputEditor::new();
        input.insert_str("alpha\nbeta\ngamma");

        let rendered = render(&mut state, &input, (30, 12));
        for line in ["alpha", "beta", "gamma"] {
            assert!(rendered.contains(line), "{line} is missing:\n{rendered}");
        }
    }

    #[test]
    fn scrolling_back_reveals_what_the_tail_view_hid() {
        // Tail-only is the state this replaces: everything above the fold was
        // unreachable for the whole session.
        let mut state = crate::tui::state::TuiState::new(status());
        for index in 0..40 {
            state
                .transcript
                .push(Item::Assistant(format!("line-number-{index}")));
        }

        let tail = render(&mut state, &InputEditor::new(), (30, 12));
        assert!(tail.contains("line-number-39"), "the tail is in view");
        assert!(
            !tail.contains("line-number-0 "),
            "the top is above the fold"
        );

        state.scroll_up = 100;
        let scrolled = render(&mut state, &InputEditor::new(), (30, 12));
        assert!(scrolled.contains("line-number-0"), "{scrolled}");
        // And the offset is clamped to what the content can support, so the
        // PageDowns it takes to get back match the PageUps that got here.
        assert!(
            state.scroll_up < 100,
            "an unclamped offset banks scroll nothing spends"
        );
    }

    #[test]
    fn wrapped_height_counts_the_rows_a_long_line_takes() {
        // The scroll offset is computed from this. Using the *unwrapped* height
        // (what `Text::height` gives) under-counts, so tail-scroll stops short
        // and the newest output sits below the fold: the reply appears to end
        // mid-sentence.
        let text = Text::from("aaaa bbbb cccc dddd");
        assert_eq!(wrapped_height(&text, 100), 1, "it fits on one row");
        assert_eq!(wrapped_height(&text, 10), 2, "two words per row");
        assert!(
            wrapped_height(&Text::from("x".repeat(25)), 10) >= 3,
            "a single long word breaks inside itself"
        );
    }

    #[test]
    fn a_tool_body_is_reachable_once_expanded() {
        // `collapsed` was set at construction and never cleared, so this branch
        // was dead code in the running app: a denial reason or error message
        // could not be read at all.
        let view = ToolView {
            call: CallId::new("toolu_1".into()),
            name: "shell".into(),
            args_summary: "cargo test".into(),
            status: ToolStatus::Denied,
            body: Some("a remembered rule denies this".into()),
            collapsed: true,
        };
        assert!(
            !tool_text_with(&view, false)
                .to_string()
                .contains("denies this"),
            "collapsed hides the body"
        );

        let expanded = ToolView {
            collapsed: false,
            ..view
        };
        assert!(
            tool_text_with(&expanded, false)
                .to_string()
                .contains("denies this"),
            "expanded must show why the call was refused"
        );
    }

    #[test]
    fn an_effort_slider_renders_the_slider_track_and_selected_level() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let profile = provider::ProviderId::Anthropic.profile();
        let mut state = crate::tui::state::TuiState::new(status());
        state.pending_picker = Some(crate::tui::state::PickerDisplay {
            selected: 4,
            stage: crate::tui::state::PickerStage::Effort {
                profile,
                model: "claude-opus-4-8".into(),
                levels: vec![
                    "off".into(),
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                    "max".into(),
                ],
            },
        });
        let input = InputEditor::new();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(rendered.contains("Effort:"), "{rendered}");
        assert!(rendered.contains("xhigh"), "{rendered}");
        assert!(rendered.contains("claude-opus-4-8"), "{rendered}");
    }

    #[test]
    fn a_session_picker_renders_active_session_summary_and_past_sessions() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = crate::tui::state::TuiState::new(status());
        state.session_id = "curr123".into();
        state.status.total_tokens = 2500;
        state
            .transcript
            .push(Item::User("first user request".into()));
        state.transcript.push(Item::Assistant("reply".into()));
        state.usage.push(journal::Usage {
            input: 1000,
            output: 1500,
            cache_read: 0,
            cache_write: 0,
        });

        let rows = vec![
            crate::tui::state::SessionRow {
                age: "10m ago".into(),
                id: "sess_past_1".into(),
                preview: "earlier work on login".into(),
            },
            crate::tui::state::SessionRow {
                age: "2d ago".into(),
                id: "sess_past_2".into(),
                preview: "benchmarks".into(),
            },
        ];
        state.pending_picker = Some(crate::tui::state::PickerDisplay {
            selected: 0,
            stage: crate::tui::state::PickerStage::Sessions { rows },
        });
        let input = InputEditor::new();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(&mut state, &input, frame))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();

        // The active session summary is displayed in the header.
        assert!(
            rendered.contains("curr123"),
            "renders active session id: {rendered}"
        );
        assert!(
            rendered.contains("just now"),
            "renders active session age: {rendered}"
        );
        assert!(
            rendered.contains("1 turn"),
            "renders turn count: {rendered}"
        );
        assert!(
            rendered.contains("2500 tokens"),
            "renders total tokens: {rendered}"
        );
        assert!(
            rendered.contains("first user request"),
            "renders preview: {rendered}"
        );

        // Past sessions are listed below the header with their ages.
        assert!(
            rendered.contains("sess_past_1"),
            "renders past session 1: {rendered}"
        );
        assert!(
            rendered.contains("10m ago"),
            "renders past session 1 age: {rendered}"
        );
        assert!(
            rendered.contains("earlier work on login"),
            "renders past session preview: {rendered}"
        );
        assert!(
            rendered.contains("sess_past_2"),
            "renders past session 2: {rendered}"
        );
        assert!(
            rendered.contains("2d ago"),
            "renders past session 2 age: {rendered}"
        );
    }

    #[test]
    fn wrap_prompt_soft_wraps_long_lines_and_places_cursor() {
        let text = "The quick brown fox jumps over the lazy dog";
        // Width 15: "The quick brown" (15), "fox jumps over" (14), "the lazy dog" (12)
        let (lines, cursor_row, cursor_col) = wrap_prompt(text, 0, 15);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "The quick brown");
        assert_eq!(lines[1], "fox jumps over");
        assert_eq!(lines[2], "the lazy dog");
        assert_eq!((cursor_row, cursor_col), (0, 0));

        // Cursor at end of buffer
        let (_, end_row, end_col) = wrap_prompt(text, text.len(), 15);
        assert_eq!((end_row, end_col), (2, 12));

        // Cursor in the middle ("brown" starts at byte 10 on row 0)
        let (_, mid_row, mid_col) = wrap_prompt(text, 10, 15);
        assert_eq!((mid_row, mid_col), (0, 10));

        // Cursor at start of second visual line ("fox" at byte 16)
        let (_, f_row, f_col) = wrap_prompt(text, 16, 15);
        assert_eq!((f_row, f_col), (1, 0));
    }

    #[test]
    fn wrap_prompt_breaks_long_words_without_spaces() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let (lines, _, _) = wrap_prompt(text, 0, 10);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "abcdefghij");
        assert_eq!(lines[1], "klmnopqrst");
        assert_eq!(lines[2], "uvwxyz");

        // Cursor at byte 12 ('m')
        let (_, row, col) = wrap_prompt(text, 12, 10);
        assert_eq!((row, col), (1, 2));
    }

    #[test]
    fn wrap_prompt_counts_a_wide_character_as_two_cells() {
        // At width 5 two CJK characters (4 cells) fill a row: a third would
        // need 6. Counted as one cell each, all three shared a row ratatui
        // then clipped, and the cursor sat half-way along the text.
        let text = "漢字漢字漢字";
        let (lines, row, col) = wrap_prompt(text, text.len(), 5);
        assert_eq!(lines, vec!["漢字", "漢字", "漢字"]);
        assert_eq!((row, col), (2, 4));
        let (_, row, col) = wrap_prompt(text, "漢".len(), 5);
        assert_eq!((row, col), (0, 2));
    }

    #[test]
    fn wrap_prompt_preserves_newlines_and_trailing_empty_line() {
        let text = "first line\nsecond line\n";
        let (lines, row, col) = wrap_prompt(text, text.len(), 20);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "first line");
        assert_eq!(lines[1], "second line");
        assert_eq!(lines[2], "");
        assert_eq!((row, col), (2, 0));
    }

    #[test]
    fn a_long_prompt_soft_wraps_in_the_input_box_without_clipping() {
        let mut state = crate::tui::state::TuiState::new(status());
        let mut input = InputEditor::new();
        let prompt = "This is a very long prompt that exceeds thirty columns and must soft wrap";
        input.insert_str(prompt);

        // Render at terminal width 30
        let rendered = render(&mut state, &input, (30, 12));
        assert!(
            rendered.contains("This is a very"),
            "first line of prompt rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("soft wrap"),
            "tail of long prompt is visible (not clipped):\n{rendered}"
        );
    }
}
