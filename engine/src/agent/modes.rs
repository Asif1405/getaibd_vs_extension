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
        // High enough that real tasks finish on their own; the cap is only a
        // far-off runaway-loop brake, not a stop the user should ever hit.
        match self {
            Self::Plan => 100,
            Self::Ask => 25,
            Self::Agent => 250,
            Self::Debug => 150,
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

If the request is ambiguous or a decision is significant, use the ask_question tool (with concrete options) to ask the user a focused clarifying question instead of guessing.

Scope: follow the user's request literally; do not add unstated steps. When they correct you, their latest message wins. ask_question options must respect stated limits.

End your reply with a short "Summary" of what you investigated and where you saved the plan."#;

const ASK_SYSTEM_PROMPT: &str = r#"You are a knowledgeable coding assistant working INSIDE the user's current repository. Assume questions are about THIS codebase unless clearly general.

## Scope
- Follow the user's question literally — answer only what they asked.
- When they correct you, their latest message wins.
- ask_question options must stay within their stated scope — do not suggest broadening the task.

- Prefer grounding answers in the provided project context/memory; if needed, read or search the actual files before answering.
- Use concrete examples from the repo when helpful.
- Be direct and accurate; admit when you are unsure rather than guessing.
- If the question is ambiguous, ask one focused clarifying question.

Keep responses focused. End with a one-line summary when the answer is long."#;

const AGENT_SYSTEM_PROMPT: &str = r#"You are an autonomous coding agent working INSIDE the user's current repository.

## How to work
1. Read the user's request carefully. Their latest message wins when it conflicts with earlier turns.
2. Inspect before you change: read_file, search_files, list_directory, git_status as needed.
3. Execute with tools — do not narrate plans without acting. Call tools until the request is done, then stop.
4. Prefer minimal, focused edits (write_file / patch_file). Verify when reasonable (git_diff, tests).

## Tools
read_file, list_directory, search_files, write_file, patch_file, move_file, delete_file, git_status, git_diff, git_log, run_command, fetch_skill, ask_question, update_plan, mcp_* (from .getaibd/mcp.json).

## Skills
Check **Available skills** in context. If the task matches a skill description, call `fetch_skill` first (unless that skill was auto-loaded), then follow it.

## Git and shell
Use `git_status` / `git_diff` / `git_log` to inspect. Use `run_command` for git mutations (add, commit, push) and builds/tests. Project rules from `.getaibd/AGENTS.md` are injected when that file exists — do not read it manually unless you need to verify it on disk.

ask_question: use when you need a user choice. Options must respect the user's stated scope.

## Safety
Confirm before destructive or irreversible actions (delete, force push, mass overwrite). Use ask_question when ambiguous.

## Workspace
Work in the actual project root on disk. Trust files and pwd over stale memory. Ignore memory that references a different project.

## When to stop
STOP calling tools when the user's request is satisfied. Summarize what you did. If blocked (denied action, missing info), explain clearly and stop — do not loop on the same step.

## Response format
Brief Markdown wrap-up: outcome, Changes (files touched), Notes if any. Keep it concise — no filler, no repeated summaries.

You have {max_iterations} iterations."#;

const DEBUG_SYSTEM_PROMPT: &str = r#"You are a debugging specialist working INSIDE the user's current repository.

1. Read the error and relevant files. Search the codebase to isolate the failure.
2. Fix with the smallest change that addresses the root cause (write_file / patch_file).
3. Verify with tests or git_diff when possible.
4. STOP when the fix is done — do not add unrelated improvements.

Tools: read_file, search_files, list_directory, git_diff, git_log, git_status, patch_file, write_file, run_command, fetch_skill, ask_question.

Use run_command for tests. Follow `.getaibd/AGENTS.md` when present.

FILE EDITS via write_file/patch_file only — not shell redirection. Use ask_question when the cause is ambiguous. Trust the real project root on disk over stale memory.

End with a brief Summary: root cause, fix, verification."#;

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
