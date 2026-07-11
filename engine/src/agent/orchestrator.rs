use std::sync::Arc;

use crate::error::AppError;
use crate::memory::{EmbeddingProvider, MemoryStore};
use crate::models::ToolMessage;
use crate::providers::Provider;
use crate::tools::ToolRegistry;

use super::modes::{AgentMode, ModeSelector};
use super::runtime::{run_agent_with_memory, AgentEvent, AgentOptions, AgentResult, MemoryContext};
use super::session::Session;

pub struct Orchestrator {
    provider: Arc<dyn Provider>,
    registry: ToolRegistry,
    memory_store: Option<Arc<MemoryStore>>,
    embedder: Option<Box<dyn EmbeddingProvider>>,
    auto_mode: bool,
    approval_gate: Option<crate::tools::approval::ApprovalGate>,
    terminal_gate: Option<crate::tools::terminal_gate::TerminalGate>,
    ask_gate: Option<crate::tools::ask_gate::AskGate>,
    editor_gate: Option<crate::tools::editor_gate::EditorGate>,
    /// Multi-agent pipeline: when enabled, Agent/Debug runs route through the
    /// staged analyzer/explorer/planner/worker/validator flow (auto-gated).
    pipeline_enabled: bool,
    pipeline_max_fix_cycles: u32,
    pipeline_explorer_max_iters: u32,
}

impl Orchestrator {
    pub fn new(provider: Arc<dyn Provider>, registry: ToolRegistry) -> Self {
        Self {
            provider,
            registry,
            memory_store: None,
            embedder: None,
            auto_mode: true,
            approval_gate: None,
            terminal_gate: None,
            ask_gate: None,
            editor_gate: None,
            pipeline_enabled: false,
            pipeline_max_fix_cycles: 2,
            pipeline_explorer_max_iters: 14,
        }
    }

    /// Enable the multi-agent pipeline for Agent/Debug runs. `GETAIBD_PIPELINE=0`
    /// disables it at runtime regardless of this flag.
    pub fn with_pipeline(
        mut self,
        enabled: bool,
        max_fix_cycles: u32,
        explorer_max_iters: u32,
    ) -> Self {
        self.pipeline_enabled = enabled;
        self.pipeline_max_fix_cycles = max_fix_cycles;
        self.pipeline_explorer_max_iters = explorer_max_iters;
        self
    }

    /// Effective pipeline switch: env override wins over the configured flag.
    fn pipeline_active(&self) -> bool {
        match std::env::var("GETAIBD_PIPELINE") {
            Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false")),
            Err(_) => self.pipeline_enabled,
        }
    }

    pub fn with_approval_gate(mut self, gate: crate::tools::approval::ApprovalGate) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    pub fn with_terminal_gate(mut self, gate: crate::tools::terminal_gate::TerminalGate) -> Self {
        self.terminal_gate = Some(gate);
        self
    }

    pub fn with_ask_gate(mut self, gate: crate::tools::ask_gate::AskGate) -> Self {
        self.ask_gate = Some(gate);
        self
    }

    pub fn with_editor_gate(mut self, gate: crate::tools::editor_gate::EditorGate) -> Self {
        self.editor_gate = Some(gate);
        self
    }

    pub fn with_memory(
        mut self,
        store: Arc<MemoryStore>,
        embedder: Box<dyn EmbeddingProvider>,
    ) -> Self {
        self.memory_store = Some(store);
        self.embedder = Some(embedder);
        self
    }

    pub fn with_auto_mode(mut self, enabled: bool) -> Self {
        self.auto_mode = enabled;
        self
    }

    pub async fn execute(
        &self,
        session: &mut Session,
        input: &str,
        mode: Option<AgentMode>,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        let detected_mode = mode.unwrap_or_else(|| {
            if self.auto_mode {
                ModeSelector::detect_mode(input)
            } else {
                AgentMode::Ask
            }
        });

        on_event(AgentEvent {
            kind: super::runtime::AgentEventKind::ModeSelected,
            content: Some(detected_mode.as_str().to_string()),
        });

        let original_max_iterations = session.max_iterations;
        session.max_iterations = detected_mode.max_iterations();

        // Render the prompt: some prompts embed `{max_iterations}` and must have it
        // substituted before reaching the model, or the literal token leaks through.
        let original_prompt = session.system_prompt.clone();
        session.system_prompt = Some(detected_mode.rendered_system_prompt(session.max_iterations));

        let result = match detected_mode {
            AgentMode::Ask => self.execute_ask(session, input, on_event).await,
            AgentMode::Plan => self.execute_plan(session, input, on_event).await,
            AgentMode::Agent if self.pipeline_active() => {
                self.execute_pipeline(session, input, on_event).await
            }
            AgentMode::Debug if self.pipeline_active() => {
                on_event(AgentEvent {
                    kind: super::runtime::AgentEventKind::Think,
                    content: Some("Analyzing error and gathering context...".to_string()),
                });
                self.execute_pipeline(session, input, on_event).await
            }
            AgentMode::Agent => self.execute_agent(session, input, on_event).await,
            AgentMode::Debug => self.execute_debug(session, input, on_event).await,
            AgentMode::Reviewer => self.execute_reviewer(session, input, on_event).await,
        };

        session.system_prompt = original_prompt;
        session.max_iterations = original_max_iterations;

        result
    }

