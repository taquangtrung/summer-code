//! The summer-code command-line frontend.
//!
//! A REPL: read a line, run one agent turn, print the model's reply. The journal
//! is a file under `.summer/sessions/`, append-only. A live model needs
//! `ANTHROPIC_API_KEY`; without it the session still starts, and the turn the
//! user attempts is what tells them what to set, rather than the CLI refusing to
//! open at all.
//!
//! `cli` is a harness crate (it opens the journal file and reads config),
//! so it carries the `#![allow(clippy::disallowed_methods)]` exemption.

#![allow(clippy::disallowed_methods)]
// `unsafe_code` is denied workspace-wide, and this crate's one use of it is
// `std::env::set_var`, which edition 2024 made unsafe because another thread may
// be reading the environment concurrently. The REPL sets a variable for the
// session it is about to start, single-threaded, before anything else runs; see
// the `SAFETY` comments at the two call sites in `repl`.
#![allow(unsafe_code)]

mod command;
mod config;
mod console;
mod credentials;
mod discovery;
mod flags;
mod keybindings;
mod mcp;
mod mode;
mod notify;
mod permission;
mod policy;
mod print;
mod read_only_gate;
mod repl;
mod rules;
mod skill;
mod subagent;
mod tui;

use std::error::Error;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, channel};

