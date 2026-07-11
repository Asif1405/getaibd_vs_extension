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
    ChatRequest, Message, ToolCall, ToolChatRequest, ToolChatResponse,
    ToolDefinition, ToolMessage, ToolStreamDelta,
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
/// `qwen-flash` is safe here even though it can route through thinking mode: this
/// call sends NO tools (`tools: []`, `tool_choice: None`), so the thinking-mode
/// `tool_choice=required` rejection never applies.
/// Other providers keep using the session model (see `verify_task_complete`).
/// Overridable at runtime via `GETAIBD_COMPLETION_MODEL` so ops can repoint it
/// without a rebuild (e.g. if a provider runs out of upstream credits).
const DEFAULT_COMPLETION_MODEL: &str = "qwen-flash";

/// Name of the explicit completion-signal tool. When the worker calls it, the run
/// treats it as a positive "I'm done" claim (verified before ending), instead of
/// inferring completion from the mere absence of a tool call.
pub(crate) const ATTEMPT_COMPLETION: &str = "attempt_completion";

/// The completion-reviewer model: env override if set, else the funded default.
fn completion_model() -> String {
    std::env::var("GETAIBD_COMPLETION_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_COMPLETION_MODEL.to_string())
}

/// Default hard cap on the *estimated input tokens* a single run may bill across all
/// its turns. The turn/stall/repeat backstops already guarantee the loop terminates,
/// but termination alone doesn't bound spend: the dominant cost of an agent run is the
/// transcript resent as prompt on every turn, so a model that keeps calling tools with
/// a large context can burn real money well inside the turn ceiling — the runaway
/// aria-pipeline hit. This is deliberately generous (a genuine long implementation run
/// stays well under it; only a non-converging loop crosses it), so it never reintroduces
/// premature stopping. Tune or disable via `GETAIBD_MAX_RUN_TOKENS` (0 = no cap).
const DEFAULT_MAX_RUN_TOKENS: u64 = 20_000_000;

/// Per-run token/cost budget: `GETAIBD_MAX_RUN_TOKENS` if set (0 disables the cap),
/// else [`DEFAULT_MAX_RUN_TOKENS`]. Returns `None` when the cap is disabled.
fn max_run_tokens() -> Option<u64> {
    match std::env::var("GETAIBD_MAX_RUN_TOKENS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(DEFAULT_MAX_RUN_TOKENS),
        },
        Err(_) => Some(DEFAULT_MAX_RUN_TOKENS),
    }
}

/// True when issuing another turn of `turn` prompt tokens would push the run's
/// cumulative spend past `budget`. The first turn (`cumulative == 0`) is always
/// allowed so a run never dies before doing any work; `None` disables the cap.
fn over_token_budget(cumulative: u64, turn: u64, budget: Option<u64>) -> bool {
    matches!(budget, Some(b) if cumulative > 0 && cumulative.saturating_add(turn) > b)
}

/// Absolute ceiling on the number of provider (LLM) calls a single run may make across
/// EVERY call site — main turns, the completion reviewer, the wrap-up summary, and each
/// context-compaction pass — not just the `iterations` counter (which tracks main turns
/// only). The turn/nudge/stall/repeat backstops stop a run long before this in practice;
/// this is the last-resort guarantee that a pathological interaction between those paths
/// can never fan out into unbounded API spend. Tunable via `GETAIBD_MAX_LLM_CALLS`
/// (0 disables it); the default sits well above any legitimate maxed-out run.
const DEFAULT_MAX_LLM_CALLS: u32 = 800;

/// Per-run cap on total provider calls: `GETAIBD_MAX_LLM_CALLS` if set (0 disables),
/// else [`DEFAULT_MAX_LLM_CALLS`].
fn max_llm_calls() -> Option<u32> {
    match std::env::var("GETAIBD_MAX_LLM_CALLS") {
        Ok(v) => match v.trim().parse::<u32>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(DEFAULT_MAX_LLM_CALLS),
        },
        Err(_) => Some(DEFAULT_MAX_LLM_CALLS),
    }
}

/// A [`Provider`] decorator that counts every outbound model call (`chat`,
/// `chat_with_tools`, and the streaming variants). Wrapping the provider once at the top
/// of the run means EVERY LLM call — wherever it originates in the loop or its helpers —
/// increments a single shared counter, which the loop checks against [`max_llm_calls`].
/// This is what lets us assert a hard, observable bound on total spend regardless of how
/// the reviewer, summarizer, and main turns interleave.
struct CountingProvider {
    inner: Arc<dyn Provider>,
    calls: Arc<std::sync::atomic::AtomicU32>,
}

impl CountingProvider {
    fn bump(&self) {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn display_name(&self) -> &'static str {
        self.inner.display_name()
    }
    fn max_retries(&self) -> u32 {
        self.inner.max_retries()
    }
    fn supports_tool_calling(&self) -> bool {
        self.inner.supports_tool_calling()
    }
    async fn health_check(&self) -> crate::models::ProviderHealth {
        self.inner.health_check().await
    }
    async fn list_models(&self) -> Result<Vec<crate::models::ModelInfo>, AppError> {
        self.inner.list_models().await
    }
    async fn chat(
        &self,
        request: &crate::models::ChatRequest,
    ) -> Result<crate::models::ChatResponse, AppError> {
        self.bump();
        self.inner.chat(request).await
    }
    fn chat_stream(
        &self,
        request: crate::models::ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<String, AppError>> + Send>> {
        self.bump();
        self.inner.chat_stream(request)
    }
    async fn chat_with_tools(
        &self,
        request: &ToolChatRequest,
    ) -> Result<ToolChatResponse, AppError> {
        self.bump();
        self.inner.chat_with_tools(request).await
    }
    fn supports_streaming_tools(&self) -> bool {
        self.inner.supports_streaming_tools()
    }
    fn chat_with_tools_stream(
        &self,
        request: ToolChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ToolStreamDelta, AppError>> + Send>>
    {
        self.bump();
        self.inner.chat_with_tools_stream(request)
    }
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
    /// A structural query (LSP references/definition/symbols) or a post-edit
    /// diagnostics request that the client (editor) should resolve and POST back.
    EditorRequest,
    /// The run stopped because it reached the step-limit brake; the user can continue.
    StepLimitReached,
    /// A provisional assistant message that was already streamed is being superseded
    /// (e.g. the completion reviewer decided more work is needed). The client should
    /// drop the last streamed assistant draft so it is not shown as a duplicate.
    DiscardDraft,
}

/// Why a run ended. Recorded on `AgentResult` and logged in `finish_agent` so the
/// distribution of stop reasons is observable in production instead of guessed at —
/// the premature-stop problem was invisible precisely because nothing reported which
/// termination path fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The model called `attempt_completion` and the reviewer confirmed the work.
    CompletionToolVerified,
    /// The model stopped calling tools and the reviewer confirmed the task is done.
    ReviewerConfirmedDone,
    /// A prose deliverable (PR description, commit message, …) — the text is the answer.
    ProseDeliverable,
    /// The model asked the user a genuine question and yielded.
    UserQuestion,
    /// Genuinely stuck: no new progress and the same outstanding items across reviews.
    StallDetected,
    /// The model emitted a byte-identical turn repeatedly.
    RepeatLoop,
    /// Hit the absolute iteration ceiling.
    StepCeiling,
    /// Ran out of consecutive-narration nudges without further progress.
    NudgeBudgetExhausted,
    /// The model stopped and the reviewer could not verify (unavailable) — accepted
    /// its decision to stop rather than looping.
    ModelStoppedUnverified,
    /// Hit the per-run token/cost budget: the model kept calling APIs without
    /// converging, so the run was stopped to bound spend (aria-pipeline's failure mode).
    CostBudgetExhausted,
    /// Hit the absolute ceiling on total provider (LLM) calls in one run — the last-resort
    /// backstop covering *every* call (turns + reviewer + summaries), not just main turns.
    CallBudgetExhausted,
    /// Ask/Plan (or any non-auto-complete run): the model's reply is the answer.
    NaturalStop,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompletionToolVerified => "completion_tool_verified",
            Self::ReviewerConfirmedDone => "reviewer_confirmed_done",
            Self::ProseDeliverable => "prose_deliverable",
            Self::UserQuestion => "user_question",
            Self::StallDetected => "stall_detected",
            Self::RepeatLoop => "repeat_loop",
            Self::StepCeiling => "step_ceiling",
            Self::NudgeBudgetExhausted => "nudge_budget_exhausted",
            Self::ModelStoppedUnverified => "model_stopped_unverified",
            Self::CostBudgetExhausted => "cost_budget_exhausted",
            Self::CallBudgetExhausted => "call_budget_exhausted",
            Self::NaturalStop => "natural_stop",
        }
    }
}

