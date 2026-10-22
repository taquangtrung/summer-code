//! The TUI driver.
//!
//! One task owns the terminal. It selects over crossterm's async event stream
//! (keys, resize), the engine's [`UiEvent`] receiver, and the broker's prompt
//! channel. A submitted line spawns `engine.turn` on a task with a fresh
//! [`Observer`](engine::Observer); the driver drains events into [`reduce`] and
//! redraws, then takes the engine back when the task joins.
//!
//! Two things this module gets right that are easy to get wrong:
//!
//! - **Fullscreen alternate screen.** The driver takes over the whole terminal
//!   window — the Claude Code and Pi feel — by enabling raw mode, entering the
//!   alternate screen, and rendering a fullscreen viewport. The trade-off versus
//!   an inline viewport is that the transcript does not survive in the shell's
//!   scrollback after the session: the alternate screen is restored on exit, so
//!   the shell picks up exactly where it was.
//! - **Panic-safe teardown.** A panic in raw mode without restoring the terminal
//!   leaves the shell wrecked, so the panic hook leaves the alternate screen and
//!   disables raw mode before the default hook prints.
//! - **Bracketed paste on, wheel reporting on.** Paste is enabled because
//!   without it a multi-line paste arrives as keystrokes and every embedded
//!   Enter submits, so pasting a stack trace starts a turn per line. The wheel
//!   is reported (see [`mouse`]) because without it the alternate screen turns
//!   each notch into Up or Down, which scrolled the prompt's history rather than
//!   the transcript under the pointer. The cost is that a plain drag no longer
//!   selects text; Shift+drag does, in every mainstream terminal emulator.

use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use engine::{Engine, UiEvent};
use futures_util::StreamExt;
use journal::CallId;
use provider::{Effort, ProviderId};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    SetTitle, disable_raw_mode, enable_raw_mode,
};
use sandbox::Grant;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tools::ShellSpec;

use crate::credentials;
use crate::mode::{ExecutionMode, Mode};
use crate::notify::ring;
use crate::tui::detail::{handle_detail_key, toggle_detail};
use crate::tui::input::{EditMode, InputAction, InputEditor};
use crate::tui::keybinding::keybindings_help_text;
use crate::tui::login::{handle_secret_key, login, run_device_flow_login};
use crate::tui::mention::expand_at_file_mentions;
use crate::tui::mouse::{DisableWheel, EnableWheel, wheel, wheel_key};
use crate::tui::picker::{
    effort_for, handle_picker_key, open_model_picker, open_model_picker_for, set_effort,
};
use crate::tui::prompt::{Answer, answer_for_key, answer_for_selection, grant_for};
use crate::tui::session::{
    fold_journal_into, learn_session_title, open_rewind_picker, open_session_picker,
    resume_session, session_name, sessions_text,
};
use crate::tui::shell::ShellRun;
use crate::tui::slash::{Command, parse_command};
use crate::tui::state::{
    Item, PromptDisplay, SecretInput, Status, ToolStatus, ToolView, TuiState, reduce, truncate,
};
use crate::tui::view::view;
use crate::tui::workspace::{
    compact_history, create_workspace_checkpoint, restore_workspace_checkpoint, rewind_workspace,
    show_usage_dashboard, show_workspace_diff,
};

mod detail;
mod export;
mod file_index;
mod history;
mod input;
mod keybinding;
mod login;
mod markdown;
mod mention;
mod mouse;
mod picker;
mod prompt;
mod session;
mod shell;
mod slash;
mod state;
mod statusline;

#[cfg(test)]
mod testing;
mod view;
mod workspace;

pub use prompt::{PromptRequest, TuiPrompter};
pub use slash::help_text;
pub(crate) use state::summarize_args;
pub use statusline::Statusline;
pub(crate) use view::{SPINNER_INTERVAL, progress_label, spinner_frame};

// ========================================================================
// Data Structures
// ========================================================================

/// A permission prompt the broker asked and the driver has not yet answered.
struct PendingPrompt {
    rule: Option<policy::RuleSpec>,
    reply: tokio::sync::oneshot::Sender<Grant>,
}

// ========================================================================
// Functions
// ========================================================================

/// Run the TUI to completion. Owns the terminal for the whole session.
///
/// `prompt_rx` is the receiving end of the [`crate::tui::prompt::TuiPrompter`]
/// the broker was built with on the TUI path. Permission prompts flow here
/// rather than over the [`UiEvent`] channel, because a prompt needs an answer.
/// Everything the TUI needs for a session besides the engine itself.
///
/// A struct rather than a parameter list: the list reached eleven, at which
/// point the order of two adjacent `Option`s is something a caller gets wrong
/// silently. Named fields make a mis-wiring a compile error.
pub struct Session {
    pub backend_name: &'static str,
    pub commands: crate::command::Registry,
    pub context_window: Option<u64>,
    /// A journal to fold in before the first turn (`summer -c`): its events
    /// give the model the prior context and rebuild the on-screen transcript.
    pub resume: Option<std::path::PathBuf>,
    /// Where this workspace's session journals live, chosen at bootstrap
    /// (central per-project store, or in-project under `--sandbox`); the one
    /// path `/sessions`, `/resume`, and `/status` all read.
    pub sessions_dir: std::path::PathBuf,
    pub effort: Option<Effort>,
    pub keymap: crate::keybindings::Keymap,
    pub mode: Mode,
    /// The broker's read-only gate, closed while the session is in plan mode.
    pub read_only_gate: std::sync::Arc<AtomicBool>,
    pub policy: String,
    pub prompt_rx: UnboundedReceiver<PromptRequest>,
    pub provider: String,
    pub provider_id: ProviderId,
    pub snapshots: std::sync::Arc<engine::Snapshots>,
    pub statusline: Statusline,
    /// Ring the terminal bell / send a desktop notification when a turn ends.
    /// From `config.notify`, defaulting on.
    pub notify: bool,
}

pub async fn run(mut engine: Engine, session: Session) -> Result<(), Box<dyn Error>> {
    let Session {
        backend_name,
        commands,
        context_window,
        resume,
        sessions_dir,
        effort,
        keymap,
        mode,
        read_only_gate,
        policy,
        mut prompt_rx,
        provider,
        provider_id,
        snapshots,
        statusline,
        notify,
    } = session;
    let session_id = engine.session_id().to_string();
    let status = Status {
        model: engine.model().to_string(),
        effort: effort.map(|e| e.as_str().to_owned()),
        execution_mode: ExecutionMode::from(mode),
        total_tokens: 0,
        context_tokens: 0,
        // The registry wins for pinned models; the cached value covers a live
        // Copilot model the registry does not list, so `{ctx}` always has a
        // window to divide by.
        context_window: provider::context_window_for(engine.model()).or(context_window),
        backend: backend_name.to_string(),
        cwd: crate::display_path(engine.cwd()),
    };
    let mut state = TuiState::new(status);
    state.context_budget = Some(u64::from(engine.context_budget()));
    // Printed before the alternate screen opens, the mode's banner was never
    // on screen in the TUI, so `--mode full` ran with its warning unseen.
    if let Some(banner) = mode.banner() {
        state.notify(banner);
    }
    state.statusline = statusline;
    state.provider_id = provider_id;
    // Restore the saved effort for the startup model so the first turn already
    // uses it and the statusline shows it -- the same as a `/effort` set live.
    if let Some(effort) = effort {
        state.efforts.insert(state.status.model.clone(), effort);
    }
    state.session_id = session_id;
    state.sessions_dir = sessions_dir.display().to_string();
    state.provider = provider;
    state.policy = policy;
    // `summer -c`: fold the newest journal in before the first turn and open
    // on its transcript, so continuing a session is one flag rather than a
    // restart plus an id copied out of `/sessions`.
    if let Some(path) = resume {
        match fold_journal_into(&mut engine, &path, &mut state) {
            Ok(_) => {}
            Err(error) => state.notify_error(format!("continue: {}: {error}", path.display())),
        }
    }
    let history_dir = crate::policy::config_dir();
    let mut input = InputEditor::new()
        .with_keymap(keymap)
        .with_user_commands(&commands)
        .with_history(
            history_dir
                .as_deref()
                .map(history::load)
                .unwrap_or_default(),
        );
    let mut events = EventStream::new();

    install_panic_hook();
    enable_raw_mode()?;
    // Take over the whole window: the alternate screen hides the shell behind
    // the TUI and is restored on every exit path below (a panic is covered by
    // the hook installed above).
    if let Err(error) = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableWheel
    ) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    claim_title(&state, false);
    // The `@` index, built off the UI thread so the first `@` does not wait.
    std::thread::spawn(file_index::warm);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(error) => {
            restore_terminal();
            return Err(error.into());
        }
    };

    // A command statusline runs in a task, off the render path, and streams its
    // output back here; the built-in statusline leaves the channels idle. The
    // task owns the receiver end of `snap_*`; the driver keeps `snap_tx` (to push
    // usage updates) and `line_rx` (to read the rendered line).
    let (snap_tx, snap_rx) = tokio::sync::mpsc::unbounded_channel::<Status>();
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some((command, shell)) = state.statusline.command() {
        let (command, shell) = (command.to_owned(), shell.clone());
        let _ = snap_tx.send(state.status.clone());
        tokio::spawn(run_statusline_task(command, shell, snap_rx, line_tx));
    } else {
        drop(snap_rx);
        drop(line_tx);
    }

    let mut engine_slot: Option<Engine> = Some(engine);
    let mut cx = DriverCx {
        commands: &commands,
        snapshots: &snapshots,
        terminal: &mut terminal,
        engine_slot: &mut engine_slot,
        state: &mut state,
        input: &mut input,
        events: &mut events,
        prompt_rx: &mut prompt_rx,
        line_rx: &mut line_rx,
        snap_tx: &snap_tx,
        read_only_gate: &read_only_gate,
        start_mode: mode,
        notify,
    };
    let outcome = drive(&mut cx).await;
    // The background session title, so it reaches the journal before exit.
    if let Some(engine) = engine_slot.as_mut() {
        engine.settle().await;
    }

    // Restore the shell: leave the alternate screen and drop raw mode so the
    // shell's prompt and echo return.
    restore_terminal();
    outcome
}

