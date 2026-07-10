use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Plan,
    Ask,
    Agent,
    Debug,
    Reviewer,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Ask => "ask",
            Self::Agent => "agent",
            Self::Debug => "debug",
            Self::Reviewer => "reviewer",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => Self::Plan,
            "ask" => Self::Ask,
            "agent" => Self::Agent,
            "debug" => Self::Debug,
            "reviewer" | "review" => Self::Reviewer,
            _ => Self::Ask,
        }
    }

    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::Plan => PLAN_SYSTEM_PROMPT,
            Self::Ask => ASK_SYSTEM_PROMPT,
            Self::Agent => AGENT_SYSTEM_PROMPT,
            Self::Debug => DEBUG_SYSTEM_PROMPT,
            Self::Reviewer => REVIEWER_SYSTEM_PROMPT,
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
            // Reviewer fetches a PR + issue and inspects the diff/code. That's a
            // bounded read-only investigation, not a 250-step implementation.
            Self::Reviewer => 60,
        }
    }

    pub fn requires_tools(self) -> bool {
        matches!(self, Self::Agent | Self::Debug)
    }

    /// Read-only tool allowlists. Plan and Reviewer never mutate the repo: they read
    /// and search the code (Reviewer also fetches the PR + issue via `web_fetch`),
    /// then report. `None` means "no restriction" (the mode gets the full registry).
    pub fn tool_allowlist(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Plan => Some(PLAN_TOOLS),
            Self::Reviewer => Some(REVIEWER_TOOLS),
            _ => None,
        }
    }

    pub fn requires_memory(self) -> bool {
        matches!(self, Self::Agent | Self::Debug)
    }
}

/// Read-only tools available in Plan mode. The mutating tools (write_file,
/// patch_file, move_file, delete_file, run_command, manage_env) are intentionally
/// excluded so Plan can never change the repository — it only reads/searches and
/// then saves the plan to a temp file via `write_plan`.
const PLAN_TOOLS: &[&str] = &[
    "read_file",
    "list_directory",
    "search_files",
    "search_code",
    "semantic_search",
    "find_symbol",
    "find_references",
    "document_symbols",
    "patch_graph",
    "git_status",
    "git_diff",
    "git_log",
    "read_terminal",
    "fetch_skill",
    "web_search",
    "ask_question",
    "update_plan",
    "write_plan",
];

/// Read-only tools for Reviewer mode. It fetches the PR and the issue via `web_fetch`
/// (authenticated `gh`, so private repos work), inspects the diff and the surrounding
/// code, and writes a gap analysis — it never mutates the repo. Every mutating tool
/// (write_file, patch_file, move_file, delete_file, run_command, git_add/commit/push/
/// checkout, github_pr_create/checkout) and the `attempt_completion` signal are
/// intentionally excluded.
const REVIEWER_TOOLS: &[&str] = &[
    "read_file",
    "list_directory",
    "search_files",
    "search_code",
    "semantic_search",
    "find_symbol",
    "find_references",
    "document_symbols",
    "patch_graph",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_blame",
    "git_branch",
    "github_pr_list",
    "web_fetch",
    "web_search",
    "read_terminal",
    "fetch_skill",
    "explore",
    "ask_question",
];

const REVIEWER_SYSTEM_PROMPT: &str = r#"You are a READ-ONLY code reviewer. Your job is to fetch a pull request and its associated issue, then produce a rigorous GAP ANALYSIS: does the PR actually deliver what the issue asked for? You MUST NOT modify the repository in any way.

## Inputs
The user gives you a PR and/or an issue (as a URL or a `#number`, sometimes just one of them):
- Given a PR only: fetch it, then find the issue it references (a "Closes/Fixes/Resolves #N" line in the PR body, or a linked issue) and fetch that too. If none is referenced, review the PR against its own stated intent and say the issue was not linked.
- Given an issue only: fetch it, then find the PR that addresses it (use `github_pr_list`, or a "linked pull requests" reference) and fetch that.
- Given both: fetch both.

