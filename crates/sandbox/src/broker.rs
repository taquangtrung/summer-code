//! The broker: the single source of authorized handles.
//!
//! Every agent-driven fs, process and network call passes through here, and the
//! typestate is what makes that structural rather than a convention: a tool
//! cannot open a file because it has no way to construct a handle. It must ask
//! the broker, and the broker asks the evaluator.
//!
//! **Every decision is journaled, including the ones nobody was asked about.**
//! The broker sends a [`PermissionRecord`] on every evaluation: allows, denies,
//! prompted answers, and auto-allows from remembered rules alike. That is what
//! makes "no write happens without a matching permission grant in the journal"
//! true by construction rather than by discipline. The failure it prevents is
//! specific: approvals are remembered per session, so the second write to a path
//! the user already approved never reaches a prompt, and if the *prompt* were
//! what emitted the record, that write would execute unaudited and the monitor
//! would flag a violation against a system behaving correctly. The record follows
//! the decision, not the prompt.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use policy::{
    Decision, ListenRule, Policy, PolicyEvaluator, Request, RulesEvaluator, Scope,
    sole_unlisted_command,
};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

use crate::backend::{Capabilities, Enforcement, Gap, Harness, NoneBackend, SandboxBackend};
use crate::callsite::current_call;
use crate::canon::canonicalize;
use crate::handle::{
    AuthorizedAddrs, AuthorizedCommand, AuthorizedDir, AuthorizedListener, AuthorizedRead,
    AuthorizedService, AuthorizedSocket, AuthorizedWrite,
};
use crate::prompt::{AutoDeny, Grant, Prompter, WritePreview};
use crate::remembered::{Remembered, Remembrance};
use crate::snapshot::{NoSnapshots, SnapshotSink};

// ========================================================================
// Constants
// ========================================================================

/// The environment variables an authorized child inherits, everywhere.
///
/// **An allowlist, because the interesting variables are the ones nobody thinks
/// to name.** The harness holds `ANTHROPIC_API_KEY`, and a denylist would have to
/// anticipate that plus every `*_TOKEN`, `*_SECRET`, cloud credential and CI
/// variable the operator's shell happens to carry. Inverting it means a new
/// secret in the environment is excluded by default and a genuinely needed
/// variable is a reviewable line here.
///
/// These four earn their place from the built-in command allow list on every
/// platform: `PATH` is what resolves a program at all, `CARGO_HOME` and
/// `RUSTUP_HOME` cover a non-default toolchain install, and `TZ` keeps
/// timestamps matching what the user sees. The temp directory is *not* here --
/// it is set rather than inherited, see [`SCRATCH_DIR`].
///
/// `TERM` is deliberately absent on every platform: the child's stdout is a
/// pipe, and advertising a terminal only invites colour escapes into the model's
/// context.
const CHILD_ENV_COMMON: &[&str] = &["CARGO_HOME", "PATH", "RUSTUP_HOME", "TZ"];

/// Unix: the home directory, the locale, and the user's identity.
///
/// `HOME` is where `git` finds `.gitconfig` and `cargo` its registry, so without
/// it both fall back to surprising defaults; the `LC_*` pair keeps collation and
/// message language matching the user's shell.
#[cfg(unix)]
const CHILD_ENV_PLATFORM: &[&str] = &[
    "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "SHELL", "USER",
];

/// Windows: what a process genuinely cannot start without.
///
/// **This list is longer than the Unix one because Windows is less forgiving.**
/// `SystemRoot` and `windir` are needed to load `ws2_32.dll` and friends, so a
/// child without them fails to initialize sockets before `main` runs;
/// `ComSpec` is how `cmd.exe` is found; `PATHEXT` is what makes `git` resolve to
/// `git.exe`; and `USERPROFILE`/`APPDATA` are where `git` and `cargo` keep their
/// configuration. Clearing the environment without restoring these does not
/// produce a hardened child, it produces one that cannot run at all.
///
/// The user's home lives across `HOMEDRIVE`+`HOMEPATH` as well as `USERPROFILE`
/// because tools disagree about which they read.
#[cfg(windows)]
const CHILD_ENV_PLATFORM: &[&str] = &[
    "APPDATA",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PUBLIC",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "USERNAME",
    "USERPROFILE",
    "WINDIR",
];

/// Scratch space for children, relative to the policy root.
///
/// **`TMPDIR` is set rather than passed through, so that writes stay inside the
/// root.** Compilers, test harnesses and `git` all need somewhere to stage work,
/// and the tempting grant is `/tmp` -- but a sandbox that permits writes to
/// `/tmp` no longer confines the agent to the repository, and the OS layer stops
/// being able to say so. Pointing children at a directory under the root keeps
/// the one-sentence property ("the agent may only write inside the root") true
/// with no exception attached, and needs no rule beyond the one the root already
/// has.
const SCRATCH_DIR: &str = ".summer/tmp";

/// How long a [`SCRATCH_DIR`] entry may sit before a later command treats it
/// as abandoned and removes it.
///
/// Generous, because a long-running spawned process (a dev server the agent
/// started, say) may still be reading or writing its own temp file when
/// pruning next runs elsewhere in the same root. Still finite, because unlike
/// `/tmp` -- which the OS clears on its own -- nothing else ever revisits this
/// directory: a project used daily would otherwise accumulate every session's
/// scratch data forever.
const SCRATCH_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// The largest file, before or after, a write prompt shows a diff for. Past
/// this a diff is not something a person reads before answering, and building
/// one would hold the prompt on a read of however large the file has grown.
const MAX_PREVIEW_BYTES: u64 = 1024 * 1024;

/// The variables that name the temp directory, per platform.
///
/// Windows reads `TEMP` and `TMP` and different tools prefer different ones, so
/// both are set; leaving one pointing at the user's real temp directory would
/// undo the confinement for whichever tool happened to read it.
#[cfg(windows)]
const TEMP_VARS: &[&str] = &["TEMP", "TMP"];

#[cfg(not(windows))]
const TEMP_VARS: &[&str] = &["TMPDIR"];

// ========================================================================
// Data Structures
// ========================================================================

/// The single source of authorized handles.
pub struct Broker {
    backend: Box<dyn SandboxBackend>,
    /// Every decision, to the engine. `std::sync::mpsc` is enough, and the
    /// choice is load-bearing rather than incidental: `Sender<T>` has been
    /// `Send + Sync` since Rust 1.72, and `ToolCx` puts an `Arc<Broker>` into a
    /// spawned task. `tokio::spawn` demands `Send`, `Arc<Broker>` is `Send` only
    /// if `Broker` is `Sync`, and that holds only if every field is. A `Sender`
    /// that was `Send` but not `Sync` would reject the spawn. The receiver never
    /// leaves the engine, which drains it between dispatches, so nothing here
    /// wants an async channel.
    decisions: Option<Sender<PermissionRecord>>,
    evaluator: Box<dyn PolicyEvaluator>,
    policy: Policy,
    /// Resolves `Decision::Ask`. No prompter, no `Ask`.
    prompter: Box<dyn Prompter>,
    /// Records a file's prior contents before each authorized write.
    ///
    /// `Arc` rather than `Box` so the frontend can hold the same store it gave
    /// the broker: `/rewind` reads the manifest the broker's writes fill, and
    /// two stores pointed at one directory would be two handles on one append-only
    /// file.
    snapshots: Arc<dyn SnapshotSink>,
    /// A derived cache, not a second store: it folds from the journal's
    /// `Permission` records and their scopes. The `Mutex` is interior mutability
    /// for that cache, not shared ownership of a decision, which is what keeps
    /// `PolicyEvaluator::evaluate` a pure `&self` function.
    remembered: Mutex<Remembered>,
    /// A descriptor on the policy root, held open for the session.
    ///
    /// **The descriptor is the root, and that is the point.** Every in-root open
    /// resolves against *this* handle rather than against the root's name, so a
    /// rename of the root directory mid-session cannot redirect anything, and
    /// `openat2`'s `RESOLVE_BENEATH` has a fixed anchor to be beneath. `None`
    /// when the root cannot be opened (a policy pointing at a path that does not
    /// exist, which the tests do), and the check-then-open path still applies.
    #[cfg(target_os = "linux")]
    root_dir: Option<File>,
}

