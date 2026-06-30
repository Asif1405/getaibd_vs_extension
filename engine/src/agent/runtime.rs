use std::path::Path;
use std::sync::Arc;

use crate::circuit_breaker::CircuitBreaker;
use crate::error::AppError;
use crate::memory::cache::ProjectAnalysisCache;
use crate::memory::context_builder::{build_smart_context, ContextBuilder};
use crate::memory::embeddings::EmbeddingProvider;
use crate::memory::indexer::MemoryIndexer;
use crate::memory::persistent::PersistentMemory;
use crate::memory::store::MemoryStore;
use crate::memory::{format_context, retrieve_context};
use crate::context::ContextConfig;
use crate::models::{
    ChatRequest, Message, ToolCall, ToolChatRequest, ToolChatResponse, ToolMessage,
    ToolStreamDelta,
};
use crate::providers::openai_compat::parse_tool_arguments;
use crate::providers::Provider;
use crate::retry::chat_with_tools_retry_cb;
use crate::tools::approval::ApprovalGate;
use crate::tools::ToolRegistry;

use super::project_rules;
use super::session::Session;
use super::thinking;

/// Default model for the strict task-completion review on the GetAIBD provider.
/// The completion check is a small, stateless JSON classification, so it runs on
/// a cheap fast model instead of the (possibly expensive) model the user picked.
/// Other providers keep using the session model (see `verify_task_complete`).
/// Overridable at runtime via `GETAIBD_COMPLETION_MODEL` so ops can repoint it
/// without a rebuild (e.g. if a provider runs out of upstream credits).
const DEFAULT_COMPLETION_MODEL: &str = "gemini-3.5-flash";

/// The completion-reviewer model: env override if set, else the funded default.
fn completion_model() -> String {
    std::env::var("GETAIBD_COMPLETION_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_COMPLETION_MODEL.to_string())
}

/// Even on million-token-window models, resending the whole transcript every turn
/// is the dominant token cost of a long agent run. Compact once the conversation
/// crosses this absolute budget regardless of how large the model's window is, so
/// Gemini (1M window) doesn't quietly resend ~850k tokens per turn before its 85%
/// threshold kicks in.
const MAX_CONTEXT_TOKENS_BEFORE_COMPACT: usize = 200_000;

/// A single tool result kept verbatim in history is re-sent on every subsequent
/// turn, so one giant `read_file`/`run_command` dump inflates every later request.
/// Clip oversized results (head + tail, with a marker) before they enter history;
/// the FULL output is still streamed to the UI via the `ToolResult` event.
const MAX_TOOL_RESULT_CHARS: usize = 16_000;
const TOOL_RESULT_HEAD_CHARS: usize = 12_000;
const TOOL_RESULT_TAIL_CHARS: usize = 2_000;

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Clip an oversized serialized tool result for the model's context, keeping the
/// head and tail (the most useful parts) and noting how much was elided.
fn cap_tool_result_for_history(s: String) -> String {
    if s.len() <= MAX_TOOL_RESULT_CHARS {
        return s;
    }
    let head_end = floor_char_boundary(&s, TOOL_RESULT_HEAD_CHARS);
    let tail_start = ceil_char_boundary(&s, s.len().saturating_sub(TOOL_RESULT_TAIL_CHARS));
    if tail_start <= head_end {
        return s;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n\n…[{omitted} characters truncated to save context; the full output is shown in the UI]…\n\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

pub struct AgentEvent {
    pub kind: AgentEventKind,
    pub content: Option<String>,
}

pub enum AgentEventKind {
    Start,
    Think,
    ToolCall,
    ToolResult,
    Response,
    Complete,
    Error,
    ModeSelected,
    /// The agent is outputting its plan before acting.
    Planning,
    /// Chain-of-thought reasoning tokens.
    Thinking,
    /// Post-action reflection on tool results.
    Reflecting,
    /// The agent decided to re-plan after reflection.
    Replanning,
    /// Context was compressed/summarized to fit the window.
    ContextCompressed,
    FileEdit,
    /// A tool needs the user to approve before it runs.
    ApprovalRequired,
    /// A shell command should be executed by the client in a managed terminal.
    TerminalExec,
    /// The agent is asking the user a clarifying question with options.
    AskRequired,
    /// The run stopped because it reached the step-limit brake; the user can continue.
    StepLimitReached,
    /// A provisional assistant message that was already streamed is being superseded
    /// (e.g. the completion reviewer decided more work is needed). The client should
    /// drop the last streamed assistant draft so it is not shown as a duplicate.
    DiscardDraft,
}

pub struct AgentResult {
    pub final_response: String,
    pub iterations: u32,
    pub mode: Option<String>,
}

pub struct MemoryContext<'a> {
    pub store: &'a MemoryStore,
    pub embedder: &'a dyn EmbeddingProvider,
    pub top_k: usize,
    pub max_entries: usize,
    /// Optional shared project-analysis cache for richer context building.
    pub analysis_cache: Option<Arc<ProjectAnalysisCache>>,
}

pub struct AgentOptions {
    pub approval_gate: Option<ApprovalGate>,
    pub terminal_gate: Option<crate::tools::terminal_gate::TerminalGate>,
    pub ask_gate: Option<crate::tools::ask_gate::AskGate>,
    pub tool_timeout_secs: u64,
    pub circuit_breaker: Option<Arc<CircuitBreaker>>,
    pub context_config: Option<ContextConfig>,
    pub enable_thinking: bool,
    /// When true, a strict reviewer (same model) verifies the original task is actually
    /// finished before the run ends, and forces the agent to keep working if it is not.
    /// Only action modes (Agent/Debug) set this; Ask/Plan are meant to yield.
    pub auto_complete: bool,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            approval_gate: None,
            terminal_gate: None,
            ask_gate: None,
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: None,
            enable_thinking: true,
            auto_complete: false,
        }
    }
}

pub async fn run_agent(
    session: &mut Session,
    task: &str,
    provider: &Arc<dyn Provider>,
    registry: &ToolRegistry,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    run_agent_with_memory(session, task, provider, registry, None, None, &mut on_event).await
}

pub async fn run_agent_with_memory(
    session: &mut Session,
    task: &str,
    provider: &Arc<dyn Provider>,
    registry: &ToolRegistry,
    memory: Option<&MemoryContext<'_>>,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    if !provider.supports_tool_calling() {
        return Err(AppError::ProviderError(format!(
            "{}: does not support tool calling",
            provider.id()
        )));
    }

    // Warm the catalog context-window cache once if this model's real window
    // isn't known yet, so summarization uses the true limit instead of a guess.
    if crate::context::cached_window(&session.model).is_none() {
        let _ = provider.list_models().await;
    }

    inject_context(session, task, memory).await;
    let images = std::mem::take(&mut session.pending_user_images);
    if images.is_empty() {
        session.push_message(ToolMessage::user(task));
    } else {
        session.push_message(ToolMessage::user_with_images(task, images));
    }
    agent_loop(session, task, provider, registry, memory, options, on_event).await
}

/// Build the environment descriptor (OS + shell) reported by the client, if any.
pub fn format_environment(os: Option<&str>, shell: Option<&str>) -> Option<String> {
    let os = os.map(str::trim).filter(|s| !s.is_empty());
    let shell = shell.map(str::trim).filter(|s| !s.is_empty());
    match (os, shell) {
        (Some(os), Some(sh)) => Some(format!("Host OS: {os}. Active shell: {sh}.")),
        (Some(os), None) => Some(format!("Host OS: {os}.")),
        (None, Some(sh)) => Some(format!("Active shell: {sh}.")),
        (None, None) => None,
    }
}

/// A system note that tells the model the host OS and shell and asks it to
/// generate commands dynamically for that environment. The client reports the
/// real shell; if it doesn't, we fall back to the OS the engine runs on.
fn environment_note(env: Option<&str>) -> String {
    let desc = env
        .map(str::to_string)
        .unwrap_or_else(|| format!("Host OS: {}.", std::env::consts::OS));
    format!(
        "ENVIRONMENT\n{desc}\n\
        Generate every terminal command using the exact syntax, built-in commands, command \
        chaining/operators, path style, and quoting rules of that shell on that OS. Use only \
        tools that exist in that environment, and never assume a different shell or OS."
    )
}

/// File paths the user *deliberately* pointed at — explicit edit-target gestures, not
/// ambient editor state. Picks up `@mention`s (`[File: path]`), deliberate selections
/// (`[Editor selection: path ...]`), and the legacy full-content open-file form
/// (`[Currently open file: path]`). Crucially it does NOT pick up `[Editor focus: path]`:
/// merely having a file on screen is ambient context, not a signal to edit/seed RAG from
/// it. Used as glob-rule hints and to seed RAG context.
fn edit_target_hints(messages: &[ToolMessage]) -> Vec<String> {
    // Markers carrying a path in `[<prefix>: path ...]` form that count as deliberate.
    // `[Editor focus: ...]` is intentionally absent.
    const EXPLICIT_PREFIXES: [&str; 3] =
        ["[File: ", "[Editor selection: ", "[Currently open file: "];
    messages
        .iter()
        .filter_map(|msg| msg.content.as_deref())
        .flat_map(|content| {
            let mut files = Vec::new();
            for line in content.lines() {
                for prefix in EXPLICIT_PREFIXES {
                    if let Some(rest) = line.strip_prefix(prefix) {
                        // Path runs up to the first `]` or ` (` (range suffix), e.g.
                        // `[Editor selection: src/a.ts (lines 3-9)]`.
                        let path = rest
                            .split_once(']')
                            .map(|(p, _)| p)
                            .unwrap_or(rest)
                            .split(" (")
                            .next()
                            .unwrap_or("")
                            .trim();
                        if !path.is_empty() {
                            files.push(path.to_string());
                        }
                        break;
                    }
                }
            }
            files
        })
        .collect()
}

async fn inject_context(session: &mut Session, task: &str, memory: Option<&MemoryContext<'_>>) {
    // Build the static prefix (system prompt + long-term memory) and prepend it so it sits
    // BEFORE the conversation history. This keeps the most recent turns closest to the task,
    // which makes them the most salient context for the model.
    let mut prefix: Vec<ToolMessage> = Vec::new();

    if let Some(sys) = &session.system_prompt {
        prefix.push(ToolMessage::system(sys.clone()));
    }

    // Project instructions: the built-in baseline merged with the project's own
    // .getaibd/AGENTS.md (user sections win, baseline fills the gaps), plus user
    // rules, nested AGENTS.md, matched glob rules, memory, and the skills catalog.
    let hint_paths = edit_target_hints(&session.messages);
    let instructions = project_rules::load_project_instructions(
        Path::new(&session.project_root),
        session.workspace_cwd.as_deref(),
        session.user_rules.as_deref(),
        task,
        &hint_paths,
    );
    for msg in instructions.system_messages {
        prefix.push(ToolMessage::system(msg));
    }

    // Make the model aware of the host OS/shell so it always generates commands for the
    // shell they actually run in, instead of assuming a POSIX/bash environment.
    prefix.push(ToolMessage::system(environment_note(
        session.environment.as_deref(),
    )));

    let persistent = PersistentMemory::new(&session.project_root);
    if persistent.exists() {
        let facts = persistent.as_context();
        if !facts.is_empty() {
            prefix.push(ToolMessage::system(facts));
        }
    }

    if let Some(mem) = memory {
        // On continuation turns the conversation already holds prior exploration; re-running
        // RAG/semantic retrieval for a short "fix it" message just re-injects unrelated files
        // and encourages the model to re-read the whole repo.
        let continuation = is_continuation_request(task, &session.messages);
        if !continuation {
        // Reuse the file paths the extension referenced in prior context messages.
        let current_files = hint_paths.clone();

        // Try to enrich context with cached project graph when available.
        let ctx_result = if let Some(cache) = mem.analysis_cache.as_ref() {
            let project_root = Path::new(&session.project_root);
            let maybe_graph = cache.get_project_graph(project_root).ok().flatten();
            let mut builder =
                ContextBuilder::new(mem.store, mem.embedder, project_root.to_path_buf())
                    .with_max_context_chars(50_000)
                    .with_min_relevance_score(0.3);
            if let Some(graph) = maybe_graph {
                builder = builder.with_project_graph(graph);
            }
            match builder.build_context(task, &current_files).await {
                Ok(window) => Ok(builder.format_context_window(&window)),
                Err(e) => Err(e),
            }
        } else {
            build_smart_context(
                mem.store,
                mem.embedder,
                Path::new(&session.project_root),
                task,
                &current_files,
            )
            .await
        };

        match ctx_result {
            Ok(ctx) if !ctx.is_empty() => {
                prefix.push(ToolMessage::system(ctx));
            }
            _ => {
                if let Ok(memories) =
                    retrieve_context(mem.store, mem.embedder, task, mem.top_k).await
                {
                    let ctx = format_context(&memories);
                    if !ctx.is_empty() {
                        prefix.push(ToolMessage::system(ctx));
                    }
                }
            }
        }
        }
    }

    if is_continuation_request(task, &session.messages) {
        prefix.push(ToolMessage::system(
            "CONTINUATION TURN: This thread already contains your prior exploration and \
             recommendations. The user's latest message is asking you to ACT on that work — \
             not to re-investigate. Do NOT re-run semantic_search, search_files, or read_file \
             on files you already discussed unless you need one specific missing detail. \
             Implement only the fixes you already identified in your previous assistant \
             message; use git_diff to verify, then stop. Do not re-summarize unrelated files.",
        ));
    }

    if !prefix.is_empty() {
        let mut combined = prefix;
        combined.append(&mut session.messages);
        session.messages = combined;
    }
}

