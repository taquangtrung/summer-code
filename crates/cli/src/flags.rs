//! The command-line vocabulary: every flag, what it asks for, and the refusal
//! to start a session on arguments the binary does not recognize.
//!
//! **An unknown flag is an error, not a silence.** The rest of the binary reads
//! these flags as `.any` scans over `std::env::args`, so a mistyped flag was
//! swallowed and the session started anyway: the user asked for `--sanbox` and
//! got an unconfined session that never mentioned it. Values already refuse to
//! be misread (`--provider` and `--mode` reject an unrecognized value, because
//! a typo on a command line is an immediate, correctable mistake in a way a
//! stale stored preference is not), and this module extends that refusal from
//! values to names.
//!
//! `--help` and `--version` are answered here, before anything else in `main`
//! runs: usage output must cost nothing -- no journal opened, no sandbox
//! entered, no config read -- which is the same ordering rule the headless
//! parse already follows.
//!
//! Hand-rolled rather than an argument-parsing crate. The vocabulary is a
//! dozen flags with no subcommands and no positional grammar beyond the one
//! prompt token that may follow [`PRINT_FLAG`], so the two registry arrays
//! below are the whole parser; a crate would buy generated help text at the
//! cost of a dependency nothing else in the binary needs.

use crate::print::{FORMAT_FLAG, GOAL_FLAG, PRINT_FLAG, PRINT_FLAG_SHORT};

// ========================================================================
// Constants
// ========================================================================

/// Accepted and ignored: the sandbox-off it asked for is now the default.
///
/// Kept because it appears in scripts, aliases, and the previous release's
/// documentation. Silently rejecting a flag that used to work would look like a
/// broken install; silently *accepting* it while doing nothing is fine, because
/// what it asked for is what happens.
pub(crate) const NO_SANDBOX_FLAG: &str = "--no-sandbox";

/// Turn the OS-level sandbox *on* (Landlock on Linux; see `sandbox::backend`
/// for the macOS and Windows state).
///
/// **The default is off, and the friction of the default-on posture is what
/// moved it.** Confinement's costs land on the paths a session shares with
/// the user's own machine rather than with the repo: `/login` cannot store
/// a credential (`~/.summer/auth.json` is outside the root the domain confines
/// the whole process to), the device flow cannot launch a browser (see
/// [`crate::open_url`]), and every user-tier file must be read before `enter`
/// closes the domain around it. Those are the interactions a first run is made
/// of, and each one failing with `EACCES` read as breakage, not as the sandbox
/// working.
///
/// The trade the default makes: with confinement on, an unlisted command
/// needs no prompt, because the child inherits the domain and reaches nothing
/// outside the root (`Broker::confined_allow`). Off, the broker still checks
/// every access against the policy and prompts for what no rule names -- the
/// posture Windows always has.
pub(crate) const SANDBOX_FLAG: &str = "--sandbox";

/// Force the plain line loop even on a TTY. The TUI is the default on a real
/// terminal; this and a piped stdout both fall back to [`crate::repl`], which
/// needs no raw mode and behaves under redirection.
pub(crate) const PLAIN_FLAG: &str = "--plain";

/// `summer -c`: resume this project's newest session — fold its journal in
/// before the first turn, so continuing where you left off is one flag rather
/// than a restart plus an id copied out of `/sessions`.
pub(crate) const CONTINUE_FLAG: &str = "--continue";
pub(crate) const CONTINUE_FLAG_SHORT: &str = "-c";

/// Picks the provider by id (see `provider::ProviderId` for the
/// full registry); an unrecognized value refuses to start rather than
/// silently falling back, since typing a flag is an immediate, correctable
/// mistake in a way a stale stored preference is not.
pub(crate) const PROVIDER_FLAG: &str = "--provider";

/// Overrides the selected provider's default model.
pub(crate) const MODEL_FLAG: &str = "--model";

/// How much the session may do without asking: `plan`, `ask`, `default`, or
/// `full`. See [`crate::mode`]; an unrecognized value refuses to start, the
/// same way `--provider` does, because a mode misread as the default is the
/// difference between a read-only session and one that edits.
pub(crate) const MODE_FLAG: &str = "--mode";