/// What was asked and what was decided, in `policy`'s vocabulary.
///
/// **`call` is a tag, not a `CallId`.** The broker still does not depend on
/// `journal` for the id type: it reads whatever opaque string the dispatcher
/// scoped around the tool task (see the crate-private `callsite` module) and
/// the engine parses it back
/// into a `CallId`. That keeps the correlation field free of a crate dependency
/// while letting several calls be in flight at once, which is what concurrent
/// dispatch needs and what serial dispatch let the broker do without.
#[derive(Debug, Clone)]
pub struct PermissionRecord {
    /// Which tool call made the request, when one was in scope.
    pub call: Option<Arc<str>>,
    pub decision: Decision,
    pub request: Request,
    /// What the user actually agreed to, not just what was asked.
    pub scope: Scope,
    /// How the decision was reached.
    pub source: DecisionSource,
    /// What the broker itself contributed to the access.
    pub supplied: Supplied,
}

/// The part of an access the request does not contain, because the broker chose
/// it rather than the tool.
///
/// **Two dimensions need this and they need different shapes.** `Listen` carries
/// a port and no address, because the address is a function of the policy
/// (`Loopback` binds `127.0.0.1`, `Any` binds `0.0.0.0`); `Connect` carries a
/// host and no addresses, because resolving the name is the broker's job now.
/// Both are facts the audit trail needs and neither can be read back off the
/// request, so they travel on the record instead of being reconstructed by
/// whoever reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supplied {
    /// The address a `Listen` was bound to.
    Bound(IpAddr),
    /// The access had nothing the broker chose: every path and exec request,
    /// and a `Connect` the policy refused before any lookup happened.
    Nothing,
    /// Every address a `Connect`'s host resolved to.
    Resolved(Vec<IpAddr>),
}

/// Where a decision came from.
///
/// The journal needs this to tell `Allowed` from `AskedAllowed` from
/// `RememberedAllow`, and those are three different facts: the policy said yes,
/// the user said yes to this, and the user said yes to something like this
/// earlier. Only the middle one reached a prompt, and a reader who cannot tell
/// them apart cannot audit what the user actually agreed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// The kernel is confining this process to the policy root, so the access
    /// was allowed without a prompt.
    ///
    /// **A fourth source, not a flavour of `Policy`, because it is a different
    /// claim.** `Policy` means a rule said yes. This means no rule said
    /// anything, and the answer was yes anyway because the OS makes the blast
    /// radius the workspace regardless of what the command does. An auditor
    /// reading the journal must be able to tell those apart: the second is only
    /// as good as the confinement was, and if the confinement was ever reported
    /// wrongly, these are exactly the records to re-examine.
    Confined,
    /// The evaluator answered outright.
    Policy,
    /// A human (or a daemon's client) was asked.
    Prompt,
    /// A rule the user agreed to earlier covered it, with no prompt.
    Remembered,
}

/// What an authorized open intends to do with the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    Write,
}

/// A policy denial, with the reason the evaluator gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub reason: String,
}

/// Why the broker has no file handle to give back.
///
/// The two halves stay apart because the model must be able to tell "you may not
/// read that" from "that file is not there", and the journal records the first as
/// `Outcome::Denied` and the second as `Outcome::Err`. Folding a missing file
/// into `Denied` would put a filesystem error in the permission audit trail.
#[derive(Debug)]
pub enum OpenError {
    Denied(Denied),
    Io(io::Error),
}

/// Why the broker refused to start.
#[derive(Debug)]
pub struct Unenforceable {
    pub backend: &'static str,
    pub gaps: Vec<Gap>,
}

/// Assembles a broker. Every seam is injected.
pub struct BrokerBuilder {
    backend: Box<dyn SandboxBackend>,
    decisions: Option<Sender<PermissionRecord>>,
    evaluator: Box<dyn PolicyEvaluator>,
    policy: Policy,
    prompter: Box<dyn Prompter>,
    snapshots: Arc<dyn SnapshotSink>,
}

/// The Phase 1a evaluator: allows everything, checks nothing. Reachable only
/// through [`Broker::permissive`], and gated with it.
#[cfg(feature = "testing")]
struct AllowAll;

// ========================================================================
// BrokerBuilder Implementations
// ========================================================================

impl BrokerBuilder {
    /// A builder over `policy`, defaulting to the rules evaluator, the `none`
    /// backend, and an auto-deny prompter. Those defaults are the test shape;
    /// the CLI overrides all three.
    pub fn new(policy: Policy) -> Self {
        Self {
            backend: Box::new(NoneBackend::new("no backend was selected")),
            decisions: None,
            evaluator: Box::new(RulesEvaluator::new()),
            policy,
            prompter: Box::new(AutoDeny),
            snapshots: Arc::new(NoSnapshots),
        }
    }

    pub fn backend(mut self, backend: Box<dyn SandboxBackend>) -> Self {
        self.backend = backend;
        self
    }

    pub fn evaluator(mut self, evaluator: Box<dyn PolicyEvaluator>) -> Self {
        self.evaluator = evaluator;
        self
    }

    pub fn prompter(mut self, prompter: Box<dyn Prompter>) -> Self {
        self.prompter = prompter;
        self
    }

    /// Where a file's prior contents are recorded before it is overwritten.
    /// Without it nothing is captured and `/rewind` has nothing to restore.
    pub fn snapshots(mut self, snapshots: Arc<dyn SnapshotSink>) -> Self {
        self.snapshots = snapshots;
        self
    }

    /// Where every decision goes. Without it the broker still evaluates, but
    /// nothing is journaled, so the engine always sets this.
    pub fn decisions(mut self, decisions: Sender<PermissionRecord>) -> Self {
        self.decisions = Some(decisions);
        self
    }

    /// Build, refusing when the backend cannot enforce what the policy demands.
    ///
    /// **Fatal by default.** A policy that denies `Connect` on a backend
    /// reporting `connect: Unenforced` refuses to start, because the alternative
    /// is that "network off" is a statement of intent that reads exactly like a
    /// fact. `allow_unenforced` downgrades it to a decision the caller made out
    /// loud, which is what the `none` backend and Windows need.
    pub fn build(self, allow_unenforced: bool) -> Result<Broker, Unenforceable> {
        let gaps = self.backend.capabilities(&self.policy).gaps();
        if !gaps.is_empty() && !allow_unenforced {
            return Err(Unenforceable {
                backend: self.backend.name(),
                gaps,
            });
        }
        #[cfg(target_os = "linux")]
        let root_dir = open_root_dir(&self.policy.root);
        Ok(Broker {
            backend: self.backend,
            decisions: self.decisions,
            evaluator: self.evaluator,
            policy: self.policy,
            prompter: self.prompter,
            snapshots: self.snapshots,
            remembered: Mutex::new(Remembered::new()),
            #[cfg(target_os = "linux")]
            root_dir,
        })
    }
}

// ========================================================================
// Broker Implementations
// ========================================================================