#[allow(clippy::too_many_lines)]
async fn agent_loop(
    session: &mut Session,
    task: &str,
    provider: &Arc<dyn Provider>,
    registry: &ToolRegistry,
    memory: Option<&MemoryContext<'_>>,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let tool_defs = registry.definitions();
    let mut iterations = 0;
    let mut last_text = String::new();
    let mut nudge_count = 0u32;
    let use_streaming = provider.supports_streaming_tools();
    let enable_thinking = options.is_none_or(|o| o.enable_thinking);

    // Auto-complete (manager/critic) state. When enabled, a strict reviewer confirms the original
    // task is actually finished before the run ends and forces the agent to keep going if not.
    // Hard caps keep this from ever looping forever or burning the balance:
    // The run is normally governed by task completion + the 85% context budget +
    // stall detection — NOT a fixed step count. These backstops only ever stop a
    // pathological model that never settles:
    //   * MAX_FORCE_CONTINUE — absolute number of forced continuations.
    //   * ABSOLUTE_MAX_ITERATIONS — hard wall on total model turns.
    //   * MAX_STALL_ROUNDS — stop when genuinely stuck (no progress, same gaps).
    const MAX_FORCE_CONTINUE: u32 = 40;
    const ABSOLUTE_MAX_ITERATIONS: u32 = 200;
    const MAX_STALL_ROUNDS: u32 = 3;
    const STEP_EXTENSION: u32 = 40;
    // Loop/repeat guard thresholds (see `last_iter_sig` below).
    const REPEAT_STOP_TOOLS: u32 = 2; // stop on the 3rd identical tool turn in a row
    const REPEAT_STOP_TEXT: u32 = 1; // stop on the 2nd identical no-tool answer
    let task_ctx = resolve_task_context(session, task);
    let auto_complete = options.is_some_and(|o| o.auto_complete) && !tool_defs.is_empty();
    let mut force_continue = 0u32;
    // Counts substantive work: file mutations plus non-inspection shell commands
    // (git commit/push, tests, builds, etc.). Used for completion + stall detection.
    let mut work_total = 0usize;
    let mut work_at_last_force = 0usize;
    // Stall detection: consecutive reviewer rounds with no new progress AND an
    // unchanged outstanding-items list mean we're genuinely stuck — stop cleanly
    // instead of forcing forever.
    let mut stall_rounds = 0u32;
    let mut last_missing: Vec<String> = Vec::new();
    // Loop/repeat guard. Catches the model emitting a byte-identical action on
    // consecutive turns — re-running the SAME tool call (e.g. re-writing the same
    // file) or re-emitting the SAME answer text. Without this, a repeated write
    // bumps `work_total`, which resets stall detection, so the agent can redo
    // finished work up to MAX_FORCE_CONTINUE times. We (a) never count an identical
    // repeat as progress, (b) nudge once to break the loop, and (c) hard-stop after
    // a couple of identical turns (thresholds REPEAT_STOP_* declared above).
    let mut last_iter_sig: Option<u64> = None;
    let mut repeat_rounds = 0u32;

    if enable_thinking && iterations == 0 {
        session.push_message(ToolMessage::system(thinking::PLANNING_PROMPT.to_string()));
    }

    if task_ctx.is_follow_up {
        let continuation = is_continuation_request(&task_ctx.current_input, &session.messages);
        let note = if continuation && !is_short_follow_up(&task_ctx.current_input) {
            format!(
                "The user's latest message (\"{}\") is a CONTINUATION — they want you to \
                 implement what you already found and recommended in your previous reply, \
                 not re-explore the codebase. The original task was: \"{}\". Read your prior \
                 assistant message in this thread. Make only the targeted edits you already \
                 proposed; do not re-summarize unrelated files or grep vendored dependencies \
                 (.venv, site-packages).",
                task_ctx.current_input, task_ctx.effective_task
            )
        } else {
            format!(
                "The user's latest message is a short follow-up (\"{}\"). The active task from \
                 this conversation is: \"{}\". Read the full history: if that task was already \
                 fully completed in a prior assistant reply, answer the follow-up briefly only — \
                 do NOT redo, re-summarize, or re-explore work you already finished. If the task is \
                 genuinely unfinished, continue from where you left off and finish only what remains \
                 — never restart from the beginning.",
                task_ctx.current_input, task_ctx.effective_task
            )
        };
        session.push_message(ToolMessage::system(note));
    }

    // Seed a structured plan up front for any task that can use tools. The model
    // lays out its own checklist via update_plan; we then keep it pinned in context
    // and use it (plus the diff-aware reviewer) to drive the run to real completion.
    if auto_complete && session.task_ledger.is_none() && task_ctx.requires_tools {
        session.push_message(ToolMessage::system(
            "FIRST, for any task that needs more than one step, call the update_plan tool with a \
             short checklist: a one-line goal followed by concrete steps each marked [ ]. As you \
             finish each step, call update_plan again to mark it [x]. Do not stop until every step \
             is [x] and the original task is genuinely done. For a trivial one-step task you may \
             skip planning and just do it.",
        ));
    }

    loop {
        if iterations >= session.max_iterations {
            // Auto-complete: at the ceiling, let the reviewer decide if we're actually done. If
            // not (and budget + progress remain), raise the ceiling and keep going instead of
            // stopping. Otherwise fall through to the manual Continue brake below.
            if auto_complete
                && force_continue < MAX_FORCE_CONTINUE
                && iterations < ABSOLUTE_MAX_ITERATIONS
            {
                on_event(AgentEvent {
                    kind: AgentEventKind::Reflecting,
                    content: Some("Reviewing whether the task is fully complete…".into()),
                });
                let final_text = if last_text.trim().is_empty() {
                    "(no summary yet)"
                } else {
                    last_text.as_str()
                };
                let workspace = workspace_changes_brief(registry).await;
                let verdict =
                    verify_task_complete(session, &task_ctx, final_text, &workspace, provider)
                        .await;
                let done = completion_accepted(&verdict, work_total, session, &task_ctx);
                if !done {
                    let progressed = work_total > work_at_last_force;
                    let stalled = is_stalled(
                        progressed,
                        &verdict.missing,
                        &mut last_missing,
                        &mut stall_rounds,
                        MAX_STALL_ROUNDS,
                    );
                    if !stalled {
                        force_continue += 1;
                        work_at_last_force = work_total;
                        nudge_count = 0;
                        session.max_iterations += STEP_EXTENSION;
                        let remaining = format_missing(&verdict.missing);
                        on_event(AgentEvent {
                            kind: AgentEventKind::Reflecting,
                            content: Some(format!(
                                "Step limit reached but the task isn't done — continuing \
                                 automatically ({force_continue}/{MAX_FORCE_CONTINUE}).\n{remaining}"
                            )),
                        });
                    session.push_message(ToolMessage::system(format!(
                        "A completion reviewer checked your work against the ACTIVE TASK and \
                         found it is NOT yet complete. Outstanding items:\n{remaining}\n\n\
                         Continue from your current progress — finish only what is still missing. \
                         Do NOT restart from scratch, re-read the whole repo, or repeat summaries \
                         you already gave."
                    )));
                        continue;
                    }
                    // Genuinely stuck — fall through to the wrap-up summary stop.
                }
            }
            on_event(AgentEvent {
                kind: AgentEventKind::StepLimitReached,
                content: Some(iterations.to_string()),
            });
            session.push_message(ToolMessage::system(
                "You have reached the step limit. Stop calling tools now and reply with a concise \
                 summary of what you accomplished, what remains, and any next steps."
                    .to_string(),
            ));
            let wrap = ToolChatRequest {
                model: session.model.clone(),
                messages: session.messages.clone(),
                tools: Vec::new(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                tool_choice: None,
                compress: false,
                cache_session_id: session.cache_session_id.clone(),
            };
            let summary = match chat_with_tools_retry_cb(provider, &wrap, None).await {
                Ok(r) => r.content.filter(|c| !c.trim().is_empty()),
                Err(_) => None,
            };
            let final_text = summary.unwrap_or_else(|| {
                if last_text.trim().is_empty() {
                    "Reached the step limit before finishing the task.".to_string()
                } else {
                    last_text.clone()
                }
            });
            return finish_agent(
                session,
                Some(final_text),
                memory,
                provider,
                iterations,
                on_event,
            )
            .await;
        }

        // Context management: we NEVER hard-drop turns (a coding agent's earlier
        // tool results and the original task are load-bearing — dropping them makes
        // it lose its place and fail unpredictably). Instead we track a running
        // token estimate and, once the conversation crosses 85% of the model's
        // context window, summarize the older middle while keeping the system
        // prompt, the original task, and the most recent turns verbatim.
        let ctx_limit = crate::context::context_window_for(&session.model);
        let tool_tokens = crate::context::count_tool_definition_tokens(&tool_defs);
        #[allow(clippy::cast_precision_loss)]
        // Compact at 85% of the model's window OR an absolute budget, whichever is
        // smaller — so huge-window models (Gemini = 1M) don't resend a giant
        // transcript every turn before their percentage threshold would trigger.
        let summarize_threshold =
            ((ctx_limit as f32 * 0.85) as usize).min(MAX_CONTEXT_TOKENS_BEFORE_COMPACT);
        // Summarize-and-refeed: never trim. Keep compressing the older middle until
        // we're back under 85% of the model's real window, or a pass can no longer
        // compress anything (guarantees termination even if the recent tail alone
        // is large).
        let mut compressed = false;
        loop {
            let used = crate::context::count_tool_message_tokens(&session.messages) + tool_tokens;
            if used <= summarize_threshold {
                break;
            }
            if !summarize_old_messages(session, provider).await {
                break;
            }
            compressed = true;
        }
        if compressed {
            on_event(AgentEvent {
                kind: AgentEventKind::ContextCompressed,
                content: Some("Summarized earlier turns to stay within the context window".into()),
            });
        }

        // Re-inject the task ledger as the most-recent system note every turn so the
        // model always sees its current plan/place. It lives outside session.messages,
        // so it is never summarized away and always reflects the latest update_plan.
        let mut req_messages = session.messages.clone();
        if let Some(ledger) = session.task_ledger.as_deref() {
            if !ledger.trim().is_empty() {
                req_messages.push(ToolMessage::system(format!(
                    "CURRENT PLAN (your task ledger — keep it updated with the update_plan tool; \
                     mark steps [x] as you finish them and do not stop until every step is [x]):\n{ledger}"
                )));
            }
        }

        let request = ToolChatRequest {
            model: session.model.clone(),
            messages: req_messages,
            tools: tool_defs.clone(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: session.reasoning_effort.clone(),
            // Never send tool_choice=required — breaks Alibaba/Qwen thinking mode.
            // Weak models are nudged via system messages; compat layer defaults to "auto".
            tool_choice: None,
            compress: session.compress,
            cache_session_id: session.cache_session_id.clone(),
        };

        let response = if use_streaming {
            // A stalled stream (idle watchdog tripped) is transient — retry the same
            // turn once with the draft discarded, rather than failing the whole task.
            // The non-streaming path already retries internally via the helper.
            match collect_streaming_response(provider, request.clone(), on_event).await {
                Ok(r) => r,
                Err(AppError::ProviderTimeout(_)) => {
                    on_event(AgentEvent {
                        kind: AgentEventKind::DiscardDraft,
                        content: None,
                    });
                    on_event(AgentEvent {
                        kind: AgentEventKind::Reflecting,
                        content: Some("The model stream stalled — retrying this step…".into()),
                    });
                    collect_streaming_response(provider, request, on_event).await?
                }
                Err(e) => return Err(e),
            }
        } else {
            let cb = options.and_then(|o| o.circuit_breaker.as_deref());
            chat_with_tools_retry_cb(provider, &request, cb).await?
        };
        iterations += 1;

        if let Some(text) = &response.content {
            if !text.trim().is_empty() {
                last_text = text.clone();
            }
        }

        // Loop guard: detect an action byte-identical to the previous turn.
        let iter_sig = iteration_signature(&response);
        let is_repeat = iter_sig.is_some() && iter_sig == last_iter_sig;
        if is_repeat {
            repeat_rounds += 1;
        } else {
            repeat_rounds = 0;
        }
        if iter_sig.is_some() {
            last_iter_sig = iter_sig;
        }
        let repeat_limit = if response.tool_calls.is_empty() {
            REPEAT_STOP_TEXT
        } else {
            REPEAT_STOP_TOOLS
        };
        if repeat_rounds >= repeat_limit {
            // The model is spinning on the same step. Drop the superseded draft and
            // finish with the last substantive thing it said.
            on_event(AgentEvent {
                kind: AgentEventKind::DiscardDraft,
                content: None,
            });
            let msg = if last_text.trim().is_empty() {
                "I stopped because I was repeating the same step without making new progress."
                    .to_string()
            } else {
                last_text.clone()
            };
            return finish_agent(session, Some(msg), memory, provider, iterations, on_event).await;
        }

        if enable_thinking {
            if let Some(text) = &response.content {
                emit_thinking_events(text, on_event);
            }
        }

        if response.tool_calls.is_empty() {
            let content_txt = response.content.as_deref().unwrap_or("");

            // Prose deliverable (PR description, commit message, release notes, …): the
            // answer IS the text the model just wrote — there are no workspace changes to
            // verify. If it produced a substantive reply and made no edits, accept it and
            // STOP. Running the strict diff-aware reviewer here would never see a file
            // change, so it would force-continue and re-emit the same text over and over.
            if auto_complete
                && task_ctx.prose_deliverable
                && work_total == 0
                && !content_txt.trim().is_empty()
                && !looks_like_user_question(content_txt)
            {
                return finish_agent(
                    session,
                    response.content,
                    memory,
                    provider,
                    iterations,
                    on_event,
                )
                .await;
            }

            // Auto-complete: the worker stopped calling tools, so it is implicitly claiming the
            // task is done. A strict, diff-aware reviewer independently checks the result against
            // the ORIGINAL task AND the real workspace changes. If genuinely complete the agent
            // QUITS immediately with this summary; otherwise we force it to keep going (and to
            // emit a real tool call). We run the reviewer even before any mutation so a model that
            // merely NARRATES a file (never calling write_file) is caught and forced to actually
            // write it. The stall detector below guarantees this can never loop forever.
            if auto_complete
                && !looks_like_user_question(content_txt)
                && force_continue < MAX_FORCE_CONTINUE
            {
                let final_text = if content_txt.trim().is_empty() {
                    last_text.as_str()
                } else {
                    content_txt
                };
                let workspace = workspace_changes_brief(registry).await;
                let verdict =
                    verify_task_complete(session, &task_ctx, final_text, &workspace, provider)
                        .await;
                let done = completion_accepted(&verdict, work_total, session, &task_ctx);
                // Only a VERIFIED reviewer may override the model's decision to stop. If
                // the reviewer call itself failed (`verified == false`) and we still did
                // not accept (action task, nothing done yet), fall through to the
                // narration nudge instead of force-continuing on fabricated "outstanding
                // items" — otherwise a provider whose review endpoint is down would end
                // every run with the confusing "completion check unavailable" message.
                if !done && verdict.verified {
                    let progressed = work_total > work_at_last_force;
                    let stalled = is_stalled(
                        progressed,
                        &verdict.missing,
                        &mut last_missing,
                        &mut stall_rounds,
                        MAX_STALL_ROUNDS,
                    );
                    if stalled {
                        // Genuinely stuck: stop cleanly and tell the user what's blocking rather
                        // than spinning. Drop the streamed draft first (see ordering note below).
                        on_event(AgentEvent {
                            kind: AgentEventKind::DiscardDraft,
                            content: None,
                        });
                        let blocked = format_missing(&verdict.missing);
                        let msg = format!(
                            "{}\n\nI couldn't fully finish — these items still look incomplete after \
                             several attempts:\n{blocked}",
                            final_text.trim()
                        );
                        return finish_agent(
                            session,
                            Some(msg),
                            memory,
                            provider,
                            iterations,
                            on_event,
                        )
                        .await;
                    }
                    force_continue += 1;
                    work_at_last_force = work_total;
                    nudge_count = 0;
                    // Drop the summary we just streamed FIRST, before any other event. The client
                    // discards the last streamed draft by the handle it is still holding; emitting
                    // a status event (e.g. Reflecting) first would detach that handle and leave the
                    // superseded summary on screen as a duplicate.
                    on_event(AgentEvent {
                        kind: AgentEventKind::DiscardDraft,
                        content: None,
                    });
                    let remaining = format_missing(&verdict.missing);
                    on_event(AgentEvent {
                        kind: AgentEventKind::Reflecting,
                        content: Some(format!(
                            "Not finished yet — continuing automatically \
                             ({force_continue}/{MAX_FORCE_CONTINUE}).\n{remaining}"
                        )),
                    });
                    session.push_message(ToolMessage::system(format!(
                        "A completion reviewer checked your work against the ACTIVE TASK and the \
                         actual workspace and found it is NOT yet complete. Outstanding \
                         items:\n{remaining}\n\nResume from where you left off — finish only \
                         these remaining items with the appropriate tools. Do NOT restart from \
                         scratch or re-summarize work already done."
                    )));
                    continue;
                }
                // Accepted as complete (reviewer said done, or reviewer was down but this
                // isn't an action task that did nothing). Otherwise fall through so the
                // narration nudge below can push an action task that hasn't acted yet.
                if done {
                    return finish_agent(
                        session,
                        response.content,
                        memory,
                        provider,
                        iterations,
                        on_event,
                    )
                    .await;
                }
            }

            // No reviewable progress yet. In action modes the model often narrates ("I'll write
            // the file now") without emitting a tool call, so nothing actually happens. Nudge it to
            // run the tools. This counts CONSECUTIVE narrations (reset whenever it actually calls a
            // tool), so a long, productive run is never cut off just because it paused to narrate.
            const MAX_NUDGES: u32 = 6;
            let described_only = !looks_like_user_question(content_txt)
                && task_ctx.requires_tools
                && nudge_count < MAX_NUDGES
                && !tool_defs.is_empty()
                && session.max_iterations > 1
                && iterations < session.max_iterations;
            if described_only {
                if !last_text.trim().is_empty() {
                    on_event(AgentEvent {
                        kind: AgentEventKind::DiscardDraft,
                        content: None,
                    });
                }
                nudge_count += 1;
                session.push_message(ToolMessage::system(
                    "You replied without calling any tool, so the task has NOT been performed yet \
                     and no files have changed. Call the appropriate tools NOW (write_file, \
                     patch_file, move_file, delete_file, run_command, etc.) to do the work end to \
                     end, then verify with git_diff. Do not describe what you will do — do it. Only \
                     ask the user a question if you are genuinely blocked, or the action is \
                     ambiguous or destructive."
                        .to_string(),
                ));
                continue;
            }
            return finish_agent(session, response.content, memory, provider, iterations, on_event)
                .await;
        }

        if !use_streaming {
            if let Some(text) = &response.content {
                if !text.is_empty() {
                    on_event(AgentEvent {
                        kind: AgentEventKind::Response,
                        content: Some(text.clone()),
                    });
                }
            }
        }

        // The model is making progress (it called tools), so refill the nudge budget
        // and stop forcing tool calls: the limit is for consecutive empty replies,
        // not the whole run. An identical repeat is NOT progress — counting it would
        // reset stall detection and let the agent redo the same work indefinitely.
        nudge_count = 0;
        if !is_repeat {
            work_total += response
                .tool_calls
                .iter()
                .filter(|tc| counts_as_work_progress(&tc.name, &tc.arguments))
                .count();
        }
        session.push_message(ToolMessage::assistant_tool_calls(
            response.tool_calls.clone(),
        ));
        execute_tool_calls(&response.tool_calls, registry, options, session, on_event).await;

        if is_repeat {
            // One identical repeat (below the hard stop): tell the model plainly so it
            // can break the loop on the next turn instead of redoing the same step.
            session.push_message(ToolMessage::system(
                "That step was identical to your previous one and has already taken effect. \
                 Do NOT repeat it. Either perform the NEXT remaining step, or — if everything \
                 the task asked for is done — stop and reply with a brief final summary."
                    .to_string(),
            ));
        }

        if enable_thinking {
            session.push_message(ToolMessage::system(thinking::REFLECTION_PROMPT.to_string()));
        }
    }
}

/// True for tools that actually change the workspace (file writes/moves/deletes).
fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "write_file" | "patch_file" | "move_file" | "delete_file")
}

