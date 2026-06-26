use async_trait::async_trait;
use futures::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::time::Duration;

use crate::error::AppError;
use crate::models::{
    ChatRequest, ChatResponse, Message, ModelInfo, ProviderHealth, ToolCall, ToolChatRequest,
    ToolChatResponse, ToolMessage, ToolStreamDelta, Usage,
};
use crate::providers::Provider;

fn compat_http_error(provider_id: &str, status: reqwest::StatusCode, body: &str) -> AppError {
    let detail = if body.is_empty() {
        format!("HTTP {status}")
    } else {
        let trimmed: String = body.chars().take(400).collect();
        format!("HTTP {status}: {trimmed}")
    };
    AppError::ProviderError(format!("{provider_id}: {detail}"))
}

async fn read_http_error(provider_id: &str, resp: reqwest::Response) -> AppError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    compat_http_error(provider_id, status, &body)
}

pub struct OpenAiCompatProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    max_retries: u32,
    provider_id: &'static str,
    provider_display_name: &'static str,
    tool_calling: bool,
}

impl OpenAiCompatProvider {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        id: String,
        display_name: String,
        base_url: String,
        api_key: Option<String>,
        default_model: String,
        timeout_secs: u64,
        max_retries: u32,
        tool_calling: bool,
    ) -> Self {
        let provider_id: &'static str = Box::leak(id.into_boxed_str());
        let provider_display_name: &'static str = Box::leak(display_name.into_boxed_str());

        let base_url = base_url.trim_end_matches('/').to_string();

        Self {
            // Long generations (high reasoning, big outputs) must not be killed mid-stream,
            // so we avoid a short total timeout. Instead: fail fast on a dead connection
            // (connect), fail on an idle/hung stream (read_timeout resets on every token),
            // and keep only a very generous absolute backstop.
            client: Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .read_timeout(Duration::from_secs(timeout_secs))
                .timeout(Duration::from_secs(1800))
                .build()
                .unwrap_or_default(),
            base_url,
            api_key,
            default_model,
            max_retries,
            provider_id,
            provider_display_name,
            tool_calling,
        }
    }

    fn add_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                return req.bearer_auth(key);
            }
        }
        req
    }

    /// Headroom compression is only supported on the GetAIBD platform API.
    fn compat_compress(&self, request: &ToolChatRequest) -> Option<bool> {
        if request.compress && self.provider_id == "getaibd" {
            Some(true)
        } else {
            None
        }
    }

    /// OpenRouter sticky-routing key (max 256 chars). GetAIBD platform only.
    fn compat_cache_session_id(cache_session_id: &Option<String>) -> Option<String> {
        let sid = cache_session_id.as_ref()?.trim();
        if sid.is_empty() {
            None
        } else {
            Some(sid.chars().take(256).collect())
        }
    }
}

#[derive(Serialize)]
struct CompatRequest {
    model: String,
    messages: Vec<CompatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Serialize)]
struct CompatToolRequest {
    model: String,
    messages: Vec<CompatMessage>,
    tools: Vec<CompatToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Serialize)]
struct CompatToolStreamRequest {
    model: String,
    messages: Vec<CompatMessage>,
    tools: Vec<CompatToolDef>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Serialize)]
struct CompatToolDef {
    r#type: String,
    function: CompatFunctionDef,
}

#[derive(Serialize)]
struct CompatFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct CompatToolCallResponse {
    #[serde(default)]
    id: String,
    #[serde(default = "default_tool_type")]
    r#type: String,
    function: CompatToolCallFunction,
    // Provider passthrough (Gemini's `thought_signature` lives here). Parsed from
    // the response AND serialized back on the next request so Google accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra_content: Option<serde_json::Value>,
}

fn default_tool_type() -> String {
    "function".to_string()
}

/// Keep only the canonical reasoning-effort levels the platform understands.
fn norm_effort(effort: &Option<String>) -> Option<String> {
    let v = effort.as_deref()?.trim().to_ascii_lowercase();
    matches!(v.as_str(), "low" | "medium" | "high").then_some(v)
}

