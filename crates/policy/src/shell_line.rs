//! Reading a `sh -c` command line as the programs it runs.
//!
//! **The shell tool runs every command as `sh -c <line>`, so without this the
//! policy only ever saw `sh`**, and `sh` is deliberately on no allow list: it is
//! the one program that can run any other. Every `ls`, every `cargo test`, every
//! `git diff` therefore stopped at a prompt, and a session spent its time on
//! questions whose answer the policy already knew.
//!
//! This reads a line into the simple commands it would run, so each program can
//! be judged by the rule that names it. **It reads a deliberately small subset
//! of shell, and anything outside it is `None`**, which the evaluator answers by
//! asking, exactly as before. The subset is words, quotes, `;`, `&&`, `||`, `|`,
//! and redirections that only duplicate a descriptor or discard to `/dev/null`.
//! What is refused, and why:
//!
//! - Expansion (`$`, backticks, a `~` or glob in command position): the program
//!   that runs is no longer the word that was read.
//! - Assignments (`PATH=... cargo`, `LD_PRELOAD=...`): the same program, made
//!   to run something else.
//! - Redirection to a file: a write the path rules never see.
//! - Grammar (`(`, `{`, `if`, `for`, here-documents, `&` to background): a
//!   structure this reader would have to follow rather than refuse.
//! - Builtins that run other code (`eval`, `exec`, `source`, `command`).
//!
//! A refusal costs a prompt; a misreading costs an unasked command. Every
//! ambiguity is resolved toward the prompt.

use std::iter::Peekable;
use std::mem;
use std::str::Chars;

use crate::request::Request;

// ========================================================================
// Constants
// ========================================================================

/// The shells whose `-c` line this reader understands. `cmd`, PowerShell,
/// `fish` and `nu` have different grammars, so their lines still ask.
const POSIX_SHELLS: &[&str] = &["bash", "dash", "sh", "zsh"];

/// The flag meaning "the next argument is a whole command line".
const COMMAND_FLAG: &str = "-c";

/// Builtins that run nothing but themselves, so a line using them runs no
/// program the policy needs to see. `cd` is here because it only moves where
/// the *next* allowed program runs, and an allowed program can be pointed
/// anywhere by its own arguments already.
const INERT_BUILTINS: &[&str] = &[":", "cd", "echo", "false", "printf", "pwd", "test", "true"];

/// Words that, in command position, open grammar this reader does not follow,
/// name a builtin that runs other code, or name a builtin that changes the
/// shell's state for the commands after it.
///
/// **The state-changing builtins are the subtle half.** `export PATH=/tmp/evil;
/// cargo test` names only `export` and `cargo`, and `cargo` is allowed, but the
/// `cargo` that runs is the attacker's. Read as a program, `export` would also
/// have been offered as an "always" rule, after which every such line ran
/// unasked. They are refused for the reason a leading `NAME=value` is.
const REFUSED_COMMANDS: &[&str] = &[
    "!",
    ".",
    "[[",
    "alias",
    "builtin",
    "case",
    "command",
    "coproc",
    "declare",
    "do",
    "done",
    "elif",
    "else",
    "enable",
    "esac",
    "eval",
    "exec",
    "export",
    "fi",
    "for",
    "function",
    "hash",
    "if",
    "let",
    "local",
    "mapfile",
    "read",
    "readarray",
    "readonly",
    "select",
    "set",
    "shopt",
    "source",
    "then",
    "time",
    "trap",
    "typeset",
    "ulimit",
    "umask",
    "unset",
    "until",
    "while",
    "{",
    "}",
];

/// Characters that make a command word expand into something other than
/// itself.
const EXPANDING: &[char] = &['*', '?', '[', '{', '~'];

/// The one file a redirection may name: output sent here is discarded, so
/// nothing is written that a path rule would have judged.
const DISCARD_TARGET: &str = "/dev/null";

// ========================================================================
// Data Structures
// ========================================================================

