//! The view model: a pure reducer over the [`UiEvent`] stream.
//!
//! The terminal is untestable; the logic in front of it must not be. This module
//! is that logic. [`reduce`] folds one [`UiEvent`] into a [`TuiState`], and
//! everything the UI shows is that fold — exactly as session state is a fold of
//! the journal. The driver never interprets an event; it hands it here.

use std::collections::HashMap;

use std::time::{Duration, Instant};

use engine::UiEvent;
use journal::{CallId, Outcome, Usage};
use provider::{CopilotModel, Effort, ProviderId, ProviderProfile};

use crate::mode::ExecutionMode;
use crate::tui::shell::strip_shell_context;
use crate::tui::statusline::Statusline;
use crate::tui::view::EDIT_TOOL;

// ========================================================================
// Constants
// ========================================================================

/// How long a notified message stays above the prompt before auto-clearing,
/// counted from the last [`TuiState::notify`] and paused while a turn or login
/// flow runs. Long enough to read a typical result, short enough that a stale
/// notice does not sit over the prompt forever.
const BANNER_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the auto-mode acceptance note stays above the prompt, counted from
/// the newest acceptance. Deliberately much shorter than the banner's timeout,
/// and *not* paused while a turn runs: acceptances arrive continuously through
/// an auto-mode turn, so a longer stay would only trade the banner's stack of
/// `Auto-accepted:` lines for a permanent one. The note's whole job is to
/// explain the permission prompt that did not appear; the transcript already
/// shows the call itself.
const AUTO_ACCEPT_TIMEOUT: Duration = Duration::from_secs(2);

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 3600;
const SECS_PER_DAY: u64 = 86400;
const SECS_PER_WEEK: u64 = 86400 * 7;
const SECS_PER_MONTH: u64 = 86400 * 30;
const SECS_PER_YEAR: u64 = 86400 * 365;

/// The tool whose arguments are the agent's plan.
const PLAN_TOOL: &str = "plan";

// ========================================================================
// Data Structures
// ========================================================================

/// The session's view state. Everything the UI shows is derived from this, and
/// this is a fold of the [`UiEvent`] stream.
pub struct TuiState {
    /// User turns, assistant text, thinking, tool views, and errors, in order.
    /// The driver renders the whole transcript each redraw; the view tail-scrolls
    /// so the newest content stays in view.
    pub transcript: Vec<Item>,
    /// Messages submitted while a turn ran, oldest first, sent one per turn as
    /// each finishes. Shown above the input so a queued message is visibly
    /// waiting rather than looking dropped.
    pub queued: Vec<String>,
    /// The agent's latest plan, from its last `plan` call, drawn as a checklist
    /// under the progress line while a turn runs. `None` until it writes one.
    pub plan: Option<PlanDisplay>,
    /// What `!` commands printed since the last message, waiting to go to the
    /// agent with the next one.
    pub shell_context: Vec<String>,
    /// The prompt size at which the engine compacts history, for the warning
    /// that it is getting close. Read from the engine at startup and after
    /// every model switch, since the budget follows the model.
    pub context_budget: Option<u64>,
    /// When each turn began, oldest first. `/rewind n` restores the workspace to
    /// the nth-from-last entry; nothing else reads it.
    pub turn_starts: Vec<std::time::SystemTime>,
    /// A turn is in flight. Drives the spinner and whether the prompt is active.
    pub streaming: bool,
    /// When the in-flight turn started, to render its elapsed-time progress
    /// line at the tail of the conversation area. `Some` only while streaming;
    /// cleared when the turn ends or fails.
    pub turn_started_at: Option<Instant>,
    /// Estimated or reported tokens uploaded (input + cache) for the current turn.
    pub turn_uploaded_tokens: u64,
    /// Estimated or reported tokens downloaded/generated for the current turn.
    pub turn_downloaded_tokens: u64,
    /// Characters streamed for the current turn so far, for real-time token estimation.
    pub streaming_output_chars: usize,
    /// The current execution mode (Auto, Interactive, Plan).
    pub execution_mode: ExecutionMode,
    /// The status line: model, spend, backend, cwd.
    pub status: Status,
    /// Per-turn usage, in arrival order. `/usage` summarizes it.
    pub usage: Vec<Usage>,
    /// What delegated sub-sessions spent, in arrival order.
    ///
    /// Kept apart from [`usage`](Self::usage) rather than appended to it: that
    /// vector's length is the session's turn count, and folding a subagent's
    /// bill into it would report delegations as turns this session took.
    pub delegated: Vec<Usage>,
    /// Session metadata captured once at startup, for `/status` and `/policy`.
    pub session_id: String,
    /// The title the engine generated for this session, once it has (see
    /// `session::learn_session_title`).
    pub session_title: Option<String>,
    /// Where this workspace's session journals live (the bootstrap-chosen
    /// store), so `/sessions`, `/resume`, and `/status` never reconstruct —
    /// and so never disagree with — the path the journal was opened at.
    pub sessions_dir: String,
    pub provider: String,
    /// The current provider's id, alongside the label above. `/effort`'s save
    /// path needs the id (not the human label) to persist, and this is the one
    /// place it lives for the running session.
    pub provider_id: ProviderId,
    pub policy: String,
    /// The statusline template, rendered against `status` each redraw. Built
    /// once from the `[tui] statusline` config at startup.
    pub statusline: Statusline,
    /// A permission prompt the broker is waiting on, rendered inline until
    /// answered. `None` outside a prompt. Set by the driver (not `reduce`, since
    /// a prompt is not a `UiEvent`); the view renders it in place of the prompt.
    pub pending_prompt: Option<PromptDisplay>,
    /// A `/model` selection in progress, rendered inline until answered or
    /// cancelled. `None` outside a picker. Set by the driver, the same way
    /// `pending_prompt` is: picking a provider or model is not a `UiEvent`
    /// either.
    pub pending_picker: Option<PickerDisplay>,
    /// System messages (command output, login-flow updates, errors), shown in
    /// a banner above the prompt. Appended to by [`TuiState::notify`], never
    /// overwritten -- two messages shown back to back (the device flow's
    /// code, immediately followed by its browser prompt) must both stay
    /// readable, not have the second erase the first before either renders.
    /// UI chrome, not conversation content, so it never lands in
    /// `transcript`. Each notice auto-clears [`BANNER_TIMEOUT`] after it was
    /// posted, independently of the others, once any ongoing turn or login
    /// finishes -- unless it is sticky ([`TuiState::notify_error`]), which no
    /// timeout clears: only Esc or `/clear` does, because an error the user
    /// has not finished reading is the one message that must not vanish
    /// underneath them. A sticky notice sitting beside plain ones no longer
    /// holds the plain ones past their own time; see [`TuiState::expire_banner`].
    pub pending_messages: Vec<Notice>,
    /// The most recent command auto mode answered for, shown as one transient
    /// line above the prompt ([`Self::auto_accepted_line`]). The newest
    /// acceptance replaces the older one rather than stacking under it, and
    /// the note leaves [`AUTO_ACCEPT_TIMEOUT`] after it -- unlike the banner,
    /// which accumulates and holds through a turn. Set by the driver
    /// (`run_turn`'s auto arm), the same way `pending_prompt` is.
    pub auto_accepted: Option<AutoAccepted>,
    /// A device-flow login is waiting on the user or on GitHub, with this
    /// status text. `None` outside a login. Its own key-reading loop owns
    /// every keystroke while this is set (the same way a turn's does), so the
    /// view must not render the normal editable prompt underneath it -- that
    /// would look typeable while every key actually went to the login
    /// instead.
    pub pending_login: Option<String>,
    /// An API-key entry (`/login <provider>` with no token) in progress,
    /// rendered as a masked box in place of the prompt. `None` outside
    /// entry. Set by the driver the same way `pending_picker` is: typing a
    /// key is not a `UiEvent`.
    pub pending_secret: Option<SecretInput>,
    /// How many monitor violations have already been put in the transcript, so
    /// each is reported once rather than re-listed after every turn.
    pub reported_violations: usize,
    /// How many rows the transcript is scrolled back from its newest line.
    ///
    /// `0` is follow-the-tail, which is what a streaming turn wants. Anything
    /// higher pins the view, so the user can read what scrolled past without the
    /// next delta yanking them back down (see [`TuiState::scroll_anchor`]).
    /// Clamped against the real content height each redraw, in the view,
    /// because that is what knows the wrapped line count and the viewport.
    pub scroll_up: usize,
    /// While scrolled back: the width the transcript was last wrapped at and
    /// the rows it came to. The view grows `scroll_up` by whatever arrived
    /// since, which is what keeps a pinned view on the line being read;
    /// counted from the bottom alone, every streamed row pushed it up one.
    /// `None` at the tail, where nothing needs counting.
    pub scroll_anchor: Option<(u16, usize)>,
    /// Whether new tool calls render with their body expanded, toggled by
    /// `/tools`.
    ///
    /// The flag lives on the state, not only on each [`ToolView`], so the
    /// preference applies to calls that have not happened yet. A toggle that
    /// only flipped the items already on screen would silently revert on the
    /// next tool call, which is the shape `/thinking` has and is the reason it
    /// reads as not working.
    pub expand_tools: bool,
    /// The Ctrl+O detail pager, for vim-motion navigation of the whole
    /// transcript. `None` in the normal chat layout; `Some` overlays a
    /// full-screen pager that owns every key (j/k, Ctrl-D/U, Ctrl-F/B, gg/G,
    /// ...) until toggled off. Independent of `scroll_up` so browsing it does
    /// not fight a streaming turn's follow-the-tail.
    pub detail: Option<Detail>,
    /// Per-model reasoning effort (`/effort`), remembered for the session.
    /// Absent means off (the default): some models 400 on `output_config.effort`,
    /// so effort is opt-in. A `/effort` call applies the new level live; switching
    /// models restores each one's setting. Runtime only, like the model choice
    /// itself -- the config file is read-only, so it does not persist across
    /// restarts.
    pub efforts: HashMap<String, Effort>,
    /// Models fetched live from Copilot's `/models`, cached for the session so
    /// reopening `/model` does not re-query the network unless re-authenticated.
    pub cached_copilot_models: Option<Vec<CopilotModel>>,
}