/// The driver's mutable collaborators, bundled so the loop functions stay
/// readable (and under clippy's argument threshold) as wake sources are added.
/// `drive` reborrows it into `run_turn`.
struct DriverCx<'a> {
    /// The user's saved-prompt commands, consulted before an unrecognized
    /// `/name` is reported as unknown.
    commands: &'a crate::command::Registry,
    /// The pre-write file states `/rewind` restores from. The same store the
    /// broker fills, so what the agent wrote is what can be taken back.
    snapshots: &'a std::sync::Arc<engine::Snapshots>,
    terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    engine_slot: &'a mut Option<Engine>,
    state: &'a mut TuiState,
    input: &'a mut InputEditor,
    events: &'a mut EventStream,
    prompt_rx: &'a mut UnboundedReceiver<PromptRequest>,
    line_rx: &'a mut UnboundedReceiver<String>,
    snap_tx: &'a UnboundedSender<Status>,
    /// The broker's read-only gate, which [`switch_mode`] keeps in step with
    /// the execution mode.
    read_only_gate: &'a AtomicBool,
    /// The `--mode` the session started in; it bounds what `/mode` may switch
    /// to.
    start_mode: Mode,
    /// Ring the terminal bell / send a desktop notification when a turn ends.
    notify: bool,
}

/// The subset of [`DriverCx`] the `/model` picker touches. Splitting it out is
/// what lets the picker run under a test without a TTY: [`EventStream`] cannot
/// be constructed headlessly, and the picker never reads it, so it does not
/// belong in the context the picker receives.
struct PickerCx<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    engine_slot: &'a mut Option<Engine>,
    state: &'a mut TuiState,
    input: &'a mut InputEditor,
}

impl<'a> DriverCx<'a> {
    /// Borrow the fields the picker needs, leaving the event stream and the
    /// statusline channels with the driver.
    fn picker(&mut self) -> PickerCx<'_> {
        PickerCx {
            terminal: self.terminal,
            engine_slot: self.engine_slot,
            state: self.state,
            input: self.input,
        }
    }
}

/// Switch the execution mode to `target`, or say why the startup mode refuses.
///
/// The one place a switch happens, so the broker's read-only gate cannot fall
/// out of step with what the status line claims.
fn switch_mode(cx: &mut DriverCx<'_>, target: ExecutionMode) {
    if let Some(reason) = cx.start_mode.refusal(target) {
        cx.state.notify_error(reason);
        return;
    }
    cx.read_only_gate
        .store(target == ExecutionMode::Plan, Ordering::Relaxed);
    cx.state.execution_mode = target;
    cx.state.status.execution_mode = target;
    let _ = cx.snap_tx.send(cx.state.status.clone());
    cx.state
        .notify(format!("Switched to {}", target.description()));
}

/// Shift+Tab and a bare `/mode`: step to the next mode the startup mode
/// allows. When none is allowed, the next one's refusal says why.
fn cycle_mode(cx: &mut DriverCx<'_>) {
    let current = cx.state.execution_mode;
    let target = current
        .next_allowed(cx.start_mode)
        .unwrap_or(current.cycle_next());
    switch_mode(cx, target);
}