    async fn execute_ask(
        &self,
        session: &mut Session,
        input: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        session.push_message(ToolMessage::user(input));

        let memory_ctx = self.memory_context();
        // Ask mode still has tool access (it can run read-only commands, etc.), so it
        // must carry the gates too — otherwise approval/ask are silently bypassed.
        let options = AgentOptions {
            approval_gate: self.approval_gate.clone(),
            terminal_gate: self.terminal_gate.clone(),
            ask_gate: self.ask_gate.clone(),
            editor_gate: self.editor_gate.clone(),
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            // Ask is meant to answer and yield, not loop to "completion".
            auto_complete: false,
            temperature: None,
            max_llm_calls: None,
            max_tool_calls: None,
        };
        // attempt_completion is an action-mode completion signal; Ask has no
        // auto-complete loop, so filter it out to keep the toolset clean.
        let registry = self
            .registry
            .filter(|name| name != super::runtime::ATTEMPT_COMPLETION);
        run_agent_with_memory(
            session,
            input,
            &self.provider,
            &registry,
            memory_ctx.as_ref(),
            Some(&options),
            on_event,
        )
        .await
    }

    async fn execute_plan(
        &self,
        session: &mut Session,
        input: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        session.push_message(ToolMessage::user(input));

        let memory_ctx = self.memory_context();
        let options = AgentOptions {
            approval_gate: self.approval_gate.clone(),
            terminal_gate: self.terminal_gate.clone(),
            ask_gate: self.ask_gate.clone(),
            editor_gate: self.editor_gate.clone(),
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            // Plan mode is meant to produce a plan and yield, not loop to "completion".
            auto_complete: false,
            temperature: None,
            max_llm_calls: None,
            max_tool_calls: None,
        };
        // Plan is read-only: hand it a filtered registry with just the read/search
        // tools plus `write_plan` (temp file), so it can never mutate the repo.
        let registry = match AgentMode::Plan.tool_allowlist() {
            Some(allow) => self.registry.filter(|name| allow.contains(&name)),
            None => self.registry.filter(|_| true),
        };
        run_agent_with_memory(
            session,
            input,
            &self.provider,
            &registry,
            memory_ctx.as_ref(),
            Some(&options),
            on_event,
        )
        .await
    }

    async fn execute_reviewer(
        &self,
        session: &mut Session,
        input: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        session.push_message(ToolMessage::user(input));

        let memory_ctx = self.memory_context();
        let options = AgentOptions {
            approval_gate: self.approval_gate.clone(),
            terminal_gate: self.terminal_gate.clone(),
            ask_gate: self.ask_gate.clone(),
            editor_gate: self.editor_gate.clone(),
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            // API reasoning effort still flows through the session. Disable only
            // the generic plan/reflect scaffolding, which otherwise tells thinking
            // models to re-plan after every read and causes review roaming.
            enable_thinking: false,
            // Reviewer fetches, inspects, and reports, then yields — it is not an
            // auto-complete action loop.
            auto_complete: false,
            temperature: None,
            // A small review converges in a handful of calls, but a LARGE PR needs
            // several reads to page through the diff and the surrounding base code it
            // touches. Set the ceiling high enough for a big PR while staying well below
            // the global default — the web_fetch per-URL cache prevents the re-fetch
            // loop that used to burn this budget, so convergence is driven by the prompt
            // ("stop once every hunk is reviewed"), not by a tight cap.
            max_llm_calls: Some(50),
            max_tool_calls: Some(50),
        };
        // Read-only: hand it the reviewer allowlist (read/search/fetch tools only), so
        // it can never mutate the repo or open a shell. attempt_completion is not in the
        // allowlist, so a stray call is dropped along with every write/run tool.
        let registry = match AgentMode::Reviewer.tool_allowlist() {
            Some(allow) => self.registry.filter(|name| allow.contains(&name)),
            None => self.registry.filter(|_| true),
        };
        run_agent_with_memory(
            session,
            input,
            &self.provider,
            &registry,
            memory_ctx.as_ref(),
            Some(&options),
            on_event,
        )
        .await
    }