impl Broker {
    /// A broker that allows everything, records nothing, and gates on no
    /// capabilities: the Phase 1a shape, kept for tests and the offline demos.
    ///
    /// **This is a bypass of the typestate, so it is behind the `testing`
    /// feature.** Every other route to a handle runs the evaluator and sends a
    /// [`PermissionRecord`]; this one does neither, which means any crate that
    /// could call it could read any file with nothing in the journal to show for
    /// it. `tools` and `query` are given no exemption from the
    /// enforcement rule precisely so they cannot do that, and a `pub fn` here
    /// handed it back to them. Callers ask for it in `dev-dependencies`, so the
    /// admission is reviewable and cannot reach a release build.
    #[cfg(feature = "testing")]
    pub fn permissive(root: PathBuf) -> Self {
        #[cfg(target_os = "linux")]
        let root_dir = open_root_dir(&root);
        Broker {
            backend: Box::new(NoneBackend::new("permissive broker")),
            decisions: None,
            evaluator: Box::new(AllowAll),
            policy: Policy::builtin(root),
            prompter: Box::new(AutoDeny),
            snapshots: Arc::new(NoSnapshots),
            remembered: Mutex::new(Remembered::new()),
            #[cfg(target_os = "linux")]
            root_dir,
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// What the backend can enforce on this kernel. The frontend reports the
    /// gaps a session without `--sandbox` accepts by not asking for enforcement.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities(&self.policy)
    }

    /// Restrict the calling worker to the policy. See [`SandboxBackend::enter`].
    ///
    /// `harness` is the egress the harness itself needs and the policy does not
    /// govern: the model connection. An OS sandbox cannot tell it from a tool's
    /// socket, so it has to be named. See [`Harness`].
    pub fn enter(&self, harness: &Harness) -> io::Result<()> {
        self.backend.enter(&self.policy, harness)
    }

    /// Seed the grant set from the journal on resume.
    ///
    /// Sync, because it touches only the in-memory cache. Without this the fold
    /// is a lie: a resumed session re-asks what the user already answered.
    pub fn restore(&self, grants: Vec<Grant>) {
        if let Ok(mut remembered) = self.remembered.lock() {
            remembered.restore(grants);
        }
    }

    /// Authorize and open `path` for reading.
    ///
    /// **The broker opens the file, and that is the authorization.** Handing back
    /// the path instead would leave the open to the tool, at a moment of the
    /// tool's choosing, re-resolving a name whose meaning the agent has had time
    /// to change: `cargo` is on the built-in allow list and runs arbitrary
    /// `build.rs`, so a symlink dropped over an approved path between the check
    /// and the open redirects the read to a file the policy never saw.
    pub async fn read(&self, path: &Path) -> Result<AuthorizedRead, OpenError> {
        let path = self.resolve(path).map_err(OpenError::Denied)?;
        self.authorize(Request::Read(path.clone()), Supplied::Nothing)
            .await
            .map_err(OpenError::Denied)?;
        let file = self.open_authorized(&path, OpenMode::Read)?;
        Ok(AuthorizedRead::new(file))
    }

    /// Authorize and enumerate `path`'s entries.
    ///
    /// The broker reads the directory, which is the authorization, the same way
    /// it opens a file for [`Broker::read`]: handing back the path would leave
    /// the enumeration to the tool, at a moment of the tool's choosing, on a
    /// name the agent has had time to swap. std offers no `openat2`-beneath
    /// equivalent for directories, so this resolves and reads directly, matching
    /// the non-Linux file-read fallback rather than the kernel-pinned path. It
    /// is still the read the policy saw, which is the property the tools crate
    /// was missing when it reached for `std::fs` and bypassed evaluation.
    pub async fn read_dir(&self, path: &Path) -> Result<AuthorizedDir, OpenError> {
        let path = self.resolve(path).map_err(OpenError::Denied)?;
        self.authorize(Request::Read(path.clone()), Supplied::Nothing)
            .await
            .map_err(OpenError::Denied)?;
        let entries = std::fs::read_dir(&path).map_err(OpenError::Io)?;
        Ok(AuthorizedDir::new(entries))
    }

    /// Authorize and open `path` for writing, creating it if it does not exist.
    ///
    /// Opened, not truncated: [`AuthorizedWrite::write`] truncates when it has
    /// content to put there, so an authorized write that is never performed
    /// leaves the file as it found it. The open is the broker's for the same
    /// reason as [`Broker::read`].
    pub async fn write(&self, path: &Path) -> Result<AuthorizedWrite, OpenError> {
        self.open_write(path, None).await
    }

    /// [`Broker::write`], for a caller that already knows what it will write.
    ///
    /// `after` is shown to the person answering, as a diff against the file's
    /// current content, if the write reaches a prompt. It decides nothing: the
    /// request evaluated is the same `Write` of the same path either way.
    pub async fn write_previewed(
        &self,
        path: &Path,
        after: &str,
    ) -> Result<AuthorizedWrite, OpenError> {
        self.open_write(path, Some(after)).await
    }

    /// Authorize and open `path` for writing, with `after` previewed at a prompt.
    async fn open_write(
        &self,
        path: &Path,
        after: Option<&str>,
    ) -> Result<AuthorizedWrite, OpenError> {
        let path = self.resolve(path).map_err(OpenError::Denied)?;
        self.authorize_previewed(Request::Write(path.clone()), Supplied::Nothing, after)
            .await
            .map_err(OpenError::Denied)?;
        // Between the decision and the open: the file still holds what it held
        // before the agent touched it, and the write has already been approved,
        // so nothing is captured for a write that will not happen.
        self.snapshots.capture(&path);
        let file = self.open_authorized(&path, OpenMode::Write)?;
        Ok(AuthorizedWrite::new(file))
    }

    /// Authorize and spawn `program`.
    ///
    /// The broker spawns, not the tool, so the authorized program is what runs.
    /// A permit the tool redeems itself is not an authorization, because the
    /// tool can redeem it for something else.
    ///
    /// **The broker also decides the child's stdio, environment and working
    /// directory**, for the same reason it decides the program. Each was
    /// previously left at the inherit default, and each was a hole:
    ///
    /// - Inherited *stdout* sent every command's output to the operator's
    ///   raw-mode terminal and handed the model an empty string, so the agent
    ///   ran `cargo test` and concluded it had produced nothing. Inherited
    ///   *stdin* let a child consume the keystrokes answering a permission
    ///   prompt, and inherited stdout let it forge one with ANSI.
    /// - An inherited *environment* handed `ANTHROPIC_API_KEY` to every approved
    ///   `sh`, `git` and `cargo`, so exfiltrating the credential took one
    ///   allowed command.
    /// - An inherited *working directory* is whatever the process last had,
    ///   which need not be the root the policy is written against.
    pub async fn exec(&self, program: &str, args: &[String]) -> Result<AuthorizedCommand, Denied> {
        self.authorize(
            Request::Exec {
                args: args.to_vec(),
                program: program.to_string(),
            },
            Supplied::Nothing,
        )
        .await?;

        let mut command = Command::new(program);
        command.args(args);
        command.current_dir(&self.policy.root);
        // Piped, so the output reaches the model; null stdin, so an interactive
        // child fails immediately instead of blocking until the dispatcher's
        // timeout and never touches the user's keyboard.
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        apply_environment(&mut command, &self.policy.root);
        // On timeout the dispatcher aborts the tool task; dropping the child
        // then kills the process rather than orphaning it.
        command.kill_on_drop(true);
        self.backend.wrap(&mut command);
        let child: Child = command
            .spawn()
            .map_err(|source| Denied::new(format!("{program}: {source}")))?;
        Ok(AuthorizedCommand::new(child))
    }

    /// Authorize and start a long-lived service the caller speaks to over
    /// stdio.
    ///
    /// **The same authorization as [`exec`](Self::exec)**, because it is the
    /// same act: a program of the caller's choosing runs with this process's
    /// privileges. It goes through `Request::Exec`, so the policy decides, the
    /// user is prompted when the policy says to ask, and the decision is
    /// journaled. Under kernel confinement it is auto-allowed for the same
    /// reason any other exec is, and inherits the same domain.
    ///
    /// That inheritance is the practical caveat and it is not small: a language
    /// server confined to the workspace cannot reach `~/.cargo`, `~/.rustup`, or
    /// its own cache directories, and several will degrade or refuse to start.
    /// Making them work needs the same kind of carve-out `Harness` already
    /// provides for the model connection, which is a policy decision rather than
    /// something this method may take on its own.
    pub async fn serve(&self, program: &str, args: &[String]) -> Result<AuthorizedService, Denied> {
        self.authorize(
            Request::Exec {
                args: args.to_vec(),
                program: program.to_string(),
            },
            Supplied::Nothing,
        )
        .await?;

        let mut command = Command::new(program);
        command.args(args);
        command.current_dir(&self.policy.root);
        // Piped, unlike `exec`'s null stdin: talking to it is the point.
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        // Inherited rather than piped: a server's diagnostics are the operator's
        // business, and a piped stderr nobody drains fills its buffer and wedges
        // the child once it has logged enough.
        command.stderr(Stdio::null());
        apply_environment(&mut command, &self.policy.root);
        command.kill_on_drop(true);
        self.backend.wrap(&mut command);
        let child = command
            .spawn()
            .map_err(|source| Denied::new(format!("{program}: {source}")))?;
        AuthorizedService::new(child)
            .ok_or_else(|| Denied::new(format!("{program}: stdio was not piped")))
    }

    /// Authorize an outbound connection.
    pub async fn connect(&self, host: &str, port: u16) -> Result<AuthorizedSocket, Denied> {
        let authorized = self.lookup(host, port).await?;
        let stream = tokio::net::TcpStream::connect(authorized.addrs())
            .await
            .map_err(|source| Denied::new(format!("{host}:{port}: {source}")))?;
        Ok(AuthorizedSocket::new(stream))
    }

    /// Authorize an outbound connection **and resolve it**, without connecting.
    ///
    /// [`Broker::connect`] is the whole story for a tool that speaks its own
    /// protocol over a socket. `web_fetch` cannot be one: HTTPS over an
    /// [`AuthorizedSocket`] means owning the TLS handshake and the HTTP framing,
    /// so the transport belongs to `reqwest` and the connection is made by the
    /// client rather than here.
    ///
    /// That left DNS outside the check. The host was authorized here and looked
    /// up again by the client, and two lookups of one name can answer
    /// differently -- so the address bytes actually moved to was one nobody
    /// evaluated. This method is the fix: the lookup happens *once*, in the
    /// broker, after the policy said yes, and the client is built over a
    /// resolver that can only replay what comes back in [`AuthorizedAddrs`].
    ///
    /// An empty answer is a denial rather than an empty list, because a caller
    /// that has to check for one will eventually forget to.
    ///
    /// **The lookup happens after the decision, never before it.** A DNS query
    /// is itself an outbound message, and a hostname is a fine place to hide
    /// bytes in, so resolving a host the policy refuses would hand the agent an
    /// exfiltration channel the denial was supposed to close. The cost of that
    /// order is that the record cannot be sent until the resolution is in, which
    /// is why this method records for itself rather than going through
    /// `authorize`: the addresses are part of what is being recorded.
    pub async fn lookup(&self, host: &str, port: u16) -> Result<AuthorizedAddrs, Denied> {
        let request = Request::Connect {
            host: host.to_string(),
            port,
        };
        let (decision, scope, source) = self.decide(&request, None).await;
        let resolved = match decision {
            Decision::Allow => resolve_host(host, port).await,
            Decision::Ask | Decision::Deny { .. } => Ok(Vec::new()),
        };
        let supplied = match resolved.as_deref() {
            Ok([]) | Err(_) => Supplied::Nothing,
            Ok(addrs) => Supplied::Resolved(addrs.iter().map(SocketAddr::ip).collect()),
        };
        self.record(&request, &decision, &scope, source, supplied);

        verdict(decision)?;
        Ok(AuthorizedAddrs::new(resolved?, host.to_string(), port))
    }

    /// Authorize an inbound listener, **and bind it**.
    ///
    /// [`Request::Listen`] carries a port and no address, which is only coherent
    /// because the bind is not the tool's to make. The broker consults
    /// [`ListenRule`], binds `127.0.0.1` under `Loopback` and `0.0.0.0` only
    /// under `Any`, and hands back an already-bound listener. If the tool held a
    /// permit and did its own bind, nothing would stop it binding `0.0.0.0`
    /// under a `Loopback` policy, and the distinction would be decoration.
    pub async fn listen(&self, port: u16) -> Result<AuthorizedListener, Denied> {
        // The address is chosen here, from the policy, before the request is
        // recorded: it is what the broker will bind, and the audit trail needs
        // it whether or not the bind then succeeds.
        let addr = match self.policy.network.listen {
            ListenRule::Any { .. } => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            // `Deny` never reaches the bind: `authorize` refuses it first.
            ListenRule::Deny | ListenRule::Loopback { .. } => IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        self.authorize(Request::Listen { port }, Supplied::Bound(addr))
            .await?;

        let listener = TcpListener::bind(SocketAddr::new(addr, port))
            .await
            .map_err(|source| Denied::new(format!("bind {addr}:{port}: {source}")))?;
        let bound = listener
            .local_addr()
            .map_err(|source| Denied::new(format!("local_addr: {source}")))?;
        Ok(AuthorizedListener::new(listener, bound))
    }

    /// Canonicalize before evaluating. This is where symlink escapes die.
    fn resolve(&self, path: &Path) -> Result<PathBuf, Denied> {
        canonicalize(path).map_err(|source| Denied::new(format!("{}: {source}", path.display())))
    }

    /// Open an already-authorized path, beneath the root descriptor where that
    /// is possible.
    ///
    /// **The `openat2` path is the one that is actually safe**; the other is the
    /// fallback that every platform started with. See [`crate::openat`] for what
    /// the difference buys. Three cases route to the fallback, and each is a
    /// deliberate decision rather than an oversight:
    ///
    /// - **The path is outside the root.** A user policy may allow reading, say,
    ///   `/etc/hosts`, and the evaluator has already said yes. `RESOLVE_BENEATH`
    ///   exists to refuse exactly that, so it cannot be the mechanism here; the
    ///   grant was explicit and outside the root by construction.
    /// - **No root descriptor**, because the root could not be opened.
    /// - **The kernel predates `openat2`** (before 5.6), reported as
    ///   `Unsupported`. A *refusal* is never mistaken for this: it propagates.
    #[cfg(target_os = "linux")]
    fn open_authorized(&self, path: &Path, mode: OpenMode) -> Result<File, OpenError> {
        if let Some(root_dir) = &self.root_dir
            && let Ok(relative) = path.strip_prefix(&self.policy.root)
            && !relative.as_os_str().is_empty()
        {
            match crate::openat::open_beneath(root_dir, relative, mode) {
                Ok(file) => return regular_file(file, path),
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
                Err(error) => return Err(OpenError::Io(error)),
            }
        }
        open_regular(&mut options_for(mode), path)
    }

    /// The check-then-open path, on platforms with no `openat2`.
    #[cfg(not(target_os = "linux"))]
    fn open_authorized(&self, path: &Path, mode: OpenMode) -> Result<File, OpenError> {
        open_regular(&mut options_for(mode), path)
    }

    /// Evaluate, resolve `Ask`, remember, and record. Every path through here
    /// emits exactly one [`PermissionRecord`].
    async fn authorize(&self, request: Request, supplied: Supplied) -> Result<(), Denied> {
        self.authorize_previewed(request, supplied, None).await
    }

    /// [`Broker::authorize`], with the content a write would leave, for a prompt.
    async fn authorize_previewed(
        &self,
        request: Request,
        supplied: Supplied,
        after: Option<&str>,
    ) -> Result<(), Denied> {
        let (decision, scope, source) = self.decide(&request, after).await;
        self.record(&request, &decision, &scope, source, supplied);
        verdict(decision)
    }

    /// The decision, the scope it was answered at, and where it came from.
    async fn decide(
        &self,
        request: &Request,
        after: Option<&str>,
    ) -> (Decision, Scope, DecisionSource) {
        match self.evaluator.evaluate(&self.policy, request) {
            Decision::Allow => (Decision::Allow, Scope::Once, DecisionSource::Policy),
            Decision::Deny { reason } => (
                Decision::Deny { reason },
                Scope::Once,
                DecisionSource::Policy,
            ),
            Decision::Ask => match self.confined_allow(request) {
                Some(allowed) => allowed,
                None => self.resolve_ask(request, after).await,
            },
        }
    }

    /// An `Ask` the kernel already answers, or `None` to go on and prompt.
    ///
    /// **This is the trade the OS sandbox is worth making.** Approval fatigue is
    /// the tax every agent charges, and the largest single source of it is
    /// `Exec`: the command allowlist cannot anticipate a project's own tooling,
    /// so every unlisted `just`, `pnpm`, `bazel`, or one-off `sed` stops the
    /// session on a prompt. Under kernel path confinement, running one is not a
    /// question worth asking: the child inherits the Landlock domain, so it
    /// reaches nothing outside the root, and everything inside the root is
    /// already allowed by the rule the user is being asked to re-approve one
    /// command at a time.
    ///
    /// Four conditions, and all four are load-bearing.
    ///
    /// **Only `Exec`.** Path asks are not covered: confinement bounds *where* a
    /// write can land, and a write inside the root is exactly what a `deny_write`
    /// rule is there to refuse. Network asks are not covered either: a tool that
    /// asks to connect is asking this broker, and the answer stays the user's.
    ///
    /// **Only when `paths` is `Enforced`.** Not "the user passed `--sandbox`",
    /// not "a backend was installed", but the backend's own report for the
    /// policy actually loaded. A `Partial` or `Unenforced` result means the
    /// confinement this reasoning rests on is not there, and the prompt is the
    /// only thing left.
    ///
    /// **Only when `connect` is `Enforced`, and this is the condition that was
    /// missing.** Auto-allowing an exec hands a *child process* the domain, and a
    /// child's `connect` never reaches this broker: nothing here can prompt for
    /// it, so keeping the `Connect` prompt is no protection at all against the
    /// command this method just waved through. The only thing standing between an
    /// auto-allowed `curl` and the repository leaving the machine is Landlock's
    /// own `ConnectTcp` rule, so the dimension that has to be enforced is the one
    /// governing the threat. It is not always there when `paths` is: a policy
    /// naming a host without a port cannot be narrowed into a network rule, so
    /// `connect` degrades to `Partial` while `paths` stays `Enforced` and the
    /// session still starts. That combination used to auto-allow every unlisted
    /// command with unrestricted egress behind it.
    ///
    /// **Only when the policy did not speak.** A `deny` rule for a program is
    /// still a denial: this converts silence into consent, never a refusal into
    /// one.
    fn confined_allow(&self, request: &Request) -> Option<(Decision, Scope, DecisionSource)> {
        if !matches!(request, Request::Exec { .. }) {
            return None;
        }
        let capabilities = self.capabilities();
        if capabilities.paths != Enforcement::Enforced {
            return None;
        }
        if capabilities.connect != Enforcement::Enforced {
            return None;
        }
        Some((Decision::Allow, Scope::Once, DecisionSource::Confined))
    }

    /// Answer an `Ask` from the remembered set if the user already covered it,
    /// otherwise prompt.
    ///
    /// **A shell line with exactly one program the policy does not allow is a
    /// question about that program**, so that command is what is recalled and
    /// what the prompter is shown: the rest of the line would run unasked on
    /// its own. Asking about the whole line instead is what made "always"
    /// useless, since the next line differed by a flag and asked again. The
    /// record still names the line as the access, with the rule that covered
    /// it as its scope.
    async fn resolve_ask(
        &self,
        request: &Request,
        after: Option<&str>,
    ) -> (Decision, Scope, DecisionSource) {
        let sole = sole_unlisted_command(self.evaluator.as_ref(), &self.policy, request);
        let subject = sole.as_ref().unwrap_or(request);

        // The recalled decision is recorded with the *rule* that covered it, not
        // `Scope::Once`. `RememberedAllow { scope: Once }` contradicts itself,
        // and the fold only rebuilds a grant from a `Scope::Rule`: with `Once`
        // here, the rule lives in exactly one record, the original prompt.
        // Compact that turn away and every remembered allow after it folds to
        // nothing, so resume re-asks what the user already answered.
        let remembered = match self.recall(request) {
            Remembrance::Unknown => self.recall(subject),
            known => known,
        };
        match remembered {
            Remembrance::Allow(rule) => {
                return (
                    Decision::Allow,
                    Scope::Rule(rule),
                    DecisionSource::Remembered,
                );
            }
            Remembrance::Deny(rule) => {
                return (
                    Decision::Deny {
                        reason: "a remembered rule denies this".to_string(),
                    },
                    Scope::Rule(rule),
                    DecisionSource::Remembered,
                );
            }
            Remembrance::Unknown => {}
        }

        let preview = if let (Request::Write(path), Some(after)) = (subject, after) {
            self.preview_write(path, after)
        } else {
            None
        };
        let grant = match &preview {
            Some(preview) => self.prompter.ask_previewed(subject, preview).await,
            None => self.prompter.ask(subject).await,
        };
        let scope = grant.scope.clone();
        if let Ok(mut remembered) = self.remembered.lock() {
            remembered.remember(grant.clone());
        }
        let decision = if grant.allow {
            Decision::Allow
        } else {
            Decision::Deny {
                reason: "denied at the prompt".to_string(),
            }
        };
        (decision, scope, DecisionSource::Prompt)
    }

    /// The preview for a write about to be prompted, or `None` to ask without one.
    ///
    /// `before` is read here for the person answering, not for the agent:
    /// nothing read is handed back to the caller or recorded. A path the policy
    /// denies reading gets no preview, since asking about an overwrite of
    /// `.env` must not put on screen what its read rule exists to keep back.
    /// Neither does a file too large to diff, or one that is not text.
    fn preview_write(&self, path: &Path, after: &str) -> Option<WritePreview> {
        let read = Request::Read(path.to_path_buf());
        if let Decision::Deny { .. } = self.evaluator.evaluate(&self.policy, &read) {
            return None;
        }
        if after.len() as u64 > MAX_PREVIEW_BYTES {
            return None;
        }
        let before = match self.open_authorized(path, OpenMode::Read) {
            Ok(file) => read_previewable(file)?,
            Err(OpenError::Io(error)) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(OpenError::Denied(_) | OpenError::Io(_)) => return None,
        };
        Some(WritePreview {
            after: after.to_string(),
            before,
        })
    }

    fn recall(&self, request: &Request) -> Remembrance {
        self.remembered
            .lock()
            .map(|remembered| remembered.recall(request))
            .unwrap_or(Remembrance::Unknown)
    }

    fn record(
        &self,
        request: &Request,
        decision: &Decision,
        scope: &Scope,
        source: DecisionSource,
        supplied: Supplied,
    ) {
        let Some(decisions) = &self.decisions else {
            return;
        };
        // A closed receiver means the engine is gone; a tool task may outlive it
        // briefly on shutdown, and that is not worth a panic.
        let _ = decisions.send(PermissionRecord {
            call: current_call(),
            decision: decision.clone(),
            request: request.clone(),
            scope: scope.clone(),
            source,
            supplied,
        });
    }
}

// ========================================================================
// AllowAll Implementations
// ========================================================================

#[cfg(feature = "testing")]
impl PolicyEvaluator for AllowAll {
    fn evaluate(&self, _policy: &Policy, _request: &Request) -> Decision {
        Decision::Allow
    }
}

// ========================================================================
// Denied Implementations
// ========================================================================

impl Denied {
    pub fn new(reason: String) -> Self {
        Self { reason }
    }
}

// ========================================================================
// Unenforceable Implementations
// ========================================================================

impl std::fmt::Display for Unenforceable {
    /// Names every gap. "Unenforced" without the dimension and the reason is not
    /// something anyone can act on. The remedy is the caller's to phrase: this
    /// crate does not know which flag (if any) a frontend exposes for building
    /// a broker that tolerates unenforced dimensions.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "the `{}` backend cannot enforce what this policy demands:",
            self.backend
        )?;
        for gap in &self.gaps {
            writeln!(f, "  - {}: {}", gap.dimension, gap.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for Unenforceable {}

// ========================================================================
// Functions
// ========================================================================

/// What a decision means for the caller that asked for it.
///
/// `decide` resolves every `Ask`; one reaching here would mean it did not, and
/// allowing it would be the wrong direction to guess in.
fn verdict(decision: Decision) -> Result<(), Denied> {
    match decision {
        Decision::Allow => Ok(()),
        Decision::Deny { reason } => Err(Denied::new(reason)),
        Decision::Ask => Err(Denied::new("unresolved ask".to_string())),
    }
}

/// A file's content for a write preview, or `None` if it is too large or not
/// UTF-8. Read through a bounded reader, so a file that grew past the limit
/// between the size check and the read still costs no more than the limit.
fn read_previewable(file: File) -> Option<String> {
    let mut content = String::new();
    let mut bounded = file.take(MAX_PREVIEW_BYTES + 1);
    bounded.read_to_string(&mut content).ok()?;
    (content.len() as u64 <= MAX_PREVIEW_BYTES).then_some(content)
}

/// Resolve `host` to every address it answers with.
///
/// An empty answer is an error here rather than an empty `Vec` reaching a
/// caller: a resolver that returns nothing and one that fails are the same fact
/// for anyone who wanted to connect.
async fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>, Denied> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|source| Denied::new(format!("{host}:{port}: {source}")))?
        .collect();
    if addrs.is_empty() {
        return Err(Denied::new(format!(
            "{host}:{port}: resolved to no addresses"
        )));
    }
    Ok(addrs)
}

/// Open `path` with `options`, refusing anything that is not a regular file.
///
/// **This is what makes tool reads safe to do blocking.** The plan's argument for
/// `std::fs` over `tokio::fs` is that a local file read has bounded latency, so a
/// thread hop costs more than the stall it avoids. That argument is sound and its
/// premise is not free: *the agent chooses the path*, and the agent is the
/// adversary. `read` a FIFO with no writer and the "bounded" read never returns,
/// inside a spawned task, so the dispatcher's timeout fires, reports
/// `Outcome::TimedOut`, aborts a task that cannot be aborted (there is no await
/// point in a blocking syscall), and leaves the thread parked forever. Repeat
/// until every worker is gone. Some `/proc` and `/dev` files do the same.
///
/// So the premise is made true rather than the conclusion abandoned: a regular
/// file on local disk really is bounded, and nothing else is openable. That also
/// costs nothing per read, where moving to `spawn_blocking` would pay the hop the
/// plan rightly declines and *still* park a pool thread on the FIFO.
///
/// `O_NONBLOCK` is the other half, and only on unix. Without it the check comes
/// too late to matter: `open` on a FIFO **itself** blocks waiting for a peer, so
/// the hang happens before there is a handle to inspect. With it, a FIFO open
/// returns immediately (or `ENXIO`), and the flag is a no-op on the regular files
/// that survive the check.
fn open_regular(options: &mut OpenOptions, path: &Path) -> Result<File, OpenError> {
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK)
    };
    let file = options.open(path).map_err(OpenError::Io)?;
    regular_file(file, path)
}

