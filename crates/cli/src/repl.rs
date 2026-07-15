//! The read-eval-print loop.
//!
//! One line in, one agent turn, the result out. Extracted from `main` so the
//! loop's load-bearing property can be tested without a live model or a pty:
//! **the console is read and released before `engine.turn`, never held across
//! it.** Holding it is the deadlock, because the permission prompt reads the same
//! console mid-turn.

use std::error::Error;
use std::io::Write;
use std::time::{Duration, Instant};

use engine::{Engine, Stop, TurnSummary, UiEvent};
use journal::Outcome;
use provider::{
    ACCESS_TOKEN_URL, DEVICE_CODE_URL, PollOutcome, ProviderId, poll_device_flow, start_device_flow,
};
use secrecy::ExposeSecret;

use crate::console::Console;
use crate::credentials;
use crate::notify::ring;
use crate::tui::{SPINNER_INTERVAL, progress_label, spinner_frame, summarize_args};

// ========================================================================
// Functions
// ========================================================================

/// Run the loop until end of input, reading commands from `console` and writing
/// prompts and results to `out`.
///
/// `live` gates the high-level progress line (see [`Progress`]): the caller
/// passes `true` only when `out` is a real terminal, since the line redraws
/// itself in place with `\r` and an ANSI clear, which corrupts a pipe or a
/// captured log. `notify` gates ringing the terminal bell / desktop
/// notification at the end of each turn (from `config.notify`); it has no
/// effect while `live` is `false`, for the same reason.
pub async fn run(
    engine: &mut Engine,
    console: &dyn Console,
    commands: &crate::command::Registry,
    sessions_dir: &std::path::Path,
    live: bool,
    notify: bool,
    out: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    loop {
        write!(out, "> ")?;
        out.flush()?;
        // Read and release. A line held across the `turn` below would be the
        // deadlock: the prompt inside the turn reads this same console.
        let Some(line) = console.read_line()? else {
            break;
        };
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if input == "?" || input == "/help" || input == "/?" {
            writeln!(out, "{}", crate::tui::help_text())?;
            continue;
        }
        // The plain loop has no picker to route other slash commands through,
        // but `/login` takes arguments and stores a credential rather than
        // starting a turn, so it is intercepted here the same way the TUI
        // intercepts it at the input edge.
        let mut tokens = input.split_whitespace();
        if tokens.next() == Some("/login") {
            let provider = tokens.next();
            let token = tokens.next();
            // The sandbox makes the store impossible (see
            // `credentials::login_blocked`), so the loop says so before a key
            // is typed rather than collecting one and failing to save it.
            if let Some(blocked) = credentials::login_blocked(crate::confined(), provider) {
                writeln!(out, "{blocked}")?;
                continue;
            }
            match credentials::device_flow_provider(provider, token) {
                Some(id) => run_device_flow_login(out, console, id).await?,
                None => {
                    // `/login <provider>` with no token prompts for the key
                    // instead of reporting usage: the plain loop has no input
                    // box to mask in, so the console hides the echo instead
                    // where it can.
                    let token = match token {
                        Some(token) => Some(token.to_string()),
                        None => match credentials::secret_prompt_provider(provider) {
                            Some(id) => {
                                let prompt = format!("{} API key: ", id.profile().label);
                                match console.read_secret(&prompt)? {
                                    Some(token) if !token.trim().is_empty() => {
                                        Some(token.trim().to_string())
                                    }
                                    _ => {
                                        writeln!(out, "login cancelled")?;
                                        continue;
                                    }
                                }
                            }
                            None => None,
                        },
                    };
                    writeln!(out, "{}", login(provider, token.as_deref()))?
                }
            }
            continue;
        }
        // `/resume <id>` here too, not only in the TUI: a piped or --plain
        // session can pick a session back up without a restart, same as the
        // TUI's arm. No id lists what is available (the plain loop has no
        // picker to route through).
        let mut tokens = input.split_whitespace();
        if tokens.next() == Some("/resume") {
            match tokens.next() {
                Some(id) => {
                    let path = sessions_dir.join(format!("{id}.jsonl"));
                    let opened: Result<usize, Box<dyn Error>> = std::fs::File::open(&path)
                        .map_err(|err| -> Box<dyn Error> { err.into() })
                        .and_then(|file| engine.resume(file).map_err(Into::into));
                    match opened {
                        Ok(count) => {
                            writeln!(out, "resumed {id} ({count} events)")?;
                        }
                        Err(error) => writeln!(out, "resume: {id}: {error}")?,
                    }
                }
                None => {
                    writeln!(
                        out,
                        "sessions at {} (newest last); /resume <session-id> continues one",
                        sessions_dir.display()
                    )?;
                    let mut ids: Vec<_> = std::fs::read_dir(sessions_dir)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                        .collect();
                    ids.sort();
                    for path in ids {
                        writeln!(
                            out,
                            "  {}",
                            path.file_stem().unwrap_or_default().to_string_lossy()
                        )?;
                    }
                }
            }
            continue;
        }
        // A saved-prompt command expands to the prompt it stands for. The
        // plain loop has no completion menu to discover them in, but a user who
        // knows the name should not have to switch frontends to use it.
        let prompt = commands.expand(&input).unwrap_or(input);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
        engine.set_observer(Some(tx));

        let mut progress: Option<Progress> = None;
        // Recreated per turn so a turn that starts well after the last one
        // ended does not open with a backlog of missed ticks.
        let mut ticker = tokio::time::interval(SPINNER_INTERVAL);

        let turn_result = {
            let turn_fut = engine.turn(prompt);
            tokio::pin!(turn_fut);

            loop {
                tokio::select! {
                    res = &mut turn_fut => {
                        while let Ok(event) = rx.try_recv() {
                            handle_repl_event(out, &event, live, &mut progress)?;
                        }
                        // The turn is over; leave no spinner behind for
                        // `print_summary` to print underneath.
                        clear_progress(out, &mut progress)?;
                        break res;
                    }
                    Some(event) = rx.recv() => {
                        handle_repl_event(out, &event, live, &mut progress)?;
                    }
                    _ = ticker.tick(), if live => {
                        draw_progress(out, &mut progress)?;
                    }
                }
            }
        };
        engine.set_observer(None);

        // Come-look-now, for whoever alt-tabbed away while this ran. Gated on
        // `live` too: a bell and an OSC escape are terminal control bytes, not
        // something to write into a pipe or a captured log.
        if live && notify {
            ring(out, "summer: turn finished")?;
        }

        match turn_result {
            Ok(summary) => print_summary(out, summary)?,
            Err(error) => writeln!(out, "error: {error}")?,
        }
    }
    engine.settle().await;
    Ok(())
}

