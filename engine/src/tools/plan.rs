use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::tools::Tool;

/// Records the agent's task ledger (goal + step checklist). The runtime captures
/// the submitted plan onto the session and re-injects it verbatim every turn, so
/// the agent keeps its plan and place across long runs and context summarization.
pub struct UpdatePlan;

impl UpdatePlan {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for UpdatePlan {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &'static str {
        "update_plan"
    }

    fn description(&self) -> &'static str {
        "Record or update your task plan/ledger: the goal and a checklist of steps with status \
         markers ([ ] todo, [~] in progress, [x] done). Call this FIRST on any multi-step task to \
         lay out the plan, then call it again whenever you finish a step or the plan changes. The \
         latest plan is always kept visible in your context, so use it to track progress and never \
         lose your place."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full, updated plan as a markdown checklist: a one-line goal \
                                    followed by steps, each marked [ ], [~], or [x]."
                }
            },
            "required": ["plan"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let plan = input.get("plan").and_then(Value::as_str).unwrap_or("").trim();
        if plan.is_empty() {
            return Err(AppError::InvalidRequest(
                "update_plan requires a non-empty 'plan'".into(),
            ));
        }
        Ok(json!({
            "ok": true,
            "note": "Plan recorded. It stays visible in your context — keep marking steps [x] as you finish them."
        }))
    }
}
