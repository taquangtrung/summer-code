//! The `skill` tool: load a skill's instruction body by name. The bodies live in
//! a registry the frontend builds at startup, so this tool holds an
//! [`Arc<dyn SkillLookup>`] and never touches the filesystem at dispatch — a
//! sandboxed worker cannot read `~/.summer`, where user skills live.

use std::sync::Arc;

use async_trait::async_trait;
use journal::Outcome;
use sandbox::ToolCx;
use serde_json::Value;

use crate::tool::{Tool, ToolSpec};

const SKILL_SCHEMA: &str = include_str!("../tools/skill.json");

/// Resolve a skill name to its instruction body. The frontend's registry
/// implements this; the tool depends on the lookup, not the registry type, so
/// `tools` needs no knowledge of where skills are stored.
pub trait SkillLookup: Send + Sync {
    fn lookup(&self, name: &str) -> Option<String>;
}

// ========================================================================
// Data Structures
// ========================================================================

/// Load a skill's full instructions by name.
pub struct Skill {
    spec: ToolSpec,
    skills: Arc<dyn SkillLookup>,
}

// ========================================================================
// Skill Implementations
// ========================================================================

impl Skill {
    pub fn new(skills: Arc<dyn SkillLookup>) -> Self {
        Self {
            spec: ToolSpec::load(SKILL_SCHEMA),
            skills,
        }
    }
}

// ========================================================================
// Tool Implementations
// ========================================================================

#[async_trait]
impl Tool for Skill {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> Outcome {
        resolve(&args, self.skills.as_ref())
    }
}

/// The pure core of [`Skill::call`]: look up the named skill and map the result
/// to an [`Outcome`]. Pulled out so it tests without a [`ToolCx`].
fn resolve(args: &Value, skills: &dyn SkillLookup) -> Outcome {
    let Some(name) = args.get("name").and_then(Value::as_str) else {
        return Outcome::Err {
            message: "skill needs a `name`".into(),
        };
    };
    match skills.lookup(name) {
        Some(body) => Outcome::Ok { content: body },
        None => Outcome::Err {
            message: format!("unknown skill: {name}"),
        },
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use serde_json::json;

    struct Fixed(HashMap<String, String>);

    impl SkillLookup for Fixed {
        fn lookup(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    #[test]
    fn resolve_returns_the_body_for_a_known_skill() {
        let mut map = HashMap::new();
        map.insert("rust-testing".into(), "body".into());
        let outcome = resolve(&json!({"name": "rust-testing"}), &Fixed(map));
        assert!(
            matches!(outcome, Outcome::Ok { ref content } if content.as_str() == "body"),
            "got {outcome:?}"
        );
    }

    #[test]
    fn resolve_errors_on_an_unknown_skill() {
        let outcome = resolve(&json!({"name": "nope"}), &Fixed(HashMap::new()));
        assert!(matches!(outcome, Outcome::Err { .. }));
    }

    #[test]
    fn resolve_errors_without_a_name() {
        let outcome = resolve(&json!({}), &Fixed(HashMap::new()));
        assert!(matches!(outcome, Outcome::Err { .. }));
    }
}
