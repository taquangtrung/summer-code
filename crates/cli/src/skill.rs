//! Skills: task-specialized instructions loaded once at startup from
//! `~/.summer/skills/<name>/SKILL.md`. Each `SKILL.md` has a `name`/
//! `description` frontmatter and an instruction body. The catalog is appended to
//! the system prompt so the model knows what is available, and a `skill` tool
//! (added by the frontend) loads a body on demand.
//!
//! Bodies are read here, at startup and outside the sandbox, because `~/.summer`
//! is beyond the repo root a sandboxed worker is confined to. The `skill` tool
//! holds the registry in memory and never reads the filesystem at dispatch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use tools::SkillLookup;

use crate::discovery::LANGUAGES;

// ========================================================================
// Constants
// ========================================================================

/// The longest description the catalog shows for one skill.
const MAX_CATALOG_DESCRIPTION: usize = 120;

// ========================================================================
// Data Structures
// ========================================================================

/// The loaded skills, keyed by name. Built once at startup; immutable after.
pub struct Registry {
    skills: BTreeMap<String, Skill>,
}

/// One loaded skill.
pub struct Skill {
    pub name: String,
    pub description: String,
    body: String,
}

// ========================================================================
// Implementations
// ========================================================================

impl Registry {
    /// Load every `SKILL.md` under `dir` (one per subdirectory), keyed by name.
    /// A missing `dir` is an empty registry, not an error.
    pub fn load(dir: &Path) -> Self {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();
        for skill in scan(dir) {
            skills.insert(skill.name.clone(), skill);
        }
        Self { skills }
    }

    /// No skills: what an unset `HOME` yields, since there is then no user
    /// config directory to read them from.
    pub fn empty() -> Self {
        Self {
            skills: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// The catalog appended to the system prompt: each skill's name and the
    /// first sentence of its description, plus the instruction to call the
    /// `skill` tool.
    ///
    /// **Only the skills this project can use.** A skill named for a language
    /// (`go-testing`, by the [`LANGUAGES`] prefixes) is listed only when the
    /// project has files in that language; one named for nothing in particular
    /// is always listed. The catalog rides on every request of the session, and
    /// a library of 167 skills put about 4,400 tokens in front of each, most of
    /// them for languages the repository never touches, and every one inviting
    /// a `skill` call. Left-out skills still load by name: only the listing is
    /// filtered, and it says how many it left out.
    pub fn catalog(&self, languages: &BTreeSet<&str>) -> String {
        let mut out = String::from("<available_skills>\n");
        let mut hidden = 0;
        for skill in self.skills.values() {
            if !applies_to(&skill.name, languages) {
                hidden += 1;
                continue;
            }
            out.push_str(&format!("  {}", skill.name));
            let description = first_sentence(&skill.description);
            if !description.is_empty() {
                out.push_str(&format!(": {description}"));
            }
            out.push('\n');
        }
        out.push_str("</available_skills>\n");
        if hidden > 0 {
            out.push_str(&format!(
                "({hidden} more skills for languages this project does not use are not listed.)\n"
            ));
        }
        out.push_str(
            "To follow a skill, call the `skill` tool with its name and apply the instructions \
             it returns.",
        );
        out
    }
}

impl SkillLookup for Registry {
    fn lookup(&self, name: &str) -> Option<String> {
        self.skills.get(name).map(|skill| skill.body.clone())
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Whether a skill named `name` belongs in the catalog of a project using
/// `languages`: yes, unless its name starts with a language the project lacks.
fn applies_to(name: &str, languages: &BTreeSet<&str>) -> bool {
    let prefix = name.split('-').next().unwrap_or_default();
    let names_a_language = LANGUAGES.iter().any(|(language, _)| *language == prefix);
    !names_a_language || languages.contains(prefix)
}

/// The first sentence of `description`, cut to [`MAX_CATALOG_DESCRIPTION`]
/// characters.
fn first_sentence(description: &str) -> String {
    let sentence = description
        .split_once(". ")
        .map_or(description, |(first, _)| first)
        .trim()
        .trim_end_matches('.');
    if sentence.chars().count() <= MAX_CATALOG_DESCRIPTION {
        return sentence.to_string();
    }
    let cut: String = sentence.chars().take(MAX_CATALOG_DESCRIPTION).collect();
    format!("{cut}…")
}

/// Scan `dir` for `<name>/SKILL.md`, parsing each into a [`Skill`].
///
/// **Sorted by path, because `read_dir` order is not a guarantee.** Two skill
/// directories declaring the same `name:` collide, and [`Registry::load`] keys by
/// name with last-insert-wins. Left in `read_dir` order, which skill wins is
/// whatever the filesystem happened to hand back: ext4 returns hash order, tmpfs
/// returns insertion order, and the same tree resolves differently on two
/// machines. Sorting makes the winner a property of the names, so a collision is
/// at least reproducible and reviewable.
///
/// A `read_dir` failure other than "no such directory" is reported rather than
/// swallowed: an unreadable skills directory yields an empty registry that looks
/// exactly like an empty one, and the whole feature then no-ops with no
/// diagnostic.
fn scan(dir: &Path) -> Vec<Skill> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "skills could not be listed");
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    let mut skills = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path.join("SKILL.md")) else {
            continue;
        };
        let fallback = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        skills.push(parse_skill(&text, &fallback));
    }
    skills
}