/// The idle event loop: redraw, then wait on the next wake — a statusline
/// output update or a terminal event. Handling the wake outside `select!` (via
/// [`Wake`]) lets `run_turn` reborrow `cx` without contending with the select's
/// own borrows on `cx.events` and `cx.line_rx`.
async fn drive(cx: &mut DriverCx<'_>) -> Result<(), Box<dyn Error>> {
    // The `!` command running now, if any: its transcript row and its task.
    let mut shell_job: Option<ShellJob> = None;
    // When a lone Esc last landed on an empty prompt, so a second one soon
    // after opens the rewind picker.
    let mut last_escape: Option<Instant> = None;
    loop {
        redraw(cx.terminal, cx.state, cx.input)?;
        let woke = tokio::select! {
            biased;
            run = finished_shell(&mut shell_job) => Wake::Shell(run),
            line = statusline_output(cx.line_rx) => Wake::Line(line),
            event = cx.events.next() => Wake::Event(event),
            _ = expire_after(cx.state.banner_expiry()) => Wake::ExpireBanner,
            _ = expire_after(cx.state.auto_accepted_expiry()) => Wake::ExpireAuto,
        };
        match woke {
            Wake::Shell(run) => {
                if let Some(job) = shell_job.take() {
                    finish_shell(cx.state, &job.call, run);
                }
            }
            Wake::Line(line) => cx.state.statusline.set_output(line),
            Wake::ExpireBanner => cx.state.expire_banner(),
            Wake::ExpireAuto => cx.state.clear_auto_accepted(),
            Wake::Event(None) => break,
            Wake::Event(Some(Err(error))) => return Err(error.into()),
            // A bracketed paste is text, not keystrokes. Routed to `insert_str`
            // so its newlines stay in the buffer: replayed as key events, each
            // one would have submitted a turn.
            Wake::Event(Some(Ok(Event::Paste(text)))) => route_paste(cx.state, cx.input, &text),
            // An open picker owns the wheel as it owns the keys: a notch moves
            // its selection rather than the prompt underneath it.
            Wake::Event(Some(Ok(Event::Mouse(mouse)))) => match wheel_key(&mouse) {
                Some(key) if cx.state.pending_picker.is_some() => {
                    handle_picker_key(&mut cx.picker(), &key).await?;
                }
                _ => wheel(cx, &mouse),
            },
            Wake::Event(Some(Ok(event))) => {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    // Starting a fresh command clears whatever notification
                    // is still showing: a new command's own result replaces
                    // it, and advice left over from a moment ago (or a
                    // finished login's messages) has no bearing on what is
                    // about to be typed. A held error banner is the
                    // exception -- it leaves by Esc, not by the user merely
                    // starting to type something else.
                    if starts_a_fresh_command(&key, cx.input.buffer()) && !cx.state.banner_sticky()
                    {
                        cx.state.clear_banner();
                    }
                    // An open API-key entry owns every key, the same
                    // interception pattern a picker or permission prompt
                    // uses: characters build the masked buffer, Enter
                    // stores, Esc cancels.
                    if cx.state.pending_secret.is_some() {
                        handle_secret_key(cx, &key).await?;
                        continue;
                    }
                    // An open `/model` picker owns every key until it is
                    // answered or cancelled, the same way a permission prompt
                    // owns `run_turn`'s keys: arrows move its selection rather
                    // than the text cursor, and Enter confirms a row rather
                    // than submitting the buffer.
                    if cx.state.pending_picker.is_some() {
                        handle_picker_key(&mut cx.picker(), &key).await?;
                        continue;
                    }
                    // Ctrl+O toggles the detail pager from anywhere it can
                    // reach the prompt, and an open pager owns every key until
                    // it is toggled off -- the same intercept pattern a picker
                    // or a permission prompt uses.
                    if is_ctrl_o(&key) {
                        toggle_detail(cx);
                        continue;
                    }
                    if cx.state.detail.is_some() {
                        handle_detail_key(cx, &key).await?;
                        continue;
                    }
                    if is_ctrl_g(&key) {
                        compose_in_external_editor(cx);
                        continue;
                    }
                    // Esc stops a running `!` command, the way it interrupts a
                    // turn; an open completion menu still gets it first.
                    if is_escape(&key)
                        && cx.input.completion().is_none()
                        && let Some(job) = shell_job.take()
                    {
                        job.handle.abort();
                        interrupt_shell(cx.state, &job.call);
                        continue;
                    }
                    // Esc Esc on an empty prompt: go back to an earlier message,
                    // Claude Code's gesture. Only on an empty prompt, so a
                    // draft is never traded for a picker, and not in Vim insert
                    // mode, where Esc means "normal mode".
                    if is_escape(&key)
                        && cx.input.buffer().is_empty()
                        && cx.input.completion().is_none()
                        && cx.input.edit_mode() != EditMode::VimInsert
                    {
                        let now = Instant::now();
                        if last_escape
                            .is_some_and(|at| now.duration_since(at) <= DOUBLE_ESCAPE_WINDOW)
                        {
                            last_escape = None;
                            open_rewind_picker(cx);
                            continue;
                        }
                        last_escape = Some(now);
                    }
                    // One thing at a time: a message sent now would reach the
                    // model without the output it is presumably about.
                    if shell_job.is_some()
                        && key.code == KeyCode::Enter
                        && key.modifiers.is_empty()
                        && cx.input.completion().is_none()
                    {
                        cx.state
                            .notify("a ! command is running: wait for it, or Esc to stop it");
                        continue;
                    }
                    match cx.input.handle(key) {
                        InputAction::Exit => return Ok(()),
                        InputAction::ConfirmExit => {
                            cx.state.notify("press Ctrl-C again to exit");
                        }
                        InputAction::ScrollUp => scroll(cx, true),
                        InputAction::ScrollDown => scroll(cx, false),
                        InputAction::ShowKeybindings => {
                            cx.state.notify(keybindings_help_text());
                        }
                        InputAction::SwitchMode => cycle_mode(cx),
                        InputAction::Submit => {
                            let text = cx.input.take();
                            record_history(&text);
                            if let Some(command) = shell::shell_command(&text) {
                                cx.state.scroll_up = 0;
                                shell_job = Some(start_shell(cx.state, command));
                                continue;
                            }
                            // Submitting means "show me what happens next", so
                            // the view reattaches to the tail however far back
                            // the user had scrolled to read.
                            cx.state.scroll_up = 0;
                            // Slash commands are parsed at the input edge and
                            // never sent to the model. Unknown `/x` is an
                            // inline error. `/resume <id>` and `/login
                            // <provider> [api-key]` need arguments beyond their
                            // name, so their arms re-tokenize `text`
                            // themselves rather than going through
                            // `handle_command`, which only ever sees the
                            // classified `Command`.
                            match parse_command(&text) {
                                Some(Ok(Command::Quit)) => return Ok(()),
                                Some(Ok(Command::Model)) => open_model_picker(cx)?,
                                Some(Ok(Command::Mode)) => match text.split_whitespace().nth(1) {
                                    Some(name) => match name.to_lowercase().as_str() {
                                        "auto" | "full" => switch_mode(cx, ExecutionMode::Auto),
                                        "interactive" | "ask" | "default" => {
                                            switch_mode(cx, ExecutionMode::Interactive)
                                        }
                                        "plan" | "planning" => switch_mode(cx, ExecutionMode::Plan),
                                        other => {
                                            cx.state.notify_error(format!("Unknown mode `{other}` (valid: auto, interactive, plan)"));
                                        }
                                    },
                                    None => cycle_mode(cx),
                                },
                                Some(Ok(Command::Diff)) => show_workspace_diff(cx),
                                Some(Ok(Command::Copy)) => {
                                    copy_to_clipboard(cx, text.split_whitespace().nth(1));
                                }
                                Some(Ok(Command::Export)) => {
                                    export_transcript(cx.state, text.split_whitespace().nth(1));
                                }
                                Some(Ok(Command::Checkpoint)) => create_workspace_checkpoint(cx),
                                Some(Ok(Command::Rewind)) => {
                                    let turns = text
                                        .split_whitespace()
                                        .nth(1)
                                        .and_then(|n| n.parse().ok())
                                        .unwrap_or(1);
                                    rewind_workspace(cx, turns);
                                }
                                Some(Ok(Command::Undo)) => restore_workspace_checkpoint(cx),
                                Some(Ok(Command::Compact)) => compact_history(cx).await,
                                Some(Ok(Command::Keymap)) => {
                                    let arg = text
                                        .strip_prefix("/keymap")
                                        .unwrap_or("")
                                        .trim()
                                        .to_lowercase();
                                    match arg.as_str() {
                                        "vim" => {
                                            cx.input.set_edit_mode(EditMode::VimNormal);
                                            cx.state.notify(
                                                "switched to Vim modal keymap (Normal mode; 'i' for Insert, Esc for Normal)".to_string(),
                                            );
                                        }
                                        "emacs" => {
                                            cx.input.set_edit_mode(EditMode::Emacs);
                                            cx.state.notify(
                                                "switched to Emacs / Readline keymap (default)"
                                                    .to_string(),
                                            );
                                        }
                                        _ => {
                                            let current = match cx.input.edit_mode() {
                                                EditMode::Emacs => "emacs",
                                                EditMode::VimNormal | EditMode::VimInsert => "vim",
                                            };
                                            cx.state.notify(format!(
                                                "current keymap: {current}. Usage: /keymap <emacs|vim>"
                                            ));
                                        }
                                    }
                                }
                                Some(Ok(Command::Usage)) => show_usage_dashboard(cx.state),
                                Some(Ok(Command::Agent)) => {
                                    let prompt = text.trim_start_matches("/agent").trim();
                                    match delegation_prompt(prompt) {
                                        Some(prompt) => run_turn_and_drain(cx, prompt).await?,
                                        None => cx.state.notify("usage: /agent <task>".to_string()),
                                    }
                                }
                                Some(Ok(Command::Review)) => {
                                    let review_prompt =
                                        prepare_code_review_prompt(&cx.state.status.cwd);
                                    if let Some(prompt) = review_prompt {
                                        run_turn_and_drain(cx, prompt).await?;
                                    } else {
                                        cx.state.notify(
                                            "workspace clean (no uncommitted changes to review)"
                                                .to_string(),
                                        );
                                    }
                                }
                                Some(Ok(Command::Statusline)) => {
                                    let fmt = text.trim_start_matches("/statusline").trim();
                                    if fmt.is_empty() || fmt == "reset" {
                                        cx.state.statusline = Statusline::default();
                                        cx.state.notify(
                                            "reset statusline to default format".to_string(),
                                        );
                                    } else {
                                        cx.state.statusline = Statusline::from_template(fmt);
                                        cx.state
                                            .notify(format!("updated statusline format: `{fmt}`"));
                                    }
                                }
                                Some(Ok(Command::Resume)) => {
                                    let id = text.split_whitespace().nth(1).unwrap_or("");
                                    if id.is_empty() {
                                        // No id named: pick one, the same way
                                        // `/model` picks a model — newest
                                        // first, the prompt filters.
                                        open_session_picker(cx);
                                    } else {
                                        resume_session(cx.engine_slot, cx.state, id.to_string())?;
                                    }
                                }
                                Some(Ok(Command::Login)) => {
                                    let mut args = text.split_whitespace().skip(1);
                                    let provider = args.next();
                                    let token = args.next();
                                    // A confined session cannot write
                                    // ~/.summer/auth.json (outside the root
                                    // the sandbox confines this process to),
                                    // so every /login path is a dead end:
                                    // say so before a key is typed or a
                                    // device flow is run, the same way the
                                    // permission prompt stops offering
                                    // rule-saving under confinement.
                                    if let Some(blocked) =
                                        credentials::login_blocked(crate::confined(), provider)
                                    {
                                        cx.state.notify_error(blocked);
                                    } else {
                                        match credentials::device_flow_provider(provider, token) {
                                            Some(id) => run_device_flow_login(cx, id).await?,
                                            None => {
                                                // `/login <provider>` with no
                                                // token opens the masked key
                                                // entry rather than the usage
                                                // error: a key typed on the
                                                // command line sits in the
                                                // clear in a buffer history
                                                // can recall.
                                                if token.is_none()
                                                    && let Some(id) =
                                                        credentials::secret_prompt_provider(
                                                            provider,
                                                        )
                                                {
                                                    cx.state.pending_secret =
                                                        Some(SecretInput::new(id));
                                                } else if let Some(profile) =
                                                    login(cx.state, provider, token)
                                                {
                                                    open_model_picker_for(
                                                        &mut cx.picker(),
                                                        profile,
                                                    )
                                                    .await?;
                                                }
                                            }
                                        }
                                    }
                                }
                                Some(Ok(Command::Effort)) => {
                                    let level = text.split_whitespace().nth(1).unwrap_or("");
                                    set_effort(cx, level);
                                }
                                Some(Ok(command)) => {
                                    let model = cx.state.status.model.clone();
                                    handle_command(command, &model, cx.state);
                                }
                                // A name the built-in registry does not know
                                // may still be one of the user's own saved
                                // prompts. Checked here rather than inside
                                // `parse_command` so the built-in set stays a
                                // pure function of the enum, and so a user file
                                // can never shadow a session command.
                                Some(Err(name)) => match cx.commands.expand(&text) {
                                    Some(prompt) => run_turn_and_drain(cx, prompt).await?,
                                    None => cx.state.notify_error(format!(
                                        "unknown command: {name} (try /help)"
                                    )),
                                },
                                None => run_turn_and_drain(cx, text).await?,
                            }
                        }
                        // An Esc no surface claimed (the editor consumes
                        // Esc itself to close an open completion menu, and
                        // the picker, key entry, and pager intercept before
                        // it ever reaches the editor) dismisses a held error
                        // banner: the one explicit way an error leaves.
                        InputAction::None if is_escape(&key) && cx.state.banner_sticky() => {
                            cx.state.clear_banner();
                        }
                        InputAction::Edited | InputAction::None => {}
                    }
                }
                // Resize and other events fall through to the next redraw.
            }
        }
    }
    Ok(())
}

