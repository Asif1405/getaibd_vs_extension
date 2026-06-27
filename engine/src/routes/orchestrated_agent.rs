use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::modes::AgentMode;
use crate::agent::orchestrator::Orchestrator;
use crate::agent::runtime::AgentEventKind;
use crate::agent::session::Session;
use crate::error::AppError;
use crate::state::AppState;
use crate::tools::ToolRegistry;

#[derive(Debug, Deserialize)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct OrchestratedRequest {
    pub provider: String,
    pub model: String,
    pub input: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub auto_mode: bool,
    #[serde(default)]
    pub use_memory: bool,
    #[serde(default)]
    pub history: Vec<HistoryMessage>,
    #[serde(default)]
    pub require_approval: bool,
    /// When true, run_command is delegated to the client's managed terminal.
    #[serde(default)]
    pub client_terminal: bool,
    /// Reasoning effort hint (low/medium/high) for thinking-capable models.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Server-side tool-output compression (GetAIBD platform only).
    #[serde(default)]
    pub compress: bool,
    /// Host OS reported by the client (e.g. "Windows", "macOS", "Linux").
    #[serde(default)]
    pub os: Option<String>,
    /// Active shell reported by the client (e.g. "PowerShell", "zsh", "bash").
    #[serde(default)]
    pub shell: Option<String>,
    /// Global user rules from VS Code settings.
    #[serde(default)]
    pub user_rules: Option<String>,
    /// Working directory for nested `.getaibd/AGENTS.md` (absolute or relative to project root).
    #[serde(default)]
    pub workspace_cwd: Option<String>,
    /// Stable per-workspace chat id for GetAIBD prompt-cache sticky routing.
    #[serde(default)]
    pub cache_session_id: Option<String>,
    /// Base64 `data:` image URLs attached to this turn (vision-capable models).
    #[serde(default)]
    pub images: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct OrchestratedResponse {
    pub mode: String,
    pub result: String,
    pub iterations: u32,
}

