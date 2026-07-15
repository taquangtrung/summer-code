//! The statusline.
//!
//! Two modes:
//!
//! - **Built-in** (the default, no config): model, cwd folder, and tokens in
//!   the Claude Code pipe-joined shape with a `? help` hint, each in its
//!   semantic color.
//! - **Command** (Claude Code style): a user shell command run periodically. Its
//!   session context is delivered two ways — `SUMMER_*` env vars and a JSON
//!   object on stdin (Claude Code's shape) — and its first stdout line is
//!   rendered with ANSI colors intact. The driver runs it off the render path
//!   and caches the output, so a slow command never stalls the UI.
//!
//! Whichever mode is configured, [`Statusline::render`] appends one more line
//! under it: the session's editing mode (`auto`/`interactive`/`plan`), the
//! fact Shift+Tab and `/mode` change that nothing else in the footer reports.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use serde::Serialize;

use tools::ShellSpec;

use crate::config::StatusLineConfig;
use crate::tui::state::Status;
use crate::tui::view::format_context_tokens;

/// The built-in statusline template, shaped after Claude Code's: the model
/// (with its reasoning effort, when one is set), the cwd's folder name, and
/// context-window usage, each pipe-joined and ending in a `? help` hint.
pub const DEFAULT_STATUSLINE: &str = "{model}{effort} | {folder} | {ctx} | ? help";

/// Reported in the stdin context as `version`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a statusline command may run before it is killed.
///
/// At or below the refresh interval on purpose: a command that cannot finish
/// before the next refresh is due can never keep the line current, so waiting
/// longer buys nothing and only stacks up children.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(900);

/// How often the wait polls the child. Short enough that the timeout is roughly
/// honoured, long enough not to spin.
const WAIT_POLL: Duration = Duration::from_millis(10);

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Debug, Clone)]
pub struct Statusline {
    mode: Mode,
}

#[derive(Debug, Clone)]
enum Mode {
    BuiltIn(Vec<Segment>),
    Command(CommandLine),
}

/// A command statusline: what to run, through which shell, and what it last
/// printed.
///
/// The shell travels with the command rather than being looked up at the call
/// site, because the two are one decision: a command written for `pwsh` run
/// through `cmd` is not a degraded statusline, it is an error message once a
/// second.
#[derive(Debug, Clone)]
struct CommandLine {
    command: String,
    output: Option<String>,
    shell: ShellSpec,
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(String),
    Variable(Variable),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variable {
    Model,
    Effort,
    Mode,
    Folder,
    Tokens,
    Ctx,
    Backend,
    Cwd,
}

// ========================================================================
// Statusline Implementations
// ========================================================================

impl Statusline {
    /// A custom template statusline built from format string (e.g. `{model}  ·  {tokens}`).
    pub fn from_template(template: &str) -> Self {
        Self {
            mode: Mode::BuiltIn(parse(template)),
        }
    }

    /// A command statusline. The command runs through `shell` with the session
    /// context as `SUMMER_*` env vars and a JSON object on stdin; its first
    /// stdout line is the status line.
    pub fn from_command(command: String, shell: ShellSpec) -> Self {
        Self {
            mode: Mode::Command(CommandLine {
                command,
                output: None,
                shell,
            }),
        }
    }

    /// The statusline a loaded config asks for, defaulting to the built-in one.
    ///
    /// The trust decision is not made here: [`crate::config`] has already
    /// dropped an executable field arriving from the untrusted project layer, so
    /// a `command` that reaches this point came from the user's own file.
    pub fn from_config(configured: Option<&StatusLineConfig>, shell: ShellSpec) -> Self {
        match configured {
            Some(line) if line.is_command() => Self::from_command(line.command.clone(), shell),
            _ => Self::default(),
        }
    }

    /// The command to run and the shell to run it through, if this is a command
    /// statusline.
    pub fn command(&self) -> Option<(&str, &ShellSpec)> {
        match &self.mode {
            Mode::Command(line) => Some((&line.command, &line.shell)),
            Mode::BuiltIn(_) => None,
        }
    }

    /// Store the latest command output. A no-op for the built-in statusline.
    pub fn set_output(&mut self, output: String) {
        if let Mode::Command(line) = &mut self.mode {
            line.output = Some(output);
        }
    }

