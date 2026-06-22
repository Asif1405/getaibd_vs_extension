use mcp_universal::models::{
    ChatRequest, ChatResponse, HealthResponse, Message, ModelInfo, ProviderHealth, ProviderInfo,
    SseDoneEvent, SseErrorEvent, SseTokenEvent, Usage,
};

#[test]
fn message_serializes_and_deserializes() {
    let msg = Message {
        role: "user".into(),
        content: "Hello".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let parsed: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.role, "user");
    assert_eq!(parsed.content, "Hello");
}

#[test]
fn chat_request_deserializes_minimal() {
    let json = r#"{
        "provider": "ollama",
        "model": "llama3",
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.provider, "ollama");
    assert_eq!(req.model, "llama3");
    assert_eq!(req.messages.len(), 1);
    assert!(req.temperature.is_none());
    assert!(req.max_tokens.is_none());
}

#[test]
fn chat_request_deserializes_with_optional_fields() {
    let json = r#"{
        "provider": "openai",
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.7,
        "max_tokens": 1024
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.temperature, Some(0.7));
    assert_eq!(req.max_tokens, Some(1024));
}

#[test]
fn chat_response_serializes_without_usage() {
    let resp = ChatResponse {
        provider: "ollama".into(),
        model: "llama3".into(),
        content: "Hello!".into(),
        usage: None,
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(!json.contains("usage"));
    assert!(json.contains("Hello!"));
}

#[test]
fn chat_response_serializes_with_usage() {
    let resp = ChatResponse {
        provider: "openai".into(),
        model: "gpt-4o".into(),
        content: "Response".into(),
        usage: Some(Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: Some(30),
        }),
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("prompt_tokens"));
    assert!(json.contains("30"));
}

#[test]
fn usage_round_trips() {
    let usage = Usage {
        prompt_tokens: Some(5),
        completion_tokens: Some(15),
        total_tokens: Some(20),
    };
    let json = serde_json::to_string(&usage).unwrap();
    let parsed: Usage = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.prompt_tokens, Some(5));
    assert_eq!(parsed.completion_tokens, Some(15));
    assert_eq!(parsed.total_tokens, Some(20));
}

#[test]
fn model_info_serializes() {
    let info = ModelInfo {
        id: "llama3".into(),
        name: "Llama 3".into(),
        capabilities: vec![],
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("llama3"));
    assert!(json.contains("Llama 3"));
}

#[test]
fn provider_info_serializes() {
    let info = ProviderInfo {
        id: "openai".into(),
        name: "OpenAI".into(),
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("openai"));
    assert!(json.contains("OpenAI"));
}

#[test]
fn provider_health_serializes_healthy() {
    let health = ProviderHealth {
        provider: "ollama".into(),
        healthy: true,
        message: None,
    };
    let json = serde_json::to_string(&health).unwrap();
    assert!(json.contains("\"healthy\":true"));
    assert!(!json.contains("message"));
}

#[test]
fn provider_health_serializes_unhealthy() {
    let health = ProviderHealth {
        provider: "ollama".into(),
        healthy: false,
        message: Some("Cannot reach server".into()),
    };
    let json = serde_json::to_string(&health).unwrap();
    assert!(json.contains("\"healthy\":false"));
    assert!(json.contains("Cannot reach server"));
}

#[test]
fn health_response_serializes() {
    let resp = HealthResponse {
        status: "healthy".into(),
        providers: vec![ProviderHealth {
            provider: "ollama".into(),
            healthy: true,
            message: None,
        }],
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"status\":\"healthy\""));
    assert!(json.contains("\"providers\""));
}

#[test]
fn sse_token_event_serializes() {
    let event = SseTokenEvent {
        content: "Hello".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(json, r#"{"content":"Hello"}"#);
}

#[test]
fn sse_done_event_serializes_without_usage() {
    let event = SseDoneEvent { usage: None };
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(json, "{}");
}

#[test]
fn sse_done_event_serializes_with_usage() {
    let event = SseDoneEvent {
        usage: Some(Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: Some(30),
        }),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("prompt_tokens"));
}

#[test]
fn sse_error_event_serializes() {
    let event = SseErrorEvent {
        message: "Provider timed out".into(),
        code: "PROVIDER_TIMEOUT".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("PROVIDER_TIMEOUT"));
    assert!(json.contains("Provider timed out"));
}