#[allow(clippy::unused_async)]
pub async fn orchestrated_agent_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<OrchestratedRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let provider = state
        .get_provider(&req.provider)
        .ok_or_else(|| AppError::UnknownProvider(req.provider.clone()))?;

    let registry = ToolRegistry::build_for_session(&state.project_root).await;

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);

    let session_id = uuid::Uuid::new_v4().to_string();
    let gate = if req.require_approval {
        let g = crate::tools::approval::ApprovalGate::new();
        state.set_approval_gate(&session_id, g.clone());
        Some(g)
    } else {
        None
    };
    let term_gate = if req.client_terminal {
        let g = crate::tools::terminal_gate::TerminalGate::new();
        state.set_terminal_gate(&session_id, g.clone());
        Some(g)
    } else {
        None
    };
    // The agent can always ask the user a clarifying question.
    let ask_gate = {
        let g = crate::tools::ask_gate::AskGate::new();
        state.set_ask_gate(&session_id, g.clone());
        g
    };
    let sid = session_id.clone();

    tokio::spawn(async move {
        let result = run_orchestrated_task(
            &state,
            req,
            provider,
            registry,
            gate,
            term_gate,
            ask_gate,
            sid.clone(),
            tx.clone(),
        )
        .await;
        state.clear_approval_gate(&sid);
        state.clear_terminal_gate(&sid);
        state.clear_ask_gate(&sid);

        match result {
            Ok(resp) => {
                let _ = tx
                    .send(Ok(Event::default()
                        .event("complete")
                        .data(serde_json::to_string(&resp).unwrap_or_default())))
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(Ok(Event::default()
                        .event("error")
                        .data(crate::routes::sse::sse_text_data(&e.to_string()))))
                    .await;
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

#[allow(clippy::too_many_arguments)]
async fn run_orchestrated_task(
    state: &AppState,
    mut req: OrchestratedRequest,
    provider: Arc<dyn crate::providers::Provider>,
    registry: ToolRegistry,
    gate: Option<crate::tools::approval::ApprovalGate>,
    term_gate: Option<crate::tools::terminal_gate::TerminalGate>,
    ask_gate: crate::tools::ask_gate::AskGate,
    session_id: String,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) -> Result<OrchestratedResponse, AppError> {
    let environment =
        crate::agent::runtime::format_environment(req.os.as_deref(), req.shell.as_deref());
    let workspace_cwd = req.workspace_cwd.as_ref().map(|p| {
        let path = std::path::PathBuf::from(p);
        if path.is_absolute() {
            path
        } else {
            state.project_root.join(path)
        }
    });
    let mut session = Session::new(&req.provider, &req.model, state.project_root.clone())
        .with_reasoning_effort(req.reasoning_effort.clone())
        .with_compress(req.compress)
        .with_environment(environment)
        .with_user_rules(req.user_rules.clone())
        .with_workspace_cwd(workspace_cwd)
        .with_cache_session_id(req.cache_session_id.clone())
        .with_user_images(std::mem::take(&mut req.images));

    for h in &req.history {
        let msg = match h.role.as_str() {
            "assistant" => crate::models::ToolMessage::assistant(h.content.clone()),
            "system" => crate::models::ToolMessage::system(h.content.clone()),
            _ => crate::models::ToolMessage::user(h.content.clone()),
        };
        session.push_message(msg);
    }

    let mut orchestrator = Orchestrator::new(provider, registry)
        .with_auto_mode(req.auto_mode)
        .with_ask_gate(ask_gate);

    if let Some(g) = gate {
        orchestrator = orchestrator.with_approval_gate(g);
    }

    if let Some(g) = term_gate {
        orchestrator = orchestrator.with_terminal_gate(g);
    }

    if req.use_memory {
        if let (Some(store), Some(embedder)) = (&state.memory_store, &state.embedder) {
            orchestrator = orchestrator.with_memory(store.clone().into(), embedder.clone_box());
        }
    }

    let mode = req.mode.as_deref().map(AgentMode::from_str);

    let mut on_event = |event: crate::agent::runtime::AgentEvent| {
        let event_name = match event.kind {
            AgentEventKind::Start => "start",
            AgentEventKind::Think => "think",
            AgentEventKind::ToolCall => "tool_call",
            AgentEventKind::ToolResult => "tool_result",
            AgentEventKind::Response => "response",
            AgentEventKind::Complete => "done",
            AgentEventKind::Error => "error",
            AgentEventKind::ModeSelected => "mode_selected",
            AgentEventKind::Planning => "planning",
            AgentEventKind::Thinking => "thinking",
            AgentEventKind::Reflecting => "reflecting",
            AgentEventKind::Replanning => "replanning",
            AgentEventKind::ContextCompressed => "context_compressed",
            AgentEventKind::FileEdit => "file_edit",
            AgentEventKind::ApprovalRequired => "approval_required",
            AgentEventKind::TerminalExec => "terminal_exec",
            AgentEventKind::AskRequired => "ask_required",
            AgentEventKind::StepLimitReached => "step_limit",
            AgentEventKind::DiscardDraft => "discard_draft",
        };

        let data = match event.kind {
            AgentEventKind::ApprovalRequired
            | AgentEventKind::TerminalExec
            | AgentEventKind::AskRequired => inject_session_id(&event.content, &session_id),
            AgentEventKind::ToolCall | AgentEventKind::ToolResult | AgentEventKind::FileEdit => {
                event.content.unwrap_or_default()
            }
            AgentEventKind::Complete
            | AgentEventKind::Error
            | AgentEventKind::Response
            | AgentEventKind::Planning
            | AgentEventKind::Thinking
            | AgentEventKind::Reflecting
            | AgentEventKind::Replanning
            | AgentEventKind::ContextCompressed
            | AgentEventKind::StepLimitReached => {
                crate::routes::sse::sse_text_data(&event.content.unwrap_or_default())
            }
            _ => event.content.unwrap_or_default(),
        };

        let _ = tx.try_send(Ok(Event::default().event(event_name).data(data)));
    };

    let result = orchestrator
        .execute(&mut session, &req.input, mode, &mut on_event)
        .await?;

    Ok(OrchestratedResponse {
        mode: result.mode.unwrap_or_else(|| "ask".to_string()),
        result: result.final_response,
        iterations: result.iterations,
    })
}

/// Adds the session id to an approval payload so the client can target this gate.
fn inject_session_id(content: &Option<String>, session_id: &str) -> String {
    let raw = content.clone().unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
            v.to_string()
        }
        Err(_) => raw,
    }
}