use engine::{
    Broker, BrokerBuilder, EngineBuilder, Harness, HookSet, Limits, NoneBackend, PermissionRecord,
    Prompter, SYSTEM_PROMPT, SandboxBackend, SkillGate, Snapshots, for_platform,
};
use journal::{JournalSink, WriterSink};
use provider::{
    Effort, Provider, ProviderConfig, ProviderId, Unconfigured, UnknownProviderId, transport,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tools::{Builtin, Dispatcher, ShellSpec, ToolSource};

use crate::console::Console;
use crate::flags::{
    CONTINUE_FLAG, CONTINUE_FLAG_SHORT, KEYMAP_FLAG, MODE_FLAG, MODEL_FLAG, NO_SANDBOX_FLAG,
    PLAIN_FLAG, PROVIDER_FLAG, Requested, SANDBOX_FLAG, resolve_request, usage_text,
};
use crate::mode::{ExecutionMode, Mode};
use crate::permission::TerminalPrompter;
use crate::print::{ExitCode, PrintRequest};
use crate::read_only_gate::ReadOnlyGate;
use crate::tui::{PromptRequest, TuiPrompter};

/// The port the sandbox carves out for the model when there is no configured
/// provider to read one from. HTTPS, because that is where the API lives.
const DEFAULT_MODEL_PORT: u16 = 443;

/// Whether this process entered kernel confinement.
///
/// **A process-global, because that is what it describes.** Landlock restricts
/// the process and everything it later spawns; there is no scope smaller than
/// the process for this fact to live in, and threading a `bool` from bootstrap
/// through both frontends into [`open_url`] would be modelling it as
/// call-site-local when it is not. Written once during bootstrap, before the
/// runtime exists, and only read afterwards.
static CONFINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ========================================================================
// Data Structures
// ========================================================================

/// Everything read before the sandbox closes, plus the broker that closed it.
///
/// **The struct exists to make the ordering a type rather than a comment.**
/// Landlock domains are inherited only by threads created *after* the
/// restriction, and everything here either reads a path outside the repo root or
/// must happen before the tokio runtime exists. Both constraints were previously
/// stated in comments and violated by the code: the runtime was built first (so
/// no tool ever ran restricted) and skills were loaded after `enter` (so
/// `~/.summer/skills` was unreadable and the whole feature silently no-opped).
struct Bootstrap {
    backend_name: &'static str,
    broker: Broker,
    cwd: PathBuf,
    /// `-c`: the newest prior journal to fold in before the first turn.
    resume: Option<PathBuf>,
    /// Where this session's journal lives: the central per-project store, or
    /// the in-project `.summer/sessions` under `--sandbox` / no-home. Carried
    /// so the TUI's `/sessions`, `/resume`, and `/status` agree with it.
    sessions_dir: PathBuf,
    /// User-configured commands the engine runs around tool calls. Built from
    /// the user tier of the config only; see `config::without_project_hooks`.
    hooks: HookSet,
    /// External MCP servers to start once the runtime exists. Carried rather
    /// than connected here because discovery is async and every server is a
    /// child process: both want the tokio runtime, and under `--sandbox` a
    /// child started after `enter` correctly inherits the confined domain.
    /// User tier only; see `config::without_project_mcp_servers`.
    mcp_servers: Vec<config::McpServerConfig>,
    /// The broker's decisions, for the engine to drain.
    permissions: Receiver<PermissionRecord>,
    /// Closed while `/mode plan` holds: the broker's evaluator reads it, the
    /// TUI flips it.
    read_only_gate: Arc<AtomicBool>,
    plain: bool,
    policy_text: String,
    /// `None` on the plain path, which prompts on stdin instead.
    prompt_rx: Option<UnboundedReceiver<PromptRequest>>,
    /// `None` when no credential is set; the session still starts.
    provider_config: Option<ProviderConfig>,
    /// The provider `provider_config` was built for, or was going to be built
    /// for had a credential existed — kept even in the no-credential case so
    /// the session banner and warnings name the right one.
    provider_id: ProviderId,
    /// The persisted reasoning effort for the startup model, applied to the
    /// engine before the first turn and mirrored into the TUI state.
    effort: Option<Effort>,
    /// The cached context window for the startup model, so the statusline shows
    /// `X% ctx (size)` from the first frame even for a live Copilot model.
    context_window: Option<u64>,
    /// User-defined slash commands: saved prompts behind a name. Both tiers,
    /// unlike hooks; see [`crate::command`] for why that difference is sound.
    commands: command::Registry,
    /// Custom key bindings from `~/.summer/keybindings.json`. Empty when the
    /// file is absent, which is the built-in bindings unchanged.
    keymap: keybindings::Keymap,
    /// Pre-write file states, for `/rewind`. Shared with the broker, which is
    /// what fills it.
    snapshots: Arc<Snapshots>,
    /// How much this session may do without asking, from `--mode`. Already
    /// merged into the policy; kept so the prompt and the banner can say which
    /// mode produced it.
    mode: Mode,
    /// The shell command lines run through, chosen once for the session.
    shell: ShellSpec,
    /// Which skills an edit must have loaded first. Built here, with `skills`,
    /// because a rule naming a skill the registry lacks is dropped with a
    /// warning, and that check needs both.
    skill_gate: SkillGate,
    skills: skill::Registry,
    statusline: tui::Statusline,
    /// Ring the terminal bell / send a desktop notification when a turn ends.
    /// From `config.notify`, defaulting on.
    notify: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    init_tracing();

    // Parsed before anything else reads the environment: a usage error
    // should cost nothing, and in particular should not open a journal or
    // enter a sandbox first. `--help` and `--version` answer here for the
    // same reason.
    let args: Vec<String> = std::env::args().collect();
    match resolve_request(&args) {
        Ok(Requested::Run) => {}
        Ok(Requested::Help) => {
            println!("{}", usage_text());
            return Ok(());
        }
        Ok(Requested::Version) => {
            println!("summer {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(message) => {
            eprintln!("summer: {message}");
            std::process::exit(ExitCode::Usage as i32);
        }
    }
    let print = match PrintRequest::from_args(&args).and_then(PrintRequest::resolve_prompt_if_asked)
    {
        Ok(print) => print,
        Err(message) => {
            eprintln!("summer: {message}");
            std::process::exit(ExitCode::Usage as i32);
        }
    };

    // The pre-sandbox phase runs here, on the main thread, with no runtime in
    // existence. `Runtime::new` eagerly spawns its worker pool, and a worker
    // spawned before `enter` never joins the Landlock domain -- which is exactly
    // what happened: `Dispatcher::dispatch` runs every tool inside
    // `tokio::spawn`, so every `read`, `edit` and `shell` call ran with full
    // filesystem access while the banner reported `sandbox: landlock`.
    let bootstrap = Bootstrap::load()?;
    let rt = tokio::runtime::Runtime::new()?;
    // Headless runs answer with a status, so this exits rather than returning:
    // `?` on a `Box<dyn Error>` would collapse every distinct stop reason into
    // rustc's own exit code 1. The interactive paths always yield `Ok`.
    let code = rt.block_on(async { session(bootstrap, print).await })?;
    std::process::exit(code as i32);
}

// ========================================================================
// Bootstrap Implementations
// ========================================================================

impl Bootstrap {
    /// Read every source outside the root, build the broker, and enter the
    /// sandbox. Nothing after this may read a path the policy does not cover.
    fn load() -> Result<Self, Box<dyn Error>> {
        let sandbox = std::env::args().any(|arg| arg == SANDBOX_FLAG);
        if std::env::args().any(|arg| arg == NO_SANDBOX_FLAG) {
            eprintln!(
                "summer: {NO_SANDBOX_FLAG} is no longer needed; the sandbox is off by default. \
                 Pass {SANDBOX_FLAG} to turn it on."
            );
        }
        // The plain loop is the fallback for anything that is not a real
        // terminal, and the explicit override for someone who wants the line
        // loop on a TTY. The TUI needs raw mode and a writable scrollback; a
        // piped stdout has neither, so `IsTerminal` is the split.
        let plain = std::env::args().any(|arg| arg == PLAIN_FLAG) || !io::stdout().is_terminal();
        // Canonical, because the policy root is what every containment check
        // compares against. A root reached through a symlink would make every
        // path under it look like an escape. `dunce::canonicalize` resolves the
        // same way `std::fs::canonicalize` does; it only differs by not
        // returning Windows's `\\?\` extended-length prefix when the plain form
        // already names the same file, which is what keeps every path derived
        // from `cwd` (the banner, the status bar, `/policy`, the journal path)
        // readable without a second normalizing pass.
        let cwd = dunce::canonicalize(std::env::current_dir()?)?;

        // Read ahead of the mode and the provider: a stored `mode` or
        // `provider` preference is the fallback for `--mode`, `--provider` and
        // `--model`.
        let config = config::load(&cwd)?;
        let mode = resolve_mode(&config)?;
        // `-c`: the newest non-empty journal in this project's store, resolved
        // *before* this session's own (initially empty) journal is opened below
        // — resolved after, "newest" would be the file we just made. `None`
        // with a notice when there is nothing to continue, so the flag is never
        // a silent no-op.
        let wants_continue =
            std::env::args().any(|arg| arg == CONTINUE_FLAG || arg == CONTINUE_FLAG_SHORT);
        let resume = if wants_continue {
            match newest_journal(&policy::journal_dir(&cwd)) {
                Some(path) => Some(path),
                None => {
                    eprintln!(
                        "summer: nothing to continue (no previous session at {}); \
                         starting fresh",
                        policy::journal_dir(&cwd).display()
                    );
                    None
                }
            }
        } else {
            None
        };
        let policy = policy::load(&cwd, mode)?;
        let policy_text = policy::render_policy(&policy);

        let provider_id = resolve_provider_id(&config)?;
        let model = resolve_model(&config);
        let effort = resolve_effort(&config);
        let context_window = resolve_context_window_pref(&config);
        // Building the provider itself is deferred into the runtime, since its
        // HTTP client wants a reactor; only the config (and therefore the
        // port to carve out) is resolved here.
        let provider_config = credentials::credential_for(provider_id)?
            .map(|auth| ProviderConfig::for_profile(provider_id.profile(), model.as_deref(), auth))
            .transpose()?;
        if provider_config.is_none() {
            let profile = provider_id.profile();
            let env_names = profile
                .env_vars
                .iter()
                .map(|var| var.name)
                .collect::<Vec<_>>()
                .join(" or ");
            eprintln!(
                "summer: no credential for {} ({env_names} not set, and nothing stored via \
                 `/login {}`). Starting the session anyway; model turns will fail until you \
                 set one and restart.",
                profile.label,
                profile.id.as_str()
            );
        }
        // Every provider with a credential right now gets its port carved
        // out, not just the selected one: `enter` is irreversible for the
        // rest of the session, so `/model` can only switch to a provider
        // whose port was already carved out here. Falling back to
        // `DEFAULT_MODEL_PORT` keeps today's behavior when nothing at all is
        // configured yet.
        let mut ports: Vec<u16> = credentials::configured_profiles()?
            .iter()
            .filter_map(|profile| profile.port())
            .collect();
        if ports.is_empty() {
            ports.push(DEFAULT_MODEL_PORT);
        }

        // One shell decision for the whole session: the `shell` tool and the
        // command statusline must agree, or a line written for one is run by the
        // other.
        let shell = config::shell(&config);
        let statusline =
            tui::Statusline::from_config(config.tui.status_line.as_ref(), shell.clone());
        let notify = config.notify_enabled();
        // Read from `~/.summer/skills`, which is outside the root and therefore
        // unreadable a few lines from now.
        let skills = match policy::config_dir() {
            Some(dir) => skill::Registry::load(&dir.join("skills")),
            None => skill::Registry::empty(),
        };
        // Built here rather than in `session`, because a bad entry warns on
        // stderr and this is still before the alternate screen is entered: a
        // warning printed after the TUI starts is a warning nobody reads. The
        // skill gate is built here for both reasons at once: it warns the same
        // way, and it is checked against the registry loaded just above.
        let hooks = config::hook_set(&config.hooks);
        let skill_gate = config::skill_gate(&config.skills, &skills);
        // **User tier only.** A binding names an action from a fixed enum, so
        // repo content cannot make one *run* anything -- but `Exit` is in that
        // enum, and a project file binding every key to it would make the
        // session unusable on a freshly cloned repository. Choosing what a
        // keystroke does is the user's, matching how `tui.statusLine` and hooks
        // are tiered.
        //
        // Read here for the same reason skills are: `~/.summer` is outside the
        // root a confined worker can reach.
        let mut keymap = policy::config_dir()
            .map(|dir| keybindings::Keymap::load_from_dir(&dir))
            .unwrap_or_default();
        if keymap.style().is_none()
            && let Some(style) = config.tui.keymap_style()
        {
            keymap.set_style(style);
        }
        if let Some(cli_keymap) = flag_value(KEYMAP_FLAG) {
            keymap.set_style(cli_keymap);
        }
        // The user tier lives outside the root, so like skills it has to be read
        // before `enter` closes the sandbox around it.
        let commands = command::Registry::load(
            policy::config_dir()
                .map(|dir| dir.join(command::USER_COMMANDS))
                .as_deref(),
            &cwd.join(command::PROJECT_COMMANDS),
        );

        // Opt-in; see [`SANDBOX_FLAG`] for the trade the off default makes.
        let backend: Box<dyn SandboxBackend> = if sandbox {
            for_platform()
        } else {
            Box::new(NoneBackend::new("off (pass --sandbox to enable)"))
        };
        let backend_name = backend.name();
        // The broker holds the sender; the engine owns the receiver and drains
        // it before every ToolResult.
        let (decisions, permissions) = channel();
        // The prompter is chosen by frontend: the TUI cannot share stdin with a
        // raw-mode UI, so it routes prompts through a channel the driver
        // answers; the plain loop keeps the blocking terminal prompter.
        let (prompter, prompt_rx): (Box<dyn Prompter>, Option<_>) = if plain {
            let console: Arc<dyn Console> = Arc::new(console::Stdin);
            (Box::new(TerminalPrompter::new(console)), None)
        } else {
            let (tui_prompter, rx) = TuiPrompter::new();
            (Box::new(tui_prompter), Some(rx))
        };

        // Where session journals live: central (Claude Code's
        // `~/.summer/projects/<encoded-root>/sessions` layout) so nothing
        // session-generated lands inside the repo — except under `--sandbox`,
        // A confined process cannot write `~` at all (Landlock governs write()
        // on held fds too, so even a pre-entered handle dies mid-session), so
        // a confined session keeps the in-project store. The same choice is
        // made for snapshots below and threaded into the TUI so `/sessions`,
        // `/resume`, and `/status` list exactly where this session writes.
        let sessions_dir = if sandbox {
            cwd.join(".summer").join("sessions")
        } else {
            policy::journal_dir(&cwd)
        };
        // Migrate legacy in-project stores into the central one, once and only
        // unconfined: under `--sandbox` the session still uses the in-project
        // store, so moving it out from under a live session is the one outcome
        // worse than a stale directory.
        if !sandbox {
            migrate_local_store(&cwd);
        }
        let snapshot_store = if sandbox {
            cwd.join(".summer").join("snapshots")
        } else {
            policy::project_store_dir(&cwd)
                .map(|store| store.join("snapshots"))
                .unwrap_or_else(|| cwd.join(".summer").join("snapshots"))
        };
        let snapshots = Arc::new(Snapshots::open(&cwd, &snapshot_store));
        let read_only_gate = Arc::new(AtomicBool::new(
            ExecutionMode::from(mode) == ExecutionMode::Plan,
        ));
        let evaluator = ReadOnlyGate::new(Arc::clone(&read_only_gate), policy::plan_overlay(&cwd)?);
        let broker = BrokerBuilder::new(policy)
            .backend(backend)
            .evaluator(Box::new(evaluator))
            .prompter(prompter)
            .snapshots(Arc::clone(&snapshots) as Arc<dyn engine::SnapshotSink>)
            .decisions(decisions)
            // Never refuse to start over unenforced dimensions when the sandbox
            // is off: a NoneBackend reports every dimension unenforced, and that
            // is the state the default asks for, not a gap to gate on. Only a
            // --sandbox the running kernel cannot fully honor still refuses, and
            // the remedy is to drop --sandbox, not to weaken a requested one.
            .build(!sandbox)
            .map_err(|unenforceable| {
                // Fatal by default. Without this, "network off" is a statement
                // of intent that reads exactly like a fact.
                eprintln!(
                    "summer: cannot enforce the sandbox:\n{unenforceable}run without \
                     {SANDBOX_FLAG} for an unconfined session."
                );
                unenforceable
            })?;

        report_limits(&broker);
        enter_sandbox(&broker, ports)?;
        // After `enter`, and only on the path that reached it: a session that
        // asked for no sandbox, or whose backend enforces nothing, has not been
        // confined and `open_url` should behave as it always did.
        CONFINED.store(sandbox, std::sync::atomic::Ordering::Relaxed);

        Ok(Self {
            backend_name,
            broker,
            cwd,
            resume,
            sessions_dir,
            commands,
            hooks,
            keymap,
            mcp_servers: config.mcp_servers.clone(),
            permissions,
            read_only_gate,
            plain,
            policy_text,
            prompt_rx,
            provider_config,
            provider_id,
            effort,
            context_window,
            mode,
            shell,
            skill_gate,
            skills,
            snapshots,
            statusline,
            notify,
        })
    }
}

/// `--provider`, then the stored preference, then the default this project
/// shipped before providers existed. A flag typo refuses to start; a stale
/// stored preference warns and falls back, the same shape `config::shell`
/// already uses for an unrecognized shell name — a config file is not
/// something the user is actively typing in the moment, so grace is the
/// better trade there.
fn resolve_provider_id(config: &config::Config) -> Result<ProviderId, Box<dyn Error>> {
    if let Some(value) = flag_value(PROVIDER_FLAG) {
        return value
            .parse::<ProviderId>()
            .map_err(|error: UnknownProviderId| -> Box<dyn Error> { error.into() });
    }
    let Some(prefs) = config.provider.as_ref() else {
        return Ok(ProviderId::Anthropic);
    };
    match prefs.id.parse() {
        Ok(id) => Ok(id),
        Err(_) => {
            eprintln!(
                "summer: unknown provider `{}` in config's `provider.id`; using anthropic. \
                 Known providers: {}.",
                prefs.id,
                credentials::known_provider_names()
            );
            Ok(ProviderId::Anthropic)
        }
    }
}

/// The session mode from `--mode`, then the stored `mode` preference, then
/// [`Mode::Default`].
///
/// `to_string` rather than `?`: `main` renders a boxed error with `Debug`, and
/// this one's `Debug` is a tuple struct while its `Display` lists the modes that
/// would have worked.
fn resolve_mode(config: &config::Config) -> Result<Mode, Box<dyn Error>> {
    match flag_value(MODE_FLAG) {
        Some(name) => name
            .parse()
            .map_err(|error: mode::UnknownMode| error.to_string().into()),
        None => Ok(config.mode.unwrap_or_default()),
    }
}

/// `--model`, then the stored preference, then whatever the resolved
/// provider defaults to (decided later, by [`ProviderConfig::for_profile`]).
fn resolve_model(config: &config::Config) -> Option<String> {
    flag_value(MODEL_FLAG).or_else(|| {
        config
            .provider
            .as_ref()
            .and_then(|prefs| prefs.model.clone())
    })
}

/// The stored effort preference, parsed to [`Effort`]. `None` when nothing is
/// configured or the stored string no longer parses -- off is the default, so a
/// bad value is ignored rather than fatal.
fn resolve_effort(config: &config::Config) -> Option<Effort> {
    config
        .provider
        .as_ref()
        .and_then(|prefs| prefs.effort.as_deref())
        .and_then(Effort::parse)
}

/// The cached context-window size for the saved model, captured when it was
/// selected. Used at startup so the statusline can show `X% ctx (size)` for a
/// live Copilot model before its `/models` catalog reloads; the static registry
/// still wins for pinned models.
fn resolve_context_window_pref(config: &config::Config) -> Option<u64> {
    config
        .provider
        .as_ref()
        .and_then(|prefs| prefs.context_window)
}

/// The value following `flag` in the process args, if present. A separate
/// helper from the boolean flags' plain `.any` scan: a value-taking flag and
/// a presence flag are different enough shapes that sharing one would just be
/// an `Option` neither caller needs.
fn flag_value(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next();
        }
    }
    None
}

