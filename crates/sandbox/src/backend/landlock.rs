//! The Landlock backend: OS-level path and network enforcement on Linux.
//!
//! **Landlock degrades quietly, and that is the whole reason
//! [`Capabilities`] exists.** Network rules need ABI v4 (Linux 6.7); an older
//! kernel silently gives you a ruleset without them. So this backend probes the
//! ABI the running kernel actually offers and reports per dimension, rather than
//! reporting what Landlock enforces in principle.
//!
//! Three Landlock properties are constraints on the daemon rather than details
//! of this file, and Phase 7 inherits all of them: rulesets are **per-thread**,
//! **inherited** only by threads and processes created after the restriction,
//! and **irreversible**. One session per process makes all three invisible;
//! Phase 7 makes all three bite at once, which is why a landlock worker must be
//! a process and not a thread in a shared pool.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath,
    PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use policy::{Effect, ListenRule, Policy};
use tokio::process::Command;

use super::{Capabilities, Enforcement, Harness, SandboxBackend};

// ========================================================================
// Constants
// ========================================================================

/// The ABI this backend builds its rules against. Not the newest: the rights we
/// ask for have to be ones we have reasoned about.
const TARGET_ABI: ABI = ABI::V4;

/// Landlock gained network rules (`AccessNet`) in ABI v4, Linux 6.7.
const NETWORK_ABI: i32 = 4;

/// Path rights exist from ABI v1, Linux 5.13.
const PATH_ABI: i32 = 1;

/// Prefixes granted read and execute.
///
/// **Without these the sandbox denies everything the policy allows.** A ruleset
/// whose only rule is the repo root does not merely confine writes: Landlock
/// refuses a handled access wherever no rule grants it, so `/bin/sh` cannot be
/// executed, the dynamic loader cannot open `libc`, and `/etc/resolv.conf` and
/// the CA bundle become unreadable. Every program on the command allow list
/// fails with `EACCES` and the harness loses DNS and TLS, which left dropping
/// `--sandbox` -- giving up OS enforcement altogether -- as the only way to have
/// a working session.
///
/// Read and execute only: nothing here is writable, so "the agent may only write
/// inside the root" still holds. `AccessFs::from_read` carries `Execute`, which
/// is why one list covers both needs.
///
/// **These prefixes are not sufficient on their own**, and assuming they were is
/// how `cargo` ended up unrunnable under the ruleset: see
/// [`program_directories`]. `/run/systemd/resolve` rather than all of `/run`: on
/// a systemd host `/etc/resolv.conf` is a symlink into it, so DNS breaks without
/// it, and the rest of `/run` is other services' sockets and runtime state.
const SYSTEM_READ_PREFIXES: &[&str] = &[
    "/bin",
    "/etc",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/run/systemd/resolve",
    "/sbin",
    "/sys",
    "/usr",
];

/// Character devices every language runtime opens, granted read and write.
///
/// Named individually rather than granting `/dev`: `/dev` also holds raw block
/// devices and the terminal, and a prefix rule would hand those over with them.
const DEVICE_PATHS: &[&str] = &["/dev/null", "/dev/random", "/dev/urandom", "/dev/zero"];

/// Toolchain roots, as an environment variable and the path it defaults to under
/// the home directory.
///
/// **A toolchain is not one program, and `PATH` only finds the first hop.**
/// `~/.cargo/bin/cargo` is a rustup *shim*: it re-execs the real binary under
/// `~/.rustup/toolchains/<toolchain>/bin/`, which no amount of `PATH` scanning
/// reveals. `cargo` also reads its registry out of `CARGO_HOME`, so without both
/// roots a build cannot resolve a dependency it already has on disk. Naming the
/// two roots is what makes the whole toolchain usable rather than just its
/// entry point.
const TOOLCHAIN_ROOTS: &[(&str, &str)] = &[("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")];

// ========================================================================
// Data Structures
// ========================================================================

/// Landlock, probed against the running kernel.
pub struct LandlockBackend {
    abi: i32,
}

/// Which network accesses the ruleset may handle for a given policy.
///
/// A `false` here means the kernel does not police that dimension at all, which
/// is strictly better than the alternative of policing it into a total denial.
/// See [`narrowable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NarrowableNet {
    bind: bool,
    connect: bool,
}

// ========================================================================
// LandlockBackend Implementations
// ========================================================================