/// Keybindings style override: `emacs` (default) or `vim`.
pub(crate) const KEYMAP_FLAG: &str = "--keymap";

/// Print the usage text and exit.
pub(crate) const HELP_FLAG: &str = "--help";
pub(crate) const HELP_FLAG_SHORT: &str = "-h";

/// Print the version and exit.
pub(crate) const VERSION_FLAG: &str = "--version";
pub(crate) const VERSION_FLAG_SHORT: &str = "-V";

/// Every flag that stands alone: present or absent, nothing after it.
const PRESENCE_FLAGS: &[&str] = &[
    CONTINUE_FLAG,
    CONTINUE_FLAG_SHORT,
    GOAL_FLAG,
    HELP_FLAG,
    HELP_FLAG_SHORT,
    NO_SANDBOX_FLAG,
    PLAIN_FLAG,
    PRINT_FLAG,
    PRINT_FLAG_SHORT,
    SANDBOX_FLAG,
    VERSION_FLAG,
    VERSION_FLAG_SHORT,
];

/// Every flag followed by exactly one value. The scan consumes the value with
/// its flag, so a value is never mistaken for an unknown flag -- the same
/// contract `flag_value` gives its callers.
const VALUE_FLAGS: &[&str] = &[
    FORMAT_FLAG,
    KEYMAP_FLAG,
    MODE_FLAG,
    MODEL_FLAG,
    PROVIDER_FLAG,
];

// ========================================================================
// Data Structures
// ========================================================================

/// What the command line asked for, resolved before anything else is read.
#[derive(Debug)]
pub(crate) enum Requested {
    /// Print the usage text; the caller exits successfully.
    Help,
    /// Start a session, interactive or headless as the flags say.
    Run,
    /// Print the version; the caller exits successfully.
    Version,
}

// ========================================================================
// Functions
// ========================================================================

/// Resolve what the command line asked for, rejecting anything the binary
/// does not recognize and missing values for flags that take one.
///
/// The walk consumes the token after a value flag with the flag itself, so a
/// value is never read as a flag. The one prompt token that may follow
/// [`PRINT_FLAG`] does not start with `-` by construction -- a dash-leading
/// token there is a flag by `PrintRequest`'s own rule, and the prompt arrives
/// from stdin instead -- so prompt words fall through the scan as plain text.
/// Help and version return wherever they appear: a user who typed `--help`
/// after three other flags has not changed their mind.
pub(crate) fn resolve_request(args: &[String]) -> Result<Requested, String> {
    // Index 0 is the program name, not an argument.
    let mut at = 1;
    while at < args.len() {
        let arg = args[at].as_str();
        if arg == HELP_FLAG || arg == HELP_FLAG_SHORT {
            return Ok(Requested::Help);
        }
        if arg == VERSION_FLAG || arg == VERSION_FLAG_SHORT {
            return Ok(Requested::Version);
        }
        if VALUE_FLAGS.contains(&arg) {
            if at + 1 >= args.len() {
                return Err(format!("{arg} expects a value after it"));
            }
            at += 2;
        } else if !arg.starts_with('-') || PRESENCE_FLAGS.contains(&arg) {
            at += 1;
        } else {
            return Err(format!(
                "unknown flag `{arg}`; run {HELP_FLAG} for the accepted list"
            ));
        }
    }
    Ok(Requested::Run)
}

