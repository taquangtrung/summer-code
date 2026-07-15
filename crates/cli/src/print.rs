//! Headless mode: one prompt in, one machine-readable answer out.
//!
//! **This is the mode that lets summer be a component rather than an
//! application.** The TUI and the plain REPL both assume a person is waiting at
//! a keyboard. A pre-commit hook, a CI job, a `Makefile` target, and any program
//! that wants to embed an agent all need the opposite: no prompts, no
//! interactivity, output on stdout, and an exit status that means something.
//!
//! Three consequences shape the module.
//!
//! **The exit status is the contract.** A caller in a shell pipeline reads
//! `$?`, not prose. `0` means the turn finished; anything else means it did not,
//! and the reasons are distinguished so a script can tell "the model refused"
//! from "the run hit its limits" from "the provider is down". See [`ExitCode`].
//!
//! **Permission prompts cannot be asked.** There is nobody to ask. The session
//! runs with whatever the policy decides on its own, and an `Ask` resolves to a
//! denial that the model sees as a tool result. A headless run that needs a
//! permission is a headless run that needs a policy rule, and the denial says
//! so.
//!
//! **Streaming output is the same event stream the TUI renders.** `stream-json`
//! is [`UiEvent`] serialized one per line, which is the TUI's input already, and
//! the shape any detached frontend would consume. It costs a serializer, not a
//! second projection of the loop.

use std::error::Error;
use std::io::{self, Read, Write};

use engine::{Engine, GoalSummary, Stop, TurnSummary, UiEvent};
use serde::Serialize;

// ========================================================================
// Constants
// ========================================================================

/// The flag that selects headless mode.
pub const PRINT_FLAG: &str = "--print";

/// Its short form.
pub const PRINT_FLAG_SHORT: &str = "-p";

/// The flag that selects the output format.
pub const FORMAT_FLAG: &str = "--output-format";

/// Keep working at the prompt until its plan is closed, rather than for one
/// turn. See [`PrintRequest::goal`].
pub const GOAL_FLAG: &str = "--goal";

/// How many turns `--goal` may take before it stops and says it did not finish.
///
/// A ceiling rather than a target. Nobody is watching an autonomous run, so the
/// failure it has to be bounded against is a model that keeps finding one more
/// thing to do; the `max_tokens` guard bounds the spend, and this bounds the
/// wall-clock.
const GOAL_MAX_TURNS: usize = 12;

// ========================================================================
// Data Structures
// ========================================================================

/// What a headless run was asked to do.
#[derive(Debug)]
pub struct PrintRequest {
    pub format: OutputFormat,
    /// Run until the plan the model opens is closed, instead of for one turn.
    ///
    /// Off by default, and that is the safe direction: an unattended loop that
    /// re-prompts itself is the one shape of run nobody is watching.
    pub goal: bool,
    /// The prompt. Empty means it is still to be read from stdin.
    pub prompt: String,
}

/// How a headless run reports itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// One JSON object at the end, carrying the result and the accounting.
    Json,
    /// One [`UiEvent`] per line as the turn runs, then a final result object.
    StreamJson,
    /// The model's final message, and nothing else.
    Text,
}

/// The process exit status of a headless run.
///
/// Distinct codes rather than a bare `1`, because the caller is a script: "the
/// model declined to do this" and "the API is unreachable" call for different
/// responses, and a single failure code makes them indistinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// The turn ran to completion.
    Ok = 0,
    /// The loop could not proceed: provider, journal sink, or context overflow.
    Failed = 1,
    /// Bad usage: an unknown format, or no prompt anywhere to read.
    Usage = 2,
    /// The model declined the request.
    Refused = 3,
    /// The run hit a configured limit before finishing.
    Limit = 4,
    /// The turn was interrupted. Not reachable from a headless run today (there
    /// is no Ctrl-C handler on this path), but the status exists so a future one
    /// does not have to overload `Failed`.
    Cancelled = 5,
}

/// The final object printed by [`OutputFormat::Json`] and at the end of
/// [`OutputFormat::StreamJson`].
///
/// Field names are the wire format for anything parsing this, so they are
/// `snake_case` and stated once here.
#[derive(Serialize)]
struct PrintResult<'a> {
    /// Always `"result"`, so a `stream-json` consumer can pick this line out of
    /// the `UiEvent`s without positional assumptions. A reader that trusts "the
    /// last line" breaks the first time a trailing event is added.
    kind: &'static str,
    is_error: bool,
    result: &'a str,
    session_id: String,
    stop: String,
    tool_calls: u32,
    total_tokens: u64,
}

// ========================================================================
// OutputFormat Implementations
// ========================================================================