/// The detail pager's state: a scroll offset into the wrapped transcript and a
/// half-typed `gg` prefix. The offset is in wrapped rows from the top, clamped
/// against the content height each redraw -- the view is the one place that
/// knows the wrapped line count and the viewport, the same reason
/// [`TuiState::scroll_up`] is clamped there too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Detail {
    /// Wrapped rows from the top of the transcript.
    pub offset: usize,
    /// The first `g` of a `gg` has been typed and is waiting for the second.
    pub pending_g: bool,
}

/// One banner line: its text, whether it is the kind of message that must
/// not vanish on a timer, and when it was posted. [`TuiState::notify`] posts
/// a plain one; only [`TuiState::notify_error`] posts a sticky one. Each
/// notice's clock is its own -- a sticky notice sitting beside a plain one
/// does not hold the plain one past its own [`BANNER_TIMEOUT`]; see
/// [`TuiState::expire_banner`].
#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub sticky: bool,
    pub at: Instant,
}

/// The most recent command auto mode answered for, and when: what the
/// transient acceptance line above the prompt shows, and the clock
/// [`AUTO_ACCEPT_TIMEOUT`] counts down from. A driver-set note rather than a
/// `UiEvent` fold, the same way `pending_prompt` is: answering a broker
/// prompt is not something the engine narrates.
#[derive(Debug, Clone)]
pub struct AutoAccepted {
    /// What the broker asked to do, verbatim (the permission prompt's
    /// description).
    pub description: String,
    /// When the acceptance happened.
    pub at: Instant,
}

/// A masked API-key entry (`/login <provider>` with no token): which provider
/// the key is for, and the buffer being typed. The view renders one `*` per
/// character ([`SecretInput::masked`]) in place of the normal prompt, and the
/// driver routes every key to it while it is open -- the same interception
/// pattern a picker or permission prompt uses -- so the key never sits in the
/// clear-text command buffer (or its recallable history). Append-and-backspace
/// only, like every password field: no cursor motion, so the cursor is always
/// at the end and needs no separate offset.
#[derive(Debug)]
pub struct SecretInput {
    /// The provider whose key is being typed: the entry box's label, and the
    /// id the key is stored under on confirm.
    pub id: ProviderId,
    buffer: String,
}

impl SecretInput {
    pub fn new(id: ProviderId) -> Self {
        Self {
            id,
            buffer: String::new(),
        }
    }

    /// The typed key. Only the store path reads it; the view renders
    /// [`Self::masked`] instead.
    pub fn secret(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Append one typed character.
    pub fn push(&mut self, ch: char) {
        self.buffer.push(ch);
    }

    /// Delete the last character; whether anything was deleted.
    pub fn pop(&mut self) -> bool {
        self.buffer.pop().is_some()
    }

    /// Append pasted text. The surrounding whitespace a clipboard often
    /// carries (a copied key picked up a trailing newline) is trimmed rather
    /// than stored: no provider's key legitimately starts or ends in
    /// whitespace, and a pasted `\n` would otherwise join the buffer a Backspace
    /// has to find. Interior characters are kept as-is -- faithfulness to the
    /// clipboard beats guessing at an internal typo.
    pub fn push_str(&mut self, text: &str) {
        for ch in text.trim().chars() {
            self.buffer.push(ch);
        }
    }

    /// The display form: one `*` per typed character, so the length is
    /// visible while the key itself never is.
    pub fn masked(&self) -> String {
        "*".repeat(self.buffer.chars().count())
    }
}

/// One row of the transcript.
#[derive(Debug)]
pub enum Item {
    /// A user prompt, as submitted.
    User(String),
    /// Assistant text, growing on `TextDelta`.
    Assistant(String),
    /// A thinking block. Collapsed by default: it is provenance, not the answer.
    Thinking { text: String, collapsed: bool },
    /// One tool call's view, from `ToolStarted` to `ToolFinished`.
    Tool(ToolView),
    /// A turn-level failure (a `Failed` event), rendered inline.
    Error(String),
}

/// The view of one tool call across its lifecycle.
#[derive(Debug)]
pub struct ToolView {
    pub call: CallId,
    pub name: String,
    /// A short, human-readable summary of the arguments (the path, the command).
    pub args_summary: String,
    pub status: ToolStatus,
    /// The outcome's content or message, once the call finished.
    pub body: Option<String>,
    pub collapsed: bool,
}

/// Where a tool call is in its lifecycle. Mirrors [`Outcome`] with a `Running`
/// state for the in-flight phase.
///
/// `Pending` is part of the lifecycle the T2 prompt introduces (a tool the
/// broker is about to authorize, before it runs); the engine narrates a tool as
/// `Running` from its first event, so `Pending` is not constructed until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    #[allow(dead_code)]
    Pending,
    Running,
    Done,
    Denied,
    Failed,
    TimedOut,
}

