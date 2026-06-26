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
    session.push_message(ToolMessage::user(task));
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

/// File paths referenced in context messages the extension injects, in the form
/// `[Currently open file: path]` or `[File: path]`. Used as glob-rule hints and
/// to seed RAG context.
fn open_file_hints(messages: &[ToolMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|msg| msg.content.as_deref())
        .flat_map(|content| {
            let mut files = Vec::new();
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("[Currently open file: ") {
                    if let Some(path) = rest.strip_suffix(']') {
                        files.push(path.to_string());
                    }
                } else if let Some(rest) = line.strip_prefix("[File: ") {
                    if let Some(path) = rest.strip_suffix(']') {
                        files.push(path.to_string());
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
    let hint_paths = open_file_hints(&session.messages);
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

    if enable_thinking && iterations == 0 {
        session.push_message(ToolMessage::system(thinking::PLANNING_PROMPT.to_string()));
    }

    if task_ctx.is_follow_up {
        session.push_message(ToolMessage::system(format!(
            "The user's latest message is a short follow-up (\"{}\"). The active task from \
             this conversation is: \"{}\". Read the full history: if that task was already \
             fully completed in a prior assistant reply, answer the follow-up briefly only — \
             do NOT redo, re-summarize, or re-explore work you already finished. If the task is \
             genuinely unfinished, continue from where you left off and finish only what remains \
             — never restart from the beginning.",
            task_ctx.current_input, task_ctx.effective_task
        )));
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
        let summarize_threshold = (ctx_limit as f32 * 0.85) as usize;
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
            collect_streaming_response(provider, request, on_event).await?
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

        if enable_thinking {
            if let Some(text) = &response.content {
                emit_thinking_events(text, on_event);
            }
        }

        if response.tool_calls.is_empty() {
            let content_txt = response.content.as_deref().unwrap_or("");

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
                if !done {
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
        // not the whole run.
        nudge_count = 0;
        work_total += response
            .tool_calls
            .iter()
            .filter(|tc| counts_as_work_progress(&tc.name, &tc.arguments))
            .count();
        session.push_message(ToolMessage::assistant_tool_calls(
            response.tool_calls.clone(),
        ));
        execute_tool_calls(&response.tool_calls, registry, options, session, on_event).await;

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
/// genuinely stuck: no new progress AND the same outstanding items repeated for
/// `max_stall` consecutive reviews. This is what lets the agent run as long as
/// it's productive while still terminating on a model that can't make headway.
fn is_stalled(
    progressed: bool,
    missing: &[String],
    last_missing: &mut Vec<String>,
    stall_rounds: &mut u32,
    max_stall: u32,
) -> bool {
    if !progressed && missing == last_missing.as_slice() {
        *stall_rounds += 1;
    } else {
        *stall_rounds = 0;
    }
    *last_missing = missing.to_vec();
    *stall_rounds >= max_stall
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

fn task_requires_tools(task: &str) -> bool {
    if looks_like_info_request(task) {
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
    let is_follow_up = is_short_follow_up(current);
    let effective_task = if is_follow_up {
        last_substantive_user_task(&session.messages)
            .filter(|t| t.trim() != current.trim())
            .unwrap_or_else(|| current.to_string())
    } else {
        current.to_string()
    };
    let requires_tools = task_requires_tools(&effective_task);
    TaskContext {
        current_input: current.to_string(),
        effective_task,
        is_follow_up,
        requires_tools,
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
    _ctx: &TaskContext,
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
    work_total > 0
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

    while let Some(delta) = stream.next().await {
        match delta? {
            ToolStreamDelta::Token(token) => {
                content.push_str(&token);
                on_event(AgentEvent {
                    kind: AgentEventKind::Response,
                    content: Some(token),
                });
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

    Ok(ToolChatResponse {
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        tool_calls,
        usage: None,
    })
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
        let reflection = reflect(session, provider).await;
        let indexer = MemoryIndexer::new(mem.store, mem.embedder);
        let _ = indexer.index_episode(&session.id, &reflection.episode).await;

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

        let _ = mem.store.prune_oldest(mem.max_entries);
    }

    Ok(AgentResult {
        final_response,
        iterations,
        mode: None,
    })
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
async fn reflect(session: &Session, provider: &Arc<dyn Provider>) -> Reflection {
    let transcript = build_transcript(session);
    if transcript.is_empty() {
        return Reflection::default();
    }

    let req = ChatRequest {
        provider: session.provider_id.clone(),
        model: session.model.clone(),
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
                content: transcript.clone(),
            },
        ],
        temperature: Some(0.2),
        max_tokens: Some(800),
        reasoning_effort: None,
        api_key: None,
        cache_session_id: session.cache_session_id.clone(),
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
                        .filter(|_| call.name == "run_command");
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
        session.push_message(ToolMessage::tool_result(&call.id, result_str));
    }
}

/// Hands a shell command to the client to run in a managed terminal, returning its result.
async fn delegate_terminal(
    call: &ToolCall,
    gate: &crate::tools::terminal_gate::TerminalGate,
    on_event: &mut impl FnMut(AgentEvent),
) -> serde_json::Value {
    let req_id = format!("{}_term", call.id);
    on_event(AgentEvent {
        kind: AgentEventKind::TerminalExec,
        content: Some(
            serde_json::json!({
                "request_id": req_id,
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
        }));
        let ctx = TaskContext {
            current_input: "commit".into(),
            effective_task: "commit and push".into(),
            is_follow_up: false,
            requires_tools: true,
        };
        assert!(completion_accepted(&v, 1, &Session::new("getaibd", "test", std::path::PathBuf::from("/tmp")), &ctx));
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
}
