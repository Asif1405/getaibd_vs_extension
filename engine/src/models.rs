use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    pub provider: String,
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Reasoning effort hint (low/medium/high) for thinking-capable models.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Optional BYOK: user can pass their own API key per request.
    /// If omitted, platform key or account-stored key is used.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Stable per-workspace chat id for GetAIBD prompt-cache sticky routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    pub provider: String,
    pub model: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// Provider-reported capabilities (e.g. "tools", "vision", "thinking").
    /// Empty when the upstream `/models` endpoint does not report them.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// True when this is the platform's free / "Auto" model. Lets the UI surface
    /// and search it by its friendly label without hardcoding its engine id.
    #[serde(default)]
    pub free: bool,
    /// True when the caller's plan cannot use this model yet (shown locked in the
    /// picker). Free-tier callers see paid models flagged this way.
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderHealth {
    pub provider: String,
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub providers: Vec<ProviderHealth>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SseTokenEvent {
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SseDoneEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SseErrorEvent {
    pub message: String,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Opaque provider passthrough attached to the tool call (the OpenAI
    /// `extra_content` field). Gemini 3.x returns a `thought_signature` here
    /// that Google REQUIRES to be echoed back verbatim on the next request —
    /// omitting it makes the follow-up call fail with HTTP 400, stalling the
    /// agent after a single tool call. Preserved here so we can send it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Base64 `data:` image URLs attached to a user turn. Kept separate from the
    /// text `content` so the provider adapter can emit OpenAI multimodal content
    /// (a `[{type:text},{type:image_url}]` array) only when images are present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl ToolMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            images: Vec::new(),
        }
    }

    /// A user turn carrying attached images (base64 `data:` URLs) alongside text.
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            images,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            images: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            images: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            name: None,
            images: Vec::new(),
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            name: None,
            images: Vec::new(),
        }
    }
}

impl From<&Message> for ToolMessage {
    fn from(m: &Message) -> Self {
        Self {
            role: m.role.clone(),
            content: Some(m.content.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChatRequest {
    pub model: String,
    pub messages: Vec<ToolMessage>,
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// OpenAI-compatible tool_choice ("auto" | "required" | "none"). `None` lets
    /// the provider default (auto). Used to FORCE a tool call when a weak model
    /// keeps narrating instead of acting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    /// When true, ask GetAIBD to compress tool outputs server-side (Headroom).
    #[serde(default)]
    pub compress: bool,
    /// Stable per-workspace chat id for GetAIBD prompt-cache sticky routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub enum ToolStreamDelta {
    Token(String),
    /// Chain-of-thought reasoning streamed in a dedicated field (e.g. DeepSeek-R1 /
    /// Kimi `reasoning_content`). Routed to the thinking channel, never the answer.
    Reasoning(String),
    ToolCallStart {
        id: String,
        name: String,
        extra: Option<serde_json::Value>,
    },
    ToolCallArgDelta(String),
    ToolCallEnd,
    Done,
}