/// The options for one [`OpenMode`].
///
/// Opened, not truncated, for writes: [`AuthorizedWrite::write`] truncates when
/// it has content to put there, so an authorized write that is never performed
/// leaves the file as it found it.
fn options_for(mode: OpenMode) -> OpenOptions {
    let mut options = OpenOptions::new();
    match mode {
        OpenMode::Read => options.read(true),
        OpenMode::Write => options.create(true).truncate(false).write(true),
    };
    options
}

/// Refuse an open that landed on something other than a regular file.
///
/// **Applied to both open paths**, because it is not about how the file was
/// reached. `openat2` guarantees containment, not file *type*: a FIFO inside the
/// repository is beneath the root and still turns a "bounded" read into a
/// permanent block, inside a spawned task, until the dispatcher's timeout fires
/// on a task with no await point to abort at.
fn regular_file(file: File, path: &Path) -> Result<File, OpenError> {
    let kind = file.metadata().map_err(OpenError::Io)?.file_type();
    if !kind.is_file() {
        return Err(OpenError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        )));
    }
    Ok(file)
}

/// A descriptor on the policy root, or `None` if it cannot be opened.
#[cfg(target_os = "linux")]
fn open_root_dir(root: &Path) -> Option<File> {
    File::open(root).ok()
}