/// True when a tool call performs real work (not read-only inspection).
fn counts_as_work_progress(name: &str, arguments: &serde_json::Value) -> bool {
    if is_mutating_tool(name) {
        return true;
    }
    if name != "run_command" {
        return false;
    }
    let cmd = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if cmd.is_empty() {
        return false;
    }
    const READ_ONLY: &[&str] = &[
        "git status",
        "git diff",
        "git log",
        "git show",
        "ls ",
        "cat ",
        "head ",
        "tail ",
        "find ",
        "grep ",
        "rg ",
        "pwd",
        "which ",
        "echo ",
    ];
    if READ_ONLY.iter().any(|k| cmd.contains(k)) {
        return false;
    }
    const SUBSTANTIVE: &[&str] = &[
        "git commit",
        "git push",
        "git add",
        "git checkout",
        "git merge",
        "git pull",
        "git stash",
        "npm install",
        "npm run",
        "pip install",
        "uv run",
        "pytest",
        "cargo build",
        "docker compose",
        "docker-compose",
        "make ",
        "manage.py migrate",
    ];
    SUBSTANTIVE.iter().any(|k| cmd.contains(k))
}

/// Verdict from the completion reviewer ("manager") about whether the original task is done.
struct CompletionVerdict {
    done: bool,
    missing: Vec<String>,
    /// False when the reviewer call itself failed (provider down), so the caller
    /// must NOT treat the verdict as authoritative — never quit a task with no work
    /// done just because the completion check was unavailable.
    verified: bool,
}