// ========================================================================
// Functions
// ========================================================================

/// Restrict this thread, and therefore every thread created after it.
///
/// The carve-out is the model's port, and it is needed because an OS sandbox
/// cannot make the distinction the threat model rests on: the provider's socket
/// is harness-driven, but the kernel sees a connect and cannot ask who wanted it.
/// So the backend permits that one port and the broker still denies a *tool*
/// that asks for it. On that port the broker's check stands without a second line
/// behind it, which is a real cost and is why it is written down here rather than
/// buried.
fn enter_sandbox(broker: &Broker, model_ports: Vec<u16>) -> Result<(), Box<dyn Error>> {
    let harness = Harness::new(model_ports);
    let Err(error) = broker.enter(&harness) else {
        return Ok(());
    };
    eprintln!("summer: could not enter the sandbox: {error}");
    eprintln!(
        "summer: the kernel would not enforce the sandbox; pass {NO_SANDBOX_FLAG} for an \
         unconfined session."
    );
    Err(error.into())
}

/// `path` formatted for a human to read: Windows's `\\?\` extended-length
/// prefix stripped wherever stripping it is safe, unchanged on every other OS.
///
/// `cwd` is already normalized this way at the source (see
/// [`Bootstrap::load`]), so this is a second, defensive pass for anything
/// derived from a path this crate did not canonicalize itself -- cheap
/// (`dunce::simplified` borrows rather than re-resolving the filesystem), so
/// paying it twice costs nothing.
fn display_path(path: &Path) -> String {
    dunce::simplified(path).display().to_string()
}

