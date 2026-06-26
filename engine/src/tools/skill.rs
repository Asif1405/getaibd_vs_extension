use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::AppError;
use crate::tools::Tool;

use crate::agent::project_rules;

pub struct FetchSkill {
    root: Arc<PathBuf>,
}

impl FetchSkill {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for FetchSkill {
    fn name(&self) -> &'static str {
        "fetch_skill"
    }

    fn description(&self) -> &'static str {
        "Load a project skill from `.getaibd/skills/` by name. Use when the user's task matches \
         a skill in the Available skills catalog."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name from the catalog (e.g. release, pre-commit)"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("name is required".into()))?
            .trim();
        if name.is_empty() {
            return Err(AppError::InvalidRequest("name is required".into()));
        }
        let skill = project_rules::load_skill_by_name(&self.root, name).ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "Unknown skill '{name}'. Check Available skills in context or list `.getaibd/skills/`."
            ))
        })?;
        Ok(json!({
            "name": skill.name,
            "description": skill.description,
            "content": skill.body,
        }))
    }
}