    /// Render the statusline against the live [`Status`]: the configured line
    /// (template segments, or the command's cached output), then the editing
    /// mode as a second line beneath it.
    pub fn render(&self, status: &Status) -> Text<'static> {
        let mut text = match &self.mode {
            Mode::BuiltIn(segments) => render_segments(segments, status),
            Mode::Command(line) => match &line.output {
                Some(text) => parse_ansi(text),
                None => Text::default(),
            },
        };
        text.lines.push(mode_line(status));
        text
    }
}

// ========================================================================
// Default Implementations
// ========================================================================

impl Default for Statusline {
    fn default() -> Self {
        Self {
            mode: Mode::BuiltIn(parse(DEFAULT_STATUSLINE)),
        }
    }
}

fn render_segments(segments: &[Segment], status: &Status) -> Text<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for segment in segments {
        match segment {
            Segment::Literal(text) => spans.push(Span::raw(text.clone())),
            Segment::Variable(variable) => {
                let (value, color) = variable.render(status);
                spans.push(Span::styled(value, Style::new().fg(color)));
            }
        }
    }
    Text::from(Line::from(spans))
}

/// The footer's second line: the session's editing mode (`auto`,
/// `interactive`, `plan`).
///
/// Appended by [`Statusline::render`] under the template or command line
/// whichever mode the statusline itself is in, because a command statusline
/// takes over the *first* line, not the fact of which tools run unasked —
/// losing sight of the mode because the footer was customized would be the
/// one change Shift+Tab makes that nothing else on screen reports.
fn mode_line(status: &Status) -> Line<'static> {
    Line::from(vec![
        Span::styled("mode: ", Style::new().fg(Color::DarkGray)),
        Span::styled(status.execution_mode.as_str(), Style::new().fg(Color::Cyan)),
    ])
}

// ========================================================================
// Variable Implementations
// ========================================================================

impl Variable {
    /// Map a placeholder name to a variable, or `None` for an unknown name.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "backend" => Some(Self::Backend),
            "ctx" => Some(Self::Ctx),
            "cwd" => Some(Self::Cwd),
            "effort" => Some(Self::Effort),
            "folder" => Some(Self::Folder),
            "mode" => Some(Self::Mode),
            "model" => Some(Self::Model),
            "tokens" => Some(Self::Tokens),
            _ => None,
        }
    }

    /// The variable's current value and its semantic color.
    fn render(self, status: &Status) -> (String, Color) {
        match self {
            Self::Backend => (status.backend.clone(), Color::Blue),
            Self::Ctx => render_ctx(status),
            Self::Cwd => (status.cwd.clone(), Color::DarkGray),
            Self::Effort => (
                status
                    .effort
                    .as_ref()
                    .map(|level| format!(" · {level}"))
                    .unwrap_or_default(),
                Color::Yellow,
            ),
            Self::Folder => (folder_name(&status.cwd), Color::DarkGray),
            Self::Mode => (status.execution_mode.as_str().to_string(), Color::Cyan),
            Self::Model => (status.model.clone(), Color::Magenta),
            Self::Tokens => (format_tokens(status.total_tokens), Color::Cyan),
        }
    }
}

/// Run a command statusline once: `sh -c <command>` with the session context as
/// `SUMMER_*` env vars and a JSON object on stdin, returning the first stdout
/// line with ANSI escapes left intact (for [`parse_ansi`]). A non-zero exit,
/// empty output, or any failure maps to `None` — a broken statusline command
/// never breaks the UI.
///
/// **Bounded by [`COMMAND_TIMEOUT`], because "never breaks the UI" was not
/// true.** `wait_with_output` waits forever, so a command that hangs parks its
/// `spawn_blocking` thread permanently -- and dropping a tokio `Runtime` waits
/// for in-flight blocking tasks, so `main` then hangs *after* restoring the
/// terminal. The user is left with a wedged shell and no interface left to
/// explain it. Killing the child on expiry turns a hung command back into the
/// "returns `None`" case this function already handles.
pub fn run_command(command: &str, shell: &ShellSpec, status: &Status) -> Option<String> {
    let expanded = expand_tilde(command);
    let mut child = Command::new(&shell.program)
        .args(shell.argv(&expanded))
        .envs(context_envs(status))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Feed the context JSON on stdin, then close it so the child sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let json = serde_json::to_string(&context_json(status)).ok()?;
        let _ = stdin.write_all(json.as_bytes());
    }
    if !wait_for(&mut child, COMMAND_TIMEOUT) {
        // Killed, and reaped: leaving it unwaited would accumulate a zombie per
        // refresh, which at one second apart adds up over a session.
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::to_owned)
}