/// Normalize tool-call args for replay. Alibaba/Qwen reject empty, `null`, or non-object JSON.
pub fn parse_tool_arguments(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str(trimmed) {
        Ok(serde_json::Value::Object(_)) => serde_json::from_str(trimmed).unwrap_or_default(),
        Ok(_) => serde_json::json!({}),
        Err(_) => serde_json::json!({}),
    }
}

pub fn compat_tool_arguments(arguments: &serde_json::Value) -> String {
    let normalized = if arguments.is_null() {
        serde_json::json!({})
    } else if matches!(arguments, serde_json::Value::String(s) if s.trim().is_empty()) {
        serde_json::json!({})
    } else if !arguments.is_object() {
        serde_json::json!({})
    } else {
        arguments.clone()
    };
    serde_json::to_string(&normalized).unwrap_or_else(|_| "{}".to_string())
}

fn compat_tool_choice(
    tools_empty: bool,
    request: &ToolChatRequest,
) -> Option<String> {
    if tools_empty {
        return None;
    }
    // Never omit tool_choice when tools are present — some upstream routers
    // (GetAIBD → Alibaba/Qwen thinking mode) default missing values to
    // `required`, which those models reject. Explicit `auto` is safe everywhere.
    if norm_effort(&request.reasoning_effort).is_some() {
        return Some("auto".to_string());
    }
    request
        .tool_choice
        .clone()
        .or_else(|| Some("auto".to_string()))
}

#[derive(Serialize, Deserialize)]
struct CompatToolCallFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Serialize, Deserialize)]
struct CompatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<CompatToolCallResponse>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct CompatResponse {
    choices: Vec<CompatChoice>,
    usage: Option<CompatUsage>,
}

#[derive(Deserialize)]
struct CompatChoice {
    message: Option<CompatMessage>,
    delta: Option<CompatDelta>,
}

#[derive(Deserialize)]
struct CompatDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<CompatStreamToolCall>>,
}