/// Run one turn to completion: hand the engine to a task with a fresh observer,
/// drain the [`UiEvent`] stream into the state, and redraw as it arrives. A
/// permission prompt arriving from the broker is rendered inline and answered
/// from the keyboard.
///
/// Run a turn, then send anything queued while it streamed. One wrapper so
/// every turn-starting path (`None`, `/agent`, `/review`) drains the queue
/// identically; without the drain, a queued message would sit unsendable until
/// the next manual submit, which is indistinguishable from having dropped it.
///
/// **An interrupt hands the queue back instead of sending it**, as Claude Code
/// does: someone who pressed Esc wants to change course, and the next queued
/// message was written for the course they just abandoned. A queued slash
/// command is handed back too, because it is a command for the idle loop to
/// run, not text for the model, and so is a `!` line.
async fn run_turn_and_drain(cx: &mut DriverCx<'_>, text: String) -> Result<(), Box<dyn Error>> {
    let mut interrupted = run_turn(cx, text).await?;
    while !cx.state.queued.is_empty() {
        let next = &cx.state.queued[0];
        let next_is_command = next.starts_with('/') || shell::shell_command(next).is_some();
        if interrupted || next_is_command {
            let returned = std::mem::take(&mut cx.state.queued).join("\n");
            cx.input.insert_str(&returned);
            cx.state.notify("queued messages are back in the editor");
            break;
        }
        let next = cx.state.queued.remove(0);
        interrupted = run_turn(cx, next).await?;
    }
    Ok(())
}

/// The engine is moved into the task and moved back out when it joins: the
/// driver cannot touch the engine while the turn owns it, which is the point —
/// the only thing the driver does mid-turn is render, answer prompts, and feed
/// the statusline its usage updates.
///
/// Returns whether the turn was interrupted.
async fn run_turn(cx: &mut DriverCx<'_>, text: String) -> Result<bool, Box<dyn Error>> {
    // Stamped before anything runs, so `/rewind n` has a boundary to restore to.
    // The snapshot manifest is keyed by wall-clock time, and this is the only
    // place that knows where one turn ends and the next begins.
    cx.state.turn_starts.push(std::time::SystemTime::now());
    let cwd = std::path::PathBuf::from(&cx.state.status.cwd);
    let text = expand_at_file_mentions(&cwd, &text, cx.state);
    // After the mention expansion, not before: command output is full of `@`
    // that were never file mentions.
    let text = shell::with_shell_context(&std::mem::take(&mut cx.state.shell_context), &text);
    let mut engine = cx.engine_slot.take().expect("engine present between turns");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    engine.set_observer(Some(tx));
    // A per-turn cancellation token: Ctrl-C cancels it, which propagates to the
    // in-flight tool (killing its child) and, between rounds, ends the turn.
    let cancel = tokio_util::sync::CancellationToken::new();
    engine.set_turn_cancel(Some(cancel.clone()));
    show_title(cx.state, true);
    let task = tokio::spawn(async move {
        let result = engine.turn(text).await;
        engine.set_observer(None);
        engine.set_turn_cancel(None);
        (engine, result)
    });

    let mut pending: Option<PendingPrompt> = None;

    // Wake sources: engine narration, broker prompts, statusline output, and the
    // terminal. `biased` folds narration first; the `None` arm breaks rather than
    // spinning on a closed channel. Keys resolve a pending prompt or, on Ctrl-C,
    // cancel the turn; resize redraws.
    //
    // **Every wake marks the frame dirty, and frames are drawn at most every
    // `FRAME_INTERVAL`.** The loop used to draw once per engine event, so a
    // burst of streamed tokens cost one full frame each, and with narration
    // polled first, keys and Ctrl-C waited behind the whole backlog. Now
    // narration is drained in one go, the frame is drawn once for all of it,
    // and a wake that comes too soon after the last frame waits for the
    // deadline arm instead: one frame late at worst, which no one can see.
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    loop {
        let since_draw = last_draw.map(|drawn| drawn.elapsed());
        if dirty && since_draw.is_none_or(|since| since >= FRAME_INTERVAL) {
            redraw(cx.terminal, cx.state, cx.input)?;
            last_draw = Some(Instant::now());
            dirty = false;
        }
        let frame_wait =
            since_draw.map_or(Duration::ZERO, |since| FRAME_INTERVAL.saturating_sub(since));
        tokio::select! {
            biased;
            event = rx.recv() => match event {
                Some(event) => {
                    fold_event(cx.state, cx.snap_tx, event);
                    while let Ok(event) = rx.try_recv() {
                        fold_event(cx.state, cx.snap_tx, event);
                    }
                }
                None => break,
            },
            request = cx.prompt_rx.recv() => {
                if let Some(request) = request {
                    match cx.state.execution_mode {
                        ExecutionMode::Auto => {
                            // Once, never as a rule: a rule outlives the mode,
                            // so switching back to interactive would keep
                            // approving everything auto had seen.
                            let _ = request.reply.send(Grant::once(true));
                            cx.state.note_auto_accepted(request.description);
                        }
                        ExecutionMode::Plan => {
                            let _ = request.reply.send(Grant::once(false));
                            cx.state.notify(format!("Denied in planning mode: {}", request.description));
                        }
                        ExecutionMode::Interactive => {
                            // **Discard typeahead before the prompt can consume it.**
                            // Keys pressed (or pasted) before the prompt existed were
                            // still queued, and the prompt answered with them: pasting
                            // text containing a `y` while the broker happened to ask
                            // silently granted the permission. An answer has to be a
                            // reply to a question the user has seen.
                            discard_typeahead();
                            // Bring the question into view: a user paged up or
                            // reading the pager would otherwise have keys
                            // captured by a prompt they cannot see.
                            cx.state.scroll_up = 0;
                            cx.state.detail = None;
                            cx.state.pending_prompt = Some(PromptDisplay {
                                armed_at: Instant::now() + PROMPT_ARM_DELAY,
                                description: request.description,
                                diff: request.diff,
                                offer: request.offer,
                                selected: 0,
                            });
                            pending = Some(PendingPrompt { rule: request.rule, reply: request.reply });
                        }
                    }
                }
            }
            line = statusline_output(cx.line_rx) => cx.state.statusline.set_output(line),
            event = cx.events.next() => match event {
                Some(Ok(Event::Paste(text))) => route_paste(cx.state, cx.input, &text),
                // The wheel works mid-turn for the reason PageUp does below.
                Some(Ok(Event::Mouse(mouse))) => wheel(cx, &mouse),
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if is_ctrl_c(&key) {
                        // First Ctrl-C during a turn interrupts it. The turn task
                        // winds down (the tool is killed, answered, and the loop
                        // returns Stop::Cancelled); the loop below drains the
                        // remaining narration and joins. An open prompt is
                        // denied first: the tool is parked on its answer and
                        // the cancel never reaches it otherwise.
                        deny_pending(&mut pending, cx.state);
                        cancel.cancel();
                    } else if is_shift_tab(&key) {
                        // The mode applies to the next question, never the one
                        // on screen: cycling through `auto` on the way to
                        // `plan` used to approve it as a remembered rule.
                        cycle_mode(cx);
                    } else if resolve_prompt(&mut pending, cx.state, &key) {
                        // The prompt owns the keys while it is open.
                    } else if is_escape(&key) && cx.state.detail.is_some() {
                        // Esc closes what it opened before it interrupts
                        // anything: the pager, then the completion menu or
                        // Vim's insert mode. Each used to cancel the turn.
                        let _ = handle_detail_key(cx, &key).await;
                    } else if is_escape(&key)
                        && (cx.input.completion().is_some()
                            || cx.input.is_searching()
                            || cx.input.edit_mode() == EditMode::VimInsert)
                    {
                        cx.input.handle(key);
                    } else if is_escape(&key) {
                        // Esc interrupts too, Claude Code's muscle memory: the
                        // same cancellation path Ctrl-C takes, not a second
                        // mechanism.
                        cancel.cancel();
                        cx.state.notify("interrupting…");
                    } else if is_ctrl_o(&key) {
                        // The pager works mid-turn too: a long streaming turn is
                        // exactly when vim-motion scrolling of the growing
                        // transcript earns its keep, and the turn keeps draining
                        // underneath while the pager owns the keys.
                        toggle_detail(cx);
                    } else if cx.state.detail.is_some() {
                        let _ = handle_detail_key(cx, &key).await;
                    } else if matches!(key.code, KeyCode::PageUp) {
                        // Scrolling works mid-turn on purpose: a long turn is
                        // exactly when someone wants to read back over what a
                        // tool just printed, and the tail keeps moving.
                        scroll(cx, true);
                    } else if matches!(key.code, KeyCode::PageDown) {
                        scroll(cx, false);
                    } else if key.code == KeyCode::Enter
                        && key.modifiers.is_empty()
                        && cx.input.completion().is_none()
                        && !cx.input.is_searching()
                        && !cx.input.buffer().ends_with('\\')
                        && !cx.input.buffer().trim().is_empty()
                    {
                        // Enter submits into the queue rather than being
                        // dropped: the message sends the moment this turn ends
                        // (see `drain_queued`), which is Claude Code's behavior
                        // and the answer to "typing while it works did
                        // nothing".
                        // A plain Enter only: Shift- or Alt-Enter, a trailing
                        // `\`, an open menu and a history search belong to
                        // the editor.
                        let text = cx.input.take();
                        record_history(&text);
                        cx.state.queued.push(text);
                        cx.state.notify(format!(
                            "queued ({}): sends when this turn ends, Esc hands it back",
                            cx.state.queued.len()
                        ));
                    } else {
                        // Everything else edits the prompt while the turn runs,
                        // so composing the next message mid-stream works instead
                        // of the keys vanishing.
                        match cx.input.handle(key) {
                            // Leaving mid-turn returned without the engine,
                            // and the next message panicked on its absence.
                            // The turn has to end first.
                            InputAction::Exit => {
                                cx.state.notify(
                                    "a turn is running: Esc interrupts it, then Ctrl-D exits",
                                );
                            }
                            InputAction::ShowKeybindings => {
                                cx.state.notify(keybindings_help_text());
                            }
                            InputAction::SwitchMode => cycle_mode(cx),
                            _ => {}
                        }
                    }
                }
                Some(Ok(Event::Resize(..))) => {}
                _ => {}
            },
            // The acceptance note's timeout runs mid-turn, unlike the banner's:
            // a tool that runs long after its acceptance must not keep the
            // `auto:` line pinned over the transcript for its whole duration.
            _ = expire_after(cx.state.auto_accepted_expiry()) => {
                cx.state.clear_auto_accepted();
            }
            _ = progress_tick() => {}
            () = tokio::time::sleep(frame_wait), if dirty => {}
        }
        dirty = true;
    }

    // The sender is gone: drain anything buffered after the last `recv`. A prompt
    // still pending here means the turn ended without the broker getting an
    // answer (it should not): deny it so the tool does not hang.
    while let Ok(event) = rx.try_recv() {
        reduce(cx.state, event);
    }
    deny_pending(&mut pending, cx.state);
    let (engine_back, turn_result) = task.await.map_err(|e| -> Box<dyn Error> { Box::new(e) })?;
    // Every outcome is already narrated (TurnEnded on Ok, Failed on Err); a
    // JoinError (task panic) is the only thing that surfaces here as an error.
    let _ = turn_result;
    report_violations(&engine_back, cx.state);
    *cx.engine_slot = Some(engine_back);
    // The turn may have created or deleted files the `@` menu should offer.
    file_index::invalidate();
    learn_session_title(cx.state);
    show_title(cx.state, false);
    // Come-look-now, for whoever alt-tabbed away while this ran. The bell and
    // the OSC 9 request go straight to the backend, bypassing ratatui's cell
    // buffer, since neither one is a screen cell to diff.
    if cx.notify {
        let _ = ring(cx.terminal.backend_mut(), "summer: turn finished");
    }
    Ok(cancel.is_cancelled())
}

