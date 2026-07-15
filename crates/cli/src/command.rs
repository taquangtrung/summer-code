//! User-defined slash commands: a saved prompt behind a name.
//!
//! A command is a Markdown file. `/review` runs `commands/review.md`'s body as
//! the turn's prompt, with `$ARGUMENTS` replaced by whatever followed the name.
//! That is the whole feature, and the smallness is the point: the built-in
//! commands in `tui::slash::COMMANDS` each do something to the session
//! (change a model, show a policy, resume), which is why they are a fixed enum.
//! A user command does not need to reach the session at all. It needs to reach
//! the *model*, and a prompt is already the interface for that.
//!
//! **Both tiers load, unlike hooks, and the difference is who pulls the
//! trigger.** A hook from repo content runs on `git clone` plus one session,
//! before the user has read anything. A project command runs when the user types
//! its name, having seen it in the menu. It is still repo-authored text going
//! into a prompt, which is why the source is [`Source::Project`] and why the
//! completion menu says so: the user should know whose words they are about to
//! send. On a name collision the user's own file wins, so a repository cannot
//! quietly redefine a command someone relies on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ========================================================================
// Constants
// ========================================================================

/// Where the user's own commands live, under the summer config directory.
pub const USER_COMMANDS: &str = "commands";

/// Where a repository's commands live, relative to the root.
pub const PROJECT_COMMANDS: &str = ".summer/commands";

/// The placeholder replaced by whatever followed the command name.
const ARGUMENTS: &str = "$ARGUMENTS";

/// The extension a command file must have.
const EXTENSION: &str = "md";

// ========================================================================
// Data Structures
// ========================================================================

/// Every loaded command, keyed by the name typed after the slash.
#[derive(Default)]
pub struct Registry {
    commands: BTreeMap<String, Command>,
}

/// One loaded command.
pub struct Command {
    body: String,
    pub description: String,
    pub name: String,
    pub source: Source,
}

/// Which tier a command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `.summer/commands` in the repository, and therefore repo-authored text.
    Project,
    /// `~/.summer/commands`, written by the user.
    User,
}

// ========================================================================
// Registry Implementations
// ========================================================================

impl Registry {
    /// Load the project tier then the user tier, so a user file wins a name
    /// collision.
    pub fn load(user_dir: Option<&Path>, project_dir: &Path) -> Self {
        let mut commands = BTreeMap::new();
        for command in scan(project_dir, Source::Project) {
            commands.insert(command.name.clone(), command);
        }
        if let Some(dir) = user_dir {
            for command in scan(dir, Source::User) {
                commands.insert(command.name.clone(), command);
            }
        }
        Self { commands }
    }

    /// The prompt `line` expands to, if it names a command.
    ///
    /// `None` for anything that is not one, so a caller can fall through to the
    /// built-in commands and then to "unknown command" exactly as before.
    pub fn expand(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix('/')?;
        let (name, arguments) = match rest.split_once(char::is_whitespace) {
            Some((name, arguments)) => (name, arguments.trim()),
            None => (rest, ""),
        };
        let command = self.commands.get(name)?;
        Some(command.render(arguments))
    }

    /// Every command whose name starts with `prefix`, as `(name, description)`
    /// pairs for the completion menu.
    pub fn completions(&self, prefix: &str) -> Vec<(String, String)> {
        self.commands
            .values()
            .filter(|command| command.name.starts_with(prefix))
            .map(|command| (command.name.clone(), command.menu_description()))
            .collect()
    }
}

// ========================================================================
// Command Implementations
// ========================================================================

impl Command {
    /// The body with `$ARGUMENTS` substituted.
    ///
    /// A body with no placeholder still gets the arguments, appended: a user who
    /// wrote `/review` as a bare instruction and then types `/review src/lib.rs`
    /// meant the path to be used, and silently discarding it makes the command
    /// look broken in a way nothing reports.
    fn render(&self, arguments: &str) -> String {
        if self.body.contains(ARGUMENTS) {
            return self.body.replace(ARGUMENTS, arguments);
        }
        if arguments.is_empty() {
            return self.body.clone();
        }
        format!("{}\n\n{arguments}", self.body.trim_end())
    }