#[derive(Deserialize)]
struct CompatStreamToolCall {
    // Some OpenAI-compatible providers (notably Google's Gemini shim and a few
    // Anthropic proxies) omit `index` when they emit a whole tool call in one
    // delta. Treat it as optional and fall back to the array position so the
    // tool call is never silently dropped on a strict-decode failure.
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<CompatStreamFunction>,
    // Gemini streams its `thought_signature` here, in the same delta as the
    // function name. Capture it so it can be echoed back on the next request.
    #[serde(default)]
    extra_content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CompatStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct CompatUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct CompatStreamChunk {
    // Default so a chunk that carries only `usage` (or any provider-specific
    // metadata) parses cleanly instead of dropping the whole line.
    #[serde(default)]
    choices: Vec<CompatChoice>,
}

impl From<&Message> for CompatMessage {
    fn from(m: &Message) -> Self {
        Self {
            role: m.role.clone(),
            content: Some(m.content.clone()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

impl From<&ToolMessage> for CompatMessage {
    fn from(m: &ToolMessage) -> Self {
        Self {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.as_ref().map(|v| {
                v.iter()
                    .map(|tc| CompatToolCallResponse {
                        id: tc.id.clone(),
                        r#type: "function".to_string(),
                        function: CompatToolCallFunction {
                            name: tc.name.clone(),
                            arguments: compat_tool_arguments(&tc.arguments),
                        },
                        extra_content: tc.extra.clone(),
                    })
                    .collect()
            }),
            tool_call_id: m.tool_call_id.clone(),
        }
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn id(&self) -> &'static str {
        self.provider_id
    }

    fn display_name(&self) -> &'static str {
        self.provider_display_name
    }

    fn max_retries(&self) -> u32 {
        self.max_retries
    }

    fn supports_tool_calling(&self) -> bool {
        self.tool_calling
    }

    async fn health_check(&self) -> ProviderHealth {
        let req = self.client.get(format!("{}/models", self.base_url));
        let healthy = self
            .add_auth(req)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        ProviderHealth {
            provider: self.provider_id.to_string(),
            healthy,
            message: if healthy {
                None
            } else {
                Some(format!(
                    "Cannot reach {} at {}",
                    self.provider_display_name, self.base_url
                ))
            },
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelEntry>,
        }
        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
            /// Friendly catalog display name (e.g. the family name); falls back to id.
            #[serde(default)]
            name: Option<String>,
            /// True for the platform's free / "Auto" model.
            #[serde(default)]
            free: bool,
            #[serde(default)]
            capabilities: Vec<String>,
            /// Real context window (tokens) from the catalog; 0/absent when unknown.
            #[serde(default)]
            context_window: usize,
        }

        let req = self.client.get(format!("{}/models", self.base_url));
        let resp: ModelsResponse = self
            .add_auth(req)
            .send()
            .await
            .map_err(|e| self.map_error(&e))?
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("{}: {e}", self.provider_id)))?;

        let mut models: Vec<ModelInfo> = resp
            .data
            .into_iter()
            .map(|m| {
                // Cache the catalog's real context window so the agent loop can use
                // it instead of guessing from the model name.
                crate::context::record_model_window(&m.id, m.context_window);
                let name = m.name.filter(|s| !s.is_empty()).unwrap_or_else(|| m.id.clone());
                ModelInfo {
                    id: m.id,
                    name,
                    capabilities: m.capabilities,
                    free: m.free,
                }
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AppError> {
        let model = if request.model.is_empty() {
            &self.default_model
        } else {
            &request.model
        };

        let body = CompatRequest {
            model: model.to_string(),
            messages: request.messages.iter().map(CompatMessage::from).collect(),
            stream: false,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            reasoning_effort: norm_effort(&request.reasoning_effort),
            session_id: if self.provider_id == "getaibd" {
                Self::compat_cache_session_id(&request.cache_session_id)
            } else {
                None
            },
        };

        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        let resp = self
            .add_auth(req)
            .send()
            .await
            .map_err(|e| self.map_error(&e))?;
        if !resp.status().is_success() {
            return Err(read_http_error(self.provider_id, resp).await);
        }
        let resp: CompatResponse = resp
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("{}: {e}", self.provider_id)))?;

        let content = resp
            .choices
            .first()
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        Ok(ChatResponse {
            provider: self.provider_id.to_string(),
            model: model.to_string(),
            content,
            usage: resp.usage.map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
        })
    }

    fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let default_model = self.default_model.clone();
        let pid = self.provider_id;

        Box::pin(async_stream::try_stream! {
            let model = if request.model.is_empty() {
                &default_model
            } else {
                &request.model
            };

            let body = CompatRequest {
                model: model.to_string(),
                messages: request.messages.iter().map(CompatMessage::from).collect(),
                stream: true,
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                reasoning_effort: norm_effort(&request.reasoning_effort),
                session_id: if pid == "getaibd" {
                    Self::compat_cache_session_id(&request.cache_session_id)
                } else {
                    None
                },
            };

            let mut req = client
                .post(format!("{base_url}/chat/completions"))
                .json(&body);
            if let Some(ref key) = api_key {
                if !key.is_empty() {
                    req = req.bearer_auth(key);
                }
            }

            let resp = req.send().await.map_err(|e| {
                if e.is_timeout() {
                    AppError::ProviderTimeout(pid.to_string())
                } else {
                    AppError::ProviderUnavailable(format!("{pid}: {e}"))
                }
            })?;

            if resp.status().is_success() {
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            let mut buffer = String::new();

            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| AppError::ProviderError(format!("{pid} stream: {e}")))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=pos).collect();
                    let line = line.trim();

                    if line.is_empty() || !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];
                    if data == "[DONE]" {
                        return;
                    }

                    if let Ok(chunk) = serde_json::from_str::<CompatStreamChunk>(data) {
                        if let Some(choice) = chunk.choices.first() {
                            if let Some(delta) = &choice.delta {
                                if let Some(content) = &delta.content {
                                    if !content.is_empty() {
                                        yield content.clone();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            } else {
                Err(read_http_error(pid, resp).await)?;
            }
        })
    }

    async fn chat_with_tools(
        &self,
        request: &ToolChatRequest,
    ) -> Result<ToolChatResponse, AppError> {
        if !self.tool_calling {
            return Err(AppError::ProviderError(format!(
                "{}: tool calling not enabled for this provider",
                self.provider_id
            )));
        }

        let model = if request.model.is_empty() {
            &self.default_model
        } else {
            &request.model
        };

        let tools: Vec<CompatToolDef> = request
            .tools
            .iter()
            .map(|t| CompatToolDef {
                r#type: "function".to_string(),
                function: CompatFunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();

        let tool_choice = compat_tool_choice(tools.is_empty(), request);
        let body = CompatToolRequest {
            model: model.to_string(),
            messages: request.messages.iter().map(CompatMessage::from).collect(),
            tools,
            tool_choice,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            reasoning_effort: norm_effort(&request.reasoning_effort),
            compress: self.compat_compress(request),
            session_id: if self.provider_id == "getaibd" {
                Self::compat_cache_session_id(&request.cache_session_id)
            } else {
                None
            },
        };

        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        let resp = self
            .add_auth(req)
            .send()
            .await
            .map_err(|e| self.map_error(&e))?;
        if !resp.status().is_success() {
            return Err(read_http_error(self.provider_id, resp).await);
        }
        let resp: CompatResponse = resp
            .json()
            .await
            .map_err(|e| AppError::ProviderError(format!("{}: {e}", self.provider_id)))?;

        let choice = resp.choices.first();
        let content = choice
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.content.clone());
        let tool_calls: Vec<ToolCall> = choice
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.tool_calls.as_ref())
            .map(|v| {
                v.iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: parse_tool_arguments(&tc.function.arguments),
                        extra: tc.extra_content.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ToolChatResponse {
            content,
            tool_calls,
            usage: resp.usage.map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
        })
    }

    fn supports_streaming_tools(&self) -> bool {
        self.tool_calling
    }

    #[allow(clippy::too_many_lines)]
    fn chat_with_tools_stream(
        &self,
        request: ToolChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ToolStreamDelta, AppError>> + Send>> {
        if !self.tool_calling {
            return Box::pin(futures::stream::once(async { Ok(ToolStreamDelta::Done) }));
        }

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let default_model = self.default_model.clone();
        let pid = self.provider_id;
        let compress = self.compat_compress(&request);

        Box::pin(async_stream::try_stream! {
            let model = if request.model.is_empty() {
                &default_model
            } else {
                &request.model
            };

            let tools: Vec<CompatToolDef> = request
                .tools
                .iter()
                .map(|t| CompatToolDef {
                    r#type: "function".to_string(),
                    function: CompatFunctionDef {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input_schema.clone(),
                    },
                })
                .collect();

            let tool_choice = compat_tool_choice(tools.is_empty(), &request);
            let body = CompatToolStreamRequest {
                model: model.to_string(),
                messages: request.messages.iter().map(CompatMessage::from).collect(),
                tools,
                stream: true,
                tool_choice,
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                reasoning_effort: norm_effort(&request.reasoning_effort),
                compress,
                session_id: if pid == "getaibd" {
                    Self::compat_cache_session_id(&request.cache_session_id)
                } else {
                    None
                },
            };

            let mut req = client
                .post(format!("{base_url}/chat/completions"))
                .json(&body);
            if let Some(ref key) = api_key {
                if !key.is_empty() {
                    req = req.bearer_auth(key);
                }
            }

            let resp = req.send().await.map_err(|e| {
                if e.is_timeout() {
                    AppError::ProviderTimeout(pid.to_string())
                } else {
                    AppError::ProviderUnavailable(format!("{pid}: {e}"))
                }
            })?;

            if resp.status().is_success() {
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            let mut buffer = String::new();
            let mut active_tool_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();

            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| AppError::ProviderError(format!("{pid} stream: {e}")))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=pos).collect();
                    let line = line.trim();

                    if line.is_empty() || !line.starts_with("data: ") {
                        continue;
                    }
                    let data = &line[6..];
                    if data == "[DONE]" {
                        for _ in &active_tool_indices {
                            yield ToolStreamDelta::ToolCallEnd;
                        }
                        yield ToolStreamDelta::Done;
                        return;
                    }

                    // Surface mid-stream provider errors (e.g. upstream 429/credit
                    // exhaustion) instead of silently ending with an empty turn — an
                    // empty turn looks to the agent like "the model said nothing" and
                    // makes it quit as if the task were done.
                    if data.contains("\"error\"") {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                            if v.get("choices").is_none() {
                                if let Some(err) = v.get("error") {
                                    let msg = err
                                        .get("message")
                                        .and_then(serde_json::Value::as_str)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| err.to_string());
                                    Err(AppError::ProviderError(format!("{pid}: {msg}")))?;
                                }
                            }
                        }
                    }

                    if let Ok(chunk) = serde_json::from_str::<CompatStreamChunk>(data) {
                        if let Some(choice) = chunk.choices.first() {
                            if let Some(delta) = &choice.delta {
                                if let Some(content) = &delta.content {
                                    if !content.is_empty() {
                                        yield ToolStreamDelta::Token(content.clone());
                                    }
                                }
                                if let Some(tool_calls) = &delta.tool_calls {
                                    for (pos, tc) in tool_calls.iter().enumerate() {
                                        let index = tc.index.unwrap_or(pos);
                                        if let Some(ref func) = tc.function {
                                            if let Some(ref name) = func.name {
                                                if !name.is_empty() {
                                                    let id = tc.id.clone().unwrap_or_else(|| format!("call_{index}"));
                                                    active_tool_indices.insert(index);
                                                    yield ToolStreamDelta::ToolCallStart {
                                                        id,
                                                        name: name.clone(),
                                                        extra: tc.extra_content.clone(),
                                                    };
                                                }
                                            }
                                            if let Some(ref args) = func.arguments {
                                                if !args.is_empty() {
                                                    yield ToolStreamDelta::ToolCallArgDelta(args.clone());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            yield ToolStreamDelta::Done;
            } else {
                Err(read_http_error(pid, resp).await)?;
            }
        })
    }
}

impl OpenAiCompatProvider {
    fn map_error(&self, e: &reqwest::Error) -> AppError {
        if e.is_timeout() {
            AppError::ProviderTimeout(self.provider_id.to_string())
        } else {
            AppError::ProviderUnavailable(format!("{}: {e}", self.provider_id))
        }
    }
}

#[cfg(test)]
mod compat_tests {
    use super::*;
    use crate::models::{ToolChatRequest, ToolMessage};

    fn tool_request(reasoning: Option<&str>) -> ToolChatRequest {
        ToolChatRequest {
            model: "qwen-flash".into(),
            messages: vec![ToolMessage::user("hi")],
            tools: vec![crate::models::ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: serde_json::json!({}),
            }],
            temperature: None,
            max_tokens: None,
            reasoning_effort: reasoning.map(str::to_string),
            tool_choice: None,
            compress: false,
            cache_session_id: None,
        }
    }

    #[test]
    fn getaibd_forwards_cache_session_id_on_tool_request() {
        assert_eq!(
            OpenAiCompatProvider::compat_cache_session_id(&Some("ws-chat-abc".into())).as_deref(),
            Some("ws-chat-abc")
        );
        assert!(OpenAiCompatProvider::compat_cache_session_id(&None).is_none());
    }

    #[test]
    fn thinking_mode_forces_auto_tool_choice() {
        let tc = compat_tool_choice(false, &tool_request(Some("high")));
        assert_eq!(tc.as_deref(), Some("auto"));
    }

    #[test]
    fn default_tool_choice_is_auto_when_tools_present() {
        let tc = compat_tool_choice(false, &tool_request(None));
        assert_eq!(tc.as_deref(), Some("auto"));
    }

    #[test]
    fn no_tool_choice_when_tools_empty() {
        let mut req = tool_request(None);
        req.tools.clear();
        assert!(compat_tool_choice(true, &req).is_none());
    }

    #[test]
    fn compat_tool_arguments_rejects_null_and_empty() {
        assert_eq!(compat_tool_arguments(&serde_json::Value::Null), "{}");
        assert_eq!(
            compat_tool_arguments(&serde_json::json!("")),
            "{}"
        );
        assert_eq!(
            compat_tool_arguments(&serde_json::json!({"plan": "step 1"})),
            r#"{"plan":"step 1"}"#
        );
    }

    #[test]
    fn parse_tool_arguments_normalizes_invalid() {
        assert_eq!(parse_tool_arguments(""), serde_json::json!({}));
        assert_eq!(parse_tool_arguments("null"), serde_json::json!({}));
        assert_eq!(parse_tool_arguments("not json"), serde_json::json!({}));
        assert_eq!(
            parse_tool_arguments(r#"{"path":"a"}"#),
            serde_json::json!({"path": "a"})
        );
    }
}