/// A concise snapshot of the REAL workspace changes (modified + created files),
/// so the completion reviewer can tell whether files the agent *claims* it wrote
/// actually exist — instead of trusting the chat text. Prefers `git status`
/// (lists modified + untracked/created files), falling back to the session's
/// tracked edits via `git_diff` for non-git workspaces.
async fn workspace_changes_brief(registry: &ToolRegistry) -> String {
    if let Some(tool) = registry.get("git_status") {
        if let Ok(v) = tool.execute(serde_json::json!({})).await {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("").trim();
            if !status.is_empty() {
                return truncate_brief(status, 1500);
            }
        }
    }
    if let Some(tool) = registry.get("git_diff") {
        if let Ok(v) = tool.execute(serde_json::json!({})).await {
            let diff = v.get("diff").and_then(|s| s.as_str()).unwrap_or("").trim();
            if !diff.is_empty() {
                return truncate_brief(diff, 1500);
            }
        }
    }
    "(no file changes detected in the workspace this session)".to_string()
}

/// Truncate a brief to a character budget on a char boundary, with a marker.
fn truncate_brief(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\n…(truncated)", &s[..i]),
        None => s.to_string(),
    }
}

/// Update stall tracking from a "not done" verdict. Returns true when the run is
/// genuinely stuck for `max_stall` consecutive reviews. This is what lets the agent
/// run as long as it's productive while still terminating on a model that can't make
/// headway.
///
/// A review only ever runs when the model has *stopped calling tools* (it thinks it
/// is done) or it hit the step ceiling. At that point there are two reliable signals
/// that it's spinning rather than finishing:
///   * NO NEW substantive work since the previous review — it just re-read / re-ran
///     inspections and claimed done again (the classic "explored, fixed it, then kept
///     re-investigating instead of stopping" loop), OR
///   * the reviewer keeps reporting ROUGHLY THE SAME outstanding items — the agent and
///     reviewer disagree and it isn't getting resolved.
///
/// Crucially we do NOT require the `missing` list to be byte-identical: it's
/// free-form text from an LLM reviewer that re-words itself every round, so an exact
/// match almost never holds and would let the run force-continue indefinitely. We
/// compare it fuzzily (token overlap) instead. The counter only resets when the run
/// made real progress AND moved on to genuinely different outstanding work.
fn is_stalled(
    progressed: bool,
    missing: &[String],
    last_missing: &mut Vec<String>,
    stall_rounds: &mut u32,
    max_stall: u32,
) -> bool {
    let same_gaps = missing_roughly_same(missing, last_missing);
    if !progressed || same_gaps {
        *stall_rounds += 1;
    } else {
        *stall_rounds = 0;
    }
    *last_missing = missing.to_vec();
    *stall_rounds >= max_stall
}

/// Fuzzy comparison of two "outstanding items" lists from the completion reviewer.
/// The reviewer is an LLM that re-words the same gaps every round, so exact equality
/// is useless for detecting "stuck on the same thing". We reduce each list to a set
/// of meaningful word tokens and treat them as the same when their Jaccard overlap is
/// high. Two empty lists count as the same (no concrete gaps either time).
fn missing_roughly_same(a: &[String], b: &[String]) -> bool {
    let sa = normalize_missing_tokens(a);
    let sb = normalize_missing_tokens(b);
    if sa.is_empty() && sb.is_empty() {
        return true;
    }
    if sa.is_empty() || sb.is_empty() {
        return false;
    }
    let intersection = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    #[allow(clippy::cast_precision_loss)]
    let jaccard = intersection as f32 / union as f32;
    jaccard >= 0.6
}

/// Reduce a list of outstanding-item strings to a set of normalized word tokens
/// (lowercased, punctuation-stripped, short/noise words dropped) for fuzzy matching.
fn normalize_missing_tokens(items: &[String]) -> std::collections::HashSet<String> {
    items
        .iter()
        .flat_map(|s| s.split_whitespace())
        .map(|w| {
            w.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| w.len() > 2)
        .collect()
}

/// A stable fingerprint of one model turn, used to detect a spinning loop where
/// the model redoes the identical action. When the turn has tool calls, only the
/// calls matter (name + canonical args) — narration around them often varies even
/// when the action is the same. With no tool calls, the normalized answer text is
/// the fingerprint. Returns None for an empty turn (nothing to compare).
fn iteration_signature(response: &ToolChatResponse) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if response.tool_calls.is_empty() {
        let text = response.content.as_deref().unwrap_or("").trim();
        if text.is_empty() {
            return None;
        }
        "text".hash(&mut h);
        normalize_for_sig(text).hash(&mut h);
    } else {
        "tools".hash(&mut h);
        for c in &response.tool_calls {
            c.name.hash(&mut h);
            // serde_json sorts object keys by default, so this is canonical for
            // identical argument sets.
            serde_json::to_string(&c.arguments)
                .unwrap_or_default()
                .hash(&mut h);
        }
    }
    Some(h.finish())
}