/// Open `url` with the OS's default browser, for `/login`'s device flow.
///
/// Shells out to the OS's own opener rather than pulling in a crate: this only
/// ever launches a browser and (on Linux) reads the opener's exit status to
/// decide whether to fall back to the next one, so there is nothing a
/// dependency would buy beyond the `#[cfg]` branches themselves.
///
/// Returns `Err` when no opener succeeded, so the device flow can print an
/// honest "open the URL yourself" instead of claiming a launch that never
/// happened -- the failure mode of a fire-and-forget `spawn` that exits
/// non-zero once control has already returned.
fn opener_succeeded(program: &str, args: &[&str]) -> bool {
    // `false` covers every reason an opener is unusable here -- the binary is
    // missing, the OS refused the spawn, or the process exited non-zero (e.g.
    // `xdg-open` printing "no default browser") -- so a caller can simply try
    // the next candidate without distinguishing them.
    //
    // Detach the opener from summer's terminal on every handle. Under summer's
    // Landlock sandbox `xdg-open` delegates to `exo-open`/`gio`, which print a
    // flurry of GLib warnings (dconf and GTK-theme files the sandbox denies)
    // that would splatter the raw-mode TUI and look exactly like a failed
    // launch, and an opener that inherited stdin could swallow a keystroke
    // meant for a permission prompt. Null stdio keeps both quiet; the browser
    // itself is started by the desktop session over DBus, outside summer's
    // process tree, so it is not subject to the sandbox the opener is.
    let Ok(mut child) = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    child.wait().map(|status| status.success()).unwrap_or(false)
}