/// Expand `~` to the user's home directory if it is at the start of a word/path.
fn expand_tilde(command: &str) -> String {
    let Some(home) = crate::policy::config_dir().and_then(|p| {
        p.parent()
            .map(|parent| parent.to_string_lossy().into_owned())
    }) else {
        return command.to_string();
    };

    let mut expanded = String::new();
    let mut chars = command.chars().peekable();
    let mut prev_char: Option<char> = None;

    while let Some(c) = chars.next() {
        if c == '~' {
            let is_start = prev_char.is_none() || prev_char.unwrap().is_whitespace();
            let next_char = chars.peek();
            let is_end = next_char.is_none()
                || *next_char.unwrap() == '/'
                || *next_char.unwrap() == '\\'
                || next_char.unwrap().is_whitespace();

            if is_start && is_end {
                expanded.push_str(&home);
                prev_char = home.chars().last();
                continue;
            }
        }
        expanded.push(c);
        prev_char = Some(c);
    }
    expanded
}

/// The session context as `SUMMER_*` env vars (a convenience for shell
/// one-liners; the richer JSON goes to stdin).
/// Wait for `child` up to `limit`, reporting whether it exited in time.
///
/// Polling rather than a signal-driven wait, because the alternatives are worse
/// here: `SIGCHLD` handling is process-global state a library function has no
/// business installing, and moving to `tokio::process` would put the statusline
/// on the async runtime whose blocking pool this deliberately stays off. The
/// poll costs a `try_wait` every 10ms for at most 900ms, once a second.
fn wait_for(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            // Exited. `wait_with_output` after this returns immediately.
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // Already reaped or unwaitable; let the caller's `wait_with_output`
            // report it rather than guessing here.
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(WAIT_POLL);
    }
}

fn context_envs(status: &Status) -> Vec<(&'static str, String)> {
    vec![
        ("SUMMER_MODEL", status.model.clone()),
        ("SUMMER_MODE", status.execution_mode.as_str().to_string()),
        ("SUMMER_CWD", status.cwd.clone()),
        ("SUMMER_BACKEND", status.backend.clone()),
        ("SUMMER_TOTAL_TOKENS", status.total_tokens.to_string()),
    ]
}

// ========================================================================
// Data Structures
// ========================================================================

/// The session context serialized to a statusline command's stdin, in Claude
/// Code's shape (`cwd`, `model.id`/`display_name`, `workspace.current_dir`/
/// `project_dir`, `version`) so a script written for it ports over.
#[derive(Serialize)]
struct StatusContext<'a> {
    cwd: &'a str,
    model: ModelContext<'a>,
    workspace: WorkspaceContext<'a>,
    total_tokens: u64,
    version: &'a str,
}

#[derive(Serialize)]
struct ModelContext<'a> {
    id: &'a str,
    display_name: &'a str,
}

#[derive(Serialize)]
struct WorkspaceContext<'a> {
    current_dir: &'a str,
    project_dir: &'a str,
}

fn context_json(status: &Status) -> StatusContext<'_> {
    StatusContext {
        cwd: &status.cwd,
        model: ModelContext {
            id: &status.model,
            display_name: &status.model,
        },
        workspace: WorkspaceContext {
            current_dir: &status.cwd,
            project_dir: &status.cwd,
        },
        total_tokens: status.total_tokens,
        version: VERSION,
    }
}