/// Apply one engine event to the view state, and pass a usage update on to the
/// statusline.
fn fold_event(state: &mut TuiState, snap_tx: &UnboundedSender<Status>, event: UiEvent) {
    let usage = matches!(event, UiEvent::Usage { .. });
    reduce(state, event);
    if usage {
        let _ = snap_tx.send(state.status.clone());
    }
}

/// `/copy [code]`: put the last reply, or its last code block, on the system
/// clipboard through the terminal (OSC 52), which reaches the local clipboard
/// over SSH too. Straight to the backend, like the bell: it is a request to the
/// terminal, not a screen cell.
fn copy_to_clipboard(cx: &mut DriverCx<'_>, argument: Option<&str>) {
    let Some(target) = export::CopyTarget::parse(argument) else {
        cx.state.notify_error("usage: /copy [code]");
        return;
    };
    let Some(text) = export::copy_text(&cx.state.transcript, target) else {
        cx.state.notify(match target {
            export::CopyTarget::Code => "the last reply has no code block to copy",
            export::CopyTarget::Reply => "there is no reply to copy yet",
        });
        return;
    };
    let inside_tmux = std::env::var_os("TMUX").is_some();
    let sequence = export::clipboard_sequence(&text, inside_tmux);
    let backend = cx.terminal.backend_mut();
    let written =
        io::Write::write_all(backend, sequence.as_bytes()).and_then(|()| io::Write::flush(backend));
    match written {
        Ok(()) => cx.state.notify(format!(
            "copied {} characters (the terminal must allow clipboard writes)",
            text.chars().count()
        )),
        Err(error) => cx.state.notify_error(format!("could not copy: {error}")),
    }
}

/// `/export [path]`: write the transcript to `path`, relative to the session's
/// directory, or to a timestamped file there.
fn export_transcript(state: &mut TuiState, argument: Option<&str>) {
    let cwd = std::path::PathBuf::from(&state.status.cwd);
    let path = match argument {
        Some(path) => cwd.join(path),
        None => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            cwd.join(format!("summer-transcript-{stamp}.md"))
        }
    };
    match std::fs::write(&path, export::transcript_markdown(&state.transcript)) {
        Ok(()) => state.notify(format!("wrote the transcript to {}", path.display())),
        Err(error) => state.notify_error(format!("could not write {}: {error}", path.display())),
    }
}

/// Keep a submitted line in `~/.summer/history` for the next session.
fn record_history(line: &str) {
    if let Some(dir) = crate::policy::config_dir() {
        history::append(&dir, line);
    }
}

/// Scroll the transcript back (`up`) or forward by half a screen.
///
/// Half rather than a whole one so a few rows survive the jump: the line you
/// were reading is still on screen afterwards, which is what makes paging
/// through a long transcript followable. The view clamps the result against the
/// real content height on the next redraw.
fn scroll(cx: &mut DriverCx<'_>, up: bool) {
    let page = cx
        .terminal
        .size()
        .map_or(1, |size| usize::from(size.height / 2).max(1));
    cx.state.scroll_up = if up {
        cx.state.scroll_up + page
    } else {
        cx.state.scroll_up.saturating_sub(page)
    };
}

/// Throw away terminal input that arrived before a permission prompt did.
///
/// Read straight from crossterm rather than through the driver's `EventStream`:
/// the stream is borrowed by the `select!` this is called from, and what needs
/// discarding is exactly what the OS has already buffered. `poll(ZERO)` is
/// non-blocking, so an empty queue costs one syscall.
fn discard_typeahead() {
    use ratatui::crossterm::event::{poll, read};

    while poll(Duration::ZERO).unwrap_or(false) {
        if read().is_err() {
            return;
        }
    }
}

/// Waits for the next statusline line; on a dropped sender (no command
/// statusline configured) this never resolves, rather than resolving to
/// nothing on every poll and starving the select branches after it.
async fn statusline_output(rx: &mut UnboundedReceiver<String>) -> String {
    match rx.recv().await {
        Some(line) => line,
        None => std::future::pending().await,
    }
}

/// Resolve when an auto-clear timer should fire: after `remaining` if set, or
/// never when there is nothing to expire. Shared by the banner's and the
/// acceptance note's timeouts; recomputed every loop iteration from
/// [`TuiState::banner_expiry`] / [`TuiState::auto_accepted_expiry`], so a new
/// `notify`, a fresh note, or a turn starting before the deadline recomputes it
/// rather than honoring a stale timer.
async fn expire_after(remaining: Option<Duration>) {
    match remaining {
        Some(time) => tokio::time::sleep(time).await,
        None => std::future::pending().await,
    }
}

/// How often the turn-progress line refreshes during a turn. A model turn that
/// is waiting on a long tool call produces no engine events, so without this
/// the spinner and elapsed-time line would freeze mid-tool; this tick fires at
/// the spinner's frame rate (100ms) to keep it moving the way Claude Code's
/// does. Recreated each turn-loop iteration, so it only fires after a full tick
/// with no engine event -- i.e. exactly the idle gap a long tool call opens.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// Save and restore the terminal's own window title: xterm's title stack,
/// which most terminals keep. One that does not ignores both, and is left
/// with this session's title after it exits, which is what every program that
/// sets one does to it.
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";

/// How much of the session's title the window title carries after the folder:
/// enough to tell tabs apart, short of crowding the folder out of a tab strip.
const WINDOW_TITLE_SESSION_LIMIT: usize = 40;

/// How soon a second Esc must follow the first to open the rewind picker.
const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(500);

/// What a `!` command's transcript row is called.
const SHELL_ITEM_NAME: &str = "!";

/// The shortest gap between two frames during a turn: about 60 a second. See
/// the turn loop for why frames are paced at all.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// The editor Ctrl+G opens when neither `$VISUAL` nor `$EDITOR` names one:
/// the one present on every Unix system.
const DEFAULT_EDITOR: &str = "vi";

/// How long a new permission prompt ignores keys. Long enough that a key
/// already on its way from a typing hand lands harmlessly; short enough that a
/// deliberate answer never notices it.
const PROMPT_ARM_DELAY: Duration = Duration::from_millis(400);

/// Sleep one [`PROGRESS_TICK`]. Recreated each turn-loop iteration, so it fires
/// only after a full tick with no engine event, terminal event, or prompt --
/// i.e. exactly the idle gap a long tool call opens.
async fn progress_tick() {
    tokio::time::sleep(PROGRESS_TICK).await;
}