impl LandlockBackend {
    pub fn new() -> Self {
        Self { abi: probe_abi() }
    }

    fn path_enforcement(&self) -> Enforcement {
        if self.abi >= PATH_ABI {
            Enforcement::Enforced
        } else {
            Enforcement::Unenforced {
                reason: self.unsupported_reason(PATH_ABI, "5.13"),
            }
        }
    }

    /// One network dimension, for a policy this kernel may or may not be able to
    /// narrow on.
    ///
    /// Three answers, and the middle one is the point. The kernel may be too old
    /// for network rules at all (`Unenforced`, and fatal). It may support them
    /// and this policy may express something they cannot -- `unbounded` names
    /// what, for the message -- in which case the broker enforces the dimension
    /// alone (`Partial`). Or both hold and the kernel narrows it (`Enforced`).
    fn net_enforcement(&self, narrowable: bool, dimension: &str, unbounded: &str) -> Enforcement {
        if self.abi < NETWORK_ABI {
            return Enforcement::Unenforced {
                reason: self.unsupported_reason(NETWORK_ABI, "6.7"),
            };
        }
        if !narrowable {
            return Enforcement::Partial {
                reason: format!(
                    "a Landlock rule names one port, and this policy allows {unbounded} for \
                     {dimension}; the broker enforces it and the kernel adds nothing"
                ),
            };
        }
        Enforcement::Enforced
    }

    /// Name the gap precisely enough to act on: which ABI, which kernel, and
    /// what this one reports.
    fn unsupported_reason(&self, needed: i32, kernel: &str) -> String {
        if self.abi <= 0 {
            return "this kernel has no Landlock support, or it is disabled at boot".to_string();
        }
        format!(
            "Landlock ABI v{needed} (Linux {kernel}) is required; this kernel reports v{}",
            self.abi
        )
    }
}

impl Default for LandlockBackend {
    fn default() -> Self {
        Self::new()
    }
}

// ========================================================================
// NarrowableNet Implementations
// ========================================================================

impl NarrowableNet {
    /// Neither dimension, for a kernel with no network rights at all.
    fn none() -> Self {
        Self {
            bind: false,
            connect: false,
        }
    }

    /// The accesses to declare before `create()`.
    fn handled(self) -> Vec<AccessNet> {
        let mut accesses = Vec::new();
        if self.bind {
            accesses.push(AccessNet::BindTcp);
        }
        if self.connect {
            accesses.push(AccessNet::ConnectTcp);
        }
        accesses
    }
}

impl SandboxBackend for LandlockBackend {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn capabilities(&self, policy: &Policy) -> Capabilities {
        // The same computation `enter` performs, so what is reported and what is
        // installed cannot drift: a dimension this policy cannot be narrowed on
        // is one the ruleset will leave unhandled.
        let narrowable = if self.abi >= NETWORK_ABI {
            narrowable(policy)
        } else {
            NarrowableNet::none()
        };
        Capabilities {
            connect: self.net_enforcement(narrowable.connect, "connect", "a host with no port"),
            // Landlock's Execute right is a path right: it governs which files
            // may be executed, which is what "no exec outside the root" needs.
            exec: self.path_enforcement(),
            listen: self.net_enforcement(narrowable.bind, "listen", "any port"),
            paths: self.path_enforcement(),
        }
    }

    /// Restrict this thread and everything it spawns afterwards.
    ///
    /// **The compatibility level is best-effort, and the doc used to claim the
    /// opposite.** A hard requirement would refuse to start on every kernel
    /// between 5.13 and 6.6, which is most of them; best-effort drops the rights
    /// the running kernel does not know (v3's `Truncate`, v4's network rights)
    /// and applies the rest. That is a real reduction, so it is
    /// [`Capabilities`]'s job to say which dimensions survive it, and this is
    /// only accurate to the dimension: `paths: Enforced` means path rights are in
    /// force *at this kernel's ABI*, not that every right named here was
    /// installed.
    fn enter(&self, policy: &Policy, harness: &Harness) -> io::Result<()> {
        // Every handled access has to be declared before `create()`: the set of
        // rights the ruleset governs is fixed when the kernel object is made.
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(TARGET_ABI))
            .map_err(to_io)?;