pub struct AgentResult {
    pub final_response: String,
    pub iterations: u32,
    pub mode: Option<String>,
    pub end_reason: EndReason,
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
    /// When set, structural code queries and post-edit diagnostics are delegated to
    /// the editor's language servers instead of the headless grep/tree-sitter path.
    pub editor_gate: Option<crate::tools::editor_gate::EditorGate>,
    pub tool_timeout_secs: u64,
    pub circuit_breaker: Option<Arc<CircuitBreaker>>,
    pub context_config: Option<ContextConfig>,
    pub enable_thinking: bool,
    /// When true, a strict reviewer (same model) verifies the original task is actually
    /// finished before the run ends, and forces the agent to keep working if it is not.
    /// Only action modes (Agent/Debug) set this; Ask/Plan are meant to yield.
    pub auto_complete: bool,
    /// Sampling temperature for the worker model. Action modes pin this low so the
    /// agent sticks to observed facts instead of confabulating outcomes. `None`
    /// falls back to the provider default (and is used for reasoning models, which
    /// reject a custom temperature).
    pub temperature: Option<f32>,
    /// Hard ceiling on total provider (LLM) calls for this run, overriding the
    /// `GETAIBD_MAX_LLM_CALLS` env / [`DEFAULT_MAX_LLM_CALLS`] default. `None` uses that
    /// default. Lets a caller (or a test) pin an explicit, deterministic cost bound.
    pub max_llm_calls: Option<u32>,
    /// Hard ceiling on tool calls executed during this run. Once reached, the
    /// runtime removes tools and asks for the final answer. `None` is unlimited.
    pub max_tool_calls: Option<u32>,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            approval_gate: None,
            terminal_gate: None,
            ask_gate: None,
            editor_gate: None,
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: None,
            enable_thinking: true,
            auto_complete: false,
            temperature: None,
            max_llm_calls: None,
            max_tool_calls: None,
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

async fn inject_context(
    session: &mut Session,
    task: &str,
    memory: Option<&MemoryContext<'_>>,
) {
    // Each turn re-injects a fresh system prefix. Strip the prior turn's leading
    // system block so instructions/RAG don't stack and tool history stays salient.
    strip_leading_system_prefix(&mut session.messages);

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
                let mut builder = ContextBuilder::new(mem.store, mem.embedder)
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
                build_smart_context(mem.store, mem.embedder, task, &current_files).await
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
/// Build a single turn's chat request from the current session state.
///
/// Performs the one mandatory clone of the (bounded) message list and re-injects
/// the task ledger as the most-recent system note so the model always sees its
/// current plan (the ledger lives outside `session.messages`, so it is never
/// summarized away). Called once per turn on the success path, and again only to
/// rebuild after a rare stream-timeout retry.
fn build_turn_request(
    session: &Session,
    tool_defs: &[ToolDefinition],
    options: Option<&AgentOptions>,
) -> ToolChatRequest {
    let mut req_messages = session.messages.clone();
    if let Some(ledger) = session.task_ledger.as_deref() {
        if !ledger.trim().is_empty() {
            req_messages.push(ToolMessage::system(format!(
                "CURRENT PLAN (your task ledger — keep it updated with the update_plan tool; \
                 mark steps [x] as you finish them and do not stop until every step is [x]):\n{ledger}"
            )));
        }
    }
    ToolChatRequest {
        model: session.model.clone(),
        messages: req_messages,
        tools: tool_defs.to_vec(),
        // Action modes pin a low temperature (set in the orchestrator) so the
        // model reports observed facts rather than inventing plausible ones;
        // None here means "use the provider default" (Ask/Plan, reasoning models).
        temperature: options.and_then(|o| o.temperature),
        max_tokens: None,
        reasoning_effort: session.reasoning_effort.clone(),
        // Never send tool_choice=required — the getaibd backend routes to
        // Alibaba/Qwen thinking mode, which hard-rejects it with a 400
        // ("tool_choice … not supported … in thinking mode"), failing the whole
        // run. Weak models are pushed to act via the system-message nudges below;
        // the compat layer sends "auto".
        tool_choice: None,
        compress: session.compress,
        cache_session_id: session.cache_session_id.clone(),
    }
}

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
    let use_streaming = provider.supports_streaming_tools();
    let enable_thinking = options.is_none_or(|o| o.enable_thinking);

    // Completion model: the run ends ONLY when the model calls attempt_completion.
    // Every other turn that stops calling tools is nudged to keep going. The caps
    // below are pure backstops against a pathological model — they can only prevent
    // an infinite/expensive loop, never fabricate a false "done":
    //   * ABSOLUTE_MAX_ITERATIONS — hard wall on total model turns.
    //   * the repeat-loop guard — stop when the model emits identical turns.
    //   * the token + LLM-call budgets (see below) — cap total spend.
    const ABSOLUTE_MAX_ITERATIONS: u32 = 400;
    const STEP_EXTENSION: u32 = 80;
    // Loop/repeat guard thresholds (see `last_iter_sig` below).
    const REPEAT_STOP_TOOLS: u32 = 2; // stop on the 3rd identical tool turn in a row
    const REPEAT_STOP_TEXT: u32 = 2; // stop on the 3rd identical no-tool answer
    // Force a convergence self-check after at most this many tool calls, so the model
    // pauses to decide "do I have everything?" instead of calling tools endlessly.
    const CHECKPOINT_TOOL_CALLS: u32 = 3;
    let task_ctx = resolve_task_context(session, task);
    let auto_complete = options.is_some_and(|o| o.auto_complete) && !tool_defs.is_empty();
    // Loop/repeat guard. Catches the model emitting a byte-identical action on
    // consecutive turns — re-running the SAME tool call (e.g. re-writing the same
    // file) or re-emitting the SAME answer text. Without this, a repeated write
    // bumps `work_total`, which resets stall detection, so the agent can redo
    // finished work up to MAX_FORCE_CONTINUE times. We (a) never count an identical
    // repeat as progress, (b) nudge once to break the loop, and (c) hard-stop after
    // a couple of identical turns (thresholds REPEAT_STOP_* declared above).
    let mut last_iter_sig: Option<u64> = None;
    let mut repeat_rounds = 0u32;
    // Stagnation guard, independent of the repeat guard above. Counts CONSECUTIVE
    // turns where the model made no tool call and did not signal completion — i.e.
    // it keeps "answering" instead of acting or finishing. Unlike the repeat guard
    // this does not require identical text, so it catches the reworded re-answer
    // loop (the model re-explaining forever). Any real tool call resets it, so a
    // task that is genuinely making progress is never cut off. After
    // NO_TOOL_TURN_STOP consecutive no-tool turns we accept the last answer and
    // stop. This decision lives entirely in the orchestrator, not the model.
    const NO_TOOL_TURN_STOP: u32 = 3;
    let mut no_tool_turns = 0u32;
    // Oscillation guard for MUTATING actions (write/patch/move/delete). Re-issuing
    // the IDENTICAL workspace mutation (same file + same content) is never useful,
    // whether consecutive (already caught by the repeat guard) or interleaved
    // (A→B→A, which the consecutive guard misses). We count identical mutating-turn
    // signatures across the run and stop once one recurs MUTATION_CYCLE_LIMIT times.
    // Deliberately restricted to mutations: read-only / verification commands
    // (`cargo build`, `pytest`, `git status`, re-reads) are legitimately repeated,
    // so counting them would falsely cut productive runs off mid-flight.
    const MUTATION_CYCLE_LIMIT: u32 = 3;
    let mut mutation_sig_counts: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::new();
    // Whether this run has changed anything on disk yet. A no-tool "done" claim on a
    // run that mutated nothing is only trustworthy for an informational request; for
    // an action task it means nothing was actually built, so it must not fast-stop.
    let mut did_mutate = false;
    // Classify the current request once: a question / explain-style ask can end on
    // the model's plain answer, whereas an imperative task must keep going until it
    // actually acts or calls attempt_completion.
    let informational_task = looks_like_informational_request(task);
    // Running estimate of input tokens billed across the run (the prompt/transcript is
    // re-sent every turn, so this is the dominant spend). Estimated locally with the
    // same counter used for compaction so it works even on the streaming path, where
    // providers return no usage object. Bounded by `run_token_budget` below.
    let run_token_budget = max_run_tokens();
    let mut cumulative_prompt_tokens: u64 = 0;
    // Count EVERY provider call this run makes — main turns, the reviewer, the wrap-up
    // summary, and each compaction pass — through one shared counter, and stop hard once
    // it crosses `call_budget`. `llm` is the counted provider used for all model calls
    // below; the ORIGINAL `provider` is still handed to `finish_agent` so the detached,
    // post-run memory reflection isn't charged against (or blocked by) the run's budget.
    let call_budget = match options.and_then(|o| o.max_llm_calls) {
        explicit @ Some(_) => explicit,
        None => max_llm_calls(),
    };
    let llm_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let tool_call_budget = options.and_then(|o| o.max_tool_calls);
    let mut tool_calls_made = 0u32;
    // Tool calls run since the last forced convergence check. After every
    // CHECKPOINT_TOOL_CALLS tools we make the model evaluate whether the task is
    // satisfied or a concrete gap remains, so it can't call tools indefinitely.
    let mut tool_calls_since_checkpoint = 0u32;
    let llm: Arc<dyn Provider> = Arc::new(CountingProvider {
        inner: provider.clone(),
        calls: llm_calls.clone(),
    });
    let llm = &llm;

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
        if tool_call_budget.is_some_and(|budget| tool_calls_made >= budget) {
            on_event(AgentEvent {
                kind: AgentEventKind::Reflecting,
                content: Some(
                    "Evidence budget reached — stopping investigation and writing the report."
                        .into(),
                ),
            });
            session.push_message(ToolMessage::system(
                "The evidence/tool budget is exhausted. Do not request more tools. Produce the \
                 final answer now using only the evidence already collected. Be concrete and \
                 concise; clearly state when no material issue was found."
                    .to_string(),
            ));
            let final_request = ToolChatRequest {
                model: session.model.clone(),
                messages: session.messages.clone(),
                tools: Vec::new(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: session.reasoning_effort.clone(),
                tool_choice: None,
                compress: session.compress,
                cache_session_id: session.cache_session_id.clone(),
            };
            let final_text = chat_with_tools_retry_cb(llm, &final_request, None)
                .await
                .ok()
                .and_then(|response| response.content)
                .filter(|text| !text.trim().is_empty())
                .or_else(|| (!last_text.trim().is_empty()).then(|| last_text.clone()))
                .unwrap_or_else(|| {
                    "The review stopped at its evidence budget before producing a report."
                        .to_string()
                });
            // This final call is non-streaming, so its text never reached the UI as
            // tokens. Emit it as a Response so the report is actually shown (the
            // Complete event's content is dropped by the client). Skip if it merely
            // echoes text already streamed this run.
            if final_text.trim() != last_text.trim() {
                on_event(AgentEvent {
                    kind: AgentEventKind::Response,
                    content: Some(final_text.clone()),
                });
            }
            return finish_agent(
                session,
                Some(final_text),
                memory,
                provider,
                iterations,
                EndReason::NaturalStop,
                on_event,
            )
            .await;
        }

        // Absolute backstop on TOTAL provider calls (turns + reviewer + summaries). The
        // turn/nudge/stall/repeat guards below stop a healthy run far sooner; this only
        // ever fires if those paths pathologically interact, and it guarantees the run can
        // never fan out into unbounded API spend. Checked first so nothing issues another
        // call past the ceiling.
        if let Some(budget) = call_budget {
            let made = llm_calls.load(std::sync::atomic::Ordering::Relaxed);
            if made >= budget {
                tracing::warn!(
                    llm_calls = made,
                    budget,
                    iterations,
                    "agent run hit the LLM-call ceiling — stopping to bound spend"
                );
                on_event(AgentEvent {
                    kind: AgentEventKind::StepLimitReached,
                    content: Some(format!(
                        "Reached the LLM-call ceiling ({made} model calls) — stopping to bound cost."
                    )),
                });
                let final_text = if last_text.trim().is_empty() {
                    "Stopped: this run reached its model-call ceiling before finishing. Re-run \
                     to continue, or raise GETAIBD_MAX_LLM_CALLS."
                        .to_string()
                } else {
                    last_text.clone()
                };
                return finish_agent(
                    session,
                    Some(final_text),
                    memory,
                    provider,
                    iterations,
                    EndReason::CallBudgetExhausted,
                    on_event,
                )
                .await;
            }
        }
        if iterations >= session.max_iterations {
            // Auto-complete runs end ONLY on attempt_completion. Hitting the soft
            // iteration ceiling is not "done" — raise the ceiling and keep going,
            // up to the hard wall (ABSOLUTE_MAX_ITERATIONS). No reviewer, no gate;
            // the model itself decides it's finished by calling attempt_completion,
            // and the repeat-loop guard + token/call budgets bound the spend.
            if auto_complete && iterations < ABSOLUTE_MAX_ITERATIONS {
                session.max_iterations =
                    (session.max_iterations + STEP_EXTENSION).min(ABSOLUTE_MAX_ITERATIONS);
                on_event(AgentEvent {
                    kind: AgentEventKind::Reflecting,
                    content: Some(
                        "Still working — the task isn't marked complete yet, so I'm continuing…"
                            .into(),
                    ),
                });
                session.push_message(ToolMessage::system(
                    "You are still mid-task. Keep going with the appropriate tools until the whole \
                     task is genuinely done, then call attempt_completion with a short summary. Do \
                     not stop or summarize instead of finishing.".to_string(),
                ));
                continue;
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
            let summary = match chat_with_tools_retry_cb(llm, &wrap, None).await {
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
                EndReason::StepCeiling,
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
        let mut announced = false;
        loop {
            let used = crate::context::count_tool_message_tokens(&session.messages) + tool_tokens;
            if used <= summarize_threshold {
                break;
            }
            // Announce once, BEFORE the first (possibly slow, provider-backed)
            // summarization pass, so the UI can show a live "Summarizing chat
            // context…" status *while* it happens instead of only after it's done.
            if !announced {
                on_event(AgentEvent {
                    kind: AgentEventKind::ContextCompressed,
                    content: Some("Summarizing chat context…".into()),
                });
                announced = true;
            }
            if !summarize_old_messages(session, llm).await {
                break;
            }
        }

        // Re-inject the task ledger as the most-recent system note every turn so the
        // model always sees its current plan/place. It lives outside session.messages,
        // so it is never summarized away and always reflects the latest update_plan.
        // Build this turn's request from the current session state. This is the
        // one mandatory clone of the (bounded) message list plus ledger injection.
        // A free fn (not an inline pre-clone) lets the rare stream-timeout retry
        // rebuild it cheaply on demand, so the success path clones once per turn
        // instead of twice.
        let request = build_turn_request(session, &tool_defs, options);

        // Hard spend cap. Estimate this turn's prompt size and stop BEFORE issuing another
        // paid request if the run would cross its token budget. Turn/stall/repeat backstops
        // already guarantee the loop ends; this additionally bounds *cost* so a model that
        // keeps calling tools without converging (aria-pipeline's runaway) can't quietly burn
        // the balance. The first turn is always allowed; the cap only trips on compounding
        // spend, and the budget is generous enough never to cut off a genuine long run.
        if run_token_budget.is_some() {
            let turn_tokens =
                (crate::context::count_tool_message_tokens(&request.messages) + tool_tokens) as u64;
            if over_token_budget(cumulative_prompt_tokens, turn_tokens, run_token_budget) {
                tracing::warn!(
                    cumulative_prompt_tokens,
                    budget = run_token_budget.unwrap_or(0),
                    iterations,
                    "agent run hit token budget — stopping to bound spend"
                );
                on_event(AgentEvent {
                    kind: AgentEventKind::StepLimitReached,
                    content: Some(format!(
                        "Token budget reached (~{cumulative_prompt_tokens} prompt tokens over \
                         {iterations} turns) — stopping to bound cost."
                    )),
                });
                let final_text = if last_text.trim().is_empty() {
                    "Stopped: this run reached its token/cost budget before the task was \
                     finished. Re-run to continue from the current state, or raise \
                     GETAIBD_MAX_RUN_TOKENS."
                        .to_string()
                } else {
                    last_text.clone()
                };
                return finish_agent(
                    session,
                    Some(final_text),
                    memory,
                    provider,
                    iterations,
                    EndReason::CostBudgetExhausted,
                    on_event,
                )
                .await;
            }
            cumulative_prompt_tokens += turn_tokens;
        }

        let mut response = if use_streaming {
            // Transient stream failures are worth retrying with backoff instead of
            // aborting the whole task: a stalled stream (idle watchdog tripped), a
            // gateway/proxy blip (502/503/504), or an upstream overload (429). Each
            // attempt rebuilds the request from the current session and discards any
            // partial draft, so the retry is idempotent. Genuine client errors (real
            // 4xx, auth, malformed request) are non-transient and surface at once.
            // Bounded by the provider's own retry budget so a hard outage still ends
            // the run instead of looping forever. Move the already-built request into
            // the first attempt (no clone); only a retry rebuilds it.
            let max = llm.max_retries();
            let mut attempt = 0u32;
            let mut req = request;
            loop {
                match collect_streaming_response(llm, req, on_event).await {
                    Ok(r) => break r,
                    Err(e @ (AppError::ProviderTimeout(_) | AppError::ProviderUnavailable(_)))
                        if attempt < max =>
                    {
                        on_event(AgentEvent {
                            kind: AgentEventKind::DiscardDraft,
                            content: None,
                        });
                        on_event(AgentEvent {
                            kind: AgentEventKind::Reflecting,
                            content: Some(format!(
                                "The model stream failed transiently ({e}) — retrying this step…"
                            )),
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(
                            100 * 2u64.saturating_pow(attempt),
                        ))
                        .await;
                        attempt += 1;
                        req = build_turn_request(session, &tool_defs, options);
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            let cb = options.and_then(|o| o.circuit_breaker.as_deref());
            chat_with_tools_retry_cb(llm, &request, cb).await?
        };
        if let Some(budget) = tool_call_budget {
            let remaining = budget.saturating_sub(tool_calls_made) as usize;
            if response.tool_calls.len() > remaining {
                response.tool_calls.truncate(remaining);
            }
        }
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
            return finish_agent(
                session,
                Some(msg),
                memory,
                provider,
                iterations,
                EndReason::RepeatLoop,
                on_event,
            )
            .await;
        }

        // Oscillation guard: a mutating turn re-issued with identical args (same
        // file + same content), whether consecutive or interleaved (A→B→A). Counted
        // across the run; the third occurrence means the model is cycling on the
        // same edit rather than progressing, so stop with its last substantive text.
        if let Some(sig) = iter_sig {
            if response
                .tool_calls
                .iter()
                .any(|c| is_mutating_tool(&c.name))
            {
                let n = mutation_sig_counts.entry(sig).or_insert(0);
                *n += 1;
                if *n >= MUTATION_CYCLE_LIMIT {
                    on_event(AgentEvent {
                        kind: AgentEventKind::DiscardDraft,
                        content: None,
                    });
                    let msg = if last_text.trim().is_empty() {
                        "I stopped because I kept re-applying the same change without \
                         making new progress."
                            .to_string()
                    } else {
                        last_text.clone()
                    };
                    return finish_agent(
                        session,
                        Some(msg),
                        memory,
                        provider,
                        iterations,
                        EndReason::RepeatLoop,
                        on_event,
                    )
                    .await;
                }
            }
        }

        if enable_thinking {
            if let Some(text) = &response.content {
                emit_thinking_events(text, on_event);
            }
        }

        // Explicit completion signal: the model called attempt_completion this turn.
        // That is a positive "I'm done" claim (verified below), NOT inferred from the
        // absence of a tool call. The summary it passed becomes the final message.
        let explicit_completion = response
            .tool_calls
            .iter()
            .any(|c| c.name == ATTEMPT_COMPLETION);
        let completion_summary = response
            .tool_calls
            .iter()
            .find(|c| c.name == ATTEMPT_COMPLETION)
            .and_then(|c| c.arguments.get("summary"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Execute any real tool calls FIRST so the transcript stays well-formed even
        // when the model batched attempt_completion with other tools (an assistant
        // tool-call message needs its matching tool results before the next request).
        if !response.tool_calls.is_empty() {
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
            session.push_message(ToolMessage::assistant_tool_calls(
                response.tool_calls.clone(),
            ));
            execute_tool_calls(&response.tool_calls, registry, options, session, on_event).await;
            tool_calls_made =
                tool_calls_made.saturating_add(response.tool_calls.len() as u32);
            tool_calls_since_checkpoint =
                tool_calls_since_checkpoint.saturating_add(response.tool_calls.len() as u32);
            // The model acted this turn — it isn't stalled on prose. Reset the
            // no-tool stagnation counter.
            no_tool_turns = 0;
            if response.tool_calls.iter().any(|c| is_mutating_tool(&c.name)) {
                did_mutate = true;
            }

            if is_repeat {
                // One identical repeat (below the hard stop): tell the model plainly so it
                // can break the loop on the next turn instead of redoing the same step.
                session.push_message(ToolMessage::system(
                    "That step was identical to your previous one and has already taken effect. \
                     Do NOT repeat it. Either perform the NEXT remaining step, or — if everything \
                     the task asked for is done — call attempt_completion with a brief final summary."
                        .to_string(),
                ));
            }

            // A normal tool turn keeps going. An explicit attempt_completion falls
            // through to the completion evaluation below (its batched tools already ran).
            if !explicit_completion {
                // Force a convergence check after every few tool calls (always, even
                // for non-reasoning models that skip the richer reflection): the model
                // must decide whether it now has everything or a concrete gap remains,
                // instead of chaining tool calls indefinitely. Between checkpoints,
                // reasoning models still get the lighter per-turn reflection.
                if tool_calls_since_checkpoint >= CHECKPOINT_TOOL_CALLS {
                    session.push_message(ToolMessage::system(
                        thinking::CHECKPOINT_PROMPT.to_string(),
                    ));
                    tool_calls_since_checkpoint = 0;
                } else if enable_thinking {
                    session
                        .push_message(ToolMessage::system(thinking::REFLECTION_PROMPT.to_string()));
                }
                continue;
            }
        }

        // Completion evaluation: reached when the model produced NO tool calls (it
        // stopped) OR it explicitly signaled completion. This is the SINGLE place the
        // run decides to end vs. keep going; every exit stamps an EndReason.
        let content_txt: &str = if explicit_completion {
            completion_summary
                .as_deref()
                .unwrap_or_else(|| response.content.as_deref().unwrap_or(""))
        } else {
            response.content.as_deref().unwrap_or("")
        };
        // The message handed back on a finish: the completion summary for an explicit
        // claim, else the model's own reply.
        let final_content: Option<String> = if explicit_completion {
            Some(content_txt.to_string())
        } else {
            response.content.clone()
        };

        // Non-auto-complete runs (Ask/Plan) are meant to answer and yield — the reply
        // is the deliverable, no completion machinery.
        if !auto_complete {
            return finish_agent(
                session,
                final_content,
                memory,
                provider,
                iterations,
                EndReason::NaturalStop,
                on_event,
            )
            .await;
        }

        // ── The one completion rule ─────────────────────────────────────────
        // A run ends cleanly ONLY when the model calls `attempt_completion`. Any
        // other turn that stopped calling tools is treated as "not done yet": we
        // nudge the model to either keep working or signal completion, then loop.
        // There is deliberately no reviewer and no prose/question/plan heuristic —
        // that machinery is what kept cutting long tasks off early. The run is
        // bounded only by the hard safety caps (iteration wall, repeat-loop guard,
        // and token/LLM-call budgets), which can never fabricate a false "done".
        if explicit_completion {
            if informational_task && !did_mutate {
                // Small models sometimes use `attempt_completion.summary` as a
                // status receipt ("Explained X; changes: none") without ever
                // providing the explanation itself. For an informational task,
                // completion means producing the actual user-facing answer.
                session.push_message(ToolMessage::system(
                    "The user requested an explanation/analysis, but your completion summary is \
                     only a status receipt. Now provide the ACTUAL answer in full using the \
                     evidence already gathered. Do not call tools. Do not output Outcome/Changes/\
                     Notes metadata and do not say merely that you explained it."
                        .to_string(),
                ));
                let answer_request = ToolChatRequest {
                    model: session.model.clone(),
                    messages: session.messages.clone(),
                    tools: Vec::new(),
                    temperature: None,
                    max_tokens: None,
                    reasoning_effort: session.reasoning_effort.clone(),
                    tool_choice: None,
                    compress: session.compress,
                    cache_session_id: session.cache_session_id.clone(),
                };
                let actual_answer = chat_with_tools_retry_cb(llm, &answer_request, None)
                    .await
                    .ok()
                    .and_then(|answer| answer.content)
                    .filter(|answer| !answer.trim().is_empty())
                    .or(final_content);
                return finish_agent(
                    session,
                    actual_answer,
                    memory,
                    provider,
                    iterations,
                    EndReason::CompletionToolVerified,
                    on_event,
                )
                .await;
            }
            tracing::debug!(iterations, "attempt_completion signaled — finishing run");
            return finish_agent(
                session,
                final_content,
                memory,
                provider,
                iterations,
                EndReason::CompletionToolVerified,
                on_event,
            )
            .await;
        }

        // Q&A / explain fast-path: when the CURRENT request was informational, the
        // model produced a substantive answer, nothing was changed on disk this run,
        // and the answer doesn't trail off announcing a next action, the answer IS
        // the deliverable. Accept it and stop — nudging here only makes the model
        // re-explain the same thing (the "same reply repeated" loop the user hit).
        // Action tasks never match `informational_task`, so they can't be stopped
        // early by this path.
        if informational_task
            && !did_mutate
            && !ends_with_action_cue(content_txt)
            && !announces_next_step(content_txt)
            && final_content
                .as_deref()
                .map(str::trim)
                .is_some_and(|t| t.len() >= 40)
        {
            tracing::debug!(iterations, "informational request answered — finishing run");
            return finish_agent(
                session,
                final_content,
                memory,
                provider,
                iterations,
                EndReason::NaturalStop,
                on_event,
            )
            .await;
        }

        // No tool calls and no completion signal: the model produced an answer
        // instead of acting/finishing. Count it toward stagnation. If it keeps
        // doing this across NO_TOOL_TURN_STOP consecutive turns (it was nudged in
        // between and still won't act or call attempt_completion), treat the last
        // answer as the deliverable and stop — this ends the reworded re-answer
        // loop without cutting off a task that is actually making tool progress.
        no_tool_turns += 1;
        if no_tool_turns >= NO_TOOL_TURN_STOP {
            tracing::debug!(
                iterations,
                no_tool_turns,
                "no-tool stagnation limit reached — accepting the last answer and finishing"
            );
            return finish_agent(
                session,
                final_content,
                memory,
                provider,
                iterations,
                EndReason::NaturalStop,
                on_event,
            )
            .await;
        }

        // Below the stop threshold: discard the superseded streamed draft, then push
        // the model to act (or to call attempt_completion if it is genuinely done).
        // On the LAST allowed retry, escalate to a stronger, more explicit nudge so
        // a genuinely stuck model gets one clear chance to change approach before we
        // stop.
        if !last_text.trim().is_empty() {
            on_event(AgentEvent {
                kind: AgentEventKind::DiscardDraft,
                content: None,
            });
        }
        let nudge = if no_tool_turns + 1 >= NO_TOOL_TURN_STOP {
            "You have now answered twice without acting. If the task requires \
             changes, STOP re-explaining and take the next concrete action with a \
             tool now. If everything the task asked for is genuinely done, call \
             attempt_completion with a brief final summary. Do not repeat your \
             previous answer."
                .to_string()
        } else {
            continue_or_complete_nudge()
        };
        session.push_message(ToolMessage::system(nudge));
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

/// Strip `<think>…</think>` / `<thinking>…</thinking>` reasoning blocks a thinking-mode
/// reviewer (e.g. `qwen-flash`) may emit before its JSON verdict. Such blocks can contain
/// stray braces that would corrupt the outermost `{…}` span extraction below.
fn strip_reasoning_tags(s: &str) -> String {
    let mut out = s.to_string();
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        while let Some(start) = out.find(open) {
            match out[start + open.len()..].find(close) {
                Some(rel) => {
                    let end = start + open.len() + rel + close.len();
                    out.replace_range(start..end, "");
                }
                // Unclosed tag: drop everything from the open tag onward.
                None => {
                    out.truncate(start);
                    break;
                }
            }
        }
    }
    out
}

/// Parse the reviewer's JSON verdict. A missing/malformed reply fails SAFE — marked
/// UNVERIFIED so the caller keeps going (bounded by the narration-nudge budget + stall
/// detection) rather than accepting an unparseable reply as "done" and quitting a
/// still-unfinished task. This matters more now that the reviewer can be a thinking-mode
/// model whose output is chattier around the JSON.
fn parse_verdict(s: &str) -> CompletionVerdict {
    let cleaned = strip_reasoning_tags(s);
    let unverified = || CompletionVerdict {
        done: false,
        missing: vec!["completion check returned no parseable verdict".to_string()],
        verified: false,
    };
    let (open, close) = match (cleaned.find('{'), cleaned.rfind('}')) {
        (Some(a), Some(b)) if a < b => (a, b),
        _ => return unverified(),
    };
    match serde_json::from_str::<serde_json::Value>(&cleaned[open..=close]) {
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
        Err(_) => unverified(),
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
        Err(_) => {
            // Reliability fallback: the cheap reviewer model can be down / rate-limited,
            // and an unverified verdict is what quit half-finished tasks. The working
            // model is definitionally up during the run (it is doing the work), so retry
            // the review once against it before giving up. Skipped when the reviewer
            // already IS the working model (BYOK), since that just failed.
            if request.model != session.model {
                let mut fallback = request;
                fallback.model = session.model.clone();
                if let Ok(r) = chat_with_tools_retry_cb(provider, &fallback, None).await {
                    return parse_verdict(r.content.as_deref().unwrap_or(""));
                }
            }
            // Both the cheap reviewer and the working-model fallback failed. Mark the
            // verdict unverified so the caller decides safely (keep going, bounded by
            // stall detection) rather than quitting a task with nothing verified.
            CompletionVerdict {
                done: false,
                missing: vec!["completion check unavailable — verify work was finished".to_string()],
                verified: false,
            }
        }
    }
}

/// Remove leading `system` messages injected by a prior turn's `inject_context`.
/// Mid-conversation system notes (nudges, reflections) are left intact.
fn strip_leading_system_prefix(messages: &mut Vec<ToolMessage>) {
    while messages.first().is_some_and(|m| m.role == "system") {
        messages.remove(0);
    }
}

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
    // A checklist or numbered step list is a multi-item action plan — treat it as
    // tool-requiring so a model that narrates the remaining steps is nudged to act
    // instead of being accepted as "done".
    if task_has_action_list(task) {
        return true;
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
        "remove",
        "rename",
        "move",
        "patch",
        "setup",
        "set up",
        "install",
        "configure",
        "integrate",
        "enable",
        "disable",
        "wire",
        "hook up",
        "scaffold",
        "generate",
        "make ",
        "finish",
        "complete",
        "continue",
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

/// True when the task text contains a checkbox list (`- [ ]` / `- [x]`) or a
/// multi-step numbered list (at least two `1.`/`2.` style items) — a strong signal
/// the request is an actionable, multi-step plan rather than a question.
fn task_has_action_list(task: &str) -> bool {
    if has_open_checkbox(task) || task.contains("[x]") || task.contains("[X]") {
        return true;
    }
    let numbered = task
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            t.starts_with("1.")
                || t.starts_with("2.")
                || t.starts_with("3.")
                || t.starts_with("- ")
                || t.starts_with("* ")
        })
        .count();
    numbered >= 2
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

/// Inputs to the (pure) stop decision, gathered on a turn where the model either
/// stopped calling tools or explicitly signaled completion. Kept as plain data so
/// the decision is unit-testable without a provider or session.
struct StopSignals {
    /// The model called `attempt_completion` this turn (explicit done claim).
    explicit_completion: bool,
    /// The model's message/summary was empty.
    content_empty: bool,
    /// The reply is a genuine question for the user.
    is_user_question: bool,
    /// The task's deliverable is prose (PR description, commit message, …).
    prose_deliverable: bool,
    /// Count of substantive work actions performed so far.
    work_total: usize,
    /// The plan/ledger or the latest message still has unchecked "[ ]" steps.
    plan_open: bool,
    /// The reply ENDS by announcing an imminent action it did not take this turn
    /// ("Let me read …", "Now I'll edit …", or a trailing ':') — mid-plan narration,
    /// never a finished task.
    dangling_action_cue: bool,
    /// Consecutive-narration nudges used so far.
    nudge_count: u32,
    /// Forced continuations used so far.
    force_continue: u32,
}

/// First-pass stop decision for a no-tool / completion-signal turn. Pure and
/// side-effect free: it never runs the (async) reviewer — it returns `Review` when
/// one is needed so the caller can invoke it. Only called in auto-complete
/// (action) modes; Ask/Plan finish directly with `NaturalStop`.
#[derive(Debug, PartialEq, Eq)]
enum StopAction {
    /// Accept a prose deliverable as the answer.
    FinishProse,
    /// The model asked the user something — yield.
    FinishUserQuestion,
    /// Plan still has open steps — nudge to do the next one.
    NudgePlanOpen,
    /// Ask the reviewer whether the task is actually complete.
    Review,
    /// The model only narrated — nudge it to act.
    NudgeNarration,
    /// No condition left to try — finish with the model's reply.
    FinishModelReply,
}

fn classify_stop(sig: &StopSignals, max_nudges: u32, max_force_continue: u32) -> StopAction {
    // An explicit completion claim always goes through verification, regardless of
    // how its accompanying prose reads.
    if sig.explicit_completion {
        return StopAction::Review;
    }
    // Prose deliverable with no workspace changes: the text IS the deliverable.
    if sig.prose_deliverable && sig.work_total == 0 && !sig.content_empty && !sig.is_user_question {
        return StopAction::FinishProse;
    }
    // A genuine question yields to the user (never force-continued).
    if sig.is_user_question {
        return StopAction::FinishUserQuestion;
    }
    // Model-authored "not done" signal: open "[ ]" steps in the plan or latest reply.
    if sig.plan_open && sig.nudge_count < max_nudges {
        return StopAction::NudgePlanOpen;
    }
    // A no-tool turn that ends by announcing an imminent action ("Let me read …",
    // "Now I'll edit …", a trailing ':') is mid-plan narration, NEVER a finished task.
    // Nudge it to actually act rather than routing to the reviewer, whose cheap model
    // can wrongly rule such a turn "done" and quit the run on its very first step.
    // Bounded by the nudge budget; a real tool call resets it, so progress is safe.
    if sig.dangling_action_cue && sig.nudge_count < max_nudges {
        return StopAction::NudgeNarration;
    }
    // Otherwise let the diff-aware reviewer judge (bounded by the force budget).
    if sig.force_continue < max_force_continue {
        return StopAction::Review;
    }
    // Force budget spent: one more narration nudge, else finish.
    if sig.nudge_count < max_nudges {
        return StopAction::NudgeNarration;
    }
    StopAction::FinishModelReply
}

/// What to do after the reviewer has spoken. Pure: mutating stall state stays in the
/// caller (only touched on the `VerifiedNotDone` path, as before).
#[derive(Debug, PartialEq, Eq)]
enum ReviewAction {
    /// The task is done — finish with this reason.
    Finish(EndReason),
    /// The reviewer verified the task is NOT done — force-continue (or stall-stop).
    VerifiedNotDone,
    /// The reviewer call itself failed — fall through to a bounded narration nudge.
    Unverified,
}

/// System nudge pushed when the model stopped calling tools without signaling
/// completion. It is the only thing standing between "the model paused" and "the run
/// keeps going", so it must be unambiguous: do the work, or call attempt_completion.
/// True when the CURRENT user request is informational — a question or an
/// "explain / describe / analyze / how does … work" ask — rather than an
/// imperative to change the workspace. Used to let a Q&A turn end on the model's
/// plain answer (its natural stop signal) instead of nudging it into re-explaining
/// the same thing. Deliberately conservative: any leading action verb disqualifies
/// it, so action tasks never take the Q&A fast-path and can't be stopped early.
fn looks_like_informational_request(task: &str) -> bool {
    let t = task.trim().to_lowercase();
    if t.is_empty() {
        return false;
    }
    // A leading imperative verb means "do something", not "tell me something".
    const ACTION_VERBS: &[&str] = &[
        "add ",
        "implement",
        "create ",
        "build ",
        "fix ",
        "refactor",
        "rename",
        "delete",
        "remove ",
        "update ",
        "change ",
        "write ",
        "edit ",
        "move ",
        "install",
        "set up",
        "setup",
        "configure",
        "make ",
        "convert",
        "migrate",
        "generate",
        "replace",
        "wire ",
        "hook up",
        "integrate",
        "run ",
        "commit",
        "push ",
        "deploy",
        "test ",
    ];
    if ACTION_VERBS.iter().any(|v| t.starts_with(v)) {
        return false;
    }
    if t.ends_with('?') {
        return true;
    }
    const INFO_CUES: &[&str] = &[
        "explain",
        "what is",
        "what are",
        "what does",
        "what's",
        "whats",
        "how does",
        "how do",
        "how is",
        "why does",
        "why is",
        "why are",
        "describe",
        "summarize",
        "summarise",
        "analyze",
        "analyse",
        "walk me through",
        "walk through",
        "tell me",
        "where is",
        "where does",
        "can you explain",
        "help me understand",
        "give me an overview",
        "overview of",
    ];
    INFO_CUES.iter().any(|c| t.contains(c))
}

/// True when a no-tool turn signals it intends to keep working — it announces a
/// next step ("let me read…", "I need to check…", "I'll look at…", "let me look at
/// the code, I need to read lines …") ANYWHERE in the message, not just the final
/// sentence like [`ends_with_action_cue`]. Such a turn is mid-plan narration, never
/// a finished answer, so the Q&A fast-path must NOT accept it and stop the run.
///
/// A false positive here is cheap (the run just nudges once more instead of ending
/// on this turn, still bounded by the stagnation/repeat guards); a false negative is
/// the expensive "quit mid-run" bug, so this errs toward detecting continuation.
fn announces_next_step(text: &str) -> bool {
    // "let me know (if …)" addresses the user; it's a sign-off, not a self-action.
    let lower = text.to_lowercase().replace("let me know", "");
    const CUES: &[&str] = &[
        "let me ",
        "let's ",
        "lets ",
        "i'll ",
        "i will ",
        "i am going to",
        "i'm going to",
        "im going to",
        "i'm gonna",
        "i need to ",
        "i have to ",
        "i still need",
        "i need more",
        "now i ",
        "now let me",
        "next i",
        "next, i",
        "first let me",
        "first, let me",
        "then i'll",
        "then let me",
        "need to read",
        "need to look",
        "need to check",
        "need to see",
        "going to read",
        "let me read",
        "let me look",
        "let me check",
        "let me see",
        "read lines",
    ];
    CUES.iter().any(|c| lower.contains(c))
}

fn continue_or_complete_nudge() -> String {
    "You stopped without calling any tool and without calling attempt_completion, so the task is \
     NOT finished. Do not describe what you will do — do it NOW with the appropriate tools \
     (write_file, patch_file, move_file, delete_file, run_command, …). When every part of the \
     task is genuinely done, call attempt_completion with a short summary — that is the ONLY way \
     to end the run. If you are truly blocked or need a decision, call ask_question instead of \
     stopping."
        .to_string()
}

fn decide_after_review(accepted: bool, verified: bool, explicit: bool) -> ReviewAction {
    if accepted {
        ReviewAction::Finish(if explicit {
            EndReason::CompletionToolVerified
        } else {
            EndReason::ReviewerConfirmedDone
        })
    } else if verified {
        ReviewAction::VerifiedNotDone
    } else {
        ReviewAction::Unverified
    }
}

fn completion_accepted(
    verdict: &CompletionVerdict,
    work_total: usize,
    session: &Session,
    ctx: &TaskContext,
    latest_text: &str,
) -> bool {
    if verdict.verified {
        if verdict.done {
            return true;
        }
        // Reviewer said incomplete but listed nothing actionable. Accept only if we
        // actually did work AND the model's own checklist has no open steps — so we
        // don't loop on repeat summaries, yet never quit a visibly-unfinished plan.
        if verdict.missing.is_empty()
            && work_total > 0
            && !plan_has_open_steps(session, latest_text)
        {
            return true;
        }
        return false;
    }
    // Reviewer UNAVAILABLE (the check call itself failed): we have no independent
    // semantic signal. Fall back to STRUCTURED state, never a guess about prose.
    // If the model's own plan still has unchecked "[ ]" steps (in the ledger OR in the
    // message it just wrote), the task is not done — don't let a down reviewer end a
    // multi-step run early (the narration nudge keeps it going, bounded by the stall
    // detector).
    if plan_has_open_steps(session, latest_text) {
        return false;
    }
    // A non-action deliverable is satisfied by the text itself — a question/explanation
    // answer or a prose artifact (PR description, commit message). Trust the stop.
    if !ctx.requires_tools || ctx.prose_deliverable {
        return true;
    }
    // Action task with the reviewer DOWN: we cannot confirm completion. `work_total > 0`
    // only means SOME work happened, NOT that the task is finished — accepting it here is
    // exactly what quits multi-step runs mid-task after a flaky reviewer call. Report
    // not-done so the caller falls through to the bounded narration nudge (capped by
    // MAX_NUDGES + stall detection, which still guarantee termination) instead of
    // stopping early on a "let me do the next step" turn.
    false
}

/// True when the task still has unchecked `[ ]` steps — a reliable, model-authored
/// signal that the work is unfinished. Reads structured state, not prose intent:
/// the `update_plan` ledger AND the model's most recent message (many models write
/// their checklist as prose in the reply instead of calling `update_plan`, which
/// otherwise leaves the ledger empty and this guard blind).
fn plan_has_open_steps(session: &Session, latest_text: &str) -> bool {
    session
        .task_ledger
        .as_deref()
        .is_some_and(has_open_checkbox)
        || has_open_checkbox(latest_text)
}

fn has_open_checkbox(plan: &str) -> bool {
    plan.lines().any(|line| {
        let t = line.trim_start();
        let t = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .unwrap_or(t);
        t.starts_with("[ ]")
    })
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

/// True when the model's message ENDS by announcing an imminent action it did not
/// take this turn — "Let me read the file", "Now I'll edit …", or any sentence that
/// trails off with a ':' ("here's what I'll do next:"). A no-tool turn that ends this
/// way is mid-plan narration, never a finished task, so the loop must nudge the model
/// to actually act (see `classify_stop`).
///
/// Deliberately narrow to avoid nudging genuine final summaries: it inspects only the
/// LAST sentence and matches leading first-person action cues, and treats "let me
/// know" as a user-facing sign-off rather than a self-action.
fn ends_with_action_cue(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    // A trailing colon means the model was about to enumerate/do something next.
    if trimmed.ends_with(':') {
        return true;
    }
    // The cue usually sits in the LAST sentence ("… what's broken. Let me read it"),
    // so split on sentence/line boundaries and test only the final non-empty fragment.
    let last = trimmed
        .rsplit(|c: char| matches!(c, '.' | '!' | '?' | '\n' | ';'))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("");
    // Strip common leading markdown ("- ", "* ", "1. ", "**") before matching.
    let lower = last
        .trim_start_matches(|c: char| matches!(c, '-' | '*' | '#' | '>' | ' ' | '\t'))
        .to_lowercase();
    // "let me know (if …)" addresses the user; it's a sign-off, not a self-action.
    if lower.starts_with("let me know") {
        return false;
    }
    const CUES: &[&str] = &[
        "let me ",
        "let's ",
        "lets ",
        "i'll ",
        "i will ",
        "i am going to",
        "i'm going to",
        "im going to",
        "now i ",
        "now i'll",
        "now let me",
        "next i'll",
        "next, i",
        "first let me",
        "first, let me",
        "then i'll",
        "then let me",
    ];
    CUES.iter().any(|c| lower.starts_with(c))
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
    end_reason: EndReason,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let final_response = content.unwrap_or_default();
    tracing::info!(
        end_reason = end_reason.as_str(),
        iterations,
        "agent run finished"
    );
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
        end_reason,
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
    // Skip the entire write-back on a nearly-full disk: the reflection LLM call plus
    // embedding + SQLite writes here can stall and, worse, tip the machine into a
    // swap-thrash freeze. This turn's facts simply aren't persisted; the next turn on
    // a healthy disk resumes memory normally.
    let disk_probe = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if crate::disk::is_low(&disk_probe) {
        tracing::warn!("Skipping memory write-back: low disk space");
        return;
    }

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
    // Run-mode policy is re-read per batch so edits to `.getaibd/permissions.json`
    // take effect mid-session. `None` when no policy file exists (no behaviour change).
    let permissions = crate::permissions::Permissions::load(&session.project_root);
    let project_root = session.project_root.clone();

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
                let approved = check_approval(
                    tool.as_ref(),
                    call,
                    options,
                    permissions.as_ref(),
                    &project_root,
                    on_event,
                )
                .await;
                if approved {
                    let ask_gate = options
                        .and_then(|o| o.ask_gate.as_ref())
                        .filter(|_| call.name == "ask_question");
                    let terminal_gate = options
                        .and_then(|o| o.terminal_gate.as_ref())
                        .filter(|_| matches!(call.name.as_str(), "run_command" | "read_terminal"));
                    let editor_gate = options.and_then(|o| o.editor_gate.as_ref()).filter(|_| {
                        matches!(
                            call.name.as_str(),
                            "find_symbol" | "find_references" | "document_symbols"
                        )
                    });
                    if let Some(gate) = ask_gate {
                        delegate_ask(call, gate, on_event).await
                    } else if let Some(gate) = terminal_gate {
                        delegate_terminal(call, gate, on_event).await
                    } else if let Some(gate) = editor_gate {
                        delegate_editor(call, gate, tool.as_ref(), timeout_secs, on_event).await
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
        // Upgrade `patch_graph`'s ripgrep reference evidence to semantic LSP
        // references when the editor is attached. No-op (keeps ripgrep) otherwise.
        if call.name == "patch_graph" {
            enrich_patch_graph_refs(&call.id, &mut result, options, on_event).await;
        }
        if let Some(edit) = result.as_object_mut().and_then(|o| o.remove("_edit")) {
            // Best-effort post-edit diagnostics: the editor's language servers when
            // attached, else a headless tree-sitter parse check. Surfaced on the tool
            // result so the model sees compile/syntax errors and self-corrects.
            let path = edit
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let new_content = edit
                .get("new_content")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(diags) =
                post_edit_diagnostics(&call.id, &path, new_content.as_deref(), options, on_event)
                    .await
            {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("diagnostics".to_string(), diags);
                }
            }
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

/// Delegates a structural query (references/definition/symbols) to the editor's
/// language servers. Falls back to the tool's own headless implementation
/// (ripgrep / tree-sitter) if the editor is unresponsive or reports `unsupported`.
async fn delegate_editor(
    call: &ToolCall,
    gate: &crate::tools::editor_gate::EditorGate,
    tool: &dyn crate::tools::Tool,
    timeout_secs: u64,
    on_event: &mut impl FnMut(AgentEvent),
) -> serde_json::Value {
    let req_id = format!("{}_lsp", call.id);
    on_event(AgentEvent {
        kind: AgentEventKind::EditorRequest,
        content: Some(
            serde_json::json!({
                "request_id": req_id,
                "tool": call.name,
                "arguments": call.arguments,
            })
            .to_string(),
        ),
    });
    if let Some(raw) = gate.request(req_id).await {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            // The editor returns `{"unsupported": true}` to defer to the headless
            // path (e.g. no language server for this file type).
            if !v
                .get("unsupported")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                return v;
            }
        }
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        tool.execute(call.arguments.clone()),
    )
    .await
    {
        Ok(Ok(val)) => val,
        Ok(Err(e)) => serde_json::json!({ "error": e.to_string() }),
        Err(_) => {
            serde_json::json!({ "error": format!("Tool execution timeout after {timeout_secs}s") })
        }
    }
}

/// Reference-resolution budget for `patch_graph` LSP enrichment, matching the
/// tool's own ripgrep budget so the fan-out can't explode on a huge diff.
const PATCH_GRAPH_LSP_BUDGET: usize = 40;
/// Sample of reference sites kept per symbol after LSP enrichment.
const PATCH_GRAPH_REF_SAMPLE: usize = 12;

/// When the editor is attached, replace each changed symbol's ripgrep reference
/// evidence in a `patch_graph` result with semantic LSP references. On
/// unsupported/empty/timeout it leaves the ripgrep value in place, so this only
/// ever upgrades precision.
async fn enrich_patch_graph_refs(
    call_id: &str,
    result: &mut serde_json::Value,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) {
    let Some(gate) = options.and_then(|o| o.editor_gate.as_ref()) else {
        return;
    };
    let Some(files) = result.get_mut("files").and_then(|f| f.as_array_mut()) else {
        return;
    };

    let mut resolved = 0usize;
    for file in files.iter_mut() {
        let Some(symbols) = file.get_mut("changed_symbols").and_then(|s| s.as_array_mut())
        else {
            continue;
        };
        for sym in symbols.iter_mut() {
            if resolved >= PATCH_GRAPH_LSP_BUDGET {
                return;
            }
            let Some(name) = sym.get("name").and_then(|n| n.as_str()).map(str::to_string)
            else {
                continue;
            };
            let req_id = format!("{call_id}_pgref_{resolved}");
            resolved += 1;

            on_event(AgentEvent {
                kind: AgentEventKind::EditorRequest,
                content: Some(
                    serde_json::json!({
                        "request_id": req_id,
                        "tool": "find_references",
                        "arguments": { "symbol": name },
                    })
                    .to_string(),
                ),
            });

            let Some(raw) = gate.request(req_id).await else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if v.get("unsupported")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(refs) = v.get("references").and_then(|r| r.as_array()) {
                let count = v
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(refs.len(), |c| c as usize);
                let sample: Vec<serde_json::Value> =
                    refs.iter().take(PATCH_GRAPH_REF_SAMPLE).cloned().collect();
                if let Some(obj) = sym.as_object_mut() {
                    obj.insert(
                        "references".into(),
                        serde_json::json!({
                            "count": count,
                            "truncated": count > sample.len(),
                            "sample": sample,
                            "source": "lsp",
                        }),
                    );
                }
            }
        }
    }
}

/// Best-effort diagnostics for a file the agent just edited. Prefers the editor's
/// language servers (via the editor gate); falls back to a tree-sitter parse check
/// on the new content. Returns `None` when there is nothing worth reporting.
async fn post_edit_diagnostics(
    call_id: &str,
    path: &str,
    new_content: Option<&str>,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) -> Option<serde_json::Value> {
    if path.is_empty() {
        return None;
    }

    if let Some(gate) = options.and_then(|o| o.editor_gate.as_ref()) {
        let req_id = format!("{call_id}_diag");
        on_event(AgentEvent {
            kind: AgentEventKind::EditorRequest,
            content: Some(
                serde_json::json!({
                    "request_id": req_id,
                    "tool": "diagnostics",
                    "arguments": { "path": path },
                })
                .to_string(),
            ),
        });
        if let Some(raw) = gate.request(req_id).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                let diags = v.get("diagnostics").cloned().unwrap_or(v);
                // Non-empty language-server diagnostics win. An empty/unsupported
                // reply falls through to the tree-sitter backstop below (covers a
                // language server that isn't loaded yet or an unopened file).
                if diags.as_array().is_some_and(|a| !a.is_empty()) {
                    return Some(diags);
                }
            }
        }
    }

    // Headless fallback (also a backstop when the editor reported nothing): only
    // when we have the new content in hand.
    let content = new_content?;
    let errors = crate::memory::chunker::syntax_errors(content, std::path::Path::new(path));
    if errors.is_empty() {
        return None;
    }
    Some(serde_json::json!(errors
        .iter()
        .map(|(line, message)| serde_json::json!({ "line": line, "message": message }))
        .collect::<Vec<_>>()))
}

/// Emit an approval request and await the client's decision. When no approval gate
/// is wired (the client didn't opt into approvals) this returns `true` — there is
/// no channel to prompt on, so the run stays permissive as before.
async fn request_approval(
    call: &ToolCall,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) -> bool {
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

async fn check_approval(
    tool: &dyn crate::tools::Tool,
    call: &ToolCall,
    options: Option<&AgentOptions>,
    permissions: Option<&crate::permissions::Permissions>,
    project_root: &Path,
    on_event: &mut impl FnMut(AgentEvent),
) -> bool {
    // Run-mode policy (`.getaibd/permissions.json`) takes precedence when present.
    // `Deny` hard-blocks; `Ask` forces a prompt even for auto-run tools; `Allow`
    // skips the prompt; `None` defers to the built-in heuristics below.
    if let Some(perm) = permissions {
        match perm.decide(&call.name, &call.arguments, project_root) {
            Some(crate::permissions::Decision::Deny) => return false,
            Some(crate::permissions::Decision::Allow) => return true,
            Some(crate::permissions::Decision::Ask) => {
                return request_approval(call, options, on_event).await
            }
            None => {}
        }
    }

    if !tool.requires_approval() {
        return true;
    }

    // Read-only inspection commands (grep/rg/find/ls/cat/…) run without prompting — they
    // can't mutate the workspace, and network/shell launchers are hard-blocked elsewhere.
    if call.name == "run_command" && crate::tools::command::is_auto_approved(&call.arguments) {
        return true;
    }

    request_approval(call, options, on_event).await
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
    fn continuation_phrase_is_follow_up_with_prior_task() {
        let mut session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        session.push_message(ToolMessage::user("analyze the pipeline"));
        session.push_message(ToolMessage::assistant(
            "Issues found: 1) dead code in text_fixes.py 2) no repo clone cache 3) single model for all stages — these are the main problems I identified after reading the codebase.",
        ));
        let ctx = resolve_task_context(&session, "fix those issues");
        assert!(ctx.is_follow_up);
        assert_eq!(ctx.effective_task, "analyze the pipeline");
    }

    #[test]
    fn continuation_requires_prior_assistant_turn() {
        assert!(!is_continuation_request("fix those issues", &[]));
        let mut session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        session.push_message(ToolMessage::user("analyze"));
        session.push_message(ToolMessage::assistant("short"));
        assert!(!is_continuation_request("fix those issues", &session.messages));
    }

    #[test]
    fn continuation_detected_with_long_prior_reply() {
        let mut session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        session.push_message(ToolMessage::user("analyze"));
        session.push_message(ToolMessage::assistant(
            "Issues found: dead code, missing cache, model routing — full analysis with ten issues listed in detail for the user to review before implementing fixes.",
        ));
        assert!(is_continuation_request("fix those issues", &session.messages));
        assert!(is_continuation_request("go ahead", &session.messages));
        assert!(!is_continuation_request(
            "analyze a completely different repository from scratch",
            &session.messages
        ));
    }

    #[test]
    fn strip_leading_system_prefix_removes_only_head() {
        let mut msgs = vec![
            ToolMessage::system("rag context"),
            ToolMessage::system("env note"),
            ToolMessage::user("hello"),
            ToolMessage::assistant("hi"),
            ToolMessage::system("reflection nudge"),
        ];
        strip_leading_system_prefix(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[2].role, "system");
    }

    #[test]
    fn continuation_phrase_detection() {
        let mut session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        session.push_message(ToolMessage::assistant("x".repeat(150)));
        assert!(is_continuation_request("fix those issues", &session.messages));
        assert!(is_continuation_request("please implement your suggestions", &session.messages));
        assert!(!is_continuation_request(
            "analyze the aria pipeline from scratch",
            &session.messages
        ));
    }

    #[test]
    fn action_task_requires_tools() {
        assert!(task_requires_tools("implement login"));
        assert!(!task_requires_tools("explain the repo"));
    }

    #[test]
    fn open_plan_steps_detected() {
        assert!(has_open_checkbox("## Plan\n- [x] one\n- [ ] two\n"));
        assert!(has_open_checkbox("* [ ] do the thing"));
        assert!(!has_open_checkbox("## Plan\n- [x] one\n- [x] two\n"));
        assert!(!has_open_checkbox("no checkboxes here"));
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
        // Reviewer unavailable + an unfinished action task → not accepted, so the run
        // keeps going instead of quitting early.
        let v = CompletionVerdict {
            done: false,
            missing: vec!["x".into()],
            verified: false,
        };
        let session = Session::new("getaibd", "test", std::path::PathBuf::from("/tmp"));
        let action = TaskContext {
            current_input: "x".into(),
            effective_task: "x".into(),
            is_follow_up: false,
            requires_tools: true,
            prose_deliverable: false,
        };
        assert!(!completion_accepted(&v, 0, &session, &action, ""));
        // "Some work happened" is NOT "task finished": with the reviewer down we still
        // refuse to accept an action task on work count alone.
        assert!(!completion_accepted(&v, 2, &session, &action, ""));
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
        assert!(completion_accepted(&down, 0, &session, &qa, "here is how the repo works"));

        // An action task with the reviewer down is never accepted on work count alone
        // (the bounded narration nudge + stall detector drive it to real completion),
        // so a flaky reviewer can't end a half-finished task early.
        let action = TaskContext {
            current_input: "implement login".into(),
            effective_task: "implement login".into(),
            is_follow_up: false,
            requires_tools: true,
            prose_deliverable: false,
        };
        assert!(!completion_accepted(&down, 0, &session, &action, ""));
        assert!(!completion_accepted(&down, 2, &session, &action, ""));

        // A prose deliverable (PR text / commit message) IS satisfied by the reply
        // itself even with the reviewer down — there is no workspace change to verify.
        let prose = TaskContext {
            current_input: "write the PR description".into(),
            effective_task: "write the PR description".into(),
            is_follow_up: false,
            requires_tools: true,
            prose_deliverable: true,
        };
        assert!(completion_accepted(&down, 0, &session, &prose, "## Summary of changes"));
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

/// Unit tests for the pure stop-decision functions. These pin the ordering of the
/// completion logic without needing a provider, so a future edit that reintroduces
/// the premature-stop bug (e.g. accepting a no-tool turn while the plan has open
/// steps) fails here loudly.
#[cfg(test)]
mod stop_decision_tests {
    use super::*;

    fn sig() -> StopSignals {
        StopSignals {
            explicit_completion: false,
            content_empty: false,
            is_user_question: false,
            prose_deliverable: false,
            work_total: 0,
            plan_open: false,
            dangling_action_cue: false,
            nudge_count: 0,
            force_continue: 0,
        }
    }

    // The exact reported failure: the model ends a no-tool turn on "Let me read …".
    // It must be nudged to act, NOT routed to the reviewer (which could wrongly
    // rule it done and quit the run on its first step).
    #[test]
    fn dangling_action_cue_nudges_instead_of_review() {
        let s = StopSignals {
            dangling_action_cue: true,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::NudgeNarration);
    }

    // A genuine question that also happens to trip the cue still yields to the user.
    #[test]
    fn user_question_beats_action_cue() {
        let s = StopSignals {
            dangling_action_cue: true,
            is_user_question: true,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::FinishUserQuestion);
    }

    // Once the narration budget is spent, the cue can't loop forever: it falls
    // through to the reviewer (bounded by the force budget).
    #[test]
    fn action_cue_stops_nudging_when_budget_spent() {
        let s = StopSignals {
            dangling_action_cue: true,
            nudge_count: 6,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::Review);
    }

    #[test]
    fn detects_trailing_action_cues() {
        assert!(ends_with_action_cue(
            "The user wants me to fix the method. Let me read the actual method to understand what's broken."
        ));
        assert!(ends_with_action_cue(
            "Let me read the `patch_graph` method and `view_patch_graph` to see what to extract."
        ));
        assert!(ends_with_action_cue("Now I'll edit the file:"));
        assert!(ends_with_action_cue("Here's my plan:"));
        assert!(ends_with_action_cue("I'll update the config next."));
    }

    #[test]
    fn ignores_finished_summaries_and_signoffs() {
        assert!(!ends_with_action_cue(
            "Done. I updated the parser and all tests pass."
        ));
        assert!(!ends_with_action_cue(
            "The refactor is complete. Let me know if you need anything else."
        ));
        assert!(!ends_with_action_cue(
            "This change is going to make the API faster."
        ));
        assert!(!ends_with_action_cue(""));
    }

    #[test]
    fn explicit_completion_always_reviews() {
        let s = StopSignals {
            explicit_completion: true,
            // Even if it also reads like a question or has open steps, an explicit
            // claim is verified rather than short-circuited.
            is_user_question: true,
            plan_open: true,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::Review);
    }

    #[test]
    fn open_plan_steps_nudge_not_finish() {
        let s = StopSignals {
            plan_open: true,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::NudgePlanOpen);
    }

    #[test]
    fn genuine_question_yields() {
        let s = StopSignals {
            is_user_question: true,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::FinishUserQuestion);
    }

    #[test]
    fn prose_deliverable_finishes_without_review() {
        let s = StopSignals {
            prose_deliverable: true,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::FinishProse);
    }

    #[test]
    fn plain_stop_goes_to_review() {
        assert_eq!(classify_stop(&sig(), 6, 40), StopAction::Review);
    }

    #[test]
    fn budget_exhausted_finishes() {
        // Force budget spent AND narration budget spent -> finish, don't loop.
        let s = StopSignals {
            force_continue: 40,
            nudge_count: 6,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::FinishModelReply);
    }

    #[test]
    fn force_spent_falls_back_to_narration_nudge() {
        let s = StopSignals {
            force_continue: 40,
            nudge_count: 0,
            ..sig()
        };
        assert_eq!(classify_stop(&s, 6, 40), StopAction::NudgeNarration);
    }

    #[test]
    fn token_budget_allows_first_turn_and_trips_on_compounding_spend() {
        let budget = Some(1_000u64);
        // First turn (nothing spent yet) is always allowed, even if huge.
        assert!(!over_token_budget(0, 5_000, budget));
        // Under budget across turns: keep going.
        assert!(!over_token_budget(400, 500, budget));
        // Crossing the budget on the next turn: stop before paying for it.
        assert!(over_token_budget(900, 200, budget));
        // Disabled cap never trips.
        assert!(!over_token_budget(10_000_000, 10_000_000, None));
        // Saturating add can't panic on absurd counts.
        assert!(over_token_budget(u64::MAX, u64::MAX, Some(u64::MAX - 1)));
    }

    #[test]
    fn review_maps_to_expected_end_reasons() {
        assert_eq!(
            decide_after_review(true, true, true),
            ReviewAction::Finish(EndReason::CompletionToolVerified)
        );
        assert_eq!(
            decide_after_review(true, true, false),
            ReviewAction::Finish(EndReason::ReviewerConfirmedDone)
        );
        assert_eq!(
            decide_after_review(false, true, false),
            ReviewAction::VerifiedNotDone
        );
        assert_eq!(
            decide_after_review(false, false, false),
            ReviewAction::Unverified
        );
    }
}

/// End-to-end regression suite driving the whole agent loop with a scripted mock
/// provider. Each test asserts the final `EndReason`, covering the termination
/// failure matrix that used to quit runs early.
#[cfg(test)]
mod completion_loop_tests {
    use super::*;
    use crate::models::{ChatResponse, ModelInfo, ProviderHealth};
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A scripted reviewer outcome served for each "no tools" (reviewer) request.
    #[derive(Clone)]
    enum Verdict {
        Done,
        NotDone(Vec<&'static str>),
        /// The reviewer call itself fails (provider down / rate-limited).
        Unavailable,
    }

    /// A provider that replays canned worker turns and reviewer verdicts. It tells
    /// the two apart by the request's tool list: worker turns carry the registry's
    /// tool definitions; the reviewer sends `tools: []`.
    struct MockProvider {
        provider_id: &'static str,
        worker: Mutex<VecDeque<ToolChatResponse>>,
        verdicts: Mutex<VecDeque<Verdict>>,
        worker_calls: AtomicUsize,
        review_calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(
            provider_id: &'static str,
            worker: Vec<ToolChatResponse>,
            verdicts: Vec<Verdict>,
        ) -> Self {
            Self {
                provider_id,
                worker: Mutex::new(worker.into()),
                verdicts: Mutex::new(verdicts.into()),
                worker_calls: AtomicUsize::new(0),
                review_calls: AtomicUsize::new(0),
            }
        }
    }

    fn text_turn(s: &str) -> ToolChatResponse {
        ToolChatResponse {
            content: Some(s.to_string()),
            tool_calls: Vec::new(),
            usage: None,
        }
    }

    fn tool_turn(name: &str, args: serde_json::Value) -> ToolChatResponse {
        ToolChatResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: name.to_string(),
                arguments: args,
                extra: None,
            }],
            usage: None,
        }
    }

    fn complete_turn(summary: &str) -> ToolChatResponse {
        tool_turn(ATTEMPT_COMPLETION, json!({ "summary": summary }))
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.provider_id
        }
        fn display_name(&self) -> &'static str {
            "Mock"
        }
        // No retries: an Unavailable verdict should consume exactly one call so the
        // test's verdict queue stays predictable.
        fn max_retries(&self) -> u32 {
            0
        }
        fn supports_tool_calling(&self) -> bool {
            true
        }
        async fn health_check(&self) -> ProviderHealth {
            ProviderHealth {
                provider: self.provider_id.to_string(),
                healthy: true,
                message: None,
            }
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
            Ok(Vec::new())
        }
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, AppError> {
            Ok(ChatResponse {
                provider: self.provider_id.to_string(),
                model: "test-model".to_string(),
                content: String::new(),
                usage: None,
            })
        }
        fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>> {
            Box::pin(futures::stream::empty())
        }
        async fn chat_with_tools(
            &self,
            request: &ToolChatRequest,
        ) -> Result<ToolChatResponse, AppError> {
            // The reviewer (and the step-ceiling wrap-up) send no tools; worker turns
            // always carry the registry's tool definitions.
            if request.tools.is_empty() {
                self.review_calls.fetch_add(1, Ordering::SeqCst);
                let verdict = self.verdicts.lock().unwrap().pop_front();
                return match verdict {
                    Some(Verdict::Done) | None => {
                        Ok(text_turn("{\"done\": true, \"missing\": []}"))
                    }
                    Some(Verdict::NotDone(items)) => {
                        let arr = items
                            .iter()
                            .map(|s| format!("{s:?}"))
                            .collect::<Vec<_>>()
                            .join(",");
                        Ok(text_turn(&format!("{{\"done\": false, \"missing\": [{arr}]}}")))
                    }
                    Some(Verdict::Unavailable) => {
                        Err(AppError::ProviderError("reviewer unavailable".to_string()))
                    }
                };
            }
            self.worker_calls.fetch_add(1, Ordering::SeqCst);
            let next = self.worker.lock().unwrap().pop_front();
            // An empty reply with no tool calls ends the run if the script runs dry.
            Ok(next.unwrap_or_else(|| text_turn("")))
        }
    }

    fn temp_root() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("getaibd-agent-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run(
        provider_id: &'static str,
        task: &str,
        worker: Vec<ToolChatResponse>,
        verdicts: Vec<Verdict>,
    ) -> (AgentResult, Arc<MockProvider>) {
        let root = temp_root();
        let registry = ToolRegistry::build_default(&root);
        let mock = Arc::new(MockProvider::new(provider_id, worker, verdicts));
        let provider: Arc<dyn Provider> = mock.clone();
        let mut session = Session::new(provider_id, "test-model", root).with_max_iterations(250);
        let options = AgentOptions {
            auto_complete: true,
            enable_thinking: false,
            ..Default::default()
        };
        let mut on_event = |_ev: AgentEvent| {};
        let result = run_agent_with_memory(
            &mut session,
            task,
            &provider,
            &registry,
            None,
            Some(&options),
            &mut on_event,
        )
        .await
        .expect("agent run should not error");
        (result, mock)
    }

    const ACTION_TASK: &str = "implement the feature: add the download endpoint and wire it up";

    #[tokio::test]
    async fn narration_with_open_plan_continues_then_completes() {
        // Turn 1 narrates with a pending "[ ]" step (no tool call). The old bug quit
        // here; now it must nudge and continue. Turn 2 signals completion.
        let (result, mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                text_turn("Progress so far:\n- [x] added endpoint\n- [ ] wire it into the router"),
                complete_turn("Endpoint added and wired into the router."),
            ],
            vec![Verdict::Done],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(result.iterations, 2, "must not finish on the open-plan turn");
        assert_eq!(mock.worker_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn no_tool_stop_nudges_then_completes() {
        // The model stops without calling any tool. Under the completion-only rule this
        // is NOT "done" — it must be nudged and continue, then finish once it actually
        // calls attempt_completion. No reviewer is consulted.
        let (result, mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                text_turn("I believe everything is done now."),
                complete_turn("All requirements implemented and verified."),
            ],
            vec![Verdict::Unavailable],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(result.iterations, 2, "a plain no-tool stop must not end the run");
        assert_eq!(mock.worker_calls.load(Ordering::SeqCst), 2);
        assert_eq!(mock.review_calls.load(Ordering::SeqCst), 0, "no reviewer any more");
    }

    #[tokio::test]
    async fn attempt_completion_finishes_immediately() {
        // attempt_completion is the ONLY clean end and is trusted on its own — the run
        // stops on the first call, without a reviewer and without extra turns.
        let (result, mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                complete_turn("Download endpoint implemented."),
                complete_turn("(never reached)"),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(result.iterations, 1);
        assert_eq!(mock.worker_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.final_response, "Download endpoint implemented.");
    }

    #[tokio::test]
    async fn informational_completion_produces_answer_not_status_receipt() {
        let (result, mock) = run(
            "mock",
            "Explain the attached agentic_fix.py file.",
            vec![complete_turn(
                "Outcome: Explained agentic_fix.py. Changes: none. Notes: none.",
            )],
            vec![Verdict::Done],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(result.iterations, 1);
        assert_eq!(mock.review_calls.load(Ordering::SeqCst), 1);
        assert_ne!(
            result.final_response,
            "Outcome: Explained agentic_fix.py. Changes: none. Notes: none."
        );
    }

    #[tokio::test]
    async fn prose_question_does_not_end_run() {
        // A prose question is not the completion signal, so the run does NOT yield on
        // it (the old behavior). It nudges — steering the model toward ask_question or
        // continuing — and finishes only when attempt_completion is called.
        let (result, _mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                text_turn("Which storage backend should the endpoint use, S3 or local disk?"),
                complete_turn("Used local disk as the default backend."),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(result.iterations, 2);
    }

    #[tokio::test]
    async fn repeated_identical_turns_stop_as_repeat_loop() {
        // The backstop for a model that neither acts nor completes: byte-identical
        // no-tool answers trip the repeat guard (REPEAT_STOP_TEXT = 2 → 3rd identical).
        let (result, _mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                text_turn("Working on the exact same thing."),
                text_turn("Working on the exact same thing."),
                text_turn("Working on the exact same thing."),
            ],
            vec![Verdict::NotDone(vec!["still missing the endpoint"])],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::RepeatLoop);
        assert_eq!(result.iterations, 3);
    }

    /// A pathological provider that NEVER converges: every turn it emits a fresh, distinct
    /// tool call (a `read_file` on a unique path, so the byte-identical repeat guard never
    /// trips) and never signals completion. It counts every call it receives so a test can
    /// assert the run's total LLM spend is hard-bounded. This is precisely the "keeps
    /// calling APIs without converging" failure the guards must contain.
    struct RunawayProvider {
        calls: AtomicUsize,
    }

    impl RunawayProvider {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for RunawayProvider {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn display_name(&self) -> &'static str {
            "Runaway"
        }
        fn max_retries(&self) -> u32 {
            0
        }
        fn supports_tool_calling(&self) -> bool {
            true
        }
        async fn health_check(&self) -> ProviderHealth {
            ProviderHealth {
                provider: "mock".to_string(),
                healthy: true,
                message: None,
            }
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
            Ok(Vec::new())
        }
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                provider: "mock".to_string(),
                model: "test-model".to_string(),
                // The step-ceiling wrap-up is a plain `chat`; return a short summary so the
                // run finishes cleanly instead of falling back to last_text.
                content: "Reached the step limit.".to_string(),
                usage: None,
            })
        }
        fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>> {
            Box::pin(futures::stream::empty())
        }
        async fn chat_with_tools(
            &self,
            request: &ToolChatRequest,
        ) -> Result<ToolChatResponse, AppError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            // The wrap-up / reviewer send no tools: answer with a benign done-ish verdict so
            // those paths terminate rather than erroring (we're testing the ceilings, not
            // the reviewer). Worker turns get an ever-changing tool call.
            if request.tools.is_empty() {
                return Ok(text_turn("{\"done\": false, \"missing\": [\"never finishes\"]}"));
            }
            Ok(tool_turn("read_file", json!({ "path": format!("nonexistent-{n}.txt") })))
        }
    }

    async fn run_runaway(
        max_iterations: u32,
        max_llm_calls: Option<u32>,
        max_tool_calls: Option<u32>,
        auto_complete: bool,
    ) -> (AgentResult, Arc<RunawayProvider>) {
        let root = temp_root();
        let registry = ToolRegistry::build_default(&root);
        let mock = Arc::new(RunawayProvider::new());
        let provider: Arc<dyn Provider> = mock.clone();
        let mut session =
            Session::new("mock", "test-model", root).with_max_iterations(max_iterations);
        let options = AgentOptions {
            auto_complete,
            enable_thinking: false,
            max_llm_calls,
            max_tool_calls,
            ..Default::default()
        };
        let mut on_event = |_ev: AgentEvent| {};
        let result = run_agent_with_memory(
            &mut session,
            ACTION_TASK,
            &provider,
            &registry,
            None,
            Some(&options),
            &mut on_event,
        )
        .await
        .expect("agent run should terminate, not error");
        (result, mock)
    }

    #[tokio::test]
    async fn runaway_tool_calls_stop_at_call_ceiling() {
        // A model that never converges, with a tiny explicit LLM-call budget. The run must
        // stop the instant the ceiling is reached — total calls are hard-bounded by it, and
        // the loop cannot fan out into unbounded spend.
        let (result, mock) = run_runaway(10_000, Some(6), None, true).await;
        assert_eq!(result.end_reason, EndReason::CallBudgetExhausted);
        assert_eq!(result.iterations, 6, "must stop exactly at the call ceiling");
        // Not one paid call past the budget: the ceiling is checked BEFORE each request.
        assert_eq!(mock.calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn runaway_tool_calls_stop_at_step_ceiling_when_call_budget_is_high() {
        // With the call budget effectively disabled, the SAME non-converging model is still
        // bounded — the iteration ceiling stops it. Total LLM calls stay within a small,
        // predictable envelope (turns + a single wrap-up summary), never unbounded.
        // Read-only Ask/Plan-style run (no auto-complete reviewer) so the iteration ceiling
        // is the sole, deterministic stop. 8 worker turns + exactly one wrap-up summary call.
        let (result, mock) = run_runaway(8, Some(u32::MAX), None, false).await;
        assert_eq!(result.end_reason, EndReason::StepCeiling);
        assert_eq!(result.iterations, 8);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 9);
    }

    #[tokio::test]
    async fn evidence_budget_forces_a_final_answer_without_more_tools() {
        let (result, mock) =
            run_runaway(100, Some(20), Some(3), false).await;
        assert_eq!(result.end_reason, EndReason::NaturalStop);
        assert_eq!(result.iterations, 3);
        assert_eq!(
            mock.calls.load(Ordering::SeqCst),
            4,
            "three tool turns plus one tool-free final report"
        );
    }

    #[tokio::test]
    async fn default_call_ceiling_bounds_a_runaway_run() {
        // No explicit budget: the DEFAULT ceiling (or an earlier guard) must still stop a
        // non-converging model. The run terminates and its total spend is bounded well
        // under any pathological blow-up.
        let (result, mock) = run_runaway(10_000, None, None, true).await;
        assert!(
            matches!(
                result.end_reason,
                EndReason::CallBudgetExhausted
                    | EndReason::StepCeiling
                    | EndReason::CostBudgetExhausted
                    | EndReason::StallDetected
            ),
            "a runaway run must hit a hard backstop, got {:?}",
            result.end_reason
        );
        assert!(
            mock.calls.load(Ordering::SeqCst) <= DEFAULT_MAX_LLM_CALLS as usize + 2,
            "total LLM calls must stay within the default ceiling"
        );
    }

    #[tokio::test]
    async fn reworded_no_tool_answers_stop_by_stagnation() {
        // Three DISTINCT no-tool answers (reworded each time, so the byte-identical
        // repeat guard never trips) with no completion signal — the exact "keeps
        // re-explaining, slightly differently, forever" loop. The stagnation counter
        // must accept the last answer and stop instead of grinding to the budget.
        let (result, mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                text_turn("Here is an explanation of the code, version one."),
                text_turn("Let me restate it: the same explanation, worded differently."),
                text_turn("To summarize once more, the code does the following things."),
                complete_turn("(never reached)"),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::NaturalStop);
        assert_eq!(
            result.iterations, 3,
            "stagnation must stop on the 3rd consecutive no-tool turn"
        );
        assert_eq!(mock.worker_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn informational_request_ends_on_first_answer() {
        // An "explain …" request answered in plain prose (no tool call, no completion
        // signal, no trailing action cue, nothing mutated) must end on that single
        // answer instead of being nudged into re-explaining the same thing — the exact
        // "same reply multiple times" loop. The 2nd scripted turn must never be reached.
        let (result, mock) = run(
            "mock",
            "explain what the download endpoint handler does in this file",
            vec![
                text_turn(
                    "The handler validates the request, streams the file from storage, \
                     and sets the Content-Disposition header so the browser downloads it.",
                ),
                text_turn("(should never be reached)"),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::NaturalStop);
        assert_eq!(result.iterations, 1, "a Q&A answer must not be repeated");
        assert_eq!(mock.worker_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn informational_narration_with_next_step_does_not_quit() {
        // Regression: an investigation turn that ANNOUNCES a next step ("Let me look
        // … I need to read lines 2195-2252") must NOT be accepted by the Q&A fast-path
        // — the run must continue (read the file) and only end on the real answer.
        // This is the "quitting mid-run" case where "I need to read" slipped past the
        // last-sentence-only action-cue check.
        let (result, mock) = run(
            "mock",
            "why does the gap analyzer keep looping without converging?",
            vec![
                text_turn(
                    "The user is right — I repeated the same read_file call. Let me look \
                     at the gap analyzer code properly. I need to read lines 2195-2252 \
                     to see the actual implementation.",
                ),
                tool_turn("read_file", json!({ "path": "runtime.rs" })),
                text_turn(
                    "The loop never converges because the analyzer re-requests the same \
                     diff each iteration without updating its baseline, so the verdict \
                     stays 'not done'.",
                ),
                text_turn("(should never be reached)"),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::NaturalStop);
        assert_eq!(
            result.iterations, 3,
            "narration announcing a next step must not stop the run"
        );
        assert!(result.final_response.contains("never converges"));
    }

    #[tokio::test]
    async fn action_task_no_tool_claim_is_not_fast_stopped() {
        // The mirror image: an ACTION request answered with a plain "it's done" but
        // NOTHING mutated must NOT take the Q&A fast-path — it has to keep going (nudge)
        // and only ends on a real attempt_completion, so we never accept empty work.
        let (result, _mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                text_turn("Everything looks complete and correct now."),
                complete_turn("Download endpoint implemented and wired in."),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(result.iterations, 2, "an action task must not stop on a bare claim");
    }

    #[tokio::test]
    async fn interleaved_identical_mutations_stop_as_repeat_loop() {
        // A→B→A→B→A of identical writes: never consecutive, so the consecutive
        // repeat guard misses it. The mutation-oscillation guard must catch the 3rd
        // identical write (A) and stop rather than let the cycle run to the budget.
        let (result, _mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                tool_turn("write_file", json!({ "path": "a.txt", "content": "A" })),
                tool_turn("write_file", json!({ "path": "b.txt", "content": "B" })),
                tool_turn("write_file", json!({ "path": "a.txt", "content": "A" })),
                tool_turn("write_file", json!({ "path": "b.txt", "content": "B" })),
                tool_turn("write_file", json!({ "path": "a.txt", "content": "A" })),
                complete_turn("(never reached)"),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::RepeatLoop);
        assert_eq!(
            result.iterations, 5,
            "must stop on the 3rd identical mutating turn (A)"
        );
    }

    #[tokio::test]
    async fn interleaved_identical_reads_do_not_trigger_oscillation() {
        // The SAME non-mutating action repeated (A→B→A→B→A of identical read_file)
        // is legitimate iteration — the mutation-only oscillation guard must NOT stop
        // it, guarding against false positives on repeated builds/tests/reads. The
        // run proceeds all the way to the completion signal.
        let (result, mock) = run(
            "mock",
            ACTION_TASK,
            vec![
                tool_turn("read_file", json!({ "path": "a.txt" })),
                tool_turn("read_file", json!({ "path": "b.txt" })),
                tool_turn("read_file", json!({ "path": "a.txt" })),
                tool_turn("read_file", json!({ "path": "b.txt" })),
                tool_turn("read_file", json!({ "path": "a.txt" })),
                complete_turn("Done after inspecting the files."),
            ],
            vec![],
        )
        .await;
        assert_eq!(result.end_reason, EndReason::CompletionToolVerified);
        assert_eq!(
            result.iterations, 6,
            "repeated non-mutating actions must not be cut off"
        );
        assert_eq!(mock.worker_calls.load(Ordering::SeqCst), 6);
    }
}