/// Why a confined process does not launch a browser.
///
/// The browser would be a child of this process and would inherit the Landlock
/// domain, so it starts and then fails on the desktop session's own files
/// (`~/.config`, `/run/user/$UID/dconf`) in ways that look like the browser is
/// broken rather than like the sandbox is working. Declining is the honest
/// outcome, and both callers already fall back to showing the URL.
/// Whether this process entered kernel confinement.
///
/// The permission prompt reads it to decide whether saving a rule to
/// `~/.summer/policy.json` is even possible: that path is outside the root the
/// domain confines this process to, so the write fails with `EACCES`. That is
/// the confinement doing its job, and it is the same rule that stops the agent
/// authoring its own policy, so the answer is to stop offering the option
/// rather than to carve out an exception for it.
pub(crate) fn confined() -> bool {
    CONFINED.load(std::sync::atomic::Ordering::Relaxed)
}

fn browser_blocked_by_sandbox() -> Option<std::io::Error> {
    CONFINED
        .load(std::sync::atomic::Ordering::Relaxed)
        .then(|| {
            std::io::Error::other(
                "the sandbox is on, so a browser launched from here would inherit it",
            )
        })
}

#[cfg(windows)]
fn open_url(url: &str) -> std::io::Result<()> {
    if let Some(error) = browser_blocked_by_sandbox() {
        return Err(error);
    }
    // The empty `""` is the window title `start` expects as its first
    // argument; without it, a URL containing spaces is parsed as the title
    // instead of being opened.
    if opener_succeeded("cmd", &["/C", "start", "", url]) {
        Ok(())
    } else {
        Err(std::io::Error::other("could not open a browser"))
    }
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) -> std::io::Result<()> {
    if let Some(error) = browser_blocked_by_sandbox() {
        return Err(error);
    }
    if opener_succeeded("open", &[url]) {
        Ok(())
    } else {
        Err(std::io::Error::other("could not open a browser"))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_url(url: &str) -> std::io::Result<()> {
    if let Some(error) = browser_blocked_by_sandbox() {
        return Err(error);
    }
    // Linux/BSD has no single reliable "open URL" binary. `xdg-open` is the
    // standard, but it is itself a shell script that recurses through its own
    // fallbacks (gio, gnome-open, ...) and exits non-zero when none of them is
    // installed or when no default browser is configured -- a status a
    // fire-and-forget `spawn` cannot see, which is exactly how an "opened in
    // your browser" message gets printed for a launch that never happened.
    //
    // Try `xdg-open` first, then `gio open` (present on GNOME-derived desktops
    // and often the very handler `xdg-open` would have reached), waiting on
    // each only long enough to read its exit status. Both openers launch the
    // browser detached and return, so neither `wait` blocks on the browser
    // window staying open. Direct-browser aliases like `x-www-browser` are
    // deliberately not tried: they `exec` the browser itself, so waiting on
    // one would freeze the login flow until the browser closed.
    if opener_succeeded("xdg-open", &[url]) || opener_succeeded("gio", &["open", url]) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no browser opener succeeded (tried xdg-open, gio open)",
        ))
    }
}