/// A plan as the checklist shows it: its steps, which one is active, and
/// whether the whole thing finished or stalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDisplay {
    /// The 1-based active step, as the `plan` tool takes it; `None` when the
    /// model named none.
    pub active: Option<usize>,
    pub progress: PlanProgress,
    pub steps: Vec<String>,
}

/// Where a plan stands, from the `plan` tool's `status` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanProgress {
    Blocked,
    Completed,
    /// The schema's default, and what an unrecognized status reads as.
    InProgress,
}

/// The status line contents. Plain strings and numbers: the view model owns no
/// engine types, so it stays testable and serialization-free.
#[derive(Debug, Clone)]
pub struct Status {
    pub model: String,
    /// The current model's reasoning effort, rendered form (e.g. `"xhigh"`).
    /// `None` is off (the default). A cached mirror of `efforts[model]` so the
    /// statusline can show it without reaching into the per-model map.
    pub effort: Option<String>,
    /// The session's execution mode (Auto, Interactive, Plan).
    pub execution_mode: ExecutionMode,
    /// Cumulative billed tokens this session (`input + output + cache_write`),
    /// the runaway budget. Distinct from `context_tokens`.
    pub total_tokens: u64,
    /// Tokens occupying the context window right now: the last round's
    /// `input + cache_read + cache_write + output`. `total_tokens` excludes
    /// `cache_read` (the runaway guard does not bill cache hits), so this is a
    /// separate number -- the window counts cached tokens because they still
    /// occupy it.
    pub context_tokens: u64,
    /// The current model's context-window capacity, resolved at model selection
    /// from Copilot's live `/models` when available (the static registry
    /// otherwise). `None` only for a model neither source knows. Drives the
    /// `{ctx}` percentage; the statusline falls back to the registry too.
    pub context_window: Option<u64>,
    pub backend: String,
    pub cwd: String,
}

/// A permission prompt awaiting an answer, for the view to render inline.
#[derive(Debug, Clone)]
pub struct PromptDisplay {
    /// When the prompt starts taking answers. Keys before this are swallowed:
    /// they were typed toward the message being composed, not at a question
    /// that had not been drawn yet.
    pub armed_at: Instant,
    pub description: String,
    /// What the write would change, drawn under the question. See
    /// [`PromptRequest::diff`](crate::tui::prompt::PromptRequest::diff).
    pub diff: Option<String>,
    pub offer: Option<String>,
    pub selected: usize,
}

/// An open `/model` picker: which stage it is showing, and the highlighted
/// row. Provider selection drills into that provider's models; a model
/// selection confirms it.
#[derive(Debug)]
pub struct PickerDisplay {
    pub selected: usize,
    pub stage: PickerStage,
}

/// Which level of the picker is showing.
#[derive(Debug)]
pub enum PickerStage {
    Models {
        profile: &'static ProviderProfile,
        /// Models fetched live from Copilot's `/models`; `None` falls back to
        /// the profile's static catalog. Only GitHub Copilot ever sets this:
        /// its real catalog is account-specific, so it is the one provider
        /// that has to be discovered online rather than listed in the
        /// registry.
        dynamic_models: Option<Vec<CopilotModel>>,
    },
    /// Choosing a reasoning effort for a just-selected model: only the levels
    /// that model supports are listed. Confirming one applies it; a model with
    /// no supported effort skips this stage entirely (none is sent, so it cannot
    /// 400 on `output_config.effort`).
    Effort {
        profile: &'static ProviderProfile,
        model: String,
        levels: Vec<String>,
    },
    Providers {
        profiles: Vec<&'static ProviderProfile>,
    },
    /// `/resume` with no id: this project's past sessions, newest first, each
    /// row previewed by its generated title or, absent one, its first user
    /// message. Confirming one resumes it.
    Sessions { rows: Vec<SessionRow> },
    /// Esc Esc: the messages that opened this session's turns, newest first.
    /// Confirming one rewinds the conversation to just before it and puts it
    /// back in the editor.
    Rewind { rows: Vec<String> },
}

/// One `/resume` picker row: the session id (what `/resume <id>` takes),
/// a relative age (e.g. "5m ago"), and a one-line preview of what the session was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub age: String,
    pub id: String,
    pub preview: String,
}

// ========================================================================
// PickerDisplay Implementations
// ========================================================================

impl PickerDisplay {
    /// Indices of rows whose text matches `query` (case-insensitive substring
    /// on what the picker shows: a provider's label, a static model's name, or
    /// a Copilot model's display name and id). An empty query matches every
    /// row, so the unfiltered list is the starting state. Drives the visible
    /// row count, the selection's clamp, and confirmation's index lookup.
    pub fn matching(&self, query: &str) -> Vec<usize> {
        let needle = query.trim().to_lowercase();
        let matches =
            |haystack: &str| needle.is_empty() || haystack.to_lowercase().contains(&needle);
        match &self.stage {
            PickerStage::Providers { profiles } => (0..profiles.len())
                .filter(|&i| matches(profiles[i].label))
                .collect(),
            PickerStage::Models {
                profile,
                dynamic_models,
            } => match dynamic_models {
                Some(models) => (0..models.len())
                    .filter(|&i| matches(&format!("{} {}", models[i].display_name(), models[i].id)))
                    .collect(),
                None => (0..profile.models.len())
                    .filter(|&i| matches(profile.models[i].name))
                    .collect(),
            },
            PickerStage::Effort { levels, .. } => {
                (0..levels.len()).filter(|&i| matches(&levels[i])).collect()
            }
            PickerStage::Rewind { rows } => {
                (0..rows.len()).filter(|&i| matches(&rows[i])).collect()
            }
            PickerStage::Sessions { rows } => (0..rows.len())
                .filter(|&i| {
                    matches(&format!(
                        "{} {} {}",
                        rows[i].id, rows[i].age, rows[i].preview
                    ))
                })
                .collect(),
        }
    }
}

// ========================================================================
// PlanDisplay Implementations
// ========================================================================

impl PlanDisplay {
    /// The plan a `plan` call's arguments describe, or `None` when they name
    /// no steps (a malformed call the tool itself will refuse).
    pub fn from_args(args: &serde_json::Value) -> Option<Self> {
        let steps: Vec<String> = args
            .get("steps")?
            .as_array()?
            .iter()
            .filter_map(|step| step.as_str().map(str::to_string))
            .collect();
        if steps.is_empty() {
            return None;
        }
        let active = args
            .get("active_step")
            .and_then(serde_json::Value::as_u64)
            .and_then(|step| usize::try_from(step).ok());
        let progress = match args.get("status").and_then(serde_json::Value::as_str) {
            Some("completed") => PlanProgress::Completed,
            Some("blocked") => PlanProgress::Blocked,
            _ => PlanProgress::InProgress,
        };
        Some(Self {
            active,
            progress,
            steps,
        })
    }
}

// ========================================================================
// Functions
// ========================================================================

impl TuiState {
    pub fn new(status: Status) -> Self {
        let execution_mode = status.execution_mode;
        Self {
            turn_starts: Vec::new(),
            transcript: Vec::new(),
            queued: Vec::new(),
            plan: None,
            shell_context: Vec::new(),
            context_budget: None,
            streaming: false,
            turn_started_at: None,
            turn_uploaded_tokens: 0,
            turn_downloaded_tokens: 0,
            streaming_output_chars: 0,
            execution_mode,
            status,
            usage: Vec::new(),
            delegated: Vec::new(),
            session_id: String::new(),
            session_title: None,
            sessions_dir: String::new(),
            provider: String::new(),
            provider_id: ProviderId::Anthropic,
            policy: String::new(),
            statusline: Statusline::default(),
            pending_prompt: None,
            pending_picker: None,
            pending_messages: Vec::new(),
            auto_accepted: None,
            pending_login: None,
            pending_secret: None,
            reported_violations: 0,
            scroll_up: 0,
            scroll_anchor: None,
            // Collapsed by default: a full transcript of tool output drowns the
            // conversation. `/tools` is what makes it reachable at all.
            expand_tools: false,
            detail: None,
            efforts: HashMap::new(),
            cached_copilot_models: None,
        }
    }