impl OutputFormat {
    /// Parse the `--output-format` value.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "json" => Some(Self::Json),
            "stream-json" => Some(Self::StreamJson),
            "text" => Some(Self::Text),
            _ => None,
        }
    }

    /// Every accepted spelling, for the usage message.
    pub fn names() -> &'static str {
        "text, json, stream-json"
    }
}

// ========================================================================
// PrintRequest Implementations
// ========================================================================

impl PrintRequest {
    /// The headless request in `args`, or `None` if this is an interactive run.
    ///
    /// Returns an error for a malformed one rather than falling back to the
    /// interactive session: a script that mistyped `--output-format` wants to be
    /// told, not to have a TUI opened on a pipe.
    pub fn from_args(args: &[String]) -> Result<Option<Self>, String> {
        let Some(at) = args
            .iter()
            .position(|arg| arg == PRINT_FLAG || arg == PRINT_FLAG_SHORT)
        else {
            return Ok(None);
        };
        // The prompt is the next argument unless that is another flag, which is
        // what makes `summer -p --output-format json < prompt.txt` work.
        let prompt = args
            .get(at + 1)
            .filter(|next| !next.starts_with('-'))
            .cloned()
            .unwrap_or_default();

        let format = match value_of(args, FORMAT_FLAG) {
            None => OutputFormat::Text,
            Some(name) => OutputFormat::parse(name).ok_or_else(|| {
                format!(
                    "unknown {FORMAT_FLAG} `{name}`: expected one of {}",
                    OutputFormat::names()
                )
            })?,
        };
        let goal = args.iter().any(|arg| arg == GOAL_FLAG);
        Ok(Some(Self {
            format,
            goal,
            prompt,
        }))
    }

    /// Resolve the prompt of a request, if there is one.
    ///
    /// Separate from [`resolve_prompt`](Self::resolve_prompt) so an interactive
    /// run never reaches the stdin read: blocking on stdin when nobody asked for
    /// headless mode would hang the TUI before it drew a frame.
    pub fn resolve_prompt_if_asked(request: Option<Self>) -> Result<Option<Self>, String> {
        request.map(Self::resolve_prompt).transpose()
    }