    async fn execute_agent(
        &self,
        session: &mut Session,
        input: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        if !self.provider.supports_tool_calling() {
            return Err(AppError::ProviderError(format!(
                "Provider {} does not support tool calling required for agent mode",
                self.provider.id()
            )));
        }

        let memory_ctx = self.memory_context();
        let options = AgentOptions {
            approval_gate: self.approval_gate.clone(),
            terminal_gate: self.terminal_gate.clone(),
            ask_gate: self.ask_gate.clone(),
            editor_gate: self.editor_gate.clone(),
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            auto_complete: true,
            // Pin a low temperature for action modes so the worker reports what it
            // actually observed instead of inventing a plausible-sounding outcome.
            // Reasoning models reject a custom temperature, so leave them at default.
            temperature: if super::thinking::model_uses_reasoning(&session.model) {
                None
            } else {
                Some(0.1)
            },
            max_llm_calls: None,
            max_tool_calls: None,
        };

        run_agent_with_memory(
            session,
            input,
            &self.provider,
            &self.registry,
            memory_ctx.as_ref(),
            Some(&options),
            on_event,
        )
        .await
    }

    async fn execute_debug(
        &self,
        session: &mut Session,
        input: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        if !self.provider.supports_tool_calling() {
            return Err(AppError::ProviderError(format!(
                "Provider {} does not support tool calling required for debug mode",
                self.provider.id()
            )));
        }

        on_event(AgentEvent {
            kind: super::runtime::AgentEventKind::Think,
            content: Some("Analyzing error and gathering context...".to_string()),
        });

        let memory_ctx = self.memory_context();
        let options = AgentOptions {
            approval_gate: self.approval_gate.clone(),
            terminal_gate: self.terminal_gate.clone(),
            ask_gate: self.ask_gate.clone(),
            editor_gate: self.editor_gate.clone(),
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            auto_complete: true,
            // Pin a low temperature for action modes so the worker reports what it
            // actually observed instead of inventing a plausible-sounding outcome.
            // Reasoning models reject a custom temperature, so leave them at default.
            temperature: if super::thinking::model_uses_reasoning(&session.model) {
                None
            } else {
                Some(0.1)
            },
            max_llm_calls: None,
            max_tool_calls: None,
        };

        run_agent_with_memory(
            session,
            input,
            &self.provider,
            &self.registry,
            memory_ctx.as_ref(),
            Some(&options),
            on_event,
        )
        .await
    }

    /// Run the staged multi-agent pipeline. The Worker phase reuses the same
    /// registry/gates/memory as `execute_agent`, so approvals and reflection are
    /// unchanged; any stage failure degrades to the plain worker loop.
    async fn execute_pipeline(
        &self,
        session: &mut Session,
        input: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<AgentResult, AppError> {
        if !self.provider.supports_tool_calling() {
            return Err(AppError::ProviderError(format!(
                "Provider {} does not support tool calling required for agent mode",
                self.provider.id()
            )));
        }
        let memory_ctx = self.memory_context();
        let deps = super::pipeline::PipelineDeps {
            provider: &self.provider,
            registry: &self.registry,
            memory: memory_ctx.as_ref(),
            approval_gate: self.approval_gate.clone(),
            terminal_gate: self.terminal_gate.clone(),
            ask_gate: self.ask_gate.clone(),
            editor_gate: self.editor_gate.clone(),
            max_fix_cycles: self.pipeline_max_fix_cycles,
            explorer_max_iters: self.pipeline_explorer_max_iters,
        };
        super::pipeline::run_pipeline(&deps, session, input, on_event).await
    }

    fn memory_context(&self) -> Option<MemoryContext<'_>> {
        match (&self.memory_store, &self.embedder) {
            (Some(store), Some(embedder)) => Some(MemoryContext {
                store,
                embedder: embedder.as_ref(),
                top_k: 10,
                max_entries: 50,
                analysis_cache: None,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConversationMemory {
    messages: Vec<ToolMessage>,
    max_messages: usize,
    summary: Option<String>,
}

impl ConversationMemory {
    pub fn new(max_messages: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_messages,
            summary: None,
        }
    }

    pub fn push(&mut self, message: ToolMessage) {
        self.messages.push(message);

        if self.messages.len() > self.max_messages {
            self.compress();
        }
    }

    pub fn messages(&self) -> &[ToolMessage] {
        &self.messages
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    fn compress(&mut self) {
        let keep_count = self.max_messages / 2;
        let to_summarize = self.messages.len() - keep_count;

        let old_messages = self.messages.drain(..to_summarize).collect::<Vec<_>>();

        let summary_text = old_messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n");

        self.summary = Some(format!("Previous conversation summary:\n{}", summary_text));
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.summary = None;
    }
}
