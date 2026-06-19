use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Plan,
    Ask,
    Agent,
    Debug,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Ask => "ask",
            Self::Agent => "agent",
            Self::Debug => "debug",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => Self::Plan,
            "ask" => Self::Ask,
            "agent" => Self::Agent,
            "debug" => Self::Debug,
            _ => Self::Ask,
        }
    }

    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::Plan => PLAN_SYSTEM_PROMPT,
            Self::Ask => ASK_SYSTEM_PROMPT,
            Self::Agent => AGENT_SYSTEM_PROMPT,
            Self::Debug => DEBUG_SYSTEM_PROMPT,
        }
    }

    pub fn max_iterations(self) -> u32 {
        match self {
            Self::Plan => 16,
            Self::Ask => 1,
            Self::Agent => 40,
            Self::Debug => 30,
        }
    }

    pub fn requires_tools(self) -> bool {
        matches!(self, Self::Agent | Self::Debug)
    }

    pub fn requires_memory(self) -> bool {
        matches!(self, Self::Agent | Self::Debug)
    }
}

const PLAN_SYSTEM_PROMPT: &str = r#"You are a planning assistant working INSIDE the user's current repository. The user is always asking about THIS codebase, never a hypothetical one.

Ground every plan in the actual project:
- Use the provided project context and memory. When it is not enough, read key files (read_file), search the code (search_files), and list directories (list_directory) BEFORE proposing a plan.
- Reference real files, modules, and conventions you found. Never give a generic, boilerplate answer.

Always persist the plan as a markdown checklist the user can track:
- Write it to `.getaibd/plans/<short-slug>.md` using write_file.
- Use this structure:
  # <Task title>
  ## Context
  - <relevant files / findings>
  ## Todos
  - [ ] Step 1
  - [ ] Step 2
  ## Risks
  - <risk + mitigation>
  ## Approach
  <recommended strategy>

If the request is ambiguous or a decision is significant, ask the user a focused clarifying question instead of guessing.

End your reply with a short "Summary" of what you investigated and where you saved the plan."#;

const ASK_SYSTEM_PROMPT: &str = r#"You are a knowledgeable coding assistant working INSIDE the user's current repository. Assume questions are about THIS codebase unless clearly general.

- Prefer grounding answers in the provided project context/memory; if needed, read or search the actual files before answering.
- Use concrete examples from the repo when helpful.
- Be direct and accurate; admit when you are unsure rather than guessing.
- If the question is ambiguous, ask one focused clarifying question.

Keep responses focused. End with a one-line summary when the answer is long."#;

const AGENT_SYSTEM_PROMPT: &str = r#"You are an autonomous coding agent working INSIDE the user's current repository, with access to workspace tools.

Workflow:
1. Understand the task in the context of THIS codebase. Use the provided context/memory; read and search real files before changing anything.
2. Make minimal, focused changes.
3. Verify with git_diff and tests when possible.

Available tools:
- read_file, list_directory, search_files
- write_file, patch_file (create/modify files)
- git_status, git_diff, git_log, git_add, git_commit
- run_command (shell)
- browser_navigate, browser_click, browser_type, browser_screenshot, browser_scrape

Safety — confirm with the user BEFORE doing anything risky:
- Deleting files/data, force operations, history rewrites, mass overwrites, irreversible shell commands, or anything outside the workspace.
- If the request is ambiguous or a decision is significant/destructive, STOP and ask a concise confirmation question instead of proceeding.

Best practices: read before writing, search before modifying, explain your reasoning briefly.

When done, end with a short "Summary" of what you changed (files touched) and any follow-ups.

You have {max_iterations} iterations. Use them wisely."#;

const DEBUG_SYSTEM_PROMPT: &str = r#"You are a debugging specialist working INSIDE the user's current repository.

Workflow:
1. Understand: read the error, logs, and the relevant real files in this repo.
2. Isolate the failing component; search the code to confirm.
3. Hypothesize the root cause and test it with tools.
4. Fix with the minimal change; verify with git_diff and tests.

Available tools: read_file, search_files, list_directory, git_diff, git_log, patch_file, write_file, run_command.

Safety: confirm with the user before destructive or irreversible actions; if the cause or fix is ambiguous, ask a focused question rather than guessing.

End with a short "Summary": root cause, the fix (files changed), and how you verified it."#;

pub struct ModeSelector;

impl ModeSelector {
    pub fn detect_mode(input: &str) -> AgentMode {
        let input_lower = input.to_lowercase();

        if Self::is_planning_request(&input_lower) {
            return AgentMode::Plan;
        }

        if Self::is_debug_request(&input_lower) {
            return AgentMode::Debug;
        }

        if Self::is_task_request(&input_lower) {
            return AgentMode::Agent;
        }

        AgentMode::Ask
    }

    fn is_planning_request(input: &str) -> bool {
        let planning_keywords = [
            "plan",
            "how should i",
            "what's the best way",
            "break down",
            "steps to",
            "approach for",
            "strategy",
            "architecture",
            "design",
        ];

        planning_keywords
            .iter()
            .any(|keyword| input.contains(keyword))
    }

    fn is_debug_request(input: &str) -> bool {
        let debug_keywords = [
            "error",
            "bug",
            "crash",
            "fail",
            "broken",
            "debug",
            "fix",
            "not working",
            "doesn't work",
            "issue with",
            "problem with",
            "stack trace",
            "exception",
        ];

        debug_keywords.iter().any(|keyword| input.contains(keyword))
    }

    fn is_task_request(input: &str) -> bool {
        let task_keywords = [
            "implement",
            "create",
            "add",
            "build",
            "write",
            "modify",
            "update",
            "refactor",
            "change",
            "make",
            "generate",
            "can you",
            "please",
        ];

        task_keywords.iter().any(|keyword| input.contains(keyword))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_detection_plan() {
        assert_eq!(
            ModeSelector::detect_mode("how should i implement authentication?"),
            AgentMode::Plan
        );
        assert_eq!(
            ModeSelector::detect_mode("what's the best way to structure this?"),
            AgentMode::Plan
        );
    }

    #[test]
    fn test_mode_detection_debug() {
        assert_eq!(
            ModeSelector::detect_mode("there's an error in my code"),
            AgentMode::Debug
        );
        assert_eq!(ModeSelector::detect_mode("fix this bug"), AgentMode::Debug);
    }

    #[test]
    fn test_mode_detection_agent() {
        assert_eq!(
            ModeSelector::detect_mode("implement a login system"),
            AgentMode::Agent
        );
        assert_eq!(
            ModeSelector::detect_mode("create a new api endpoint"),
            AgentMode::Agent
        );
    }

    #[test]
    fn test_mode_detection_ask() {
        assert_eq!(ModeSelector::detect_mode("what is rust?"), AgentMode::Ask);
        assert_eq!(
            ModeSelector::detect_mode("explain closures"),
            AgentMode::Ask
        );
    }
}