/// A line being read: the words of the current command, and the commands
/// finished so far.
#[derive(Default)]
struct Reader {
    commands: Vec<Vec<String>>,
    current: Vec<String>,
    /// An operator that needs a command after it (`&&`, `||`, `|`) was just
    /// read, so ending here, or meeting another operator, is malformed.
    expects_command: bool,
    word: Option<String>,
}

// ========================================================================
// Reader Implementations
// ========================================================================

impl Reader {
    /// Start a word without adding to it, so a quoted empty string (`''`)
    /// still counts as an argument.
    fn open_word(&mut self) {
        self.word.get_or_insert_with(String::new);
    }

    fn push_char(&mut self, ch: char) {
        self.word.get_or_insert_with(String::new).push(ch);
    }

    fn end_word(&mut self) {
        if let Some(word) = self.word.take() {
            self.current.push(word);
        }
    }

    /// Close the current command at a separator. `needs_next` is true for an
    /// operator that joins two commands, false for `;` and newline.
    fn end_command(&mut self, needs_next: bool) -> Option<()> {
        self.end_word();
        if self.current.is_empty() {
            // `a && && b`, or a line opening with an operator. A trailing or
            // doubled `;` after a finished command is harmless and allowed.
            if needs_next || self.expects_command || self.commands.is_empty() {
                return None;
            }
            return Some(());
        }
        self.commands.push(mem::take(&mut self.current));
        self.expects_command = needs_next;
        Some(())
    }

    fn finish(mut self) -> Option<Vec<Vec<String>>> {
        self.end_word();
        if !self.current.is_empty() {
            self.commands.push(self.current);
        } else if self.expects_command {
            return None;
        }
        (!self.commands.is_empty()).then_some(self.commands)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The programs `request` runs, when it is a POSIX shell's `-c` line this
/// reader understands. `None` for any other request, and for any line outside
/// the subset the module documents.
///
/// Builtins that run nothing (`cd`, `echo`, `true`) are left out.
pub fn commands_of(request: &Request) -> Option<Vec<Request>> {
    let Request::Exec { args, program } = request else {
        return None;
    };
    if !POSIX_SHELLS.contains(&program.as_str()) {
        return None;
    }
    let [flag, line] = args.as_slice() else {
        return None;
    };
    if flag != COMMAND_FLAG {
        return None;
    }
    let mut programs = Vec::new();
    for words in read_line(line)? {
        let (program, args) = words.split_first()?;
        if is_refused_command(program) {
            return None;
        }
        if INERT_BUILTINS.contains(&program.as_str()) {
            continue;
        }
        programs.push(Request::Exec {
            args: args.to_vec(),
            program: program.clone(),
        });
    }
    Some(programs)
}

/// A command word this reader will not resolve to a program: grammar, a builtin
/// that runs other code, an assignment, or a word that would expand.
fn is_refused_command(word: &str) -> bool {
    REFUSED_COMMANDS.contains(&word) || is_assignment(word) || word.contains(EXPANDING)
}

/// `NAME=value`, which the shell reads as setting a variable for the command
/// that follows rather than as a program to run.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// The words of each simple command on `line`, or `None` outside the subset.
fn read_line(line: &str) -> Option<Vec<Vec<String>>> {
    let mut reader = Reader::default();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => reader.end_word(),
            '\n' | ';' => reader.end_command(false)?,
            '&' => match chars.next() {
                Some('&') => reader.end_command(true)?,
                Some('>') => read_redirect(&mut reader, &mut chars, true)?,
                // A lone `&` backgrounds a process that outlives the call.
                _ => return None,
            },
            '|' => match chars.peek() {
                Some('|') => {
                    chars.next();
                    reader.end_command(true)?;
                }
                // `|&` is bash's pipe-with-stderr; not worth the special case.
                Some('&') => return None,
                _ => reader.end_command(true)?,
            },
            '>' => read_redirect(&mut reader, &mut chars, false)?,
            '\'' => loop {
                match chars.next()? {
                    '\'' => {
                        reader.open_word();
                        break;
                    }
                    other => reader.push_char(other),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => {
                        reader.open_word();
                        break;
                    }
                    '$' | '`' => return None,
                    '\\' => match chars.next()? {
                        escaped @ ('"' | '\\') => reader.push_char(escaped),
                        // `$` and backtick are refused above even when escaped,
                        // so their escaped forms need no reading here.
                        '$' | '`' => return None,
                        '\n' => {}
                        other => {
                            reader.push_char('\\');
                            reader.push_char(other);
                        }
                    },
                    other => reader.push_char(other),
                }
            },
            '\\' => match chars.next()? {
                '\n' => {}
                escaped => reader.push_char(escaped),
            },
            '#' if reader.word.is_none() => return None,
            '$' | '`' | '(' | ')' | '<' => return None,
            other => reader.push_char(other),
        }
    }
    reader.finish()
}