        // Only the dimensions this policy can actually be narrowed on. Handling
        // one it cannot express would deny everything on it; see [`narrowable`].
        let network = if self.abi >= NETWORK_ABI {
            narrowable(policy)
        } else {
            NarrowableNet::none()
        };
        for access in network.handled() {
            ruleset = ruleset.handle_access(access).map_err(to_io)?;
        }

        let mut created = ruleset.create().map_err(to_io)?;

        // The root is the only place the agent may *write*. Everything else is
        // denied by omission, which is Landlock's model: a handled access with
        // no rule granting it is refused.
        let root = PathFd::new(&policy.root).map_err(to_io)?;
        created = created
            .add_rule(PathBeneath::new(root, AccessFs::from_all(TARGET_ABI)))
            .map_err(to_io)?;
        created = add_system_rules(created, policy)?;

        if self.abi >= NETWORK_ABI {
            created = add_network_rules(created, policy, harness, network)?;
        }

        let status = created.restrict_self().map_err(to_io)?;
        match status.ruleset {
            // Nothing was applied. The caller cleared this at startup or the
            // kernel changed under us; either way, say so rather than let the
            // absence pass for safety.
            RulesetStatus::NotEnforced => Err(io::Error::other(
                "Landlock reported no enforcement on this kernel",
            )),
            RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => Ok(()),
        }
    }

    /// Landlock is inherited by children, so an exec needs no wrapping.
    fn wrap(&self, _command: &mut Command) {}
}

// ========================================================================
// Functions
// ========================================================================

/// Ask the kernel which Landlock ABI it supports.
///
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returns
/// the version and creates nothing, which is the only way to answer this without
/// restricting ourselves first. The `landlock` crate does exactly this but keeps
/// it private, on the reasoning that building *rules* from a runtime-detected ABI
/// leads to non-deterministic sandboxes. That reasoning is right and does not
/// apply here: this value is used to *report* a gap, never to choose a right.
fn probe_abi() -> i32 {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // Safety: the null attr pointer with size 0 is the documented version query.
    // It has no side effects and creates no file descriptor.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if version < 0 { 0 } else { version as i32 }
}

/// Grant the prefixes outside the root that any program needs to run.
///
/// A prefix that does not exist on this system is skipped rather than fatal:
/// `/lib64` is absent on many distributions and `/opt` on most, and refusing to
/// start over one would make the backend unusable for the sake of a path nothing
/// was going to open.
fn add_system_rules(
    mut created: landlock::RulesetCreated,
    policy: &Policy,
) -> io::Result<landlock::RulesetCreated> {
    let readable = AccessFs::from_read(TARGET_ABI);
    for prefix in SYSTEM_READ_PREFIXES {
        created = add_path_rule(created, Path::new(prefix), readable)?;
    }
    for directory in program_directories(policy) {
        created = add_path_rule(created, &directory, readable)?;
    }
    for path in DEVICE_PATHS {
        created = add_path_rule(created, Path::new(path), readable | AccessFs::WriteFile)?;
    }
    Ok(created)
}

/// Every directory the policy's allowed programs actually live in.
///
/// **The system prefixes are where programs live on a server, not on a developer
/// machine.** `cargo` is on the built-in allow list and resolves to
/// `~/.cargo/bin/cargo` for every rustup install; `node` under nvm, `python`
/// under pyenv, and anything installed by `mise` or `volta` are the same shape.
/// A ruleset covering only `/usr` and `/bin` refuses to execute them, so the
/// agent's central job -- build the project, run its tests -- failed with
/// `EACCES`, and the only remedy was to drop `--sandbox`, giving up every
/// dimension of enforcement at once. Granting `/usr` and hoping is not a policy;
/// reading the policy is.
///
/// **Only `Allow` rules**, for the reason [`add_network_rules`] gives: an `Ask`
/// reaches here from the untrusted project tier, and letting repo content pick a
/// directory to make executable is the widening that tier exists to prevent. The
/// system prefixes and the toolchain roots cover the shells and interpreters an
/// approved `Ask` realistically names.
///
/// The *parent directory* rather than the binary, because programs exec their
/// siblings: `git` reaches for `git-remote-https`, a rustup shim for the real
/// toolchain. Read and execute only -- a `bin` directory is not somewhere the
/// agent may write.
fn program_directories(policy: &Policy) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for rule in &policy.commands {
        if rule.effect != Effect::Allow {
            continue;
        }
        let Some(program) = resolve_program(&rule.program) else {
            continue;
        };
        if let Some(parent) = program.parent() {
            directories.insert(parent.to_path_buf());
        }
    }
    directories.extend(toolchain_roots());
    // Granting the home directory or `/` would hand over everything beneath
    // them, which is the opposite of the point. A `PATH` entry that broad is a
    // misconfiguration, and the system prefixes still cover the ordinary case.
    for path in home_dir().into_iter().chain([PathBuf::from("/")]) {
        directories.remove(&path);
    }
    directories
}

