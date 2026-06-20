use async_trait::async_trait;
use serde_json::{json, Value};

use super::Tool;
use crate::error::AppError;

/// Lets the agent ask the user a focused clarifying question with selectable
/// options. The prompting itself is delegated to the client (see `delegate_ask`
/// in the agent runtime); this tool exists so the model can call it by name and
/// get a well-defined schema. When no interactive client is attached, `execute`
/// returns an empty answer so the agent falls back to asking in plain text.
pub struct AskQuestion;

impl AskQuestion {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskQuestion {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskQuestion {
    fn name(&self) -> &'static str {
        "ask_question"
    }

    fn description(&self) -> &'static str {
        "Ask the user one focused clarifying question and let them pick from options. \
Use this whenever you need the user to make a choice or resolve ambiguity before \
continuing, instead of asking in plain prose. The user's chosen answer is returned to you."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The single, focused question to ask the user."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Suggested answers the user can pick from. The user can also type their own."
                },
                "multiple": {
                    "type": "boolean",
                    "description": "Allow the user to select more than one option (default false)."
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let question = input["question"].as_str().unwrap_or_default();
        Ok(json!({
            "answer": "",
            "note": format!(
                "No interactive client is attached to collect an answer. Ask the user this in your reply: {question}"
            )
        }))
    }
}