/// Normalize text for repeat comparison: collapse whitespace and lowercase, so
/// trivial reformatting doesn't hide that the same answer was produced again.
fn normalize_for_sig(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Render the reviewer's outstanding items as a short bullet list for prompts/events.
fn format_missing(missing: &[String]) -> String {
    if missing.is_empty() {
        "Some requested work is still incomplete.".to_string()
    } else {
        missing
            .iter()
            .map(|m| format!("- {m}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A compact, truncated tail of the conversation so the reviewer sees recent activity
/// without paying for the whole transcript.
fn recent_activity_brief(messages: &[ToolMessage]) -> String {
    messages
        .iter()
        .rev()
        .take(14)
        .filter_map(|m| {
            let c = m.content.as_deref().unwrap_or("").trim();
            if c.is_empty() {
                return None;
            }
            let cut = c.char_indices().nth(220).map_or(c.len(), |(i, _)| i);
            Some(format!("[{}] {}", m.role, &c[..cut]))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the reviewer's JSON verdict. Fails OPEN (treats as done) on any parse problem so a
/// malformed reply can never trap the agent in a forced-continue loop.
fn parse_verdict(s: &str) -> CompletionVerdict {
    let (open, close) = match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if a < b => (a, b),
        _ => {
            return CompletionVerdict {
                done: true,
                missing: Vec::new(),
                verified: true,
            }
        }
    };
    match serde_json::from_str::<serde_json::Value>(&s[open..=close]) {
        Ok(v) => {
            let done = v.get("done").and_then(|x| x.as_bool()).unwrap_or(true);
            let missing = v
                .get("missing")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|i| i.as_str())
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            CompletionVerdict {
                done,
                missing,
                verified: true,
            }
        }
        Err(_) => CompletionVerdict {
            done: true,
            missing: Vec::new(),
            verified: true,
        },
    }
}

/// Resolved view of what the agent is actually working on in a multi-turn chat.
struct TaskContext {
    /// Latest user message for this run (e.g. "ok").
    current_input: String,
    /// The real task to judge completion against (may be an earlier user turn).
    effective_task: String,
    /// True when `current_input` is a short follow-up, not a new substantive request.
    is_follow_up: bool,
    /// True when the effective task expects file/shell mutations (not explain-only).
    requires_tools: bool,
    /// True when the deliverable is prose the user reads/copies (PR description,
    /// commit message, etc.) — satisfied by the text itself, with no workspace
    /// changes to verify, so it must not be force-continued by the diff reviewer.
    prose_deliverable: bool,
}

/// Ask the same model, acting as a strict completion reviewer ("manager"), whether the active
/// task is fully done given conversation progress so far.
async fn verify_task_complete(
    session: &Session,
    ctx: &TaskContext,
    final_text: &str,
    workspace: &str,
    provider: &Arc<dyn Provider>,
) -> CompletionVerdict {
    let recent = recent_activity_brief(&session.messages);
    let progress = prior_progress_brief(&session.messages);
    let system = "You are a STRICT completion reviewer for an autonomous coding agent. Given the \
        CURRENT USER MESSAGE, the ACTIVE TASK (the real work item in this thread), what the agent \
        has already done in this conversation, the agent's latest message, recent activity, and \
        ACTUAL WORKSPACE CHANGES, decide whether the ACTIVE TASK is FULLY complete. Reply with \
        ONLY compact JSON: {\"done\": true|false, \"missing\": [\"specific unfinished item\"]}. \
        Judge ONLY against what the ACTIVE TASK asked for — NOT follow-ups like \"ok\" or \"thanks\". \
        CRUCIALLY: trust WORKSPACE CHANGES over claims. If the agent says it wrote a file but it \
        does NOT appear in workspace changes, set done=false. If the ACTIVE TASK was a \
        question/explanation and a prior assistant message in PROGRESS SO FAR already fully \
        answered it, set done=true even when the CURRENT USER MESSAGE is only an acknowledgment \
        (ok, great, thanks) and the latest reply is brief. For commit/push/git tasks: if PROGRESS \
        SO FAR shows git commit and/or push already succeeded, set done=true — a clean working \
        tree afterward is expected, not evidence of missing work. IGNORE offers and optional next \
        steps. Never output anything except the JSON object.";
    let user = format!(
        "CURRENT USER MESSAGE:\n{}\n\nACTIVE TASK:\n{}\n\nPROGRESS SO FAR (prior work in this \
         thread):\n{}\n\nAGENT'S LATEST MESSAGE:\n{}\n\nWORKSPACE CHANGES (actual files changed \
         this session):\n{}\n\nRECENT ACTIVITY:\n{recent}",
        ctx.current_input,
        ctx.effective_task,
        progress,
        final_text,
        workspace,
    );
    // On GetAIBD, route this lightweight review to a cheap fast model. Other
    // providers (BYOK) keep using the user's selected model, since a GetAIBD
    // catalog id wouldn't be valid for them.
    let review_model = if session.provider_id == "getaibd" {
        completion_model()
    } else {
        session.model.clone()
    };
    let request = ToolChatRequest {
        model: review_model,
        messages: vec![ToolMessage::system(system), ToolMessage::user(user)],
        tools: Vec::new(),
        temperature: Some(0.0),
        max_tokens: Some(500),
        reasoning_effort: None,
        tool_choice: None,
        compress: false,
        cache_session_id: session.cache_session_id.clone(),
    };
    match chat_with_tools_retry_cb(provider, &request, None).await {
        Ok(r) => parse_verdict(r.content.as_deref().unwrap_or("")),
        // The reviewer call itself failed (e.g. completion model is down). Mark the
        // verdict unverified so the caller decides safely: accept completion only if
        // real work was actually done, otherwise keep going (bounded by stall
        // detection) rather than quitting a task with nothing accomplished.
        Err(_) => CompletionVerdict {
            done: false,
            missing: vec!["completion check unavailable — verify work was finished".to_string()],
            verified: false,
        },
    }
}

/// True when the thread already has a substantive assistant reply the user may be
/// building on (exploration, findings, proposed fixes).
fn has_prior_assistant_turn(messages: &[ToolMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "assistant"
            && m.content.as_deref().is_some_and(|c| c.trim().len() > 120)
    })
}

/// Continuation directives: the user wants the agent to act on prior findings, not
/// re-explore ("fix those issues", "apply your suggestions", "go ahead", …).
fn is_continuation_request(text: &str, messages: &[ToolMessage]) -> bool {
    if !has_prior_assistant_turn(messages) {
        return false;
    }
    if is_short_follow_up(text) {
        return true;
    }
    let lower = text.trim().to_lowercase();
    const PHRASES: &[&str] = &[
        "fix it",
        "fix that",
        "fix those",
        "fix this",
        "fix them",
        "apply",
        "implement",
        "go ahead",
        "do it",
        "do that",
        "do those",
        "make the change",
        "make those",
        "make that",
        "yes fix",
        "yes please",
        "please fix",
        "please apply",
        "address those",
        "address the",
        "apply your",
        "apply the",
        "implement your",
        "implement the",
        "implement those",
        "proceed",
        "carry out",
        "go for it",
        "those issues",
        "those fixes",
        "your suggestion",
        "your suggestions",
        "what you suggested",
        "what you found",
        "what you identified",
        "the issues you",
        "the fix you",
        "the changes you",
        "ship it",
    ];
    if PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // Short directive after a long assistant reply (e.g. "fix them" / "apply now").
    if lower.len() <= 80 && lower.split_whitespace().count() <= 12 {
        const VERBS: &[&str] = &[
            "fix", "apply", "implement", "change", "update", "patch", "ship", "commit",
        ];
        if VERBS.iter().any(|v| lower.contains(v)) {
            return true;
        }
    }
    false
}

/// Short follow-ups that refer to the prior substantive turn, not a new task.
fn is_short_follow_up(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    if t.len() > 28 {
        return false;
    }
    let lower = t.to_lowercase();
    let normalized: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
    const EXACT: &[&str] = &[
        "ok", "okay", "k", "kk", "yep", "yeah", "yes", "no", "nope", "nah", "thanks", "thankyou",
        "thx", "ty", "great", "cool", "nice", "good", "gotit", "understood", "sure", "fine",
        "alright", "right", "so", "hi", "hello", "hey", "bye", "done", "perfect", "awesome",
        "listed", "hm", "hmm",
    ];
    if EXACT.iter().any(|w| normalized == *w) {
        return true;
    }
    matches!(lower.as_str(), "so?" | "and?" | "and so?" | "now what?")
}

fn is_substantive_user_message(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && !is_short_follow_up(t)
}

/// Last substantive user request in the thread (skips "ok", "thanks", etc.).
fn last_substantive_user_task(messages: &[ToolMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .rev()
        .find(|t| is_substantive_user_message(t))
        .map(|s| s.to_string())
}

/// Requests whose deliverable is prose the user will read or copy — a PR/MR
/// description, commit message, release notes, etc. These are satisfied by the
/// text the model writes; there are NO file or shell changes to make, so the
/// diff-aware completion reviewer must not force them to keep going. Distinct
/// from "write a file/function/test", which genuinely needs tools.
fn looks_like_prose_deliverable(task: &str) -> bool {
    let lower = task.to_lowercase();

    // Unambiguous prose artifacts — always prose, whatever verb is used.
    const STRONG: &[&str] = &[
        "pr description",
        "pr desc",
        "pr message",
        "pr summary",
        "pull request description",
        "pull-request description",
        "merge request description",
        "mr description",
        "commit message",
        "release notes",
    ];
    if STRONG.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // "write / draft / compose / give me a <prose-noun>" — but NOT when the object
    // is clearly a file, code, or something written to disk (those need tools).
    let has_gen_verb = [
        "write ",
        "draft ",
        "compose ",
        "give me ",
        "generate ",
        "rewrite ",
        "reword ",
        "rephrase ",
        "summarize ",
        "summarise ",
    ]
    .iter()
    .any(|v| lower.contains(v));
    if !has_gen_verb {
        return false;
    }
    let has_prose_noun = [
        "description",
        "message",
        "summary",
        "blurb",
        "paragraph",
        "reply",
        "response",
        "email",
        "caption",
        "tagline",
        "headline",
        "explanation",
        "write-up",
        "writeup",
    ]
    .iter()
    .any(|n| lower.contains(n));
    if !has_prose_noun {
        return false;
    }
    let targets_file_or_code = [
        "file",
        "function",
        "class",
        "method",
        "module",
        "test",
        "script",
        "config",
        "endpoint",
        "component",
        "schema",
        "migration",
        "readme",
        "changelog",
        "docstring",
        "doc string",
        "to disk",
        "into ",
        ".md",
        ".py",
        ".rs",
        ".ts",
        ".js",
        ".json",
        ".yml",
        ".yaml",
        ".txt",
    ]
    .iter()
    .any(|c| lower.contains(c));
    !targets_file_or_code
}

fn task_requires_tools(task: &str) -> bool {
    if looks_like_info_request(task) || looks_like_prose_deliverable(task) {
        return false;
    }
    let lower = task.to_lowercase();
    [
        "implement",
        "create",
        "add",
        "build",
        "write",
        "modify",
        "update",
        "refactor",
        "change",
        "fix",
        "delete",
        "move",
        "patch",
        "setup",
        "install",
        "run ",
        "migrate",
        "commit",
        "push",
        "pull",
        "git ",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn resolve_task_context(session: &Session, current: &str) -> TaskContext {
    let is_continuation = is_continuation_request(current, &session.messages);
    let is_follow_up = is_short_follow_up(current) || is_continuation;
    let effective_task = if is_follow_up {
        last_substantive_user_task(&session.messages)
            .filter(|t| t.trim() != current.trim())
            .unwrap_or_else(|| current.to_string())
    } else {
        current.to_string()
    };
    let requires_tools = task_requires_tools(&effective_task);
    let prose_deliverable = looks_like_prose_deliverable(&effective_task);
    TaskContext {
        current_input: current.to_string(),
        effective_task,
        is_follow_up,
        requires_tools,
        prose_deliverable,
    }
}

/// Prior assistant answers and tool use so the reviewer can tell what's already done.
fn prior_progress_brief(messages: &[ToolMessage]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "assistant" => {
                if let Some(c) = m.content.as_deref() {
                    let t = c.trim();
                    if !t.is_empty() {
                        let cut = t.char_indices().nth(400).map_or(t.len(), |(i, _)| i);
                        lines.push(format!("Assistant: {}", &t[..cut]));
                    }
                }
                if let Some(calls) = &m.tool_calls {
                    for call in calls {
                        lines.push(format!("Tool called: {}", call.name));
                    }
                }
            }
            "tool" => {
                if let Some(c) = m.content.as_deref() {
                    let t = c.trim();
                    if !t.is_empty() {
                        let cut = t.char_indices().nth(160).map_or(t.len(), |(i, _)| i);
                        let label = m.name.as_deref().unwrap_or("tool");
                        lines.push(format!("Tool result ({label}): {}", &t[..cut]));
                    }
                } else if let Some(name) = &m.name {
                    lines.push(format!("Tool result: {name}"));
                }
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        "(no prior progress in this thread)".to_string()
    } else {
        // Keep the tail — most recent work matters most for resume vs restart.
        lines.into_iter().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

/// Explain / describe / overview requests — answer in prose, no tool nudge.
fn looks_like_info_request(task: &str) -> bool {
    let lower = task.to_lowercase();
    [
        "explain",
        "what is",
        "what's",
        "whats",
        "describe",
        "tell me",
        "how does",
        "overview",
        "summarize",
        "summary of",
        "walk me through",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn completion_accepted(
    verdict: &CompletionVerdict,
    work_total: usize,
    _session: &Session,
    ctx: &TaskContext,
) -> bool {
    if verdict.verified {
        if verdict.done {
            return true;
        }
        // Reviewer said incomplete but listed nothing — don't loop on repeat summaries.
        if verdict.missing.is_empty() && work_total > 0 {
            return true;
        }
        return false;
    }
    // Reviewer UNAVAILABLE (the check call itself failed): we have no independent
    // signal, so trust the model's decision to stop in every case EXCEPT an action
    // task where it did literally nothing — that one is likely just narration, and the
    // separate narration nudge will push it to actually act. Crucially we must never
    // trap a genuine completion (a Q&A/analysis answer, a prose deliverable, or work
    // that actually changed the workspace) behind a down reviewer.
    work_total > 0 || !ctx.requires_tools || ctx.prose_deliverable
}

/// True when the model's text is a genuine question/confirmation for the user (so we
/// should let it finish rather than nudging it to keep calling tools).
fn looks_like_user_question(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if t.ends_with('?') {
        return true;
    }
    let lower = t.to_lowercase();
    [
        "would you like",
        "do you want",
        "should i ",
        "shall i ",
        "could you clarify",
        "can you clarify",
        "which option",
        "let me know if",
        "please confirm",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn emit_thinking_events(text: &str, on_event: &mut impl FnMut(AgentEvent)) {
    let blocks = thinking::parse_thinking_blocks(text);
    for block in blocks {
        match block {
            thinking::ThinkingBlock::Plan(content) => {
                on_event(AgentEvent {
                    kind: AgentEventKind::Planning,
                    content: Some(content),
                });
            }
            thinking::ThinkingBlock::Thinking(content) => {
                on_event(AgentEvent {
                    kind: AgentEventKind::Thinking,
                    content: Some(content),
                });
            }
            thinking::ThinkingBlock::Reflection(content) => {
                if thinking::reflection_has_replan(&content) {
                    on_event(AgentEvent {
                        kind: AgentEventKind::Replanning,
                        content: Some(content),
                    });
                } else {
                    on_event(AgentEvent {
                        kind: AgentEventKind::Reflecting,
                        content: Some(content),
                    });
                }
            }
            thinking::ThinkingBlock::Text(_) => {}
        }
    }
}

/// Max time to wait for the NEXT meaningful streaming delta (token or tool-call
/// data) before treating the turn as a stalled stream. Generous enough for a slow
/// first token under heavy reasoning, but bounded so a heartbeat-only/black-holed
/// stream can't hang the agent forever.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

async fn collect_streaming_response(
    provider: &Arc<dyn Provider>,
    request: ToolChatRequest,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<ToolChatResponse, AppError> {
    use futures::StreamExt;

    let mut stream = provider.chat_with_tools_stream(request);
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_args = String::new();
    let mut current_tool_extra: Option<serde_json::Value> = None;
    // Separates inline reasoning (<thinking>/<think>/<plan>/<reflection> tags some
    // models emit in their content stream) from the visible answer, so chain-of-thought
    // goes to the collapsible thinking block instead of leaking into the reply. Returns
    // only the NEW clean / reasoning text on each push so streaming stays incremental.
    let mut router = ReasoningRouter::default();

    // Idle watchdog at the *delta* level. reqwest's read_timeout only fires when no
    // BYTES arrive, but a gateway that emits SSE keepalive/heartbeat bytes during a
    // stalled generation keeps resetting it while producing no real output — so the
    // model can silently black-hole and the agent hangs forever at "Agent working…".
    // Those keepalives yield no ToolStreamDelta, so bounding the wait for the NEXT
    // delta turns an infinite stall into a clean, recoverable timeout. The window is
    // generous so a slow first token on heavy-reasoning models is never cut off.
    loop {
        let delta = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(delta)) => delta,
            Ok(None) => break,
            Err(_) => {
                return Err(AppError::ProviderTimeout(format!(
                    "stream stalled: no model output for {}s",
                    STREAM_IDLE_TIMEOUT.as_secs()
                )));
            }
        };
        match delta? {
            ToolStreamDelta::Token(token) => {
                let (visible, reasoning) = router.push(&token);
                if !reasoning.is_empty() {
                    on_event(AgentEvent {
                        kind: AgentEventKind::Thinking,
                        content: Some(reasoning),
                    });
                }
                if !visible.is_empty() {
                    content.push_str(&visible);
                    on_event(AgentEvent {
                        kind: AgentEventKind::Response,
                        content: Some(visible),
                    });
                }
            }
            ToolStreamDelta::Reasoning(reasoning) => {
                if !reasoning.trim().is_empty() {
                    on_event(AgentEvent {
                        kind: AgentEventKind::Thinking,
                        content: Some(reasoning),
                    });
                }
            }
            ToolStreamDelta::ToolCallStart { id, name, extra } => {
                if !current_tool_id.is_empty() {
                    let args = parse_tool_arguments(&current_tool_args);
                    tool_calls.push(ToolCall {
                        id: current_tool_id.clone(),
                        name: current_tool_name.clone(),
                        arguments: args,
                        extra: current_tool_extra.take(),
                    });
                    current_tool_args.clear();
                }
                current_tool_id = id;
                current_tool_name = name;
                current_tool_extra = extra;
            }
            ToolStreamDelta::ToolCallArgDelta(args) => {
                current_tool_args.push_str(&args);
            }
            ToolStreamDelta::ToolCallEnd | ToolStreamDelta::Done => {
                if !current_tool_id.is_empty() {
                    let args = parse_tool_arguments(&current_tool_args);
                    tool_calls.push(ToolCall {
                        id: current_tool_id.clone(),
                        name: current_tool_name.clone(),
                        arguments: args,
                        extra: current_tool_extra.take(),
                    });
                    current_tool_id.clear();
                    current_tool_name.clear();
                    current_tool_args.clear();
                }
            }
        }
    }

    // Flush anything the router withheld as a possible-but-incomplete tag (e.g. a
    // trailing bare "<"): the stream ended, so it can only be literal answer text.
    let (visible, reasoning) = router.flush();
    if !reasoning.is_empty() {
        on_event(AgentEvent {
            kind: AgentEventKind::Thinking,
            content: Some(reasoning),
        });
    }
    if !visible.is_empty() {
        content.push_str(&visible);
        on_event(AgentEvent {
            kind: AgentEventKind::Response,
            content: Some(visible),
        });
    }

    Ok(ToolChatResponse {
        content: if content.trim().is_empty() {
            None
        } else {
            Some(content)
        },
        tool_calls,
        usage: None,
    })
}

/// Incrementally separates inline chain-of-thought (wrapped in `<thinking>`,
/// `<think>`, `<plan>`, or `<reflection>` tags, as several models emit when asked to
/// reason) from the visible answer in a token stream. Each `push` returns only the
/// NEW `(visible, reasoning)` text produced by that token, so the caller can forward
/// the answer and the reasoning to different UI channels live. Robust to tags that
/// span multiple tokens and to malformed/half-written tags — reasoning is never
/// leaked into the answer, and a partial tag at the tail is withheld until it
/// resolves (then flushed by `flush` when the stream ends).
#[derive(Default)]
struct ReasoningRouter {
    /// Raw content accumulated so far (every Token concatenated).
    acc: String,
    /// Bytes of clean visible text already returned to the caller.
    visible_emitted: usize,
    /// Bytes of reasoning text already returned to the caller.
    reasoning_emitted: usize,
}

const REASONING_TAGS: &[&str] = &["thinking", "reflection", "plan", "think"];

impl ReasoningRouter {
    fn push(&mut self, token: &str) -> (String, String) {
        self.acc.push_str(token);
        self.emit_new(false)
    }

    fn flush(&mut self) -> (String, String) {
        self.emit_new(true)
    }

    /// Re-split the full accumulator and return the portion not yet emitted on each
    /// channel. `final_pass` treats a trailing partial tag as literal answer text.
    fn emit_new(&mut self, final_pass: bool) -> (String, String) {
        let (visible, reasoning) = split_reasoning(&self.acc, final_pass);
        let vis_new = visible.get(self.visible_emitted..).unwrap_or("").to_string();
        let rea_new = reasoning.get(self.reasoning_emitted..).unwrap_or("").to_string();
        self.visible_emitted = visible.len();
        self.reasoning_emitted = reasoning.len();
        (vis_new, rea_new)
    }
}

enum TagMatch {
    Open(usize),
    Close(usize),
    /// `<` begins a recognized tag but the rest hasn't arrived yet.
    Partial,
    /// `<` is literal text, not the start of a reasoning tag.
    NotTag,
}

/// Classify the `<...` at the start of `s` (which must begin with `<`).
fn identify_tag(s: &str) -> TagMatch {
    for tag in REASONING_TAGS {
        let open = format!("<{tag}>");
        if s.starts_with(&open) {
            return TagMatch::Open(open.len());
        }
        let close = format!("</{tag}>");
        if s.starts_with(&close) {
            return TagMatch::Close(close.len());
        }
    }
    // Could this still become a tag once more bytes arrive?
    for tag in REASONING_TAGS {
        if format!("<{tag}>").starts_with(s) || format!("</{tag}>").starts_with(s) {
            return TagMatch::Partial;
        }
    }
    TagMatch::NotTag
}

/// Split accumulated content into `(visible_answer, reasoning)`, dropping the tag
/// markers themselves. Withholds a trailing partial tag unless `final_pass` is set.
fn split_reasoning(acc: &str, final_pass: bool) -> (String, String) {
    let mut visible = String::new();
    let mut reasoning = String::new();
    let mut inside = false;
    let mut rest = acc;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            if inside {
                reasoning.push_str(rest);
            } else {
                visible.push_str(rest);
            }
            break;
        };
        let (before, from_lt) = rest.split_at(lt);
        if inside {
            reasoning.push_str(before);
        } else {
            visible.push_str(before);
        }
        match identify_tag(from_lt) {
            TagMatch::Open(len) => {
                inside = true;
                rest = &from_lt[len..];
            }
            TagMatch::Close(len) => {
                inside = false;
                rest = &from_lt[len..];
            }
            TagMatch::Partial => {
                if final_pass {
                    // No more tokens coming — treat the leftover as literal text.
                    if inside {
                        reasoning.push_str(from_lt);
                    } else {
                        visible.push_str(from_lt);
                    }
                }
                break;
            }
            TagMatch::NotTag => {
                if inside {
                    reasoning.push('<');
                } else {
                    visible.push('<');
                }
                rest = &from_lt[1..];
            }
        }
    }
    (visible, reasoning)
}

/// Leading messages that compression must never touch: the system prefix
/// (system prompt, environment note, injected memory/context) plus the first
/// user message (the original task). Without this, summarizing the front of the
/// list makes the agent forget its instructions and its goal.
fn pinned_head_len(messages: &[ToolMessage]) -> usize {
    let mut i = 0;
    while i < messages.len() && messages[i].role == "system" {
        i += 1;
    }
    if i < messages.len() && messages[i].role == "user" {
        i += 1;
    }
    i
}

/// Replaces the older MIDDLE of the conversation with a faithful LLM-generated
/// summary, keeping the pinned head (system prompt + original task) and the most
/// recent turns verbatim. Falls back to truncation if the model call fails.
/// Summarize the older middle of the conversation, keeping the pinned head
/// (system prompt + original task) and a verbatim recent tail. Returns true if it
/// actually summarized. Never drops a turn without folding it into the summary.
async fn summarize_old_messages(session: &mut Session, provider: &Arc<dyn Provider>) -> bool {
    let pinned = pinned_head_len(&session.messages);
    let total = session.messages.len();
    // Keep a generous, verbatim recent tail so in-flight work stays intact.
    let keep_tail = (total / 2).max(10);
    // Bail unless there's a meaningful middle to compress between head and tail.
    if total <= pinned + keep_tail + 3 {
        return false;
    }
    let mut summarize_end = total - keep_tail;
    // Never let the kept tail begin with a `tool` message whose matching
    // assistant tool_call is in the summarized middle — that orphan would make the
    // next provider request invalid. Absorb such leading tool results into the
    // summary by extending the boundary forward.
    while summarize_end < total && session.messages[summarize_end].role == "tool" {
        summarize_end += 1;
    }
    if summarize_end <= pinned {
        return false;
    }
    let old: Vec<ToolMessage> = session
        .messages
        .splice(pinned..summarize_end, std::iter::empty())
        .collect();

    let transcript = old
        .iter()
        .filter_map(|m| {
            let c = m.content.as_deref().unwrap_or("");
            if c.is_empty() {
                None
            } else {
                Some(format!("[{}] {}", m.role, c))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if transcript.trim().is_empty() {
        return false;
    }

    let req = ChatRequest {
        provider: session.provider_id.clone(),
        model: session.model.clone(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "You compress a coding agent's conversation. Produce a concise but \
                          lossless summary that preserves: the user's goal, decisions made, \
                          files created or modified, important facts learned about the codebase, \
                          the current task state, and any unresolved TODOs. Use short bullet points."
                    .to_string(),
            },
            Message {
                role: "user".to_string(),
                content: transcript,
            },
        ],
        temperature: Some(0.2),
        max_tokens: Some(800),
        reasoning_effort: None,
        api_key: None,
        cache_session_id: session.cache_session_id.clone(),
    };

    let summary = match provider.chat(&req).await {
        Ok(resp) if !resp.content.trim().is_empty() => resp.content,
        _ => old
            .iter()
            .filter_map(|m| {
                let c = m.content.as_deref().unwrap_or("");
                if c.is_empty() {
                    None
                } else {
                    let cut = c.char_indices().nth(200).map_or(c.len(), |(i, _)| i);
                    Some(format!("[{}] {}", m.role, &c[..cut]))
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };

    let insert_at = pinned_head_len(&session.messages);
    session.messages.insert(
        insert_at,
        ToolMessage::system(format!(
            "Summary of earlier steps (older detail compressed; the system instructions \
             and original task above still apply):\n{summary}"
        )),
    );
    true
}

async fn finish_agent(
    session: &Session,
    content: Option<String>,
    memory: Option<&MemoryContext<'_>>,
    provider: &Arc<dyn Provider>,
    iterations: u32,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let final_response = content.unwrap_or_default();
    on_event(AgentEvent {
        kind: AgentEventKind::Complete,
        content: Some(final_response.clone()),
    });

    if let Some(mem) = memory {
        // Reflection is an extra LLM round-trip plus embedding writes. Running it inline
        // kept the turn "working" for seconds AFTER the answer was already on screen, so
        // run it detached: the user's turn finishes immediately and memory updates land in
        // the background. Capture everything it needs as owned data first.
        let transcript = build_transcript(session);
        let provider = provider.clone();
        let store = mem.store.clone();
        let embedder = mem.embedder.clone_box();
        let session_id = session.id.clone();
        let provider_id = session.provider_id.clone();
        let model = session.model.clone();
        let cache_session_id = session.cache_session_id.clone();
        let max_entries = mem.max_entries;
        tokio::spawn(async move {
            reflect_and_index(
                transcript,
                &provider,
                store,
                embedder,
                &session_id,
                &provider_id,
                &model,
                &cache_session_id,
                max_entries,
            )
            .await;
        });
    }

    Ok(AgentResult {
        final_response,
        iterations,
        mode: None,
    })
}

/// Background memory write-back: reflect on the finished task and index the episode,
/// durable facts, and any playbook, then prune. Runs detached after the turn completes so
/// it never delays the user-visible response.
#[allow(clippy::too_many_arguments)]
async fn reflect_and_index(
    transcript: String,
    provider: &Arc<dyn Provider>,
    store: MemoryStore,
    embedder: Box<dyn EmbeddingProvider>,
    session_id: &str,
    provider_id: &str,
    model: &str,
    cache_session_id: &Option<String>,
    max_entries: usize,
) {
    let reflection = reflect(&transcript, provider, provider_id, model, cache_session_id).await;
    let indexer = MemoryIndexer::new(&store, embedder.as_ref());
    let _ = indexer.index_episode(session_id, &reflection.episode).await;

    for f in &reflection.facts {
        let fact = f.fact.trim();
        if fact.is_empty() {
            continue;
        }
        let category = if f.category.trim().is_empty() {
            "General"
        } else {
            f.category.trim()
        };
        let content = format!("Fact [{category}]: {fact}");
        let id = format!("fact-{}", stable_hash(&content));
        let _ = indexer.index_persistent_fact(&id, &content).await;
    }

    if let Some(pb) = &reflection.playbook {
        let title = pb.title.trim();
        if !title.is_empty() && !pb.steps.is_empty() {
            let steps = pb
                .steps
                .iter()
                .filter(|s| !s.trim().is_empty())
                .enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            let content = format!("Playbook: {title}\n{steps}");
            let id = format!("playbook-{}", stable_hash(title));
            let _ = indexer.index_persistent_fact(&id, &content).await;
        }
    }

    // Pruning is a synchronous rusqlite delete; offload it so it can't block the
    // async runtime (H-3).
    let _ = tokio::task::spawn_blocking(move || store.prune_oldest(max_entries)).await;
}

#[derive(Default)]
struct Reflection {
    episode: String,
    facts: Vec<ReflectFact>,
    playbook: Option<ReflectPlaybook>,
}

#[derive(serde::Deserialize)]
struct ReflectFact {
    #[serde(default)]
    category: String,
    #[serde(default)]
    fact: String,
}

#[derive(serde::Deserialize)]
struct ReflectPlaybook {
    #[serde(default)]
    title: String,
    #[serde(default)]
    steps: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ReflectionJson {
    #[serde(default)]
    episode: String,
    #[serde(default)]
    facts: Vec<ReflectFact>,
    #[serde(default)]
    playbook: Option<ReflectPlaybook>,
}

fn build_transcript(session: &Session) -> String {
    let mut transcript = String::new();
    for m in &session.messages {
        if let Some(c) = m.content.as_deref() {
            if !c.is_empty() {
                let cut = c.char_indices().nth(600).map_or(c.len(), |(i, _)| i);
                transcript.push_str(&format!("[{}] {}\n", m.role, &c[..cut]));
            }
        }
        if let Some(calls) = &m.tool_calls {
            for call in calls {
                transcript.push_str(&format!("[{} -> {}]\n", m.role, call.name));
            }
        }
    }
    transcript.trim().to_string()
}

fn stable_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn parse_reflection(raw: &str) -> Option<ReflectionJson> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&raw[start..=end]).ok()
}

/// Single end-of-task reflection: distills an episode and extracts durable facts
/// and an optional reusable playbook, all stored in the vector memory.
async fn reflect(
    transcript: &str,
    provider: &Arc<dyn Provider>,
    provider_id: &str,
    model: &str,
    cache_session_id: &Option<String>,
) -> Reflection {
    if transcript.is_empty() {
        return Reflection::default();
    }

    let req = ChatRequest {
        provider: provider_id.to_string(),
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "You are the reflection step of a coding agent. Read the completed task \
                          transcript and return ONLY a JSON object (no prose, no code fences) with \
                          keys: \"episode\" (string: a compact recap of the goal, files changed, \
                          tools used, outcome, and gotchas); \"facts\" (array of {\"category\", \
                          \"fact\"} for durable, reusable knowledge about this project or the \
                          user's preferences, e.g. build/test commands, conventions, key paths; \
                          empty if none); \"playbook\" (object {\"title\", \"steps\"[]} ONLY if a \
                          reusable multi-step procedure was discovered that would speed up a \
                          similar future task, otherwise null)."
                    .to_string(),
            },
            Message {
                role: "user".to_string(),
                content: transcript.to_string(),
            },
        ],
        temperature: Some(0.2),
        max_tokens: Some(800),
        reasoning_effort: None,
        api_key: None,
        cache_session_id: cache_session_id.clone(),
    };

    match provider.chat(&req).await {
        Ok(resp) if !resp.content.trim().is_empty() => {
            if let Some(parsed) = parse_reflection(&resp.content) {
                let episode = if parsed.episode.trim().is_empty() {
                    resp.content.clone()
                } else {
                    parsed.episode
                };
                Reflection {
                    episode,
                    facts: parsed.facts,
                    playbook: parsed.playbook,
                }
            } else {
                Reflection {
                    episode: resp.content,
                    ..Reflection::default()
                }
            }
        }
        _ => {
            let cut = transcript
                .char_indices()
                .nth(1500)
                .map_or(transcript.len(), |(i, _)| i);
            Reflection {
                episode: format!("Task transcript:\n{}", &transcript[..cut]),
                ..Reflection::default()
            }
        }
    }
}

async fn execute_tool_calls(
    calls: &[ToolCall],
    registry: &ToolRegistry,
    options: Option<&AgentOptions>,
    session: &mut Session,
    on_event: &mut impl FnMut(AgentEvent),
) {
    let timeout_secs = options.map_or(300, |o| o.tool_timeout_secs);

    for call in calls {
        on_event(AgentEvent {
            kind: AgentEventKind::ToolCall,
            content: Some(
                serde_json::json!({ "name": call.name, "arguments": call.arguments }).to_string(),
            ),
        });

        let result = match registry.get(&call.name) {
            Some(tool) => {
                if session.is_action_denied(&call.name, &call.arguments) {
                    serde_json::json!({
                        "error": "This action was denied earlier in this run. Do not retry it — ask the user or try a different approach."
                    })
                } else {
                let approved = check_approval(tool.as_ref(), call, options, on_event).await;
                if approved {
                    let ask_gate = options
                        .and_then(|o| o.ask_gate.as_ref())
                        .filter(|_| call.name == "ask_question");
                    let terminal_gate = options
                        .and_then(|o| o.terminal_gate.as_ref())
                        .filter(|_| matches!(call.name.as_str(), "run_command" | "read_terminal"));
                    if let Some(gate) = ask_gate {
                        delegate_ask(call, gate, on_event).await
                    } else if let Some(gate) = terminal_gate {
                        delegate_terminal(call, gate, on_event).await
                    } else {
                        let execution = tool.execute(call.arguments.clone());
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(timeout_secs),
                            execution,
                        )
                        .await
                        {
                            Ok(Ok(val)) => val,
                            Ok(Err(e)) => serde_json::json!({ "error": e.to_string() }),
                            Err(_) => {
                                serde_json::json!({ "error": format!("Tool execution timeout after {}s", timeout_secs) })
                            }
                        }
                    }
                } else {
                    session.record_denied(&call.name, &call.arguments);
                    serde_json::json!({ "error": "Tool execution denied by user" })
                }
                }
            }
            None => serde_json::json!({ "error": format!("Unknown tool: {}", call.name) }),
        };

        // The plan/ledger tool is special: persist its content on the session so
        // we can re-inject it verbatim every turn and shield it from summarization.
        if call.name == "update_plan" {
            if let Some(plan) = call.arguments.get("plan").and_then(|v| v.as_str()) {
                let plan = plan.trim();
                if !plan.is_empty() {
                    session.task_ledger = Some(plan.to_string());
                    on_event(AgentEvent {
                        kind: AgentEventKind::Planning,
                        content: Some(plan.to_string()),
                    });
                }
            }
        }

        let mut result = result;
        if let Some(edit) = result.as_object_mut().and_then(|o| o.remove("_edit")) {
            on_event(AgentEvent {
                kind: AgentEventKind::FileEdit,
                content: Some(edit.to_string()),
            });
        }

        on_event(AgentEvent {
            kind: AgentEventKind::ToolResult,
            content: Some(
                serde_json::json!({ "name": call.name, "result": result }).to_string(),
            ),
        });

        let result_str = serde_json::to_string(&result).unwrap_or_default();
        session.push_message(ToolMessage::tool_result(
            &call.id,
            cap_tool_result_for_history(result_str),
        ));
    }
}

/// Hands a terminal request to the client, returning its result. Covers both running a
/// command (`run_command` -> action "exec") and reading prior terminal output in the
/// session (`read_terminal` -> action "read"); the client's managed terminal pool keeps
/// per-terminal scrollback so the agent can inspect earlier runs across all terminals.
async fn delegate_terminal(
    call: &ToolCall,
    gate: &crate::tools::terminal_gate::TerminalGate,
    on_event: &mut impl FnMut(AgentEvent),
) -> serde_json::Value {
    let req_id = format!("{}_term", call.id);
    let action = if call.name == "read_terminal" {
        "read"
    } else {
        "exec"
    };
    on_event(AgentEvent {
        kind: AgentEventKind::TerminalExec,
        content: Some(
            serde_json::json!({
                "request_id": req_id,
                "action": action,
                "arguments": call.arguments,
            })
            .to_string(),
        ),
    });
    match gate.request(req_id).await {
        Some(raw) => serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or_else(|_| serde_json::json!({ "stdout": raw, "stderr": "", "exit_code": 0 })),
        None => serde_json::json!({ "error": "Terminal execution timed out or was cancelled" }),
    }
}

/// Asks the user a clarifying question through the client and returns their answer.
async fn delegate_ask(
    call: &ToolCall,
    gate: &crate::tools::ask_gate::AskGate,
    on_event: &mut impl FnMut(AgentEvent),
) -> serde_json::Value {
    let req_id = format!("{}_ask", call.id);
    on_event(AgentEvent {
        kind: AgentEventKind::AskRequired,
        content: Some(
            serde_json::json!({
                "request_id": req_id,
                "question": call.arguments.get("question").cloned().unwrap_or_default(),
                "options": call.arguments.get("options").cloned().unwrap_or_default(),
                "multiple": call.arguments.get("multiple").cloned().unwrap_or(serde_json::Value::Bool(false)),
            })
            .to_string(),
        ),
    });
    match gate.request(req_id).await {
        Some(answer) => serde_json::json!({ "answer": answer }),
        None => serde_json::json!({ "error": "The question timed out or was dismissed without an answer" }),
    }
}

async fn check_approval(
    tool: &dyn crate::tools::Tool,
    call: &ToolCall,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) -> bool {
    if !tool.requires_approval() {
        return true;
    }

    // Read-only inspection commands (grep/rg/find/ls/cat/…) run without prompting — they
    // can't mutate the workspace, and network/shell launchers are hard-blocked elsewhere.
    if call.name == "run_command" && crate::tools::command::is_auto_approved(&call.arguments) {
        return true;
    }

    let Some(gate) = options.and_then(|o| o.approval_gate.as_ref()) else {
        return true;
    };

    let req_id = format!("{}_{}", call.id, call.name);
    on_event(AgentEvent {
        kind: AgentEventKind::ApprovalRequired,
        content: Some(
            serde_json::json!({
                "request_id": req_id,
                "tool_name": call.name,
                "arguments": call.arguments,
            })
            .to_string(),
        ),
    });
    gate.request(req_id).await
}

#[cfg(test)]
mod task_context_tests {
    use super::*;
    use crate::models::ToolMessage;

    #[test]
    fn continuation_detects_fix_those_issues() {
        let mut session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        session.push_message(ToolMessage::user(
            "Login view should redirect when session expires",
        ));
        session.push_message(ToolMessage::assistant(
            "I found the issue in notifications/views.py — My Work uses X-Up-Location but \
             notification_row.html opens a modal. Fix: update notification_row.html to match \
             tasks/views.py redirect pattern…",
        ));
        let ctx = resolve_task_context(&session, "fix those issues");
        assert!(ctx.is_follow_up);
        assert_eq!(
            ctx.effective_task,
            "Login view should redirect when session expires"
        );
        assert!(is_continuation_request("fix those issues", &session.messages));
        assert!(is_continuation_request("go ahead and apply", &session.messages));
        assert!(!is_continuation_request(
            "fix those issues",
            &[ToolMessage::user("only message")]
        ));
    }

    #[test]
    fn follow_up_resolves_effective_task() {
        let mut session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        session.push_message(ToolMessage::user("explain the repo"));
        session.push_message(ToolMessage::assistant("SweLoop is a Django app…"));
        let ctx = resolve_task_context(&session, "ok");
        assert!(ctx.is_follow_up);
        assert_eq!(ctx.effective_task, "explain the repo");
        assert!(!ctx.requires_tools);
    }

    #[test]
    fn action_task_requires_tools() {
        assert!(task_requires_tools("implement login"));
        assert!(!task_requires_tools("explain the repo"));
    }

    #[test]
    fn small_tool_results_are_untouched() {
        let s = "small output".to_string();
        assert_eq!(cap_tool_result_for_history(s.clone()), s);
    }

    #[test]
    fn large_tool_results_are_clipped_with_marker() {
        let big = "x".repeat(MAX_TOOL_RESULT_CHARS + 50_000);
        let out = cap_tool_result_for_history(big.clone());
        assert!(out.len() < big.len());
        assert!(out.contains("characters truncated"));
        // Head and tail are preserved.
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.ends_with(&"x".repeat(100)));
    }

    #[test]
    fn clipping_respects_utf8_boundaries() {
        // Multi-byte chars must not be split mid-codepoint (would panic on slice).
        let big = "é".repeat(MAX_TOOL_RESULT_CHARS);
        let out = cap_tool_result_for_history(big);
        assert!(out.contains("characters truncated"));
    }

    #[test]
    fn completion_verdict_when_unverified() {
        let v = CompletionVerdict {
            done: false,
            missing: vec!["x".into()],
            verified: false,
        };
        assert!(!completion_accepted(&v, 0, &Session::new("getaibd", "test", std::path::PathBuf::from("/tmp")), &TaskContext {
            current_input: "x".into(),
            effective_task: "x".into(),
            is_follow_up: false,
            requires_tools: true,
            prose_deliverable: false,
        }));
        let ctx = TaskContext {
            current_input: "commit".into(),
            effective_task: "commit and push".into(),
            is_follow_up: false,
            requires_tools: true,
            prose_deliverable: false,
        };
        assert!(completion_accepted(&v, 1, &Session::new("getaibd", "test", std::path::PathBuf::from("/tmp")), &ctx));
    }

    #[test]
    fn unavailable_reviewer_does_not_trap_non_action_tasks() {
        // Reviewer call failed (verified=false). A Q&A/analysis task does no workspace
        // mutations, so work_total stays 0 — but it must still be accepted rather than
        // looped forever with the "completion check unavailable" message.
        let down = CompletionVerdict {
            done: false,
            missing: vec!["completion check unavailable — verify work was finished".into()],
            verified: false,
        };
        let session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        let qa = TaskContext {
            current_input: "explain the repo".into(),
            effective_task: "explain the repo".into(),
            is_follow_up: false,
            requires_tools: false,
            prose_deliverable: false,
        };
        assert!(completion_accepted(&down, 0, &session, &qa));

        // An action task that did literally nothing is NOT accepted (so the narration
        // nudge can push it to actually act).
        let action = TaskContext {
            current_input: "implement login".into(),
            effective_task: "implement login".into(),
            is_follow_up: false,
            requires_tools: true,
            prose_deliverable: false,
        };
        assert!(!completion_accepted(&down, 0, &session, &action));
        // …but once it has done real work, accept even with the reviewer down.
        assert!(completion_accepted(&down, 2, &session, &action));
    }

    #[test]
    fn git_commit_counts_as_work_progress() {
        let args = serde_json::json!({ "command": "git commit -m test" });
        assert!(counts_as_work_progress("run_command", &args));
        let status = serde_json::json!({ "command": "git status -sb" });
        assert!(!counts_as_work_progress("run_command", &status));
    }

    #[test]
    fn commit_tasks_require_tools() {
        assert!(task_requires_tools("commit and push staged files"));
    }

    #[test]
    fn prose_deliverables_do_not_require_tools() {
        // The reported bug: "write a brief PR description" is prose, not a file edit,
        // so it must not be force-continued by the diff-aware completion reviewer.
        assert!(looks_like_prose_deliverable("write a brief PR description"));
        assert!(looks_like_prose_deliverable("draft a PR desc for this change"));
        assert!(looks_like_prose_deliverable("write the commit message"));
        assert!(looks_like_prose_deliverable("generate release notes"));
        assert!(!task_requires_tools("write a brief PR description"));
        assert!(!task_requires_tools("write the commit message"));

        // Real tool work is still classified as needing tools.
        assert!(!looks_like_prose_deliverable("write a config file"));
        assert!(task_requires_tools("write the auth middleware"));
        assert!(task_requires_tools("write unit tests for the parser"));
    }

    #[test]
    fn resolve_marks_prose_deliverable() {
        let session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        let ctx = resolve_task_context(&session, "write a brief PR description");
        assert!(ctx.prose_deliverable);
        assert!(!ctx.requires_tools);
    }

    #[test]
    fn broadened_prose_matcher() {
        // Generic prose deliverables now match.
        assert!(looks_like_prose_deliverable("write a short summary of the changes"));
        assert!(looks_like_prose_deliverable("draft a reply to this issue"));
        assert!(looks_like_prose_deliverable("compose a release email"));
        // …but not when the object is clearly a file or code (those need tools).
        assert!(!looks_like_prose_deliverable("write a description into the README file"));
        assert!(!looks_like_prose_deliverable("write the error message in utils.py"));
        assert!(!looks_like_prose_deliverable("write a docstring for this function"));
        // No prose noun → not a prose deliverable.
        assert!(!looks_like_prose_deliverable("write the login handler"));
    }

    fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args,
            extra: None,
        }
    }

    fn resp(content: Option<&str>, calls: Vec<ToolCall>) -> ToolChatResponse {
        ToolChatResponse {
            content: content.map(str::to_string),
            tool_calls: calls,
            usage: None,
        }
    }

    #[test]
    fn identical_tool_turns_share_a_signature() {
        let a = resp(
            Some("writing the file"),
            vec![tool_call("write_file", serde_json::json!({"path": "a.txt", "content": "x"}))],
        );
        let b = resp(
            Some("different narration, same action"),
            vec![tool_call("write_file", serde_json::json!({"path": "a.txt", "content": "x"}))],
        );
        // Narration differs but the action is identical → same signature.
        assert_eq!(iteration_signature(&a), iteration_signature(&b));

        let c = resp(
            None,
            vec![tool_call("write_file", serde_json::json!({"path": "a.txt", "content": "DIFFERENT"}))],
        );
        assert_ne!(iteration_signature(&a), iteration_signature(&c));
    }

    #[test]
    fn text_signature_ignores_whitespace_and_case() {
        let a = resp(Some("Here is the answer."), vec![]);
        let b = resp(Some("here   is the   ANSWER."), vec![]);
        assert_eq!(iteration_signature(&a), iteration_signature(&b));
    }

    #[test]
    fn empty_turn_has_no_signature() {
        assert!(iteration_signature(&resp(None, vec![])).is_none());
        assert!(iteration_signature(&resp(Some("   "), vec![])).is_none());
    }

    #[test]
    fn reasoning_router_keeps_thinking_out_of_the_answer() {
        let mut r = ReasoningRouter::default();
        let mut visible = String::new();
        let mut reasoning = String::new();
        // Feed it one char at a time to prove tags spanning tokens are handled.
        for ch in "<thinking>secret plan</thinking>Hello world".chars() {
            let (v, t) = r.push(&ch.to_string());
            visible.push_str(&v);
            reasoning.push_str(&t);
        }
        let (v, t) = r.flush();
        visible.push_str(&v);
        reasoning.push_str(&t);
        assert_eq!(visible, "Hello world");
        assert_eq!(reasoning, "secret plan");
    }

    #[test]
    fn reasoning_router_handles_malformed_unclosed_tag() {
        // Kimi's leak: an opening tag and a malformed close (no '>'). All of it must be
        // treated as reasoning, never surfaced as the answer.
        let mut r = ReasoningRouter::default();
        let (mut v, mut t) = r.push("<thinking>I am reasoning</thinking");
        let (vf, tf) = r.flush();
        v.push_str(&vf);
        t.push_str(&tf);
        assert_eq!(v.trim(), "");
        assert!(t.contains("I am reasoning"));
    }

    #[test]
    fn reasoning_router_passes_literal_angle_brackets() {
        // A real "<" in the answer (e.g. code) must not be eaten as a tag.
        let mut r = ReasoningRouter::default();
        let (v, t) = r.push("if a < b and c > d");
        let (vf, _) = r.flush();
        assert_eq!(format!("{v}{vf}"), "if a < b and c > d");
        assert!(t.is_empty());
    }

    #[test]
    fn reasoning_router_is_incremental_and_monotonic() {
        // Each push returns only the NEW visible text; concatenation equals the answer.
        let mut r = ReasoningRouter::default();
        let chunks = ["Hel", "lo <think>", "noise", "</think> wor", "ld"];
        let mut visible = String::new();
        for c in chunks {
            let (v, _) = r.push(c);
            visible.push_str(&v);
        }
        let (v, _) = r.flush();
        visible.push_str(&v);
        assert_eq!(visible, "Hello  world");
    }

    #[test]
    fn no_progress_stalls_even_when_reviewer_rewords_each_round() {
        // The exact end-condition bug: after the agent fixed the issue it kept
        // re-investigating (no new substantive work), while the LLM reviewer re-worded
        // the SAME complaint every round. The old exact-equality check reset the stall
        // counter forever, so the agent force-continued up to the hard cap. With fuzzy
        // matching + no-progress, three such reviews must stall.
        let max_stall = 3;
        let mut last_missing: Vec<String> = Vec::new();
        let mut stall_rounds = 0u32;
        let rounds = [
            vec!["The whitespace-pre-wrap class is not in the compiled CSS".to_string()],
            vec!["Compiled stylesheet is missing the whitespace-pre-wrap utility".to_string()],
            vec!["whitespace-pre-wrap rule still absent from the compiled CSS file".to_string()],
        ];
        let mut stalled = false;
        for missing in &rounds {
            // progressed=false every round (re-investigation only).
            stalled = is_stalled(false, missing, &mut last_missing, &mut stall_rounds, max_stall);
        }
        assert!(stalled, "re-worded same gap with no progress must stall");
    }

    #[test]
    fn real_progress_on_new_gaps_resets_stall() {
        let max_stall = 3;
        let mut last_missing: Vec<String> = Vec::new();
        let mut stall_rounds = 0u32;
        // First a no-progress round bumps the counter.
        assert!(!is_stalled(false, &["need to add the migration".to_string()], &mut last_missing, &mut stall_rounds, max_stall));
        assert_eq!(stall_rounds, 1);
        // Then the agent makes real progress AND the outstanding work genuinely changes
        // to a different item → counter resets, the run keeps going.
        let progressed = true;
        let next = vec!["wire the new endpoint into the router".to_string()];
        assert!(!is_stalled(progressed, &next, &mut last_missing, &mut stall_rounds, max_stall));
        assert_eq!(stall_rounds, 0);
    }

    #[test]
    fn missing_roughly_same_is_fuzzy() {
        let a = vec!["The whitespace-pre-wrap class is missing from compiled CSS".to_string()];
        let b = vec!["compiled CSS is missing the whitespace-pre-wrap class".to_string()];
        assert!(missing_roughly_same(&a, &b), "reordered/reworded same gap should match");

        let c = vec!["add a database migration for the new column".to_string()];
        assert!(!missing_roughly_same(&a, &c), "unrelated gaps should not match");

        // Two empty lists are "the same" (no concrete gap either time).
        assert!(missing_roughly_same(&[], &[]));
        assert!(!missing_roughly_same(&a, &[]));
    }
}