async fn session(
    bootstrap: Bootstrap,
    print: Option<PrintRequest>,
) -> Result<ExitCode, Box<dyn Error>> {
    let Bootstrap {
        backend_name,
        broker,
        commands,
        cwd,
        resume,
        sessions_dir,
        hooks,
        keymap,
        mcp_servers,
        permissions,
        read_only_gate,
        plain,
        policy_text,
        prompt_rx,
        provider_config,
        provider_id,
        effort,
        context_window,
        mode,
        shell,
        skill_gate,
        skills,
        snapshots,
        statusline,
        notify,
    } = bootstrap;

    // Which client speaks to which profile is `provider::transport`'s to know;
    // this decides only what to do without a credential. A session still starts
    // in that case, with a provider that explains itself on the first turn.
    let (provider, provider_name): (Box<dyn Provider>, &str) = match provider_config.clone() {
        Some(config) => (transport(provider_id, config), provider_id.profile().label),
        None => (Box::new(Unconfigured::new()), "unconfigured"),
    };

    // The catalog goes in the system prompt so the model knows what exists; the
    // `skill` tool loads a body from the registry already in memory.
    let mut system_prompt = if skills.is_empty() {
        SYSTEM_PROMPT.to_string()
    } else {
        let languages = discovery::project_languages(&cwd);
        format!("{SYSTEM_PROMPT}\n\n{}", skills.catalog(&languages))
    };
    if let Some((rule_name, rule_content)) = rules::load_project_rules(&cwd) {
        tracing::info!("loaded project rules from {rule_name}");
        system_prompt.push_str(&rule_content);
    }
    if let Some(env_summary) = discovery::detect_environment(&cwd) {
        tracing::info!("detected project environment and recommended tools");
        system_prompt.push_str(&env_summary);
    }
    // A constrained mode has to say so. The policy already refuses, but a model
    // that cannot tell a deliberate mode from a broken workspace keeps trying
    // the same edit, and reports the refusals as the task failing.
    if let Some(note) = mode.prompt_note() {
        system_prompt.push_str(note);
    }
    // Shared with the sub-sessions the `subagent` tool runs: a subagent is the
    // same user doing the same work, so it answers to the same policy and
    // inherits the grants already given rather than re-asking for them.
    let broker = Arc::new(broker);
    let mut builtin = Builtin::with_shell(shell.clone());
    if !skills.is_empty() {
        builtin = builtin.with_skill(Arc::new(skills));
    }
    // Offered only when a sub-session could actually run. Without a credential
    // there is nothing to delegate *to*, and a tool that accepted the delegation
    // and answered anyway is exactly what this one used to be.
    // The sending half goes to the runner, the receiving half to the engine, so
    // what a sub-session spends reaches the counter that guards this one.
    let (spent, delegated) = channel();
    if let Some(config) = provider_config {
        builtin = builtin.with_subagent(Arc::new(subagent::Nested::new(
            Arc::clone(&broker),
            cwd.clone(),
            config,
            provider_id,
            shell,
            spent,
        )));
    }
    // External tools last, so a name a server chose can never be the one the
    // dispatcher keeps: discovery namespaces every remote tool under its
    // server, and `Dispatcher::new` still refuses a duplicate, so the two
    // together make shadowing a built-in unrepresentable rather than a race.
    //
    // After `enter` (the runtime does not exist before it), which is what puts
    // an MCP server inside the confined domain under `--sandbox` instead of
    // beside it.
    let mut tools = builtin.tools();
    tools.extend(mcp::connect(&mcp_servers, &broker, &cwd).await);
    let dispatcher = Dispatcher::new(tools)?;
    let sink = open_journal(&sessions_dir)?;

    // The observer exists only for `--output-format stream-json`. Attaching one
    // unconditionally would make every plain and TUI session pay for a channel
    // whose receiver is dropped, and a dropped receiver turns each emit into a
    // failed send.
    let streaming = print
        .as_ref()
        .is_some_and(|request| request.format == print::OutputFormat::StreamJson);
    let mut ui_events = None;
    let mut builder = EngineBuilder::new(
        cwd.clone(),
        system_prompt,
        provider,
        dispatcher,
        broker,
        sink,
        permissions,
    )
    .limits(Limits::defaults())
    .hooks(hooks)
    .delegated(delegated)
    .skill_gate(skill_gate);
    if streaming {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        ui_events = Some(rx);
        builder = builder.observer(tx);
    }
    // Not for `--print`: a one-shot headless run has no `/resume` picker
    // asking what its session was about.
    if print.is_none() {
        builder = builder.title_sessions();
    }
    // Every real session, headless included: a long `--print` run overflows the
    // window the same way an interactive one does, and the mechanical summary
    // ("dropped 12 turns, 30 tool calls") is where a long task silently loses
    // what it learned. Safe here because nothing on this path replays a
    // recorded journal -- `bench --replay` builds its own engine and leaves
    // this off, which is what keeps positional replay aligned.
    builder = builder.summarize_compactions();
    let mut engine = builder.build()?;
    // Apply the persisted effort before the first turn, so a saved level is in
    // effect from the opening reply (the TUI also mirrors it into its state for
    // display). The plain REPL has no statusline to mirror, but the engine
    // still needs it.
    engine.set_effort(effort);

    // Before the banner: a headless run's stdout is its output, and a greeting
    // on the first line breaks every caller parsing it.
    if let Some(request) = print {
        let mut stdout = io::stdout();
        return print::run(&mut engine, request, ui_events.as_mut(), &mut stdout).await;
    }

    println!(
        "summer-code ready (cwd: {}, sandbox: {backend_name}). Ctrl-D to exit.",
        display_path(&cwd)
    );
    if let Some(banner) = mode.banner() {
        println!("{banner}");
    }
    // An interactive session ends when the user ends it, which is success. Only
    // the headless path has a stop reason worth reporting as a status.
    if plain {
        // `-c` on the plain path: fold here, where the engine is in hand. The
        // TUI path folds inside `tui::run`, which also rebuilds the transcript.
        if let Some(path) = &resume {
            match engine.resume(std::fs::File::open(path)?) {
                Ok(count) => println!("continued {} ({count} events)", path.display()),
                Err(error) => eprintln!("summer: could not continue {}: {error}", path.display()),
            }
        }
        let console: Arc<dyn Console> = Arc::new(console::Stdin);
        let mut stdout = io::stdout();
        // The high-level progress line redraws itself in place with `\r` and
        // an ANSI clear; on a pipe or a captured log that would corrupt the
        // output, so it only runs when stdout is a real terminal (true for
        // `--plain` used interactively, false when `plain` was forced by a
        // non-terminal stdout).
        let live = stdout.is_terminal();
        repl::run(
            &mut engine,
            console.as_ref(),
            &commands,
            &sessions_dir,
            live,
            notify,
            &mut stdout,
        )
        .await?;
    } else {
        // The engine moves into the TUI; it owns the terminal for the session.
        // The observer is attached per-turn inside `tui::run`, so nothing about
        // the plain loop changes when this branch is taken instead.
        tui::run(
            engine,
            tui::Session {
                backend_name,
                commands,
                context_window,
                resume,
                sessions_dir,
                effort,
                keymap,
                mode,
                read_only_gate,
                policy: policy_text,
                prompt_rx: prompt_rx.expect("the TUI path owns the prompt receiver"),
                provider: provider_name.to_string(),
                provider_id,
                snapshots,
                statusline,
                notify,
            },
        )
        .await?;
    }
    Ok(ExitCode::Ok)
}