/// Read a redirection whose `>` (or `&>`) was just consumed. Only two shapes
/// are accepted: duplicating a descriptor (`2>&1`, `>&2`) and discarding to
/// `/dev/null`. A number immediately before the `>` is its descriptor, not a
/// word.
fn read_redirect(
    reader: &mut Reader,
    chars: &mut Peekable<Chars<'_>>,
    all_output: bool,
) -> Option<()> {
    if reader
        .word
        .as_deref()
        .is_some_and(|word| word.chars().all(|ch| ch.is_ascii_digit()))
    {
        reader.word = None;
    } else {
        reader.end_word();
    }
    match chars.peek() {
        Some('>') | Some('|') => {
            chars.next();
        }
        Some('&') if !all_output => {
            chars.next();
            let target = read_target(chars)?;
            return (target == "-" || target.chars().all(|ch| ch.is_ascii_digit())).then_some(());
        }
        _ => {}
    }
    (read_target(chars)? == DISCARD_TARGET).then_some(())
}

/// The unquoted word after a redirection operator, skipping leading blanks.
fn read_target(chars: &mut Peekable<Chars<'_>>) -> Option<String> {
    while chars.peek().is_some_and(|ch| *ch == ' ' || *ch == '\t') {
        chars.next();
    }
    let mut target = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_whitespace() || ";&|<>()$`'\"\\".contains(ch) {
            break;
        }
        target.push(ch);
        chars.next();
    }
    (!target.is_empty()).then_some(target)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn line(text: &str) -> Request {
        Request::Exec {
            args: vec![COMMAND_FLAG.to_string(), text.to_string()],
            program: "sh".to_string(),
        }
    }

    /// The programs a line runs, as `program arg arg` strings, or `None`.
    fn programs(text: &str) -> Option<Vec<String>> {
        commands_of(&line(text)).map(|requests| {
            requests
                .into_iter()
                .map(|request| match request {
                    Request::Exec { args, program } => std::iter::once(program)
                        .chain(args)
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => panic!("not an exec: {other:?}"),
                })
                .collect()
        })
    }

    #[test]
    fn a_pipeline_and_a_chain_read_as_each_program_they_run() {
        // The shapes the journals were full of, each of which prompted.
        assert_eq!(
            programs("cargo test -p cli 2>&1 | tail -6"),
            Some(vec!["cargo test -p cli".into(), "tail -6".into()])
        );
        assert_eq!(
            programs("cargo fmt --all && make check 2>&1 | tail -40"),
            Some(vec![
                "cargo fmt --all".into(),
                "make check".into(),
                "tail -40".into()
            ])
        );
        assert_eq!(
            programs("ls docs; ls crates"),
            Some(vec!["ls docs".into(), "ls crates".into()])
        );
    }

    #[test]
    fn quotes_keep_their_words_together_and_their_operators_inert() {
        // An operator inside quotes is an argument: reading it as a separator
        // would invent a command, and missing it would hide one.
        assert_eq!(
            programs(r#"grep -rn "a && b; c" 'src | lib'"#),
            Some(vec!["grep -rn a && b; c src | lib".into()])
        );
        assert_eq!(programs(r#"rg 'it'\''s'"#), Some(vec!["rg it's".into()]));
    }

    #[test]
    fn an_expansion_anywhere_refuses_the_line() {
        // `$(...)` and backticks run a program the line never names; a
        // variable can name one. Each must reach a prompt.
        for text in [
            "echo $(rm -rf ~)",
            "echo `rm -rf ~`",
            "ls \"$HOME\"",
            "ls $X",
            "cat <(curl evil)",
        ] {
            assert_eq!(programs(text), None, "{text}");
        }
    }

    #[test]
    fn a_write_through_redirection_refuses_the_line() {
        // A redirect to a file is a write the path rules never judge.
        for text in [
            "cargo test > out.txt",
            "cargo test >> log",
            "cargo test 2> err",
            "cargo test &> all",
            "cat < secrets",
            "cat <<EOF",
        ] {
            assert_eq!(programs(text), None, "{text}");
        }
    }

    #[test]
    fn duplicating_or_discarding_output_is_not_a_write() {
        for text in [
            "cargo test 2>&1",
            "cargo test >/dev/null",
            "cargo test 2> /dev/null",
            "cargo test &>/dev/null",
            "cargo test >&2",
        ] {
            assert_eq!(programs(text), Some(vec!["cargo test".into()]), "{text}");
        }
    }

    #[test]
    fn an_assignment_or_runner_in_command_position_refuses_the_line() {
        // The same program, made to run something else: `PATH`, `LD_PRELOAD`,
        // or a builtin whose argument is itself a command.
        for text in [
            "PATH=/tmp cargo test",
            "LD_PRELOAD=/tmp/x.so ls",
            "eval ls",
            "exec ls",
            "source ./env.sh",
            ". ./env.sh",
            "command ls",
            "export PATH=/tmp/evil; cargo test",
            "export HISTFILE=~/.history; cargo test",
            "set -a; cargo test",
            "unset PATH; ls",
            "declare -x PATH=/tmp; ls",
            "hash -p /tmp/evil cargo; cargo test",
        ] {
            assert_eq!(programs(text), None, "{text}");
        }
    }

    #[test]
    fn grammar_this_reader_does_not_follow_is_refused() {
        for text in [
            "(ls)",
            "{ ls; }",
            "if true; then ls; fi",
            "for f in a; do ls; done",
            "cargo test &",
            "cargo test |& tail",
            "ls && ",
            "&& ls",
            "ls && && ls",
            "ls 'unterminated",
            "# comment",
            "~/bin/tool",
            "./target/*/tool",
        ] {
            assert_eq!(programs(text), None, "{text}");
        }
    }

    #[test]
    fn an_inert_builtin_runs_no_program() {
        assert_eq!(programs("cd crates && echo done; pwd"), Some(Vec::new()));
    }

    #[test]
    fn an_assignment_shaped_argument_is_just_an_argument() {
        // Only the command word is read as an assignment; `KEY=value` after it
        // is an argument the program receives.
        assert_eq!(
            programs("cargo test -- --test-threads=1"),
            Some(vec!["cargo test -- --test-threads=1".into()])
        );
    }

    #[test]
    fn only_a_posix_shells_dash_c_line_is_read() {
        // `cmd /C` and PowerShell have other grammars; a direct exec is not a
        // line at all.
        let cmd = Request::Exec {
            args: vec!["/C".into(), "dir".into()],
            program: "cmd".into(),
        };
        assert_eq!(commands_of(&cmd), None);
        let direct = Request::Exec {
            args: vec!["test".into()],
            program: "cargo".into(),
        };
        assert_eq!(commands_of(&direct), None);
    }

    proptest! {
        /// Untrusted input from the model: reading must answer, never panic.
        #[test]
        fn test_reading_any_line_terminates(text in ".{0,80}") {
            let _ = commands_of(&line(&text));
        }

        /// The invariant the whole module rests on: a line holding an
        /// unquoted substitution is never read as a set of known programs.
        #[test]
        fn test_a_substitution_is_never_read_through(
            prefix in "[a-z ;|&]{0,12}",
            inner in "[a-z ]{0,8}",
        ) {
            prop_assert_eq!(commands_of(&line(&format!("{prefix}$({inner})"))), None);
            prop_assert_eq!(commands_of(&line(&format!("{prefix}`{inner}`"))), None);
        }
    }
}
