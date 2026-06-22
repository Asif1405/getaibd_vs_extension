use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::agent::runtime::{run_agent_with_memory, AgentEventKind, AgentOptions, MemoryContext};
use crate::agent::session::Session;
use crate::error::AppError;
use crate::state::AppState;
use crate::tools::approval::ApprovalGate;

#[derive(Debug, Deserialize)]
pub struct AgentRequest {
    pub provider: String,
    pub model: String,
    pub task: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub require_approval: bool,
    /// Host OS reported by the client (e.g. "Windows", "macOS", "Linux").
    #[serde(default)]
    pub os: Option<String>,
    /// Active shell reported by the client (e.g. "PowerShell", "zsh", "bash").
    #[serde(default)]
    pub shell: Option<String>,
}

fn default_max_iterations() -> u32 {
    40
}

#[derive(Debug, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub approved: bool,
    /// Session UUID returned in the approval_required SSE event.
    pub session_id: Option<String>,
}

#[allow(clippy::unused_async)]
pub async fn agent_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let provider = state
        .providers
        .get(&req.provider)
        .ok_or_else(|| AppError::UnknownProvider(req.provider.clone()))?
        .clone();

    if !provider.supports_tool_calling() {
        return Err(AppError::ProviderError(format!(
            "{}: does not support tool calling",
            req.provider
        )));
    }

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);

    // Each agent session gets its own scoped gate identified by a UUID
    let session_id = Uuid::new_v4().to_string();
    let gate = if req.require_approval {
        let g = ApprovalGate::new();
        state.set_approval_gate(&session_id, g.clone());
        Some(g)
    } else {
        None
    };
    let ask_gate = {
        let g = crate::tools::ask_gate::AskGate::new();
        state.set_ask_gate(&session_id, g.clone());
        g
    };

    let project_root = state.project_root.clone();
    let max_iter = req.max_iterations.min(state.max_iterations);
    let registry = crate::tools::ToolRegistry::build_default(&project_root);
    let sid = session_id.clone();

    let sid_for_task = session_id.clone();
    tokio::spawn(async move {
        run_agent_task(
            &state,
            req,
            provider,
            registry,
            max_iter,
            project_root,
            gate,
            ask_gate,
            sid_for_task,
            tx,
        )
        .await;
        state.clear_approval_gate(&sid);
        state.clear_ask_gate(&sid);
    });

    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_task(
    state: &AppState,
    req: AgentRequest,
    provider: Arc<dyn crate::providers::Provider>,
    registry: crate::tools::ToolRegistry,
    max_iter: u32,
    project_root: std::path::PathBuf,
    gate: Option<ApprovalGate>,
    ask_gate: crate::tools::ask_gate::AskGate,
    session_id_for_events: String,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    let environment =
        crate::agent::runtime::format_environment(req.os.as_deref(), req.shell.as_deref());
    let mut session = Session::new(&req.provider, &req.model, project_root)
        .with_max_iterations(max_iter)
        .with_environment(environment);

    if let Some(sys) = req.system_prompt {
        session = session.with_system_prompt(sys);
    }

    let mem_ctx = match (&state.memory_store, &state.embedder) {
        (Some(store), Some(embedder)) => Some(MemoryContext {
            store,
            embedder: embedder.as_ref(),
            top_k: state.memory_top_k,
            max_entries: state.memory_max_entries,
            analysis_cache: Some(state.analysis_cache.clone()),
        }),
        _ => None,
    };

    let opts = AgentOptions {
        approval_gate: gate,
        terminal_gate: None,
        ask_gate: Some(ask_gate),
        tool_timeout_secs: 300,
        circuit_breaker: Some(state.circuit_breaker.clone()),
        context_config: Some(state.context_config.clone()),
        enable_thinking: crate::agent::thinking::model_uses_reasoning(&session.model),
        auto_complete: true,
    };

    let approval_session = session_id_for_events.clone();
    let mut event_handler = |event: crate::agent::runtime::AgentEvent| {
        if let Some(evt) = agent_event_to_sse(&event, &approval_session) {
            let _ = tx.try_send(Ok(evt));
        }
    };

    let result = run_agent_with_memory(
        &mut session,
        &req.task,
        &provider,
        &registry,
        mem_ctx.as_ref(),
        Some(&opts),
        &mut event_handler,
    )
    .await;

    match result {
        Ok(agent_result) => {
            if let Ok(evt) = Event::default()
                .event("complete")
                .json_data(serde_json::json!({ "iterations": agent_result.iterations }))
            {
                let _ = tx.send(Ok(evt)).await;
            }
        }
        Err(e) => {
            let evt = Event::default().event("error").data(e.to_string());
            let _ = tx.send(Ok(evt)).await;
        }
    }
}