/// Put any new runtime-property violation in the transcript.
///
/// **The monitor's findings had no route to a human.** A violation means the
/// harness did something the journal says it should not have -- an access with no
/// matching grant, a path outside the root -- which is the strongest signal this
/// system produces, and its only surfacing was a `warn!` that a raw-mode TUI
/// interleaves with its own drawing or loses entirely. The count is tracked so
/// each is reported once rather than re-listed after every turn.
fn report_violations(engine: &Engine, state: &mut TuiState) {
    let violations = engine.violations();
    for violation in violations.iter().skip(state.reported_violations) {
        state.transcript.push(Item::Error(format!(
            "runtime property violated ({}): {}",
            violation.property, violation.detail
        )));
    }
    state.reported_violations = violations.len();
}

/// A `!` command in flight: the transcript row it updates, and its task.
struct ShellJob {
    call: CallId,
    handle: tokio::task::JoinHandle<ShellRun>,
}

/// Start `command` in the session's directory, with a running row for it in
/// the transcript.
fn start_shell(state: &mut TuiState, command: &str) -> ShellJob {
    let call = CallId::new(format!("user-shell-{}", state.transcript.len()));
    state.transcript.push(Item::Tool(ToolView {
        call: call.clone(),
        name: SHELL_ITEM_NAME.to_string(),
        args_summary: command.to_string(),
        status: ToolStatus::Running,
        body: None,
        collapsed: false,
    }));
    let cwd = std::path::PathBuf::from(&state.status.cwd);
    let handle = tokio::spawn(shell::run_shell(
        command.to_string(),
        cwd,
        ShellSpec::platform_default(),
    ));
    ShellJob { call, handle }
}

/// Wait for the running `!` command, or forever when there is none, so the
/// idle loop's other wakes are never starved by an empty arm.
async fn finished_shell(job: &mut Option<ShellJob>) -> Result<ShellRun, tokio::task::JoinError> {
    match job {
        Some(job) => (&mut job.handle).await,
        None => std::future::pending().await,
    }
}

/// Show a finished `!` command's output, and keep it for the next message.
fn finish_shell(
    state: &mut TuiState,
    call: &CallId,
    run: Result<ShellRun, tokio::task::JoinError>,
) {
    let Ok(run) = run else {
        interrupt_shell(state, call);
        return;
    };
    if let Some(view) = tool_view_mut(state, call) {
        view.status = run.status();
        view.body = (!run.output.trim().is_empty()).then(|| run.output.clone());
    }
    state.shell_context.push(run.context_block());
    state.notify("the output goes to the agent with your next message");
}

/// Mark a `!` command stopped before it finished. Nothing reaches the agent.
fn interrupt_shell(state: &mut TuiState, call: &CallId) {
    if let Some(view) = tool_view_mut(state, call) {
        view.status = ToolStatus::Failed;
        view.body = Some("interrupted".to_string());
    }
}

fn tool_view_mut<'a>(state: &'a mut TuiState, call: &CallId) -> Option<&'a mut ToolView> {
    state
        .transcript
        .iter_mut()
        .rev()
        .find_map(|item| match item {
            Item::Tool(view) if &view.call == call => Some(view),
            _ => None,
        })
}

/// What woke the idle loop: a statusline output line, a terminal event, or
/// one of the two auto-clear timers (the banner's, which runs idle, and the
/// acceptance note's, which also runs mid-turn -- but the idle loop still owns
/// a note left over from a turn that ended inside its timeout). Handling it
/// outside `select!` lets the branch futures (which borrow their sources) drop
/// before `run_turn` borrows them anew.
enum Wake {
    Event(Option<Result<Event, io::Error>>),
    /// A `!` command finished, or its task failed.
    Shell(Result<ShellRun, tokio::task::JoinError>),
    Line(String),
    ExpireBanner,
    ExpireAuto,
}

/// How often a command statusline refreshes. The driver renders the latest
/// output each redraw, so this bounds how often the user command runs.
const STATUSLINE_INTERVAL: Duration = Duration::from_secs(1);

/// Run a command statusline in the background: every [`STATUSLINE_INTERVAL`],
/// run the configured command with the latest [`Status`] snapshot (received on
/// `snap_rx` whenever usage changes) and stream its first stdout line back on
/// `line_tx`. The command runs via `spawn_blocking`, so a slow one never stalls
/// input.
async fn run_statusline_task(
    command: String,
    shell: ShellSpec,
    mut snap_rx: UnboundedReceiver<Status>,
    line_tx: UnboundedSender<String>,
) {
    let mut latest: Option<Status> = None;
    // The last line sent, so an unchanged one is not sent again: each line
    // received is a full redraw, and a statusline that prints the same thing
    // every second redrew the whole transcript every second for nothing.
    let mut shown: Option<String> = None;
    let mut tick = tokio::time::interval(STATUSLINE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            snap = snap_rx.recv() => match snap {
                Some(snap) => latest = Some(snap),
                None => break,
            },
            _ = tick.tick() => {
                let Some(status) = latest.clone() else { continue; };
                let command = command.clone();
                let shell = shell.clone();
                let result = tokio::task::spawn_blocking(
                    move || statusline::run_command(&command, &shell, &status),
                )
                .await;
                if let Ok(Some(line)) = result
                    && shown.as_ref() != Some(&line)
                {
                    shown = Some(line.clone());
                    let _ = line_tx.send(line);
                }
            }
        }
    }
}

/// Route `key` to an open prompt. Returns `false` only when no prompt is open.
///
/// **The prompt is modal**: while it is open every key is its, answered or
/// swallowed, and none reaches the editor. A key the prompt ignored used to
/// fall through to the draft, so typing the next message across a prompt left
/// half of it in the draft and let the other half answer. Keys inside
/// [`PROMPT_ARM_DELAY`] of the prompt appearing are swallowed too: they were
/// typed before anyone could have read the question.
fn resolve_prompt(
    pending: &mut Option<PendingPrompt>,
    state: &mut TuiState,
    key: &ratatui::crossterm::event::KeyEvent,
) -> bool {
    let Some(p) = pending.take() else {
        return false;
    };
    let armed = state
        .pending_prompt
        .as_ref()
        .is_none_or(|prompt| Instant::now() >= prompt.armed_at);
    if !armed {
        *pending = Some(p);
        return true;
    }
    let offer = state
        .pending_prompt
        .as_ref()
        .and_then(|p| p.offer.as_deref());
    let max_idx = if offer.is_some() { 2 } else { 1 };

    // Arrow keys and Ctrl-N / Ctrl-P switch between bullet items
    match key.code {
        KeyCode::Up | KeyCode::Left => {
            if let Some(prompt) = state.pending_prompt.as_mut() {
                prompt.selected = prompt.selected.saturating_sub(1);
            }
            *pending = Some(p);
            return true;
        }
        KeyCode::Down | KeyCode::Right => {
            if let Some(prompt) = state.pending_prompt.as_mut() {
                prompt.selected = (prompt.selected + 1).min(max_idx);
            }
            *pending = Some(p);
            return true;
        }
        KeyCode::Char('p' | 'P') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(prompt) = state.pending_prompt.as_mut() {
                prompt.selected = prompt.selected.saturating_sub(1);
            }
            *pending = Some(p);
            return true;
        }
        KeyCode::Char('n' | 'N') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(prompt) = state.pending_prompt.as_mut() {
                prompt.selected = (prompt.selected + 1).min(max_idx);
            }
            *pending = Some(p);
            return true;
        }
        KeyCode::Enter => {
            let selected = state.pending_prompt.as_ref().map_or(0, |p| p.selected);
            let answer = answer_for_selection(selected, offer);
            let grant = grant_for(answer, p.rule);
            state.pending_prompt = None;
            let _ = p.reply.send(grant);
            return true;
        }
        KeyCode::Esc => {
            let grant = grant_for(Answer::Deny, p.rule);
            state.pending_prompt = None;
            let _ = p.reply.send(grant);
            return true;
        }
        _ => {}
    }

    // The number printed beside each option.
    let Some(answer) = key_char(key).and_then(|ch| answer_for_key(ch, offer)) else {
        *pending = Some(p);
        return true;
    };
    let grant = grant_for(answer, p.rule);
    state.pending_prompt = None;
    let _ = p.reply.send(grant);
    true
}

/// Deny and clear any prompt still pending when a turn ends, so a tool never
/// blocks forever waiting on an answer that will not come.
fn deny_pending(pending: &mut Option<PendingPrompt>, state: &mut TuiState) {
    if let Some(p) = pending.take() {
        let _ = p.reply.send(Grant::once(false));
    }
    state.pending_prompt = None;
}

/// The character `key` answers a permission prompt with, if any.
///
/// **Modifiers disqualify it**, which they did not before. `Ctrl-Y` and `Alt-Y`
/// both carry `KeyCode::Char('y')`, so a chord the user pressed meaning
/// something else granted the permission. A bare letter is the only thing the
/// prompt offers, so a bare letter is the only thing it accepts. `SHIFT` is
/// allowed through because it is what produces `Y` in the first place.
fn key_char(key: &ratatui::crossterm::event::KeyEvent) -> Option<char> {
    let unmodified = (key.modifiers - KeyModifiers::SHIFT).is_empty();
    match key.code {
        KeyCode::Char(ch) if unmodified => Some(ch),
        _ => None,
    }
}