/// `/login <id>` with no token, for a provider whose profile marks
/// `oauth_device_flow`: run GitHub's device-code flow to completion, blocking
/// the loop the same way a turn does. There is only one such provider today
/// (GitHub Copilot), so this calls its device-flow functions directly rather
/// than through an indirection nothing else would use yet; see the TUI's
/// `run_device_flow_login` for the same flow with live redraws and
/// Esc/Ctrl-C cancellation, which this loop has no room for between prompts.
async fn run_device_flow_login(
    out: &mut impl Write,
    console: &dyn Console,
    id: ProviderId,
) -> Result<(), Box<dyn Error>> {
    let client = reqwest::Client::new();
    let code = match start_device_flow(&client, DEVICE_CODE_URL).await {
        Ok(code) => code,
        Err(error) => return writeln!(out, "{}: {error}", id.as_str()).map_err(Into::into),
    };
    writeln!(
        out,
        "{}: go to {} and enter code {}",
        id.as_str(),
        code.verification_uri,
        code.user_code
    )?;
    write!(
        out,
        "{}: open that URL in your browser now? [Y/n] ",
        id.as_str()
    )?;
    out.flush()?;
    // Read and release before the poll loop's own `.await`s, the same rule
    // this loop follows around every `engine.turn`: nothing here holds the
    // console across an await.
    let answer = console.read_line()?.unwrap_or_default();
    if defaults_to_yes(&answer) {
        match crate::open_url(&code.verification_uri) {
            Ok(()) => writeln!(out, "{}: opened in your browser.", id.as_str())?,
            Err(error) => writeln!(
                out,
                "{}: could not open a browser automatically ({error}); open the URL above yourself.",
                id.as_str()
            )?,
        }
    }
    out.flush()?;

    writeln!(
        out,
        "{}: waiting for you to approve in the browser (checking every {}s)…",
        id.as_str(),
        code.interval.as_secs()
    )?;
    out.flush()?;
    let mut interval = code.interval;
    let outcome = loop {
        tokio::time::sleep(interval).await;
        match poll_device_flow(&client, ACCESS_TOKEN_URL, &code.device_code).await {
            Ok(PollOutcome::Pending) => continue,
            Ok(PollOutcome::SlowDown { interval: slower }) => {
                interval = slower.unwrap_or(interval) + Duration::from_secs(5);
                writeln!(
                    out,
                    "{}: GitHub asked us to slow down; checking every {}s…",
                    id.as_str(),
                    interval.as_secs()
                )?;
                out.flush()?;
                continue;
            }
            other => break other,
        }
    };

    let message = match outcome {
        Ok(PollOutcome::Authorized { github_token }) => {
            match credentials::store(id, github_token.expose_secret().to_string()) {
                Ok(()) => format!(
                    "stored a credential for {}. Restart the session for it to take effect.",
                    id.profile().label
                ),
                Err(error) => format!("could not store credential: {error}"),
            }
        }
        Ok(PollOutcome::Expired) => {
            format!(
                "{}: the device code expired; run /login {} again",
                id.as_str(),
                id.as_str()
            )
        }
        Ok(PollOutcome::Denied) => format!("{}: authorization was denied", id.as_str()),
        Ok(PollOutcome::Pending | PollOutcome::SlowDown { .. }) => {
            unreachable!("the loop above only breaks on Authorized, Expired, Denied, or Err")
        }
        Err(error) => format!("{}: {error}", id.as_str()),
    };
    writeln!(out, "{message}").map_err(Into::into)
}