    /// Show `text` as a system message in the banner above the prompt. The
    /// single entry point every command result, login update, and inline
    /// error goes through -- this is UI chrome, not part of the
    /// conversation, so it never joins `transcript`.
    ///
    /// Also restarts every other plain notice's own countdown, so a fresh
    /// notice keeps an older plain one company for the full
    /// [`BANNER_TIMEOUT`] rather than letting the older one's timer expire
    /// mid-read. A sticky notice is untouched by this either way: it has no
    /// countdown to restart.
    pub fn notify(&mut self, text: impl Into<String>) {
        self.post(text.into(), false);
    }

    /// Show an error in the banner and **hold it**: unlike a plain notice, it
    /// does not auto-clear on any timeout -- only Esc in the driver, or
    /// `/clear`, dismisses it. A five-second timeout is right for a command
    /// result and wrong for "could not store credential", which is read
    /// twice, acted on, and only then allowed to leave. Marked sticky so the
    /// view can render it apart from a plain [`Self::notify`] line, which
    /// otherwise looked identical down to the color, and so
    /// [`Self::expire_banner`] knows to leave it in place.
    pub fn notify_error(&mut self, text: impl Into<String>) {
        self.post(text.into(), true);
    }

    /// Add a notice, replacing an identical one already showing rather than
    /// stacking a copy: four `!` commands in a row left four copies of the
    /// same advice above the prompt, which read as four different messages.
    fn post(&mut self, text: String, sticky: bool) {
        let at = Instant::now();
        let text = capitalize_first(&text);
        self.pending_messages
            .retain(|notice| notice.text != text || notice.sticky != sticky);
        self.touch_plain_notices(at);
        self.pending_messages.push(Notice { text, sticky, at });
    }

    /// Restart every plain notice's countdown from `at`, e.g. because a fresh
    /// notice just arrived to keep them company. A sticky notice has no
    /// countdown, so it is left alone.
    fn touch_plain_notices(&mut self, at: Instant) {
        for notice in &mut self.pending_messages {
            if !notice.sticky {
                notice.at = at;
            }
        }
    }

    /// Whether the banner is currently held by an error: at least one notice
    /// posted through [`Self::notify_error`] is still showing. The driver
    /// routes an otherwise-unclaimed Esc to [`Self::clear_banner`] while this
    /// is true.
    pub fn banner_sticky(&self) -> bool {
        self.pending_messages.iter().any(|notice| notice.sticky)
    }

    /// How long until the soonest plain notice auto-clears, for the driver's
    /// expire wake ([`Self::expire_banner`]); `None` when there is nothing to
    /// time: every remaining notice is sticky, the banner is empty, or an
    /// ongoing turn or login is holding every countdown.
    ///
    /// The hold is the whole point of "after a timeout *if the ongoing command
    /// is completed*": a device-flow code or a post-turn violation report must
    /// not vanish while the user is still mid-flow. [`Duration::ZERO`] means
    /// a notice's timeout already elapsed (e.g. during a long turn) -- expire
    /// it on the next idle tick. A sticky notice never contributes a
    /// deadline: it leaves only by Esc or `/clear`.
    pub fn banner_expiry(&self) -> Option<Duration> {
        if self.ongoing() {
            return None;
        }
        self.pending_messages
            .iter()
            .filter(|notice| !notice.sticky)
            .map(|notice| BANNER_TIMEOUT.saturating_sub(notice.at.elapsed()))
            .min()
    }

    /// Whether a turn is streaming or a login flow is running -- the states
    /// whose own event loop pauses the banner's countdown.
    fn ongoing(&self) -> bool {
        self.streaming || self.pending_login.is_some()
    }

    /// Drop every plain notice whose own [`BANNER_TIMEOUT`] has elapsed,
    /// leaving any sticky one in place. The driver's timer wake calls this,
    /// never [`Self::clear_banner`]: an elapsed timeout is not the same
    /// gesture as Esc or `/clear`, which drop everything regardless of age or
    /// stickiness.
    pub fn expire_banner(&mut self) {
        self.pending_messages
            .retain(|notice| notice.sticky || notice.at.elapsed() < BANNER_TIMEOUT);
    }

    /// Drop every pending message unconditionally, sticky or not. The
    /// explicit-dismissal path: `/clear`, Esc on a held banner, and starting
    /// a fresh command all mean "get rid of all of this now," which is a
    /// different gesture from a notice quietly timing out
    /// ([`Self::expire_banner`]).
    pub fn clear_banner(&mut self) {
        self.pending_messages.clear();
    }

    /// Note a command auto mode just answered for. The newest note *replaces*
    /// any older one: acceptances arrive in bursts through an auto-mode turn,
    /// and the display is one transient line -- the accumulating banner is the
    /// shape this replaces.
    pub fn note_auto_accepted(&mut self, description: String) {
        self.auto_accepted = Some(AutoAccepted {
            description,
            at: Instant::now(),
        });
    }

    /// The acceptance note worth showing right now, or `None` once it has
    /// expired (or was never noted). Freshness is decided here, not in the
    /// view, so every redraw drops an expired note even if no expire wake has
    /// fired to clear the field yet.
    pub fn auto_accepted_line(&self) -> Option<&str> {
        let note = self.auto_accepted.as_ref()?;
        (note.at.elapsed() < AUTO_ACCEPT_TIMEOUT).then_some(note.description.as_str())
    }

    /// How long until the acceptance note leaves, for the driver's expire
    /// wake; `None` when there is nothing to expire. [`Duration::ZERO`] means
    /// the timeout already elapsed -- clear on the next tick. Unlike
    /// [`Self::banner_expiry`] this runs *during* a turn: the note is a
    /// per-acceptance fact, not a banner the turn holds.
    pub fn auto_accepted_expiry(&self) -> Option<Duration> {
        let note = self.auto_accepted.as_ref()?;
        Some(AUTO_ACCEPT_TIMEOUT.saturating_sub(note.at.elapsed()))
    }

    /// Drop the acceptance note, however long it had left.
    pub fn clear_auto_accepted(&mut self) {
        self.auto_accepted = None;
    }
}