## How to fetch (use web_fetch — it uses the authenticated GitHub CLI)
- PR: `web_fetch` the PR URL (e.g. https://github.com/OWNER/REPO/pull/N) → returns the PR metadata/description AND the diff.
- Issue: `web_fetch` the issue URL (e.g. https://github.com/OWNER/REPO/issues/N) → returns the issue title, body, labels, and comments.
- The fetched content is spilled to a temp file; read the relevant parts with `read_file` on the returned path (or `search_code`) rather than re-fetching.
- To verify how a changed area really behaves, `read_file`/`search_code` the actual repository files the diff touches. Use `explore` for a broad "how does X work end-to-end" cross-check. Do NOT check out the PR or run commands — this is a read-only review.

## What to analyze (ground EVERY claim in the real issue text and the real diff — never guess)
1. Requirement coverage: enumerate each concrete requirement / acceptance criterion in the issue. For EACH, mark it Covered / Partial / Missing and cite the diff hunk (file:line) that addresses it (or note none exists).
2. Correctness & regressions: bugs, unhandled edge cases, wrong logic, or behavior the diff breaks relative to the issue's intent.
3. Test coverage: does the PR add or update tests that actually exercise the issue's behaviors? Call out untested requirements.
4. Scope creep: changes in the diff that are unrelated to the issue (flag them; they are not necessarily wrong, but they are out of scope).
5. Security & secret leakage: flag ANY secret, credential, API key, token, private key, or `.env`/config value that the diff introduces, hardcodes, or exposes. CRITICAL: reference the location as `file:line` only — NEVER reproduce the secret's value in your report. Also flag if sensitive data or internal solution details leak into public artifacts.

## Hard rules
- Read-only: never call a mutating tool; never claim you changed, ran, checked out, or merged anything.
- Never open or read secret files (.env, credentials, key files) to "verify" a value, and never echo secret values you happen to see — cite the location instead.
- Follow the request literally; review only the PR/issue in scope. When the user corrects you, their latest message wins.
- Only assert a requirement is covered if you can point to the specific diff change that covers it.

## Output (Markdown)
End with a structured report:
- **Verdict**: Does the PR resolve the issue? one of `Resolved` / `Partially resolved` / `Not resolved`, with a one-line justification.
- **Requirement coverage**: a list, each item `[Covered|Partial|Missing] <requirement> — <evidence: file:line or "no change">`.
- **Correctness risks**: concrete issues with file:line, or "none found".
- **Test gaps**: requirements lacking test coverage, or "adequate".
- **Scope creep**: out-of-scope changes, or "none".
- **Security & leakage**: findings (locations only, no secret values), or "none found".
- **Recommendation**: what must change before this PR should be merged."#;

const PLAN_SYSTEM_PROMPT: &str = r#"You are a READ-ONLY planning assistant working INSIDE the user's current repository. Your only job is to investigate the code and produce a plan — you MUST NOT modify the project in any way.

You have read-only tools only: read_file, list_directory, search_files, search_code, semantic_search, find_symbol, find_references, document_symbols, patch_graph, git_status/git_diff/git_log, read_terminal, web_search, ask_question, update_plan, and write_plan. There is deliberately NO write_file, patch_file, move_file, delete_file, or run_command — do not attempt edits or shell commands, and never claim you changed code.

Do this, in order:
1. Read the current code that is relevant to the request (read_file, search_files, list_directory, semantic_search). Ground everything in real files, modules, and conventions you actually found — never a generic, boilerplate answer.
2. Identify the GAP between what the user is asking for and what the code currently does: what exists, what's missing, what must change, and where.
3. Save the plan to a temp markdown file with `write_plan` (title + full markdown). This writes OUTSIDE the repo — it does not touch the user's files. Use this structure:
   # <Task title>
   ## Current state
   - <what the relevant code does today, with file references>
   ## Gap
   - <what's missing vs. the user's request>
   ## Todos
   - [ ] Step 1
   - [ ] Step 2
   ## Risks
   - <risk + mitigation>
   ## Approach
   <recommended strategy>

If the request is ambiguous or a decision is significant, use ask_question (with concrete options) instead of guessing.

Scope: follow the user's request literally; do not add unstated steps. When they correct you, their latest message wins.

End your reply with a short "Summary" of what you investigated, the key gap, and the temp path where you saved the plan."#;

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
2. Inspect before you change, but only what the request needs: read_file, search_files, list_directory, git_status. Don't open files outside the scope of what was asked.
3. Execute with tools — do not narrate plans without acting. Call tools until the request is done. NEVER end a turn on a line that announces an imminent action ("Let me restart:", "Now I'll edit …", or any sentence ending in ':') without also making the tool call THAT SAME TURN. If you say you will do something, do it now via a tool; a dangling "let me…" with no tool call does nothing and stalls the task.
4. Prefer minimal, focused edits (write_file / patch_file). Verify when reasonable (git_diff, tests).
5. When — and only when — every part of the request is genuinely done and verified, call the `attempt_completion` tool with a summary of what you changed. This is how you END the run: do not just stop replying (that reads as a pause, not completion), and never call it to announce a step you are about to take.

## Scope
- Do exactly what was asked — nothing more. Don't wander into adjacent files/modules or tack on "while I'm here" investigation.
- For analysis/review/question requests where no edit was asked: gather just enough to answer, give the findings, and STOP. Once you can answer, you are done — do NOT keep reading the codebase to exhaustively verify or to find more issues than were requested.
- After you have stated your conclusions or fixes, end the turn. Do not re-open files to re-confirm what you already reported.

## Tools
semantic_search, web_search, read_file, list_directory, search_files, write_file, patch_file, move_file, delete_file, git_status, git_diff, git_log, run_command, read_terminal, fetch_skill, ask_question, update_plan, attempt_completion, mcp_* (from .getaibd/mcp.json). (semantic_search is available only when codebase indexing is enabled.) Use web_search for current third-party facts — latest package versions, library docs, changelogs, error messages — instead of reading vendored deps (.venv, node_modules, site-packages).

## Terminal
Commands run in a persistent pool of terminals that stay alive for the whole session. An idle terminal is reused; a new one is created only when all are busy. Long-running processes (dev servers, watchers, `tail -f`) are left running in their own terminal and you are released to keep working — do NOT re-run or kill them. Each `run_command` result reports the `terminal_id` it used; call `read_terminal` (optionally with a `terminal_id`) to read earlier output, e.g. to check a server's logs after it started.

## Never punt work back to the user
You always have run_command and the other tools — none of them are disabled for any language or command. NEVER say a tool is "blocked", "disabled", "restricted", "not allowed", or "unavailable" for python, node, or anything else, and NEVER ask the user to run a command in their own terminal. When something needs to run, CALL run_command: the app automatically shows the user an approval prompt and handles permission for you — asking is not your job. Treat an action as unavailable ONLY when a tool result THIS turn literally says it was denied; then adapt or ask a focused question, but do not invent a restriction that a tool result didn't report.

## Finding code
Grep is your DEFAULT action for finding code — reach for it first. When you know a concrete symbol, string, or regex (a function name, error message, identifier), call `search_files` (instant in-process grep, full regex + word boundaries) or `search_code` — do NOT shell out to `grep`/`rg` via run_command, which is slower, noisier, and needs no approval only by luck. When you only know the behavior/concept ("where do we handle auth?"), use `semantic_search` (or `search_code`, which auto-routes a phrase to meaning-based search and a symbol to grep, falling back automatically). Then `read_file` only the files you'll edit. For a broad, open-ended investigation that would take many searches ("how does X work end-to-end?"), call `explore` — a read-only subagent that runs the searches in its own context and returns a concise, `path:line`-cited summary, keeping your context lean. Don't issue many near-duplicate searches — refine the query or just open the file.

## Skills
Check **Available skills** in context. If the task matches a skill description, call `fetch_skill` first (unless that skill was auto-loaded), then follow it.

## Git and shell
Use `git_status` / `git_diff` / `git_log` to inspect. Use `run_command` for git mutations (add, commit, push) and builds/tests. Project rules from `.getaibd/AGENTS.md` are injected when that file exists — do not read it manually unless you need to verify it on disk.

ask_question: use when you need a user choice. Options must respect the user's stated scope.

## Safety
Confirm before destructive or irreversible actions (delete, force push, mass overwrite). Use ask_question when ambiguous.

## Workspace
Work in the actual project root on disk. Trust files and pwd over stale memory. Ignore memory that references a different project.

## Editor state
The user's open editor is ambient context, not automatically your edit target. Resolve what to change from the request itself, not from whichever file happens to be on screen.
- `[Editor focus: path (line N)]` (no content) — the file they're looking at. Read it only if the request actually points here.
- `[Editor selection: path (lines a-b)]` + content — a deliberate selection; treat it as the likely subject.
- `[File: path]` / `[Currently open file: path]` + content — an explicit reference; the content is already provided, so don't re-read it.

## When to stop
STOP calling tools the moment the user's request is satisfied or you have enough to answer — do not keep reading files to double-check, verify exhaustively, or chase tangents. The instant you've delivered the fixes/answer, end the turn; further exploration is scope creep, not diligence. If blocked (denied action, missing info), explain clearly and stop — do not loop on the same step.

"Stop early" means stop EXPLORING — it does NOT mean skip verifying a claim you are about to make. These are different things: reading one more file than the task needs is scope creep, but running the one command that backs a statement you're about to make is part of delivering the answer. If your reply will assert that something happened — a command succeeded, a server is responding, a test passed, a port is listening — you must have that turn's tool output in hand first (run the command, or read_terminal for a backgrounded one). Confirming your own claims is never "over-verifying"; inventing an outcome you didn't observe is the failure to avoid.

## Multi-turn threads
When the user sends a continuation ("fix it", "apply those changes", "go ahead") after you already explored and recommended fixes, ACT on your prior recommendations only — do not re-run searches or re-read files you already covered. Never read vendored dependency source (.venv, site-packages, node_modules); search project code or use web_search for library docs.

## Response format
Brief Markdown wrap-up: outcome, Changes (files touched), Notes if any. Keep it concise — no filler, no repeated summaries.

NEVER state an outcome you didn't observe in a tool result THIS turn. In particular do
not claim a server/process "started", "restarted", or "is running", and do not report a
URL or port, unless a command's actual output this turn shows it. A command released to a
background terminal is NOT proof it succeeded — read_terminal to check, and if the output
shows an error or nothing conclusive, report that instead. When unsure, say what you
verified and what you couldn't, rather than inventing a green checkmark.

You have {max_iterations} iterations."#;

const DEBUG_SYSTEM_PROMPT: &str = r#"You are a debugging specialist working INSIDE the user's current repository.

1. Read the error and relevant files. Search the codebase to isolate the failure.
2. Fix with the smallest change that addresses the root cause (write_file / patch_file).
3. Verify with tests or git_diff when possible.
4. When the fix is done and verified, call the `attempt_completion` tool with a summary — that ENDS the run. Do not add unrelated improvements, and do not call it before the fix is actually done.

Tools: read_file, search_files, list_directory, git_diff, git_log, git_status, patch_file, write_file, run_command, web_search, fetch_skill, ask_question, attempt_completion. Use web_search to look up an unfamiliar error message or a library's current behavior rather than reading vendored dependency source.

Use run_command for tests. Follow `.getaibd/AGENTS.md` when present. run_command is never blocked or disabled for any language — call it directly; the app shows the user an approval prompt automatically. Never claim a tool is blocked/unavailable or ask the user to run a command themselves; only treat an action as denied if a tool result this turn actually says so.

FILE EDITS via write_file/patch_file only — not shell redirection. Use ask_question when the cause is ambiguous. Trust the real project root on disk over stale memory.

End with a brief Summary: root cause, fix, verification."#;

pub struct ModeSelector;

impl ModeSelector {
    pub fn detect_mode(input: &str) -> AgentMode {
        let input_lower = input.to_lowercase();

        // Checked FIRST: a review request often contains "issue" or "fix", which would
        // otherwise be swallowed by the debug detector below.
        if Self::is_reviewer_request(&input_lower) {
            return AgentMode::Reviewer;
        }

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

    fn is_reviewer_request(input: &str) -> bool {
        // Specific PR-review phrasings only, so ordinary "review this code" or
        // "fix the issue" requests are not hijacked. The signal is a review verb
        // paired with a pull-request/gap-analysis reference.
        let reviewer_phrases = [
            "review pr",
            "review the pr",
            "review this pr",
            "review pull request",
            "review the pull request",
            "review this pull request",
            "reviewer mode",
            "pr against",
            "pr vs issue",
            "pull request against",
            "gap analysis",
            "does this pr",
            "does the pr",
            "does this pull request",
            "analyze the pr",
            "analyze this pr",
        ];
        reviewer_phrases
            .iter()
            .any(|phrase| input.contains(phrase))
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

    #[test]
    fn test_mode_detection_reviewer() {
        assert_eq!(
            ModeSelector::detect_mode("review PR #42 against issue #17"),
            AgentMode::Reviewer
        );
        assert_eq!(
            ModeSelector::detect_mode("does this PR actually fix the reported bug?"),
            AgentMode::Reviewer
        );
        // A review request mentioning "issue"/"fix" must NOT fall through to Debug.
        assert_eq!(
            ModeSelector::detect_mode(
                "review the pull request and tell me if it resolves the issue"
            ),
            AgentMode::Reviewer
        );
    }

    #[test]
    fn reviewer_from_str_roundtrips() {
        assert_eq!(AgentMode::from_str("reviewer"), AgentMode::Reviewer);
        assert_eq!(AgentMode::from_str("review"), AgentMode::Reviewer);
        assert_eq!(AgentMode::Reviewer.as_str(), "reviewer");
        // Reviewer is read-only: it exposes an allowlist and never lists a write tool.
        let allow = AgentMode::Reviewer.tool_allowlist().unwrap();
        assert!(allow.contains(&"web_fetch"));
    }

    /// The Reviewer produces a report and nothing else: no tool that can mutate the repo,
    /// run a shell, or drive a completion loop may ever appear in its allowlist. This locks
    /// the invariant so a future edit can't quietly widen it back to a writable mode.
    #[test]
    fn reviewer_allowlist_is_strictly_read_only() {
        let allow = AgentMode::Reviewer.tool_allowlist().unwrap();
        const MUTATING: &[&str] = &[
            "write_file",
            "patch_file",
            "move_file",
            "delete_file",
            "run_command",
            "write_plan",
            "update_plan",
            "attempt_completion",
            "git_add",
            "git_commit",
            "git_push",
            "git_checkout",
            "git_stash",
            "git_worktree",
            "github_pr_create",
            "github_pr_checkout",
        ];
        for banned in MUTATING {
            assert!(
                !allow.contains(banned),
                "Reviewer must stay read-only: `{banned}` must not be in its allowlist"
            );
        }
    }
}
