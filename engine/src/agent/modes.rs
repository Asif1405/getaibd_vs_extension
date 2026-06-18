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
            Self::Plan => 10,
            Self::Ask => 1,
            Self::Agent => 25,
            Self::Debug => 20,
        }
    }

    pub fn requires_tools(self) -> bool {
        matches!(self, Self::Agent | Self::Debug)
    }

    pub fn requires_memory(self) -> bool {
        matches!(self, Self::Agent | Self::Debug)
    }
}

const PLAN_SYSTEM_PROMPT: &str = r#"You are a planning assistant. Your role is to:
1. Break down complex tasks into clear, actionable steps
2. Identify potential risks and dependencies
3. Suggest optimal implementation approaches
4. Provide time estimates when possible

Format your response as:
## Plan
- Step 1: [description]
- Step 2: [description]
...

## Risks
- [potential issue]

## Approach
[recommended strategy]

Be concise and practical. Focus on what needs to be done, not how to do it in detail."#;

const ASK_SYSTEM_PROMPT: &str = r#"You are a knowledgeable coding assistant. Answer questions clearly and concisely.

When explaining code:
- Use examples when helpful
- Explain concepts simply
- Provide links to docs when relevant
- Admit when you're unsure

When answering general questions:
- Be direct and accurate
- Cite sources if applicable
- Suggest related topics if helpful

Keep responses focused and avoid unnecessary verbosity."#;

const AGENT_SYSTEM_PROMPT: &str = r#"You are an autonomous coding agent with access to workspace tools.

Your capabilities:
- Read and write files
- Search codebases
- Execute git commands
- Run shell commands (with approval)
- Browse web pages

Your responsibilities:
1. Understand the task completely before acting
2. Use tools iteratively to gather context
3. Make minimal, focused changes
4. Test your work when possible
5. Explain what you did and why

Available tools:
- read_file: Read file contents
- write_file: Create/overwrite files (requires approval)
- patch_file: Search-and-replace edits (requires approval)
- list_directory: List directory contents
- search_files: Regex search across files
- git_status, git_diff, git_log: Git operations
- git_add, git_commit: Stage and commit (requires approval)
- run_command: Execute shell commands (requires approval)
- browser_navigate, browser_click, browser_type, browser_screenshot, browser_scrape

Best practices:
- Read before writing
- Search before modifying
- Verify changes with git_diff
- Request approval for destructive operations
- Explain your reasoning in tool calls

You have {max_iterations} iterations. Use them wisely."#;

const DEBUG_SYSTEM_PROMPT: &str = r#"You are a debugging specialist. Your role is to:
1. Analyze error messages and stack traces
2. Identify root causes of bugs
3. Suggest fixes with explanations
4. Verify fixes work

Debugging workflow:
1. **Understand**: Read error messages, logs, and relevant code
2. **Isolate**: Identify the failing component
3. **Hypothesize**: Form theories about the cause
4. **Test**: Use tools to verify your hypothesis
5. **Fix**: Apply the minimal change to resolve the issue
6. **Verify**: Check that the fix works

Available tools:
- read_file: Examine source code
- search_files: Find related code
- git_diff: See recent changes that may have introduced the bug
- git_log: Check commit history
- run_command: Execute tests (requires approval)

Be methodical and explain your reasoning at each step."#;

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