    /// The prompt, reading stdin if the command line did not carry one.
    ///
    /// Reading stdin is what makes `git diff | summer -p 'review this'` and
    /// `summer -p < task.md` both work.
    pub fn resolve_prompt(self) -> Result<Self, String> {
        if !self.prompt.trim().is_empty() {
            return Ok(self);
        }
        let mut buffer = String::new();
        io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| format!("reading the prompt from stdin failed: {error}"))?;
        if buffer.trim().is_empty() {
            return Err(format!(
                "no prompt: pass one after {PRINT_FLAG} or pipe it on stdin"
            ));
        }
        Ok(Self {
            prompt: buffer,
            ..self
        })
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Run one turn headlessly and report it, returning the process exit status.
///
/// `events` is the receiving half of the engine's observer channel when the
/// format streams, and `None` otherwise. It is drained *after* the turn rather
/// than concurrently: the channel is unbounded, so nothing is lost by draining
/// late, and a single-threaded drain keeps the emitted order exactly the
/// engine's without a second task interleaving with this one's writes.
pub async fn run(
    engine: &mut Engine,
    request: PrintRequest,
    events: Option<&mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>>,
    out: &mut impl Write,
) -> Result<ExitCode, Box<dyn Error>> {
    // One turn, or turns until the plan closes. `run_goal` collapses to a single
    // turn when the model opens no plan, so the two paths differ only for work
    // the model itself said was multi-step.
    let outcome = if request.goal {
        engine
            .run_goal(request.prompt, GOAL_MAX_TURNS)
            .await
            .map(GoalSummary::into_turn)
    } else {
        engine.turn(request.prompt).await
    };
    let session_id = engine.session_id().to_string();

    if let Some(events) = events {
        while let Ok(event) = events.try_recv() {
            writeln!(out, "{}", serde_json::to_string(&event)?)?;
        }
    }

    let summary = match outcome {
        Ok(summary) => summary,
        Err(error) => {
            report_failure(&error.to_string(), &session_id, request.format, out)?;
            return Ok(ExitCode::Failed);
        }
    };

    let code = exit_code(&summary.stop);
    match request.format {
        OutputFormat::Text => writeln!(out, "{}", summary.text)?,
        OutputFormat::Json | OutputFormat::StreamJson => {
            let result = PrintResult {
                kind: "result",
                is_error: code != ExitCode::Ok,
                result: &summary.text,
                session_id,
                stop: describe(&summary.stop),
                tool_calls: summary.tool_calls,
                total_tokens: total_tokens(&summary),
            };
            writeln!(out, "{}", serde_json::to_string(&result)?)?;
        }
    }
    out.flush()?;
    Ok(code)
}

/// Report a turn that could not run at all, in the requested shape.
///
/// A `text` run puts this on stderr, not stdout: a caller doing
/// `answer=$(summer -p ...)` must not capture an error message as though it
/// were the model's answer.
fn report_failure(
    message: &str,
    session_id: &str,
    format: OutputFormat,
    out: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    match format {
        OutputFormat::Text => eprintln!("summer: {message}"),
        OutputFormat::Json | OutputFormat::StreamJson => {
            let result = PrintResult {
                kind: "result",
                is_error: true,
                result: message,
                session_id: session_id.to_string(),
                stop: "failed".to_string(),
                tool_calls: 0,
                total_tokens: 0,
            };
            writeln!(out, "{}", serde_json::to_string(&result)?)?;
            out.flush()?;
        }
    }
    Ok(())
}

/// The exit status a stop reason maps to.
fn exit_code(stop: &Stop) -> ExitCode {
    match stop {
        Stop::Cancelled => ExitCode::Cancelled,
        Stop::Done => ExitCode::Ok,
        Stop::HitLimit(_) => ExitCode::Limit,
        // A turn that stopped short of what it set out to do, which is what
        // `Limit` already means to a caller branching on the status. A status of
        // its own would be more precise and is not worth a wire-format change
        // until something asks to branch on it; `describe` carries the
        // difference for anything reading the JSON.
        Stop::PlanIncomplete | Stop::VerifyFailed => ExitCode::Limit,
        Stop::Refused { .. } => ExitCode::Refused,
    }
}

/// A stop reason as the string the JSON output carries.
fn describe(stop: &Stop) -> String {
    match stop {
        Stop::Cancelled => "cancelled".to_string(),
        Stop::Done => "done".to_string(),
        Stop::HitLimit(limit) => format!("limit:{limit:?}").to_lowercase(),
        Stop::PlanIncomplete => "plan_incomplete".to_string(),
        Stop::VerifyFailed => "verify_failed".to_string(),
        // The category when the model gave one: "refused" alone tells a script
        // nothing it can branch on.
        Stop::Refused { category } => match category {
            Some(category) => format!("refused:{category}"),
            None => "refused".to_string(),
        },
    }
}

/// Every token the turn's final round accounted for.
fn total_tokens(summary: &TurnSummary) -> u64 {
    let usage = &summary.usage;
    u64::from(usage.input)
        + u64::from(usage.output)
        + u64::from(usage.cache_read)
        + u64::from(usage.cache_write)
}

/// The value following `flag`, if the flag is present with one.
fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use engine::LimitKind;

    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn no_print_flag_means_an_interactive_session() {
        let parsed = PrintRequest::from_args(&args(&["summer", "--sandbox"])).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn the_prompt_follows_the_flag() {
        let parsed = PrintRequest::from_args(&args(&["summer", "-p", "fix the bug"]))
            .unwrap()
            .expect("a print request");
        assert_eq!(parsed.prompt, "fix the bug");
        assert_eq!(parsed.format, OutputFormat::Text);
    }

    #[test]
    fn a_flag_after_print_is_not_mistaken_for_the_prompt() {
        // `summer -p --output-format json < task.md` must read the prompt from
        // stdin, not run a turn whose prompt is the string "--output-format".
        let parsed = PrintRequest::from_args(&args(&["summer", "-p", "--output-format", "json"]))
            .unwrap()
            .expect("a print request");
        assert_eq!(parsed.prompt, "", "the prompt should be left for stdin");
        assert_eq!(parsed.format, OutputFormat::Json);
    }

    #[test]
    fn an_unknown_format_is_an_error_not_a_silent_fallback() {
        // Falling back to `text` would print prose into a pipe that is parsing
        // JSON, and the caller's failure would be a parse error somewhere else
        // entirely.
        let error =
            PrintRequest::from_args(&args(&["summer", "-p", "hi", "--output-format", "yaml"]))
                .expect_err("an unknown format must be rejected");
        assert!(error.contains("yaml"), "{error}");
        assert!(error.contains("stream-json"), "{error}");
    }

    #[test]
    fn every_stop_reason_maps_to_a_distinct_status() {
        // The whole point of the codes: a script must be able to tell a refusal
        // from a limit from a crash.
        assert_eq!(exit_code(&Stop::Done), ExitCode::Ok);
        assert_eq!(
            exit_code(&Stop::HitLimit(LimitKind::Turns)),
            ExitCode::Limit
        );
        assert_eq!(
            exit_code(&Stop::Refused { category: None }),
            ExitCode::Refused
        );
        assert_ne!(ExitCode::Limit as i32, ExitCode::Refused as i32);
    }

    #[test]
    fn a_limit_stop_names_which_limit() {
        assert_eq!(describe(&Stop::HitLimit(LimitKind::Tokens)), "limit:tokens");
    }
}
