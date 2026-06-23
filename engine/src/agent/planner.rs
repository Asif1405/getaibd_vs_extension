use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::models::{ToolChatRequest, ToolMessage};
use crate::providers::Provider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    RetrieveContext,
    GenerateCode,
    ApplyPatch,
    RunCommand,
    VerifyTests,
    AskUser,
    Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: usize,
    pub kind: StepKind,
    pub description: String,
    pub depends_on: Vec<usize>,
    pub estimated_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    pub task: String,
    pub reasoning: String,
    pub steps: Vec<PlanStep>,
    pub verify_commands: Vec<String>,
}

impl TaskPlan {
    /// Returns steps whose dependencies are all satisfied by `completed` step IDs.
    pub fn next_steps_from_completed<'a>(&'a self, completed: &[usize]) -> Vec<&'a PlanStep> {
        self.steps
            .iter()
            .filter(|s| {
                !completed.contains(&s.id) && s.depends_on.iter().all(|dep| completed.contains(dep))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_id: usize,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed(String),
}

pub struct TaskPlanner {
    provider: Arc<dyn Provider>,
}

impl TaskPlanner {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }

    pub async fn plan(&self, task: &str, context: &str) -> Result<TaskPlan, AppError> {
        let system = r#"You are a precise task planner for a code editing system. Given a task and project context, produce a detailed execution plan.

You MUST respond with valid JSON in this exact format:
{
  "task": "original task description",
  "reasoning": "why you chose this approach",
  "steps": [
    {
      "id": 1,
      "kind": "retrieve_context",
      "description": "what to do in this step",
      "depends_on": [],
      "estimated_files": ["src/foo.rs"]
    }
  ],
  "verify_commands": ["cargo check", "cargo test"]
}

Step kinds: retrieve_context, generate_code, apply_patch, run_command, verify_tests, ask_user
Order steps by dependency. Keep steps atomic and verifiable."#;

        let user = format!(
            "Project context:\n{}\n\nTask: {}\n\nProduce the execution plan.",
            context, task
        );

        let request = ToolChatRequest {
            model: self
                .provider
                .list_models()
                .await
                .ok()
                .and_then(|m| m.into_iter().next().map(|m| m.id))
                .unwrap_or_else(|| "default".to_string()),
            messages: vec![ToolMessage::system(system), ToolMessage::user(&user)],
            tools: vec![],
            temperature: Some(0.2),
            max_tokens: Some(2048),
            reasoning_effort: None,
            tool_choice: None,
            compress: false,
        };

        let response = self.provider.chat_with_tools(&request).await?;
        let content = response.content.unwrap_or_default();

        self.parse_plan(&content)
    }

    fn parse_plan(&self, response: &str) -> Result<TaskPlan, AppError> {
        let json_str = extract_json(response)
            .ok_or_else(|| AppError::InvalidRequest("No valid JSON plan in response".into()))?;

        serde_json::from_str::<TaskPlan>(json_str)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to parse plan JSON: {}", e)))
    }

    pub fn next_steps<'a>(&self, plan: &'a TaskPlan, completed: &[usize]) -> Vec<&'a PlanStep> {
        plan.next_steps_from_completed(completed)
    }
}

fn extract_json(s: &str) -> Option<&str> {
    if let Some(start) = s.find("```json") {
        let after = &s[start + 7..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim());
        }
    }
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if inner.starts_with('{') {
                return Some(inner);
            }
        }
    }
    let trimmed = s.trim();
    if trimmed.starts_with('{') {
        return Some(trimmed);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_steps_respects_dependencies() {
        let plan = TaskPlan {
            task: "test".into(),
            reasoning: "test".into(),
            steps: vec![
                PlanStep {
                    id: 1,
                    kind: StepKind::RetrieveContext,
                    description: "step1".into(),
                    depends_on: vec![],
                    estimated_files: vec![],
                },
                PlanStep {
                    id: 2,
                    kind: StepKind::GenerateCode,
                    description: "step2".into(),
                    depends_on: vec![1],
                    estimated_files: vec![],
                },
                PlanStep {
                    id: 3,
                    kind: StepKind::ApplyPatch,
                    description: "step3".into(),
                    depends_on: vec![2],
                    estimated_files: vec![],
                },
            ],
            verify_commands: vec![],
        };

        let planner = TaskPlanner {
            provider: Arc::new(crate::providers::openai_compat::OpenAiCompatProvider::new(
                "test".to_string(),
                "Test".to_string(),
                "http://localhost:9999".to_string(),
                None,
                "test-model".to_string(),
                60,
                3,
                false,
            )),
        };

        let next = planner.next_steps(&plan, &[]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, 1);

        let next = planner.next_steps(&plan, &[1]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, 2);

        let next = planner.next_steps(&plan, &[1, 2]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, 3);
    }
}