    /// The description shown in the completion menu, tagged when the command
    /// came from the repository.
    fn menu_description(&self) -> String {
        let text = if self.description.is_empty() {
            "user command"
        } else {
            &self.description
        };
        match self.source {
            Source::Project => format!("{text} (project)"),
            Source::User => text.to_string(),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Read every `<name>.md` directly under `dir`.
///
/// Sorted by path for the reason [`crate::skill`] sorts: `read_dir` order is not
/// a guarantee, and a collision that resolves differently on two machines is
/// worse than either resolution.
fn scan(dir: &Path, source: Source) -> Vec<Command> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "commands could not be listed");
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    let mut commands = Vec::new();
    for path in paths {
        if path.extension().is_none_or(|ext| ext != EXTENSION) {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (description, body) = split_description(&text);
        commands.push(Command {
            body,
            description,
            name,
            source,
        });
    }
    commands
}

/// Split an optional `---`-fenced `description:` from the body.
///
/// The same frontmatter shape `SKILL.md` uses, because a user who has written
/// one should not have to learn a second format. A file with no frontmatter is
/// all body, which is what makes the one-line command file possible.
fn split_description(text: &str) -> (String, String) {
    let Some(after_open) = text.strip_prefix("---\n") else {
        return (String::new(), text.to_string());
    };
    let Some((front_end, fence_len)) = after_open
        .find("\n---\n")
        .map(|index| (index, "\n---\n".len()))
        .or_else(|| after_open.find("\n---").map(|index| (index, "\n---".len())))
    else {
        // An unclosed fence is not frontmatter; treating it as one would eat
        // the file.
        return (String::new(), text.to_string());
    };
    let description = after_open[..front_end]
        .lines()
        .find_map(|line| line.strip_prefix("description:"))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    (description, after_open[front_end + fence_len..].to_string())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn a_command_expands_to_its_body() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "review.md",
            "Review the working tree carefully.\n",
        );
        let registry = Registry::load(None, dir.path());
        assert_eq!(
            registry.expand("/review").as_deref(),
            Some("Review the working tree carefully.\n")
        );
    }

    #[test]
    fn arguments_replace_the_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "fix.md",
            "Fix the bug in $ARGUMENTS and add a test.",
        );
        let registry = Registry::load(None, dir.path());
        assert_eq!(
            registry.expand("/fix src/lib.rs").as_deref(),
            Some("Fix the bug in src/lib.rs and add a test.")
        );
    }

    #[test]
    fn arguments_are_appended_when_the_body_has_no_placeholder() {
        // Dropping them silently makes the command look broken: the user typed
        // a path and nothing anywhere used it or said why.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "review.md", "Review this.\n");
        let registry = Registry::load(None, dir.path());
        assert_eq!(
            registry.expand("/review src/lib.rs").as_deref(),
            Some("Review this.\n\nsrc/lib.rs")
        );
    }

    #[test]
    fn an_unknown_name_is_not_a_command() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "review.md", "body");
        let registry = Registry::load(None, dir.path());
        assert_eq!(registry.expand("/nope"), None);
        assert_eq!(registry.expand("not a command at all"), None);
        assert_eq!(registry.expand("/"), None);
    }

    #[test]
    fn a_user_command_wins_a_collision_with_a_project_one() {
        // The rule that stops a cloned repository from redefining a command the
        // user relies on.
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write(user.path(), "deploy.md", "the user's deploy");
        write(project.path(), "deploy.md", "the repo's deploy");

        let registry = Registry::load(Some(user.path()), project.path());
        assert_eq!(
            registry.expand("/deploy").as_deref(),
            Some("the user's deploy")
        );
    }

    #[test]
    fn a_project_command_is_labelled_as_one_in_the_menu() {
        // The user is about to send repo-authored text to a model. The menu is
        // where they find that out.
        let project = tempfile::tempdir().unwrap();
        write(
            project.path(),
            "build.md",
            "---\ndescription: build it\n---\nbody",
        );
        let registry = Registry::load(None, project.path());
        let completions = registry.completions("");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].0, "build");
        assert!(completions[0].1.contains("(project)"), "{completions:?}");
    }

    #[test]
    fn frontmatter_is_stripped_from_the_prompt() {
        // Sending `description: ...` to the model as part of the instruction is
        // both noise and confusing.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.md",
            "---\ndescription: does x\n---\nDo the thing.\n",
        );
        let registry = Registry::load(None, dir.path());
        assert_eq!(registry.expand("/x").as_deref(), Some("Do the thing.\n"));
    }

    #[test]
    fn a_non_markdown_file_is_not_a_command() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "notes.txt", "not a command");
        write(dir.path(), "README", "also not");
        let registry = Registry::load(None, dir.path());
        assert!(
            registry.completions("").is_empty(),
            "a stray file became a command"
        );
    }

    #[test]
    fn a_missing_directory_is_an_empty_registry() {
        let registry = Registry::load(None, Path::new("/nonexistent/summer/commands"));
        assert!(registry.completions("").is_empty());
    }

    #[test]
    fn completions_filter_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "review.md", "a");
        write(dir.path(), "refactor.md", "b");
        write(dir.path(), "deploy.md", "c");
        let registry = Registry::load(None, dir.path());

        let names: Vec<String> = registry
            .completions("re")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["refactor", "review"]);
    }
}
