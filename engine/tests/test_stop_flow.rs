// Reproduces the agent "not stopping in time" / duplicate-summary behaviour by
// driving the real agent_loop with a scripted provider and recording the exact
// AgentEvent sequence the client would receive.
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use mcp_universal::agent::runtime::{
    run_agent_with_memory, AgentEvent, AgentEventKind, AgentOptions,
};
use mcp_universal::agent::session::Session;
use mcp_universal::error::AppError;
use mcp_universal::models::{
    ChatRequest, ChatResponse, ModelInfo, ProviderHealth, ToolCall, ToolChatRequest,
    ToolChatResponse,
};
use mcp_universal::providers::Provider;
use mcp_universal::tools::{Tool, ToolRegistry};
use serde_json::{json, Value};

/// A no-op "write_file" tool so tool_defs is non-empty (auto_complete requires it)
/// and the mutating-progress guard counts a real mutation.
struct NoopWrite;

#[async_trait]
impl Tool for NoopWrite {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "write a file"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}}})
    }
    async fn execute(&self, _input: Value) -> Result<Value, AppError> {
        Ok(json!({"ok": true}))
    }
}

/// Scripted provider: emits one mutating tool call, then a "done" summary, and a
/// completion reviewer that says NOT done on its first call and done afterwards.
struct ScriptedProvider {
    worker_calls: AtomicU32,
    reviewer_calls: AtomicU32,
}

fn is_reviewer(req: &ToolChatRequest) -> bool {
    req.tools.is_empty()
        && req
            .messages
            .iter()
            .any(|m| m.content.as_deref().unwrap_or("").contains("completion reviewer"))
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "getaibd"
    }
    fn display_name(&self) -> &'static str {
        "Scripted"
    }
    fn supports_tool_calling(&self) -> bool {
        true
    }
    async fn health_check(&self) -> ProviderHealth {
        ProviderHealth {
            provider: "getaibd".into(),
            healthy: true,
            message: None,
        }
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        Ok(vec![])
    }
    async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, AppError> {
        unreachable!("agent uses chat_with_tools")
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
        if is_reviewer(request) {
            let n = self.reviewer_calls.fetch_add(1, Ordering::SeqCst);
            // First review: say there is outstanding work. After that: done.
            let body = if n == 0 {
                json!({"done": false, "missing": ["wire the python backend into compose"]})
            } else {
                json!({"done": true, "missing": []})
            };
            return Ok(ToolChatResponse {
                content: Some(body.to_string()),
                tool_calls: vec![],
                usage: None,
            });
        }

        let n = self.worker_calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // First worker turn: actually do a mutation.
            Ok(ToolChatResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    arguments: json!({"path": "Dockerfile"}),
                    extra: None,
                }],
                usage: None,
            })
        } else {
            // Every later turn: a tool-less "done" summary that also offers next steps.
            Ok(ToolChatResponse {
                content: Some(
                    "Done. Created the Dockerfile.\n\nNext steps: I can also add a \
                     Makefile target or wire the Python backend."
                        .into(),
                ),
                tool_calls: vec![],
                usage: None,
            })
        }
    }
}

fn label(kind: &AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::Start => "Start",
        AgentEventKind::Think => "Think",
        AgentEventKind::ToolCall => "ToolCall",
        AgentEventKind::ToolResult => "ToolResult",
        AgentEventKind::Response => "Response",
        AgentEventKind::Complete => "Complete",
        AgentEventKind::Error => "Error",
        AgentEventKind::ModeSelected => "ModeSelected",
        AgentEventKind::Planning => "Planning",
        AgentEventKind::Thinking => "Thinking",
        AgentEventKind::Reflecting => "Reflecting",
        AgentEventKind::Replanning => "Replanning",
        AgentEventKind::ContextCompressed => "ContextCompressed",
        AgentEventKind::FileEdit => "FileEdit",
        AgentEventKind::ApprovalRequired => "ApprovalRequired",
        AgentEventKind::TerminalExec => "TerminalExec",
        AgentEventKind::AskRequired => "AskRequired",
        AgentEventKind::StepLimitReached => "StepLimitReached",
        AgentEventKind::DiscardDraft => "DiscardDraft",
    }
}

#[tokio::test]
async fn stop_flow_event_sequence() {
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        worker_calls: AtomicU32::new(0),
        reviewer_calls: AtomicU32::new(0),
    });

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NoopWrite));

    let tmp = std::env::temp_dir().join(format!("stopflow-{}", uuid_like()));
    let _ = std::fs::create_dir_all(&tmp);
    let mut session = Session::new("getaibd", "claude-opus-4-8", tmp).with_max_iterations(25);

    let opts = AgentOptions {
        enable_thinking: false,
        auto_complete: true,
        ..Default::default()
    };

    let events: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let mut on_event = move |e: AgentEvent| {
        ev.lock().unwrap().push((
            label(&e.kind).to_string(),
            e.content.unwrap_or_default(),
        ));
    };

    let result = run_agent_with_memory(
        &mut session,
        "make a Dockerfile",
        &provider,
        &registry,
        None,
        Some(&opts),
        &mut on_event,
    )
    .await
    .expect("agent run");

    let log = events.lock().unwrap();
    eprintln!("=== EVENT SEQUENCE ({} events) ===", log.len());
    for (i, (k, c)) in log.iter().enumerate() {
        let short: String = c.chars().take(70).collect();
        eprintln!("{i:>3} {k:<18} {short}");
    }
    eprintln!("=== iterations: {} ===", result.iterations);

    let kinds: Vec<&str> = log.iter().map(|(k, _)| k.as_str()).collect();
    let discard_at = kinds
        .iter()
        .position(|k| *k == "DiscardDraft")
        .expect("force-continue path should emit DiscardDraft");

    // FIX: DiscardDraft must NOT be immediately preceded by a status event (Reflecting/
    // Planning/Thinking/Replanning) — those detach the client's draft-bubble handle, which
    // would leave the superseded summary on screen as a duplicate.
    let prev = if discard_at == 0 { "" } else { kinds[discard_at - 1] };
    assert!(
        !matches!(prev, "Reflecting" | "Planning" | "Thinking" | "Replanning"),
        "DiscardDraft must not be preceded by a handle-detaching status event, got `{prev}`"
    );

    // Exactly one final summary should be delivered (no duplicate "done" message).
    let completes = kinds.iter().filter(|k| **k == "Complete").count();
    assert_eq!(completes, 1, "expected exactly one Complete/summary event");
}

fn uuid_like() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