fn agent_event_to_sse(
    event: &crate::agent::runtime::AgentEvent,
    session_id: &str,
) -> Option<Event> {
    let data = event.content.as_deref().unwrap_or("");
    match event.kind {
        AgentEventKind::Start => Some(Event::default().event("start").data("Agent started")),
        AgentEventKind::Think => Some(Event::default().event("think").data("Thinking...")),
        AgentEventKind::ToolCall => Some(Event::default().event("tool_call").data(data)),
        AgentEventKind::ToolResult => Some(Event::default().event("tool_result").data(data)),
        AgentEventKind::Response => Some(Event::default().event("response").data(data)),
        AgentEventKind::Complete => {
            Some(Event::default().event("complete").data("Agent completed"))
        }
        AgentEventKind::Error => Some(Event::default().event("error").data(data)),
        AgentEventKind::ModeSelected => {
            Some(Event::default().event("mode_selected").data(data))
        }
        AgentEventKind::Planning => Some(Event::default().event("planning").data(data)),
        AgentEventKind::Thinking => Some(Event::default().event("thinking").data(data)),
        AgentEventKind::Reflecting => Some(Event::default().event("reflecting").data(data)),
        AgentEventKind::Replanning => Some(Event::default().event("replanning").data(data)),
        AgentEventKind::ContextCompressed => {
            Some(Event::default().event("context_compressed").data(data))
        }
        AgentEventKind::FileEdit => Some(Event::default().event("file_edit").data(data)),
        AgentEventKind::ApprovalRequired => {
            let payload = inject_session_id(data, session_id);
            Some(Event::default().event("approval_required").data(payload))
        }
        AgentEventKind::TerminalExec => {
            let payload = inject_session_id(data, session_id);
            Some(Event::default().event("terminal_exec").data(payload))
        }
        AgentEventKind::AskRequired => {
            let payload = inject_session_id(data, session_id);
            Some(Event::default().event("ask_required").data(payload))
        }
        AgentEventKind::StepLimitReached => {
            Some(Event::default().event("step_limit").data(data))
        }
        AgentEventKind::DiscardDraft => {
            Some(Event::default().event("discard_draft").data(""))
        }
    }
}

/// Adds the session id to an approval payload so the client can target this gate.
fn inject_session_id(raw: &str, session_id: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
            v.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

#[allow(clippy::unused_async)]
pub async fn approve_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ApprovalResponse>,
) -> Result<Json<serde_json::Value>, AppError> {
    let gate = if let Some(sid) = &req.session_id {
        state.get_approval_gate(sid)
    } else {
        state.any_approval_gate()
    };

    if let Some(gate) = gate {
        let sent = gate.respond(&req.request_id, req.approved).await;
        Ok(Json(serde_json::json!({ "acknowledged": sent })))
    } else {
        Err(AppError::InvalidRequest(
            "No active agent session with approval".into(),
        ))
    }
}

#[derive(Debug, Deserialize)]
pub struct TerminalResultRequest {
    pub request_id: String,
    /// Session UUID returned in the terminal_exec SSE event.
    pub session_id: Option<String>,
    /// JSON-encoded result, e.g. {"stdout":..,"stderr":..,"exit_code":..}.
    pub result: String,
}

#[allow(clippy::unused_async)]
pub async fn terminal_result_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TerminalResultRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let gate = if let Some(sid) = &req.session_id {
        state.get_terminal_gate(sid)
    } else {
        state.any_terminal_gate()
    };

    if let Some(gate) = gate {
        let sent = gate.respond(&req.request_id, req.result).await;
        Ok(Json(serde_json::json!({ "acknowledged": sent })))
    } else {
        Err(AppError::InvalidRequest(
            "No active agent session awaiting terminal result".into(),
        ))
    }
}

#[derive(Debug, Deserialize)]
pub struct AskResultRequest {
    pub request_id: String,
    /// Session UUID returned in the ask_required SSE event.
    pub session_id: Option<String>,
    /// The user's selected/typed answer.
    pub answer: String,
}

#[allow(clippy::unused_async)]
pub async fn ask_result_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AskResultRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let gate = if let Some(sid) = &req.session_id {
        state.get_ask_gate(sid)
    } else {
        state.any_ask_gate()
    };

    if let Some(gate) = gate {
        let sent = gate.respond(&req.request_id, req.answer).await;
        Ok(Json(serde_json::json!({ "acknowledged": sent })))
    } else {
        Err(AppError::InvalidRequest(
            "No active agent session awaiting an answer".into(),
        ))
    }
}
