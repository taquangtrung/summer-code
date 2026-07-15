//! The `plan` tool: record and manage step-by-step task execution plans.

use async_trait::async_trait;
use journal::Outcome;
use sandbox::ToolCx;
use serde_json::Value;

use crate::tool::{Tool, ToolSpec};

const PLAN_SCHEMA: &str = include_str!("../tools/plan.json");

// ========================================================================
// Data Structures
// ========================================================================

pub struct PlanTool {
    spec: ToolSpec,
}

// ========================================================================
// PlanTool Implementations
// ========================================================================

impl PlanTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(PLAN_SCHEMA),
        }
    }
}

impl Default for PlanTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for PlanTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(steps_val) = args.get("steps").and_then(Value::as_array) else {
            return Outcome::Err {
                message: "plan tool requires a `steps` array".into(),
            };
        };

        let steps: Vec<String> = steps_val
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();

        let active_step = args.get("active_step").and_then(Value::as_u64).unwrap_or(1) as usize;

        let status = args
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("in_progress");

        let mut out = format!("### Active Task Plan ({status})\n\n");
        for (i, step) in steps.iter().enumerate() {
            let num = i + 1;
            let marker = if num == active_step { "👉" } else { "  " };
            out.push_str(&format!("{marker} Step {num}: {step}\n"));
        }

        let _ = cx;
        Outcome::Ok { content: out }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_plan_tool_spec() {
        let tool = PlanTool::new();
        assert_eq!(tool.spec().name, "plan");
    }
}