/// Rebuild the visible conversation from a session journal: user turns,
/// assistant text, thinking, and tool calls with their outcomes — the same
/// shapes [`reduce`] builds live, folded straight from the journal's records so
/// a resumed session opens on its own transcript instead of an empty screen.
/// Permission, usage, and bookkeeping records have no on-screen shape and are
/// skipped; tool calls render from the `ToolCall`/`ToolResult` pair (name,
/// args, outcome), so a `ToolUse` block inside a `ModelTurn` is not also
/// rendered (that would double every call).
pub fn rebuild_transcript(records: &[journal::Record]) -> Vec<Item> {
    let mut transcript = Vec::new();
    for record in records {
        match &record.kind {
            journal::RecordKind::UserMessage { text } => {
                transcript.push(Item::User(strip_shell_context(text).to_string()));
            }
            journal::RecordKind::ModelTurn(turn) => {
                for block in &turn.blocks {
                    match block {
                        journal::Block::Text { text } => {
                            transcript.push(Item::Assistant(text.clone()))
                        }
                        journal::Block::Thinking(thinking) => transcript.push(Item::Thinking {
                            text: thinking.text.clone(),
                            collapsed: true,
                        }),
                        // Rendered from the ToolCall/ToolResult events instead;
                        // see the doc comment.
                        journal::Block::ToolUse(_) => {}
                    }
                }
            }
            journal::RecordKind::ToolCall(call) => transcript.push(Item::Tool(ToolView {
                call: call.call.clone(),
                name: call.name.clone(),
                args_summary: summarize_args(&call.name, &call.args),
                status: ToolStatus::Running,
                body: None,
                collapsed: true,
            })),
            journal::RecordKind::ToolResult(result) => {
                // The newest still-open view for this call: a call id appearing
                // twice means the first never finished, and the open one is the
                // one this result closes.
                for item in transcript.iter_mut().rev() {
                    if let Item::Tool(view) = item
                        && view.call == result.call
                        && view.status == ToolStatus::Running
                    {
                        let (status, body) = tool_outcome(&result.outcome);
                        view.status = status;
                        view.body = body;
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    // A turn cancelled mid-call leaves its tool view `Running`; close it as
    // timed-out-shaped bookkeeping so the rebuilt transcript never shows a
    // spinner for work that stopped when the session did.
    for item in &mut transcript {
        if let Item::Tool(view) = item
            && view.status == ToolStatus::Running
        {
            view.status = ToolStatus::TimedOut;
        }
    }
    transcript
}

/// Fold one [`UiEvent`] into the state. Pure and headless: this is where the
/// tests live.
///
/// `TextDelta` appends to the current assistant item (opening one if the last
/// item is not assistant text); `TurnReset` drops the in-flight assistant
/// message; `ToolStarted` pushes a running tool; `ToolFinished` sets its outcome;
/// `Usage` updates the status totals; `TurnEnded`/`Failed` end streaming.
pub fn reduce(state: &mut TuiState, event: UiEvent) {
    match event {
        UiEvent::TurnStarted { user } => {
            state.turn_uploaded_tokens = state
                .status
                .context_tokens
                .max((user.len() as u64).div_ceil(4));
            state.turn_downloaded_tokens = 0;
            state.streaming_output_chars = 0;
            state
                .transcript
                .push(Item::User(strip_shell_context(&user).to_string()));
            state.streaming = true;
            state.turn_started_at = Some(Instant::now());
        }
        UiEvent::TextDelta { text } => {
            state.streaming_output_chars += text.len();
            state.turn_downloaded_tokens = (state.streaming_output_chars as u64)
                .div_ceil(4)
                .max(state.turn_downloaded_tokens);
            match state.transcript.last_mut() {
                Some(Item::Assistant(buf)) => buf.push_str(&text),
                _ => state.transcript.push(Item::Assistant(text)),
            }
        }
        UiEvent::ThinkingDelta { text } => {
            state.streaming_output_chars += text.len();
            state.turn_downloaded_tokens = (state.streaming_output_chars as u64)
                .div_ceil(4)
                .max(state.turn_downloaded_tokens);
            match state.transcript.last_mut() {
                Some(Item::Thinking { text: buf, .. }) => buf.push_str(&text),
                _ => state.transcript.push(Item::Thinking {
                    text,
                    collapsed: true,
                }),
            }
        }
        // The provider discarded the partial turn and re-sent it whole. The UI
        // has already been handed the dead attempt's deltas, so drop the in-flight
        // assistant message (text and thinking) back to the last non-streaming
        // item. This only ever touches the trailing stream of a
        // single round, which is where a restart can occur.
        UiEvent::TurnReset {} => {
            state.streaming_output_chars = 0;
            state.turn_downloaded_tokens = 0;
            while matches!(
                state.transcript.last(),
                Some(Item::Assistant(_)) | Some(Item::Thinking { .. })
            ) {
                state.transcript.pop();
            }
        }
        UiEvent::ToolStarted { call, name, args } => {
            if name == PLAN_TOOL
                && let Some(plan) = PlanDisplay::from_args(&args)
            {
                state.plan = Some(plan);
            }
            // An edit is shown unfolded, as Claude Code shows its diffs: the
            // change is what a reader wants to see, and its result is the
            // hunk diff that says where it landed.
            let collapsed = !state.expand_tools && name != EDIT_TOOL;
            state.transcript.push(Item::Tool(ToolView {
                call,
                args_summary: summarize_args(&name, &args),
                name,
                status: ToolStatus::Running,
                body: None,
                collapsed,
            }));
        }
        UiEvent::ToolFinished { call, outcome } => {
            for item in state.transcript.iter_mut().rev() {
                if let Item::Tool(view) = item
                    && view.call == call
                {
                    let (status, body) = tool_outcome(&outcome);
                    view.status = status;
                    view.body = body;
                    break;
                }
            }
        }
        UiEvent::Usage {
            usage,
            total_tokens,
        } => {
            state.status.total_tokens = total_tokens;
            state.turn_uploaded_tokens =
                u64::from(usage.input) + u64::from(usage.cache_read) + u64::from(usage.cache_write);
            state.turn_downloaded_tokens = u64::from(usage.output);
            state.status.context_tokens = state.turn_uploaded_tokens + state.turn_downloaded_tokens;
            state.usage.push(usage);
        }
        // The totals on `Usage` already include this; what the vector adds is
        // the ability to say how much of the bill was delegated.
        UiEvent::Delegated { usage } => state.delegated.push(usage),
        UiEvent::TurnEnded { .. } => {
            state.streaming = false;
            state.turn_started_at = None;
        }
        UiEvent::Failed { message } => {
            state.streaming = false;
            state.turn_started_at = None;
            state.transcript.push(Item::Error(message));
        }
    }
}

/// Render a tool's [`Outcome`] to a status and an optional body string.
fn tool_outcome(outcome: &Outcome) -> (ToolStatus, Option<String>) {
    match outcome {
        Outcome::Ok { content } => (ToolStatus::Done, Some(content.clone())),
        Outcome::Denied { reason } => (ToolStatus::Denied, Some(reason.clone())),
        Outcome::Err { message } => (ToolStatus::Failed, Some(message.clone())),
        Outcome::TimedOut => (ToolStatus::TimedOut, None),
    }
}

/// A short summary of a tool's arguments: the path for file tools, the command
/// for shell, the pattern for grep, and a trimmed JSON blob for anything else.
/// The goal is a header a person can scan, not a faithful rendering.
///
/// **One line, with its breaks shown.** A header is a single span, and ratatui
/// drops a newline inside a span: `echo a` + newline + `rm -rf build` read as
/// `echo arm -rf build`, a command that never ran, in the line a user scans
/// to judge what did.
pub(crate) fn summarize_args(name: &str, args: &serde_json::Value) -> String {
    let field = match name {
        "read" | "edit" | "write" => "path",
        "grep" => "pattern",
        "shell" => "command",
        _ => return truncate(&one_line(&args.to_string()), 60),
    };
    match args.get(field).and_then(|value| value.as_str()) {
        Some(text) => truncate(&one_line(text), 60),
        None => truncate(&one_line(&args.to_string()), 60),
    }
}

/// `text` on one line: each line break shown as `⏎`, each tab as a space.
pub(crate) fn one_line(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\n', " ⏎ ")
        .replace('\t', " ")
}

/// Truncate to `limit` characters on a character boundary, with an ellipsis.
pub(crate) fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    format!("{}…", &text[..end])
}

/// Format a timestamp as a compact relative age: "just now", "5m ago", "2h ago", "3d ago", "2w ago", "3mo ago", "1y ago".
pub fn format_age(time: std::time::SystemTime) -> String {
    format_age_from(time, std::time::SystemTime::now())
}

/// Pure helper for [`format_age`], so relative time formatting is deterministic in tests.
pub fn format_age_from(time: std::time::SystemTime, now: std::time::SystemTime) -> String {
    let compact = format_duration_compact(now.duration_since(time).unwrap_or(Duration::ZERO));
    if compact.is_empty() {
        "just now".to_string()
    } else {
        format!("{compact} ago")
    }
}

/// The compact breakdown [`format_age_from`] adds "ago" to: "5m", "2h", "3d",
/// "2w", "3mo", "1y", or `""` for anything under a second. Also what a
/// [`Notice`]'s own `at` renders as directly (an `Instant`, not a
/// `SystemTime`, so it has no timestamp to hand `format_age` in the first
/// place) -- the same compactness, without implying "ago" for a duration a
/// held notice is still accumulating rather than looking back on.
pub(crate) fn format_duration_compact(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs == 0 {
        String::new()
    } else if secs < SECS_PER_MINUTE {
        format!("{secs}s")
    } else if secs < SECS_PER_HOUR {
        let mins = secs / SECS_PER_MINUTE;
        let rem_secs = secs % SECS_PER_MINUTE;
        if rem_secs == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m {rem_secs}s")
        }
    } else if secs < SECS_PER_DAY {
        format!("{}h", secs / SECS_PER_HOUR)
    } else if secs < SECS_PER_WEEK {
        format!("{}d", secs / SECS_PER_DAY)
    } else if secs < SECS_PER_MONTH {
        format!("{}w", secs / SECS_PER_WEEK)
    } else if secs < SECS_PER_YEAR {
        format!("{}mo", secs / SECS_PER_MONTH)
    } else {
        format!("{}y", secs / SECS_PER_YEAR)
    }
}

/// Capitalize the first word (the first non-whitespace character) in `text`.
pub(crate) fn capitalize_first(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut capitalized = false;
    for ch in text.chars() {
        if !capitalized && !ch.is_whitespace() {
            for upper in ch.to_uppercase() {
                result.push(upper);
            }
            capitalized = true;
        } else {
            result.push(ch);
        }
    }
    result
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use journal::{CallId, Outcome};

    /// The banner's text, in order, for a test that does not care about
    /// [`Notice::sticky`].
    fn notice_texts(state: &TuiState) -> Vec<&str> {
        state
            .pending_messages
            .iter()
            .map(|notice| notice.text.as_str())
            .collect()
    }

    fn status() -> Status {
        Status {
            model: "mock".into(),
            effort: None,
            execution_mode: ExecutionMode::Interactive,
            total_tokens: 0,
            context_tokens: 0,
            context_window: None,
            backend: "none".into(),
            cwd: "/repo".into(),
        }
    }

    fn reduce_all(events: Vec<UiEvent>) -> TuiState {
        let mut state = TuiState::new(status());
        for event in events {
            reduce(&mut state, event);
        }
        state
    }

    #[test]
    fn a_plan_call_becomes_the_checklist_and_the_next_one_replaces_it() {
        let plan = |call: &str, args: serde_json::Value| UiEvent::ToolStarted {
            call: CallId::new(call.into()),
            name: "plan".into(),
            args,
        };
        let state = reduce_all(vec![
            plan(
                "toolu_1",
                serde_json::json!({"steps": ["read", "edit", "test"], "active_step": 1}),
            ),
            plan(
                "toolu_2",
                serde_json::json!({"steps": ["read", "edit", "test"], "active_step": 3, "status": "blocked"}),
            ),
        ]);
        assert_eq!(
            state.plan,
            Some(PlanDisplay {
                active: Some(3),
                progress: PlanProgress::Blocked,
                steps: vec!["read".into(), "edit".into(), "test".into()],
            })
        );

        let state = reduce_all(vec![plan("toolu_3", serde_json::json!({"steps": []}))]);
        assert_eq!(state.plan, None, "a plan with no steps draws nothing");
    }

    #[test]
    fn text_deltas_append_to_one_assistant_item() {
        let state = reduce_all(vec![
            UiEvent::TurnStarted { user: "hi".into() },
            UiEvent::TextDelta { text: "hel".into() },
            UiEvent::TextDelta { text: "lo".into() },
            UiEvent::TurnEnded {
                stop: engine::Stop::Done,
            },
        ]);
        assert!(matches!(
            state.transcript.as_slice(),
            [Item::User(u), Item::Assistant(a)] if u == "hi" && a == "hello"
        ));
        assert!(!state.streaming);
    }

    #[test]
    fn a_tool_runs_then_finishes_in_place() {
        let call = CallId::new("toolu_1".into());
        let state = reduce_all(vec![
            UiEvent::TurnStarted {
                user: "read it".into(),
            },
            UiEvent::ToolStarted {
                call: call.clone(),
                name: "read".into(),
                args: serde_json::json!({"path": "/repo/a.rs"}),
            },
            UiEvent::ToolFinished {
                call: call.clone(),
                outcome: Outcome::Ok {
                    content: "body".into(),
                },
            },
            UiEvent::TurnEnded {
                stop: engine::Stop::Done,
            },
        ]);
        let tool = state
            .transcript
            .iter()
            .find_map(|item| match item {
                Item::Tool(view) => Some(view),
                _ => None,
            })
            .unwrap();
        assert_eq!(tool.name, "read");
        assert_eq!(tool.args_summary, "/repo/a.rs");
        assert_eq!(tool.status, ToolStatus::Done);
        assert_eq!(tool.body.as_deref(), Some("body"));
    }

    #[test]
    fn turn_reset_drops_the_in_flight_assistant_message() {
        // The provider streamed half a turn, then restarted it. The dead text
        // must not survive into the transcript: without a reset the UI renders
        // text from an attempt the model never finished.
        let state = reduce_all(vec![
            UiEvent::TurnStarted { user: "hi".into() },
            UiEvent::TextDelta {
                text: "dead attem".into(),
            },
            UiEvent::TurnReset {},
            UiEvent::TextDelta {
                text: "real text".into(),
            },
            UiEvent::TurnEnded {
                stop: engine::Stop::Done,
            },
        ]);
        assert!(matches!(
            state.transcript.last(),
            Some(Item::Assistant(a)) if a == "real text"
        ));
    }

    #[test]
    fn usage_updates_the_status_totals() {
        let state = reduce_all(vec![UiEvent::Usage {
            usage: journal::Usage::new(1, 2, 0, 0),
            total_tokens: 4500,
        }]);
        assert_eq!(state.status.total_tokens, 4500);
    }

    #[test]
    fn usage_sets_context_tokens_including_cache_read() {
        // The context-window count must include `cache_read` (a cached token
        // still occupies the window), which `total_tokens` deliberately drops.
        // 100 input + 9 cache_read + 0 cache_write + 20 output = 129 in play.
        let state = reduce_all(vec![UiEvent::Usage {
            usage: journal::Usage::new(100, 20, 9, 0),
            total_tokens: 120,
        }]);
        assert_eq!(state.status.context_tokens, 129);
        assert_eq!(
            state.status.total_tokens, 120,
            "runaway total excludes cache_read"
        );
    }

    #[test]
    fn a_failed_turn_pushes_an_error_and_clears_streaming() {
        let state = reduce_all(vec![
            UiEvent::TurnStarted { user: "hi".into() },
            UiEvent::Failed {
                message: "stream broke".into(),
            },
        ]);
        assert!(!state.streaming);
        assert!(matches!(
            state.transcript.last(),
            Some(Item::Error(m)) if m == "stream broke"
        ));
    }

    #[test]
    fn rebuild_transcript_replays_the_recorded_conversation() {
        // What `-c` and `/resume` open on: user turns, assistant text, thinking
        // (collapsed), and tool calls closed by their results — the same shapes
        // `reduce` builds live.
        let call = journal::CallId::new("toolu_1".into());
        let records = vec![
            journal::Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: journal::RecordId::new(),
                kind: journal::RecordKind::UserMessage {
                    text: "read it".into(),
                },
                parent: None,
                session: journal::SessionId::new(),
            },
            journal::Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: journal::RecordId::new(),
                kind: journal::RecordKind::ModelTurn(journal::ModelTurn::new(
                    vec![
                        journal::Block::Thinking(journal::BlockThinking::new(
                            "sig".into(),
                            "pondering".into(),
                        )),
                        journal::Block::Text {
                            text: "on it".into(),
                        },
                    ],
                    journal::StopReason::ToolUse,
                    journal::Usage::zero(),
                )),
                parent: None,
                session: journal::SessionId::new(),
            },
            journal::Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: journal::RecordId::new(),
                kind: journal::RecordKind::ToolCall(journal::ToolCall::new(
                    call.clone(),
                    "read".into(),
                    serde_json::json!({"path": "/repo/a.rs"}),
                )),
                parent: None,
                session: journal::SessionId::new(),
            },
            journal::Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: journal::RecordId::new(),
                kind: journal::RecordKind::ToolResult(journal::ToolResult::new(
                    call.clone(),
                    journal::Outcome::Ok {
                        content: "body".into(),
                    },
                )),
                parent: None,
                session: journal::SessionId::new(),
            },
        ];
        let items = rebuild_transcript(&records);
        assert!(matches!(&items[0], Item::User(t) if t == "read it"));
        assert!(
            matches!(&items[1], Item::Thinking { text, collapsed: true } if text == "pondering")
        );
        assert!(matches!(&items[2], Item::Assistant(t) if t == "on it"));
        match &items[3] {
            Item::Tool(view) => {
                assert_eq!(view.name, "read");
                assert_eq!(view.status, ToolStatus::Done);
                assert_eq!(view.body.as_deref(), Some("body"));
            }
            other => panic!("expected a tool, got {other:?}"),
        }
    }

    #[test]
    fn rebuild_transcript_closes_a_call_the_journal_left_open() {
        // A turn cancelled mid-call never recorded a result; the rebuilt view
        // must not show a spinner for work that stopped with the session.
        let call = journal::CallId::new("toolu_2".into());
        let records = vec![journal::Record {
            at: std::time::SystemTime::UNIX_EPOCH,
            id: journal::RecordId::new(),
            kind: journal::RecordKind::ToolCall(journal::ToolCall::new(
                call,
                "shell".into(),
                serde_json::json!({"command": "make"}),
            )),
            parent: None,
            session: journal::SessionId::new(),
        }];
        let items = rebuild_transcript(&records);
        match &items[0] {
            Item::Tool(view) => assert_eq!(view.status, ToolStatus::TimedOut),
            other => panic!("expected a tool, got {other:?}"),
        }
    }

    #[test]
    fn notify_appends_to_pending_messages_and_never_touches_the_transcript() {
        // The bug this pins: a command result leaking into `transcript` would
        // put UI chrome back in the conversation record it was moved out of.
        let mut state = TuiState::new(status());
        state.notify("stored a credential for Anthropic");
        assert_eq!(notice_texts(&state), ["Stored a credential for Anthropic"]);
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn notify_capitalizes_the_first_word_of_new_notifications() {
        let mut state = TuiState::new(status());
        state.notify("resumed session");
        state.notify_error("failed to connect");
        state.notify("  leading whitespace preserved");
        state.notify("Already capitalized");
        assert_eq!(
            notice_texts(&state),
            [
                "Resumed session",
                "Failed to connect",
                "  Leading whitespace preserved",
                "Already capitalized",
            ]
        );
        assert_eq!(
            state
                .pending_messages
                .iter()
                .map(|n| n.sticky)
                .collect::<Vec<_>>(),
            [false, true, false, false],
            "only the notify_error line is sticky"
        );
    }

    #[test]
    fn a_second_notify_does_not_erase_the_first() {
        // The exact regression the review named: the device flow's device
        // code, immediately followed by its "open in browser?" prompt with no
        // redraw between them, must not lose the code to the second message
        // overwriting the first.
        let mut state = TuiState::new(status());
        state.notify("go to https://github.com/login/device and enter code ABCD-1234");
        state.notify("open that URL in your browser now? [Y/n]");
        assert_eq!(
            notice_texts(&state),
            [
                "Go to https://github.com/login/device and enter code ABCD-1234",
                "Open that URL in your browser now? [Y/n]",
            ]
        );
    }

    #[test]
    fn banner_expiry_is_none_without_any_messages() {
        let state = TuiState::new(status());
        assert_eq!(state.banner_expiry(), None);
    }

    #[test]
    fn a_repeated_notice_replaces_its_copy_instead_of_stacking() {
        let mut state = TuiState::new(status());
        state.notify("the output goes to the agent with your next message");
        state.notify("queued (1)");
        state.notify("the output goes to the agent with your next message");
        let texts: Vec<&str> = state
            .pending_messages
            .iter()
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "Queued (1)",
                "The output goes to the agent with your next message"
            ],
            "one copy, moved to the newest"
        );
    }

    #[test]
    fn notify_starts_the_banner_countdown() {
        let mut state = TuiState::new(status());
        state.notify("hi");
        // Idle with a fresh message: a countdown is pending and not yet spent.
        let remaining = state.banner_expiry().expect("countdown pending");
        assert!(
            remaining <= BANNER_TIMEOUT && remaining > Duration::ZERO,
            "remaining is within the timeout: {remaining:?}"
        );
    }

    #[test]
    fn banner_expiry_is_held_while_a_turn_streams() {
        let mut state = TuiState::new(status());
        state.notify("hi");
        state.streaming = true;
        assert_eq!(state.banner_expiry(), None, "held while a turn runs");
    }

    #[test]
    fn banner_expiry_is_held_during_a_login_flow() {
        let mut state = TuiState::new(status());
        state.notify("enter the device code");
        state.pending_login = Some("waiting for GitHub…".into());
        assert_eq!(state.banner_expiry(), None, "held while login runs");
    }

    #[test]
    fn banner_expiry_is_zero_once_the_timeout_has_elapsed() {
        // Back-date the notice past the timeout, the way a long turn would.
        let mut state = TuiState::new(status());
        state.notify("hi");
        state.pending_messages[0].at = Instant::now()
            .checked_sub(BANNER_TIMEOUT + Duration::from_secs(1))
            .unwrap();
        assert_eq!(state.banner_expiry(), Some(Duration::ZERO));
    }

    #[test]
    fn an_error_banner_is_held_not_timed_out() {
        // The regression the user hit: "could not store credential" vanished
        // on the same five-second timer as every command result, before it
        // could be read. An error suspends its own countdown outright -- even
        // idle, even long past what would be the timeout, no expiry is
        // scheduled for it. Only a dismiss (Esc) or `/clear` drops it.
        let mut state = TuiState::new(status());
        state.notify_error("could not store credential: permission denied");
        assert!(state.banner_sticky());
        state.pending_messages[0].at = Instant::now()
            .checked_sub(BANNER_TIMEOUT + Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            state.banner_expiry(),
            None,
            "a held error never schedules its own auto-clear"
        );
    }

    #[test]
    fn an_error_does_not_hold_the_messages_shown_beside_it() {
        // The regression this replaces: a single sticky flag over the whole
        // banner meant an error arriving beside a routine notice froze both
        // until Esc, even though only the error needed holding. Each
        // notice's clock is now its own.
        let mut state = TuiState::new(status());
        state.notify("fetching models from GitHub Copilot...");
        state.notify_error("could not fetch Copilot models");
        assert!(state.banner_sticky(), "the error is still held");
        assert!(
            state.banner_expiry().is_some(),
            "the plain notice beside it still counts down on its own"
        );

        // Back-date the plain notice past its timeout, the way a long turn
        // would, without touching the sticky one.
        state.pending_messages[0].at = Instant::now()
            .checked_sub(BANNER_TIMEOUT + Duration::from_secs(1))
            .unwrap();
        state.expire_banner();
        assert_eq!(
            notice_texts(&state),
            ["Could not fetch Copilot models"],
            "expiry drops the timed-out plain notice and leaves the sticky one"
        );
    }

    #[test]
    fn a_fresh_notice_keeps_an_older_plain_one_company() {
        // Without the refresh, an old notice sitting almost at its timeout
        // when a second one arrives would vanish moments later, mid-read,
        // while the second one is still fresh.
        let mut state = TuiState::new(status());
        state.notify("first");
        state.pending_messages[0].at = Instant::now()
            .checked_sub(BANNER_TIMEOUT - Duration::from_millis(10))
            .unwrap();
        state.notify("second");
        let remaining = state.banner_expiry().expect("still counting down");
        assert!(
            remaining > Duration::from_millis(100),
            "the first notice's clock was restarted by the second: {remaining:?}"
        );
    }

    #[test]
    fn a_dismissed_error_banner_counts_down_again_for_later_notices() {
        let mut state = TuiState::new(status());
        state.notify_error("boom");
        state.clear_banner();
        assert!(!state.banner_sticky(), "the dismiss releases the hold");
        state.notify("ok");
        assert!(
            state.banner_expiry().is_some(),
            "the next ordinary notice auto-clears as before"
        );
    }

    #[test]
    fn a_secret_input_masks_one_star_per_character_not_per_byte() {
        let mut secret = SecretInput::new(ProviderId::Anthropic);
        assert_eq!(secret.masked(), "");
        assert!(secret.is_empty());
        secret.push('k');
        secret.push('\u{e9}'); // two bytes, one character
        secret.push('y');
        assert_eq!(secret.secret(), "k\u{e9}y");
        assert_eq!(secret.masked(), "***", "one star per character");
        assert!(!secret.is_empty());
        assert!(secret.pop());
        assert_eq!(secret.secret(), "k\u{e9}");
        assert_eq!(secret.masked(), "**");
        // Popping an empty entry reports that nothing was deleted.
        let mut empty = SecretInput::new(ProviderId::ZAi);
        assert!(!empty.pop());
    }

    #[test]
    fn a_pasted_secret_is_trimmed_before_it_joins_the_buffer() {
        // The clipboard path: a key copied out of a provider dashboard
        // arrives with the newline the selection carried. Storing that
        // newline would corrupt the key, so the ends are trimmed -- and a
        // paste into an entry that already has typed characters appends,
        // like every other input.
        let mut secret = SecretInput::new(ProviderId::Anthropic);
        secret.push_str("  sk-ant-pasted\r\n");
        assert_eq!(secret.secret(), "sk-ant-pasted");
        assert_eq!(secret.masked().chars().count(), 13);

        secret.push_str("-tail");
        assert_eq!(secret.secret(), "sk-ant-pasted-tail");
    }

    #[test]
    fn clear_banner_empties_messages() {
        let mut state = TuiState::new(status());
        state.notify("hi");
        state.clear_banner();
        assert!(state.pending_messages.is_empty());
        assert_eq!(state.banner_expiry(), None);
    }

    #[test]
    fn an_acceptance_note_replaces_its_predecessor() {
        // Auto mode answers a prompt per tool call, so notes arrive in bursts.
        // One transient line means the newest replaces the older one -- the
        // opposite of the banner, which appends precisely because its messages
        // must all stay readable. Stacking acceptances was the wall of
        // `Auto-accepted:` lines this display replaced.
        let mut state = TuiState::new(status());
        state.note_auto_accepted("run `sh -c make check`".into());
        state.note_auto_accepted("edit crates/cli/src/main.rs".into());
        assert_eq!(
            state.auto_accepted_line(),
            Some("edit crates/cli/src/main.rs"),
            "only the newest note shows"
        );
        assert_eq!(
            state.pending_messages.len(),
            0,
            "the note is not banner chrome"
        );
    }

    #[test]
    fn an_acceptance_note_expires_even_mid_turn() {
        // The note is a per-acceptance fact, not a banner the turn holds: a
        // tool that runs long after its acceptance must not pin the line over
        // the transcript for the whole run. The banner's countdown, by
        // contrast, is held while a turn streams.
        let mut state = TuiState::new(status());
        state.note_auto_accepted("run `sh -c make check`".into());
        state.streaming = true;
        assert_eq!(state.banner_expiry(), None, "sanity: the banner is held");
        assert!(
            state.auto_accepted_expiry().is_some(),
            "the note's clock runs while a turn streams"
        );

        // Back-date the note past its timeout, the way a long tool call would.
        let stale = Instant::now().checked_sub(AUTO_ACCEPT_TIMEOUT + Duration::from_secs(1));
        if let Some(note) = &mut state.auto_accepted {
            note.at = stale.expect("a timeout-sized step back fits an Instant");
        }
        assert_eq!(
            state.auto_accepted_line(),
            None,
            "an expired note does not show"
        );
        assert_eq!(
            state.auto_accepted_expiry(),
            Some(Duration::ZERO),
            "an expired note asks to be cleared on the next tick"
        );
        state.clear_auto_accepted();
        assert_eq!(
            state.auto_accepted_expiry(),
            None,
            "a cleared note has no timer"
        );
    }

    #[test]
    fn a_multi_line_command_shows_its_line_break_in_the_header() {
        // A span drops `\n`, so this rendered as `echo arm -rf build`.
        let summary = summarize_args(
            "shell",
            &serde_json::json!({"command": "echo a\nrm -rf build"}),
        );
        assert_eq!(summary, "echo a ⏎ rm -rf build");
    }

    #[test]
    fn args_summary_picks_the_named_field_and_truncates() {
        let call = CallId::new("c".into());
        let long = "x".repeat(80);
        let state = reduce_all(vec![UiEvent::ToolStarted {
            call,
            name: "read".into(),
            args: serde_json::json!({"path": long}),
        }]);
        let tool = match state.transcript.last() {
            Some(Item::Tool(view)) => view,
            other => panic!("expected a tool, got {other:?}"),
        };
        assert_eq!(tool.args_summary.chars().count(), 61, "60 chars + ellipsis");
        assert!(tool.args_summary.ends_with('…'));
    }

    #[test]
    fn format_age_renders_compact_relative_durations() {
        let base = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        assert_eq!(format_age_from(base, base), "just now");
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(30)),
            "30s ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(5 * 60)),
            "5m ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(4 * 60 + 45)),
            "4m 45s ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(2 * 3600)),
            "2h ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(3 * 86400)),
            "3d ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(2 * 7 * 86400)),
            "2w ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(3 * 30 * 86400)),
            "3mo ago"
        );
        assert_eq!(
            format_age_from(base, base + Duration::from_secs(2 * 365 * 86400)),
            "2y ago"
        );
    }
}