/// Where a program named in the policy resolves to, or `None`.
///
/// A rule may be a glob (`git*`), which names no single file and is skipped: a
/// grant has to be a real path. A rule that already contains a separator is
/// taken as the path it is.
fn resolve_program(program: &str) -> Option<PathBuf> {
    if program.contains(['*', '?', '[', ']', '{', '}']) {
        return None;
    }
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// The toolchain roots that exist on this machine.
fn toolchain_roots() -> Vec<PathBuf> {
    let home = home_dir();
    TOOLCHAIN_ROOTS
        .iter()
        .filter_map(|(variable, fallback)| match std::env::var_os(variable) {
            Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
            _ => home.as_ref().map(|home| home.join(fallback)),
        })
        .filter(|root| root.is_dir())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Add one path rule, skipping a path this system does not have.
fn add_path_rule(
    created: landlock::RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> io::Result<landlock::RulesetCreated> {
    let Ok(fd) = PathFd::new(path) else {
        return Ok(created);
    };
    created
        .add_rule(PathBeneath::new(fd, access))
        .map_err(to_io)
}

/// Translate the policy's network rules into Landlock port rules.
///
/// **This layer is coarser than the policy on purpose.** Landlock's network
/// rules are port-scoped; they cannot express a host. So the kernel enforces
/// "which ports at all", and the broker enforces "which host", which is the only
/// division available. The broker's check is the authority; this narrows what a
/// bug in it could reach.
fn add_network_rules(
    mut created: landlock::RulesetCreated,
    policy: &Policy,
    harness: &Harness,
    network: NarrowableNet,
) -> io::Result<landlock::RulesetCreated> {
    if network.connect {
        // The harness's own egress first. The kernel cannot tell the model's
        // socket from a tool's, so denying outbound to enforce `Connect: Deny`
        // would kill the model connection along with the tools. The broker still
        // denies a tool that asks for this port; on it alone, that check stands
        // without a second line behind it.
        for port in &harness.egress_ports {
            created = created
                .add_rule(NetPort::new(*port, AccessNet::ConnectTcp))
                .map_err(to_io)?;
        }

        // **Only `Allow`.** `Ask` reaches here from the untrusted project tier,
        // whose allows the trust merge downgraded precisely so repo content
        // cannot grant itself a capability. Installing its port anyway would let
        // that content widen the second line of defense even though the broker
        // still prompts -- the one thing the tier exists to prevent. The cost is
        // that a user approving such a prompt is then refused by the kernel; that
        // is the direction to fail in, and `/status` reports the ruleset.
        for rule in &policy.network.connect {
            if rule.effect != Effect::Allow {
                continue;
            }
            let Some(port) = rule.port else {
                continue;
            };
            created = created
                .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
                .map_err(to_io)?;
        }
    }

    if network.bind
        && let ListenRule::Any { ports } | ListenRule::Loopback { ports } = &policy.network.listen
    {
        for port in ports {
            created = created
                .add_rule(NetPort::new(*port, AccessNet::BindTcp))
                .map_err(to_io)?;
        }
    }

    Ok(created)
}

/// Which network dimensions this policy can be expressed on.
///
/// **A dimension the policy leaves unbounded must be left unhandled.** Landlock
/// rules name a port, and "any port" has no rule that says it. Handling the
/// access with no rule to match does not mean "unrestricted", it means *denied*:
/// the kernel would refuse every bind and connect the broker had just approved.
/// That is the shape of the shipped bug -- the default `listen` carries an empty
/// port list, which the broker reads as "any port" and the ruleset as "no
/// ports", so the kernel refused every bind the policy allowed.
///
/// So an unbounded dimension drops to broker-only enforcement, which is the same
/// trade already made for the model's egress port, and `Capabilities` is what
/// says so out loud.
fn narrowable(policy: &Policy) -> NarrowableNet {
    let connect = policy
        .network
        .connect
        .iter()
        .filter(|rule| rule.effect == Effect::Allow)
        .all(|rule| rule.port.is_some());
    let bind = match &policy.network.listen {
        // No rules and a handled access is a total denial, which is exactly what
        // `Deny` asks for.
        ListenRule::Deny => true,
        ListenRule::Any { ports } | ListenRule::Loopback { ports } => !ports.is_empty(),
    };
    NarrowableNet { bind, connect }
}

fn to_io(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("landlock: {error}"))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_answers_without_restricting_this_thread() {
        // The point of the syscall probe: it must be safe to call from a live
        // process. If it restricted us, every later test in this binary would
        // fail.
        let abi = probe_abi();
        assert!(abi >= 0, "probe returned {abi}");
        // Prove nothing was restricted: this read would fail under a ruleset
        // that did not grant it.
        assert!(std::fs::metadata("/").is_ok());
    }

    #[test]
    fn capabilities_track_the_probed_abi() {
        let backend = LandlockBackend::new();
        let policy = Policy::builtin(std::path::PathBuf::from("/repo"));
        let capabilities = backend.capabilities(&policy);

        if backend.abi >= NETWORK_ABI {
            // Outbound is denied by default with no portless allow to express,
            // so the kernel narrows it exactly.
            assert_eq!(capabilities.connect, Enforcement::Enforced);
            // Inbound is *not* `Enforced` under the default policy, and saying
            // so was the overclaim: "loopback, any port" has no port for a
            // Landlock rule to name. See the test below.
            assert!(matches!(capabilities.listen, Enforcement::Partial { .. }));
        } else {
            // The quiet degradation this exists to surface.
            assert!(matches!(
                capabilities.connect,
                Enforcement::Unenforced { .. }
            ));
        }

        if backend.abi >= PATH_ABI {
            assert_eq!(capabilities.paths, Enforcement::Enforced);
        }
    }

    #[test]
    fn a_policy_landlock_cannot_narrow_reports_partial_not_enforced() {
        // **The overclaim this replaces.** The default policy is "loopback, any
        // port", and a Landlock rule names one port -- so the kernel installs
        // nothing for `listen` and the broker enforces it alone. Reporting
        // `Enforced` claimed a second line of defense that was not there.
        // Reporting `Unenforced` would be worse: `build` refuses on gaps, so the
        // shipped default would decline to start and teach everyone to drop
        // `--sandbox`, losing every dimension instead of one.
        let backend = LandlockBackend { abi: NETWORK_ABI };
        let capabilities = backend.capabilities(&Policy::builtin(PathBuf::from("/repo")));

        assert!(matches!(capabilities.listen, Enforcement::Partial { .. }));
        assert!(
            capabilities.gaps().is_empty(),
            "a partial dimension must not refuse to start"
        );
        let limits = capabilities.limits();
        assert_eq!(limits.len(), 1, "and must still be reported");
        assert_eq!(limits[0].dimension, "listen");
    }

    #[test]
    fn a_policy_with_explicit_ports_is_fully_enforced() {
        // The other side: name the ports and the kernel can narrow them, so the
        // dimension really does get a second line of defense.
        let mut policy = Policy::builtin(PathBuf::from("/repo"));
        policy.network.listen = ListenRule::Loopback { ports: vec![8080] };
        let backend = LandlockBackend { abi: NETWORK_ABI };

        assert_eq!(
            backend.capabilities(&policy).listen,
            Enforcement::Enforced,
            "an enumerable port list is exactly what a Landlock rule can express"
        );
    }

    #[test]
    fn an_unsupported_kernel_reports_why_not_just_that() {
        // "Unenforced" alone is unactionable. The reason has to name the ABI and
        // the kernel that would provide it.
        let backend = LandlockBackend { abi: 1 };
        let Enforcement::Unenforced { reason } = backend.net_enforcement(true, "connect", "x")
        else {
            panic!("ABI v1 cannot enforce network rules");
        };
        assert!(reason.contains("v4"), "{reason}");
        assert!(reason.contains("6.7"), "{reason}");
        assert!(reason.contains("v1"), "{reason}");
    }

    #[test]
    fn a_kernel_without_landlock_says_so() {
        let backend = LandlockBackend { abi: 0 };
        let Enforcement::Unenforced { reason } = backend.path_enforcement() else {
            panic!("ABI 0 cannot enforce anything");
        };
        assert!(reason.contains("no Landlock support"), "{reason}");
    }
}
