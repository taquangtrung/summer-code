//! Which shell runs a command line, and how it is invoked.
//!
//! **`sh -c` is a Unix assumption, and three separate places had baked it in**:
//! the `shell` tool, the TUI's command statusline, and the bench runner's
//! success check. On Windows there is no `sh` on `PATH`, so each of those was
//! not "degraded" but simply broken -- the agent's most-used tool failed with
//! `program not found` on every call.
//!
//! The seam is deliberately thin. A shell is a program plus the one flag that
//! means "the next argument is a whole command line", and that pair is the only
//! thing that varies:
//!
//! ```text
//! sh -c "cargo test"          cmd.exe /C "cargo test"
//! bash -c "cargo test"        powershell.exe -Command "cargo test"
//! nu -c "cargo test"          pwsh.exe -Command "cargo test"
//! ```
//!
//! **The program stays a single argv element that the policy sees.** It is what
//! `Broker::exec` authorizes and what a remembered grant is scoped to, so a
//! shell chosen by config is a shell the user is prompted about by name. Nothing
//! here composes a string a shell will re-parse.
//!
//! No `PATH` probing: this crate does not touch the filesystem, and guessing
//! would make the *authorized program* differ between two machines running the
//! same config. The platform default is the one shell that is always present,
//! and anything else is named explicitly by the user.

// ========================================================================
// Constants
// ========================================================================

/// Every shell that can be named in config, with its command-line flag.
///
/// A table rather than a `match`, because it is also the error message: naming
/// an unknown shell should say what the known ones are.
const KNOWN_SHELLS: &[(&str, &str)] = &[
    ("bash", "-c"),
    ("cmd", "/C"),
    ("dash", "-c"),
    ("fish", "-c"),
    ("nu", "-c"),
    ("powershell", "-Command"),
    ("pwsh", "-Command"),
    ("sh", "-c"),
    ("zsh", "-c"),
];

// ========================================================================
// Data Structures
// ========================================================================

/// The program that runs a command line, and the flag that introduces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    /// The flag meaning "what follows is one command line": `-c`, `/C`,
    /// `-Command`.
    pub flag: String,
    /// The program, as the policy will see it.
    pub program: String,
}

// ========================================================================
// ShellSpec Implementations
// ========================================================================

impl ShellSpec {
    /// The shell to use when the user has not named one.
    ///
    /// `sh` on Unix and `cmd` on Windows: in both cases the one shell the
    /// platform guarantees. `cmd.exe` is unglamorous next to PowerShell, and it
    /// is the right default anyway -- it is always installed, starts in
    /// milliseconds rather than hundreds of them, and is not subject to an
    /// execution policy that can refuse to run anything at all. A user who wants
    /// `pwsh` or `nu` says so once in their config.
    pub fn platform_default() -> Self {
        if cfg!(windows) {
            Self::new("cmd", "/C")
        } else {
            Self::new("sh", "-c")
        }
    }

    /// Look up a shell by name, as written in config.
    ///
    /// The name is matched without its `.exe` suffix and case-insensitively,
    /// because `cmd`, `cmd.exe` and `CMD.EXE` are the same request and refusing
    /// two of them would be a puzzle rather than a policy.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        let stem = name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(name)
            .trim_end_matches(".exe")
            .trim_end_matches(".EXE");
        KNOWN_SHELLS
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(stem))
            .map(|(known, flag)| Self::new(known, flag))
    }

    /// The shells [`ShellSpec::from_name`] accepts, for an error message.
    pub fn known_names() -> Vec<&'static str> {
        KNOWN_SHELLS.iter().map(|(name, _)| *name).collect()
    }

    fn new(program: &str, flag: &str) -> Self {
        Self {
            flag: flag.to_string(),
            program: program.to_string(),
        }
    }

    /// The argv for running `command` through this shell.
    pub fn argv(&self, command: &str) -> Vec<String> {
        vec![self.flag.clone(), command.to_string()]
    }
}

impl Default for ShellSpec {
    fn default() -> Self {
        Self::platform_default()
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_shell_the_platform_guarantees() {
        // The one that is always installed, so a fresh machine works with no
        // config at all.
        let spec = ShellSpec::platform_default();
        if cfg!(windows) {
            assert_eq!(spec.program, "cmd");
            assert_eq!(spec.flag, "/C");
        } else {
            assert_eq!(spec.program, "sh");
            assert_eq!(spec.flag, "-c");
        }
    }

    #[test]
    fn each_shell_gets_its_own_command_flag() {
        // The bug this table prevents: invoking PowerShell with `-c` (which it
        // reads as an abbreviation of `-Command` only by luck) or `cmd` with
        // `-c` (which it does not understand at all, so every command fails).
        assert_eq!(ShellSpec::from_name("cmd").unwrap().flag, "/C");
        assert_eq!(ShellSpec::from_name("pwsh").unwrap().flag, "-Command");
        assert_eq!(ShellSpec::from_name("powershell").unwrap().flag, "-Command");
        assert_eq!(ShellSpec::from_name("nu").unwrap().flag, "-c");
        assert_eq!(ShellSpec::from_name("bash").unwrap().flag, "-c");
    }

    #[test]
    fn a_name_is_matched_without_its_extension_or_case() {
        // `cmd`, `cmd.exe` and `CMD.EXE` are one request; refusing two of them
        // would be a puzzle rather than a policy.
        let expected = ShellSpec::from_name("cmd").unwrap();
        for written in ["cmd.exe", "CMD.EXE", "Cmd", "  cmd  "] {
            assert_eq!(ShellSpec::from_name(written).as_ref(), Some(&expected));
        }
    }

    #[test]
    fn a_path_is_reduced_to_the_shell_it_names() {
        // A user who writes an absolute path means that shell's *flag*; the
        // program the policy sees stays the bare name, so a grant reads as
        // "run pwsh" rather than embedding one machine's install location.
        assert_eq!(
            ShellSpec::from_name(r"C:\Program Files\PowerShell\7\pwsh.exe")
                .unwrap()
                .program,
            "pwsh"
        );
        assert_eq!(ShellSpec::from_name("/bin/bash").unwrap().program, "bash");
    }

    #[test]
    fn an_unknown_shell_is_refused_rather_than_guessed() {
        // Guessing a flag produces a shell that starts and then misreads every
        // command, which is far harder to diagnose than "I do not know that
        // shell".
        assert!(ShellSpec::from_name("my-shell").is_none());
        assert!(ShellSpec::known_names().contains(&"nu"));
    }

    #[test]
    fn argv_puts_the_command_in_one_argument() {
        // The whole command line is a single argv element. Splitting it would
        // hand the shell's own quoting rules to the argv builder, and the
        // policy would see a different program than the one that runs.
        let argv = ShellSpec::from_name("sh").unwrap().argv("echo a b; echo c");
        assert_eq!(argv, vec!["-c", "echo a b; echo c"]);
    }
}