/// The notice for a session started with [`NO_SANDBOX_FLAG`].
///
/// It names both halves of what was given up, because the second is the one
/// nobody expects: without confinement there is no kernel backstop *and* every
/// unlisted command goes back to prompting, since `Broker::confined_allow` has
/// nothing to rest on.
/// Say where the OS backs the policy less completely than the policy expresses.
///
/// **Printed on a successful start, not only on a refusal.** A limit is not a
/// gap: the dimension is enforced, just by the broker alone rather than by the
/// broker and the kernel. That is a real reduction in depth, and the only moment
/// anyone would notice it is now -- so it is said once, plainly, rather than left
/// to a reader of the source to work out from the policy and the kernel version.
fn report_limits(broker: &engine::Broker) {
    let limits = broker.capabilities().limits();
    if limits.is_empty() {
        return;
    }
    eprintln!(
        "summer: the `{}` backend enforces part of this policy on its own:",
        broker.backend_name()
    );
    for limit in limits {
        eprintln!("  - {}: {}", limit.dimension, limit.reason);
    }
}

/// The newest non-empty journal in a sessions directory, by modified time:
/// what `-c` continues. Empty files are skipped — the just-created journal of
/// a starting session is empty, and "continuing" it would fold nothing.
fn newest_journal(dir: &std::path::Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                && std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0)
        })
        .max_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
}

