use async_trait::async_trait;
use serde_json::{json, Value};

use super::Tool;
use crate::error::AppError;

/// Lets the agent maintain a structured, user-visible to-do list for the current
/// task. The actual state lives on the session and is rendered as a live
/// checklist in the client (see `apply_todo_write` in the agent runtime); this
/// tool exists so the model can call it by name with a well-defined schema.
pub struct TodoWrite;

impl TodoWrite {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TodoWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &'static str {
        "todo_write"
    }

    fn description(&self) -> &'static str {
        "Create and update a structured to-do list for the current task so the user can track progress. \
Call it once up front to lay out the subtasks, then call it again after each step to update statuses. \
Use `merge: false` to replace the whole list (e.g. when first creating it) and `merge: true` to update \
existing items by id without resending the rest. Keep exactly one item `in_progress` at a time and mark \
items `completed` as soon as they are done. Use this for any large or multi-step task; skip it for trivial \
one- or two-step requests."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "merge": {
                    "type": "boolean",
                    "description": "If true, update existing items by id and append new ones. If false (default), replace the entire list."
                },
                "todos": {
                    "type": "array",
                    "description": "The to-do items.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable identifier for the item, used to update it later."
                            },
                            "content": {
                                "type": "string",
                                "description": "Short description of the subtask."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": "Current status of the item."
                            }
                        },
                        "required": ["id", "content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, _input: Value) -> Result<Value, AppError> {
        // The runtime intercepts `todo_write` and applies it against session
        // state (see `apply_todo_write`). This fallback only runs if it is ever
        // invoked without that interception.
        Ok(json!({ "ok": true }))
    }
}