/// Shift+Tab / BackTab switches execution mode.
fn is_shift_tab(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
}

/// Ctrl-C, the interrupt key. In the input phase the editor maps an empty-line
/// Ctrl-C to exit; during a turn it cancels the in-flight turn.
fn is_ctrl_c(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c'))
}

/// Ctrl+O: the detail pager's global toggle, reached from the idle prompt and
/// mid-turn alike.
fn is_ctrl_g(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g'))
}

/// Ctrl+G: edit the prompt in `$VISUAL` or `$EDITOR`, then put what was saved
/// back in the box.
///
/// The TUI steps out of the way for the editor's lifetime: the alternate
/// screen and raw mode are released so the editor gets a normal terminal, and
/// both are taken back, with a full redraw, when it exits. Nothing polls the
/// event stream while this runs, so crossterm is not reading the keys meant
/// for the editor. Blocking is fine: between turns there is nothing else for
/// this thread to do.
fn compose_in_external_editor(cx: &mut DriverCx<'_>) {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| DEFAULT_EDITOR.to_string());
    let path = std::env::temp_dir().join(format!("summer-prompt-{}.md", std::process::id()));
    if let Err(error) = std::fs::write(&path, cx.input.buffer()) {
        cx.state
            .notify_error(format!("Ctrl+G: could not write the draft: {error}"));
        return;
    }
    // `code --wait` and similar name the program and its flags in one value.
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or(DEFAULT_EDITOR);
    restore_terminal();
    let status = std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .status();
    let _ = enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableWheel
    );
    claim_title(cx.state, false);
    let _ = cx.terminal.clear();
    match status {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => {
                cx.input.clear();
                cx.input.insert_str(text.trim_end_matches('\n'));
            }
            Err(error) => cx.state.notify_error(format!("Ctrl+G: {error}")),
        },
        Ok(status) => cx.state.notify(format!(
            "{program} exited with {status}; the draft is unchanged"
        )),
        Err(error) => cx
            .state
            .notify_error(format!("Ctrl+G: could not start {program}: {error}")),
    }
    let _ = std::fs::remove_file(&path);
}

fn is_ctrl_o(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('o'))
}

/// Whether `key` is the very first character of a fresh `/` or `?` command against
/// `buffer`'s state just before it -- the moment a stale notification should
/// clear rather than sit underneath whatever gets typed next. A `/` or `?` typed
/// mid-message (a path, a URL, a question) does not count: only a bare leading one does.
/// A held error banner is not cleared here -- see the call site.
fn starts_a_fresh_command(key: &ratatui::crossterm::event::KeyEvent, buffer: &str) -> bool {
    (matches!(key.code, KeyCode::Char('/')) || matches!(key.code, KeyCode::Char('?')))
        && buffer.is_empty()
}

/// Whether `key` is a bare Esc. Only consulted after the editor returned
/// `InputAction::None` -- meaning no completion menu was open for Esc to
/// close -- so this never steals the dismissal key from the menu (or from
/// the picker, key entry, and pager, which intercept earlier still).
fn is_escape(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc)
}

/// Apply a slash command to the view state. `/quit`, `/resume`, and `/login`
/// are all handled by the caller before this runs (`/quit` leaves the loop;
/// the other two need arguments this function is never given); everything
/// else mutates state and the next redraw shows the result.
fn handle_command(command: Command, model: &str, state: &mut TuiState) {
    match command {
        Command::Help => state.notify(help_text()),
        Command::Status => state.notify(status_text(state)),
        Command::Policy => state.notify(state.policy.clone()),
        Command::Sessions => state.notify(sessions_text(state)),
        Command::Resume => {}
        Command::Login => {}
        Command::Effort => {}
        Command::Model => state.notify(format!("model: {model}")),
        Command::Thinking => {
            for item in &mut state.transcript {
                if let Item::Thinking { collapsed, .. } = item {
                    *collapsed = !*collapsed;
                }
            }
        }
        Command::Tools => {
            // The flag first, so the preference survives into calls that have
            // not happened yet, then the items already on screen so the toggle
            // has a visible effect straight away.
            state.expand_tools = !state.expand_tools;
            for item in &mut state.transcript {
                if let Item::Tool(view) = item {
                    view.collapsed = !state.expand_tools;
                }
            }
        }
        Command::Diff => {}
        // Needs the engine, which `handle_command` does not receive: compaction
        // is a change to the conversation the next turn is built from, not to
        // what is on screen. `drive` intercepts it before reaching here.
        Command::Compact => {}
        Command::Keymap => {}
        // These need the driver's snapshot store, which `handle_command` does
        // not receive; `drive` intercepts them before reaching here.
        Command::Rewind => {}
        Command::Undo => {}
        Command::Checkpoint => {}
        Command::Usage => show_usage_dashboard(state),
        Command::Agent => {}
        Command::Review => {}
        Command::Statusline => {}
        Command::Copy | Command::Export => {}
        Command::Mode => {}
        Command::Clear => {
            // The transcript empties; the next redraw shows a blank content
            // area. The message banner clears with it -- `/clear` is the
            // explicit "start fresh" moment, since nothing else does anymore.
            state.transcript.clear();
            state.plan = None;
            state.clear_banner();
        }
        Command::Quit => {}
    }
}

/// `/agent <task>`: the turn text that delegates `task` to a subagent.
///
/// **A turn, not a side channel, and that is what makes it real.** The previous
/// version wrote a shell script that echoed the prompt, never ran it, and
/// reported "spawned background subagent". Delegation goes through the `subagent`
/// tool now, and the shortest honest way to reach a tool is to ask for it in a
/// turn: the transcript shows the call and its finding, the journal records both,
/// Ctrl-C cancels it, and a permission prompt still reaches the user.
///
/// `None` when the task is empty, which is a usage error rather than a turn.
fn delegation_prompt(task: &str) -> Option<String> {
    let task = task.trim();
    if task.is_empty() {
        return None;
    }
    Some(format!(
        "Delegate this to a subagent: call the `subagent` tool with it as the prompt, \
         then report what came back. Do not do the work yourself.\n\n{task}"
    ))
}

/// Prepare automated pre-commit code review prompt for workspace diff.
fn prepare_code_review_prompt(cwd_str: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(cwd_str)
        .output()
        .ok()?;
    let diff = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if diff.is_empty() {
        return None;
    }
    Some(format!(
        "Please perform a strict pre-commit code review of the following workspace diff. Analyze:\n\
         1. Correctness and potential bugs / regression risks.\n\
         2. Missing error handling, unsafe unwraps, or edge cases.\n\
         3. Code quality, performance, and best practices.\n\n\
         Workspace Diff:\n```diff\n{}\n```",
        diff
    ))
}

/// `/status`: the session card — model, provider, sandbox, cwd, session name
/// and id, journal path, and running spend.
fn status_text(state: &TuiState) -> String {
    let journal = format!("{}/{}.jsonl", state.sessions_dir, state.session_id);
    format!(
        "status:\n  model: {}\n  effort: {}\n  mode: {}\n  provider: {}\n  sandbox: {}\n  cwd: {}\n  session: {} ({})\n  session file: {}\n  tokens: {}",
        state.status.model,
        effort_for(state, &state.status.model)
            .map(Effort::as_str)
            .unwrap_or("off"),
        state.execution_mode.label(),
        state.provider,
        state.status.backend,
        state.status.cwd,
        session_name(std::path::Path::new(&journal)),
        state.session_id,
        journal,
        state.status.total_tokens,
    )
}

/// Where a bracketed paste goes: into the open masked key entry when one is
/// waiting (the paste is the key itself, copied out of a provider dashboard),
/// into the command buffer otherwise (composed text). Not routing on the
/// entry was the bug: the key landed in the clear-text buffer, which is not
/// rendered while the entry is open, so a paste looked like a no-op and the
/// key sat in the clear. Pure, so the routing is testable without an
/// [`EventStream`], which cannot be constructed headlessly.
fn route_paste(state: &mut TuiState, input: &mut InputEditor, text: &str) {
    if let Some(secret) = state.pending_secret.as_mut() {
        secret.push_str(text);
    } else {
        input.insert_str(text);
    }
}

/// Redraw the fullscreen frame. The whole transcript renders each redraw and
/// the view tail-scrolls to keep the newest content in view; the alternate
/// screen has no scrollback to commit.
///
/// Each frame goes out as one synchronized update (DEC mode 2026): the
/// terminal holds what it receives until the end marker and paints it at
/// once, instead of showing a half-written frame while a reply streams in,
/// which is the flicker fast output has. A terminal without the mode ignores
/// both markers. The end marker is sent even when the draw failed, since a
/// terminal left inside an update stops repainting.
fn redraw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut TuiState,
    input: &mut InputEditor,
) -> io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal.draw(|frame| view(state, input, frame)).map(|_| ());
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    drawn
}