/// Open an append-only journal file in this workspace's session store — the
/// caller decides which (`Bootstrap::load`: central under
/// `~/.summer/projects/<encoded-root>/sessions` so nothing session-generated
/// lands inside the repo, or the in-project `.summer/sessions` when confined
/// or home-less).
fn open_journal(dir: &std::path::Path) -> Result<Box<dyn JournalSink>, Box<dyn Error>> {
    std::fs::create_dir_all(dir)?;
    let session_id = journal::SessionId::new();
    let path: PathBuf = dir.join(format!("{session_id}.jsonl"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    eprintln!("[journal: {}]", display_path(&path));
    Ok(Box::new(WriterSink::new(file)))
}

/// Move a legacy in-project store (`<cwd>/.summer/{sessions,snapshots}`) into
/// the central per-project store, once: only when the legacy directory exists
/// and the central one does not, so a migrated machine never re-copies and a
/// fresh clone never creates anything. `rename` first (the common same-filesystem
/// case); across filesystems it falls back to a recursive copy and leaves the
/// original in place for the user to delete — losing history silently is worse
/// than a stale directory. Best-effort throughout: a store that cannot move
/// still works where it is, and the outcome is said out loud either way.
fn migrate_local_store(cwd: &std::path::Path) {
    let Some(central) = policy::project_store_dir(cwd) else {
        return;
    };
    for name in ["sessions", "snapshots"] {
        let legacy = cwd.join(".summer").join(name);
        let target = central.join(name);
        if !legacy.is_dir() || target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&legacy, &target) {
            Ok(()) => {
                eprintln!("summer: moved .summer/{name} to {}", target.display());
            }
            Err(rename_error) => match copy_dir_recursive(&legacy, &target) {
                Ok(()) => eprintln!(
                    "summer: copied .summer/{name} to {} (the original is left in place; \
                     delete it once the copy checks out: {rename_error})",
                    target.display()
                ),
                Err(copy_error) => {
                    eprintln!(
                        "summer: could not move .summer/{name} to {} ({rename_error}, then \
                         {copy_error}); it stays where it is and keeps working",
                        target.display()
                    );
                }
            },
        }
    }
}

/// Recursive directory copy, for the cross-filesystem migration fallback.
/// Files and directories only (no symlink chasing: a store holds plain files).
fn copy_dir_recursive(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod project_rule_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_claude_md_or_agents_md_rules() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("CLAUDE.md");
        std::fs::write(&file_path, "Rule: Always test before commit").unwrap();

        let loaded = rules::load_project_rules(dir.path());
        assert!(loaded.is_some());
        let (name, content) = loaded.unwrap();
        assert!(name.contains("CLAUDE.md"));
        assert!(content.contains("<user_rules>"));
        assert!(content.contains("Rule: Always test before commit"));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn display_path_strips_the_extended_length_prefix() {
        let path = Path::new(r"\\?\D:\Workspace\summer-code");
        assert_eq!(display_path(path), r"D:\Workspace\summer-code");
    }

    #[test]
    fn display_path_leaves_a_verbatim_unc_path_as_is() {
        // `dunce::simplified` only strips the prefix from a verbatim *disk*
        // path (`\\?\C:\...`); a verbatim UNC path keeps it, since collapsing
        // `\\?\UNC\server\share` to `\\server\share` can change which share
        // legacy path resolution reaches.
        let path = Path::new(r"\\?\UNC\server\share\dir");
        assert_eq!(display_path(path), r"\\?\UNC\server\share\dir");
    }

    #[test]
    fn display_path_leaves_an_unprefixed_path_alone() {
        let path = Path::new(r"D:\Workspace\summer-code");
        assert_eq!(display_path(path), r"D:\Workspace\summer-code");
    }
}
