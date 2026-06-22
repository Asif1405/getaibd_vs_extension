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

        let original_prompt = session.system_prompt.clone();
        session.system_prompt = Some(detected_mode.system_prompt().to_string());

        let original_max_iterations = session.max_iterations;
        session.max_iterations = detected_mode.max_iterations();

        let result = match detected_mode {
            AgentMode::Ask => self.execute_ask(session, input, on_event).await,
            AgentMode::Plan => self.execute_plan(session, input, on_event).await,
            AgentMode::Agent => self.execute_agent(session, input, on_event).await,
            AgentMode::Debug => self.execute_debug(session, input, on_event).await,
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
        run_agent_with_memory(
            session,
            input,
            &self.provider,
            &self.registry,
            memory_ctx.as_ref(),
            None,
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
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            // Plan mode is meant to produce a plan and yield, not loop to "completion".
            auto_complete: false,
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
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            auto_complete: true,
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
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: Some(crate::context::ContextConfig::default()),
            enable_thinking: super::thinking::model_uses_reasoning(&session.model),
            auto_complete: true,
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