/// Restore the shell after the TUI: leave the alternate screen, drop raw mode,
/// and flush. Safe to call when the terminal is already restored — every step
/// ignores its error — so the panic hook and every exit path can share it.
fn restore_terminal() {
    let _ = io::Write::write_all(&mut io::stdout(), POP_TITLE.as_bytes());
    let _ = execute!(
        io::stdout(),
        DisableWheel,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = io::Write::flush(&mut io::stdout());
}

/// Save the terminal's own title and show this session's in its place, given
/// back by [`restore_terminal`]. See [`show_title`].
fn claim_title(state: &TuiState, running: bool) {
    let _ = io::Write::write_all(&mut io::stdout(), PUSH_TITLE.as_bytes());
    show_title(state, running);
}

/// Name the window after the session's folder, marked while a turn runs, so
/// the tab that finished is findable among several without switching to each.
/// The bell says *that* a turn ended; this says *which*, and once the session
/// has a title, what it is about.
fn show_title(state: &TuiState, running: bool) {
    let title = title_for(&state.status.cwd, state.session_title.as_deref(), running);
    let _ = execute!(io::stdout(), SetTitle(title));
}

/// The window title for a session in `cwd`, named `session` if it has a title.
fn title_for(cwd: &str, session: Option<&str>, running: bool) -> String {
    let folder = std::path::Path::new(cwd).file_name().map_or_else(
        || cwd.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let marker = if running { "● " } else { "" };
    match session {
        Some(session) => format!(
            "{marker}summer · {folder} · {}",
            truncate(session, WINDOW_TITLE_SESSION_LIMIT)
        ),
        None => format!("{marker}summer · {folder}"),
    }
}

/// Install a panic hook that restores the terminal before the default hook
/// prints. Without it, a panic leaves the terminal in the alternate screen with
/// raw mode on: no echo, no line discipline, and a shell that looks dead until
/// `reset`.
///
/// **Only for a panic on the thread installing it**, which is the one the UI
/// runs on. A hook runs for every thread's panic, and most panics elsewhere are
/// survived: the dispatcher turns a tool's into an error result, and a
/// statusline command's is dropped. Restoring for those left the session
/// running with raw mode off, drawing over the shell's own screen. A panic
/// in the turn task still ends the session, through `run`'s ordinary exit path,
/// which restores the terminal itself.
fn install_panic_hook() {
    let prior = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            restore_terminal();
        }
        prior(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::timeout;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{starts_a_fresh_command, status_text, statusline_output};
    use crate::tui::testing::state_with;

    use crate::tui::TuiState;
    use crate::tui::state::PromptDisplay;
    use sandbox::Grant;

    #[test]
    fn status_text_shows_the_session_card() {
        let mut state = state_with(600, &[]);
        state.session_id = "abc123".into();
        state.sessions_dir = "/home/u/.summer/projects/-repo-x/sessions".into();
        state.provider = "anthropic".into();
        let text = status_text(&state);
        assert!(text.contains("model: mock"), "{text}");
        assert!(text.contains("provider: anthropic"), "{text}");
        assert!(
            text.contains("session: untitled session (abc123)"),
            "{text}"
        );
        assert!(
            text.contains("session file: /home/u/.summer/projects/-repo-x/sessions/abc123.jsonl"),
            "{text}"
        );
    }

    #[test]
    fn starts_a_fresh_command_only_on_a_bare_leading_slash_or_question_mark() {
        let slash = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(
            starts_a_fresh_command(&slash, ""),
            "a `/` on an empty buffer starts a command"
        );
        assert!(
            !starts_a_fresh_command(&slash, "src"),
            "a `/` continuing a path (src/main.rs) is not a fresh command"
        );
        let question = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(
            starts_a_fresh_command(&question, ""),
            "a `?` on an empty buffer starts a command"
        );
        assert!(
            !starts_a_fresh_command(&question, "what is this"),
            "a `?` ending a sentence is not a fresh command"
        );
        let other = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(
            !starts_a_fresh_command(&other, ""),
            "only `/` or `?` counts, not any key on an empty buffer"
        );
    }

    #[tokio::test]
    async fn statusline_output_never_resolves_once_the_sender_is_dropped() {
        // The bug this closes: a `biased` select listing this branch first
        // starved every branch after it (most critically the keyboard event
        // stream) whenever no command statusline was configured, because a
        // dropped sender used to make `recv` resolve to `None` on every poll
        // instead of pending.
        let (tx, mut rx) = unbounded_channel::<String>();
        tx.send("cwd: /repo".to_string()).expect("receiver open");
        assert_eq!(statusline_output(&mut rx).await, "cwd: /repo");

        drop(tx);
        let closed = timeout(Duration::from_millis(20), statusline_output(&mut rx)).await;
        assert!(closed.is_err(), "a dropped sender must never resolve");
    }

    #[test]
    fn a_delegation_needs_a_task_to_delegate() {
        // `/agent` with no task is a usage error, not an empty turn sent to the
        // model. The turn text itself is asserted in the engine's own suite,
        // where the tool it names can actually run.
        assert!(super::delegation_prompt("   ").is_none());
        let prompt = super::delegation_prompt("find every caller of `fold`").expect("a task");
        assert!(prompt.contains("`subagent` tool"), "{prompt}");
        assert!(prompt.contains("find every caller of `fold`"), "{prompt}");
    }

    #[test]
    fn prepare_code_review_prompt_returns_none_or_diff_prompt() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // Initialize a repo of our own: TMPDIR may point inside a git
        // repository (the workspace itself), whose dirty tree would otherwise
        // leak into `git diff HEAD` and fail this test on any uncommitted
        // change.
        std::fs::write(repo.join("file.txt"), "one\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(git(&["init", "--quiet"]));
        assert!(git(&["add", "file.txt"]));
        assert!(git(&["commit", "--quiet", "--message", "init"]));

        // A committed, clean tree has no workspace diff to review.
        let prompt = super::prepare_code_review_prompt(&repo.to_string_lossy());
        assert!(prompt.is_none());

        // A tracked modification becomes the review prompt's diff body.
        std::fs::write(repo.join("file.txt"), "two\n").unwrap();
        let prompt = super::prepare_code_review_prompt(&repo.to_string_lossy());
        let prompt = prompt.expect("a dirty tree should yield a review prompt");
        assert!(prompt.contains("-one"));
        assert!(prompt.contains("+two"));
    }

    #[test]
    fn resolve_prompt_bullet_navigation_and_enter() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut pending = Some(super::PendingPrompt {
            rule: None,
            reply: tx,
        });
        let mut state = state_with(0, &[]);
        state.pending_prompt = Some(PromptDisplay {
            armed_at: Instant::now(),
            description: "test".into(),
            diff: None,
            offer: Some("rule".into()),
            selected: 0,
        });

        // Down arrow moves selected from 0 to 1
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(super::resolve_prompt(&mut pending, &mut state, &down));
        assert_eq!(state.pending_prompt.as_ref().unwrap().selected, 1);
        assert!(pending.is_some());

        // Ctrl-N moves selected from 1 to 2
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(super::resolve_prompt(&mut pending, &mut state, &ctrl_n));
        assert_eq!(state.pending_prompt.as_ref().unwrap().selected, 2);
        assert!(pending.is_some());

        // Ctrl-P moves selected from 2 back to 1
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(super::resolve_prompt(&mut pending, &mut state, &ctrl_p));
        assert_eq!(state.pending_prompt.as_ref().unwrap().selected, 1);
        assert!(pending.is_some());

        // Enter submits the selected choice (1 -> AllowRule)
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(super::resolve_prompt(&mut pending, &mut state, &enter));
        assert!(pending.is_none());
        assert!(state.pending_prompt.is_none());
        let grant = rx.try_recv().unwrap();
        assert!(grant.allow);
    }

    fn open_prompt(
        armed_at: Instant,
    ) -> (
        Option<super::PendingPrompt>,
        TuiState,
        tokio::sync::oneshot::Receiver<Grant>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pending = Some(super::PendingPrompt {
            rule: None,
            reply: tx,
        });
        let mut state = state_with(0, &[]);
        state.pending_prompt = Some(PromptDisplay {
            armed_at,
            description: "run rm -rf build".into(),
            diff: None,
            offer: Some("rule".into()),
            selected: 0,
        });
        (pending, state, rx)
    }

    #[test]
    fn a_prompt_ignores_keys_until_it_could_have_been_read() {
        // Enter pressed to send the next message, landing just as the prompt
        // appeared, granted "Allow once".
        let (mut pending, mut state, mut rx) =
            open_prompt(Instant::now() + Duration::from_secs(60));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(super::resolve_prompt(&mut pending, &mut state, &enter));
        assert!(pending.is_some(), "an unarmed prompt was answered");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn an_open_prompt_swallows_prose_instead_of_answering_or_leaking_it() {
        // "and also": `a` used to grant a remembered rule, and the letters the
        // prompt did not want fell through into the draft.
        let (mut pending, mut state, mut rx) = open_prompt(Instant::now());
        for ch in "and also".chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            assert!(
                super::resolve_prompt(&mut pending, &mut state, &key),
                "{ch}"
            );
        }
        assert!(pending.is_some());
        assert!(rx.try_recv().is_err());

        let three = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE);
        assert!(super::resolve_prompt(&mut pending, &mut state, &three));
        assert!(!rx.try_recv().expect("answered").allow);
    }
}
