use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

use crate::models::{Message, ToolDefinition, ToolMessage};

/// Process-wide cache of model id -> real context window (tokens), populated from
/// the gateway `/models` catalog when the model list is fetched. Lets the agent
/// use each model's TRUE window instead of guessing from a name table.
fn window_cache() -> &'static Mutex<HashMap<String, usize>> {
    static CACHE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a model's catalog-reported context window. Ignores 0 (unknown).
pub fn record_model_window(model: &str, window: usize) {
    if window == 0 || model.is_empty() {
        return;
    }
    if let Ok(mut cache) = window_cache().lock() {
        cache.insert(model.to_string(), window);
    }
}

/// Peek the cached catalog window for a model without falling back to the name
/// table. `None` means the catalog hasn't been fetched (or didn't report one).
pub fn cached_window(model: &str) -> Option<usize> {
    window_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(model).copied())
        .filter(|&w| w > 0)
}

/// The context window to use for a model: the catalog value reported by the
/// gateway when known, otherwise a best-effort estimate from the model name.
pub fn context_window_for(model: &str) -> usize {
    if let Ok(cache) = window_cache().lock() {
        if let Some(&w) = cache.get(model) {
            if w > 0 {
                return w;
            }
        }
    }
    model_context_limit(model)
}

/// Approximate token count from text. Most LLM tokenizers average ~4 chars per token.
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

fn message_tokens(msg: &Message) -> usize {
    estimate_tokens(&msg.content) + 4 // role overhead
}