/// Clear the child's environment, re-add only the allowlist, and point the
/// platform's temp variables at [`SCRATCH_DIR`] under `root`.
///
/// A variable that is unset in the harness stays unset in the child, so this
/// never invents a value except for the temp directory, which is set precisely
/// so the harness's own is not what the child reaches for.
///
/// A scratch directory that cannot be created is not fatal: the child then falls
/// back to the platform default, which is the behaviour before this existed, and
/// failing an authorized command over a `mkdir` would be a worse trade than
/// losing the confinement it buys.
fn apply_environment(command: &mut Command, root: &Path) {
    command.env_clear();
    for key in CHILD_ENV_COMMON.iter().chain(CHILD_ENV_PLATFORM) {
        // `var_os`, not `var`: `PATH` and `USERPROFILE` routinely hold
        // non-UTF-8 bytes, and dropping the variable because it does not decode
        // would leave the child unable to find any program at all.
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let scratch = root.join(SCRATCH_DIR);
    if std::fs::create_dir_all(&scratch).is_ok() {
        prune_scratch(&scratch);
        for key in TEMP_VARS {
            command.env(key, &scratch);
        }
    }
}

/// Remove `scratch`'s entries older than [`SCRATCH_RETENTION`].
///
/// Runs from [`apply_environment`], on every authorized command that gets a
/// scratch directory, rather than on a timer: the broker has no background
/// task, and pruning only when something is about to use the directory is the
/// cheapest way to bound its growth without one. An entry this cannot read the
/// age of, or cannot remove (a race with whatever wrote it, a permissions
/// error), is left in place -- this is housekeeping, not something an
/// authorized command should fail over.
fn prune_scratch(scratch: &Path) {
    let Some(cutoff) = SystemTime::now().checked_sub(SCRATCH_RETENTION) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified > cutoff {
            continue;
        }
        if metadata.is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Resolve `path` against `cwd` when it is relative, returning an owned path.
/// Tools call this to normalize a model-supplied relative path before asking the
/// broker for a handle.
pub fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use policy::RuleSpec;

    use super::*;

    fn policy() -> Policy {
        Policy::builtin(PathBuf::from("/repo"))
    }

    /// Answers every prompt with yes, for the tests whose subject is not the
    /// decision. The shipped [`AutoDeny`] is the opposite and is what an
    /// unattended session gets; this exists only so a test can reach the code
    /// past the prompt.
    struct AllowOnce;

    #[async_trait::async_trait]
    impl Prompter for AllowOnce {
        async fn ask(&self, _request: &Request) -> Grant {
            Grant::once(true)
        }
    }

    /// Answers "always" with the rule the CLI's prompt would offer, and keeps
    /// what it was shown.
    struct AlwaysRecording {
        asked: Arc<Mutex<Vec<Request>>>,
    }

    #[async_trait::async_trait]
    impl Prompter for AlwaysRecording {
        async fn ask(&self, request: &Request) -> Grant {
            self.asked.lock().expect("lock").push(request.clone());
            let Request::Exec { args, program } = request else {
                return Grant::once(true);
            };
            Grant {
                allow: true,
                scope: Scope::Rule(RuleSpec::for_exec(program, args)),
            }
        }
    }

    /// Asks about every write, and denies reading any file named `secret`.
    struct AskWrites;

    impl PolicyEvaluator for AskWrites {
        fn evaluate(&self, _policy: &Policy, request: &Request) -> Decision {
            match request {
                Request::Read(path) if path.ends_with("secret") => Decision::Deny {
                    reason: "secret".to_string(),
                },
                Request::Write(_) => Decision::Ask,
                Request::Connect { .. }
                | Request::Exec { .. }
                | Request::Listen { .. }
                | Request::Read(_) => Decision::Allow,
            }
        }
    }

    /// Allows once, and keeps the preview each prompt was shown, `None` for a
    /// prompt that came without one.
    struct PreviewRecording {
        shown: Arc<Mutex<Vec<Option<WritePreview>>>>,
    }

    #[async_trait::async_trait]
    impl Prompter for PreviewRecording {
        async fn ask(&self, _request: &Request) -> Grant {
            self.shown.lock().expect("lock").push(None);
            Grant::once(true)
        }

        async fn ask_previewed(&self, _request: &Request, preview: &WritePreview) -> Grant {
            self.shown.lock().expect("lock").push(Some(preview.clone()));
            Grant::once(true)
        }
    }

    fn shell(line: &str) -> Request {
        Request::Exec {
            args: vec!["-c".to_string(), line.to_string()],
            program: "sh".to_string(),
        }
    }

    #[tokio::test]
    async fn always_on_a_line_covers_the_next_line_using_that_program() {
        // The journals' bug: "always" remembered the exact line, so the next
        // run of the same tool with one flag changed asked again, every time.
        let asked = Arc::new(Mutex::new(Vec::new()));
        let broker = BrokerBuilder::new(policy())
            .backend(Box::new(NoneBackend::new("test")))
            .evaluator(Box::new(RulesEvaluator::new()))
            .prompter(Box::new(AlwaysRecording {
                asked: asked.clone(),
            }))
            .build(true)
            .expect("unenforced is allowed here");

        let (first, _, source) = broker
            .decide(&shell("python3 -m unittest 2>&1 | tail -5"), None)
            .await;
        assert_eq!((first, source), (Decision::Allow, DecisionSource::Prompt));
        assert_eq!(
            asked.lock().expect("lock").as_slice(),
            [Request::Exec {
                args: vec!["-m".into(), "unittest".into()],
                program: "python3".into(),
            }],
            "the prompt is about the one program the policy did not allow"
        );

        let (second, scope, source) = broker
            .decide(&shell("python3 -m pytest -q | head -20"), None)
            .await;
        assert_eq!(
            (second, source),
            (Decision::Allow, DecisionSource::Remembered)
        );
        assert_eq!(
            scope,
            Scope::Rule(RuleSpec::Program {
                program: "python3".into()
            })
        );
        assert_eq!(asked.lock().expect("lock").len(), 1, "asked a second time");

        // A different program is a different question.
        broker.decide(&shell("node build.js"), None).await;
        assert_eq!(asked.lock().expect("lock").len(), 2);
    }

    /// A backend that reports whatever enforcement a test needs on the two
    /// dimensions `confined_allow` consults, so it can be exercised without a
    /// kernel that has Landlock.
    struct FakeCapabilities {
        connect: Enforcement,
        paths: Enforcement,
    }

    impl SandboxBackend for FakeCapabilities {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn capabilities(&self, _policy: &Policy) -> Capabilities {
            Capabilities {
                connect: self.connect.clone(),
                exec: Enforcement::Enforced,
                listen: Enforcement::Enforced,
                paths: self.paths.clone(),
            }
        }

        fn enter(&self, _policy: &Policy, _harness: &Harness) -> io::Result<()> {
            Ok(())
        }

        fn wrap(&self, _command: &mut Command) {}
    }

    fn broker_with(paths: Enforcement) -> Broker {
        broker_reporting(Enforcement::Enforced, paths)
    }

    fn broker_reporting(connect: Enforcement, paths: Enforcement) -> Broker {
        BrokerBuilder::new(policy())
            .backend(Box::new(FakeCapabilities { connect, paths }))
            // No prompter is installed, so any request that reaches `resolve_ask`
            // is auto-denied. That is what makes the assertions below sharp: an
            // `Allow` can only have come from `confined_allow`.
            .build(true)
            .expect("the fake backend enforces enough to build")
    }

    fn exec_request() -> Request {
        Request::Exec {
            program: "just".to_string(),
            args: vec!["test".to_string()],
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_service_stays_open_across_several_exchanges() {
        // The property `AuthorizedCommand` cannot provide: the child is still
        // there for a second request. `cat` echoes stdin to stdout line by
        // line, which is exactly the shape of a stdio server without needing
        // one installed.
        let broker = BrokerBuilder::new(Policy::builtin(std::env::temp_dir()))
            .backend(Box::new(NoneBackend::new("test")))
            .evaluator(Box::new(RulesEvaluator::new()))
            .prompter(Box::new(AllowOnce))
            .build(true)
            .expect("unenforced is allowed here");

        let mut service = broker
            .serve("cat", &[])
            .await
            .expect("cat starts and its stdio is piped");

        for message in ["first\n", "second\n"] {
            service.send(message.as_bytes()).await.expect("send");
            let mut line = String::new();
            service.read_line(&mut line).await.expect("read");
            assert_eq!(line, message, "the child answered a later exchange");
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_service_reads_an_exact_body_after_its_header() {
        // The framing every stdio protocol uses: a length line, then that many
        // bytes. `read_exact` must stop at the boundary rather than consuming
        // whatever else is buffered.
        let broker = BrokerBuilder::new(Policy::builtin(std::env::temp_dir()))
            .backend(Box::new(NoneBackend::new("test")))
            .evaluator(Box::new(RulesEvaluator::new()))
            .prompter(Box::new(AllowOnce))
            .build(true)
            .expect("unenforced is allowed here");

        let mut service = broker.serve("cat", &[]).await.expect("cat starts");
        service.send(b"5\nHELLOWORLD").await.expect("send");

        let mut header = String::new();
        service.read_line(&mut header).await.expect("header");
        assert_eq!(header.trim(), "5");

        let mut body = vec![0u8; 5];
        service.read_exact(&mut body).await.expect("body");
        assert_eq!(&body, b"HELLO", "read past its length");
    }

    #[tokio::test]
    async fn a_service_is_denied_exactly_like_an_exec() {
        // It runs a program of the caller's choosing, so it is the same act and
        // gets the same check. A service that skipped the policy would be an
        // exec bypass wearing a different name.
        let broker = BrokerBuilder::new(Policy::builtin(std::env::temp_dir()))
            .backend(Box::new(NoneBackend::new("test")))
            .evaluator(Box::new(RulesEvaluator::new()))
            .build(true)
            .expect("unenforced is allowed here");
        // No prompter is installed, so the `Ask` an unlisted program produces is
        // auto-denied.
        assert!(broker.serve("definitely-not-allowed", &[]).await.is_err());
    }

    #[tokio::test]
    async fn kernel_confinement_allows_an_unlisted_command_without_asking() {
        // The prompt this removes is the most common one in a real session: a
        // project's own tooling is never on the built-in allowlist.
        let broker = broker_with(Enforcement::Enforced);
        let (decision, _scope, source) = broker.decide(&exec_request(), None).await;
        assert_eq!(decision, Decision::Allow);
        assert_eq!(
            source,
            DecisionSource::Confined,
            "the allow must be attributable to the sandbox, not to a rule"
        );
    }

    #[tokio::test]
    async fn without_kernel_confinement_the_same_command_still_asks() {
        // The condition the whole argument rests on. `Partial` is the dangerous
        // case: it is not `Unenforced`, and treating "some enforcement" as
        // "confined" would auto-allow commands with nothing behind them.
        for paths in [
            Enforcement::Unenforced {
                reason: "no backend".to_string(),
            },
            Enforcement::Partial {
                reason: "kernel too old for some rules".to_string(),
            },
        ] {
            let broker = broker_with(paths.clone());
            let (decision, _scope, source) = broker.decide(&exec_request(), None).await;
            assert!(
                matches!(decision, Decision::Deny { .. }),
                "with {paths:?} the ask should have reached the auto-denying prompter"
            );
            assert_ne!(source, DecisionSource::Confined, "{paths:?}");
        }
    }

    #[tokio::test]
    async fn unconfined_egress_stops_the_exec_auto_allow() {
        // The regression this guards. Auto-allowing an exec hands the domain to a
        // child whose `connect` never reaches this broker, so the `Connect`
        // prompt below protects nothing against it: Landlock's own rule is the
        // only thing left, and a policy naming a host without a port cannot be
        // narrowed into one. That leaves `connect` at `Partial` while `paths`
        // stays `Enforced` and the session still starts, which used to auto-allow
        // every unlisted command with unrestricted egress behind it.
        for connect in [
            Enforcement::Unenforced {
                reason: "kernel too old for network rules".to_string(),
            },
            Enforcement::Partial {
                reason: "a host with no port".to_string(),
            },
        ] {
            let broker = broker_reporting(connect.clone(), Enforcement::Enforced);
            let (decision, _scope, source) = broker.decide(&exec_request(), None).await;
            assert!(
                matches!(decision, Decision::Deny { .. }),
                "with connect {connect:?} the exec ask should have reached the prompter"
            );
            assert_ne!(source, DecisionSource::Confined, "{connect:?}");
        }
    }

    #[tokio::test]
    async fn confinement_does_not_auto_allow_the_network() {
        // A tool that asks to connect is asking this broker, and the answer stays
        // the user's however well confined the session is.
        let broker = broker_with(Enforcement::Enforced);
        let request = Request::Connect {
            host: "evil.test".to_string(),
            port: 443,
        };
        let (decision, _scope, source) = broker.decide(&request, None).await;
        assert!(matches!(decision, Decision::Deny { .. }));
        assert_ne!(source, DecisionSource::Confined);
    }

    #[tokio::test]
    async fn confinement_does_not_override_a_policy_denial() {
        // Silence becomes consent; a refusal never does. `.git/` is denied by
        // rule, and a confined session must still refuse to write it.
        let broker = broker_with(Enforcement::Enforced);
        let request = Request::Write(PathBuf::from("/repo/.git/config"));
        let (decision, _scope, source) = broker.decide(&request, None).await;
        assert!(
            matches!(decision, Decision::Deny { .. }),
            "a deny rule outranks confinement"
        );
        assert_eq!(source, DecisionSource::Policy);
    }

    #[test]
    fn a_policy_the_backend_cannot_enforce_refuses_to_start() {
        // The gate: without it, "network off" is a statement of intent that
        // reads exactly like a fact. The `none` backend enforces nothing, so a
        // policy handed to it must not quietly appear to be in force.
        let error = BrokerBuilder::new(policy())
            .backend(Box::new(NoneBackend::new("no backend was selected")))
            .build(false);
        let Err(error) = error else {
            panic!("an unenforceable policy must refuse to start");
        };

        assert_eq!(error.backend, "none");
        assert_eq!(error.gaps.len(), 4, "every dimension is a gap");
    }

    #[test]
    fn allow_unenforced_downgrades_the_refusal() {
        // What the `none` backend and Windows need. The check still runs and
        // every decision is still journaled; only the second line is missing.
        let broker = BrokerBuilder::new(policy())
            .backend(Box::new(NoneBackend::new("none")))
            .build(true)
            .expect("allow_unenforced clears the check");
        assert_eq!(broker.backend_name(), "none");
    }

    #[test]
    fn the_refusal_names_every_gap() {
        // "Unenforced" with no dimension and no reason is not something anyone
        // can act on. The remedy is deliberately not in this Display: a library
        // error type does not know which flag a frontend exposes, so the caller
        // states the way out.
        let error = BrokerBuilder::new(policy())
            .backend(Box::new(NoneBackend::new("kernel too old")))
            .build(false);
        let Err(error) = error else {
            panic!("refuses");
        };
        let rendered = error.to_string();

        for dimension in ["connect", "exec", "listen", "paths"] {
            assert!(
                rendered.contains(dimension),
                "{dimension} unnamed: {rendered}"
            );
        }
        assert!(rendered.contains("kernel too old"), "{rendered}");
        // No frontend-flag remedy leaked into a library error message.
        assert!(!rendered.contains("--"), "{rendered}");
    }

    #[test]
    fn prune_scratch_removes_stale_entries_but_keeps_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stale.txt");
        let fresh = dir.path().join("fresh.txt");
        std::fs::write(&stale, b"old").unwrap();
        std::fs::write(&fresh, b"new").unwrap();

        let stale_time = SystemTime::now()
            .checked_sub(SCRATCH_RETENTION + Duration::from_secs(60))
            .expect("a day and a minute before now fits in a SystemTime");
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(stale_time)
            .unwrap();

        prune_scratch(dir.path());

        assert!(
            !stale.exists(),
            "an entry older than the retention window is pruned"
        );
        assert!(fresh.exists(), "a fresh entry survives pruning");
    }

    #[test]
    fn prune_scratch_on_a_missing_directory_does_nothing() {
        // `apply_environment` calls this only after `create_dir_all` succeeds,
        // but the function must not assume that: a path that vanished between
        // the two, or was never created, must not panic the authorized
        // command this runs ahead of.
        prune_scratch(Path::new("/nonexistent/summer-scratch-prune-test"));
    }

    #[tokio::test]
    async fn a_prompted_write_shows_the_file_before_and_after_except_where_reading_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonicalize(dir.path()).expect("canonical root");
        std::fs::write(root.join("a.txt"), "old\n").expect("a.txt");
        std::fs::write(root.join("secret"), "SECRET=1\n").expect("secret");
        let shown = Arc::new(Mutex::new(Vec::new()));
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .evaluator(Box::new(AskWrites))
            .prompter(Box::new(PreviewRecording {
                shown: shown.clone(),
            }))
            .build(true)
            .expect("unenforced is allowed here");

        for (name, after) in [("a.txt", "new\n"), ("b.txt", "fresh\n"), ("secret", "x\n")] {
            broker
                .write_previewed(&root.join(name), after)
                .await
                .expect("allowed at the prompt");
        }
        assert_eq!(
            shown.lock().expect("lock").as_slice(),
            [
                Some(WritePreview {
                    after: "new\n".into(),
                    before: "old\n".into(),
                }),
                Some(WritePreview {
                    after: "fresh\n".into(),
                    before: String::new(),
                }),
                None,
            ],
            "an existing file diffs against its content, a new one against nothing, \
             and a read-denied one is asked about without showing what it holds"
        );
    }
}