/// Parse a string with ANSI SGR escapes into a styled [`Text`]. Handles the
/// subset a statusline script realistically emits: reset, bold/dim/italic/
/// underline, the 16 base colors and their bright variants, 256-color
/// (`38;5;n` / `48;5;n`), and 24-bit RGB (`38;2;r;g;b` / `48;2;r;g;b`). Other
/// escapes are stripped. (Full ANSI fidelity is `ansi-to-tui`; this covers the
/// common cases without a dependency.)
fn parse_ansi(input: &str) -> Text<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::new();
    let mut text = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            flush_span(&mut spans, &mut text, style);
            // Scan to the CSI final byte (0x40..=0x7e).
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                apply_sgr(&mut style, &input[i + 2..j]);
            }
            i = j + 1;
        } else {
            let ch = input[i..].chars().next().unwrap();
            text.push(ch);
            i += ch.len_utf8();
        }
    }
    flush_span(&mut spans, &mut text, style);
    Text::from(Line::from(spans))
}

fn flush_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

/// Apply one SGR parameter list (the bytes between `ESC[` and `m`) to `style`.
fn apply_sgr(style: &mut Style, params: &str) {
    let nums: Vec<u32> = params
        .split(';')
        .map(|p| p.parse::<u32>().unwrap_or(0))
        .collect();
    let mut k = 0;
    while k < nums.len() {
        match nums[k] {
            0 => *style = Style::new(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 => *style = style.fg(base_color(nums[k] as u8 - 30)),
            38 => {
                if let Some(color) = extended_color(&nums, &mut k) {
                    *style = style.fg(color);
                }
            }
            39 => *style = style.fg(Color::Reset),
            40..=47 => *style = style.bg(base_color(nums[k] as u8 - 40)),
            48 => {
                if let Some(color) = extended_color(&nums, &mut k) {
                    *style = style.bg(color);
                }
            }
            49 => *style = style.bg(Color::Reset),
            90..=97 => *style = style.fg(bright_color(nums[k] as u8 - 90)),
            100..=107 => *style = style.bg(bright_color(nums[k] as u8 - 100)),
            _ => {}
        }
        k += 1;
    }
}

/// Resolve a `38`/`48` extended color, advancing `k` past the sub-params it
/// consumed (`5;n` → 256-color, `2;r;g;b` → RGB).
fn extended_color(nums: &[u32], k: &mut usize) -> Option<Color> {
    match nums.get(*k + 1).copied()? {
        5 => {
            let n = nums.get(*k + 2).copied().unwrap_or(0) as u8;
            *k += 2;
            Some(Color::Indexed(n))
        }
        2 => {
            let byte = |i: usize| nums.get(*k + i).copied().unwrap_or(0) as u8;
            let color = Color::Rgb(byte(2), byte(3), byte(4));
            *k += 4;
            Some(color)
        }
        _ => None,
    }
}

fn base_color(n: u8) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::White,
    }
}

fn bright_color(n: u8) -> Color {
    match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Split a template into literal and variable segments. `{name}` is a variable
/// when `name` is known; an unknown name or an unmatched brace is literal, so a
/// typo shows up verbatim rather than vanishing.
fn parse(template: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        literal.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match Variable::from_name(name) {
                    Some(variable) => {
                        push_literal(&mut segments, &mut literal);
                        segments.push(Segment::Variable(variable));
                    }
                    None => {
                        literal.push('{');
                        literal.push_str(name);
                        literal.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                literal.push('{');
                literal.push_str(after);
                rest = "";
                break;
            }
        }
    }
    literal.push_str(rest);
    push_literal(&mut segments, &mut literal);
    segments
}

fn push_literal(segments: &mut Vec<Segment>, literal: &mut String) {
    if !literal.is_empty() {
        segments.push(Segment::Literal(std::mem::take(literal)));
    }
}

/// Format a token count with k/M suffixes.
fn format_tokens(total: u64) -> String {
    if total >= 1_000_000 {
        format!("{:.1}M", total as f64 / 1_000_000.0)
    } else if total >= 1_000 {
        format!("{:.1}k", total as f64 / 1_000.0)
    } else {
        total.to_string()
    }
}