/// Approximate context window (in tokens) for a model, used to decide when to
/// summarize the conversation. Under-estimating is harmful: it makes the agent
/// summarize (or, previously, drop) its own context too early, so it can lose its
/// place and fail unpredictably. Every model the platform serves has at least a
/// 128k window, so unknown models default to 128k rather than a tiny 8k.
pub fn model_context_limit(model: &str) -> usize {
    let m = model.to_lowercase();
    // Order matters: match the most specific identifiers first.
    if m.contains("gpt-3.5") {
        16_385
    } else if m.contains("gpt-4o")
        || m.contains("gpt-4-turbo")
        || m.contains("gpt-4.1")
        || m.contains("gpt-5")
        || m.contains("o1")
        || m.contains("o3")
        || m.contains("o4-mini")
    {
        128_000
    } else if m.contains("gpt-4") {
        8_192
    } else if m.contains("claude") {
        // Claude 3.x and 4.x all expose at least a 200k window.
        200_000
    } else if m.contains("gemini") {
        // Gemini 1.5/2/2.5/3.x are all >=1M.
        1_000_000
    } else if m.contains("grok") {
        131_072
    } else if m.contains("llama-4") {
        // Llama 4 Scout/Maverick expose very large windows (>=320k).
        320_000
    } else if m.contains("deepseek")
        || m.contains("kimi")
        || m.contains("moonshot")
        || m.contains("qwen")
        || m.contains("glm")
        || m.contains("mistral")
        || m.contains("mixtral")
        || m.contains("magistral")
        || m.contains("llama")
    {
        128_000
    } else {
        128_000
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrimStrategy {
    /// Drop oldest non-system messages first.
    DropOldest,
    /// Summarize dropped messages into a single system note (placeholder for future).
    Summarize,
}

impl Default for TrimStrategy {
    fn default() -> Self {
        Self::DropOldest
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub strategy: TrimStrategy,
    #[serde(default = "default_reserve_ratio")]
    pub reserve_for_completion: f32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: None,
            // Summarize rather than silently dropping old turns: a coding agent's
            // earlier tool results (files it read, edits it made) are load-bearing
            // context, and hard-dropping them mid-task makes it lose its place and
            // fail unpredictably. Trimming should only ever kick in near the real
            // window, and even then it must preserve a trace of what happened.
            strategy: TrimStrategy::Summarize,
            reserve_for_completion: default_reserve_ratio(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_reserve_ratio() -> f32 {
    0.25
}

/// Trims messages to fit within the context window for the given model.
///
/// Preserves:
/// - All system messages (moved to front)
/// - The most recent user message (always kept)
/// - As many recent messages as fit
///
/// Returns trimmed messages and a flag indicating if trimming occurred.
pub fn trim_to_context(
    messages: &[Message],
    model: &str,
    config: &ContextConfig,
) -> (Vec<Message>, bool) {
    if !config.enabled || messages.is_empty() {
        return (messages.to_vec(), false);
    }

    let ctx_limit = config.max_tokens.unwrap_or_else(|| model_context_limit(model));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let budget = (ctx_limit as f32 * (1.0 - config.reserve_for_completion)) as usize;

    let total: usize = messages.iter().map(message_tokens).sum();
    if total <= budget {
        return (messages.to_vec(), false);
    }

    let mut system_msgs: Vec<&Message> = Vec::new();
    let mut non_system: Vec<&Message> = Vec::new();

    for msg in messages {
        if msg.role == "system" {
            system_msgs.push(msg);
        } else {
            non_system.push(msg);
        }
    }

    let system_cost: usize = system_msgs.iter().map(|m| message_tokens(m)).sum();
    let remaining_budget = budget.saturating_sub(system_cost);

    // Walk non-system messages from most recent backwards, keeping what fits
    let mut kept: Vec<&Message> = Vec::new();
    let mut used = 0;
    for msg in non_system.iter().rev() {
        let cost = message_tokens(msg);
        if used + cost > remaining_budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(msg);
    }
    kept.reverse();

    let trimmed = non_system.len() != kept.len();

    let mut result: Vec<Message> = system_msgs.into_iter().cloned().collect();
    if trimmed {
        let dropped_msgs: Vec<&Message> = non_system[..non_system.len() - kept.len()].to_vec();
        let dropped = dropped_msgs.len();

        match config.strategy {
            TrimStrategy::Summarize => {
                let summary = summarize_messages(&dropped_msgs);
                result.push(Message {
                    role: "system".to_string(),
                    content: format!("[{dropped} earlier message(s) summarized]\n{summary}"),
                });
            }
            TrimStrategy::DropOldest => {
                result.push(Message {
                    role: "system".to_string(),
                    content: format!("[{dropped} earlier message(s) trimmed to fit context window]"),
                });
            }
        }
    }
    result.extend(kept.into_iter().cloned());

    (result, trimmed)
}

fn summarize_messages(msgs: &[&Message]) -> String {
    let mut lines = Vec::new();
    for msg in msgs {
        if msg.content.is_empty() {
            continue;
        }
        let role_label = match msg.role.as_str() {
            "assistant" => "Assistant",
            "user" => "User",
            _ => &msg.role,
        };
        let truncated = if msg.content.len() > 150 {
            format!("{}...", &msg.content[..150])
        } else {
            msg.content.clone()
        };
        lines.push(format!("- {role_label}: {truncated}"));
    }
    if lines.is_empty() {
        "No meaningful content in dropped messages.".to_string()
    } else {
        lines.join("\n")
    }
}

fn tool_message_tokens(msg: &ToolMessage) -> usize {
    let content_len = msg.content.as_deref().map_or(0, str::len);
    let tool_calls_len = msg
        .tool_calls
        .as_ref()
        .map_or(0, |calls| calls.iter().map(|c| c.name.len() + c.arguments.to_string().len()).sum());
    (content_len + tool_calls_len) / 4 + 4
}

/// Approximate total token usage of an agent conversation. Used to decide when
/// the running context has grown enough to warrant summarizing older turns.
pub fn count_tool_message_tokens(messages: &[ToolMessage]) -> usize {
    messages.iter().map(tool_message_tokens).sum()
}

/// Approximate tokens consumed by the tool schemas sent on every turn. They are
/// part of the prompt but live outside the message list, so the budget check must
/// add them in or it under-counts and summarizes too late.
pub fn count_tool_definition_tokens(defs: &[ToolDefinition]) -> usize {
    defs.iter()
        .map(|d| {
            estimate_tokens(&d.name)
                + estimate_tokens(&d.description)
                + d.input_schema.to_string().len() / 4
                + 8 // wrapper overhead per tool
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn modern_models_get_large_windows() {
        // Regression: these used to fall through to an 8k default, which made the
        // agent trim its own tool results every turn and never finish (no files).
        for model in [
            "kimi-k2-thinking",
            "llama-4-maverick",
            "llama-4-scout",
            "gpt-5.4-pro",
            "qwen-3-max",
            "deepseek-v4",
            "glm-4.6",
            "some-future-model",
        ] {
            assert!(
                model_context_limit(model) >= 128_000,
                "{model} should have a >=128k window, got {}",
                model_context_limit(model)
            );
        }
        assert_eq!(model_context_limit("gpt-3.5-turbo"), 16_385);
    }

    #[test]
    fn no_trim_when_under_budget() {
        let msgs = vec![msg("user", "hello")];
        let cfg = ContextConfig::default();
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(!trimmed);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn trims_oldest_non_system() {
        let mut msgs = vec![msg("system", "you are helpful")];
        for i in 0..500 {
            msgs.push(msg("user", &"x".repeat(200)));
            msgs.push(msg("assistant", &format!("response {i} with lots of text to fill the context window quickly")));
        }
        let cfg = ContextConfig {
            enabled: true,
            max_tokens: Some(1000),
            strategy: TrimStrategy::DropOldest,
            reserve_for_completion: 0.25,
        };
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(trimmed);
        assert!(result.len() < msgs.len());
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content, "you are helpful");
        // Second message should be the trim notice
        assert!(result[1].content.contains("trimmed"));
    }

    #[test]
    fn disabled_does_nothing() {
        let msgs = vec![msg("user", &"x".repeat(100_000))];
        let cfg = ContextConfig {
            enabled: false,
            ..Default::default()
        };
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(!trimmed);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn summarize_strategy_includes_content() {
        let mut msgs = vec![msg("system", "you are helpful")];
        for i in 0..500 {
            msgs.push(msg("user", &format!("question {i}")));
            msgs.push(msg("assistant", &"a".repeat(200)));
        }
        let cfg = ContextConfig {
            enabled: true,
            max_tokens: Some(1000),
            strategy: TrimStrategy::Summarize,
            reserve_for_completion: 0.25,
        };
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(trimmed);
        assert!(result[1].content.contains("summarized"));
        assert!(result[1].content.contains("User:"));
    }

    #[test]
    fn counts_tokens_for_tool_messages() {
        let msgs = vec![
            ToolMessage::system("system prompt"),
            ToolMessage::user(&"x".repeat(400)),
        ];
        // ~400 chars / 4 ≈ 100 tokens for the user msg, plus overhead.
        assert!(count_tool_message_tokens(&msgs) >= 100);
    }
}