/// Whether a `[Y/n]` answer means yes: blank (plain Enter) counts as the
/// capitalized default the prompt names, matching the convention that
/// prompt's brackets set.
fn defaults_to_yes(answer: &str) -> bool {
    let answer = answer.trim().to_ascii_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

/// `/login <provider> [api-key]`: store a credential locally. The plain loop has
/// no `/model` to switch to it immediately, so the message points at a
/// restart instead, unlike the TUI's.
fn login(provider: Option<&str>, token: Option<&str>) -> String {
    let (id, token) = match credentials::parse_login(provider, token) {
        Ok(pair) => pair,
        Err(message) => return message,
    };
    match credentials::store(id, token) {
        Ok(()) => format!(
            "stored a credential for {}. Restart the session for it to take effect.",
            id.profile().label
        ),
        Err(error) => format!("could not store credential: {error}"),
    }
}

// ========================================================================
// Data Structures
// ========================================================================

/// The high-level line: a spinner, the turn's elapsed time, and a real-time
/// token estimate, redrawn in place at the cursor while a turn runs. The
/// detail-level lines [`handle_repl_event`] already prints (tool start/finish)
/// scroll like a log; this one never does, matching the split
/// `tui::view` draws between the transcript and its own progress
/// line, just without a full-screen terminal to pin it to.
///
/// Tracks the same figures `tui::state::reduce` tracks for the TUI's
/// mirror of this line, so the two report the same numbers for the same turn.
struct Progress {
    started: Instant,
    uploaded_tokens: u64,
    downloaded_tokens: u64,
    streaming_chars: usize,
    is_thinking: bool,
    /// Whether the line is currently on screen, so [`clear_progress`] erases
    /// it exactly once instead of guessing.
    shown: bool,
}

// ========================================================================
// Progress Implementations
// ========================================================================

impl Progress {
    fn new(user: &str) -> Self {
        Self {
            started: Instant::now(),
            uploaded_tokens: (user.len() as u64).div_ceil(4),
            downloaded_tokens: 0,
            streaming_chars: 0,
            is_thinking: false,
            shown: false,
        }
    }

    fn reduce(&mut self, event: &UiEvent) {
        match event {
            UiEvent::TextDelta { text } => {
                self.is_thinking = false;
                self.streaming_chars += text.len();
                self.downloaded_tokens = (self.streaming_chars as u64)
                    .div_ceil(4)
                    .max(self.downloaded_tokens);
            }
            UiEvent::ThinkingDelta { text } => {
                self.is_thinking = true;
                self.streaming_chars += text.len();
                self.downloaded_tokens = (self.streaming_chars as u64)
                    .div_ceil(4)
                    .max(self.downloaded_tokens);
            }
            UiEvent::TurnReset {} => {
                self.streaming_chars = 0;
                self.downloaded_tokens = 0;
                self.is_thinking = false;
            }
            UiEvent::Usage { usage, .. } => {
                self.uploaded_tokens = u64::from(usage.input)
                    + u64::from(usage.cache_read)
                    + u64::from(usage.cache_write);
                self.downloaded_tokens = u64::from(usage.output);
            }
            _ => {}
        }
    }

    fn text(&self) -> String {
        let elapsed = self.started.elapsed();
        format!(
            "{} {}",
            spinner_frame(elapsed),
            progress_label(
                elapsed,
                self.uploaded_tokens,
                self.downloaded_tokens,
                self.is_thinking
            )
        )
    }
}

/// Erase the on-screen progress line, if one is shown. Called before any
/// detail-level line so that line starts at column zero instead of trailing
/// after spinner text.
fn clear_progress(out: &mut impl Write, progress: &mut Option<Progress>) -> std::io::Result<()> {
    if let Some(p) = progress
        && p.shown
    {
        write!(out, "\r\x1b[K")?;
        out.flush()?;
        p.shown = false;
    }
    Ok(())
}

/// (Re)draw the progress line at the cursor. A no-op while `progress` is
/// `None`, which is how this stays silent when `live` is `false`.
fn draw_progress(out: &mut impl Write, progress: &mut Option<Progress>) -> std::io::Result<()> {
    if let Some(p) = progress {
        write!(out, "\r\x1b[K{}", p.text())?;
        out.flush()?;
        p.shown = true;
    }
    Ok(())
}

fn handle_repl_event(
    out: &mut impl Write,
    event: &UiEvent,
    live: bool,
    progress: &mut Option<Progress>,
) -> std::io::Result<()> {
    if live {
        match event {
            UiEvent::TurnStarted { user } => *progress = Some(Progress::new(user)),
            _ => {
                if let Some(p) = progress {
                    p.reduce(event);
                }
            }
        }
    }
    match event {
        UiEvent::ToolStarted { name, args, .. } => {
            clear_progress(out, progress)?;
            let summary = summarize_args(name, args);
            if summary.is_empty() {
                writeln!(out, "● tool: {name}")?;
            } else {
                writeln!(out, "● tool: {name} {summary}")?;
            }
            out.flush()?;
            draw_progress(out, progress)?;
        }
        UiEvent::ToolFinished { outcome, .. } => {
            clear_progress(out, progress)?;
            match outcome {
                Outcome::Ok { .. } => writeln!(out, "✓ tool: done")?,
                Outcome::Denied { reason } => writeln!(out, "✗ tool: denied ({reason})")?,
                Outcome::Err { message } => writeln!(out, "✗ tool: failed ({message})")?,
                Outcome::TimedOut => writeln!(out, "⏱ tool: timed out")?,
            }
            out.flush()?;
            draw_progress(out, progress)?;
        }
        UiEvent::TurnReset {} => {
            clear_progress(out, progress)?;
            writeln!(out, "[retrying turn]")?;
            out.flush()?;
            draw_progress(out, progress)?;
        }
        _ => {}
    }
    Ok(())
}

fn print_summary(out: &mut impl Write, summary: TurnSummary) -> std::io::Result<()> {
    match summary.stop {
        Stop::Done => {
            if !summary.text.is_empty() {
                writeln!(out, "{}", summary.text)?;
            }
            writeln!(
                out,
                "[turn: {} tool calls, {} tokens]",
                summary.tool_calls,
                summary.usage.input + summary.usage.output,
            )
        }
        Stop::HitLimit(kind) => writeln!(out, "[stopped: hit {kind:?} limit]"),
        // An answer, not an error: print it and take the next prompt.
        Stop::Refused { category } => {
            let reason = category.unwrap_or_else(|| "unspecified".into());
            writeln!(out, "[the model declined: {reason}]")
        }
        Stop::Cancelled => writeln!(out, "[interrupted]"),
        Stop::PlanIncomplete => {
            if !summary.text.is_empty() {
                writeln!(out, "{}", summary.text)?;
            }
            writeln!(
                out,
                "[stopped: the active plan still has steps outstanding]"
            )
        }
        Stop::VerifyFailed => {
            if !summary.text.is_empty() {
                writeln!(out, "{}", summary.text)?;
            }
            writeln!(out, "[stopped: the edits still do not compile]")
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    use engine::{BrokerBuilder, EngineBuilder, Limits};
    use journal::{CallId, StopReason, Usage, WriterSink};
    use policy::Policy;
    use provider::{
        MockProvider, Provider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd, Unconfigured,
    };
    use sandbox::NoneBackend;
    use tools::{Builtin, Dispatcher, ToolSource};

    use super::*;
    use crate::permission::TerminalPrompter;

    /// A console that yields queued lines, then end of input. The REPL and the
    /// prompt share one instance, exactly as they share one stdin.
    struct Scripted {
        lines: Mutex<VecDeque<String>>,
    }

    impl Scripted {
        fn new(lines: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                lines: Mutex::new(lines.iter().map(|s| format!("{s}\n")).collect()),
            })
        }
    }

    impl Console for Scripted {
        fn read_line(&self) -> io::Result<Option<String>> {
            Ok(self.lines.lock().expect("scripted lock").pop_front())
        }
    }

    fn shell_call(command: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_1".into()),
                name: "shell".into(),
            }),
            // The marker path is a temp dir, no characters that need escaping.
            StreamEvent::ToolCallDelta(format!(r#"{{"command":"{command}"}}"#)),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::zero(),
            }),
        ]
    }

    fn done() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::zero(),
            }),
        ]
    }

    #[tokio::test]
    async fn a_shell_prompt_is_answered_from_the_console_the_repl_reads() {
        // The exact scenario the review named: the first `shell` call any user
        // makes. `sh` is not in the built-in allow list, so it prompts, and the
        // answer comes from the same console the REPL reads commands from. The
        // old loop held that console across the turn, so the prompt waited on a
        // lock the loop would not release until the turn returned: a hang.
        //
        // This test hangs rather than fails if the property regresses, so it
        // runs under a wall-clock cap that turns the hang into a failure.
        let root = std::env::temp_dir().join(format!("summer-repl-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");
        let marker = root.join("ran.marker");
        let _ = std::fs::remove_file(&marker);

        // One console, shared: the REPL reads "go", the prompt reads "y".
        let console = Scripted::new(&["go", "y"]);

        let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
            ScriptedTurn {
                events: shell_call(&format!("touch {}", marker.display())),
            },
            ScriptedTurn { events: done() },
        ]));

        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears the startup check under allow_unenforced");

        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        let deadline = Duration::from_secs(10);
        let start = Instant::now();
        tokio::time::timeout(
            deadline,
            run(
                &mut engine,
                console.as_ref(),
                &crate::command::Registry::default(),
                std::path::Path::new("/tmp/summer-test-sessions"),
                false,
                false,
                &mut out,
            ),
        )
        .await
        .expect("the REPL must not hang waiting on a prompt it starved")
        .expect("the loop runs to end of input");

        assert!(
            start.elapsed() < deadline,
            "the loop completed only by timing out"
        );
        assert!(
            marker.exists(),
            "the shell ran, so the prompt was answered `y` from the shared console"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn a_session_with_no_credential_runs_and_reports_it_per_turn() {
        // `make dev` with no ANTHROPIC_API_KEY opens the session rather than
        // exiting, and the turn the user attempts is what tells them what to
        // set. So the loop must run to end of input, and each turn must surface
        // the actionable error instead of crashing the session.
        let root = std::env::temp_dir().join(format!("summer-nocred-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");

        let console = Scripted::new(&["hi", "still here"]);
        let provider: Box<dyn Provider> = Box::new(Unconfigured::new());

        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears the startup check under allow_unenforced");

        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        run(
            &mut engine,
            console.as_ref(),
            &crate::command::Registry::default(),
            std::path::Path::new("/tmp/summer-test-sessions"),
            false,
            false,
            &mut out,
        )
        .await
        .expect("the loop runs to end of input, it does not exit on a failed turn");

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(
            text.matches("no credential").count(),
            2,
            "both turns report what to set, and the session keeps running between them: {text}"
        );
    }

    #[tokio::test]
    async fn a_login_without_a_token_prompts_for_the_key_and_stores_it() {
        // `/login zai` with no token must not dead-end on the usage error:
        // the loop asks the console for the key (hidden echo on a real TTY;
        // the next scripted line here), and the answer is stored like a
        // pasted token would be. HOME is swapped for a temp dir so the test
        // writes no real credential file.
        let root = std::env::temp_dir().join(format!("summer-login-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("home");
        // SAFETY: not applicable — this mutates the process env, which is
        // inherently racy across parallel tests; no other test in this crate
        // writes credentials, and the original HOME is restored below.
        let original_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let console = Scripted::new(&["/login zai", "glm-test"]);
        let provider: Box<dyn Provider> = Box::new(Unconfigured::new());
        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears the startup check under allow_unenforced");
        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        let result = run(
            &mut engine,
            console.as_ref(),
            &crate::command::Registry::default(),
            std::path::Path::new("/tmp/summer-test-sessions"),
            false,
            false,
            &mut out,
        )
        .await;
        // SAFETY: see above.
        unsafe {
            match &original_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
        result.expect("the loop runs to end of input");

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("stored a credential for"),
            "the prompted key is stored like a pasted one: {text}"
        );
        let stored = std::fs::read_to_string(home.join(".summer/auth.json")).expect("auth file");
        assert!(stored.contains("glm-test"), "{stored}");
    }

    #[tokio::test]
    async fn a_cancelled_login_prompt_reports_cancellation_not_usage() {
        // EOF at the key prompt (Scripted exhausted) is a cancellation, not
        // a usage error: the user answered the question they were asked.
        let root = std::env::temp_dir().join(format!("summer-loginc-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");

        let console = Scripted::new(&["/login zai"]);
        let provider: Box<dyn Provider> = Box::new(Unconfigured::new());
        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears the startup check under allow_unenforced");
        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        run(
            &mut engine,
            console.as_ref(),
            &crate::command::Registry::default(),
            std::path::Path::new("/tmp/summer-test-sessions"),
            false,
            false,
            &mut out,
        )
        .await
        .expect("the loop runs to end of input");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("login cancelled"), "{text}");
        assert!(
            !text.contains("usage:"),
            "a cancellation is not a usage error: {text}"
        );
    }

    #[test]
    fn defaults_to_yes_treats_a_blank_enter_as_the_bracketed_default() {
        // The bug this catches: a `[Y/n]` prompt promises that plain Enter
        // means yes; a blank answer falling through to "no" instead would
        // silently skip opening the browser for anyone who just hits Enter.
        for accepted in ["", "y", "Y", "yes", "YES", "  y  "] {
            assert!(defaults_to_yes(accepted), "{accepted:?} should mean yes");
        }
        for declined in ["n", "N", "no", "nope", "x"] {
            assert!(!defaults_to_yes(declined), "{declined:?} should mean no");
        }
    }

    #[tokio::test]
    async fn a_tool_call_is_displayed_live_in_repl() {
        let root = std::env::temp_dir().join(format!("summer-repl-tool-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");
        let marker = root.join("ran_live.marker");
        let _ = std::fs::remove_file(&marker);

        let console = Scripted::new(&["run tool", "y"]);
        let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
            ScriptedTurn {
                events: shell_call(&format!("touch {}", marker.display())),
            },
            ScriptedTurn { events: done() },
        ]));

        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears startup check");

        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        run(
            &mut engine,
            console.as_ref(),
            &crate::command::Registry::default(),
            std::path::Path::new("/tmp/summer-test-sessions"),
            false,
            false,
            &mut out,
        )
        .await
        .expect("the loop runs");

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("● tool: shell"),
            "tool start must be printed live to repl: {text}"
        );
        assert!(
            text.contains("✓ tool: done"),
            "tool finish must be printed live to repl: {text}"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn the_high_level_progress_line_is_drawn_live_and_cleared_before_the_summary() {
        // The gap `live: true` closes on top of the detail-level tool lines
        // above: a single status line -- spinner, elapsed time, token estimate
        // -- redraws itself in place at the cursor while the turn runs, the
        // same split the TUI draws between its transcript and its own
        // `progress_line`, just without a full-screen terminal to pin it to.
        let root =
            std::env::temp_dir().join(format!("summer-repl-progress-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");
        let marker = root.join("ran_progress.marker");
        let _ = std::fs::remove_file(&marker);

        let console = Scripted::new(&["run tool", "y"]);
        let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
            ScriptedTurn {
                events: shell_call(&format!("touch {}", marker.display())),
            },
            ScriptedTurn { events: done() },
        ]));

        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears startup check");

        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        run(
            &mut engine,
            console.as_ref(),
            &crate::command::Registry::default(),
            std::path::Path::new("/tmp/summer-test-sessions"),
            true,
            false,
            &mut out,
        )
        .await
        .expect("the loop runs");

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("\r\x1b[K"),
            "the progress line redraws in place with a carriage return and an ANSI clear: {text:?}"
        );
        assert!(
            text.contains("working…"),
            "the progress line shows the working label: {text:?}"
        );
        // The turn's final `clear_progress` is the last redraw of all: nothing
        // after it should still be sitting on the spinner's line.
        let last_clear = text.rfind("\r\x1b[K").expect("at least one redraw");
        let after = &text[last_clear + "\r\x1b[K".len()..];
        assert!(
            !after.contains("working…"),
            "the spinner must be cleared before the turn summary prints: {after:?}"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn entering_question_mark_in_repl_prints_help() {
        use std::sync::Arc;
        use std::sync::mpsc::channel;

        use engine::{BrokerBuilder, EngineBuilder, Limits};
        use journal::WriterSink;
        use policy::Policy;
        use provider::{MockProvider, Provider};
        use sandbox::NoneBackend;
        let console = Scripted::new(&["?"]);
        let root = std::env::temp_dir().join(format!("summer-repl-help-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");

        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(TerminalPrompter::new(console.clone())))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears startup check");

        let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![]));
        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(std::io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .build()
        .expect("engine builds");

        let mut out = Vec::new();
        run(
            &mut engine,
            console.as_ref(),
            &crate::command::Registry::default(),
            std::path::Path::new("/tmp/summer-test-sessions"),
            false,
            false,
            &mut out,
        )
        .await
        .expect("the loop runs");

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("commands:"),
            "shows help commands list: {text}"
        );
        assert!(text.contains("/help"), "contains /help description: {text}");
        assert!(
            text.contains("/status"),
            "contains /status description: {text}"
        );
    }
}