/// Parse a `SKILL.md`: optional `---` frontmatter (`name`, `description`) plus a
/// body. A missing frontmatter uses the fallback name and treats the whole file
/// as the body.
fn parse_skill(text: &str, fallback_name: &str) -> Skill {
    match split_frontmatter(text) {
        Some((name, description, body)) => Skill {
            name: if name.is_empty() {
                fallback_name.to_string()
            } else {
                name
            },
            description,
            body,
        },
        None => Skill {
            name: fallback_name.to_string(),
            description: String::new(),
            body: text.to_string(),
        },
    }
}

/// Split YAML-ish `---`-fenced frontmatter from the body. Returns
/// `(name, description, body)`, or `None` when there is no closing fence. The
/// body is sliced verbatim (a line-collecting split would drop its trailing
/// newline).
fn split_frontmatter(text: &str) -> Option<(String, String, String)> {
    let after_open = text.strip_prefix("---\n")?;
    // The closing fence is a line equal to "---": either `\n---\n` or a trailing
    // `\n---`. Slicing on the byte offsets preserves the body verbatim.
    let (front_end, fence_len) = after_open
        .find("\n---\n")
        .map(|index| (index, "\n---\n".len()))
        .or_else(|| after_open.find("\n---").map(|index| (index, "\n---".len())))?;
    let front = &after_open[..front_end];
    let body = &after_open[front_end + fence_len..];
    let mut name = String::new();
    let mut description = String::new();
    for line in front.lines() {
        if let Some(value) = line.strip_prefix("name:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("description:") {
            description = value.trim().to_string();
        }
    }
    Some((name, description, body.to_string()))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_name_description_and_body() {
        let text =
            "---\nname: rust-testing\ndescription: Write idiomatic tests.\n---\n# Body\nDo X.\n";
        let skill = parse_skill(text, "fallback");
        assert_eq!(skill.name, "rust-testing");
        assert_eq!(skill.description, "Write idiomatic tests.");
        assert_eq!(skill.body, "# Body\nDo X.\n");
    }

    #[test]
    fn a_missing_name_falls_back_to_the_directory() {
        let text = "---\ndescription: No name.\n---\nbody\n";
        let skill = parse_skill(text, "dir-name");
        assert_eq!(skill.name, "dir-name");
        assert_eq!(skill.body, "body\n");
    }

    #[test]
    fn no_frontmatter_uses_the_whole_file_as_the_body() {
        let text = "just a body\nline two\n";
        let skill = parse_skill(text, "plain");
        assert_eq!(skill.name, "plain");
        assert_eq!(skill.description, "");
        assert_eq!(skill.body, text);
    }

    #[test]
    fn an_unclosed_frontmatter_is_treated_as_no_frontmatter() {
        // A stray opening `---` with no close must not eat the file.
        let text = "---\nname: oops\nthis is the body\n";
        let skill = parse_skill(text, "dir");
        assert_eq!(skill.name, "dir");
        assert_eq!(skill.body, text);
    }

    #[test]
    fn catalog_lists_each_skill_and_hints_at_the_tool() {
        let registry = Registry {
            skills: [
                (
                    "b".into(),
                    Skill {
                        name: "b".into(),
                        description: "second".into(),
                        body: String::new(),
                    },
                ),
                (
                    "a".into(),
                    Skill {
                        name: "a".into(),
                        description: String::new(),
                        body: String::new(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let catalog = registry.catalog(&BTreeSet::new());
        assert!(catalog.contains("<available_skills>"));
        assert!(catalog.contains("  a\n"), "no-description skill: {catalog}");
        assert!(catalog.contains("  b: second"));
        assert!(catalog.contains("`skill` tool"));
    }

    #[test]
    fn the_catalog_lists_only_skills_for_languages_the_project_uses() {
        // Every language's skills used to ride on every request, each one an
        // invitation to a `skill` call the project had no use for.
        let skill = |name: &str, description: &str| {
            (
                name.to_string(),
                Skill {
                    name: name.into(),
                    description: description.into(),
                    body: String::new(),
                },
            )
        };
        let registry = Registry {
            skills: [
                skill(
                    "rust-testing",
                    "Write idiomatic tests. With proptest where it fits.",
                ),
                skill("go-testing", "Write Go tests."),
                skill("code-review", "Review a diff."),
            ]
            .into_iter()
            .collect(),
        };
        let catalog = registry.catalog(&BTreeSet::from(["rust"]));
        assert!(
            catalog.contains("  rust-testing: Write idiomatic tests\n"),
            "{catalog}"
        );
        assert!(
            catalog.contains("  code-review: Review a diff"),
            "language-neutral stays"
        );
        assert!(!catalog.contains("go-testing"), "{catalog}");
        assert!(catalog.contains("1 more skills"), "says what it left out");
        assert_eq!(
            registry.lookup("go-testing").as_deref(),
            Some(""),
            "a left-out skill still loads by name"
        );
    }

    #[test]
    fn load_reads_a_subdirectory_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("rust-testing");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-testing\ndescription: tests.\n---\nbody here\n",
        )
        .unwrap();

        let registry = Registry::load(dir.path());
        assert_eq!(registry.skills.len(), 1);
        assert_eq!(
            registry.lookup("rust-testing").as_deref(),
            Some("body here\n"),
            "the lookup the skill tool uses returns the body"
        );
        assert!(
            registry
                .catalog(&BTreeSet::from(["rust"]))
                .contains("rust-testing: tests")
        );
    }

    #[test]
    fn a_name_collision_resolves_by_directory_order_not_readdir_order() {
        // Two directories declaring the same `name:`. `load` keys by name with
        // last-insert-wins, so the winner is the last *path* in sorted order --
        // `y`, never `x`, on every filesystem. Without the sort in `scan` this
        // assertion holds or fails depending on what `read_dir` happened to
        // return, which is the bug: the same tree resolved differently on two
        // machines.
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in [("y", "two"), ("x", "one")] {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: dup\n---\n{body}\n"),
            )
            .unwrap();
        }

        let registry = Registry::load(dir.path());
        assert_eq!(registry.skills.len(), 1, "one entry for the colliding name");
        assert!(
            registry.lookup("dup").unwrap().contains("two"),
            "the last path in sorted order must win"
        );
    }

    #[test]
    fn scan_orders_skills_by_path() {
        // The property the collision rule rests on. Directories are created in
        // reverse order so a filesystem preserving insertion order would fail
        // this without the sort.
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["c", "b", "a"] {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\n---\n"),
            )
            .unwrap();
        }

        let names: Vec<String> = scan(dir.path()).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }
}