/// The usage text, spelled from the flag constants so the wording cannot drift
/// from what the scan accepts (the test below holds it to exactly that).
pub(crate) fn usage_text() -> String {
    let lines = vec![
        "summer: an agentic coding assistant for the terminal".to_string(),
        String::new(),
        "Usage:".to_string(),
        "  summer [flags]                  interactive session, the default".to_string(),
        format!(
            "  summer {PRINT_FLAG_SHORT} <prompt> [flags]    one headless turn, exit when it ends"
        ),
        String::new(),
        "Session:".to_string(),
        usage_line(
            &format!("{CONTINUE_FLAG_SHORT}, {CONTINUE_FLAG}"),
            "resume this project's newest session",
        ),
        usage_line(
            &format!("{MODE_FLAG} <plan|ask|default|full>"),
            "how much the session may do without asking",
        ),
        usage_line(PLAIN_FLAG, "the plain line loop instead of the TUI"),
        usage_line(
            SANDBOX_FLAG,
            "turn on OS-level confinement (Landlock on Linux)",
        ),
        usage_line(NO_SANDBOX_FLAG, "accepted and ignored: off is the default"),
        String::new(),
        "Model:".to_string(),
        usage_line(
            &format!("{PROVIDER_FLAG} <id>"),
            "provider for this session",
        ),
        usage_line(
            &format!("{MODEL_FLAG} <name>"),
            "override the provider's default model",
        ),
        usage_line(&format!("{KEYMAP_FLAG} <emacs|vim>"), "input editor keymap"),
        String::new(),
        "Headless, with --print:".to_string(),
        usage_line(
            &format!("{PRINT_FLAG_SHORT}, {PRINT_FLAG} [prompt]"),
            "one prompt in, one answer out",
        ),
        usage_line(
            &format!("{FORMAT_FLAG} <text|json|stream-json>"),
            "shape of the answer on stdout",
        ),
        usage_line(GOAL_FLAG, "keep working until the model's plan closes"),
        String::new(),
        usage_line(
            &format!("{HELP_FLAG_SHORT}, {HELP_FLAG}"),
            "print this text and exit",
        ),
        usage_line(
            &format!("{VERSION_FLAG_SHORT}, {VERSION_FLAG}"),
            "print the version and exit",
        ),
        String::new(),
        format!("The prompt may also arrive on stdin: summer {PRINT_FLAG_SHORT} < task.md"),
    ];
    lines.join("\n")
}

/// One aligned usage line: the flag spellings, then the description.
fn usage_line(spelling: &str, description: &str) -> String {
    format!("  {spelling:<34} {description}")
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn an_unknown_flag_is_rejected_not_swallowed() {
        // Before the scan, a typo like this started an unconfined session
        // without a word -- the flag the user meant never applied.
        let error = resolve_request(&args(&["summer", "--sanbox"])).unwrap_err();
        assert!(error.contains("--sanbox"), "{error}");
        assert!(error.contains("unknown flag"), "{error}");
    }

    #[test]
    fn a_value_flag_without_its_value_is_rejected() {
        // `flag_value` would have returned `None` and the session would run
        // with the stored default, silently ignoring the flag entirely.
        let error = resolve_request(&args(&["summer", "--provider"])).unwrap_err();
        assert!(error.contains("expects a value"), "{error}");
    }

    #[test]
    fn no_sandbox_stays_accepted_for_old_scripts() {
        // Dropping it from the registry would turn every documented
        // `--no-sandbox` invocation into a startup failure.
        assert!(matches!(
            resolve_request(&args(&["summer", "--no-sandbox"])),
            Ok(Requested::Run)
        ));
    }

    #[test]
    fn help_and_version_win_wherever_they_appear() {
        assert!(matches!(
            resolve_request(&args(&["summer", "--sandbox", "--help"])),
            Ok(Requested::Help)
        ));
        assert!(matches!(
            resolve_request(&args(&["summer", "-V"])),
            Ok(Requested::Version)
        ));
    }

    #[test]
    fn the_headless_prompt_reads_as_plain_text() {
        // Prompt words carry no dash and are not flags; a dash-leading token
        // after --print is a flag by PrintRequest's rule, so it is scanned as
        // one here too.
        assert!(matches!(
            resolve_request(&args(&[
                "summer",
                "-p",
                "fix",
                "the",
                "bug",
                "--output-format",
                "json"
            ])),
            Ok(Requested::Run)
        ));
    }

    #[test]
    fn usage_text_names_every_flag_the_scan_accepts() {
        // A flag missing from the usage text is a flag a user cannot discover;
        // the registry and the text must move together.
        let text = usage_text();
        for flag in PRESENCE_FLAGS.iter().chain(VALUE_FLAGS) {
            assert!(text.contains(flag), "usage text is missing {flag}");
        }
    }
}