/// The `{ctx}` variable: how full the model's context window is, as
/// `23.5% ctx (200K)` -- the percentage of the window the current turn
/// occupies, then the window size. Falls back to a bare token count when the
/// model is not in the registry and its window is unknown (a live Copilot model
/// the static catalog does not pin), so it still reports *something* rather
/// than rendering `ctx` with no number behind it.
fn render_ctx(status: &Status) -> (String, Color) {
    let used = status.context_tokens;
    // Prefer the static registry (correct for every pinned model), then the
    // live Copilot window `resolve_context_window` captured at selection for a
    // model the registry does not pin. Either way `None` means truly unknown.
    let window = provider::context_window_for(&status.model).or(status.context_window);
    match window {
        Some(window) if window > 0 => {
            let pct = used as f64 / window as f64 * 100.0;
            (
                format!("{:.1}% ctx ({})", pct, format_context_tokens(window)),
                Color::Green,
            )
        }
        _ => (
            format!("{} ctx", format_context_tokens(used)),
            Color::DarkGray,
        ),
    }
}

/// The last segment of a path: the folder name a prompt shows. Backslashes are
/// normalized to forward slashes first so a Windows path resolves the same way;
/// an empty or root path falls back to itself rather than becoming an empty
/// string, matching the shell-command statusline's `folderName` convention.
fn folder_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .split('/')
        // `rfind`, not `filter(..).last()`: the split is double-ended, so this
        // takes the last non-empty segment directly instead of walking all of
        // them to reach it.
        .rfind(|segment| !segment.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_owned())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// **Haiku, because these tests are about rendering a 200K window.** The
    /// fixture used to name an Opus id and assert `200K`, which held only while
    /// the catalog mispinned every Claude model at 200K; correcting those to
    /// their real 1M windows broke four tests that were never about Opus. A
    /// genuinely-200K model keeps each assertion saying what it meant.
    fn status() -> Status {
        Status {
            model: "claude-haiku-4-5".into(),
            effort: Some("xhigh".into()),
            execution_mode: crate::mode::ExecutionMode::Interactive,
            total_tokens: 1_500,
            context_tokens: 47_000,
            context_window: None,
            backend: "landlock".into(),
            cwd: "/repo".into(),
        }
    }

    #[test]
    fn parse_splits_literals_and_variables() {
        let segments = parse("{model} · {tokens}");
        assert!(matches!(segments[0], Segment::Variable(Variable::Model)));
        assert!(matches!(segments[1], Segment::Literal(_)));
        assert!(matches!(segments[2], Segment::Variable(Variable::Tokens)));
    }

    #[test]
    fn unknown_braces_are_literal_not_dropped() {
        let segments = parse("{foo} {model}");
        assert_eq!(segments.len(), 2);
        assert!(matches!(segments[0], Segment::Literal(_)));
        assert!(matches!(segments[1], Segment::Variable(Variable::Model)));
    }

    #[test]
    fn an_unmatched_brace_is_literal() {
        let segments = parse("hi {model");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Literal(_)));
    }

    #[test]
    fn the_builtin_default_renders_every_value() {
        let rendered = Statusline::default().render(&status()).to_string();
        // status() is claude-haiku-4-5 (200k window) at xhigh effort with 47k
        // context tokens in play: 47_000 / 200_000 = 23.5%, the shape the
        // statusline exists to surface.
        assert_eq!(
            rendered,
            "claude-haiku-4-5 · xhigh | repo | 23.5% ctx (200K) | ? help\nmode: interactive"
        );
    }

    #[test]
    fn default_carries_its_variables() {
        let variables: Vec<_> = parse(DEFAULT_STATUSLINE)
            .into_iter()
            .filter_map(|segment| match segment {
                Segment::Variable(variable) => Some(variable),
                _ => None,
            })
            .collect();
        assert_eq!(variables.len(), 4);
        assert!(variables.contains(&Variable::Model));
        assert!(variables.contains(&Variable::Effort));
        assert!(variables.contains(&Variable::Folder));
        assert!(variables.contains(&Variable::Ctx));
    }

    #[test]
    fn effort_is_omitted_when_no_level_is_set() {
        // Effort is opt-in and usually off; off must render as nothing (not a
        // dangling ` · ` separator) so the default statusline stays clean for
        // the common case.
        let mut status = status();
        status.effort = None;
        let rendered = Statusline::default().render(&status).to_string();
        assert_eq!(
            rendered,
            "claude-haiku-4-5 | repo | 23.5% ctx (200K) | ? help\nmode: interactive"
        );
    }

    #[test]
    fn the_editing_mode_rides_a_second_line_for_every_mode() {
        // The footer's second line is the editing mode, under the configured
        // line whichever statusline mode is in play: it is the one fact that
        // changes what runs unasked, and a customized footer must not lose
        // sight of it. Each name is the one `/mode` and Shift+Tab accept.
        for (mode, name) in [
            (crate::mode::ExecutionMode::Auto, "auto"),
            (crate::mode::ExecutionMode::Interactive, "interactive"),
            (crate::mode::ExecutionMode::Plan, "plan"),
        ] {
            let mut status = status();
            status.execution_mode = mode;
            let text = Statusline::default().render(&status);
            assert_eq!(text.lines.len(), 2, "template line, then mode line");
            assert_eq!(text.lines[1].to_string(), format!("mode: {name}"));
            // The mode name keeps the `{mode}` variable's cyan, so the live
            // value reads the same on both lines it can appear on.
            assert_eq!(text.lines[1].spans[1].style.fg, Some(Color::Cyan));
        }
    }

    #[test]
    fn folder_variable_renders_the_last_path_segment() {
        let mut status = status();
        status.cwd = "/home/dev/work/summer-code".into();
        let line = Statusline::from_template("{folder}");
        assert_eq!(
            line.render(&status).to_string(),
            "summer-code\nmode: interactive"
        );
    }

    #[test]
    fn folder_name_handles_each_shape() {
        assert_eq!(folder_name("/home/dev/x"), "x");
        assert_eq!(folder_name("repo"), "repo");
        assert_eq!(folder_name("/"), "/");
        assert_eq!(folder_name(""), "");
        assert_eq!(
            folder_name("C:\\Users\\dev\\app"),
            "app",
            "backslashes are normalized"
        );
    }

    #[test]
    fn ctx_renders_usage_against_the_models_window() {
        // claude-haiku-4-5 has a 200k window; 47k in play is 23.5%, the exact
        // shape the statusline is meant to surface.
        let mut status = status();
        status.context_tokens = 47_000;
        let (text, color) = render_ctx(&status);
        assert_eq!(text, "23.5% ctx (200K)");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn ctx_formats_a_million_token_window_with_one_decimal() {
        // The format the request named: `1.0M`, not `1000K` or `1M`.
        let mut status = status();
        status.model = "claude-opus-5".into();
        status.context_tokens = 235_000;
        let (text, _) = render_ctx(&status);
        assert_eq!(text, "23.5% ctx (1.0M)");
    }

    #[test]
    fn ctx_falls_back_to_a_bare_count_when_the_window_is_unknown() {
        // A model neither the registry nor the live window knows still reports
        // usage, just without a percentage or window size.
        let mut status = status();
        status.model = "some-live-copilot-model".into();
        status.context_window = None;
        status.context_tokens = 47_000;
        let (text, _) = render_ctx(&status);
        assert_eq!(text, "47K ctx");
    }

    #[test]
    fn ctx_uses_the_live_window_for_a_model_the_registry_does_not_pin() {
        // A live Copilot model the static registry doesn't list still renders
        // the percentage form, using the window captured at selection
        // (`status.context_window`) -- the fix for `0K ctx` showing in place of
        // `0% ctx (size)`.
        let mut status = status();
        status.model = "claude-sonnet-4-5".into();
        status.context_window = Some(1_000_000);
        status.context_tokens = 250_000;
        let (text, _) = render_ctx(&status);
        assert_eq!(text, "25.0% ctx (1.0M)");
    }

    #[test]
    fn tokens_format() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn a_command_statusline_renders_its_cached_output() {
        let mut line = Statusline::from_command("anything".into(), ShellSpec::platform_default());
        // Before the command's first output, the mode line is all the footer
        // has — the mode is footer chrome, not the command's to print.
        assert_eq!(line.render(&status()).to_string(), "mode: interactive");
        line.set_output("on main · 2 files".into());
        assert_eq!(
            line.render(&status()).to_string(),
            "on main · 2 files\nmode: interactive"
        );
    }

    /// The platform's default shell, which is what these tests spawn.
    fn shell() -> ShellSpec {
        ShellSpec::platform_default()
    }

    /// Echo two context variables, in the default shell's own syntax.
    ///
    /// The one place these tests cannot be written once: `cmd` expands `%VAR%`
    /// and `sh` expands `$VAR`, and each treats the other's form as literal
    /// text. Everything below uses programs both platforms ship.
    #[cfg(windows)]
    const ECHO_CONTEXT: &str = "echo %SUMMER_MODEL% in %SUMMER_CWD%";
    #[cfg(not(windows))]
    const ECHO_CONTEXT: &str = "echo \"$SUMMER_MODEL in $SUMMER_CWD\"";

    /// A command that exits non-zero and prints nothing.
    #[cfg(windows)]
    const FAILS: &str = "exit 1";
    #[cfg(not(windows))]
    const FAILS: &str = "false";

    /// A command that succeeds and prints nothing.
    #[cfg(windows)]
    const SILENT: &str = "exit 0";
    #[cfg(not(windows))]
    const SILENT: &str = "true";

    #[test]
    fn run_command_passes_context_env_and_returns_stdout() {
        let out = run_command(ECHO_CONTEXT, &shell(), &status());
        assert_eq!(
            out.as_deref().map(str::trim),
            Some("claude-haiku-4-5 in /repo")
        );
    }

    #[test]
    fn run_command_receives_context_json_on_stdin() {
        // `sort` passes a single line through unchanged and ships on both
        // Windows and every Unix, so the context JSON is what it prints.
        let out = run_command("sort", &shell(), &status()).unwrap_or_default();
        assert!(
            out.contains(r#""cwd":"/repo""#),
            "stdin JSON has cwd: {out}"
        );
        assert!(out.contains(r#""model""#), "stdin JSON has model: {out}");
        assert!(
            out.contains(r#""total_tokens""#),
            "stdin JSON has tokens: {out}"
        );
    }

    #[test]
    fn run_command_returns_none_for_failure_or_empty_output() {
        assert!(
            run_command(FAILS, &shell(), &status()).is_none(),
            "non-zero exit"
        );
        assert!(
            run_command(SILENT, &shell(), &status()).is_none(),
            "empty output"
        );
    }

    #[test]
    fn parse_ansi_renders_colors_as_styled_spans() {
        let text = parse_ansi("\x1b[31mred\x1b[0m plain");
        assert_eq!(text.to_string(), "red plain", "text content is preserved");
        let spans = &text.lines[0].spans;
        assert_eq!(spans.len(), 2, "two runs split by the color change");
        assert_eq!(spans[0].content.to_string(), "red");
        assert_eq!(spans[0].style.fg, Some(Color::Red));
        assert_eq!(spans[1].content.to_string(), " plain");
        assert_eq!(spans[1].style.fg, None, "reset returns to the default");
    }

    #[test]
    fn parse_ansi_handles_256_and_truecolor() {
        let indexed = parse_ansi("\x1b[38;5;202monochrome\x1b[0m");
        assert_eq!(
            indexed.lines[0].spans[0].style.fg,
            Some(Color::Indexed(202))
        );
        let rgb = parse_ansi("\x1b[38;2;1;2;3mrgb\x1b[0m");
        assert_eq!(rgb.lines[0].spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn test_expand_tilde() {
        let home = crate::policy::config_dir()
            .and_then(|p| {
                p.parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(
                expand_tilde("node ~/.gemini/antigravity-cli/statusline.js"),
                format!("node {}/.gemini/antigravity-cli/statusline.js", home)
            );
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("echo ~"), format!("echo {}", home));
            assert_eq!(
                expand_tilde("node ~otheruser/script.js"),
                "node ~otheruser/script.js"
            );
        }
    }

    #[test]
    fn statusline_custom_template_renders_formatted_line() {
        let line = Statusline::from_template("{model} | {tokens}");
        let status = Status {
            model: "claude-3-5-sonnet".into(),
            effort: None,
            execution_mode: crate::mode::ExecutionMode::Interactive,
            total_tokens: 1_500,
            context_tokens: 0,
            context_window: None,
            backend: "anthropic".into(),
            cwd: "/app".into(),
        };
        let text = line.render(&status);
        assert_eq!(
            text.to_string(),
            "claude-3-5-sonnet | 1.5k\nmode: interactive"
        );
    }
}
