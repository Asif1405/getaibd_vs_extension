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
use crate::models::{ToolCall, ToolChatRequest, ToolChatResponse, ToolMessage, ToolStreamDelta};
use crate::providers::Provider;
use crate::retry::chat_with_tools_retry_cb;
use crate::tools::approval::ApprovalGate;
use crate::tools::ToolRegistry;

use super::session::Session;
use super::thinking;

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
    pub tool_timeout_secs: u64,
    pub circuit_breaker: Option<Arc<CircuitBreaker>>,
    pub context_config: Option<ContextConfig>,
    pub enable_thinking: bool,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            approval_gate: None,
            tool_timeout_secs: 300,
            circuit_breaker: None,
            context_config: None,
            enable_thinking: true,
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
    agent_loop(session, provider, registry, memory, options, on_event).await
}

async fn inject_context(session: &mut Session, task: &str, memory: Option<&MemoryContext<'_>>) {
    if let Some(sys) = &session.system_prompt {
        session.push_message(ToolMessage::system(sys.clone()));
    }

    let persistent = PersistentMemory::new(&session.project_root);
    if persistent.exists() {
        let facts = persistent.as_context();
        if !facts.is_empty() {
            session.push_message(ToolMessage::system(facts));
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
                session.push_message(ToolMessage::system(ctx));
            }
            _ => {
                if let Ok(memories) =
                    retrieve_context(mem.store, mem.embedder, task, mem.top_k).await
                {
                    let ctx = format_context(&memories);
                    if !ctx.is_empty() {
                        session.push_message(ToolMessage::system(ctx));
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn agent_loop(
    session: &mut Session,
    provider: &Arc<dyn Provider>,
    registry: &ToolRegistry,
    memory: Option<&MemoryContext<'_>>,
    options: Option<&AgentOptions>,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let tool_defs = registry.definitions();
    let mut iterations = 0;
    let use_streaming = provider.supports_streaming_tools();
    let enable_thinking = options.is_none_or(|o| o.enable_thinking);

    if enable_thinking && iterations == 0 {
        session.push_message(ToolMessage::system(thinking::PLANNING_PROMPT.to_string()));
    }

    loop {
        if iterations >= session.max_iterations {
            return Err(AppError::InvalidRequest(
                "Max agent iterations reached".into(),
            ));
        }

        const COMPRESS_THRESHOLD: usize = 30;
        if session.messages.len() > COMPRESS_THRESHOLD {
            session.compress_messages();
            on_event(AgentEvent {
                kind: AgentEventKind::ContextCompressed,
                content: Some("Conversation memory compressed".into()),
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
        };

        let response = if use_streaming {
            collect_streaming_response(provider, request, on_event).await?
        } else {
            let cb = options.and_then(|o| o.circuit_breaker.as_deref());
            chat_with_tools_retry_cb(provider, &request, cb).await?
        };
        iterations += 1;

        if enable_thinking {
            if let Some(text) = &response.content {
                emit_thinking_events(text, on_event);
            }
        }

        if response.tool_calls.is_empty() {
            return finish_agent(session, response.content, memory, iterations, on_event).await;
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

        session.push_message(ToolMessage::assistant_tool_calls(
            response.tool_calls.clone(),
        ));
        execute_tool_calls(&response.tool_calls, registry, options, session, on_event).await;

        if enable_thinking {
            session.push_message(ToolMessage::system(thinking::REFLECTION_PROMPT.to_string()));
        }
    }
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

    while let Some(delta) = stream.next().await {
        match delta? {
            ToolStreamDelta::Token(token) => {
                content.push_str(&token);
                on_event(AgentEvent {
                    kind: AgentEventKind::Response,
                    content: Some(token),
                });
            }
            ToolStreamDelta::ToolCallStart { id, name } => {
                if !current_tool_id.is_empty() {
                    let args: serde_json::Value =
                        serde_json::from_str(&current_tool_args).unwrap_or_default();
                    tool_calls.push(ToolCall {
                        id: current_tool_id.clone(),
                        name: current_tool_name.clone(),
                        arguments: args,
                    });
                    current_tool_args.clear();
                }
                current_tool_id = id;
                current_tool_name = name;
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

async fn finish_agent(
    session: &Session,
    content: Option<String>,
    memory: Option<&MemoryContext<'_>>,
    iterations: u32,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let final_response = content.unwrap_or_default();
    on_event(AgentEvent {
        kind: AgentEventKind::Complete,
        content: Some(final_response.clone()),
    });

    if let Some(mem) = memory {
        let indexer = MemoryIndexer::new(mem.store, mem.embedder);
        let _ = indexer.index_session(&session.id, &session.messages).await;
        let _ = mem.store.prune_oldest(mem.max_entries);
    }

    Ok(AgentResult {
        final_response,
        iterations,
        mode: None,
    })
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
            content: Some(format!("{}({})", call.name, call.arguments)),
        });

        let result = match registry.get(&call.name) {
            Some(tool) => {
                let approved = check_approval(tool.as_ref(), call, options, on_event).await;
                if approved {
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
                } else {
                    serde_json::json!({ "error": "Tool execution denied by user" })
                }
            }
            None => serde_json::json!({ "error": format!("Unknown tool: {}", call.name) }),
        };

        on_event(AgentEvent {
            kind: AgentEventKind::ToolResult,
            content: Some(serde_json::to_string(&result).unwrap_or_default()),
        });

        let result_str = serde_json::to_string(&result).unwrap_or_default();
        session.push_message(ToolMessage::tool_result(&call.id, result_str));
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
        kind: AgentEventKind::ToolCall,
        content: Some(format!("Approval required: {} - {}", call.name, req_id)),
    });
    gate.request(req_id).await
}
