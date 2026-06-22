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
use crate::context::{trim_to_context_tool_messages, ContextConfig};
use crate::models::{
    ChatRequest, Message, ToolCall, ToolChatRequest, ToolChatResponse, ToolMessage,
    ToolStreamDelta,
};
use crate::providers::Provider;
use crate::retry::chat_with_tools_retry_cb;
use crate::tools::approval::ApprovalGate;
use crate::tools::ToolRegistry;

use super::session::Session;
use super::thinking;

/// Model used for the strict task-completion review on the GetAIBD provider.
/// The completion check is a small, stateless JSON classification, so it runs on
/// a cheap fast model instead of the (possibly expensive) model the user picked.
/// Other providers keep using the session model (see `verify_task_complete`).
const GETAIBD_COMPLETION_MODEL: &str = "gemini-3.5-flash";

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

async fn inject_context(session: &mut Session, task: &str, memory: Option<&MemoryContext<'_>>) {
    // Build the static prefix (system prompt + long-term memory) and prepend it so it sits
    // BEFORE the conversation history. This keeps the most recent turns closest to the task,
    // which makes them the most salient context for the model.
    let mut prefix: Vec<ToolMessage> = Vec::new();

    if let Some(sys) = &session.system_prompt {
        prefix.push(ToolMessage::system(sys.clone()));
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
        // Extract file paths referenced in prior context messages injected by the extension.
        // The extension injects content in the form "[Currently open file: path]" or "[File: path]".
        let current_files: Vec<String> = session
            .messages
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
            .collect();

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
    //   * MAX_FORCE_CONTINUE — absolute number of forced continuations.
    //   * a no-progress guard — never force-continue unless new tools ran since the last push.
    const MAX_FORCE_CONTINUE: u32 = 8;
    const STEP_EXTENSION: u32 = 40;
    let auto_complete = options.is_some_and(|o| o.auto_complete) && !tool_defs.is_empty();
    let mut force_continue = 0u32;
    // Counts only file-mutating tool calls (see `is_mutating_tool`). The force-continue
    // guard compares these so the agent only keeps going when it actually changed
    // something since the last review — never just because it re-read or re-checked.
    let mut mutating_total = 0usize;
    let mut mutating_at_last_force = 0usize;

    if enable_thinking && iterations == 0 {
        session.push_message(ToolMessage::system(thinking::PLANNING_PROMPT.to_string()));
    }

    loop {
        if iterations >= session.max_iterations {
            // Auto-complete: at the ceiling, let the reviewer decide if we're actually done. If
            // not (and budget + progress remain), raise the ceiling and keep going instead of
            // stopping. Otherwise fall through to the manual Continue brake below.
            if auto_complete
                && force_continue < MAX_FORCE_CONTINUE
                && mutating_total > mutating_at_last_force
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
                let verdict = verify_task_complete(session, task, final_text, provider).await;
                if !verdict.done {
                    force_continue += 1;
                    mutating_at_last_force = mutating_total;
                    nudge_count = 0;
                    session.max_iterations += STEP_EXTENSION;
                    let remaining = format_missing(&verdict.missing);
                    on_event(AgentEvent {
                        kind: AgentEventKind::Reflecting,
                        content: Some(format!(
                            "Step limit reached but the task isn't done — continuing automatically \
                             ({force_continue}/{MAX_FORCE_CONTINUE}).\n{remaining}"
                        )),
                    });
                    session.push_message(ToolMessage::system(format!(
                        "A completion reviewer checked your work against the ORIGINAL task and found \
                         it is NOT yet complete. Outstanding items:\n{remaining}\n\nKeep working and \
                         finish these by calling the appropriate tools. Do not stop until done."
                    )));
                    continue;
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

        const COMPRESS_THRESHOLD: usize = 70;
        if session.messages.len() > COMPRESS_THRESHOLD {
            summarize_old_messages(session, provider).await;
            on_event(AgentEvent {
                kind: AgentEventKind::ContextCompressed,
                content: Some("Conversation summarized to keep context focused".into()),
            });
        }

        if let Some(ctx_cfg) = options.and_then(|o| o.context_config.as_ref()) {
            let (trimmed, was_trimmed) =
                trim_to_context_tool_messages(&session.messages, &session.model, ctx_cfg);
            if was_trimmed {
                session.messages = trimmed;
                on_event(AgentEvent {
                    kind: AgentEventKind::ContextCompressed,
                    content: Some("Context trimmed to fit model window".into()),
                });
            }
        }

        let request = ToolChatRequest {
            model: session.model.clone(),
            messages: session.messages.clone(),
            tools: tool_defs.clone(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: session.reasoning_effort.clone(),
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
            // In action modes the model often narrates ("I'll write the file now") without
            // emitting a tool call, so nothing actually happens. Nudge it to run the tools.
            // This counts CONSECUTIVE narrations (reset whenever it actually calls a tool),
            // so a long, productive run is never cut off just because it paused to narrate.
            const MAX_NUDGES: u32 = 6;
            let content_txt = response.content.as_deref().unwrap_or("");
            let described_only = !looks_like_user_question(content_txt)
                && nudge_count < MAX_NUDGES
                && !tool_defs.is_empty()
                && session.max_iterations > 1
                && iterations < session.max_iterations;
            if described_only {
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

            // Auto-complete: the worker says it's done. A strict reviewer (same model)
            // independently checks the result against the ORIGINAL task. If everything is
            // genuinely complete the agent QUITS immediately with this summary (no extra
            // re-summarization round). Only when real work is still missing do we force it to
            // keep going. Bounded by MAX_FORCE_CONTINUE and a no-progress guard.
            if auto_complete
                && !looks_like_user_question(content_txt)
                && force_continue < MAX_FORCE_CONTINUE
                && mutating_total > mutating_at_last_force
            {
                on_event(AgentEvent {
                    kind: AgentEventKind::Reflecting,
                    content: Some("Reviewing whether the task is fully complete…".into()),
                });
                let final_text = if content_txt.trim().is_empty() {
                    last_text.as_str()
                } else {
                    content_txt
                };
                let verdict = verify_task_complete(session, task, final_text, provider).await;
                if !verdict.done {
                    force_continue += 1;
                    mutating_at_last_force = mutating_total;
                    nudge_count = 0;
                    // The summary we just streamed is being superseded by more work — tell the
                    // client to drop it so the user never sees a duplicate "done" message.
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
                        "A completion reviewer checked your work against the ORIGINAL task and found \
                         it is NOT yet complete. Outstanding items:\n{remaining}\n\nResume now and \
                         finish these by calling the appropriate tools (write_file, patch_file, \
                         run_command, etc.). Do the work end to end, then verify with git_diff. Do \
                         not stop or summarize until everything is genuinely done."
                    )));
                    continue;
                }
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

        // The model is making progress (it called tools), so refill the nudge budget:
        // the limit is for consecutive empty replies, not the whole run.
        nudge_count = 0;
        mutating_total += response
            .tool_calls
            .iter()
            .filter(|tc| is_mutating_tool(&tc.name))
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

/// True for tools that actually change the workspace. Only these count as "new
/// progress" for the auto-complete force-continue guard: a model that merely
/// re-reads files, runs `git_status`, or re-runs a `--check` and then repeats its
/// "I'm done" summary is NOT making progress, so it must not be able to keep the
/// completion-reviewer loop alive (which otherwise re-summarises up to the hard
/// cap and looks like the agent is stuck).
fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "write_file" | "patch_file" | "move_file" | "delete_file")
}

/// Verdict from the completion reviewer ("manager") about whether the original task is done.
struct CompletionVerdict {
    done: bool,
    missing: Vec<String>,
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
            CompletionVerdict { done, missing }
        }
        Err(_) => CompletionVerdict {
            done: true,
            missing: Vec::new(),
        },
    }
}

/// Ask the same model, acting as a strict completion reviewer ("manager"), whether the original
/// task is fully done. Used to auto-continue instead of stopping at a half-finished task.
async fn verify_task_complete(
    session: &Session,
    task: &str,
    final_text: &str,
    provider: &Arc<dyn Provider>,
) -> CompletionVerdict {
    let recent = recent_activity_brief(&session.messages);
    let system = "You are a STRICT completion reviewer for an autonomous coding agent. Given the \
        ORIGINAL TASK and the agent's final message plus recent activity, decide whether the task \
        is FULLY and CONCRETELY complete. Reply with ONLY compact JSON: \
        {\"done\": true|false, \"missing\": [\"specific unfinished item\"]}. Be strict: if any \
        requested part was only described or planned but not actually carried out, or any requested \
        file/change/answer is absent, set done=false and list concrete missing items. If the task \
        was a question or explanation and a complete answer was already given, set done=true with an \
        empty missing list. Never output anything except the JSON object.";
    let user = format!(
        "ORIGINAL TASK:\n{task}\n\nAGENT'S FINAL MESSAGE:\n{final_text}\n\nRECENT ACTIVITY:\n{recent}"
    );
    // On GetAIBD, route this lightweight review to a cheap fast model. Other
    // providers (BYOK) keep using the user's selected model, since a GetAIBD
    // catalog id wouldn't be valid for them.
    let review_model = if session.provider_id == "getaibd" {
        GETAIBD_COMPLETION_MODEL.to_string()
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
    };
    match chat_with_tools_retry_cb(provider, &request, None).await {
        Ok(r) => parse_verdict(r.content.as_deref().unwrap_or("")),
        // Fail open: a reviewer error must not block the agent from finishing.
        Err(_) => CompletionVerdict {
            done: true,
            missing: Vec::new(),
        },
    }
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
                    let args: serde_json::Value =
                        serde_json::from_str(&current_tool_args).unwrap_or_default();
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
                    let args: serde_json::Value =
                        serde_json::from_str(&current_tool_args).unwrap_or_default();
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
async fn summarize_old_messages(session: &mut Session, provider: &Arc<dyn Provider>) {
    let pinned = pinned_head_len(&session.messages);
    let total = session.messages.len();
    // Keep a generous, verbatim recent tail so in-flight work stays intact.
    let keep_tail = (total / 2).max(10);
    // Bail unless there's a meaningful middle to compress between head and tail.
    if total <= pinned + keep_tail + 3 {
        return;
    }
    let summarize_end = total - keep_tail;
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
        return;
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
                    serde_json::json!({ "error": "Tool execution denied by user" })
                }
            }
            None => serde_json::json!({ "error": format!("Unknown tool: {}", call.name) }),
        };

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
