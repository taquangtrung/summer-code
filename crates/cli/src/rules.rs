//! Where the agent's standing instructions come from.
//!
//! Two tiers, both the user's own words: `~/.summer/rules` for instructions that
//! follow them between repositories, and `.summer/rules`, `AGENTS.md` or
//! `CLAUDE.md` in the repository for instructions that belong to the project.
//! Both go into the system prompt.
//!
//! Nested directories may carry their own. Those are found per edit rather than
//! at startup, by `tools::subtree_rules`, because they live inside the
//! repository and are read while the agent is running: that makes them
//! agent-driven I/O and subject to the policy, which this module's startup reads
//! are not.
//!
//! **These are the user's rules and only the user's.** This module briefly also
//! shipped a `load_language_rules` returning a canned Rust/TypeScript/Python
//! style guide compiled into the binary. It was never called, and it should not
//! have been: opinions the user did not write do not belong in their agent's
//! prompt, and the two tiers above are exactly the mechanism for holding the
//! ones they did.

use std::fs;
use std::path::Path;

use crate::policy;

// ========================================================================
// Functions
// ========================================================================

/// Load user and project-specific rules from `~/.summer/rules`, `./.summer/rules`, `./AGENTS.md`, or `./CLAUDE.md`.
pub fn load_project_rules(cwd: &Path) -> Option<(String, String)> {
    let mut rules = Vec::new();
    let mut names = Vec::new();

    // Global user rules (~/.summer/rules or ~/.summer/rules.md)
    if let Some(config_dir) = policy::config_dir() {
        for (filename, name) in [
            ("rules", "~/.summer/rules"),
            ("rules.md", "~/.summer/rules.md"),
        ] {
            let path = config_dir.join(filename);
            if let Ok(content) = fs::read_to_string(&path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    rules.push(trimmed.to_string());
                    names.push(name.to_string());
                    break;
                }
            }
        }
    }

    // Project-level rules (.summer/rules, .summer/rules.md, AGENTS.md, CLAUDE.md)
    let candidates = [
        (cwd.join(".summer").join("rules"), ".summer/rules"),
        (cwd.join(".summer").join("rules.md"), ".summer/rules.md"),
        (cwd.join("AGENTS.md"), "AGENTS.md"),
        (cwd.join("CLAUDE.md"), "CLAUDE.md"),
    ];
    for (path, name) in &candidates {
        if let Ok(content) = fs::read_to_string(path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                rules.push(trimmed.to_string());
                names.push(name.to_string());
                break;
            }
        }
    }

    if rules.is_empty() {
        None
    } else {
        let joined_names = names.join(", ");
        let mut content = format!("\n\n<user_rules>\n# Active Rules ({joined_names})\n");
        for r in rules {
            content.push_str(&r);
            content.push_str("\n\n");
        }
        content.push_str("</user_rules>");
        Some((joined_names, content))
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_rules_are_found_in_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("AGENTS.md"), "Project Rule from AGENTS.md").unwrap();

        let (names, content) = load_project_rules(root).expect("AGENTS.md is a rules source");
        assert_eq!(
            names, "AGENTS.md",
            "the source is named so a user can find where a rule came from"
        );
        assert!(content.contains("Project Rule from AGENTS.md"), "{content}");
        assert!(content.contains("<user_rules>"), "{content}");
    }

    #[test]
    fn the_first_matching_project_file_wins() {
        // The candidates are ordered, and both being read would put the same
        // instructions in the prompt twice.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".summer")).unwrap();
        fs::write(root.join(".summer/rules"), "from dot-summer").unwrap();
        fs::write(root.join("AGENTS.md"), "from agents-md").unwrap();

        let (names, content) = load_project_rules(root).expect("a rules source");
        assert_eq!(names, ".summer/rules");
        assert!(!content.contains("from agents-md"), "{content}");
    }

    #[test]
    fn a_tree_with_no_rules_files_yields_none() {
        // `None` rather than an empty `<user_rules>` block: an empty block is
        // prompt tokens spent saying nothing, on every turn of every session.
        let temp = tempfile::tempdir().unwrap();
        assert!(load_project_rules(temp.path()).is_none());
    }
}
